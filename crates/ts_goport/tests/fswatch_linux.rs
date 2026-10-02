//! The Linux file watcher on the real kernel, default backend only: Go
//! internal/fswatch `TestWatchFileCreate` and `TestSubscribeSubfileUpdate`
//! (`watcher_test.go`), and the Linux case of Go `Default` (`watcher.go`).
//! `fswatch::default()` is fanotify when `fanotify_init` succeeds (Linux 5.13
//! or later with the needed permission), else inotify, as in Go.
//!
//! PORT: Go runs each test on every available backend (`runForEachWatcher`).
//! `go_baselines` `units_platform/fswatch_watcher.rs` ports that. This binary
//! runs the same test bodies on the default backend only, with the Go helpers
//! that they use: `runWithRetry` (3 attempts, timeout scale 1, 5 and 15),
//! `newTmpDir`, `subPath`, `subscribeFor` (recursive, then the Linux settle
//! sleep), the recording watcher, `expectEventSequence` and `expectContains`.
//! A Go `t.Fatal` is a panic.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ts_goport::fswatch::{self, Event, EventKind, Watch};

// ----- retry -------------------------------------------------------------

// Go: testutil_test.go:49 retryAttempts
const RETRY_ATTEMPTS: u32 = 3;

// Go: testutil_test.go:54 retryTimeoutScale
fn retry_timeout_scale(attempt: u32) -> u32 {
    match attempt {
        1 => 1,
        2 => 5,
        _ => 15,
    }
}

// Go: testutil_test.go:161 runWithRetry
/// Runs `body` with the attempt number 1, 2 and 3 until one attempt does not
/// panic. The panic hook prints each failed attempt.
fn run_with_retry(name: &str, body: impl Fn(u32)) {
    for attempt in 1..=RETRY_ATTEMPTS {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(attempt)));
        if result.is_ok() {
            if attempt > 1 {
                println!("{name}: retry: succeeded on attempt {attempt}/{RETRY_ATTEMPTS}");
            }
            return;
        }
        if attempt < RETRY_ATTEMPTS {
            println!(
                "{name}: retry: attempt {attempt}/{RETRY_ATTEMPTS} failed, retrying with {}× timeout scale",
                retry_timeout_scale(attempt + 1)
            );
        }
    }
    panic!("{name}: retry: gave up after {RETRY_ATTEMPTS} attempts");
}

// ----- temp dirs and names -----------------------------------------------

/// Go `t.TempDir()`: the directory is removed on drop.
struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

// Go: watcher_test.go:111 newTmpDir
/// A fresh temp dir, and its path with symlinks resolved so it matches what
/// backends report.
fn new_tmp_dir() -> (TmpDir, String) {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "goport-fswatch-{}-{n}-{}",
        std::process::id(),
        nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let resolved = dir
        .canonicalize()
        .unwrap_or_else(|e| panic!("EvalSymlinks: {e}"));
    let resolved = resolved.to_str().unwrap().to_string();
    (TmpDir(dir), resolved)
}

// Go: watcher_test.go:134 nameCounter
static NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

// Go: watcher_test.go:136 uniqueName and 143 subPath
/// A unique name in `dir`. Go uses `rand.Int63()` for the suffix; any unique
/// suffix does.
fn sub_path(dir: &str) -> String {
    let n = NAME_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{dir}/test{n}{}", nanos() % (1u128 << 63))
}

fn write_file(path: &str, data: &str) {
    std::fs::write(path, data).unwrap_or_else(|e| panic!("WriteFile {path}: {e}"));
}

// ----- recordingWatcher --------------------------------------------------

// Go: watcher_test.go:220 recordingWatcher
/// The events that a watch delivered and not yet taken. Go also keeps the
/// callback errors; these tests do not read them.
struct Recorder {
    attempt: u32,
    buf: Mutex<Vec<Event>>,
    cond: Condvar,
}

impl Recorder {
    // Go: watcher_test.go:239 deadline (the inotify and fanotify base,
    // watcher_test.go:31 defaultEventTimeout, times the retry scale)
    fn deadline(&self) -> Duration {
        Duration::from_secs(1) * retry_timeout_scale(self.attempt)
    }

    // Go: watcher_test.go:346 waitForEvent
    /// Waits up to `d` until an event matches `pred`, then takes every
    /// buffered event (also at the timeout).
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

    // Go: watcher_test.go:385 waitForAll
    /// Takes events until every wanted event arrived or `d` passed, and
    /// returns all of them.
    fn wait_for_all(&self, d: Duration, want: &[W]) -> Vec<Event> {
        let deadline = Instant::now() + d;
        let mut collected = Vec::new();
        let mut buf = self.buf.lock().unwrap();
        loop {
            collected.append(&mut buf);
            let now = Instant::now();
            if have_all(&collected, want) || now >= deadline {
                return collected;
            }
            buf = self.cond.wait_timeout(buf, deadline - now).unwrap().0;
        }
    }
}

/// Go `t.Cleanup(func() { _ = sub.Close() })`.
struct WatchGuard(Box<dyn Watch>);

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

// Go: watcher_test.go:159 subscribeFor and 202 subscribeForOpts. The sleep
// is watcher_test.go:167 settleSleep (60 ms on Linux); preSubscribeSleep is 0
// on Linux.
/// Watches `dir` recursively with the default watcher, then waits for the
/// watch to settle.
fn subscribe_for(attempt: u32, dir: &str) -> (Arc<Recorder>, WatchGuard) {
    let r = Arc::new(Recorder {
        attempt,
        buf: Mutex::new(Vec::new()),
        cond: Condvar::new(),
    });
    // Go: watcher_test.go:256 callback
    let rec = r.clone();
    let cb: fswatch::WatchCallback = Arc::new(move |events, _err| {
        rec.buf.lock().unwrap().extend(events);
        rec.cond.notify_all();
    });
    let sub = fswatch::default()
        .watch_directory(dir, cb, &[fswatch::with_recursive()])
        .unwrap_or_else(|e| panic!("subscribe: {}", e.error()));
    let guard = WatchGuard(sub);
    std::thread::sleep(Duration::from_millis(60));
    (r, guard)
}

// ----- assertions --------------------------------------------------------

// Go: watcher_test.go wantEvent
type W = (EventKind, String);

// Go: watcher_test.go:422 haveAll
fn have_all(got: &[Event], want: &[W]) -> bool {
    want.iter()
        .all(|(k, p)| got.iter().any(|e| e.kind == *k && e.path == *p))
}

// Go: watcher_test.go:503 toWantEvents
fn to_want_events(events: &[Event]) -> Vec<W> {
    events.iter().map(|e| (e.kind, e.path.clone())).collect()
}

// Go: watcher_test.go:454 expectEventSequence, with 532
// assertEventSequence and 542 filterToWantedPaths
/// Waits until every wanted event arrived, then checks that the events for
/// the wanted paths are exactly `want`, in order.
fn expect_event_sequence(r: &Recorder, want: &[W]) {
    let got = r.wait_for_all(r.deadline(), want);
    let got: Vec<Event> = got
        .into_iter()
        .filter(|e| want.iter().any(|(_, p)| *p == e.path))
        .collect();
    let got_w = to_want_events(&got);
    assert!(
        got_w == want,
        "event sequence mismatch\nwant: {want:?}\n got: {got_w:?}"
    );
}

// Go: watcher_test.go:465 expectContains, with 569 containsEvent
/// Waits until an event of `kind` for `path` arrived.
fn expect_contains(r: &Recorder, kind: EventKind, path: &str) {
    let d = r.deadline();
    let got = r.wait_for_event(d, |e| e.kind == kind && e.path == path);
    assert!(
        got.iter().any(|e| e.kind == kind && e.path == path),
        "expected event {} {path} within {d:?}, got {:?}",
        kind.string(),
        to_want_events(&got)
    );
}

// ----- tests -------------------------------------------------------------

// Go: watcher.go:230 Default (the linux case). Go has no test for it; its
// fanotify_linux_test.go:35 TestLinuxFanotifyBackendSelection checks only the
// fanotify watcher.
#[test]
fn default_watcher_is_the_go_choice() {
    let want = if fswatch::fanotify_linux::fanotify_available() {
        "fanotify"
    } else {
        "inotify"
    };
    assert_eq!(fswatch::default().name(), want);
    assert!(fswatch::default().available());
}

// Go: watcher_test.go:853 TestWatchFileCreate
#[test]
fn file_create_is_an_update() {
    run_with_retry("TestWatchFileCreate", |attempt| {
        let (_tmp, dir) = new_tmp_dir();
        let (r, _watch) = subscribe_for(attempt, &dir);

        let f = sub_path(&dir);
        write_file(&f, "hello");
        expect_event_sequence(&r, &[(EventKind::Update, f)]);
    });
}

// Go: watcher_test.go:1094 TestSubscribeSubfileUpdate
#[test]
fn recursive_watch_sees_a_subdirectory_file_update() {
    run_with_retry("TestSubscribeSubfileUpdate", |attempt| {
        let (_tmp, dir) = new_tmp_dir();
        let sub = sub_path(&dir);
        std::fs::create_dir(&sub).unwrap_or_else(|e| panic!("Mkdir {sub}: {e}"));
        let (r, _watch) = subscribe_for(attempt, &dir);
        let f = sub_path(&sub);
        // WatchDirectory-then-create so the create event populates the
        // watcher's tree before the modify arrives.
        write_file(&f, "v1");
        let _ = r.wait_for_event(r.deadline(), |_| true);
        write_file(&f, "v2-longer");
        expect_contains(&r, EventKind::Update, &f);
    });
}
