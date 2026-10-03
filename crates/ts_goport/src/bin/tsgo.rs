//! `tsgo`: the Go port of cmd/tsc.
//!
//! Go: cmd/tsc/main.go `runMain`. Every command line other than `--lsp`
//! and `--api` goes to `execute_tsc::command_line` (Go
//! `execute.CommandLine`), which parses all arguments.
//!
//! The exit code is the Go `ExitStatus` (0 to 5). Unported Go code that a
//! run reaches is listed on stderr as `unported: <name> <count>`. Such a
//! run and a panic exit with `EXIT_UNPORTED` (70), a code tsgo never
//! returns, like the other goport bins. A Go panic that the port keeps
//! (`core::go_panic`) ends the run as in Go: the output so far,
//! `panic: <message>` on stderr and exit 2.
//!
//! `--lsp` and `--api` run `cmd::tsgo::lsp::run_lsp` and
//! `cmd::tsgo::api::run_api` (Go cmd/tsc/lsp.go and api.go), the entry
//! points that `goport --lsp` and `goport --api` run too.
//!
//! Go `signal.NotifyContext(ctx, SIGINT, SIGTERM)` is
//! `cmd::tsgo::main::notify_context`. Only watch and build mode read the
//! context; a plain compile goes on after a signal, as in Go.
//! Not Go: when the run gets 4 KiB pages, a worker copy of the binary does
//! the work, so the exit does not wait for its memory to unmap (`launch`).
//!
//! As the Go runtime does at start: each thread unblocks the signals that
//! Go must get (`GO_UNBLOCKED`; those with a handler thread, `HELD`, once
//! their handlers are set), the signals that Go drops get a handler
//! that does nothing, SIGQUIT prints its name and exits 2, SIGHUP ends the
//! process by the signal (`go_signal_handlers`; each exit acts on such a
//! signal that came, `act_on_recorded`), SIGINT and SIGTERM do so until
//! `notify_context` (`notify_defaults`), and the soft open-file limit goes
//! up (`go_runtime_start`; PORTING.md "Process start").
//! PORT: Go `core.ApplyDebugStackLimit` (`TS_GO_DEBUG_STACK_LIMIT`) is a
//! debug setting and is skipped. The work runs on a thread with the stack
//! size of `gostd::stack::max_stack_size` (1 GiB with no address space or
//! data limit), like the other goport bins.
//! PORT: Go `osSys` and `newSystem` (cmd/tsc/sys.go) are ported as
//! `OsSystem` and `new_os_system` in execute/tsc/compile.rs.
//! PORT: `enablevtprocessing_windows.go` (the Windows console) is not
//! ported.

use std::any::Any;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use ts_goport::cmd::tsgo::api::run_api;
use ts_goport::cmd::tsgo::lsp::run_lsp;
use ts_goport::cmd::tsgo::main::notify_context;
use ts_goport::execute::execute_tsc::{GoTsc, command_line};
use ts_goport::execute::tsc::{EXIT_UNPORTED, System, new_os_system};
use ts_goport::gostd::context;
use ts_goport::prelude::*;

const UNPORTED_PREFIX: &str = "unported Go code";

/// jemalloc is the global allocator (default feature `jemalloc`). A build
/// without the feature uses glibc malloc. See `goport.rs`
/// `set_malloc_tunables`.
#[cfg(all(feature = "jemalloc", not(windows)))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Same as `goport.rs` `JEMALLOC_CONF`. `scripts/build-release.sh` reads it
/// from this line for its BOLT runs.
#[cfg(all(target_os = "linux", target_env = "gnu", feature = "jemalloc"))]
const JEMALLOC_CONF: &str = "narenas:4,thp:always,metadata_thp:disabled,cache_oblivious:false";

/// The `arg0` of a worker (see `launch`) is this word, the launcher's
/// process id, and the number, device and inode of the launcher's end of
/// the pipe that takes the exit code: `tsgo-worker 4242 3 15 81234`. The
/// arguments, not the environment, name a worker, so a process that the
/// worker starts gets nothing of it.
#[cfg(target_os = "linux")]
const WORKER_ARG0: &str = "tsgo-worker";

/// A worker's link to its launcher (see `launch`).
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct Worker {
    /// The number of the launcher's end of the pipe that takes the exit code.
    fd: u32,
    /// The device and inode of that pipe.
    dev: u64,
    ino: u64,
    /// The launcher.
    launcher: rustix::process::Pid,
}

// Go: cmd/tsc/main.go:14 main
fn main() {
    // First: before any thread starts (`thp_guard` can start one), so each
    // thread gets Go's mask, with `HELD` blocked until its handlers. It
    // allocates nothing.
    #[cfg(target_os = "linux")]
    start_signal_mask();
    // Next: it must run before the first heap allocation.
    let huge_pages = ts_goport::thp_guard::thp_guard();
    #[cfg(target_os = "linux")]
    if let Some(code) = launch(huge_pages) {
        std::process::exit(code);
    }
    // No worker: this process runs the work. SIGINT and SIGTERM end it by
    // their default action from here until `notify_context`. The action
    // runs in the handler, so the exec of `set_malloc_tunables` loses no
    // signal; the new image sets it again.
    #[cfg(target_os = "linux")]
    notify_defaults();
    // Unused off Linux (`launch` is Linux only).
    #[cfg(not(target_os = "linux"))]
    let _ = huge_pages;
    // A worker gets its parent-death signal here, or ends when its
    // launcher has ended (`worker`).
    #[cfg(target_os = "linux")]
    let _ = worker();
    // One budget sets the parse and bind threads and the malloc arenas.
    // tsgo has one more thread with an arena than goport: the
    // `notify_context` signal thread.
    let budget = ThreadBudget::one_program(1);
    set_malloc_tunables(&budget);
    // Go sets its handlers once, before `main`, in its last image. tsgo
    // sets them after `launch` (a launcher sends the signals on) and after
    // the exec of `set_malloc_tunables`: a signal that a handler took
    // before that exec would be lost, as the exec ends the `go-signals`
    // thread before it acts. A signal of `HELD` that comes before them
    // waits (blocked, also across the exec) and comes once they are set.
    #[cfg(target_os = "linux")]
    {
        go_signal_handlers();
        unblock_held();
    }
    budget.install();
    // After the exec in `set_malloc_tunables`: a raised limit would read as
    // the original one there.
    go_runtime_start();
    // Go: `System.SinceStart` counts from the process start. The tunables
    // step above may exec the binary again, so the clock starts after it.
    let start = Instant::now();
    install_panic_hook();
    // The thread ends the process itself once `run_main` has written the
    // output, so the exit does not wait for the thread stacks (up to 1 GiB
    // each, `max_stack_size`) to unmap, the thread-local destructors or the
    // join. Go runs `runMain` on the main goroutine. A thread that cannot
    // start ends the process as the Go runtime does (`GoThread`).
    let work = ts_goport::core::GoThread::new()
        .name("tsgo".to_string())
        .stack_size(ts_goport::gostd::stack::max_stack_size())
        .spawn(move || exit(run_main(start)));
    // Reached only when `run_main` panics.
    let _ = work.join();
    #[cfg(target_os = "linux")]
    act_on_recorded();
    eprintln!("tsgo: work thread failed");
    std::process::exit(EXIT_UNPORTED);
}

/// Copied from `goport.rs` `set_malloc_tunables`, which explains the
/// values. jemalloc gets `JEMALLOC_CONF`. With glibc malloc, `arena_max`
/// comes from `budget` (`ThreadBudget::one_program`): with the signal thread
/// it is 7 here, and 10 at 8 or more cores (3 spare arenas for the parse
/// workers that a large program adds). At 6, two checkers share one arena
/// lock (zod: 3.9k voluntary context switches, 0.5k at 7). Under an address
/// space or data limit, glibc malloc gets `arena_max=1`, with jemalloc too.
/// The variables stay set, so the exec runs once. A jemalloc build with
/// `JEMALLOC_CONF` built in execs only under a limit.
fn set_malloc_tunables(budget: &ThreadBudget) {
    // Unused off Linux.
    let _ = budget;
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::os::unix::process::CommandExt;
        let mut vars = Vec::new();
        // A build with `JEMALLOC_CONF` built into jemalloc
        // (`JEMALLOC_SYS_WITH_MALLOC_CONF`, set by `scripts/build-release.sh`)
        // needs no exec for it: jemalloc reads it at its start, and
        // `_RJEM_MALLOC_CONF` still overrides it.
        #[cfg(feature = "jemalloc")]
        if option_env!("JEMALLOC_SYS_WITH_MALLOC_CONF") != Some(JEMALLOC_CONF) {
            vars.push(("_RJEM_MALLOC_CONF", String::from(JEMALLOC_CONF)));
        }
        if let Some(value) = budget.glibc_tunables() {
            vars.push(("GLIBC_TUNABLES", value));
        }
        vars.retain(|(name, _)| std::env::var_os(name).is_none());
        if vars.is_empty() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut args = std::env::args_os();
        let mut command = std::process::Command::new(exe);
        if let Some(arg0) = args.next() {
            command.arg0(arg0);
        }
        // `exec` returns only when it fails (a binary that is gone, for
        // example). It has given SIGPIPE its default action for the new
        // image (std `Command`), so a write to a closed pipe or socket
        // would end this process, as the second SIGINT or SIGTERM did
        // (`notify_context` writes to its closed self-pipe). std ignores
        // SIGPIPE at start, and Go gets EPIPE there. A handler that does
        // nothing gives this process EPIPE again.
        // PORT: std and rustix have no safe `SIG_IGN`.
        let _ = command.args(args).envs(vars).exec();
        let ignore = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGPIPE, ignore);
    }
}

/// Runs the work in a worker copy of this binary and returns its exit code
/// (perf16). The worker sends the code over a pipe once its output is
/// written (`exit`), so this process exits before the worker unmaps its
/// memory. With 4 KiB pages that unmap takes about 50 ms for effect (1.3 GB);
/// with huge pages it takes about 4 ms, less than a second process costs
/// (about 1 ms on query check). So by default the worker runs only when
/// `thp_guard` says the run gets 4 KiB pages (`huge_pages` false).
/// `GOPORT_LAUNCH=0` never starts a worker and `GOPORT_LAUNCH=1` always
/// does. None when this process runs the work: it is a worker, no worker is
/// wanted, `--lsp`, `--api` or watch mode (`long_running`: they end on
/// their own), or the worker cannot start. The launcher sends SIGINT,
/// SIGTERM, SIGHUP and the signals that Go throws on to the worker
/// (`forward_signals`), and drops the signals that Go drops, SIGCHLD too
/// (`drop_go_signals`: an ignored SIGCHLD would make `wait` fail). When a
/// signal kills the worker, the launcher ends by the same signal
/// (`end_by_signal`; a pid 1 exits 128 + N, as Go does there), so the
/// caller sees what a run without a worker would give. When the worker
/// ends while the launcher still holds a SIGQUIT, SIGSTKFLT, SIGSYS or
/// SIGHUP for it, the launcher acts on that signal itself (`act_on`), as
/// Go would have acted when it came, unless the worker has thrown a signal
/// (`THROWN`), which printed its name once already.
#[cfg(target_os = "linux")]
fn launch(huge_pages: bool) -> Option<i32> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let wanted = match std::env::var_os("GOPORT_LAUNCH") {
        Some(v) if v == "0" => false,
        Some(v) if v == "1" => true,
        _ => !huge_pages,
    };
    if !wanted || worker().is_some() || ts_goport::thp_guard::long_running() {
        return None;
    }
    let mut args = std::env::args_os();
    args.next()?;
    let args: Vec<_> = args.collect();
    let exe = std::env::current_exe().ok()?;
    // Both ends keep their close-on-exec flag, so the worker and the
    // processes it starts get neither. The worker opens `write` again by
    // the number of `read` (`exit`). THP off (`prctl`) stays off in the
    // worker.
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
    let launcher = rustix::process::getpid().as_raw_pid();
    let stat = rustix::fs::fstat(&read).ok()?;
    // Before any handler: `ignored_at_start` reads the actions once.
    let _ = own_status();
    // Before the worker starts, so a failed start leaves no worker behind.
    // `HELD` stays blocked until the thread has taken SIGHUP too.
    let forward = forward_signals();
    drop_go_signals();
    let started = std::process::Command::new(exe)
        .arg0(format!(
            "{WORKER_ARG0} {launcher} {} {} {}",
            read.as_raw_fd(),
            stat.st_dev,
            stat.st_ino
        ))
        .args(args)
        .spawn();
    let Ok(mut worker) = started else {
        // No worker: this process runs the work, with `HELD` blocked until
        // its handlers (`main`). `forward` drops here, so its thread ends;
        // a signal that came while the start was tried waits for the run's
        // handlers.
        return None;
    };
    let mut unsent = None;
    if let Some((forward, ready, held)) = forward {
        let _ = forward.send(rustix::process::Pid::from_child(&worker));
        let _ = ready.recv();
        unsent = Some(held);
    }
    unblock_held();
    // `write` stays open until the worker ends, so the read below ends when
    // the code comes or when the worker has ended without it. The thread
    // does not reap the worker (`NOWAIT`): `wait` below does. When the
    // thread cannot start, `write` closes now and the read ends at once:
    // the launcher then takes the code from the worker's exit, which waits
    // for the worker's memory to unmap. No code or signal is lost, so this
    // port-only thread falls back instead of ending the run (`GoThread`).
    let pid = rustix::process::Pid::from_child(&worker);
    let _ = std::thread::Builder::new()
        .name("worker-exit".to_string())
        .spawn(move || {
            use rustix::process::{WaitId, WaitIdOptions, waitid};
            let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
            while let Err(rustix::io::Errno::INTR) = waitid(WaitId::Pid(pid), options) {}
            drop(write);
        });
    let mut code = [0; 4];
    if std::fs::File::from(read).read_exact(&mut code).is_ok() {
        let code = i32::from_le_bytes(code);
        if code == THROWN {
            return Some(EXIT_GO_PANIC);
        }
        if let Some(unsent) = &unsent {
            act_on(unsent);
        }
        return Some(code);
    }
    // The worker ended without sending a code.
    let status = worker.wait();
    if let Some(signal) = status.as_ref().ok().and_then(ExitStatusExt::signal) {
        // End by the same signal. Where that returns (a pid 1, or a
        // signal that the launcher catches), exit 128 + N below.
        end_by_signal(signal);
    } else if let Some(unsent) = &unsent {
        act_on(unsent);
    }
    Some(match status {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(_) => EXIT_UNPORTED,
    })
}

/// This process as a worker (see `launch`): its `arg0` is `WORKER_ARG0`
/// with a launcher and its pipe, and its parent is that launcher. The
/// first call decides, at the start of `main`. A worker gets a
/// parent-death SIGKILL, so it ends when its launcher ends, as a killed Go
/// tsgo stops at once.
/// A process whose `arg0` names a launcher that is not its parent runs as
/// a plain tsgo, unless that launcher has ended (`has_ended`). Then this
/// process is the launcher's worker and the launcher died before the
/// parent check (a SIGKILL soon after the start), so it kills itself, as
/// the parent-death signal would have. std and rustix have no safe way to
/// set that signal between fork and exec, so the launcher can also die
/// after the check and before the signal is set: the second parent check
/// finds that.
#[cfg(target_os = "linux")]
fn worker() -> Option<Worker> {
    use rustix::process::{Signal, getpid, getppid, kill_process, set_parent_process_death_signal};
    static WORKER: std::sync::OnceLock<Option<Worker>> = std::sync::OnceLock::new();
    *WORKER.get_or_init(|| {
        let arg0 = std::env::args_os().next()?;
        let rest = arg0
            .to_str()?
            .strip_prefix(WORKER_ARG0)?
            .strip_prefix(' ')?;
        let mut fields = rest.split(' ');
        let launcher = rustix::process::Pid::from_raw(fields.next()?.parse().ok()?)?;
        let fd = fields.next()?.parse().ok()?;
        let dev = fields.next()?.parse().ok()?;
        let ino = fields.next()?.parse().ok()?;
        if fields.next().is_some() {
            return None;
        }
        if getppid() == Some(launcher) {
            let _ = set_parent_process_death_signal(Some(Signal::KILL));
            if getppid() == Some(launcher) {
                return Some(Worker {
                    fd,
                    dev,
                    ino,
                    launcher,
                });
            }
        } else if !has_ended(launcher) {
            return None;
        }
        let _ = kill_process(getpid(), Signal::KILL);
        std::process::exit(EXIT_UNPORTED)
    })
}

/// Whether the process `pid` has ended: it is gone, or it is a zombie (it
/// has ended and its parent has not reaped it yet). The state is the field
/// after the command name in /proc/<pid>/stat. Without that file, or
/// without this process's own /proc (`own_proc`: there the file is of
/// another process), `kill` with no signal tells only whether the process
/// is gone: a zombie launcher then gives a plain tsgo.
#[cfg(target_os = "linux")]
fn has_ended(pid: rustix::process::Pid) -> bool {
    let path = format!("/proc/{}/stat", pid.as_raw_pid());
    match own_proc().then(|| std::fs::read(path)) {
        // `<pid> (<name>) <state> ...`: the name can hold ") ".
        Some(Ok(stat)) => {
            let name_end = stat.iter().rposition(|&b| b == b')');
            let state = name_end.and_then(|end| stat.get(end + 2));
            matches!(state, Some(b'Z' | b'X'))
        }
        _ => rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH),
    }
}

/// Whether /proc is the /proc of this process's PID namespace, so that
/// /proc/<pid> is the process `pid`. In a PID namespace that has the /proc
/// of another one (`bwrap --unshare-pid` without `--proc`, `unshare -pf`
/// without `--mount-proc`), /proc/<pid> is another process or none: for
/// the pid of a worker it can be a kernel thread whose parent has the pid
/// of the launcher. The `NSpid` line of /proc/self/status has this process's
/// pid in each PID namespace from the one of /proc down to its own, so it
/// is the one pid that `getpid` gives only in its own /proc. A kernel
/// without that line (before 4.1) has the `Pid` line, the pid in the
/// namespace of /proc. False without /proc (`own_status`).
#[cfg(target_os = "linux")]
fn own_proc() -> bool {
    own_status().own_proc
}

/// Whether `signal` was ignored (`SIG_IGN`) when this process started,
/// from the `SigIgn` line of /proc/self/status (a hex mask with bit N-1 for
/// signal N). Go then keeps SIGHUP and SIGINT ignored
/// (`sigInstallGoHandler`), and so does tsgo: it sets no handler for them
/// (`go_signal_handlers`, `forward_signals`) until `notify_context` (Go
/// `Notify`) takes SIGINT. /proc/self is this process in any /proc that
/// shows it.
/// PORT: std, rustix and signal-hook have no safe way to read an action.
/// Without /proc, no signal counts as ignored.
#[cfg(target_os = "linux")]
fn ignored_at_start(signal: i32) -> bool {
    own_status().ignored & (1 << (signal - 1)) != 0
}

/// What /proc/self/status says (`own_proc`, `ignored_at_start`). The first
/// call reads the file, before this process sets a handler for SIGHUP or
/// SIGINT; the later calls use its result. All false without the file.
#[cfg(target_os = "linux")]
fn own_status() -> OwnStatus {
    static STATUS: std::sync::OnceLock<OwnStatus> = std::sync::OnceLock::new();
    *STATUS.get_or_init(|| {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return OwnStatus::default();
        };
        let pid = rustix::process::getpid().as_raw_pid().to_string();
        let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name));
        let own_proc = match field("NSpid:") {
            Some(pids) => pids.split_whitespace().eq([pid.as_str()]),
            None => field("Pid:").map(str::trim) == Some(pid.as_str()),
        };
        let ignored = field("SigIgn:").and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok());
        OwnStatus {
            own_proc,
            ignored: ignored.unwrap_or(0),
        }
    })
}

/// See `own_status`.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Default)]
struct OwnStatus {
    own_proc: bool,
    /// The `SigIgn` mask.
    ignored: u64,
}

/// Sends each SIGINT, SIGTERM and SIGHUP and each signal that Go throws
/// (`GO_THROWN`) that the launcher gets on to the worker, on a thread. So a
/// signal reaches the work as in a run without a worker: `notify_context`
/// catches SIGINT and SIGTERM there, and a plain compile goes on, as in
/// Go; SIGQUIT prints its name there once, also when it went to the whole
/// process group; SIGHUP ends the worker (`die_from_signal`) and then the
/// launcher by the same signal. Without this, the signal would end the
/// launcher and then the parent-death signal would kill the worker, and a
/// launcher that is pid 1 would not get SIGHUP at all (`end_by_signal`).
/// The thread takes SIGHUP only after the worker has started and only when
/// it was not ignored at start (`ignored_at_start`): an exec keeps an ignored
/// signal but gives a caught one its default action, so the worker gets
/// the caller's action for SIGHUP, as `go_signal_handlers` needs.
/// `launch` calls it before it starts the worker and sends the worker's pid
/// to the returned sender; a signal that comes first waits for it. The
/// thread then answers on the returned receiver once it has taken SIGHUP,
/// so `launch` can unblock `HELD`. When no worker starts, `launch` drops
/// the sender and the thread ends.
/// Each signal also waits until the worker catches it (`caught`), for at
/// most `HOLD_LIMIT` after the worker starts, and only with this process's
/// own /proc (`own_proc`). Otherwise it goes on at once, and so does a
/// signal that the worker was seen to catch before (the thread of
/// `notify_context`, which the wait looks for, ends after its first
/// signal). Each signal waits on its own, so one that waits does not hold
/// a later one: after a SIGINT to the whole process group (a terminal's
/// Ctrl-C), the worker's `notify_context` thread has ended at its own copy,
/// so the launcher's copy waits until `HOLD_LIMIT`, and a SIGQUIT or SIGHUP
/// that then comes to the launcher only goes on at once, as in Go.
/// A SIGQUIT, SIGSTKFLT, SIGSYS or SIGHUP that waits when the worker ends
/// stays in the returned `Arrivals` (the handler records each one when it
/// comes, and the thread takes it when it sends it on), so `launch` acts
/// on it (`act_on`). A SIGINT or SIGTERM that waits then is dropped, as
/// before: the worker has passed its `notify_context`.
/// The thread starts as a Go runtime thread does (`GoThread`): when the OS
/// refuses it, the launcher ends with Go's text and exit 2, before there is
/// a worker. Go has no launcher, so its process gets every signal; a
/// launcher that went on without this thread would drop them (dropping
/// `signals` removes their actions, not their handlers).
#[cfg(target_os = "linux")]
fn forward_signals() -> Option<(
    std::sync::mpsc::Sender<rustix::process::Pid>,
    std::sync::mpsc::Receiver<()>,
    std::sync::Arc<Arrivals>,
)> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let thrown: Vec<i32> = GO_THROWN
        .iter()
        .map(|(signal, _)| signal.as_raw())
        .collect();
    let acted: Vec<i32> = thrown.iter().copied().chain([SIGHUP]).collect();
    // Before the actions of `signals`, so a flag is set when the thread
    // reads its signal. SIGHUP's comes with SIGHUP itself, below.
    let unsent = std::sync::Arc::new(Arrivals::new(&acted));
    for &signal in &thrown {
        let _ = unsent.listen(signal);
    }
    let mut signals =
        signal_hook::iterator::Signals::new([SIGINT, SIGTERM].into_iter().chain(thrown)).ok()?;
    let (send, receive) = std::sync::mpsc::channel();
    let (taken, ready) = std::sync::mpsc::channel();
    let held_on = unsent.clone();
    ts_goport::core::GoThread::new()
        .name("forward-signals".to_string())
        .spawn(move || {
            let unsent = held_on;
            let Ok(pid) = receive.recv() else {
                return;
            };
            // The worker has started.
            let limit = own_proc().then(|| Instant::now() + HOLD_LIMIT);
            if !ignored_at_start(SIGHUP) {
                let _ = unsent.listen(SIGHUP);
                let _ = signals.add_signal(SIGHUP);
            }
            // Each signal of `HELD` has its action in the launcher now.
            let _ = taken.send(());
            unblock_held();
            // The signals that the worker was seen to catch (a bit per
            // signal, as in `SigCgt`): they go on at once.
            let mut caught_before = 0u64;
            // The signals that wait, in the order they came, each once.
            let mut held: Vec<rustix::process::Signal> = Vec::new();
            // The files are read at once, then after pauses of 1, 2, 4
            // and 8 ms, then every 8 ms.
            let mut pause = std::time::Duration::from_millis(1);
            loop {
                let came = if held.is_empty() {
                    signals.wait()
                } else {
                    signals.pending()
                };
                for signal in came.filter_map(rustix::process::Signal::from_named_raw) {
                    if !held.contains(&signal) {
                        held.push(signal);
                        pause = std::time::Duration::from_millis(1);
                    }
                }
                held.retain(|&signal| {
                    let raw = signal.as_raw();
                    let bit = 1u64 << (raw - 1).unsigned_abs();
                    let wait = match limit {
                        Some(until) if caught_before & bit == 0 && Instant::now() < until => {
                            match caught(pid, signal) {
                                Some(true) => {
                                    caught_before |= bit;
                                    false
                                }
                                Some(false) => true,
                                // The worker has ended: `launch` acts on
                                // a signal that `unsent` records.
                                None if unsent.has(raw) => return false,
                                None => false,
                            }
                        }
                        _ => false,
                    };
                    // `launch` may have taken it at the worker's end.
                    if !wait && (!unsent.has(raw) || unsent.took(raw)) {
                        let _ = rustix::process::kill_process(pid, signal);
                    }
                    wait
                });
                if !held.is_empty() {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(std::time::Duration::from_millis(8));
                }
            }
        });
    Some((send, ready, unsent))
}

/// Whether the worker `pid` catches `signal` now (`forward_signals`), so a
/// forwarded signal finds the handlers that the worker sets at its start
/// (`go_signal_handlers`, `notify_context`) and does not end it by the
/// default action. A signal that came before them (soon after the start,
/// or while the start of `forward_signals` was tried again) waits: Go
/// sets its handlers before `main`, and `runMain` calls `NotifyContext`
/// before any work (cmd/tsc/main.go:29).
/// The kernel lists the caught signals in /proc/<pid>/status (`SigCgt`, a
/// hex mask with bit N-1 for signal N). signal-hook sets the handler (the
/// bit) a moment before it publishes the action that the handler runs
/// (signal-hook-registry 1.4.8 `register_unchecked_impl`), and a signal in
/// between does nothing. So it also needs the thread that the worker
/// starts after it has registered the signal (`ready_thread`). The caller
/// calls it only with this process's own /proc (`own_proc`).
/// None when the worker has ended: that file is gone, or it does not show a
/// live child of this process. True when the file cannot be read for
/// another reason: a wait for it would last until its limit.
#[cfg(target_os = "linux")]
fn caught(pid: rustix::process::Pid, signal: rustix::process::Signal) -> Option<bool> {
    let status = match std::fs::read_to_string(format!("/proc/{}/status", pid.as_raw_pid())) {
        Ok(status) => status,
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                || err.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return None;
        }
        Err(_) => return Some(true),
    };
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
    };
    let launcher = rustix::process::getpid().as_raw_pid().to_string();
    let child = field("PPid:") == Some(launcher.as_str());
    let live = field("State:").is_some_and(|state| !state.starts_with(['Z', 'X']));
    let mask = field("SigCgt:").and_then(|mask| u64::from_str_radix(mask, 16).ok());
    let mask = mask.filter(|_| child && live)?;
    let bit = 1u64 << (signal.as_raw() - 1).unsigned_abs();
    Some(mask & bit != 0 && has_thread(pid, ready_thread(signal)))
}

/// The name of the thread that a worker starts once its action for
/// `signal`, a signal that a launcher sends on, is published
/// (`caught`): `notify_context` registers SIGINT and SIGTERM
/// and then starts `signal.NotifyContext` (cmd/tsgo/main.rs), and
/// `go_signal_handlers` registers the others and then starts
/// `GO_SIGNALS_THREAD`.
#[cfg(target_os = "linux")]
fn ready_thread(signal: rustix::process::Signal) -> &'static str {
    use rustix::process::Signal;
    if signal == Signal::INT || signal == Signal::TERM {
        "signal.NotifyContext"
    } else {
        GO_SIGNALS_THREAD
    }
}

/// Whether the process `pid` has a thread named `name`, from
/// /proc/<pid>/task (the kernel keeps the first 15 bytes of a name). True
/// when that directory cannot be read: a wait for it would last until its
/// limit.
#[cfg(target_os = "linux")]
fn has_thread(pid: rustix::process::Pid, name: &str) -> bool {
    let name = &name.as_bytes()[..name.len().min(15)];
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", pid.as_raw_pid())) else {
        return true;
    };
    tasks.flatten().any(|task| {
        std::fs::read(task.path().join("comm"))
            .is_ok_and(|comm| comm.strip_suffix(b"\n") == Some(name))
    })
}

/// How long after the worker starts a forwarded signal can wait for the
/// worker's handlers (`caught`). The worker sets them a few
/// milliseconds after its start. After this time a signal goes on at once,
/// as without the wait, so no wait lasts the whole run (a worker that is
/// stopped at its start, for example).
#[cfg(target_os = "linux")]
const HOLD_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Signals that the Go runtime catches and drops when no `signal.Notify`
/// asks for them: `_SigNotify` alone or with `_SigUnblock` or `_SigIgn`,
/// without `_SigDefault`, in `runtime/sigtab_linux_generic.go` (go1.27.1).
/// The default action of SIGCHLD, SIGURG and SIGWINCH (`_SigIgn`) does
/// nothing too, but Go sets its handler also when they were ignored at
/// start. The real-time signals 35 to 64 (`GO_DROPPED_RT`) are such
/// signals too; Go leaves 32 to 34 to the C library. SIGPIPE is apart: Go
/// has its own rule for stdout and stderr. Go sets no handler for SIGCONT
/// and the stop signals (`_SigDefault`).
#[cfg(target_os = "linux")]
const GO_DROPPED: [rustix::process::Signal; 12] = {
    use rustix::process::Signal;
    [
        Signal::USR1,
        Signal::USR2,
        Signal::ALARM,
        Signal::CHILD,
        Signal::URG,
        Signal::XCPU,
        Signal::XFSZ,
        Signal::VTALARM,
        Signal::PROF,
        Signal::WINCH,
        Signal::IO,
        Signal::POWER,
    ]
};

/// The real-time signals that Go drops (see `GO_DROPPED`).
#[cfg(target_os = "linux")]
const GO_DROPPED_RT: std::ops::RangeInclusive<i32> = 35..=64;

/// Signals for which the Go runtime prints the name from its table and
/// exits 2 (`_SigThrow` in `runtime/sigtab_linux_generic.go`), also when the
/// signal was ignored at start: Go keeps an inherited `SIG_IGN` only for
/// SIGHUP and SIGINT (`runtime/signal_unix.go` `sigInstallGoHandler`).
/// PORT: Go then prints the goroutines; the port prints only the name.
/// PORT: SIGABRT and SIGTRAP keep their default actions: Rust's abort (a
/// stack overflow too) raises SIGABRT, and debuggers use SIGTRAP.
#[cfg(target_os = "linux")]
const GO_THROWN: [(rustix::process::Signal, &str); 3] = {
    use rustix::process::Signal;
    [
        (Signal::QUIT, "SIGQUIT: quit"),
        (Signal::STKFLT, "SIGSTKFLT: stack fault"),
        (Signal::SYS, "SIGSYS: bad system call"),
    ]
};

/// The signals that the Go runtime unblocks on each of its threads, so a
/// caller cannot block them (`runtime/signal_unix.go` `minitSignalMask` and
/// `blockableSig`, go1.27.1): `_SigUnblock`, `_SigKill` or `_SigThrow` in
/// `runtime/sigtab_linux_generic.go`, and SIGURG, its preemption signal
/// (`start_signal_mask`).
/// PORT: Go also unblocks the signals 32 to 34 (`_SigUnblock`). nix's
/// `SigSet` has no real-time signals, so tsgo keeps them as the caller set
/// them (glibc does not block 32 and 33, its own signals).
/// PORT: with `GODEBUG=asyncpreemptoff=1` Go lets a caller block SIGURG.
/// tsgo does not read `GODEBUG`; both drop SIGURG (`GO_DROPPED`).
#[cfg(target_os = "linux")]
const GO_UNBLOCKED: [nix::sys::signal::Signal; 15] = {
    use nix::sys::signal::Signal;
    [
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGILL,
        Signal::SIGTRAP,
        Signal::SIGABRT,
        Signal::SIGBUS,
        Signal::SIGFPE,
        Signal::SIGSEGV,
        Signal::SIGTERM,
        Signal::SIGSTKFLT,
        Signal::SIGCHLD,
        Signal::SIGURG,
        Signal::SIGPROF,
        Signal::SIGSYS,
    ]
};

// Go: runtime/signal_unix.go minitSignalMask (go1.27.1)
/// Unblocks the signals that Go unblocks on each of its threads
/// (`GO_UNBLOCKED`), but blocks those of `HELD` until their handlers are
/// set (`unblock_held`), in one change of the mask. `main` calls it first,
/// before any thread starts, so each thread gets the new mask: in tsgo, in
/// a launcher and in its worker. A process that tsgo starts gets the mask
/// of the thread that starts it (std `Command` keeps it), so the caller's
/// mask without these signals, as from Go: Go saves the mask of the thread
/// before the fork and the child sets it (runtime/proc.go
/// `syscall_runtime_BeforeFork`, `syscall_runtime_AfterForkInChild`).
/// Only the worker starts while `HELD` is blocked, and it sets its own.
#[cfg(target_os = "linux")]
fn start_signal_mask() {
    use nix::sys::signal::SigSet;
    if let Ok(mut mask) = SigSet::thread_get_mask() {
        for signal in GO_UNBLOCKED {
            mask.remove(signal);
        }
        for signal in HELD {
            mask.add(signal);
        }
        let _ = mask.thread_set_mask();
    }
}

/// The signals whose handlers act on a thread of tsgo: SIGHUP and the
/// signals that Go throws (`GO_THROWN`), and in a launcher SIGINT and
/// SIGTERM too (`forward_signals`). From the start of `main`
/// (`start_signal_mask`) until their actions are set, every thread blocks
/// them, so one that comes in between waits for them, also across the exec
/// of `set_malloc_tunables` (an exec keeps the mask and the waiting
/// signals). Then `unblock_held` unblocks them: `main` after
/// `go_signal_handlers`, and in a launcher, `launch` once the thread of
/// `forward_signals` has taken SIGHUP; the threads that start before that
/// (`go-signals`, `forward-signals`) unblock them themselves. Without it a
/// signal could be lost: signal-hook sets a handler a moment before it
/// publishes the action that the handler runs, and a signal in between
/// does nothing (signal-hook-registry 1.4.8 `register_unchecked_impl` and
/// `handler`), which a thread that the OS stops there makes milliseconds.
/// Go sets its handlers before `main`, so a signal that comes before them
/// has its default action there; such a signal waits in tsgo.
/// A process that runs the work (no launcher) keeps SIGINT and SIGTERM
/// blocked only until `launch` has chosen: then `notify_defaults` sets
/// their default actions and unblocks them.
/// PORT: the `thp_guard` watcher thread starts before the handlers and
/// keeps them blocked; it reads only /proc/buddyinfo.
#[cfg(target_os = "linux")]
const HELD: [nix::sys::signal::Signal; 6] = {
    use nix::sys::signal::Signal;
    [
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGTERM,
        Signal::SIGSTKFLT,
        Signal::SIGSYS,
    ]
};

/// Unblocks `HELD` in this thread, once their actions are set.
#[cfg(target_os = "linux")]
fn unblock_held() {
    use nix::sys::signal::SigSet;
    let _ = HELD.into_iter().collect::<SigSet>().thread_unblock();
}

// Go: runtime/sigtab_linux_generic.go SIGINT and SIGTERM (`_SigNotify +
// _SigKill`), runtime/signal_unix.go `dieFromSignal`
/// Gives SIGINT and SIGTERM their default actions in a process that runs
/// the work, until `notify_context` (Go `NotifyContext`) takes them, and
/// unblocks them (`HELD`). `main` calls it once `launch` has chosen. Go ends
/// the process at such a signal in its handler, and so does tsgo: the
/// action sets the default action and raises the signal on the thread that
/// the signal came to, so no exit can come first. (Followups9 round c acted
/// on the `go-signals` thread, and a short run often exited before it.)
/// The actions are set while the signals are blocked: signal-hook sets a
/// handler a moment before it publishes the action that the handler runs.
/// So the handler is there before `notify_context` registers its own
/// action, and a signal never comes to a handler without an action. The
/// actions do nothing once `notify_starts` clears their flag
/// (`NOTIFY_DEFAULT`). An ignored SIGINT stays ignored, as in Go
/// (`ignored_at_start`). A pid 1 exits 128 + N at once, as
/// `die_from_signal` does there.
/// PORT: std and rustix have no safe `SIG_DFL`. signal-hook's conditional
/// default action sets it in the handler; in a pid 1 its raise does
/// nothing and it then calls `abort`, so a pid 1 uses its conditional
/// shutdown (`_exit`).
/// PORT: not with `--lsp` and `--api`. They call their own
/// `notify_context` (cmd/tsgo/lsp.rs and api.rs), which `run_main` cannot
/// see, so SIGINT and SIGTERM keep the kernel's default action until it; a
/// signal that comes while that sets its handler can be lost there.
#[cfg(target_os = "linux")]
fn notify_defaults() {
    use nix::sys::signal::{SigSet, Signal};
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::flag::{register_conditional_default, register_conditional_shutdown};
    let server = std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--lsp" || arg == "--api");
    if !server {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let pid_1 = rustix::process::getpid().is_init();
        let mut signals = Vec::new();
        for signal in [SIGINT, SIGTERM] {
            if signal == SIGINT && ignored_at_start(signal) {
                continue;
            }
            let action = if pid_1 {
                register_conditional_shutdown(signal, 128 + signal, flag.clone())
            } else {
                register_conditional_default(signal, flag.clone())
            };
            if action.is_ok() {
                signals.push(signal);
            }
        }
        let _ = NOTIFY_DEFAULT.set(NotifyDefault { signals, flag });
    }
    let notified: SigSet = [Signal::SIGINT, Signal::SIGTERM].into_iter().collect();
    let _ = notified.thread_unblock();
}

/// The default actions of `notify_defaults`: the signals that have one,
/// and the flag that keeps them on until `notify_starts`.
#[cfg(target_os = "linux")]
struct NotifyDefault {
    signals: Vec<i32>,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// See `NotifyDefault`. Set once, by `notify_defaults`.
#[cfg(target_os = "linux")]
static NOTIFY_DEFAULT: std::sync::OnceLock<NotifyDefault> = std::sync::OnceLock::new();

/// Gives each signal that Go drops (`GO_DROPPED`, `GO_DROPPED_RT`) a handler
/// that does nothing. Not `SIG_IGN`: an exec resets a caught signal to its
/// default action, so a process that tsgo starts gets the default actions,
/// as from Go, also when tsgo got them ignored. An ignored SIGCHLD also
/// changes tsgo itself: the kernel then reaps each child when it ends, so
/// a wait for it fails (ECHILD). `launch` calls it before it starts the
/// worker, and `go_signal_handlers` before the run starts a process.
#[cfg(target_os = "linux")]
fn drop_go_signals() {
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signals = GO_DROPPED.iter().map(|signal| signal.as_raw());
    for signal in signals.chain(GO_DROPPED_RT) {
        let _ = signal_hook::flag::register(signal, dropped.clone());
    }
}

/// The handlers that the Go runtime sets before `main`, in a process that
/// runs the work: the signals that Go drops get a handler that does
/// nothing (`drop_go_signals`), a signal that Go throws (`GO_THROWN`) ends
/// the process (`throw`), and SIGHUP ends it by SIGHUP (`die_from_signal`;
/// as a pid 1 it exits 129, where the default action would do nothing),
/// unless it was ignored at start (`ignored_at_start`). SIGINT and SIGTERM
/// have theirs from `notify_defaults`. `main` calls it once, after the exec
/// of `set_malloc_tunables`, as Go sets its handlers in its last image, and
/// then unblocks `HELD`. Go acts in the signal handler. tsgo acts on a
/// thread, which waits on a pipe until a signal comes, and on each exit
/// path: the handler records each signal when it comes (`RECORDED`), and an
/// exit acts on a recorded signal first (`act_on_recorded`), so an exit
/// that comes before the thread acts does not lose it. The thread starts as
/// a Go runtime thread does (`GoThread`): when the OS refuses it, the run
/// ends with Go's text and exit 2. Going on without it would drop these
/// signals (dropping `signals` removes their actions, not their handlers).
#[cfg(target_os = "linux")]
fn go_signal_handlers() {
    use signal_hook::consts::SIGHUP;
    let mut ended: Vec<i32> = GO_THROWN
        .iter()
        .map(|(signal, _)| signal.as_raw())
        .collect();
    // Go keeps an ignored SIGHUP (`sigInstallGoHandler`).
    if !ignored_at_start(SIGHUP) {
        ended.push(SIGHUP);
    }
    // Before the thread's own actions, so each flag is set when the
    // thread reads its signal.
    let (recorded, _) = Arrivals::register(&ended);
    let _ = RECORDED.set(recorded);
    if let Ok(mut signals) = signal_hook::iterator::Signals::new(ended) {
        ts_goport::core::GoThread::new()
            .name(GO_SIGNALS_THREAD.to_string())
            .spawn(move || {
                // Started after the actions above (`HELD`).
                unblock_held();
                if let Some(signal) = signals.forever().next() {
                    act(signal);
                }
            });
    }
    // After those: the many handlers that do nothing.
    drop_go_signals();
}

/// The thread of `go_signal_handlers`. A launcher looks for it
/// (`ready_thread`).
#[cfg(target_os = "linux")]
const GO_SIGNALS_THREAD: &str = "go-signals";

/// The signals of `go_signal_handlers`, each with a flag that its handler
/// sets when it comes.
#[cfg(target_os = "linux")]
static RECORDED: std::sync::OnceLock<Arrivals> = std::sync::OnceLock::new();

/// Called on each exit path of a process that runs the work, before it
/// exits: acts on a signal that came (`RECORDED`) and that the `go-signals`
/// thread has not acted on yet, as Go would have acted when it came (a
/// short run such as `--version` often exits before the thread runs). The
/// output so far is written already, as Go's is.
#[cfg(target_os = "linux")]
fn act_on_recorded() {
    if let Some(recorded) = RECORDED.get() {
        act_on(recorded);
    }
}

/// Acts on the first signal of `arrivals` whose flag it takes (`act`).
/// Returns when no signal came.
#[cfg(target_os = "linux")]
fn act_on(arrivals: &Arrivals) {
    for signal in arrivals.signals() {
        if arrivals.took(signal) {
            act(signal);
        }
    }
}

/// Ends the process after `signal`, as Go's handler ends it: it prints the
/// name and exits 2 for a signal that Go throws (`throw`), and ends the
/// process by the signal for SIGHUP (`die_from_signal`). It acts once: a
/// thread that comes here after another (the `go-signals` thread and an
/// exit path, at the same signal or at two) waits while that one ends the
/// process.
#[cfg(target_os = "linux")]
fn act(signal: i32) -> ! {
    static ACTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ACTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        loop {
            std::thread::park();
        }
    }
    match GO_THROWN.iter().find(|(s, _)| s.as_raw() == signal) {
        Some((_, name)) => throw(name),
        None => die_from_signal(signal),
    }
}

/// A flag for each of some signals. A flag action (`listen`) sets it each
/// time its signal comes, and `took` takes it. signal-hook runs the actions
/// of a signal in the order of their registration, and each run of its
/// handler uses one list of actions (signal-hook-registry 1.4.8 `handler`):
/// so a flag whose action is registered before another action of the same
/// signal is set when that action runs.
#[cfg(target_os = "linux")]
struct Arrivals(Vec<(i32, std::sync::Arc<std::sync::atomic::AtomicBool>)>);

#[cfg(target_os = "linux")]
impl Arrivals {
    /// A flag for each signal of `signals`, with no action yet.
    fn new(signals: &[i32]) -> Self {
        let flag = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Arrivals(signals.iter().map(|&signal| (signal, flag())).collect())
    }

    /// `new` with the action of each flag (`listen`), and the ids of those
    /// actions.
    fn register(signals: &[i32]) -> (Self, Vec<signal_hook::SigId>) {
        let arrivals = Self::new(signals);
        let ids = signals
            .iter()
            .filter_map(|&signal| arrivals.listen(signal))
            .collect();
        (arrivals, ids)
    }

    /// Registers the action that sets the flag of `signal` when it comes.
    fn listen(&self, signal: i32) -> Option<signal_hook::SigId> {
        let (_, flag) = self.0.iter().find(|(s, _)| *s == signal)?;
        signal_hook::flag::register(signal, flag.clone()).ok()
    }

    /// Whether `signal` has a flag here.
    fn has(&self, signal: i32) -> bool {
        self.0.iter().any(|(s, _)| *s == signal)
    }

    /// The signals of the flags.
    fn signals(&self) -> impl Iterator<Item = i32> + '_ {
        self.0.iter().map(|(signal, _)| *signal)
    }

    /// Whether `signal` came since its flag was last taken. It takes the
    /// flag, so of two threads that call it, only one gets true.
    fn took(&self, signal: i32) -> bool {
        self.0
            .iter()
            .any(|(s, flag)| *s == signal && flag.swap(false, std::sync::atomic::Ordering::SeqCst))
    }
}

/// Called by `run_main` just before `notify_context`: from now on a
/// SIGINT or SIGTERM that comes is recorded as one that came while
/// `notify_context` ran (the returned flags), and its default action
/// (`NOTIFY_DEFAULT`) does nothing. The flags are registered before the
/// default actions end, so each signal finds one of them or both. A signal
/// that finds both came before `notify_context` started, and ends the run.
/// Go decides in its handler: a signal that comes before `signal.Notify`
/// asks for it ends the run, and a later one goes to the channel only.
#[cfg(target_os = "linux")]
fn notify_starts() -> (Arrivals, Vec<signal_hook::SigId>) {
    let Some(defaults) = NOTIFY_DEFAULT.get() else {
        return (Arrivals::new(&[]), Vec::new());
    };
    let during = Arrivals::register(&defaults.signals);
    defaults
        .flag
        .store(false, std::sync::atomic::Ordering::SeqCst);
    during
}

/// Called by `run_main` once `notify_context` has returned, with the flags
/// of `notify_starts`. It removes their actions first: signal-hook returns
/// only once each run of its handler that has the old list has ended, so
/// no flag changes after that. A signal that came while `notify_context`
/// ran may have come before or after it registered, so this sends it
/// again, now to `notify_context` only (Go: a signal that comes while
/// `Notify` runs goes one way or the other). A second copy of a signal
/// that `notify_context` took does nothing: its thread ends after the
/// first.
#[cfg(target_os = "linux")]
fn notify_started((during, ids): &(Arrivals, Vec<signal_hook::SigId>)) {
    for id in ids {
        signal_hook::low_level::unregister(*id);
    }
    for signal in during.signals() {
        if during.took(signal)
            && let Some(signal) = rustix::process::Signal::from_named_raw(signal)
        {
            let _ = rustix::process::kill_process(rustix::process::getpid(), signal);
        }
    }
}

/// The rest of the start of a process that runs the work, as the Go
/// runtime and the Go `syscall` package start (the handlers come earlier,
/// `go_signal_handlers`): the soft open-file limit goes up to one below
/// the hard limit (`gostd::rlimit::raise_open_file_limit`), and fd 1 is
/// checked for `O_NONBLOCK`, as Go `os.NewFile` does at start
/// (`stdio::init`).
/// PORT: other systems than Linux keep the default actions of the signals
/// (Go's tables differ there).
fn go_runtime_start() {
    ts_goport::execute::tsc::stdio::init();
    ts_goport::gostd::rlimit::raise_open_file_limit();
}

/// Ends the process after a signal that Go throws (`GO_THROWN`), as the Go
/// runtime does: it writes `name` to fd 2 with a raw write, sends `THROWN`
/// to the launcher in a worker (`send_code`) and exits 2 (`EXIT_GO_PANIC`,
/// the exit code of a Go fatal error). An error of the write (a closed or
/// broken stderr) is ignored, as in Go.
/// First it writes the stdout bytes that a report keeps on a regular file
/// (`stdio::flush_cli_stdout_at_exit`), so the file has the pieces written
/// so far, as Go's has (`os.Stdout` has no buffer). It skips them when
/// another thread holds their lock. It waits for no lock, so it cannot
/// wait for the work thread: that thread can hold the stdout or stderr
/// lock in a write that blocks on a full pipe. So it ends with `_exit`:
/// `std::process::exit` flushes the std stdout buffer when no other thread
/// holds its lock, and waits when another thread is in its cleanup.
#[cfg(target_os = "linux")]
fn throw(name: &str) -> ! {
    ts_goport::execute::tsc::stdio::flush_cli_stdout_at_exit();
    let line = format!("{name}\n");
    let _ = rustix::io::write(rustix::stdio::stderr(), line.as_bytes());
    if let Some(worker) = worker() {
        send_code(worker, THROWN);
    }
    signal_hook::low_level::exit(EXIT_GO_PANIC)
}

/// The code that a worker sends when it ends after a signal that Go throws
/// (`throw`). Its launcher then exits 2 (`EXIT_GO_PANIC`) and does not act
/// on such a signal that it still holds (`launch`): the worker has printed
/// the name once already. No exit code is negative.
#[cfg(target_os = "linux")]
const THROWN: i32 = -EXIT_GO_PANIC;

/// Ends the process after SIGHUP as the Go runtime ends it after a signal
/// with `_SigKill` in its table when no `signal.Notify` asks for it
/// (`dieFromSignal`; tsgo asks only for SIGINT and SIGTERM). First it
/// writes the kept stdout bytes, as `throw` does, so a file has the pieces
/// written so far, as Go's has. Then it ends by the signal
/// (`end_by_signal`), or, as a pid 1, exits 128 + N, as Go does there. It
/// sends no code: a launcher takes the signal from the worker's exit and
/// ends by it too. It ends with `_exit`, as `throw` does.
#[cfg(target_os = "linux")]
fn die_from_signal(signal: i32) -> ! {
    ts_goport::execute::tsc::stdio::flush_cli_stdout_at_exit();
    end_by_signal(signal);
    signal_hook::low_level::exit(128 + signal)
}

/// Ends this process by `signal` with the default action of the signal, as
/// Go `dieFromSignal` does, so the caller sees a process that the signal
/// ended. It sets the default action, unblocks the signal and raises it.
/// It returns in the pid 1 of a PID namespace (`docker run` without
/// `--init`, `unshare -pf`, `bwrap --as-pid-1`): the kernel drops a signal
/// with the default action that such a process sends itself. Go then exits
/// 128 + N, as a shell reports a process that a signal ended, and so does
/// each caller. It also returns for a signal that has no entry in
/// signal-hook's table (SIGPWR, SIGSTKFLT, the real-time signals) or that
/// the table takes as ignored (SIGIO).
/// PORT: std and rustix have no safe `SIG_DFL`. signal-hook sets it, and
/// it calls `abort` when the raise returns; in a pid 1, glibc's `abort`
/// then ends the process by SIGSEGV (rc 139, maybe a core file). So a pid
/// 1 does not raise the signal. Go raises it, and the kernel drops it.
#[cfg(target_os = "linux")]
fn end_by_signal(signal: i32) {
    if rustix::process::getpid().is_init() {
        return;
    }
    let _ = signal_hook::low_level::emulate_default_handler(signal);
}

/// Ends the process with `code` once the work has written its output. A
/// worker (see `launch`) flushes stdout and sends the code (`send_code`).
/// First it acts on a signal that came before and that the `go-signals`
/// thread has not acted on yet (`act_on_recorded`): Go acts on it when it
/// comes, so its run ends there.
fn exit(code: i32) -> ! {
    #[cfg(target_os = "linux")]
    {
        let worker = worker();
        if worker.is_some() {
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
        }
        act_on_recorded();
        if let Some(worker) = worker {
            send_code(worker, code);
        }
    }
    std::process::exit(code)
}

/// Sends `code` to the launcher of `worker` (see `launch`). First it points
/// stdout and stderr at /dev/null, so a reader of the launcher's output gets
/// its end of file when the launcher ends, while this process unmaps its
/// memory. It takes no std lock (`throw`).
#[cfg(target_os = "linux")]
fn send_code(worker: Worker, code: i32) {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open};
    use std::os::fd::AsRawFd;
    if let Ok(null) = std::fs::File::options().write(true).open("/dev/null") {
        let _ = rustix::stdio::dup2_stdout(&null);
        let _ = rustix::stdio::dup2_stderr(&null);
    }
    // The launcher's end of the pipe, by its number, through /proc. Only
    // with this process's own /proc (`own_proc`): in another one,
    // /proc/<launcher> is another process or none. The code goes only to a
    // FIFO with the device and inode that the launcher passed. The first
    // open (`O_PATH`) only names the file and opens no FIFO or device, so
    // it cannot wait or change another file. The open for writing opens
    // the checked file again through /proc/self/fd (this process's own
    // files in any /proc that shows it), not the path, and does not wait
    // for a reader (`O_NONBLOCK`). Each new file has a close-on-exec flag.
    // Without its own /proc, or when an open fails, the launcher takes the
    // code from the worker's exit.
    if !own_proc() {
        return;
    }
    let path = format!("/proc/{}/fd/{}", worker.launcher.as_raw_pid(), worker.fd);
    let Ok(file) = open(path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()) else {
        return;
    };
    let checked = fstat(&file).is_ok_and(|stat| {
        FileType::from_raw_mode(stat.st_mode) == FileType::Fifo
            && stat.st_dev == worker.dev
            && stat.st_ino == worker.ino
    });
    let again = format!("/proc/self/fd/{}", file.as_raw_fd());
    let flags = OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if checked && let Ok(pipe) = open(again, flags, Mode::empty()) {
        let _ = std::fs::File::from(pipe).write_all(&code.to_le_bytes());
    }
}

// Go: cmd/tsc/main.go:18 runMain
// PORT: the arguments are the port form of the Go `osutil.Args()` bytes (see
// `scanner_util::GO_STRING_MARKER`). The system writer writes the Go bytes
// of the output (`GoOutput`).
fn run_main(start: Instant) -> i32 {
    let args: Vec<String> = ts_goport::frontend::osutil::args()[1..].to_vec();

    if let Some(first) = args.first() {
        match first.as_str() {
            "--lsp" => return finish(catch_unwind(AssertUnwindSafe(|| run_lsp(&args[1..])))),
            "--api" => return finish(catch_unwind(AssertUnwindSafe(|| run_api(&args[1..])))),
            _ => {}
        }
    }

    // Go: ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
    #[cfg(target_os = "linux")]
    let during = notify_starts();
    let (ctx, stop) = notify_context(&context::background());
    #[cfg(target_os = "linux")]
    notify_started(&during);
    // PORT: Go `newSystem()` calls `os.Exit` on this error, so `stop` does
    // not run there either.
    let sys = match new_os_system() {
        Ok(sys) => sys,
        Err(status) => return status.code(),
    };
    let sys: Rc<dyn System> = Rc::new(sys.with_start(start));
    let result = catch_unwind(AssertUnwindSafe(|| {
        command_line(&ctx, sys.clone(), &args, &GoTsc).status.code()
    }));
    // Go: defer stop()
    stop();
    // `--showConfig` output has no trailing newline, and
    // `std::process::exit` runs no destructors, so flush here.
    let _ = sys.writer().borrow_mut().flush();
    finish(result)
}

/// Prints a Go panic and the unported counts and returns the exit code:
/// `EXIT_UNPORTED` when the run panicked or reached unported code,
/// `EXIT_GO_PANIC` after a Go panic, else `result`.
fn finish(result: std::thread::Result<i32>) -> i32 {
    let code = match result {
        Ok(code) => code,
        Err(payload) if print_go_panic(payload.as_ref()) => EXIT_GO_PANIC,
        Err(payload) => {
            note_panic(payload.as_ref());
            EXIT_UNPORTED
        }
    };
    if report_unported() {
        return EXIT_UNPORTED;
    }
    code
}

/// Prints `unported: <name> <count>` lines to stderr. True when any.
fn report_unported() -> bool {
    let unported = unported_report();
    let mut stderr = std::io::stderr().lock();
    for (name, count) in &unported {
        let _ = writeln!(stderr, "unported: {name} {count}");
    }
    !unported.is_empty()
}

/// Keeps unported panics quiet (they are counted) and prints other panics.
/// The run prints a Go panic. A panic that a Go `recover()` catches
/// (`core::go_recover`: an API request answers it) prints nothing, as in Go.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if info.payload().is::<GoPanic>() {
            return;
        }
        let message = payload_message(info.payload());
        if message.starts_with(UNPORTED_PREFIX) || ts_goport::core::in_go_recover() {
            if std::env::var_os("GOPORT_TRACE").is_some() {
                eprintln!(
                    "trace: {message}\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
            }
            return;
        }
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        // The output so far comes first, also in one file with stderr.
        ts_goport::execute::tsc::stdio::flush_cli_stdout_at_exit();
        eprintln!("tsgo: panic{location}: {message}");
        if std::env::var_os("GOPORT_TRACE").is_some() {
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
    }));
}

fn payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        String::new()
    }
}

/// Counts a caught panic. Unported panics already counted themselves; any
/// other panic is counted as `panic`.
fn note_panic(payload: &(dyn Any + Send)) {
    if !payload_message(payload).starts_with(UNPORTED_PREFIX) {
        record_unported("panic");
    }
}
