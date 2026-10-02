//! Go panics and unported-code hits: `GoPanic`, `go_panic`, `unported!`
//! and the unported registry. They live in `goport_util` as the module
//! `core`, so util files keep their `crate::core::` paths and
//! `$crate::core::record_unported` resolves. `ts_goport`'s `core.rs`
//! re-exports them.

/// Unported hits of every thread, by Go name.
static UNPORTED_NAMES: std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Records one hit of unported Go code. The runner reports every name.
/// A run with any hit is not a match.
pub fn record_unported(go_name: &'static str) {
    let mut names = UNPORTED_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *names.entry(go_name).or_default() += 1;
}

/// All unported names hit so far on any thread, with hit counts.
#[must_use]
pub fn unported_report() -> Vec<(&'static str, u64)> {
    let names = UNPORTED_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    names.iter().map(|(k, v)| (*k, *v)).collect()
}

/// Puts back the unported hits that `unported_report` returned. Work that
/// is thrown away and redone uses it, so the hits are not counted twice.
pub fn restore_unported(report: &[(&'static str, u64)]) {
    let mut names = UNPORTED_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *names = report.iter().copied().collect();
}

/// Marks unported Go code. It records the hit, then panics so the gap is
/// loud. Use only where a port is missing, never as a fallback.
#[macro_export]
macro_rules! unported {
    ($go_name:expr) => {{
        $crate::core::record_unported($go_name);
        panic!("unported Go code: {}", $go_name)
    }};
}

/// The panic payload of `go_panic`.
pub struct GoPanic {
    /// The Go panic value as the Go runtime prints it (port form). It is
    /// also the Go `%v` of the value that `recover()` returns.
    pub message: String,
    /// The Go type of a value whose type is a named string type, such as
    /// `lsproto.DocumentUri` (`go_panic_typed`). The runtime prints it as
    /// `<type>("<message>")`. `None` for a plain string or an error.
    pub go_type: Option<&'static str>,
    /// A Go `recover()` raised the value again with `panic(r)`
    /// (`go_repanic`). The runtime adds ` [recovered, repanicked]`.
    pub repanicked: bool,
    /// The port site, for the stderr report.
    pub location: &'static std::panic::Location<'static>,
}

/// Go `panic(message)` at a site where the pinned Go panics on the same
/// input. It is not a port gap, so the run ends as the Go runtime ends it:
/// guards that keep a run going after a port gap pass it on
/// (`resume_go_panic`), and the bins write the output so far, print it with
/// `print_go_panic` and exit `EXIT_GO_PANIC`. Other panics stay port gaps
/// (`execute::tsc::EXIT_UNPORTED`).
#[track_caller]
pub fn go_panic(message: String) -> ! {
    std::panic::panic_any(GoPanic {
        message,
        go_type: None,
        repanicked: false,
        location: std::panic::Location::caller(),
    })
}

/// `go_panic` with a value of the named Go string type `go_type`, for
/// example `panic("overlay not found: " + uri)` where `uri` is a
/// `lsproto.DocumentUri`. `recover()` gives the same text, but the runtime
/// prints `panic: <go_type>("<message>")`.
#[track_caller]
pub fn go_panic_typed(go_type: &'static str, message: String) -> ! {
    std::panic::panic_any(GoPanic {
        message,
        go_type: Some(go_type),
        repanicked: false,
        location: std::panic::Location::caller(),
    })
}

/// Go `if r := recover(); r != nil { ...; panic(r) }`: raises a caught
/// panic again. A `go_panic` value is marked, so the runtime line gets
/// ` [recovered, repanicked]`. Any other payload continues as it is.
pub fn go_repanic(mut payload: Box<dyn std::any::Any + Send>) -> ! {
    if let Some(panic) = payload.downcast_mut::<GoPanic>() {
        panic.repanicked = true;
    }
    std::panic::resume_unwind(payload)
}

/// Runs `f` as the goroutine of Go `sync.WaitGroup.Go(f)`. At Go 1.26 that
/// goroutine has a deferred recover that panics again with the value of a
/// panic in `f` (`go_repanic`), so the runtime line ends with
/// ` [recovered, repanicked]`. The port runs the task on the calling thread.
pub fn go_wait_group_task<R>(f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => go_repanic(payload),
    }
}

/// Runs `f` as the goroutine of Go `sync.WaitGroup.Go(f)` where the port
/// runs it inline, inside work that a caller's `recover()` guards (the
/// autoimport registry build under a request). A Go `recover()` sees only
/// its own goroutine, so in Go a panic in `f` ends the process whatever the
/// caller recovers: the runtime prints the value with
/// ` [recovered, repanicked]` (see `go_wait_group_task`) and exits
/// `EXIT_GO_PANIC`. The port does the same for a `go_panic` value. Any
/// other payload is a port gap and continues as it is, so the caller's
/// guard still catches it.
pub fn go_wait_group_goroutine<R>(f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(mut payload) => {
            let Some(panic) = payload.downcast_mut::<GoPanic>() else {
                std::panic::resume_unwind(payload)
            };
            panic.repanicked = true;
            print_go_panic(payload.as_ref());
            std::process::exit(EXIT_GO_PANIC)
        }
    }
}

thread_local! {
    /// How many `go_recover` calls this thread is inside.
    static GO_RECOVER_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Go `defer func() { if r := recover(); r != nil { ... } }()` around `f`,
/// for a recover that answers the panic and goes on (the IPC request
/// handlers). Returns the payload of a panic in `f`. The Go runtime prints
/// nothing for a recovered panic, so the bins' panic hooks stay quiet while
/// `in_go_recover` is true.
// PORT: Go's request recovers (ipc/conn_async.go:206-223,
// ipc/conn_sync.go:119-136, api/session.go:1148-1153) put
// `panic: <value>\n<stack>` in the error response and write nothing to
// stderr. In `tsgo` a plain Rust panic in `f` (a port gap that is not
// `unported!`) is quiet too: its message is in the error response, and
// `GOPORT_TRACE=1` prints it with the backtrace of the panic site on
// stderr. The `goport` dev bin prints it with its port site.
pub fn go_recover<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    GO_RECOVER_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    GO_RECOVER_DEPTH.with(|depth| depth.set(depth.get() - 1));
    result
}

/// True inside `go_recover` on this thread.
#[must_use]
pub fn in_go_recover() -> bool {
    GO_RECOVER_DEPTH.with(|depth| depth.get() > 0)
}

/// Runs `f` as a task of Go `core.parallelWorkGroup` (`sync.WaitGroup.Go`).
/// Inside `go_recover` this is `go_wait_group_goroutine`: Go's recover sees
/// only its own goroutine, so a Go panic in `f` ends the process. Elsewhere
/// it is `go_wait_group_task`: the panic reaches the bin, which writes the
/// output so far and prints it as Go prints it.
pub fn go_work_group_task<R>(f: impl FnOnce() -> R) -> R {
    if in_go_recover() {
        go_wait_group_goroutine(f)
    } else {
        go_wait_group_task(f)
    }
}

/// The Go runtime when the OS refuses a new thread (runtime/os_linux.go
/// `newosproc`): it prints the error and the thread count, then
/// `throw("newosproc")` ends the process with exit 2. The port site takes
/// the place of the goroutine dump. `GoThread` calls it after Go's retry;
/// use `GoThread` to start a thread for work that Go runs on goroutines,
/// so the run fails as Go's does.
#[cold]
#[inline(never)]
#[track_caller]
pub fn go_fatal_newosproc(err: &std::io::Error) -> ! {
    // PORT: Go prints `mcount()`, its own count of threads.
    let threads = std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count);
    let errno = err.raw_os_error().unwrap_or(0);
    let mut text = format!(
        "runtime: failed to create new OS thread (have {threads} already; errno={errno})\n"
    );
    if err.kind() == std::io::ErrorKind::WouldBlock {
        text.push_str("runtime: may need to increase max user processes (ulimit -u)\n");
    }
    let location = std::panic::Location::caller();
    text.push_str(&format!(
        "fatal error: newosproc\n\n\t{}:{}\n",
        location.file(),
        location.line()
    ));
    use std::io::Write;
    let _ = std::io::stderr().write_all(text.as_bytes());
    std::process::exit(EXIT_GO_PANIC)
}

/// `std::thread::Builder` for a thread that runs work Go runs on
/// goroutines. A start that the OS refuses goes as a Go runtime thread
/// start goes (`newosproc`): it tries again while the error is EAGAIN, then
/// ends the process with Go's text (`go_fatal_newosproc`). Do not start one
/// while this thread holds a lock that one of its thread-local destructors
/// takes: the exit runs those destructors (glibc `exit`), and the process
/// hangs.
#[derive(Default)]
pub struct GoThread {
    name: Option<String>,
    stack_size: Option<usize>,
}

impl GoThread {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `std::thread::Builder::name`.
    #[must_use]
    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    /// `std::thread::Builder::stack_size`.
    #[must_use]
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = Some(size);
        self
    }

    /// `std::thread::Builder::spawn`, with Go's retry and fatal error.
    #[track_caller]
    pub fn spawn<F, T>(self, f: F) -> std::thread::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(f)));
        newosproc(|| self.builder().spawn(take_once(&slot)))
    }

    /// `std::thread::Builder::spawn_scoped`, with Go's retry and fatal
    /// error.
    #[track_caller]
    pub fn spawn_scoped<'scope, 'env, F, T>(
        self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
        f: F,
    ) -> std::thread::ScopedJoinHandle<'scope, T>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(f)));
        newosproc(|| self.builder().spawn_scoped(scope, take_once(&slot)))
    }

    fn builder(&self) -> std::thread::Builder {
        let mut builder = std::thread::Builder::new();
        if let Some(name) = &self.name {
            builder = builder.name(name.clone());
        }
        if let Some(size) = self.stack_size {
            builder = builder.stack_size(size);
        }
        builder
    }
}

/// The function of one try to start a thread. `Builder::spawn` drops the
/// function of a thread that it could not start, so each try takes `f`
/// from a shared slot, and only the started thread takes it.
fn take_once<F: FnOnce() -> T, T>(
    slot: &std::sync::Arc<std::sync::Mutex<Option<F>>>,
) -> impl FnOnce() -> T + use<F, T> {
    let slot = std::sync::Arc::clone(slot);
    move || {
        let f = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        f.expect("only the started thread takes its function")()
    }
}

// Go: runtime/os_linux.go:170 newosproc (go1.27.1)
// `retryOnEAGAIN` (runtime/retry.go:14) calls `clone` up to 20 times while
// it fails with EAGAIN, and sleeps 1, 2, ... 20 ms after each failure.
// Then, or at once for another error, the start throws.
// PORT: Go on Windows tries again on ERROR_ACCESS_DENIED instead
// (runtime/os_windows.go:794 createThread); the port does not.
#[track_caller]
fn newosproc<H>(mut clone: impl FnMut() -> std::io::Result<H>) -> H {
    let mut tries = 0;
    loop {
        let err = match clone() {
            Ok(handle) => return handle,
            Err(err) => err,
        };
        if err.kind() != std::io::ErrorKind::WouldBlock {
            go_fatal_newosproc(&err)
        }
        tries += 1;
        std::thread::sleep(std::time::Duration::from_millis(tries));
        if tries == 20 {
            go_fatal_newosproc(&err)
        }
    }
}

/// `go_panic` with the Go runtime text for a nil pointer dereference, at a
/// site where the pinned Go dereferences nil on the same input. It is cold
/// and out of line, so the nil check at a hot site is one compare.
#[cold]
#[inline(never)]
#[track_caller]
pub fn go_nil_dereference() -> ! {
    go_panic("runtime error: invalid memory address or nil pointer dereference".to_string())
}

/// The Go runtime exit code after a panic that nothing recovers.
pub const EXIT_GO_PANIC: i32 = 2;

/// Continues a caught `go_panic`. Returns any other payload.
pub fn resume_go_panic(payload: Box<dyn std::any::Any + Send>) -> Box<dyn std::any::Any + Send> {
    if payload.is::<GoPanic>() {
        std::panic::resume_unwind(payload);
    }
    payload
}

/// Prints a caught `go_panic` to stderr and returns true. The first line is
/// the Go runtime one (`panic: <value>`, Go `printpanics`): a typed value
/// is `<type>("<message>")`, each newline in the message is followed by a
/// tab (Go `printindented`), and a value raised again after a recover ends
/// with ` [recovered, repanicked]`. The port site takes the place of the
/// goroutine trace. False for any other payload.
pub fn print_go_panic(payload: &(dyn std::any::Any + Send)) -> bool {
    let Some(panic) = payload.downcast_ref::<GoPanic>() else {
        return false;
    };
    let message = panic.message.replace('\n', "\n\t");
    let value = match panic.go_type {
        Some(go_type) => format!("{go_type}(\"{message}\")"),
        None => message,
    };
    let suffix = if panic.repanicked {
        " [recovered, repanicked]"
    } else {
        ""
    };
    let text = format!(
        "panic: {value}{suffix}\n\n\t{}:{}\n",
        panic.location.file(),
        panic.location.line()
    );
    use std::io::Write;
    let _ = std::io::stderr().write_all(&crate::scanner_util::go_string_bytes(&text));
    true
}
