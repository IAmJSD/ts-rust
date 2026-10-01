//! Go `syscall` RLIMIT_NOFILE handling (syscall/rlimit.go, go1.27.1). A Go
//! process raises its soft open-file limit to one below the hard limit at
//! start, and a process that it starts gets the original limit back
//! (syscall/exec_linux.go forkAndExecInChild1; `spawn` here).

#[cfg(unix)]
use std::sync::OnceLock;

/// The open-file limit at start, when `raise_open_file_limit` raised it.
#[cfg(unix)]
static ORIGINAL: OnceLock<rustix::process::Rlimit> = OnceLock::new();

// Go: rlimit.go:30 init
/// Raises the soft open-file limit to one below the hard limit (Go uses
/// one below, so that it can see a later change by another process's
/// `prlimit`). The bins call it at start, before other threads open files.
/// PORT: Go on macOS lowers the raised limit to `kern.maxfilesperproc`
/// (`adjustFileLimit`). The port does not raise to an unlimited hard
/// limit (on Linux that fails in Go too).
pub fn raise_open_file_limit() {
    #[cfg(unix)]
    {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let limit = getrlimit(Resource::Nofile);
        let (Some(current), Some(max)) = (limit.current, limit.maximum) else {
            return;
        };
        if max == 0 || current >= max - 1 {
            return;
        }
        let raised = Rlimit {
            current: Some(max - 1),
            maximum: Some(max),
        };
        if setrlimit(Resource::Nofile, raised).is_ok() {
            let _ = ORIGINAL.set(limit);
        }
    }
}

// Go: exec_linux.go:644 "Restore original rlimit." (go1.27.1)
/// Starts `cmd` (Go `os/exec` `Start`) with the soft open-file limit that
/// this process had before `raise_open_file_limit`, as Go gives it to each
/// process that it starts. When another process changed this process's
/// limit since then (`prlimit`), the child gets the changed limit, as in Go.
/// PORT: Go sets the limit in the child between fork and exec. That needs
/// `pre_exec`, which is `unsafe`. So the soft limit of this process goes
/// back for the length of the start and up again after it. Starts wait for
/// each other here. In that time, another thread of this process that
/// opens a file with more files open than the original limit gets EMFILE.
/// When the start itself fails at the original limit (EMFILE for its pipes,
/// or EBADF when an fd it passes is above that limit), it runs again with the
/// raised limit, and the child keeps that limit.
/// PORT: Linux only. Other systems start the child with the raised limit:
/// there a kqueue watcher holds a file per watched path, more than the
/// original limit (256 on macOS) can hold.
pub fn spawn(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "linux")]
    if let Some(&original) = ORIGINAL.get() {
        use rustix::io::Errno;
        use rustix::process::{Resource, getrlimit, setrlimit};
        static STARTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _start = STARTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = getrlimit(Resource::Nofile);
        let max = original.maximum;
        if now.maximum == max
            && now.current == max.map(|max| max - 1)
            && setrlimit(Resource::Nofile, original).is_ok()
        {
            let child = cmd.spawn();
            let _ = setrlimit(Resource::Nofile, now);
            return match child {
                Err(err)
                    if matches!(Errno::from_io_error(&err), Some(Errno::MFILE | Errno::BADF)) =>
                {
                    cmd.spawn()
                }
                child => child,
            };
        }
    }
    cmd.spawn()
}
