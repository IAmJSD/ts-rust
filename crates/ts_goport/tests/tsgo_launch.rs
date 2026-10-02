//! The `tsgo` worker check (bin/tsgo.rs `launch`, `worker` and
//! `send_code`). A process is a worker only when its `arg0` is
//! `tsgo-worker <launcher> <fd> <device> <inode>` and its parent is that
//! launcher. A worker sends its exit code on the launcher's pipe, and only
//! to a FIFO with that device and inode, which it opens without waiting
//! for a reader. A tsgo whose `arg0` names a launcher that is not its
//! parent runs as a plain tsgo: it gives its own output and exit code and
//! sends nothing. When that launcher has ended (gone, or a zombie), the
//! tsgo is a worker whose launcher died before the check: it kills itself
//! before it does any work. Any other tsgo runs as a plain tsgo.
//!
//! This test process takes the place of the launcher: it holds the read
//! end of a pipe, which the tsgo opens through /proc as a worker does.
//!
//! In a PID namespace that has the /proc of another one, /proc/<pid> is
//! another process: the launcher and the worker do not read it there
//! (`own_proc`).
//!
//! The ends by a signal: as the pid 1 of a PID namespace, where the kernel
//! drops a signal with the default action, tsgo exits 128 + N where Go
//! does (`end_by_signal`, `die_from_signal`); and a launcher whose caller
//! ignores SIGCHLD still gets its worker's exit (`drop_go_signals`).
#![cfg(target_os = "linux")]

use std::io::{Read, Seek};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::Signal;

/// What a tsgo run gives.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    /// The exit code and some stdout, and the code on the pipe.
    Sends,
    /// The exit code and some stdout, and nothing on the pipe.
    Plain,
    /// The end by SIGKILL, with no stdout and nothing on the pipe.
    Killed,
}

#[test]
fn a_tsgo_started_from_a_worker_is_not_a_worker() {
    let this = std::process::id();
    let parent = std::os::unix::process::parent_id();
    let dir = TempDir::new(std::env::temp_dir().join(format!("tsgo_launch-{this}")));
    // Every file here has a close-on-exec flag: the tsgo gets none.
    // A regular file: the worker writes only to a FIFO.
    let mut file = std::fs::File::from(
        rustix::fs::memfd_create("tsgo_launch", rustix::fs::MemfdFlags::CLOEXEC).unwrap(),
    );
    // A named FIFO with no reader. An open without `O_NONBLOCK` waits for a
    // reader (an anonymous pipe does not wait), so the worker must open it
    // without waiting, send nothing and end.
    let fifo = no_reader_fifo(&dir.0.join("fifo"));
    // Launchers that have ended before the tsgo's parent check: one is
    // reaped and its pid is free, one is a zombie of this process.
    let reaped = ended_process(true);
    let zombie = ended_process(false);
    // `--version` exits 0 and an unknown option exits 1, so the exit code
    // of each run shows that it did the work.
    let cases: [(&[&str], i32); 2] = [(&["--version"], 0), (&["--noSuchOption"], 1)];
    for (args, code) in cases {
        // `GOPORT_LAUNCH=1`: a tsgo that is not a worker starts its own.
        for launch in ["0", "1"] {
            let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
            drop(write);
            let stat = rustix::fs::fstat(&read).unwrap();
            let (dev, ino) = (stat.st_dev, stat.st_ino);
            let runs = [
                (worker_arg0(this, &read), Outcome::Sends),
                // The named launcher is not the parent, and it is alive.
                (worker_arg0(parent, &read), Outcome::Plain),
                // The named launcher has ended. With both parent checks
                // removed, these run as workers.
                (worker_arg0(reaped.id(), &read), Outcome::Killed),
                (worker_arg0(zombie.id(), &read), Outcome::Killed),
                // The file at the number is not the launcher's pipe.
                (fields_arg0(this, &read, dev, ino + 1), Outcome::Plain),
                (fields_arg0(this, &read, dev + 1, ino), Outcome::Plain),
                (worker_arg0(this, &file), Outcome::Plain),
                (worker_arg0(this, &fifo), Outcome::Plain),
                // The R152 form, without the device and inode.
                (
                    format!("tsgo-worker {this} {}", read.as_raw_fd()),
                    Outcome::Plain,
                ),
                ("tsgo".to_string(), Outcome::Plain),
            ];
            for (arg0, outcome) in runs {
                let case = format!("{args:?} GOPORT_LAUNCH={launch} arg0 {arg0:?}");
                let (status, stdout) = run(&arg0, args, launch, &case);
                if outcome == Outcome::Killed {
                    assert_eq!(status.signal(), Some(9), "{case}: {status} {stdout}");
                    assert_eq!(stdout, "", "{case}: output");
                } else {
                    assert_eq!(status.code(), Some(code), "{case}: {stdout}");
                    assert!(!stdout.is_empty(), "{case}: no output");
                }
                // The tsgo has ended and no process has the pipe open for
                // writing (a worker of the tsgo never opens it), so this
                // reads what the tsgo sent and then the end of file.
                let mut sent = Vec::new();
                let mut pipe = std::fs::File::from(read.try_clone().unwrap());
                pipe.read_to_end(&mut sent).unwrap();
                let expected = if outcome == Outcome::Sends {
                    code.to_le_bytes().to_vec()
                } else {
                    Vec::new()
                };
                assert_eq!(sent, expected, "{case}: sent on the pipe");
                file.rewind().unwrap();
                let mut sent = Vec::new();
                file.read_to_end(&mut sent).unwrap();
                assert_eq!(sent, Vec::<u8>::new(), "{case}: sent to the file");
            }
        }
    }
    // Reaps the zombie.
    zombie.wait();
}

/// tsgo in a PID namespace that has the test's /proc, as with `unshare -pf`
/// without `--mount-proc`. bash is the namespace's pid 1, so a tsgo that it
/// starts is pid 2 and its worker pid 3. On a host, /proc/3 is then a
/// kernel thread whose parent has pid 2, so it looks like a live worker
/// that does not catch the signal. The launcher sends a signal on at once
/// there: SIGQUIT ends the run with Go's text and exit 2 well before the
/// launcher's wait limit (bin/tsgo.rs `HOLD_LIMIT`, 2 s), as with the
/// test's own /proc. And a tsgo whose `arg0` names a launcher that is gone
/// in its namespace (the test's pid, live in /proc) kills itself.
/// `unshare -U` needs no privilege on most hosts. Where it cannot run, the
/// test says so and passes.
#[test]
fn a_tsgo_with_the_proc_of_another_pid_namespace() {
    const UNSHARE: [&str; 4] = ["-Upf", "--map-root-user", "--kill-child", "bash"];
    let probe = Command::new("unshare")
        .args(UNSHARE)
        .args(["-c", "exit 0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `unshare -Upf --map-root-user bash` cannot run here");
        return;
    }
    let tsgo = env!("CARGO_BIN_EXE_tsgo");
    // The test's own /proc: the started process is the launcher.
    quit_launcher(Command::new(tsgo), 0, "own /proc");
    // The launcher is the child of bash, the child of unshare.
    let mut unshare = Command::new("unshare");
    unshare
        .args(UNSHARE)
        .args(["-c", "\"$0\" \"$@\"; exit $?", tsgo]);
    quit_launcher(unshare, 2, "another /proc");
    // The worker check. The subshell makes the tsgo pid 2, not pid 1 (the
    // first process of a namespace does not get its own SIGKILL).
    let this = std::process::id();
    assert!(this > 2, "the test's pid {this} is a pid of the namespace");
    let output = Command::new("unshare")
        .args(UNSHARE)
        .args(["-c", "(exec -a \"$1\" \"$0\" --version); exit $?", tsgo])
        .arg(format!("tsgo-worker {this} 0 0 0"))
        .env("GOPORT_LAUNCH", "0")
        .stderr(Stdio::null())
        .output()
        .unwrap();
    // bash gives 128 + 9 for a child that SIGKILL ended.
    assert_eq!(output.status.code(), Some(137), "{output:?}");
    assert_eq!(output.stdout, b"", "worker check: output");
}

/// tsgo as the pid 1 of a PID namespace (`unshare -pf --mount-proc`, as
/// `docker run` without `--init`): the kernel drops each signal with the
/// default action that such a process gets or sends itself. As in Go N,
/// SIGHUP ends the run with exit 129 (Go `dieFromSignal`: 128 + N): sent
/// to tsgo with and without a worker, and sent to the worker, which the
/// signal ends. A launcher that raised the worker's signal again in a pid 1
/// went on to `abort`, which ends a pid 1 by SIGSEGV. And a launcher whose
/// caller ignores SIGCHLD takes the exit code from the worker's exit in a
/// PID namespace with the /proc of another one (`own_proc`), as Go gives
/// it. Where `unshare -U`, `env --default-signal` or `env --ignore-signal`
/// cannot run, the test says so and passes.
#[test]
fn a_tsgo_that_is_pid_1_of_a_pid_namespace() {
    if !unshare_runs(&DEFAULT_HUP) {
        eprintln!(
            "skipped: `unshare -Upf --map-root-user --mount-proc env --default-signal=HUP` cannot run here"
        );
        return;
    }
    let tsgo = env!("CARGO_BIN_EXE_tsgo");
    let unshare = || {
        let mut unshare = Command::new("unshare");
        unshare.args(UNSHARE).args(DEFAULT_HUP).arg(tsgo);
        unshare
    };
    // unshare's child is tsgo, pid 1; the worker is its child.
    for (launch, depth, case) in [
        ("1", 1, "pid 1, SIGHUP to the launcher"),
        ("1", 2, "pid 1, SIGHUP to the worker"),
        ("0", 1, "pid 1, SIGHUP to tsgo without a worker"),
    ] {
        let (status, stderr, ended) = signal_run(unshare(), launch, &[(depth, Signal::HUP)], case);
        // unshare exits with its child's exit code.
        assert_eq!(status.code(), Some(129), "{case}: {status} {stderr}");
        assert_eq!(stderr, "", "{case}: stderr");
        assert!(
            ended < Duration::from_secs(1),
            "{case}: ended {ended:?} after the signal"
        );
    }
    let ignored = Command::new("env")
        .args(["--ignore-signal=CHLD", "true"])
        .status();
    if !ignored.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --ignore-signal=CHLD` cannot run here");
        return;
    }
    // Without --mount-proc: the /proc of the test's namespace. The worker
    // sends no code there.
    let output = Command::new("unshare")
        .args(&UNSHARE[..3])
        .args(["env", "--ignore-signal=CHLD", tsgo, "--version"])
        .env("GOPORT_LAUNCH", "1")
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "SIGCHLD ignored: {output:?}");
    assert!(
        output.stdout.starts_with(b"Version "),
        "SIGCHLD ignored: {output:?}"
    );
}

/// A launcher whose caller ignores SIGCHLD (`env --ignore-signal=CHLD`):
/// without its own handler for SIGCHLD the kernel reaps the worker at its
/// end and the launcher's wait fails, so a worker that a signal ends gave
/// exit 70. Go gets the end of each child it starts, and so does the
/// launcher: it ends by the worker's signal, as without a worker. Where
/// `env --ignore-signal` cannot run, the test says so and passes.
#[test]
fn a_tsgo_whose_caller_ignores_sigchld() {
    let probe = Command::new("env")
        .args(["--ignore-signal=CHLD", "true"])
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --ignore-signal=CHLD` cannot run here");
        return;
    }
    let mut command = Command::new("env");
    command.args(["--ignore-signal=CHLD", env!("CARGO_BIN_EXE_tsgo")]);
    // env runs tsgo in its own process, the launcher; the worker is its
    // child.
    let case = "SIGCHLD ignored, SIGKILL to the worker";
    let (status, stderr, _) = signal_run(command, "1", &[(1, Signal::KILL)], case);
    assert_eq!(status.signal(), Some(9), "{case}: {status} {stderr}");
}

/// A tsgo whose caller ignores SIGHUP (`nohup`, here `env
/// --ignore-signal=HUP`): Go keeps an ignored SIGHUP at start, so the run
/// goes on after it, with and without a worker and also when the worker
/// gets it; SIGQUIT then ends it with Go's text and exit 2. Where `env
/// --ignore-signal` cannot run, the test says so and passes.
#[test]
fn a_tsgo_whose_caller_ignores_sighup() {
    let probe = Command::new("env")
        .args(["--ignore-signal=HUP", "true"])
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --ignore-signal=HUP` cannot run here");
        return;
    }
    // env runs tsgo in its own process; the worker is its child.
    let runs: [(&str, &[(usize, Signal)]); 2] = [
        (
            "1",
            &[(0, Signal::HUP), (1, Signal::HUP), (0, Signal::QUIT)],
        ),
        ("0", &[(0, Signal::HUP), (0, Signal::QUIT)]),
    ];
    for (launch, signals) in runs {
        let case = format!("SIGHUP ignored, GOPORT_LAUNCH={launch}");
        let mut command = Command::new("env");
        command.args(["--ignore-signal=HUP", env!("CARGO_BIN_EXE_tsgo")]);
        let (status, stderr, _) = signal_run(command, launch, signals, &case);
        assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
        assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
    }
}

/// Starts `command` with `--all` and `GOPORT_LAUNCH=1`, sends SIGQUIT to
/// its launcher once the worker has written some output, and checks that
/// the run ends soon after with Go's text and exit 2. The launcher is the
/// started process or its descendant `depth` levels down.
fn quit_launcher(command: Command, depth: usize, case: &str) {
    let (status, stderr, ended) = signal_run(command, "1", &[(depth, Signal::QUIT)], case);
    assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
    assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
    assert!(
        ended < Duration::from_secs(1),
        "{case}: ended {ended:?} after SIGQUIT"
    );
}

/// Starts `command` with `--all` and `GOPORT_LAUNCH=launch`, sends each
/// signal of `signals` to the started process's descendant `depth` levels
/// down once tsgo has written some output, 300 ms apart, and returns how
/// the run ended, its stderr and the time from the last signal to the end.
/// The output is more than the stdout pipe holds and the test does not
/// read it, so tsgo cannot end before the first signal comes, and its
/// handlers are set before it writes. A run that ends before the last
/// signal, or has not ended 60 s after it, fails the test.
fn signal_run(
    mut command: Command,
    launch: &str,
    signals: &[(usize, Signal)],
    case: &str,
) -> (ExitStatus, String, Duration) {
    const LIMIT: Duration = Duration::from_secs(60);
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
    let size = rustix::pipe::fcntl_setpipe_size(&write, 4096).unwrap();
    // `--all` writes about 19 KB.
    assert!(size <= 8192, "{case}: the pipe holds {size} bytes");
    let mut child = command
        .arg("--all")
        .env("GOPORT_LAUNCH", launch)
        .stdout(Stdio::from(write))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Closes the test's copy of the write end.
    drop(command);
    let start = Instant::now();
    while rustix::io::ioctl_fionread(&read).unwrap() == 0 {
        assert!(
            child.try_wait().unwrap().is_none(),
            "{case}: ended before output"
        );
        assert!(start.elapsed() < LIMIT, "{case}: no output in {LIMIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    for (i, &(depth, signal)) in signals.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "{case}: ended before {signal:?}"
        );
        let mut target = child.id();
        for _ in 0..depth {
            target = child_of(target).unwrap_or_else(|| panic!("{case}: no process to signal"));
        }
        let pid = rustix::process::Pid::from_raw(target.cast_signed()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
    }
    let sent = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if sent.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{case}: the run did not end in {LIMIT:?} after the signal");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let ended = sent.elapsed();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, stderr, ended)
}

/// `env` with the default action of SIGHUP, also when the test runs under
/// `nohup`: tsgo keeps an ignored SIGHUP, as Go does. `env` execs its
/// command, so a pid 1 stays pid 1.
const DEFAULT_HUP: [&str; 2] = ["env", "--default-signal=HUP"];

/// `unshare` with a new user and PID namespace and its own /proc, as
/// `docker run` without `--init`: its child is pid 1 there.
const UNSHARE: [&str; 4] = ["-Upf", "--map-root-user", "--kill-child", "--mount-proc"];

/// Whether `unshare` (`UNSHARE`) can run `wrap` with `true` here.
fn unshare_runs(wrap: &[&str]) -> bool {
    Command::new("unshare")
        .args(UNSHARE)
        .args(wrap)
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The pid of a child of `pid`, from /proc.
fn child_of(pid: u32) -> Option<u32> {
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let child = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = std::fs::read(format!("/proc/{child}/stat")).ok()?;
            // `<pid> (<name>) <state> <ppid> ...`: the name can hold ") ".
            let name_end = stat.iter().rposition(|&b| b == b')')?;
            let rest = std::str::from_utf8(stat.get(name_end + 2..)?).ok()?;
            let parent = rest.split(' ').nth(1)?.parse::<u32>().ok()?;
            (parent == pid).then_some(child)
        })
}

/// A process that has ended: a tsgo `--version`. When `reap` is false, it
/// stays a zombie until `EndedProcess::wait`.
fn ended_process(reap: bool) -> EndedProcess {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tsgo"))
        .arg("--version")
        .env("GOPORT_LAUNCH", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    if reap {
        child.wait().unwrap();
    } else {
        // Waits for the end without reaping (`NOWAIT`).
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        waitid(WaitId::Pid(Pid::from_child(&child)), options).unwrap();
    }
    EndedProcess(child)
}

/// A process of `ended_process`.
struct EndedProcess(Child);

impl EndedProcess {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn wait(mut self) {
        self.0.wait().unwrap();
    }
}

/// A new directory that is removed when the test ends, also when it fails.
struct TempDir(PathBuf);

impl TempDir {
    fn new(path: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs tsgo with the `arg0` `name` and `args`, and returns its exit status
/// and stdout. A tsgo that has not ended after `LIMIT` fails the test: a
/// worker whose open of the pipe waits for a reader never ends.
fn run(name: &str, args: &[&str], launch: &str, case: &str) -> (ExitStatus, String) {
    const LIMIT: Duration = Duration::from_secs(60);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tsgo"))
        .arg0(name)
        .args(args)
        .env("GOPORT_LAUNCH", launch)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{case}: tsgo did not end in {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    (status, stdout)
}

/// The `arg0` of a worker of `launcher` with the file at `fd`.
fn worker_arg0(launcher: u32, fd: &impl AsFd) -> String {
    let stat = rustix::fs::fstat(fd).unwrap();
    fields_arg0(launcher, fd, stat.st_dev, stat.st_ino)
}

/// The `arg0` of a worker of `launcher` with the number of `fd` and the
/// device and inode `dev` and `ino`.
fn fields_arg0(launcher: u32, fd: &impl AsFd, dev: u64, ino: u64) -> String {
    format!(
        "tsgo-worker {launcher} {} {dev} {ino}",
        fd.as_fd().as_raw_fd()
    )
}

/// The write end of a new named FIFO at `path` that has no reader: it
/// opens a reader first, so the open for writing does not wait, and then
/// closes it.
fn no_reader_fifo(path: &Path) -> std::fs::File {
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    rustix::fs::mkfifoat(rustix::fs::CWD, path, mode).unwrap();
    let reader = std::fs::File::options()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
        .open(path)
        .unwrap();
    let writer = std::fs::File::options().write(true).open(path).unwrap();
    drop(reader);
    writer
}
