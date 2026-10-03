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
//! does (`end_by_signal`, `die_from_signal`, stdio.rs `sigpipe`); a
//! launcher whose caller ignores SIGCHLD still gets its worker's exit
//! (`drop_go_signals`); and the signals that Go unblocks at start end the
//! run also when the caller blocked them (`GO_UNBLOCKED`).
//!
//! The holds: a launcher holds each signal until its worker catches it,
//! each on its own (`caught`), and tsgo blocks the signals it handles until
//! their handlers are set (`HELD`). Each exit acts on a signal that came
//! before it (`act_on_recorded`, `act_on`).
#![cfg(target_os = "linux")]

use std::io::{Read, Seek, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};

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
/// default action that such a process gets or sends itself. SIGHUP ends
/// the run with exit 129, as Go at pin N does (Go `dieFromSignal`: 128 plus
/// the signal number): sent to tsgo with and without a worker, and sent to
/// the worker, which the signal ends. A launcher that raised the worker's
/// signal again in a pid 1 went on to `abort`, which ends a pid 1 by
/// SIGSEGV. And a launcher whose caller ignores SIGCHLD takes the exit code
/// from the worker's exit in a PID namespace with the /proc of another one
/// (`own_proc`), as Go gives it. Where `unshare -U`, `env
/// --default-signal` or `env --ignore-signal` cannot run, the test says so
/// and passes.
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

/// tsgo as the pid 1 of a PID namespace whose stdout is a pipe with no
/// reader: its write gets EPIPE, and as in Go N the run exits 141 (Go
/// `dieFromSignal`: 128 + SIGPIPE, as the raise of SIGPIPE does nothing
/// there), with and without a worker (stdio.rs `sigpipe`). A run without a
/// worker went on to signal-hook's `abort`, which ends a pid 1 by SIGSEGV.
/// Where `unshare -U` cannot run, the test says so and passes.
#[test]
fn a_pid_1_tsgo_whose_stdout_is_broken() {
    if !unshare_runs(&[]) {
        eprintln!("skipped: `unshare -Upf --map-root-user --mount-proc` cannot run here");
        return;
    }
    for launch in ["0", "1"] {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        drop(read);
        let output = Command::new("unshare")
            .args(UNSHARE)
            .args([env!("CARGO_BIN_EXE_tsgo"), "--all"])
            .env("GOPORT_LAUNCH", launch)
            .stdout(Stdio::from(write))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        // unshare exits with its child's exit code.
        assert_eq!(
            output.status.code(),
            Some(141),
            "GOPORT_LAUNCH={launch}: {output:?}"
        );
    }
}

/// A tsgo whose caller blocks SIGHUP and SIGQUIT (a signal mask, which an
/// exec keeps): Go unblocks them on each thread at its start
/// (`minitSignalMask`), and so does tsgo, in the launcher and in the
/// worker (`GO_UNBLOCKED`). So SIGQUIT ends the run with Go's text and exit
/// 2, and SIGHUP ends it by SIGHUP: sent to tsgo with and without a worker,
/// and to the worker. With the caller's mask the signals stayed pending
/// and the run went on. Where `env --default-signal` cannot run, the test
/// says so and passes.
#[test]
fn a_tsgo_whose_caller_blocks_signals() {
    use nix::sys::signal::{SigSet, Signal as NixSignal};
    let probe = Command::new("env").args(DEFAULT_HUP).arg("true").status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --default-signal=HUP` cannot run here");
        return;
    }
    let runs = [
        ("1", 0, Signal::QUIT, "SIGQUIT to the launcher"),
        ("0", 0, Signal::QUIT, "SIGQUIT to tsgo without a worker"),
        ("1", 1, Signal::HUP, "SIGHUP to the worker"),
        ("0", 0, Signal::HUP, "SIGHUP to tsgo without a worker"),
    ];
    for (launch, depth, signal, case) in runs {
        let case = format!("SIGHUP and SIGQUIT blocked, {case}");
        // A child gets the mask of the thread that starts it, so a thread
        // of its own blocks them.
        let run = std::thread::spawn(move || {
            let blocked: SigSet = [NixSignal::SIGHUP, NixSignal::SIGQUIT]
                .into_iter()
                .collect();
            blocked.thread_block().unwrap();
            // `env` execs tsgo with the default action of SIGHUP.
            let mut command = Command::new(DEFAULT_HUP[0]);
            command
                .args(&DEFAULT_HUP[1..])
                .arg(env!("CARGO_BIN_EXE_tsgo"));
            let (status, stderr, ended) = signal_run(command, launch, &[(depth, signal)], &case);
            if signal == Signal::QUIT {
                assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
                assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
            } else {
                assert_eq!(status.signal(), Some(1), "{case}: {status} {stderr}");
            }
            assert!(
                ended < Duration::from_secs(1),
                "{case}: ended {ended:?} after the signal"
            );
        });
        if let Err(panic) = run.join() {
            std::panic::resume_unwind(panic);
        }
    }
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

/// A signal that a launcher gets before its worker has set its handlers
/// waits until the worker catches it (bin/tsgo.rs `caught`), as Go sets its
/// handlers before `main`: SIGTERM waits for `notify_context` (a plain
/// compile goes on after it, as in Go), whose thread ends at it. The test
/// stops the worker (SIGSTOP) as soon as it has started (its `arg0` is a
/// worker's), sends SIGTERM to the launcher, checks that it did not reach
/// the worker (`ShdPnd` in /proc), and lets the worker go on. The thread of
/// `notify_context` must then end well before the wait's limit
/// (`HOLD_LIMIT`, 2 s after the worker's start), so a wait for another
/// thread (`ready_thread`) fails the test. SIGQUIT to the launcher then
/// ends the run with Go's text and exit 2. A worker that the test stopped
/// too late (it catches SIGTERM) ends with its launcher and the run starts
/// again, up to `ATTEMPTS` times; where no attempt is in time, the test
/// says so and passes.
#[test]
fn a_launcher_holds_a_signal_until_its_worker_catches_it() {
    const ATTEMPTS: usize = 20;
    for _ in 0..ATTEMPTS {
        let (_read, write, _) = small_pipe();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tsgo"))
            .arg("--all")
            .env("GOPORT_LAUNCH", "1")
            .stdout(Stdio::from(write))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let worker = loop {
            // Before its exec, the child is the launcher's copy.
            let started = child_of(child.id()).filter(|worker| {
                std::fs::read(format!("/proc/{worker}/cmdline"))
                    .is_ok_and(|cmdline| cmdline.starts_with(b"tsgo-worker "))
            });
            if let Some(worker) = started {
                break worker;
            }
            assert!(child.try_wait().unwrap().is_none(), "ended before a worker");
            assert!(start.elapsed() < LIMIT, "no worker in {LIMIT:?}");
        };
        let worker_pid = Pid::from_raw(worker.cast_signed()).unwrap();
        rustix::process::kill_process(worker_pid, Signal::STOP).unwrap();
        let status = loop {
            let status = std::fs::read_to_string(format!("/proc/{worker}/status")).unwrap();
            if field(&status, "State:").starts_with('T') {
                break status;
            }
            assert!(start.elapsed() < LIMIT, "the worker did not stop");
        };
        if mask(&status, "SigCgt:") & bit(Signal::TERM) != 0 {
            // Too late. The worker gets its parent-death SIGKILL.
            child.kill().unwrap();
            child.wait().unwrap();
            continue;
        }
        let launcher = Pid::from_child(&child);
        rustix::process::kill_process(launcher, Signal::TERM).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the launcher ended while its worker was stopped"
        );
        let status = std::fs::read_to_string(format!("/proc/{worker}/status")).unwrap();
        let pending = mask(&status, "ShdPnd:") & bit(Signal::TERM);
        assert_eq!(pending, 0, "sent on before the worker caught it");
        rustix::process::kill_process(worker_pid, Signal::CONT).unwrap();
        let resumed = Instant::now();
        // The worker catches SIGTERM and has no `notify_context` thread, now
        // and 50 ms later (it starts a moment after the handler).
        let mut ended_once = false;
        loop {
            let status = std::fs::read_to_string(format!("/proc/{worker}/status")).unwrap();
            let ended = mask(&status, "SigCgt:") & bit(Signal::TERM) != 0
                && !has_thread(worker, "signal.NotifyContext");
            if ended && ended_once {
                break;
            }
            ended_once = ended;
            assert!(
                resumed.elapsed() < Duration::from_secs(1),
                "the thread of notify_context still runs 1 s after the worker went on"
            );
            std::thread::sleep(Duration::from_millis(if ended { 50 } else { 1 }));
        }
        rustix::process::kill_process(launcher, Signal::QUIT).unwrap();
        let (status, stderr) = end_of(child, "hold");
        assert_eq!(status.code(), Some(2), "{status} {stderr}");
        assert!(stderr.starts_with("SIGQUIT: quit"), "{stderr}");
        return;
    }
    eprintln!("skipped: no worker was stopped before it caught SIGTERM");
}

/// A SIGINT to the whole process group (a terminal's Ctrl-C) reaches the
/// launcher and its worker. The worker's `notify_context` thread ends at its
/// own copy, so the launcher's copy waits for that thread until the wait's
/// limit (bin/tsgo.rs `HOLD_LIMIT`, 2 s after the worker's start). A signal
/// that then comes to the launcher only does not wait behind it: SIGQUIT
/// ends the run at once with Go's text and exit 2, and SIGHUP ends it by
/// SIGHUP, as in Go. Where `env --default-signal` cannot run, the test says
/// so and passes.
#[test]
fn a_launcher_does_not_hold_a_signal_behind_another() {
    let probe = Command::new("env").args(DEFAULT_HUP).arg("true").status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --default-signal=HUP` cannot run here");
        return;
    }
    for signal in [Signal::QUIT, Signal::HUP] {
        let case = format!("SIGINT to the group, then {signal:?} to the launcher");
        // `env` execs tsgo, so the started process is the launcher.
        let mut command = Command::new(DEFAULT_HUP[0]);
        command
            .args(&DEFAULT_HUP[1..])
            .arg(env!("CARGO_BIN_EXE_tsgo"))
            .process_group(0);
        let signals = [(GROUP, Signal::INT), (0, signal)];
        let (status, stderr, ended) = signal_run(command, "1", &signals, &case);
        if signal == Signal::QUIT {
            assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
            assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
        } else {
            assert_eq!(status.signal(), Some(1), "{case}: {status} {stderr}");
        }
        assert!(
            ended < Duration::from_secs(1),
            "{case}: ended {ended:?} after the signal"
        );
    }
}

/// A SIGHUP that comes to a pid 1 launcher before it starts its worker
/// waits for the launcher's handler (bin/tsgo.rs `HELD`) and ends the run
/// with exit 129, as Go does. Without the wait, the kernel dropped it (the
/// default action in a pid 1) and the run went on. The test stops the
/// launcher (SIGSTOP) once it blocks `HELD` and before it has a child,
/// sends SIGHUP and lets it go on. A launcher that the test stopped too
/// late (it has a child) ends and the run starts again, up to `ATTEMPTS`
/// times; where no attempt is in time, or where `unshare -U` or `env
/// --default-signal` cannot run, the test says so and passes.
#[test]
fn a_pid_1_launcher_keeps_a_sighup_from_before_its_worker() {
    const ATTEMPTS: usize = 40;
    if !unshare_runs(&DEFAULT_HUP) {
        eprintln!(
            "skipped: `unshare -Upf --map-root-user --mount-proc env --default-signal=HUP` cannot run here"
        );
        return;
    }
    let tsgo = env!("CARGO_BIN_EXE_tsgo");
    // SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGSTKFLT and SIGSYS.
    let held = [1, 2, 3, 15, 16, 31]
        .into_iter()
        .map(|signal| 1u64 << (signal - 1))
        .sum::<u64>();
    for _ in 0..ATTEMPTS {
        let (_read, write, _) = small_pipe();
        let mut child = Command::new("unshare")
            .args(UNSHARE)
            .args(DEFAULT_HUP)
            .args([tsgo, "--all"])
            .env("GOPORT_LAUNCH", "1")
            .stdout(Stdio::from(write))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        // unshare's child, tsgo once it blocks `HELD`: pid 1 there.
        let launcher = loop {
            let blocking = child_of(child.id()).filter(|pid| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .is_ok_and(|exe| exe == Path::new(tsgo))
                    && std::fs::read_to_string(format!("/proc/{pid}/status"))
                        .is_ok_and(|status| mask(&status, "SigBlk:") & held == held)
            });
            if let Some(pid) = blocking {
                break pid;
            }
            assert!(child.try_wait().unwrap().is_none(), "ended before tsgo");
            assert!(start.elapsed() < LIMIT, "no tsgo in {LIMIT:?}");
        };
        let launcher_pid = Pid::from_raw(launcher.cast_signed()).unwrap();
        if child_of(launcher).is_none() {
            rustix::process::kill_process(launcher_pid, Signal::STOP).unwrap();
            loop {
                let status = std::fs::read_to_string(format!("/proc/{launcher}/status")).unwrap();
                if field(&status, "State:").starts_with('T') {
                    break;
                }
                assert!(start.elapsed() < LIMIT, "the launcher did not stop");
            }
            if child_of(launcher).is_none() {
                rustix::process::kill_process(launcher_pid, Signal::HUP).unwrap();
                rustix::process::kill_process(launcher_pid, Signal::CONT).unwrap();
                let (status, stderr) = end_of(child, "SIGHUP before the worker");
                // unshare exits with its child's exit code.
                assert_eq!(status.code(), Some(129), "{status} {stderr}");
                return;
            }
        }
        // Too late. unshare kills its child (`--kill-child`).
        child.kill().unwrap();
        child.wait().unwrap();
    }
    eprintln!("skipped: no launcher was stopped before it started its worker");
}

/// SIGHUP ends tsgo by SIGHUP, as Go's `dieFromSignal` does: sent to tsgo
/// without a worker, to a launcher and to its worker. Where `env
/// --default-signal` cannot run, the test says so and passes.
#[test]
fn a_sighup_ends_tsgo_by_sighup() {
    let probe = Command::new("env").args(DEFAULT_HUP).arg("true").status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --default-signal=HUP` cannot run here");
        return;
    }
    // `env` execs tsgo; the worker is its child.
    for (launch, depth, case) in [
        ("0", 0, "SIGHUP to tsgo without a worker"),
        ("1", 0, "SIGHUP to the launcher"),
        ("1", 1, "SIGHUP to the worker"),
    ] {
        let mut command = Command::new(DEFAULT_HUP[0]);
        command
            .args(&DEFAULT_HUP[1..])
            .arg(env!("CARGO_BIN_EXE_tsgo"));
        let (status, stderr, ended) = signal_run(command, launch, &[(depth, Signal::HUP)], case);
        assert_eq!(status.signal(), Some(1), "{case}: {status} {stderr}");
        assert_eq!(stderr, "", "{case}: stderr");
        assert!(
            ended < Duration::from_secs(1),
            "{case}: ended {ended:?} after the signal"
        );
    }
}

/// tsgo catches SIGURG and SIGWINCH and drops them, as Go does (bin/tsgo.rs
/// `GO_DROPPED`), also when they were ignored at start: Go sets its handler
/// for them anyway, so a process that tsgo starts gets their default
/// actions. /proc shows them caught and not ignored in tsgo, in a launcher
/// and in its worker, and when they come the run goes on; SIGQUIT then ends
/// it with Go's text and exit 2. Where `env --ignore-signal` cannot run,
/// the test says so and passes.
#[test]
fn tsgo_catches_sigurg_and_sigwinch() {
    const IGNORE: [&str; 2] = ["env", "--ignore-signal=URG,WINCH"];
    let probe = Command::new(IGNORE[0])
        .args(&IGNORE[1..])
        .arg("true")
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `env --ignore-signal=URG,WINCH` cannot run here");
        return;
    }
    let dropped = bit(Signal::URG) | bit(Signal::WINCH);
    let runs: [(&str, &[(usize, Signal)]); 2] = [
        (
            "0",
            &[(0, Signal::URG), (0, Signal::WINCH), (0, Signal::QUIT)],
        ),
        (
            "1",
            &[
                (0, Signal::URG),
                (0, Signal::WINCH),
                (1, Signal::URG),
                (1, Signal::WINCH),
                (0, Signal::QUIT),
            ],
        ),
    ];
    for (launch, signals) in runs {
        let case = format!("SIGURG and SIGWINCH ignored at start, GOPORT_LAUNCH={launch}");
        // `env` execs tsgo; the worker is its child.
        let mut command = Command::new(IGNORE[0]);
        command.args(&IGNORE[1..]).arg(env!("CARGO_BIN_EXE_tsgo"));
        let check = |pid: u32| {
            let mut pids = vec![pid];
            if launch == "1" {
                pids.extend(child_of(pid));
                assert_eq!(pids.len(), 2, "{case}: no worker");
            }
            for pid in pids {
                let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
                assert_eq!(
                    mask(&status, "SigCgt:") & dropped,
                    dropped,
                    "{case}: {pid} caught"
                );
                assert_eq!(
                    mask(&status, "SigIgn:") & dropped,
                    0,
                    "{case}: {pid} ignored"
                );
            }
        };
        let (status, stderr, _) = signal_run_with(command, launch, signals, &case, check);
        assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
        assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
    }
}

/// A signal that comes while tsgo sets its handler for it waits for that
/// handler (bin/tsgo.rs `HELD`). signal-hook sets a handler a moment before
/// it publishes the action that the handler runs, and a signal in between
/// did nothing, so the run went on. The test makes that moment long: tsgo
/// runs under `strace -e inject=rt_sigaction:delay_exit=...`, so each
/// `rt_sigaction` call of its first thread returns 5 ms late, and the test
/// sends SIGQUIT as soon as /proc shows that tsgo catches it. The run ends
/// with Go's text and exit 2, with and without a worker (a launcher sets
/// its handlers on its first thread). Where strace cannot run, the test
/// says so and passes.
#[test]
fn a_signal_while_tsgo_sets_its_handler() {
    const STRACE: [&str; 6] = [
        "-o",
        "/dev/null",
        "-e",
        "trace=rt_sigaction",
        "-e",
        "inject=rt_sigaction:delay_exit=5000",
    ];
    let probe = Command::new("strace")
        .args(STRACE)
        .arg("true")
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: strace cannot run here");
        return;
    }
    let tsgo = env!("CARGO_BIN_EXE_tsgo");
    for launch in ["0", "1"] {
        let case = format!("GOPORT_LAUNCH={launch}");
        let (_read, write, _) = small_pipe();
        let mut child = Command::new("strace")
            .args(STRACE)
            .args([tsgo, "--all"])
            .env("GOPORT_LAUNCH", launch)
            .stdout(Stdio::from(write))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let traced = loop {
            // strace's child, once it is tsgo.
            let traced = child_of(child.id()).filter(|pid| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .is_ok_and(|exe| exe == Path::new(tsgo))
            });
            if let Some(pid) = traced {
                break pid;
            }
            assert!(child.try_wait().unwrap().is_none(), "{case}: ended");
            assert!(start.elapsed() < LIMIT, "{case}: no tsgo in {LIMIT:?}");
        };
        loop {
            let status = std::fs::read_to_string(format!("/proc/{traced}/status")).unwrap();
            if mask(&status, "SigCgt:") & bit(Signal::QUIT) != 0 {
                break;
            }
            assert!(start.elapsed() < LIMIT, "{case}: no handler in {LIMIT:?}");
        }
        let pid = Pid::from_raw(traced.cast_signed()).unwrap();
        rustix::process::kill_process(pid, Signal::QUIT).unwrap();
        // strace exits as tsgo does.
        let (status, stderr) = end_of(child, &case);
        assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
        assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
    }
}

/// A signal that comes before tsgo exits ends the run as in Go, also when
/// the thread that acts on it has not run yet: each exit acts on a
/// recorded signal first (bin/tsgo.rs `act_on_recorded`), and a launcher
/// on a signal that it still holds when its worker ends (`act_on`). Go acts
/// in the signal handler, so a short run (`--version`, a config error)
/// never exits before it. The test makes that thread late: tsgo runs under
/// `strace -e inject=recvfrom:delay_exit=...`, so the `go-signals` thread
/// (in a launcher, `forward-signals`) leaves its wait on the signal pipe
/// 5 s after the signal. The output is more than the stdout pipe holds
/// (`small_pipe`), so the run waits in a write. The test sends the signal,
/// waits until that thread has it (strace holds the thread: its state is
/// `t`), and then reads the output, so the run exits before that thread
/// acts. SIGQUIT must end the run with Go's text and exit 2, and SIGHUP by
/// SIGHUP: without a worker, and sent to a launcher whose worker ends
/// first. Before, the run ended with exit 0 (R158 and followups9 round c):
/// the first `exit_group` sets the exit code, and the late thread dies at
/// it. strace keeps the ended process until the delay is over, so the test
/// checks the outcome, not the time. Where strace cannot run, the test says
/// so and passes.
#[test]
fn an_exit_acts_on_a_signal_that_came_before_it() {
    const STRACE: [&str; 7] = [
        "-f",
        "-o",
        "/dev/null",
        "-e",
        "trace=recvfrom",
        "-e",
        "inject=recvfrom:delay_exit=5000000",
    ];
    let probe = Command::new("strace")
        .args(STRACE)
        .arg("true")
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: strace cannot run here");
        return;
    }
    let tsgo = env!("CARGO_BIN_EXE_tsgo");
    let runs = [
        ("0", Signal::QUIT, "go-signals"),
        ("0", Signal::HUP, "go-signals"),
        ("1", Signal::QUIT, "forward-signals"),
        ("1", Signal::HUP, "forward-signals"),
    ];
    for (launch, signal, thread) in runs {
        let case = format!("{signal:?}, GOPORT_LAUNCH={launch}");
        let (read, write, filled) = small_pipe();
        // `env` gives strace and tsgo the default action of SIGHUP.
        let child = Command::new(DEFAULT_HUP[0])
            .args(&DEFAULT_HUP[1..])
            .arg("strace")
            .args(STRACE)
            .args([tsgo, "--all"])
            .env("GOPORT_LAUNCH", launch)
            .stdout(Stdio::from(write))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        // tsgo sets its handlers before it writes.
        while rustix::io::ioctl_fionread(&read).unwrap() == filled {
            assert!(start.elapsed() < LIMIT, "{case}: no output in {LIMIT:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
        // strace's child, the tsgo that the signal goes to.
        let traced = child_of(child.id())
            .filter(|pid| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .is_ok_and(|exe| exe == Path::new(tsgo))
            })
            .unwrap_or_else(|| panic!("{case}: no tsgo under strace"));
        let pid = Pid::from_raw(traced.cast_signed()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
        // Twice, 100 ms apart: a thread that gets the signal itself stops
        // in strace a moment too.
        let held = || thread_state(traced, thread) == Some(b't');
        loop {
            if held() {
                std::thread::sleep(Duration::from_millis(100));
                if held() {
                    break;
                }
            }
            assert!(
                start.elapsed() < LIMIT,
                "{case}: {thread} did not leave its wait"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = std::fs::File::from(read);
        let drain = std::thread::spawn(move || std::io::copy(&mut &output, &mut std::io::sink()));
        // strace exits as tsgo does.
        let (status, stderr) = end_of(child, &case);
        let _ = drain.join();
        if signal == Signal::QUIT {
            assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
            assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
        } else {
            assert_eq!(status.signal(), Some(1), "{case}: {status} {stderr}");
        }
    }
}

/// A SIGINT or SIGTERM that comes before `notify_context` ends a run
/// without a worker by the signal, as Go's handler does (`_SigKill`), also
/// a short run (bin/tsgo.rs `notify_defaults`). tsgo runs `--version` under
/// `strace -e inject=...`: each `rt_sigaction` call returns 5 ms late, so
/// the handlers take a few hundred milliseconds, and the `go-signals`
/// thread leaves its wait on the signal pipe 5 s after a signal. The test
/// sends the signal as soon as /proc shows that tsgo catches it. Followups9
/// round c acted on it on the `go-signals` thread, so the run exited 0
/// first. Where strace cannot run, the test says so and passes.
#[test]
fn a_signal_before_notify_context_ends_a_short_run() {
    const STRACE: [&str; 9] = [
        "-f",
        "-o",
        "/dev/null",
        "-e",
        "trace=rt_sigaction,recvfrom",
        "-e",
        "inject=rt_sigaction:delay_exit=5000",
        "-e",
        "inject=recvfrom:delay_exit=5000000",
    ];
    let probe = Command::new("strace")
        .args(STRACE)
        .arg("true")
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: strace cannot run here");
        return;
    }
    let tsgo = env!("CARGO_BIN_EXE_tsgo");
    for signal in [Signal::INT, Signal::TERM] {
        let case = format!("{signal:?}");
        let child = Command::new("strace")
            .args(STRACE)
            .args([tsgo, "--version"])
            .env("GOPORT_LAUNCH", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let traced = loop {
            let traced = child_of(child.id()).filter(|pid| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .is_ok_and(|exe| exe == Path::new(tsgo))
            });
            if let Some(pid) = traced {
                break pid;
            }
            assert!(start.elapsed() < LIMIT, "{case}: no tsgo in {LIMIT:?}");
        };
        loop {
            let status = std::fs::read_to_string(format!("/proc/{traced}/status")).unwrap();
            if mask(&status, "SigCgt:") & bit(signal) != 0 {
                break;
            }
            assert!(start.elapsed() < LIMIT, "{case}: no handler in {LIMIT:?}");
        }
        let pid = Pid::from_raw(traced.cast_signed()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
        // strace ends as tsgo does.
        let (status, stderr) = end_of(child, &case);
        assert_eq!(
            status.signal(),
            Some(signal.as_raw()),
            "{case}: {status} {stderr}"
        );
    }
}

/// The state letter of the thread named `name` of the process `pid`, from
/// /proc/<pid>/task/<tid>/stat (`<tid> (<name>) <state> ...`).
fn thread_state(pid: u32, name: &str) -> Option<u8> {
    let name = &name.as_bytes()[..name.len().min(15)];
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    tasks.flatten().find_map(|task| {
        let comm = std::fs::read(task.path().join("comm")).ok()?;
        if comm.strip_suffix(b"\n") != Some(name) {
            return None;
        }
        let stat = std::fs::read(task.path().join("stat")).ok()?;
        let name_end = stat.iter().rposition(|&b| b == b')')?;
        stat.get(name_end + 2).copied()
    })
}

/// A launcher whose worker cannot start runs the work itself (bin/tsgo.rs
/// `launch`), as a run that never was a launcher: a plain compile goes on
/// after SIGINT and SIGTERM, and SIGQUIT ends it with Go's text and exit 2.
/// The test runs tsgo from a memfd, whose path (`/memfd:tsgo (deleted)`)
/// cannot start a worker, and where the exec of `set_malloc_tunables` fails
/// too: the run then ended by SIGPIPE at the second signal (that exec gave
/// SIGPIPE its default action). Where a memfd cannot run, the test says so
/// and passes.
#[test]
fn a_launcher_whose_worker_cannot_start() {
    let memfd = rustix::fs::memfd_create("tsgo", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut file = std::fs::File::from(memfd);
    let mut bin = std::fs::File::open(env!("CARGO_BIN_EXE_tsgo")).unwrap();
    std::io::copy(&mut bin, &mut file).unwrap();
    // The started process's own copy of the memfd, which it closes at exec.
    let tsgo = format!("/proc/self/fd/{}", file.as_raw_fd());
    let probe = Command::new(&tsgo)
        .arg("--version")
        .env("GOPORT_LAUNCH", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|status| status.success()) {
        eprintln!("skipped: tsgo cannot run from a memfd here");
        return;
    }
    let case = "no worker";
    let signals = [(0, Signal::INT), (0, Signal::TERM), (0, Signal::QUIT)];
    let (status, stderr, _) = signal_run_with(Command::new(&tsgo), "1", &signals, case, |pid| {
        assert_eq!(child_of(pid), None, "{case}: a worker started");
    });
    assert_eq!(status.code(), Some(2), "{case}: {status} {stderr}");
    assert!(stderr.starts_with("SIGQUIT: quit"), "{case}: {stderr}");
}

/// A worker opens the launcher's end of the pipe only after it has checked
/// the file at the number: a FIFO with the device and inode that the
/// launcher passed (bin/tsgo.rs `send_code`). The first open (`O_PATH`)
/// opens no end of a FIFO. Here the file at the number is a named FIFO
/// with a reader or a writer that waits in its open for the other end:
/// with the inode of another file, the worker must not wake them. With its
/// own inode, the worker sends the code to the reader.
#[test]
fn a_worker_opens_only_the_launchers_pipe() {
    use rustix::fs::{Mode, OFlags};
    let this = std::process::id();
    let dir = TempDir::new(std::env::temp_dir().join(format!("tsgo_launch-open-{this}")));
    let path = dir.0.join("fifo");
    rustix::fs::mkfifoat(rustix::fs::CWD, &path, Mode::RUSR | Mode::WUSR).unwrap();
    // The launcher's file: the FIFO, by a file that opens neither end.
    let named = rustix::fs::open(&path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap();
    let stat = rustix::fs::fstat(&named).unwrap();
    let runs = [
        (
            "a waiting reader, another inode",
            false,
            stat.st_ino + 1,
            None,
        ),
        (
            "a waiting writer, another inode",
            true,
            stat.st_ino + 1,
            None,
        ),
        ("a waiting reader, its inode", false, stat.st_ino, Some(0)),
    ];
    for (case, writer, ino, sent) in runs {
        let waiting = waiting_open(&path, writer);
        let arg0 = fields_arg0(this, &named, stat.st_dev, ino);
        let (status, stdout) = run(&arg0, &["--version"], "0", case);
        assert_eq!(status.code(), Some(0), "{case}: {stdout}");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            waiting.is_finished(),
            sent.is_some(),
            "{case}: the worker opened the FIFO"
        );
        // Opens the other end, so an open that still waits ends.
        let _ = std::fs::File::options()
            .read(writer)
            .write(!writer)
            .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
            .open(&path);
        let expected = sent.map_or_else(Vec::new, |code: i32| code.to_le_bytes().to_vec());
        assert_eq!(waiting.join().unwrap(), expected, "{case}: sent");
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
/// down (or to its process group: `GROUP`, for a command that starts in a
/// group of its own) once tsgo has written some output, 300 ms apart, and returns how
/// the run ended, its stderr and the time from the last signal to the end.
/// The output is more than the stdout pipe holds (`small_pipe`) and the
/// test does not read it, so tsgo cannot end before the first signal
/// comes, and its handlers are set before it writes. A run that ends
/// before the last signal, or has not ended 60 s after it, fails the test.
fn signal_run(
    command: Command,
    launch: &str,
    signals: &[(usize, Signal)],
    case: &str,
) -> (ExitStatus, String, Duration) {
    signal_run_with(command, launch, signals, case, |_| {})
}

/// `signal_run`, which calls `check` with the started process's pid once
/// tsgo has written some output, before the first signal.
fn signal_run_with(
    mut command: Command,
    launch: &str,
    signals: &[(usize, Signal)],
    case: &str,
    check: impl FnOnce(u32),
) -> (ExitStatus, String, Duration) {
    let (read, write, filled) = small_pipe();
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
    while rustix::io::ioctl_fionread(&read).unwrap() == filled {
        assert!(
            child.try_wait().unwrap().is_none(),
            "{case}: ended before output"
        );
        assert!(start.elapsed() < LIMIT, "{case}: no output in {LIMIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    check(child.id());
    for (i, &(depth, signal)) in signals.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "{case}: ended before {signal:?}"
        );
        if depth == GROUP {
            let group = Pid::from_raw(child.id().cast_signed()).unwrap();
            rustix::process::kill_process_group(group, signal).unwrap();
            continue;
        }
        let mut target = child.id();
        for _ in 0..depth {
            target = child_of(target).unwrap_or_else(|| panic!("{case}: no process to signal"));
        }
        let pid = rustix::process::Pid::from_raw(target.cast_signed()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
    }
    let sent = Instant::now();
    let (status, stderr) = end_of(child, case);
    (status, stderr, sent.elapsed())
}

/// How the run `child` ended and its stderr. A run that has not ended
/// after `LIMIT` fails the test.
fn end_of(mut child: Child, case: &str) -> (ExitStatus, String) {
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{case}: the run did not end in {LIMIT:?} after the signal");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, stderr)
}

/// How long a test waits for output or for the end of a run.
const LIMIT: Duration = Duration::from_secs(60);

/// The `depth` of `signal_run` that sends a signal to the process group of
/// the started process.
const GROUP: usize = usize::MAX;

/// A pipe whose write end takes 4 KiB more and then waits, with any page
/// size, and the bytes it holds already. `--all` writes about 19 KB. The
/// smallest pipe is one page (4 KiB, or 64 KiB on some hosts), so the test
/// fills all of it but 4 KiB.
fn small_pipe() -> (OwnedFd, OwnedFd, u64) {
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
    let size = rustix::pipe::fcntl_setpipe_size(&write, 4096).unwrap();
    let filled = size.saturating_sub(4096);
    let fill = std::fs::File::from(write.try_clone().unwrap());
    (&fill).write_all(&vec![b'\n'; filled]).unwrap();
    (read, write, filled as u64)
}

/// The bit of `signal` in a signal mask of /proc/<pid>/status.
fn bit(signal: Signal) -> u64 {
    1 << (signal.as_raw() - 1)
}

/// The value of the line `name` of /proc/<pid>/status `status`.
fn field<'a>(status: &'a str, name: &str) -> &'a str {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .unwrap_or_else(|| panic!("no {name} in {status}"))
        .trim()
}

/// The signal mask of the line `name` (`SigCgt:`, `ShdPnd:`) of
/// /proc/<pid>/status `status`.
fn mask(status: &str, name: &str) -> u64 {
    u64::from_str_radix(field(status, name), 16).unwrap()
}

/// Whether the process `pid` has a thread named `name` (the kernel keeps
/// the first 15 bytes of a name).
fn has_thread(pid: u32, name: &str) -> bool {
    let name = &name.as_bytes()[..name.len().min(15)];
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return false;
    };
    tasks.flatten().any(|task| {
        std::fs::read(task.path().join("comm"))
            .is_ok_and(|comm| comm.strip_suffix(b"\n") == Some(name))
    })
}

/// A thread that opens the FIFO at `path` for writing (`writer`) or for
/// reading and waits there for the other end. A reader then reads to the
/// end of file. The thread returns what it read. This returns once the
/// thread waits (its state in /proc is sleeping).
fn waiting_open(path: &Path, writer: bool) -> std::thread::JoinHandle<Vec<u8>> {
    let path = path.to_path_buf();
    let (send, receive) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        send.send(nix::unistd::gettid().as_raw()).unwrap();
        let mut file = std::fs::File::options()
            .read(!writer)
            .write(writer)
            .open(&path)
            .unwrap();
        let mut read = Vec::new();
        if !writer {
            file.read_to_end(&mut read).unwrap();
        }
        read
    });
    let tid = receive.recv().unwrap();
    let start = Instant::now();
    loop {
        let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).unwrap();
        if field(&status, "State:").starts_with('S') {
            return thread;
        }
        assert!(start.elapsed() < LIMIT, "the open does not wait");
        std::thread::sleep(Duration::from_millis(1));
    }
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

/// The pid of a child of `pid`: from the `children` file of each of its
/// threads in /proc, or, on a kernel without those files, from the parent
/// of each process.
fn child_of(pid: u32) -> Option<u32> {
    let mut listed = false;
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
        .ok()?
        .flatten()
    {
        if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
            listed = true;
            if let Some(child) = children.split_whitespace().next() {
                return child.parse().ok();
            }
        }
    }
    if listed {
        return None;
    }
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
