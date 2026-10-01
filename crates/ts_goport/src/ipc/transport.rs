//! Port of internal/ipc/transport.go, with internal/ipc/transport_windows.go
//! as its `cfg(windows)` part at the end (internal/api before tsgo#4712).
//!
//! PORT: Go `io.ReadWriteCloser` (what `Transport.Accept` returns) is the
//! trait `ReadWriteCloser`. Go hands the same value to the protocol reader,
//! the protocol writer and the caller that may close it, so the connection
//! is an `Arc<dyn ReadWriteCloser>` and its methods take `&self`.
//! `ConnReader` and `ConnWriter` are the `std::io` views that the protocols
//! read and write through. Go `net.Listener` is the trait `NetListener`.

use crate::ipc::prelude::*;

use crate::gostd::{GoError, errors};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

/// Go `io.ReadWriteCloser` for an API connection.
pub trait ReadWriteCloser: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write(&self, buf: &[u8]) -> std::io::Result<usize>;
    fn flush(&self) -> std::io::Result<()>;
    fn close(&self) -> Result<(), GoError>;
}

/// `std::io::Read` over a shared connection (Go passes the connection itself
/// as the `io.Reader`).
pub struct ConnReader(pub Arc<dyn ReadWriteCloser>);

impl Read for ConnReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

/// `std::io::Write` over a shared connection (Go passes the connection
/// itself as the `io.Writer`).
pub struct ConnWriter(pub Arc<dyn ReadWriteCloser>);

impl Write for ConnWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Go `net.Listener`, the part `PipeTransport` uses.
pub trait NetListener: Send + Sync {
    fn accept(&self) -> Result<Arc<dyn ReadWriteCloser>, GoError>;
    fn close(&self) -> Result<(), GoError>;
    /// Go `Addr().String()`.
    fn addr(&self) -> String;
}

// Go: ipc/transport.go:9 Transport
// Transport is an interface for accepting connections from API clients.
pub trait Transport {
    // Accept waits for and returns the next connection.
    fn accept(&mut self) -> Result<Arc<dyn ReadWriteCloser>, GoError>;
    // Close stops the transport from accepting new connections.
    fn close(&mut self) -> Result<(), GoError>;
}

// Go: ipc/transport.go:17 PipeTransport
// PipeTransport accepts connections on a Unix domain socket or Windows named pipe.
pub struct PipeTransport {
    listener: Box<dyn NetListener>,
}

// Go: ipc/transport.go:23 NewPipeTransport
// NewPipeTransport creates a new transport listening on the given path.
// On Unix, this creates a Unix domain socket. On Windows, this creates a named pipe.
pub fn new_pipe_transport(path: &str) -> Result<PipeTransport, GoError> {
    let listener = new_pipe_listener(path)?;
    Ok(PipeTransport { listener })
}

// PORT: the Go methods are also inherent methods, so callers need not
// import `Transport`.
impl PipeTransport {
    // Go: ipc/transport.go:32 Accept
    // Accept implements Transport.
    pub fn accept(&self) -> Result<Arc<dyn ReadWriteCloser>, GoError> {
        self.listener.accept()
    }

    // Go: ipc/transport.go:37 Close
    // Close implements Transport.
    pub fn close(&self) -> Result<(), GoError> {
        self.listener.close()
    }

    // Go: ipc/transport.go:42 Path
    // Path returns the path of the pipe/socket.
    pub fn path(&self) -> String {
        self.listener.addr()
    }
}

impl Transport for PipeTransport {
    fn accept(&mut self) -> Result<Arc<dyn ReadWriteCloser>, GoError> {
        PipeTransport::accept(self)
    }

    fn close(&mut self) -> Result<(), GoError> {
        PipeTransport::close(self)
    }
}

// Go: ipc/transport.go:48 StdioTransport
// StdioTransport wraps stdin/stdout as a single connection transport.
// It only accepts one connection.
// PORT: Go `io.ReadCloser` / `io.WriteCloser` are boxed `Read` / `Write`
// values; a nil one is `None`. Accept moves them into the connection
// (`used` already stops a second Accept).
pub struct StdioTransport {
    stdin: Option<Box<dyn Read + Send>>,
    stdout: Option<Box<dyn Write + Send>>,
    used: bool,
}

// Go: ipc/transport.go:55 NewStdioTransport
// NewStdioTransport creates a transport using the given stdin/stdout.
pub fn new_stdio_transport(
    stdin: Option<Box<dyn Read + Send>>,
    stdout: Option<Box<dyn Write + Send>>,
) -> StdioTransport {
    StdioTransport {
        stdin,
        stdout,
        used: false,
    }
}

impl StdioTransport {
    // Go: ipc/transport.go:63 Accept
    // Accept implements Transport.
    pub fn accept(&mut self) -> Result<Arc<dyn ReadWriteCloser>, GoError> {
        if self.used {
            return Err(errors::EOF.clone());
        }
        self.used = true;
        Ok(Arc::new(StdioConn {
            stdin: Mutex::new(StdioFile::new(self.stdin.take())),
            stdout: Mutex::new(StdioFile::new(self.stdout.take())),
        }))
    }

    // Go: ipc/transport.go:77 Close
    // Close implements Transport.
    pub fn close(&mut self) -> Result<(), GoError> {
        Ok(())
    }
}

impl Transport for StdioTransport {
    fn accept(&mut self) -> Result<Arc<dyn ReadWriteCloser>, GoError> {
        StdioTransport::accept(self)
    }

    fn close(&mut self) -> Result<(), GoError> {
        StdioTransport::close(self)
    }
}

// Go: ipc/transport.go:81 stdioConn
// PORT: Go embeds the reader and writer; here each sits behind a mutex so
// the connection can be shared (`ReadWriteCloser` takes `&self`).
struct StdioConn {
    stdin: Mutex<StdioFile<Box<dyn Read + Send>>>,
    stdout: Mutex<StdioFile<Box<dyn Write + Send>>>,
}

/// One file of a `StdioConn`: Go's nil interface, an open file, or a file
/// after `Close`. Go reads, writes or closes a nil one and panics with a nil
/// dereference; a closed `*os.File` gives an error.
/// PORT: the error texts name the files that cmd/tsgo passes (`os.Stdin`
/// and `os.Stdout`); the port's handles carry no name.
enum StdioFile<T> {
    Nil,
    Open(T),
    Closed,
}

impl<T> StdioFile<T> {
    fn new(file: Option<T>) -> StdioFile<T> {
        file.map_or(StdioFile::Nil, StdioFile::Open)
    }

    // The file for Go `f.Read` / `f.Write`: `op` is "read" or "write"
    // (the text of os.File `wrapErr(op, ErrClosed)`).
    fn file(&mut self, op: &str, name: &str) -> std::io::Result<&mut T> {
        match self {
            StdioFile::Nil => crate::core::go_nil_dereference(),
            StdioFile::Open(file) => Ok(file),
            StdioFile::Closed => Err(std::io::Error::other(format!(
                "{op} {name}: file already closed"
            ))),
        }
    }

    // Go `f.Close()`. The port drops the handle; dropping `std::io::Stdin`
    // or `Stdout` leaves the process stream open and reports no error.
    fn close(&mut self, name: &str) -> Result<(), GoError> {
        match self {
            StdioFile::Nil => crate::core::go_nil_dereference(),
            StdioFile::Open(_) => {
                *self = StdioFile::Closed;
                Ok(())
            }
            StdioFile::Closed => Err(errors::new(format!("close {name}: file already closed"))),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

const STDIN_NAME: &str = "/dev/stdin";
const STDOUT_NAME: &str = "/dev/stdout";

impl ReadWriteCloser for StdioConn {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        lock(&self.stdin).file("read", STDIN_NAME)?.read(buf)
    }

    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        lock(&self.stdout).file("write", STDOUT_NAME)?.write(buf)
    }

    // PORT: Go's `stdioConn` has no flush; its files have no buffer. The
    // port's protocol writer flushes after each message, so a nil or closed
    // stdout flushes nothing (the write before it reports the error).
    fn flush(&self) -> std::io::Result<()> {
        match &mut *lock(&self.stdout) {
            StdioFile::Open(stdout) => stdout.flush(),
            StdioFile::Nil | StdioFile::Closed => Ok(()),
        }
    }

    // Go: ipc/transport.go:88 Close
    fn close(&self) -> Result<(), GoError> {
        let err1 = lock(&self.stdin).close(STDIN_NAME);
        let err2 = lock(&self.stdout).close(STDOUT_NAME);
        err1?;
        err2
    }
}

#[cfg(unix)]
use crate::ipc::transport_unix::new_pipe_listener;

// ---------------------------------------------------------------------------
// transport_windows.go (//go:build windows)
// ---------------------------------------------------------------------------

// Go: ipc/transport_windows.go:12 newPipeListener
// newPipeListener creates a Windows named pipe listener.
// PORT: Go `winio.ListenPipe(path, nil)` makes a first pipe handle that
// reserves the name and a new pipe instance for each `Accept`. The port uses
// miow's safe `NamedPipe` (no `unsafe`, D-W1): the listener keeps the pipe
// instance that the next `Accept` connects, and makes the next one after a
// client connects. Both reject remote clients and allow any number of
// instances. The failure text is winio's `open <path>: <system message>`.
// PORT divergence: winio's buffer sizes are 0; these are miow's 64 KiB,
// because `--api` reads and writes its connection on one thread (Go uses
// overlapped I/O on goroutines). An API session of the LSP server reads on
// its `api-reader` thread and writes on the dispatch thread
// (`lsp/server.rs` `start_api_reader`). miow opens the pipe with
// `FILE_FLAG_OVERLAPPED` and each read and write waits on its own thread's
// event, so a read that waits does not hold a write on another thread.
// `Close` does not end an `Accept` that waits on another thread. The LSP
// server's `api-accept` thread closes the transport after its `Accept`; if
// the server ends first, the process exit ends the wait, as in Go.
#[cfg(windows)]
pub fn new_pipe_listener(path: &str) -> Result<Box<dyn NetListener>, GoError> {
    let first = new_pipe_instance(path, true)
        .map_err(|err| errors::new(format!("open {path}: {}", windows_error_text(&err))))?;
    Ok(Box::new(WindowsPipeListener {
        state: Mutex::new(WindowsPipeListenerState {
            closed: false,
            waiting: Some(first),
        }),
        path: path.to_string(),
    }))
}

/// One server instance of the named pipe `path`: duplex, byte mode, local
/// clients only. `first` fails when the name exists.
#[cfg(windows)]
fn new_pipe_instance(path: &str, first: bool) -> std::io::Result<miow::pipe::NamedPipe> {
    miow::pipe::NamedPipeBuilder::new(path)
        .first(first)
        .inbound(true)
        .outbound(true)
        .accept_remote(false)
        .max_instances(255)
        .create()
}

/// Go `windows.Errno.Error()`: the system message, which is Rust's text
/// without its " (os error N)".
#[cfg(windows)]
fn windows_error_text(err: &std::io::Error) -> String {
    let text = err.to_string();
    match err.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&text)
            .to_string(),
        None => text,
    }
}

/// Go winio `ErrPipeListenerClosed` (`net.ErrClosed`).
#[cfg(windows)]
const ERR_PIPE_LISTENER_CLOSED: &str = "use of closed network connection";

/// Go winio `*win32PipeListener`.
#[cfg(windows)]
struct WindowsPipeListener {
    state: Mutex<WindowsPipeListenerState>,
    path: String,
}

#[cfg(windows)]
struct WindowsPipeListenerState {
    closed: bool,
    /// The instance that the next `Accept` connects.
    waiting: Option<miow::pipe::NamedPipe>,
}

#[cfg(windows)]
impl NetListener for WindowsPipeListener {
    // Go winio win32PipeListener.Accept
    // PORT: winio makes the instance and waits for a client on its listener
    // goroutine; a client that connected and closed at once
    // (`ERROR_NO_DATA`) is skipped. Here the waiting instance is taken, so
    // the lock is not held while it waits.
    fn accept(&self) -> Result<Arc<dyn ReadWriteCloser>, GoError> {
        const ERROR_NO_DATA: i32 = 232;
        let pipe = {
            let mut state = lock(&self.state);
            if state.closed {
                return Err(errors::new(ERR_PIPE_LISTENER_CLOSED));
            }
            match state.waiting.take() {
                Some(pipe) => pipe,
                None => new_pipe_instance(&self.path, false)
                    .map_err(|err| errors::new(windows_error_text(&err)))?,
            }
        };
        loop {
            match pipe.connect() {
                Ok(()) => break,
                Err(err) if err.raw_os_error() == Some(ERROR_NO_DATA) => {
                    let _ = pipe.disconnect();
                }
                Err(err) => return Err(errors::new(windows_error_text(&err))),
            }
        }
        let mut state = lock(&self.state);
        if !state.closed {
            state.waiting = new_pipe_instance(&self.path, false).ok();
        }
        Ok(Arc::new(WindowsPipeConn {
            pipe: Mutex::new(Some(Arc::new(pipe))),
        }))
    }

    // Go winio win32PipeListener.Close: never fails, also when called again.
    fn close(&self) -> Result<(), GoError> {
        let mut state = lock(&self.state);
        state.closed = true;
        state.waiting = None;
        Ok(())
    }

    // Go `l.Addr().String()`: the pipe path.
    fn addr(&self) -> String {
        self.path.clone()
    }
}

/// Go winio `*win32Pipe`, a connected server instance.
/// PORT: `pipe` is `None` after `Close`, which closes the handle once a read
/// or write that runs on another thread ends (that call keeps its own
/// reference). As on Unix (`transport_unix.rs`), a read after `Close` is the
/// end of the stream and a write fails.
#[cfg(windows)]
struct WindowsPipeConn {
    pipe: Mutex<Option<Arc<miow::pipe::NamedPipe>>>,
}

#[cfg(windows)]
impl ReadWriteCloser for WindowsPipeConn {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(pipe) = lock(&self.pipe).clone() else {
            return Ok(0);
        };
        match (&*pipe).read(buf) {
            // The client closed its end: Go's winio reads `io.EOF`.
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(0),
            result => result,
        }
    }

    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(pipe) = lock(&self.pipe).clone() else {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        };
        (&*pipe).write(buf)
    }

    // PORT: Go's pipe has no buffer to flush. miow's `flush`
    // (FlushFileBuffers) would wait until the client reads everything.
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn close(&self) -> Result<(), GoError> {
        drop(lock(&self.pipe).take());
        Ok(())
    }
}

// Go: ipc/transport_windows.go:17 GeneratePipePath
// GeneratePipePath returns a platform-appropriate pipe path for the given name.
#[cfg(windows)]
pub fn generate_pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // PORT: no Go test. It checks what Go-B's `stdioConn` does (a Go test
    // run against pin 16c25522e123): a nil file panics with a nil
    // dereference, and after Close, `os.Stdin` and `os.Stdout` give "file
    // already closed".
    #[test]
    fn stdio_conn_nil_and_closed_files() {
        let nil_text = |f: &dyn Fn()| {
            let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .expect_err("a nil file panics");
            payload
                .downcast_ref::<crate::core::GoPanic>()
                .map(|panic| panic.message.clone())
        };
        let nil_deref =
            Some("runtime error: invalid memory address or nil pointer dereference".to_string());
        let conn = new_stdio_transport(None, None).accept().expect("accept");
        assert_eq!(nil_text(&|| drop(conn.read(&mut [0; 8]))), nil_deref);
        assert_eq!(nil_text(&|| drop(conn.write(b"x"))), nil_deref);
        assert_eq!(nil_text(&|| drop(conn.close())), nil_deref);

        let conn = new_stdio_transport(
            Some(Box::new(std::io::empty())),
            Some(Box::new(std::io::sink())),
        )
        .accept()
        .expect("accept");
        assert!(conn.close().is_ok());
        let err = conn.read(&mut [0; 8]).expect_err("read after close");
        assert_eq!(err.to_string(), "read /dev/stdin: file already closed");
        let err = conn.write(b"x").expect_err("write after close");
        assert_eq!(err.to_string(), "write /dev/stdout: file already closed");
        let err = conn.close().expect_err("close again");
        assert_eq!(err.error(), "close /dev/stdin: file already closed");
    }
}
