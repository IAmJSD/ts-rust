//! Port-only tests (no Go counterpart): a background task's call to the
//! client (the `update_watches` registerCapability) does not hold the
//! messages after it when the client answers late or never
//! (`lsp::Server::wait_client_reply`). Go waits on the task's goroutine.
//!
//! The client is written by hand: `lsptestutil::LspClient` answers server
//! requests inline, so it cannot hold an answer back. Each client message
//! is JSON text. The client answers each server request at once, except
//! the watch registrations while `hold` is set. It logs what it sees in
//! order: `watch`, `diag <uri>` and `answer <id>`.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use ts_goport::frontend::bundled;
use ts_goport::gostd::{GoError, context, errors};
use ts_goport::jsonrpc::{ID, MessageKind};
use ts_goport::lsp::{self, lsproto};

use super::lsptestutil;
use super::projecttestutil::files;

const INDEX: &str = "file:///test/index.ts";
const TSCONFIG: &str = "file:///test/tsconfig.json";
/// R169 holds a queued message for the 1 s watch request timeout.
const LIMIT: Duration = Duration::from_millis(800);

struct Reader(Receiver<Option<lsproto::Message>>);

impl lsp::Reader for Reader {
    fn read(&mut self) -> (Option<lsproto::Message>, Option<GoError>) {
        match self.0.recv() {
            Ok(Some(msg)) => (Some(msg), None),
            _ => (None, Some(errors::EOF.clone())),
        }
    }
}

struct Writer(Sender<lsproto::Message>);

impl lsp::Writer for Writer {
    fn write(&mut self, msg: &lsproto::Message) -> Result<(), GoError> {
        let _ = self.0.send(round_trip(msg));
        Ok(())
    }
}

/// The message after a trip through JSON, as a byte pipe gives it.
fn round_trip(msg: &lsproto::Message) -> lsproto::Message {
    parse(&msg.marshal_json().expect("marshal message"))
}

fn parse(json: &[u8]) -> lsproto::Message {
    let mut msg = lsproto::Message::default();
    msg.unmarshal_json(json).expect("unmarshal message");
    msg
}

struct Client {
    input: Sender<Option<lsproto::Message>>,
    output: Receiver<lsproto::Message>,
    /// Gets a value when the server's `run` returns.
    ended: Receiver<()>,
    ended_seen: Cell<bool>,
    next_id: Cell<i32>,
    /// Watch registrations get no answer while set.
    hold: Cell<bool>,
    held: RefCell<Vec<ID>>,
    log: RefCell<Vec<String>>,
}

impl Client {
    /// Starts a server on /test with `tsconfig` and an index.ts, and
    /// initializes it with dynamic watch registration.
    fn start(tsconfig: &str) -> Client {
        let setup = lsptestutil::server_setup(
            "/test",
            files(&[
                ("/test/tsconfig.json", tsconfig),
                ("/test/index.ts", "export const x = 1;\n"),
            ]),
        );
        let (input, input_reader) = channel();
        let (output_writer, output) = channel();
        let (ended_tx, ended) = channel();
        std::thread::Builder::new()
            .name("lsp-server".to_string())
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let server = lsp::new_server(lsp::ServerOptions {
                    in_: Box::new(Reader(input_reader)),
                    out: Box::new(Writer(output_writer)),
                    err: Box::new(std::io::sink()),
                    cwd: setup.cwd,
                    fs: bundled::wrap_fs(setup.files.expect("map fs").fs()),
                    default_library_path: setup.default_library_path,
                    typings_location: String::new(),
                    parse_cache: None,
                    npm_install: None,
                    spawn: None,
                    progress_delay: Duration::ZERO,
                    set_parent_process_id: None,
                });
                let _ = server.run(&context::background());
                let _ = ended_tx.send(());
            })
            .expect("start the server thread");
        let client = Client {
            input,
            output,
            ended,
            ended_seen: Cell::new(false),
            next_id: Cell::new(0),
            hold: Cell::new(false),
            held: RefCell::default(),
            log: RefCell::default(),
        };
        let caps = r#"{"workspace":{"didChangeWatchedFiles":{"dynamicRegistration":true,"relativePatternSupport":true}}}"#;
        let init = client.request(
            "initialize",
            &format!(r#"{{"processId":null,"rootUri":"file:///test","capabilities":{caps}}}"#),
        );
        assert!(client.wait_answer(init, Duration::from_secs(60)));
        client.notify("initialized", "{}");
        client
    }

    fn send(&self, json: String) {
        self.input
            .send(Some(parse(json.as_bytes())))
            .expect("send to the server");
    }

    fn request(&self, method: &str, params: &str) -> i32 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        self.send(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        id
    }

    fn notify(&self, method: &str, params: &str) {
        self.send(format!(
            r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#
        ));
    }

    fn reply(&self, id: &ID) {
        let resp = lsproto::ResponseMessage {
            id: Some(id.clone()),
            result: Some(Box::new(lsproto::Null)),
            ..Default::default()
        };
        self.input
            .send(Some(round_trip(&resp.message())))
            .expect("send to the server");
    }

    fn open_index(&self) {
        self.notify(
            "textDocument/didOpen",
            &format!(
                r#"{{"textDocument":{{"uri":"{INDEX}","languageId":"typescript","version":1,"text":"export const x = 1;\n"}}}}"#
            ),
        );
    }

    fn hover(&self) -> i32 {
        self.request(
            "textDocument/hover",
            &format!(
                r#"{{"textDocument":{{"uri":"{INDEX}"}},"position":{{"line":0,"character":13}}}}"#
            ),
        )
    }

    /// Reads server messages until the log has `entry`, for at most
    /// `timeout`. Returns whether it came.
    fn wait_for(&self, entry: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.log.borrow().iter().any(|seen| seen == entry) {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(msg) = self.output.recv_timeout(left) else {
                return false;
            };
            self.handle(&msg);
        }
        true
    }

    fn wait_answer(&self, id: i32, timeout: Duration) -> bool {
        self.wait_for(&format!("answer {id}"), timeout)
    }

    fn handle(&self, msg: &lsproto::Message) {
        let json = String::from_utf8(msg.marshal_json().expect("marshal")).expect("utf-8");
        match msg.kind {
            MessageKind::RESPONSE => {
                let id = msg.as_response().id.as_ref().expect("response id");
                self.log
                    .borrow_mut()
                    .push(format!("answer {}", id.string()));
            }
            MessageKind::REQUEST => {
                let req = msg.as_request();
                let id = req.id.as_ref().expect("request id");
                let watch = req.method == lsproto::Method::CLIENT_REGISTER_CAPABILITY
                    && json.contains(r#""watchers""#);
                if watch {
                    self.log.borrow_mut().push("watch".to_string());
                }
                if watch && self.hold.get() {
                    self.held.borrow_mut().push(id.clone());
                } else {
                    self.reply(id);
                }
            }
            _ if msg.as_request().method == lsproto::Method::TEXT_DOCUMENT_PUBLISH_DIAGNOSTICS => {
                let uri = json
                    .split(r#""uri":""#)
                    .nth(1)
                    .and_then(|rest| rest.split('"').next());
                self.log
                    .borrow_mut()
                    .push(format!("diag {}", uri.unwrap_or("")));
            }
            _ => {}
        }
    }

    /// Answers the held watch registrations and stops holding.
    fn release(&self) {
        self.hold.set(false);
        for id in self.held.take() {
            self.reply(&id);
        }
    }

    /// The log entries after the first `from`.
    fn log_after(&self, from: &str) -> Vec<String> {
        let log = self.log.borrow();
        let start = log
            .iter()
            .position(|seen| seen == from)
            .map_or(log.len(), |i| i + 1);
        log[start..].to_vec()
    }

    /// Waits up to `timeout` for the server's `run` to return. Returns
    /// whether it did.
    fn wait_end(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.ended_seen.get() {
            if self.ended.try_recv().is_ok() {
                self.ended_seen.set(true);
            } else if Instant::now() >= deadline {
                return false;
            } else if let Ok(msg) = self.output.recv_timeout(Duration::from_millis(5)) {
                self.handle(&msg);
            }
        }
        true
    }

    /// Answers what is held, ends the input and waits for the server.
    fn close(self) {
        self.release();
        let _ = self.input.send(None);
        assert!(
            self.wait_end(Duration::from_secs(30)),
            "the server did not end"
        );
    }
}

/// Opens index.ts and holds its task's first watch registration, then
/// sends `requests` (method and params) at once. Each answer must come
/// before `LIMIT`, while the registration still waits. Then answers it.
fn requests_do_not_wait_for_a_late_watch_reply(requests: &[(&str, String)]) -> Client {
    let client = Client::start("{}");
    client.hold.set(true);
    client.open_index();
    assert!(client.wait_for("watch", Duration::from_secs(60)));
    let start = Instant::now();
    let ids: Vec<i32> = requests
        .iter()
        .map(|(method, params)| client.request(method, params))
        .collect();
    for id in ids {
        let left = LIMIT.saturating_sub(start.elapsed());
        assert!(
            client.wait_answer(id, left),
            "request {id} waited for the watch registration: {:?}",
            client.log.borrow()
        );
    }
    assert_eq!(client.held.borrow().len(), 1, "{:?}", client.log.borrow());
    client.release();
    client
}

child_test! {
    fn a_queued_request_does_not_wait_for_a_late_watch_reply() {
        let client = requests_do_not_wait_for_a_late_watch_reply(&[(
            "textDocument/hover",
            format!(r#"{{"textDocument":{{"uri":"{INDEX}"}},"position":{{"line":0,"character":13}}}}"#),
        )]);
        // The task ends with its tsconfig.json publish. Its reply came in
        // time, so the watch is not pending: the next snapshot update
        // registers nothing again.
        assert!(client.wait_for(&format!("diag {TSCONFIG}"), Duration::from_secs(60)));
        client.log.borrow_mut().push("settled".to_string());
        client.notify(
            "textDocument/didChange",
            &format!(
                r#"{{"textDocument":{{"uri":"{INDEX}","version":2}},"contentChanges":[{{"text":"export const x = 2;\n"}}]}}"#
            ),
        );
        let hover = client.hover();
        assert!(client.wait_answer(hover, Duration::from_secs(60)));
        let after = client.log_after("settled");
        assert!(!after.iter().any(|seen| seen == "watch"), "{after:?}");
        client.close();
    }
}

child_test! {
    fn a_burst_does_not_wait_for_a_late_watch_reply() {
        let doc = format!(r#"{{"textDocument":{{"uri":"{INDEX}"}}}}"#);
        let at = format!(r#"{{"textDocument":{{"uri":"{INDEX}"}},"position":{{"line":0,"character":13}}}}"#);
        let client = requests_do_not_wait_for_a_late_watch_reply(&[
            ("textDocument/hover", at.clone()),
            ("textDocument/completion", at.clone()),
            ("textDocument/documentSymbol", doc.clone()),
            ("textDocument/foldingRange", doc.clone()),
            ("textDocument/references", format!(r#"{{"textDocument":{{"uri":"{INDEX}"}},"position":{{"line":0,"character":13}},"context":{{"includeDeclaration":true}}}}"#)),
            ("textDocument/diagnostic", doc),
        ]);
        client.close();
    }
}

child_test! {
    fn shutdown_and_exit_do_not_wait_for_a_watch_reply() {
        let client = Client::start("{}");
        client.hold.set(true);
        client.open_index();
        assert!(client.wait_for("watch", Duration::from_secs(60)));
        let shutdown = client.request("shutdown", "null");
        assert!(
            client.wait_answer(shutdown, LIMIT),
            "shutdown waited for the watch registration"
        );
        client.send(r#"{"jsonrpc":"2.0","method":"exit"}"#.to_string());
        assert!(
            client.wait_end(LIMIT),
            "exit waited for the watch registration"
        );
        client.close();
    }
}

child_test! {
    // A client that answers at once sees the order of a wait that only
    // blocks: the task's registrations and tsconfig.json diagnostics come
    // before the answer of the request after didOpen.
    fn a_fast_client_keeps_the_order() {
        let client = Client::start(r#"{"compilerOptions":{"bogusOption":true}}"#);
        client.open_index();
        let hover = client.hover();
        assert!(client.wait_answer(hover, Duration::from_secs(60)));
        let log = client.log.borrow().clone();
        let at = |entry: &str| log.iter().position(|seen| seen == entry);
        let order = (at("watch"), at(&format!("diag {TSCONFIG}")), at(&format!("answer {hover}")));
        assert!(
            matches!(order, (Some(watch), Some(diag), Some(answer)) if watch < answer && diag < answer),
            "{log:?}"
        );
        client.close();
    }
}

child_test! {
    // A served didChange and diagnostic pull adopt a newer snapshot while
    // the didOpen task waits. That disposes the task's snapshot, unless the
    // task holds a ref: without it the task's publish reads a released
    // program and panics.
    fn a_task_goes_on_after_a_served_snapshot_update() {
        let client = Client::start("{}");
        client.hold.set(true);
        client.open_index();
        assert!(client.wait_for("watch", Duration::from_secs(60)));
        client.notify(
            "textDocument/didChange",
            &format!(
                r#"{{"textDocument":{{"uri":"{INDEX}","version":2}},"contentChanges":[{{"text":"export const x = 2;\n"}}]}}"#
            ),
        );
        // The served pull's task registers again, while the first waits.
        client.hold.set(false);
        let pull = client.request(
            "textDocument/diagnostic",
            &format!(r#"{{"textDocument":{{"uri":"{INDEX}"}}}}"#),
        );
        assert!(client.wait_answer(pull, LIMIT), "{:?}", client.log.borrow());
        assert_eq!(client.held.borrow().len(), 1);
        client.log.borrow_mut().push("released".to_string());
        client.release();
        let hover = client.hover();
        assert!(
            client.wait_answer(hover, Duration::from_secs(60)),
            "{:?}",
            client.log.borrow()
        );
        // The first task published its project's diagnostics.
        let after = client.log_after("released");
        let at = |entry: &str| after.iter().position(|seen| seen == entry);
        let order = (at(&format!("diag {TSCONFIG}")), at(&format!("answer {hover}")));
        assert!(
            matches!(order, (Some(diag), Some(answer)) if diag < answer),
            "{after:?}"
        );
        client.close();
    }
}
