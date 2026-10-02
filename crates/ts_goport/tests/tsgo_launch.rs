//! The `tsgo` worker check (bin/tsgo.rs `launch`, `worker` and
//! `send_code`). A process is a worker only when its `arg0` is
//! `tsgo-worker <launcher> <fd> <device> <inode>` and its parent is that
//! launcher. A worker sends its exit code on the launcher's pipe, and only
//! to a FIFO with that device and inode, which it opens without waiting
//! for a reader. Any other tsgo, also one whose `arg0` names a launcher
//! that is not its parent, runs as a plain tsgo: it gives its own output
//! and exit code and sends nothing.
//!
//! This test process takes the place of the launcher: it holds the read
//! end of a pipe, which the tsgo opens through /proc as a worker does.
#![cfg(target_os = "linux")]

use std::io::{Read, Seek};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
            // (arg0, whether the tsgo sends `code` on the pipe)
            let runs = [
                (worker_arg0(this, &read), true),
                // The named launcher is not the parent.
                (worker_arg0(parent, &read), false),
                // The file at the number is not the launcher's pipe.
                (fields_arg0(this, &read, dev, ino + 1), false),
                (fields_arg0(this, &read, dev + 1, ino), false),
                (worker_arg0(this, &file), false),
                (worker_arg0(this, &fifo), false),
                // The R152 form, without the device and inode.
                (format!("tsgo-worker {this} {}", read.as_raw_fd()), false),
                ("tsgo".to_string(), false),
            ];
            for (arg0, sends) in runs {
                let case = format!("{args:?} GOPORT_LAUNCH={launch} arg0 {arg0:?}");
                let (status, stdout) = run(&arg0, args, launch, &case);
                assert_eq!(status, Some(code), "{case}: {stdout}");
                assert!(!stdout.is_empty(), "{case}: no output");
                // The tsgo has ended and no process has the pipe open for
                // writing (a worker of the tsgo never opens it), so this
                // reads what the tsgo sent and then the end of file.
                let mut sent = Vec::new();
                let mut pipe = std::fs::File::from(read.try_clone().unwrap());
                pipe.read_to_end(&mut sent).unwrap();
                let expected = if sends {
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

/// Runs tsgo with the `arg0` `name` and `args`, and returns its exit code
/// and stdout. A tsgo that has not ended after `LIMIT` fails the test: a
/// worker whose open of the pipe waits for a reader never ends.
fn run(name: &str, args: &[&str], launch: &str, case: &str) -> (Option<i32>, String) {
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
    (status.code(), stdout)
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
