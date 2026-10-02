//! Go: the 14 Linux tests that typescript-go #4376, #4379 and #4495 added to
//! `internal/fswatch/{watcher,walkdir,eventlist}_test.go` (pin 52168999f3dc):
//! symlinked directory roots (pnpm and bun layouts), recursive watch
//! consolidation, `rebasePath`, `physicalDirFor`, `isInDirectoryOrSelf`,
//! `drainForSequences` and `walkDir` on a symlinked root. The other tests of
//! these Go files are in `fswatch_watcher.rs`, `fswatch_walkdir.rs` and
//! `fswatch_eventlist.rs`.
//!
//! PORT: the harness of `fswatch_watcher.rs` is private to that file, so
//! this file has a small copy: `run_for_each_watcher` runs each available
//! backend on its own thread with the Go retry (three attempts, timeouts
//! scaled 1, 5 and 15), and drop guards do the Go `t.Cleanup` work. Go
//! `sub.(*watch)` has no Rust form (a `Box<dyn Watch>` has no `Any`), so
//! the tests read the watch's dirWatch from the watcher's `dir_watches`.
//! The two FSEvents tests of #4495 (`fsevents_darwin_shared_test.go`) are
//! darwin only and are not ported.
//!
//! ts#64210 adds the path comparer tests of `watcher_test.go`
//! (`TestPathComparer`, `TestPathComparerUnicodeAlignment`,
//! `TestFileCallbackCaseSensitivity`, `TestPathComparerExactKeys`). Its
//! darwin tests (`fsevents_darwin_nfd_test.go`,
//! `fsevents_darwin_shared_test.go`) need CoreFoundation folding and a
//! volume query, which the port does not have, and are not ported.
#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use ts_goport::fswatch::fanotify_linux::{fanotify_available, new_fanotify_backend};
use ts_goport::fswatch::pathcompare::{ComparisonPath, PathComparer, equal_fold};
use ts_goport::fswatch::pathkey::PathComparerExported;
use ts_goport::fswatch::walkdir_unix::walk_dir;
use ts_goport::fswatch::{
    self, Callback, DirWatch, Event, EventKind, EventList, MAX_WAIT_TIME, NATIVE_PATH_FOLDING,
    RECURSIVE_CONSOLIDATE_THRESHOLD, Watch, WatchCallback, Watcher, WatcherBase, WatcherImpl,
    WatcherStruct, is_in_directory_or_self, join_path_suffix, new_debounce, new_dir_watch,
    new_watcher, physical_dir_for, rebase_path, walk_dir_generic, with_recursive,
};
use ts_goport::gostd::{GoError, errors};
use ts_goport::scanner_util::go_string_from_bytes;

use super::Failures;
use super::fswatch_watcher::{TmpDir, new_tmp_dir};
use crate::astnav_api::panic_message;

// ----- harness -----------------------------------------------------------

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

/// Go `filepath.Join` for the Linux paths of these tests.
fn join(dir: &str, parts: &[&str]) -> String {
    let mut out = dir.trim_end_matches('/').to_string();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    out
}

/// Go `t.TempDir()`: a fresh temp dir, not resolved, removed on drop.
fn temp_dir() -> (TmpDir, String) {
    let (tmp, _) = new_tmp_dir();
    let p = s(tmp.path());
    (tmp, p)
}

/// Go `newTmpDir(t)`: a fresh temp dir with symlinks resolved.
fn resolved_tmp_dir() -> (TmpDir, String) {
    let (tmp, resolved) = new_tmp_dir();
    (tmp, s(&resolved))
}

// Go: watcher_test.go:121 makeDirSymlink (the unix branch)
fn make_dir_symlink(target: &str, link: &str) {
    std::os::unix::fs::symlink(target, link)
        .unwrap_or_else(|e| panic!("directory symlink support is not available: {e}"));
}

fn mkdir(p: &str) {
    std::fs::create_dir(p).unwrap_or_else(|e| panic!("mkdir {p}: {e}"));
}

fn mkdir_all(p: &str) {
    std::fs::create_dir_all(p).unwrap_or_else(|e| panic!("mkdir -p {p}: {e}"));
}

fn write_file(p: &str, data: &str) {
    std::fs::write(p, data).unwrap_or_else(|e| panic!("write {p}: {e}"));
}

fn noop_callback() -> WatchCallback {
    Arc::new(|_, _| {})
}

/// Closes a watch on drop (Go `t.Cleanup(func() { _ = sub.Close() })`).
struct Sub(Box<dyn Watch>);

impl Drop for Sub {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

/// A dirWatch whose debounce entry is removed on drop (Go
/// `newDirectWatcher` and its `t.Cleanup`).
struct Direct(Arc<DirWatch>);

impl Drop for Direct {
    fn drop(&mut self) {
        self.0.destroy_debounce();
    }
}

// Go: watcher_test.go:150 newDirectWatcher
// PORT: Go sets `physicalDir` after `newDirWatch` in one test; the Rust
// field is set at creation, so it is a parameter here.
fn new_direct_watcher(dir: &str, physical_dir: &str) -> Direct {
    Direct(new_dir_watch(
        dir,
        physical_dir,
        new_debounce(),
        true,
        PathComparer::default(),
    ))
}

/// A callback that keeps its events and errors (the test callbacks of the
/// direct dirWatch tests, which Go `t.Fatal` on an error).
fn collecting_callback() -> (WatchCallback, Arc<Mutex<(Vec<Event>, Vec<String>)>>) {
    let got = Arc::new(Mutex::new((Vec::new(), Vec::new())));
    let g = got.clone();
    let cb: WatchCallback = Arc::new(move |events: Vec<Event>, err: Option<GoError>| {
        let mut g = g.lock().unwrap();
        if let Some(err) = err {
            g.1.push(err.error());
        }
        g.0.extend(events);
    });
    (cb, got)
}

/// The events that a `collecting_callback` got; fails on an error.
fn collected(got: &Mutex<(Vec<Event>, Vec<String>)>) -> Vec<Event> {
    let g = got.lock().unwrap();
    assert!(g.1.is_empty(), "callback error: {:?}", g.1);
    g.0.clone()
}

type W = (EventKind, String);

fn to_want_events(events: &[Event]) -> Vec<W> {
    events.iter().map(|e| (e.kind, e.path.clone())).collect()
}

// Go: watcher_test.go:532 assertEventSequence
fn assert_event_sequence(got: &[Event], want: &[W]) {
    let got: Vec<W> = to_want_events(got)
        .into_iter()
        .filter(|(_, p)| want.iter().any(|(_, wp)| wp == p))
        .collect();
    assert_eq!(
        got, want,
        "event sequence mismatch\nwant: {want:?}\n got: {got:?}"
    );
}

// Go: testutil_test.go retryAttempts and retryTimeoutScale
const RETRY_ATTEMPTS: u32 = 3;

fn retry_timeout_scale(attempt: u32) -> u32 {
    match attempt {
        1 => 1,
        2 => 5,
        _ => 15,
    }
}

// Go: fanotify_linux_test.go fanotifyNoRenameWatcher
static FANOTIFY_NO_RENAME_WATCHER: LazyLock<Arc<WatcherStruct>> = LazyLock::new(|| {
    new_watcher("fanotify-no-rename", |w| {
        if fanotify_available() {
            w.factory = Some(|| -> Arc<dyn WatcherImpl> { new_fanotify_backend(true) });
        }
    })
});

// Go: watcher_test.go:77 init (availableWatchers)
fn available_watchers() -> Vec<Arc<dyn Watcher>> {
    let mut out: Vec<Arc<dyn Watcher>> = fswatch::all_watchers()
        .into_iter()
        .filter(|w| w.available())
        .collect();
    let extra: Arc<dyn Watcher> = FANOTIFY_NO_RENAME_WATCHER.clone();
    if extra.available() {
        out.push(extra);
    }
    out
}

// Go: watcher_test.go:97 runForEachWatcher with testutil_test.go runWithRetry
/// Runs `body(attempt, watcher)` for every available watcher on its own
/// thread, up to three attempts, and fails with every backend that failed.
fn run_for_each_watcher(test: &str, body: fn(u32, &Arc<dyn Watcher>)) {
    let watchers = available_watchers();
    assert!(!watchers.is_empty(), "{test}: no available watcher");
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for w in &watchers {
            let failures = &failures;
            scope.spawn(move || {
                let name = format!("{test}/{}", w.name());
                let mut last = String::new();
                for attempt in 1..=RETRY_ATTEMPTS {
                    match catch_unwind(AssertUnwindSafe(|| body(attempt, w))) {
                        Ok(()) => return,
                        Err(payload) => last = panic_message(payload.as_ref()),
                    }
                }
                failures.lock().unwrap().push(format!(
                    "{name}: retry: gave up after {RETRY_ATTEMPTS} attempts; last failure: {last}"
                ));
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Go: watcher_test.go:220 recordingWatcher
struct Recorder {
    attempt: u32,
    buf: Mutex<Vec<Event>>,
    cond: Condvar,
}

impl Recorder {
    fn new(attempt: u32) -> Arc<Recorder> {
        Arc::new(Recorder {
            attempt,
            buf: Mutex::new(Vec::new()),
            cond: Condvar::new(),
        })
    }

    // Go: watcher_test.go:239 deadline (defaultEventTimeout on Linux)
    fn deadline(&self) -> Duration {
        Duration::from_secs(1) * retry_timeout_scale(self.attempt)
    }

    // Go: watcher_test.go:256 callback
    fn callback(self: &Arc<Self>) -> WatchCallback {
        let r = Arc::downgrade(self);
        Arc::new(move |events: Vec<Event>, _err: Option<GoError>| {
            let Some(r) = r.upgrade() else {
                return;
            };
            r.buf.lock().unwrap().extend(events);
            r.cond.notify_all();
        })
    }

    // Go: watcher_test.go:293 drainQuiet
    fn drain_quiet(&self, d: Duration) -> Vec<Event> {
        self.buf.lock().unwrap().clear();
        std::thread::sleep(d);
        std::mem::take(&mut *self.buf.lock().unwrap())
    }

    // Go: watcher_test.go:346 waitForEvent
    fn wait_for_event(&self, d: Duration, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + d;
        let mut buf = self.buf.lock().unwrap();
        loop {
            let now = Instant::now();
            if buf.iter().any(&pred) || now >= deadline {
                return std::mem::take(&mut *buf);
            }
            buf = self.cond.wait_timeout(buf, deadline - now).unwrap().0;
        }
    }
}

// Go: watcher_test.go:159 subscribeFor (with the Linux settleSleep)
fn subscribe_for(attempt: u32, dir: &str, w: &Arc<dyn Watcher>) -> (Arc<Recorder>, Sub) {
    let r = Recorder::new(attempt);
    let sub = w
        .watch_directory(dir, r.callback(), &[with_recursive()])
        .unwrap_or_else(|e| panic!("subscribe: {}", e.error()));
    std::thread::sleep(Duration::from_millis(60));
    (r, Sub(sub))
}

// Go: watcher_test.go:465 expectContains
fn expect_contains(r: &Recorder, kind: EventKind, path: &str) -> Vec<Event> {
    let d = r.deadline();
    let got = r.wait_for_event(d, |e| e.kind == kind && e.path == path);
    if !got.iter().any(|e| e.kind == kind && e.path == path) {
        panic!(
            "expected event {} {path} within {d:?}, got {:?}",
            kind.string(),
            to_want_events(&got)
        );
    }
    got
}

// Go: watcher_test.go:488 assertNoEventsForPath
fn assert_no_events_for_path(got: &[Event], path: &str, msg: &str) {
    let got: Vec<W> = to_want_events(got)
        .into_iter()
        .filter(|(_, p)| p == path)
        .collect();
    assert!(got.is_empty(), "{msg} {path}, got {got:?}");
}

// ----- walkdir_test.go ---------------------------------------------------

// Go: walkdir_test.go:34 TestWalkDirDoesNotFollowRootSymlinkedDir
#[test]
fn test_walk_dir_does_not_follow_root_symlinked_dir() {
    type Walk = fn(&str) -> Result<(), GoError>;
    let walks: [(&str, Walk); 2] = [
        ("native", |dir| walk_dir(dir, true, None)),
        ("generic", |dir| walk_dir_generic(dir, true, None)),
    ];
    let mut failures = Failures::new("TestWalkDirDoesNotFollowRootSymlinkedDir");
    for (name, walk) in walks {
        let (_root_tmp, root) = resolved_tmp_dir();
        let (_other_tmp, other) = temp_dir();
        let target = join(&other, &["target"]);
        mkdir(&target);
        let link = join(&root, &["link"]);
        make_dir_symlink(&target, &link);

        if walk(&link).is_ok() {
            failures.fail(name, "expected error for root symlinked directory".into());
        }
    }
    failures.finish();
}

// ----- eventlist_test.go -------------------------------------------------

// Go: eventlist_test.go:127 TestEventListDrainForSequences
#[test]
fn test_event_list_drain_for_sequences() {
    let el = EventList::default();
    el.create("file.txt");
    let start_after_create = el.sequence();
    el.remove("file.txt");

    let (events_by_callback, err) = el.drain_for_sequences(&[0, start_after_create]);
    if let Some(err) = err {
        panic!("{}", err.error());
    }
    assert!(
        events_by_callback[0].is_empty(),
        "create+delete should cancel for original callback, got {:?}",
        events_by_callback[0]
    );
    assert_eq!(
        events_by_callback[1].len(),
        1,
        "expected delete for later callback, got {:?}",
        events_by_callback[1]
    );
    let got = &events_by_callback[1][0].0;
    assert!(
        got.kind == EventKind::Delete && got.path == "file.txt",
        "expected delete for file.txt, got {got:?}"
    );
}

// ----- watcher_test.go: path helpers -------------------------------------

// Go: watcher_test.go:578 TestPathComparer (ts#64210)
#[test]
fn test_path_comparer() {
    // (root, path, suffix, exact, ignoreCase)
    let tests: [(&str, &str, &str, bool, bool); 30] = [
        ("/root", "/root", "", true, true),
        ("/root", "/root/file.ts", "/file.ts", true, true),
        ("/root", "/ROOT", "", false, true),
        ("/root", "/ROOT/File.ts", "/File.ts", false, true),
        (
            "/root",
            "/ROOT/Nested/File.ts",
            "/Nested/File.ts",
            false,
            true,
        ),
        ("/root", "/ROOT2/File.ts", "", false, false),
        ("/root", "/roo", "", false, false),
        ("/root/sub", "/ROOT", "", false, false),
        ("/root", "/other/File.ts", "", false, false),
        ("/root", "/ROOTish/File.ts", "", false, false),
        ("/root/sub", "/ROOT/SUB", "", false, true),
        ("/root/sub", "/ROOT/su", "", false, false),
        ("/root/[", "/ROOT/{/File.ts", "", false, false),
        ("/root/@", "/ROOT/`/File.ts", "", false, false),
        ("/", "/File.ts", "File.ts", true, true),
        ("/", "/", "", true, true),
        ("", "", "", false, false),
        ("", "/File.ts", "", false, false),
        (
            "/caf\u{00e9}",
            "/CAF\u{00c9}/File.ts",
            "/File.ts",
            false,
            true,
        ),
        ("/s", "/\u{017f}/File.ts", "/File.ts", false, true),
        ("/\u{017f}", "/S/File.ts", "/File.ts", false, true),
        ("/s/sub", "/\u{017f}/SUB/File.ts", "/File.ts", false, true),
        ("/\u{017f}/sub", "/S/SUB/File.ts", "/File.ts", false, true),
        ("/s", "/\u{017f}oo/File.ts", "", false, false),
        ("/k", "/\u{212a}/File.ts", "/File.ts", false, true),
        ("/\u{03c3}", "/\u{03c2}/File.ts", "/File.ts", false, true),
        ("/\u{00e9}", "/\u{00c8}/File.ts", "", false, false),
        (
            "/\u{00df}",
            "/SS/File.ts",
            "/File.ts",
            false,
            NATIVE_PATH_FOLDING,
        ),
        ("/root/s", "/ROOT/\u{017f}/File.ts", "/File.ts", false, true),
        ("/root/\u{017f}", "/ROOT/S", "", false, true),
    ];
    let mut failures = Failures::new("TestPathComparer");
    for (root, path, want_suffix, exact, ignore_case_want) in tests {
        for ignore_case in [false, true] {
            let comparer = PathComparer { ignore_case };
            let want = if ignore_case { ignore_case_want } else { exact };
            let name = format!("({root:?}, {path:?}), ignoreCase={ignore_case}");
            let (suffix, ok) = comparer.suffix(root, path);
            if ok != want || ok && suffix != want_suffix {
                failures.fail(
                    &format!("suffix{name}"),
                    format!("got ({suffix:?}, {ok}), want ({want_suffix:?}, {want})"),
                );
            }
            if comparer.contains(root, path) != want {
                failures.fail(&format!("contains{name}"), format!("want {want}"));
            }
            for to in ["/display", "/"] {
                let (rebased, ok) = comparer.rebase(path, root, to);
                if ok != want || ok && rebased != join_path_suffix(to, want_suffix) {
                    failures.fail(
                        &format!("rebase{name} to {to:?}"),
                        format!("got ({rebased:?}, {ok})"),
                    );
                }
            }
        }
    }
    failures.finish();
}

// Go: watcher_test.go:642 TestPathComparerUnicodeAlignment (ts#64210)
// PORT: the malformed parts ("\xff", "\xfe", "\xc3") are Go strings in the
// port form (`go_string_from_bytes`), and `strings.EqualFold` is
// `pathcompare::equal_fold`.
#[test]
fn test_path_comparer_unicode_alignment() {
    let mut parts: Vec<String> = [
        "s",
        "S",
        "\u{017f}",
        "k",
        "K",
        "\u{212a}",
        "\u{03c3}",
        "\u{03c2}",
        "\u{00e9}",
        "\u{00c9}",
        "\u{00c8}",
        "\u{10400}",
        "\u{10428}",
    ]
    .iter()
    .map(|part| part.to_string())
    .collect();
    for byte in [0xffu8, 0xfe, 0xc3] {
        parts.push(go_string_from_bytes(vec![byte]));
    }
    let comparer = PathComparer { ignore_case: true };
    for padding in 0..16 {
        let prefix = format!("/{}", "a".repeat(padding));
        for a in &parts {
            for b in &parts {
                for child in ["", "/child"] {
                    let root = format!("{prefix}{a}{child}");
                    let path = format!("{prefix}{b}{}/File.ts", child.to_uppercase());
                    let want = equal_fold(a, b);
                    let (suffix, ok) = comparer.suffix(&root, &path);
                    assert!(
                        ok == want && (!ok || suffix == "/File.ts"),
                        "suffix({root:?}, {path:?}): got ({suffix:?}, {ok}), want match={want}"
                    );
                }
            }
        }
    }
}

// Go: watcher_test.go:664 TestFileCallbackCaseSensitivity (ts#64210)
// PORT: Go calls `dw.setComparer` on a direct dirWatch; the port passes the
// comparer to `new_dir_watch`.
#[test]
fn test_file_callback_case_sensitivity() {
    for ignore_case in [false, true] {
        let dw = Direct(new_dir_watch(
            "/root",
            "/root",
            new_debounce(),
            true,
            PathComparer { ignore_case },
        ));
        let (cb, got) = collecting_callback();
        dw.0.add_callback("/root", "/root", false, cb, None, "/root/file.ts");
        dw.0.events.update("/root/FILE.ts");
        dw.0.events.update("/root/other.ts");
        dw.0.trigger_callbacks();
        let got = collected(&got);
        if ignore_case {
            assert!(
                got.len() == 1 && got[0].path == "/root/file.ts",
                "case-insensitive callback: got {got:?}"
            );
        } else {
            assert!(got.is_empty(), "case-sensitive callback: got {got:?}");
        }
    }
}

// Go: watcher_test.go:686 TestPathComparerExactKeys (ts#64210)
#[test]
fn test_path_comparer_exact_keys() {
    let c = PathComparerExported::default();
    let paths = [
        "/A/File.ts".to_string(),
        "/stra\u{00df}e/\u{0130}.ts".to_string(),
        "/cafe\u{0301}.ts".to_string(),
        go_string_from_bytes(b"/bad\xff.ts".to_vec()),
    ];
    for path in &paths {
        let got = c.key(path);
        assert_eq!(&got, path, "Key({path:?}) = {got:?}");
    }
    let (_, ok) = c.rebase("/A/file.ts", "/a", "/target");
    assert!(!ok, "zero comparer must use exact matching");
    let (got, ok) = c.rebase("/a/file.ts", "/a", "/target");
    assert!(ok && got == "/target/file.ts", "Rebase = {got:?}, {ok}");
}

// Go: watcher_test.go:702 TestRebasePath
// PORT: Go builds the paths with `filepath.Join` on the volume root; on
// Linux the root is "/".
#[test]
fn test_rebase_path() {
    let tests = [
        ("exact root", "/from", "/from", "/to", "/to"),
        ("child", "/from/child", "/from", "/to", "/to/child"),
        (
            "sibling",
            "/from-sibling/child",
            "/from",
            "/to",
            "/from-sibling/child",
        ),
        ("from root", "/child", "/", "/to", "/to/child"),
        ("to root", "/from/child", "/from", "/", "/child"),
    ];
    let mut failures = Failures::new("TestRebasePath");
    for (name, path, from, to, want) in tests {
        failures.check_eq(name, rebase_path(path, from, to), want.to_string());
    }
    failures.finish();
}

// Go: watcher_test.go:764 TestPhysicalDirForResolvesSymlinkAncestor
#[test]
fn test_physical_dir_for_resolves_symlink_ancestor() {
    let (_tmp, root) = temp_dir();
    let target = join(&root, &["target"]);
    mkdir_all(&join(&target, &["nested"]));
    let link = join(&root, &["link"]);
    make_dir_symlink(&target, &link);

    let dir = join(&link, &["nested"]);
    let want = physical_dir_for(&join(&target, &["nested"]));
    let got = physical_dir_for(&dir);
    assert_eq!(
        got, want,
        "physicalDirFor({dir:?}) = {got:?}, want {want:?}"
    );
}

// Go: watcher_test.go:782 TestIsInDirectoryOrSelf
#[test]
fn test_is_in_directory_or_self() {
    let tests = [
        ("exact", "/parent", "/parent", true),
        ("child", "/parent", "/parent/child", true),
        ("nested", "/parent", "/parent/child/nested", true),
        ("sibling prefix", "/parent", "/parent-sibling", false),
        ("root self", "/", "/", true),
        ("root child", "/", "/child", true),
        ("empty dir", "", "/parent/child", false),
    ];
    let mut failures = Failures::new("TestIsInDirectoryOrSelf");
    for (name, dir, path, want) in tests {
        failures.check_eq(name, is_in_directory_or_self(dir, path), want);
    }
    failures.finish();
}

// ----- watcher_test.go: symlinked roots on every backend -----------------

// Go: watcher_test.go:1252 TestSubscribeSymlinkedDirectoryRebasesTargetEvents
#[test]
fn test_subscribe_symlinked_directory_rebases_target_events() {
    run_for_each_watcher(
        "TestSubscribeSymlinkedDirectoryRebasesTargetEvents",
        |attempt, w| {
            let (_tmp, dir) = resolved_tmp_dir();
            let target = join(&dir, &["target"]);
            mkdir(&target);
            let link = join(&dir, &["link"]);
            make_dir_symlink(&target, &link);

            let (r, _sub) = subscribe_for(attempt, &link, w);
            write_file(&join(&target, &["child"]), "x");
            expect_contains(&r, EventKind::Update, &join(&link, &["child"]));
        },
    );
}

// Go: watcher_test.go:1272 TestRecursiveSubscribeSymlinkedDirectoryDoesNotFollowDescendantSymlink
#[test]
fn test_recursive_subscribe_symlinked_directory_does_not_follow_descendant_symlink() {
    run_for_each_watcher(
        "TestRecursiveSubscribeSymlinkedDirectoryDoesNotFollowDescendantSymlink",
        |attempt, w| {
            if w.has_fast_recursive_backend() {
                println!("SKIP: fast recursive backends do not use the userspace recursive walk");
                return;
            }
            let (_tmp, dir) = resolved_tmp_dir();
            let target = join(&dir, &["target"]);
            mkdir(&target);
            let link = join(&dir, &["link"]);
            make_dir_symlink(&target, &link);

            let descendant_target = join(&dir, &["descendant-target"]);
            mkdir(&descendant_target);
            let descendant_link = join(&target, &["descendant-link"]);
            make_dir_symlink(&descendant_target, &descendant_link);

            let (r, _sub) = subscribe_for(attempt, &link, w);
            let logical_grandchild = join(&link, &["descendant-link", "grandchild"]);
            let physical_grandchild = join(&descendant_target, &["grandchild"]);
            write_file(&logical_grandchild, "x");

            write_file(&join(&target, &["marker"]), "flush");

            let mut got = expect_contains(&r, EventKind::Update, &join(&link, &["marker"]));
            got.extend(r.drain_quiet(2 * MAX_WAIT_TIME));
            assert_no_events_for_path(
                &got,
                &logical_grandchild,
                "expected no events through descendant symlink",
            );
            assert_no_events_for_path(
                &got,
                &physical_grandchild,
                "expected no events for descendant symlink target",
            );
        },
    );
}

// ----- watcher_test.go: consolidation ------------------------------------

// Go: watcher_test.go:1530 countingWatcherImpl
// PORT: Go also keeps the closed watches, which no test reads.
struct CountingWatcherImpl {
    base: WatcherBase,
    subscribed: Mutex<Vec<Arc<DirWatch>>>,
}

thread_local! {
    /// PORT: the Go factory closure sets the test's `impl` variable. A Rust
    /// `WatcherFactory` is a `fn` pointer, so the factory puts the backend
    /// here. `getImpl` calls the factory on the test thread.
    static LAST_COUNTING_IMPL: RefCell<Option<Arc<CountingWatcherImpl>>> =
        const { RefCell::new(None) };
}

// Go: watcher_test.go:1536 newCountingWatcherImpl
fn new_counting_watcher_impl() -> Arc<dyn WatcherImpl> {
    let impl_ = Arc::new_cyclic(|self_: &Weak<CountingWatcherImpl>| {
        let b = CountingWatcherImpl {
            base: WatcherBase::default(),
            subscribed: Mutex::new(Vec::new()),
        };
        let self_impl: Weak<dyn WatcherImpl> = self_.clone();
        b.base.init(self_impl);
        b
    });
    LAST_COUNTING_IMPL.with(|last| *last.borrow_mut() = Some(impl_.clone()));
    impl_
}

impl WatcherImpl for CountingWatcherImpl {
    fn start(&self) -> Result<(), GoError> {
        self.base.notify_started();
        Ok(())
    }

    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        self.subscribed.lock().unwrap().push(w.clone());
        Ok(())
    }

    fn close_watch(&self, _: &Arc<DirWatch>) -> Result<(), GoError> {
        Ok(())
    }

    fn base(&self) -> &WatcherBase {
        &self.base
    }
}

/// Go `&watcher{name: "fsevents", factory: newCountingWatcherImpl}`.
fn counting_fsevents_watcher() -> Arc<WatcherStruct> {
    new_watcher("fsevents", |w| w.factory = Some(new_counting_watcher_impl))
}

fn last_counting_impl() -> Arc<CountingWatcherImpl> {
    LAST_COUNTING_IMPL
        .with(|last| last.borrow().clone())
        .expect("the counting backend was made")
}

/// Watches `count` sibling dirs `parent/pkg<i>` (not recursive).
fn watch_packages(w: &WatcherStruct, parent: &str, count: usize) -> Vec<Sub> {
    let mut subs = Vec::new();
    for i in 0..count {
        let dir = join(parent, &[&format!("pkg{i}")]);
        mkdir_all(&dir);
        let sub = w
            .watch_directory(&dir, noop_callback(), &[])
            .unwrap_or_else(|e| panic!("{}", e.error()));
        subs.push(Sub(sub));
    }
    subs
}

/// Fails unless `dw` is a recursive watch on `parent`.
fn assert_consolidated(dw: &DirWatch, parent: &str) {
    assert!(
        dw.dir == parent && dw.recursive,
        "expected consolidated recursive watch on {parent}, got dir={} recursive={}",
        dw.dir,
        dw.recursive
    );
}

// Go: watcher_test.go:1557 TestFastRecursiveWatcherConsolidatesSiblingDirectories
#[test]
fn test_fast_recursive_watcher_consolidates_sibling_directories() {
    let (_tmp, root) = temp_dir();
    let parent = join(&root, &["node_modules", ".bun"]);
    mkdir_all(&parent);

    let w = counting_fsevents_watcher();
    let _subs = watch_packages(&w, &parent, RECURSIVE_CONSOLIDATE_THRESHOLD + 2);

    let impl_ = last_counting_impl();
    let consolidated = {
        let subscribed = impl_.subscribed.lock().unwrap();
        assert_eq!(
            subscribed.len(),
            RECURSIVE_CONSOLIDATE_THRESHOLD,
            "expected {RECURSIVE_CONSOLIDATE_THRESHOLD} subscriptions after consolidation"
        );
        subscribed.last().unwrap().clone()
    };
    assert_consolidated(&consolidated, &parent);

    let key = w.key_for_dir_watch(&join(&parent, &["pkg11"]), false);
    let has_pkg_watch =
        w.mu.lock()
            .unwrap()
            .dir_watches
            .as_ref()
            .is_some_and(|m| m.contains_key(&key));
    assert!(
        !has_pkg_watch,
        "expected later package watch to reuse consolidated parent instead of creating its own stream"
    );
}

// Go: watcher_test.go:1609 TestFastRecursiveWatcherDoesNotConsolidateSymlinkOutsideRoot
#[test]
fn test_fast_recursive_watcher_does_not_consolidate_symlink_outside_root() {
    let (_tmp, root) = temp_dir();
    let parent = join(&root, &["node_modules", ".bun"]);
    mkdir_all(&parent);

    let w = counting_fsevents_watcher();
    let _subs = watch_packages(&w, &parent, RECURSIVE_CONSOLIDATE_THRESHOLD);

    let impl_ = last_counting_impl();
    let consolidated = impl_.subscribed.lock().unwrap().last().unwrap().clone();
    assert_consolidated(&consolidated, &parent);

    let target = join(&root, &["outside"]);
    mkdir_all(&target);
    let link = join(&parent, &["linked"]);
    make_dir_symlink(&target, &link);

    let _sub = Sub(w
        .watch_directory(&link, noop_callback(), &[])
        .unwrap_or_else(|e| panic!("{}", e.error())));

    // PORT: Go reads `sub.(*watch).dw`. The port reads the dirWatch that
    // `getOrCreateDirWatch` stored for the link.
    let key = w.key_for_dir_watch(&link, false);
    let dw =
        w.mu.lock()
            .unwrap()
            .dir_watches
            .as_ref()
            .and_then(|m| m.get(&key).cloned());
    let Some(dw) = dw else {
        panic!("symlink outside consolidated physical root should keep its own watch");
    };
    assert!(
        !Arc::ptr_eq(&dw, &consolidated),
        "symlink outside consolidated physical root should keep its own watch"
    );
    let physical = physical_dir_for(&link);
    assert!(
        dw.dir == link && dw.physical_dir == physical,
        "expected watch on symlink root {link} ({physical}), got {} ({})",
        dw.dir,
        dw.physical_dir
    );
}

/// The pnpm layout of the two consolidated symlink child tests:
/// `root/logical-parent` links to `root/physical-parent`, and
/// `logical-parent/link` links to `physical-parent/target`. Returns
/// (logical parent, link).
fn pnpm_layout(root: &str) -> (String, String) {
    let physical_parent = join(root, &["physical-parent"]);
    mkdir_all(&join(&physical_parent, &["target"]));
    let logical_parent = join(root, &["logical-parent"]);
    make_dir_symlink(&physical_parent, &logical_parent);
    let link = join(&logical_parent, &["link"]);
    make_dir_symlink(&join(&physical_parent, &["target"]), &link);
    (logical_parent, link)
}

// Go: watcher_test.go:1672 TestConsolidatedSymlinkChildMapsSharedLogicalPath
#[test]
fn test_consolidated_symlink_child_maps_shared_logical_path() {
    let (_tmp, root) = temp_dir();
    let (logical_parent, link) = pnpm_layout(&root);

    // Go `callback{dir, physicalDir, watchDir, watchPhysicalDir}`; the other
    // fields are the Go zero values.
    let cb = Callback {
        id: 0,
        dir: link.clone(),
        physical_dir: physical_dir_for(&link),
        watch_dir: logical_parent.clone(),
        watch_physical_dir: physical_dir_for(&logical_parent),
        recursive: false,
        fn_: noop_callback(),
        ignore: None,
        since_seq: 0,
        terminal: None,
        delivered: false,
        comparer: PathComparer::default(),
        dir_comparison: ComparisonPath::default(),
        physical_comparison: ComparisonPath::default(),
        file_comparison: ComparisonPath::default(),
    };
    let event = Event {
        kind: EventKind::Update,
        path: join(&logical_parent, &["target", "file.ts"]),
    };
    let got = cb.map_event(event);
    let want = join(&link, &["file.ts"]);
    assert_eq!(got.path, want, "mapEvent path");
}

// Go: watcher_test.go:1699 TestConsolidatedSymlinkChildTerminatesFromSharedLogicalPath
#[test]
fn test_consolidated_symlink_child_terminates_from_shared_logical_path() {
    let (_tmp, root) = temp_dir();
    let (logical_parent, link) = pnpm_layout(&root);

    let dw = new_direct_watcher(&logical_parent, &physical_dir_for(&logical_parent));
    let (id, _) =
        dw.0.watch(&link, &physical_dir_for(&link), true, noop_callback(), None);
    let err = errors::new("terminated");
    assert!(
        dw.0.terminate_callbacks_for_deleted_root(
            &join(&logical_parent, &["target"]),
            1,
            err.clone()
        ),
        "expected symlink child callback to terminate"
    );
    let locked = dw.0.mu.lock().unwrap();
    for cb in &locked.callbacks {
        if cb.id == id && !cb.terminal.as_ref().is_some_and(|t| errors::is(t, &err)) {
            panic!(
                "terminal error = {:?}, want {}",
                cb.terminal.as_ref().map(|t| t.error()),
                err.error()
            );
        }
    }
}

// Go: watcher_test.go:1728 TestConsolidatedChildWatchFiltersAgainstRequestedDir
#[test]
fn test_consolidated_child_watch_filters_against_requested_dir() {
    let (_tmp, tmp) = temp_dir();
    let parent = join(&tmp, &["parent"]);
    let child = join(&parent, &["child"]);
    let sibling = join(&parent, &["sibling"]);
    let dw = new_direct_watcher(&parent, &parent);

    let (cb, got) = collecting_callback();
    dw.0.watch(&child, &child, false, cb, None);
    dw.0.events.update_watch_root_at(&child, 1);
    dw.0.events.update(&join(&child, &["file.ts"]));
    dw.0.events.update(&join(&child, &["nested", "file.ts"]));
    dw.0.events.update(&join(&sibling, &["file.ts"]));
    dw.0.trigger_callbacks();

    let sort_key = |(kind, path): &W| (*kind as i32, path.clone());
    let mut got_w = to_want_events(&collected(&got));
    let mut want: Vec<W> = vec![
        (EventKind::Update, child.clone()),
        (EventKind::Update, join(&child, &["file.ts"])),
    ];
    got_w.sort_by_key(sort_key);
    want.sort_by_key(sort_key);
    assert_eq!(
        got_w, want,
        "event mismatch\nwant: {want:?}\n got: {got_w:?}"
    );
}

// Go: watcher_test.go:1767 TestConsolidatedChildWatchIgnoresEventsBeforeSubscribe
#[test]
fn test_consolidated_child_watch_ignores_events_before_subscribe() {
    let (_tmp, tmp) = temp_dir();
    let parent = join(&tmp, &["parent"]);
    let child = join(&parent, &["child"]);
    let dw = new_direct_watcher(&parent, &parent);

    dw.0.events.update(&child);
    let (cb, got) = collecting_callback();
    dw.0.watch(&child, &child, true, cb, None);
    dw.0.events.remove(&child);
    dw.0.trigger_callbacks();

    assert_event_sequence(&collected(&got), &[(EventKind::Delete, child)]);
}

// Go: watcher_test.go:1788 TestRecursiveWatchWithIgnoreDoesNotFilterByLogicalRoot
#[test]
fn test_recursive_watch_with_ignore_does_not_filter_by_logical_root() {
    let (_tmp, tmp) = temp_dir();
    let dir = join(&tmp, &["root"]);
    let dw = new_direct_watcher(&dir, &dir);

    let (_other_tmp, other) = temp_dir();
    let outside_path = join(&other, &["outside", "pkg", "index.ts"]);
    let (cb, got) = collecting_callback();
    dw.0.watch(&dir, &dir, true, cb, Some(Arc::new(|_: &str| false)));
    dw.0.events.update(&outside_path);
    dw.0.trigger_callbacks();

    assert_event_sequence(&collected(&got), &[(EventKind::Update, outside_path)]);
}
