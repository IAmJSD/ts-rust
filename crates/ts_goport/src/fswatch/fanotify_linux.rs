//! Go: internal/fswatch/fanotify_linux.go (the Linux fanotify backend).
//!
//! PORT: D-W1 (no `libc`, no `unsafe`). The syscalls go through the `unix`
//! shim (`nix` for fanotify, `name-to-handle-at` for file handles, `rustix`
//! for the rest). `fanotify_available()` probes `FanotifyInit` as Go does, so
//! `Default()` picks fanotify when the kernel allows it. Records are read with
//! `unix::FanotifyEventMetadata::from_ne_bytes` and `from_ne_bytes` on the
//! info records instead of Go's `unsafe.Pointer` cast and
//! `binary.NativeEndian`.
//!
//! PORT: locks. As in inotify_linux.rs, the port takes `self.base.mu` where
//! Go takes `b.mu` and keeps the backend fields in `self.locked`, which is
//! only taken after `base.mu` and never held across a call that takes it
//! again.

use crate::fswatch::prelude::*;

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::frontend::vfs::osvfs::go_string_from_os;
use crate::fswatch::unix;
use crate::fswatch::walkdir_unix::walk_dir;
use crate::gostd::errors;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

// ---------------------------------------------------------------------------
// fanotify_linux.go: Linux fanotify backend
//
// Uses Linux's fanotify(7) API (kernel ≥ 5.13 without CAP_SYS_ADMIN) to
// watch directory trees. Unlike inotify, fanotify uses FID-based event
// reporting (FAN_REPORT_FID | FAN_REPORT_DFID_NAME): each event carries the
// parent directory's file handle and the child entry name, so watch
// dispatch is keyed by (fsid, handle_type, handle_bytes) instead of a wd
// integer. This avoids the inotify per-user watch limit (fs.inotify.
// max_user_watches) entirely.
//
//	┌──────────────────────────────────────────────────────────────┐
//	│                     fanotifyBackend                          │
//	│                                                              │
//	│  ┌───────────┐        poll(2)        ┌──────────────────┐    │
//	│  │ pipe[0]   ├──────────────────────►│                  │    │
//	│  │ (wakeup)  │                       │  start()         │    │
//	│  └───────────┘                       │  goroutine       │    │
//	│  ┌───────────┐                       │  (event loop)    │    │
//	│  │ fanotify  ├──────────────────────►│                  │    │
//	│  │ fd        │                       └────────┬─────────┘    │
//	│  └───────────┘                                │              │
//	│                                      handleEvents()          │
//	│                                               │              │
//	│                                  parseFanotifyDfidNames      │
//	│                                  (extract handleKey + name)  │
//	│                                               │              │
//	│                                               ▼              │
//	│                               ┌─────────────────────────┐    │
//	│                               │ subscriptions           │    │
//	│                               │ map[handleKey] → []sub  │    │
//	│                               │  sub.dirWatch.events    │    │
//	│                               └─────────────────────────┘    │
//	│                                                              │
//	│  handleKey = (fsid, handle_type, handle_bytes)               │
//	│  obtained via statfs(2) + name_to_handle_at(2) per dir       │
//	└──────────────────────────────────────────────────────────────┘
//
// Goroutines and threading:
//   - One long-lived goroutine (start), launched by watcherBase.run(). It
//     owns the poll(2) loop and runs for the process lifetime. All event
//     reading and dispatch (handleEvents, handleParsedEvent,
//     handleSubscription, handleRenameEvent) execute on this goroutine,
//     under b.mu.
//   - subscribe/closeWatch run on the caller's goroutine under
//     watcherBase.mu. The event loop acquires b.mu for watch map
//     access, providing safe interleaving.
//
// Callback delivery:
//   dirWatch.notify() posts to the shared process-wide debouncer. After a
//   coalescing window (50 ms min / 500 ms max), the debouncer invokes all
//   registered WatchCallbacks on its own dedicated goroutine; never on
//   the caller's goroutine or the event-loop goroutine.
//
// WatchDirectory flow (caller goroutine):
//  1. Walk the target directory.
//  2. On the first subscribe, probe FAN_RENAME support (Linux 5.17+) by
//     attempting a fanotify_mark with FAN_RENAME. If the kernel returns
//     EINVAL or EOPNOTSUPP, fall back to FAN_MOVED_FROM | FAN_MOVED_TO
//     (two separate events instead of one paired event for renames).
//  3. For every directory found:
//     a. fanotify_mark(FAN_MARK_ADD | FAN_MARK_ONLYDIR) to watch it.
//     b. name_to_handle_at(2) to obtain the directory's file handle.
//     c. statfs(2) to obtain the filesystem ID (fsid).
//     d. Map (fsid, handle_type, handle_bytes) → fanotifySubscription.
//
// Event format:
//   Each event has a FanotifyEventMetadata header followed by variable-length
//   info records. parseFanotifyDfidNames extracts DFID_NAME records
//   (FAN_EVENT_INFO_TYPE_DFID_NAME, OLD_DFID_NAME, NEW_DFID_NAME) containing
//   the parent directory's file handle and child entry name. The file handle
//   is matched against the watch map to find the watched directory.
//
// Event dispatch (on start goroutine):
//   - FAN_CREATE / FAN_MOVED_TO  → events.create (→ EventUpdate); if the new
//     entry is a directory (FAN_ONDIR), recursively walk and mark it.
//   - FAN_MODIFY               → events.update (→ EventUpdate).
//   - FAN_DELETE* / FAN_MOVE*    → events.remove (→ EventDelete); drop
//     subscriptions for the removed path and any descendants.
//   - FAN_RENAME (5.17+)         → single paired event with OLD_DFID_NAME +
//     NEW_DFID_NAME info records; handleRenameEvent deletes the old path and
//     creates the new path in one pass.
//   - FAN_Q_OVERFLOW             → set ErrOverflow on every active dirWatch.
//
//   Merged events: fanotify can merge consecutive events on the same object
//   into one event with multiple mask bits. When both create and delete bits
//   are set, handleSubscription stats the path to determine which happened
//   last (exists → delete-then-create = update; gone → create-then-delete =
//   events cancel out).
//
//   After processing all buffered events, call dirWatch.notify() on each
//   touched dirWatch to trigger the debouncer.
//
// Shutdown:
//   Write a byte to pipe[1] → poll sees POLLIN on pipe[0] → loop exits →
//   deferred closeFDs closes fanotify fd, pipe fds, and signals endedSignal.
// ---------------------------------------------------------------------------

// Go: fanotify_linux.go:115 fanotifyInitFlags
pub const FANOTIFY_INIT_FLAGS: u32 = unix::FAN_CLASS_NOTIF
    | unix::FAN_CLOEXEC
    | unix::FAN_NONBLOCK
    | unix::FAN_REPORT_FID
    | unix::FAN_REPORT_DFID_NAME;

// Go: fanotify_linux.go:118 fanotifyMarkMaskBase
pub const FANOTIFY_MARK_MASK_BASE: u64 = unix::FAN_CREATE
    | unix::FAN_DELETE
    | unix::FAN_MODIFY
    | unix::FAN_DELETE_SELF
    | unix::FAN_MOVE_SELF
    | unix::FAN_ONDIR
    | unix::FAN_EVENT_ON_CHILD;

// Go: fanotify_linux.go:123 fanotifyMarkMaskRename
// Used when FAN_RENAME is available (Linux 5.17+).
pub const FANOTIFY_MARK_MASK_RENAME: u64 = FANOTIFY_MARK_MASK_BASE | unix::FAN_RENAME;

// Go: fanotify_linux.go:126 fanotifyMarkMaskMovedFromTo
// Fallback when FAN_RENAME is not available.
pub const FANOTIFY_MARK_MASK_MOVED_FROM_TO: u64 =
    FANOTIFY_MARK_MASK_BASE | unix::FAN_MOVED_FROM | unix::FAN_MOVED_TO;

// Go: fanotify_linux.go:128 fanotifyMarkAddFlags
pub const FANOTIFY_MARK_ADD_FLAGS: u32 =
    unix::FAN_MARK_ADD | unix::FAN_MARK_ONLYDIR | unix::FAN_MARK_DONT_FOLLOW;

// Go: fanotify_linux.go:130 fanotifyBufferSize
pub const FANOTIFY_BUFFER_SIZE: usize = 8192;

// Go: fanotify_linux.go:135 fanotifyHandleKey
/// fanotifyHandleKey uniquely identifies a filesystem object by its fsid and
/// file handle. Used as a map key for watch dispatch.
///
/// PORT: Go keeps the raw handle bytes in a `string`; the port keeps them in
/// a `Vec<u8>` (the bytes are not text).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FanotifyHandleKey {
    pub fsid: [i32; 2],
    pub handle_type: i32,
    /// raw handle bytes as string for map comparability
    pub handle: Vec<u8>,
}

// Go: fanotify_linux.go:141 makeFanotifyHandleKey
pub fn make_fanotify_handle_key(
    fsid: [i32; 2],
    handle_type: i32,
    handle_bytes: &[u8],
) -> FanotifyHandleKey {
    FanotifyHandleKey {
        fsid,
        handle_type,
        handle: handle_bytes.to_vec(),
    }
}

// Go: fanotify_linux.go:150 fanotifySubscription
/// fanotifySubscription mirrors inotifySubscription for the fanotify backend.
pub struct FanotifySubscription {
    pub path: String,
    pub watch_path: String,
    pub dir_watch: Arc<DirWatch>,
    pub key: FanotifyHandleKey,
}

// Go: fanotify_linux.go:158 fanotifyDfidName
/// fanotifyDfidName holds parsed directory FID + name from an info record.
#[derive(Clone, Debug)]
pub struct FanotifyDfidName {
    pub key: FanotifyHandleKey,
    /// child entry name, or "" for self-events on directories
    pub name: String,
}

// Go: fanotify_linux.go:164 fanotifyBackend
/// fanotifyBackend is the fanotify-based watcher backend for Linux.
pub struct FanotifyBackend {
    pub base: WatcherBase,

    pub pipe_write_fd: AtomicI32,
    /// when true, skip FAN_RENAME probe (for testing fallback path)
    pub no_rename: bool,

    pub ended_signal: SignalChan,

    // Persistent buffers reused across handleEvents calls. Only accessed
    // from the start goroutine, so no synchronization needed.
    // PORT: Rust needs a lock to share them; only the start thread takes it.
    pub read_buf: Mutex<Vec<u8>>,
    pub watchers_touched: Mutex<FxHashMap<usize, Arc<DirWatch>>>,

    /// PORT: the fields Go guards with `b.mu` (see the file comment).
    pub locked: Mutex<FanotifyLocked>,
}

/// PORT: the `fanotifyBackend` fields that Go guards with `b.mu`.
pub struct FanotifyLocked {
    pub pipe_fds: [i32; 2],
    pub fanotify_fd: i32,
    /// fanotifyMarkMaskRename or fanotifyMarkMaskMovedFromTo; 0 until first subscribe
    pub mark_mask: u64,
    pub subscriptions: FxHashMap<FanotifyHandleKey, Vec<Arc<FanotifySubscription>>>,
}

// Go: fanotify_linux.go:182 init
// PORT: Go's `init()` sets the factory on the package var
// `fanotifyWatcher`; the port's package var calls this when it is built.
pub fn init(fanotify_watcher: &mut WatcherStruct) {
    if fanotify_available() {
        let factory: WatcherFactory = || -> Arc<dyn WatcherImpl> { new_fanotify_backend(false) };
        fanotify_watcher.factory = Some(factory);
    }
}

// Go: fanotify_linux.go:190 fanotifyAvailable
/// fanotifyAvailable probes whether fanotify_init succeeds with the flags
/// this backend needs.
///
/// PORT: Go `runtime.GOOS` is `std::env::consts::OS`.
pub fn fanotify_available() -> bool {
    if std::env::consts::OS == "android" {
        return false;
    }
    let fd = match unix::fanotify_init(
        FANOTIFY_INIT_FLAGS,
        (unix::O_RDONLY | unix::O_CLOEXEC) as u32,
    ) {
        Ok(fd) => fd,
        Err(_) => return false,
    };
    let _ = unix::close(fd);
    true
}

// Go: fanotify_linux.go:206 newFanotifyBackend
/// newFanotifyBackend creates a fanotify backend. If noRename is true, the
/// backend skips the FAN_RENAME probe and forces the FAN_MOVED_FROM/FAN_MOVED_TO
/// fallback path; this is only used by the fanotify-no-rename test watcher to
/// exercise the fallback path on kernels that natively support FAN_RENAME.
pub fn new_fanotify_backend(no_rename: bool) -> Arc<FanotifyBackend> {
    Arc::new_cyclic(|self_: &Weak<FanotifyBackend>| {
        let b = FanotifyBackend {
            base: WatcherBase::default(),
            pipe_write_fd: AtomicI32::new(-1),
            no_rename,
            ended_signal: SignalChan::new(),
            read_buf: Mutex::new(vec![0u8; FANOTIFY_BUFFER_SIZE]),
            watchers_touched: Mutex::new(FxHashMap::default()),
            locked: Mutex::new(FanotifyLocked {
                pipe_fds: [-1, -1],
                fanotify_fd: -1,
                mark_mask: 0,
                subscriptions: FxHashMap::default(),
            }),
        };
        let self_impl: Weak<dyn WatcherImpl> = self_.clone();
        b.base.init(self_impl);
        b
    })
}

// PORT: Go `defer func() { b.closeFDs(); close(b.endedSignal) }()` in
// start. The guard also runs when start unwinds, as a Go defer does in a
// panic.
struct FanotifyStartDefer<'a>(&'a FanotifyBackend);

impl Drop for FanotifyStartDefer<'_> {
    fn drop(&mut self) {
        self.0.close_fds();
        self.0.ended_signal.close();
    }
}

impl WatcherImpl for FanotifyBackend {
    // Go: fanotify_linux.go:221 fanotifyBackend.start
    fn start(&self) -> Result<(), GoError> {
        let mut pipe_fds = self.locked.lock().unwrap().pipe_fds;
        if let Err(err) = unix::pipe2(&mut pipe_fds, unix::O_CLOEXEC | unix::O_NONBLOCK) {
            return Err(errors::errorf(
                format!("unable to open pipe: {}", err.error()),
                vec![err],
            ));
        }
        self.locked.lock().unwrap().pipe_fds = pipe_fds;
        self.pipe_write_fd.store(pipe_fds[1], Ordering::SeqCst);
        let _defer = FanotifyStartDefer(self);

        let fd = match unix::fanotify_init(
            FANOTIFY_INIT_FLAGS,
            (unix::O_RDONLY | unix::O_CLOEXEC) as u32,
        ) {
            Ok(fd) => fd,
            Err(err) => {
                return Err(errors::errorf(
                    format!("unable to initialize fanotify: {}", err.error()),
                    vec![err],
                ));
            }
        };
        self.locked.lock().unwrap().fanotify_fd = fd;

        let mut pollfds: Vec<unix::PollFd> = vec![
            unix::PollFd {
                fd: pipe_fds[0],
                events: unix::POLLIN,
                revents: 0,
            },
            unix::PollFd {
                fd,
                events: unix::POLLIN,
                revents: 0,
            },
        ];

        self.base.notify_started();

        loop {
            if let Err(err) = unix::poll(&mut pollfds, 500) {
                if errors::is(&err, &errors::from_value(unix::EINTR)) {
                    continue;
                }
                return Err(errors::errorf(
                    format!("unable to poll: {}", err.error()),
                    vec![err],
                ));
            }
            if pollfds[0].revents != 0 {
                break;
            }
            if pollfds[1].revents != 0 {
                self.handle_events()?;
            }
        }

        Ok(())
    }

    // Go: fanotify_linux.go:282 fanotifyBackend.shutdown
    fn shutdown(&self) {
        let fd = self.pipe_write_fd.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        let _ = unix::write(fd, b"X");
        self.ended_signal.wait();
    }

    // Go: fanotify_linux.go:291 fanotifyBackend.subscribe
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        // Probe FAN_RENAME on the first subscribe using the actual watch
        // directory. FAN_RENAME (Linux 5.17+) yields a single paired event
        // for renames; when unavailable we fall back to FAN_MOVED_FROM/
        // FAN_MOVED_TO which produces two separate events but is otherwise
        // equivalent. The kernel rejects unknown mask bits with EINVAL.
        {
            let mut l = self.locked.lock().unwrap();
            if l.mark_mask == 0 {
                if self.no_rename {
                    l.mark_mask = FANOTIFY_MARK_MASK_MOVED_FROM_TO;
                } else {
                    l.mark_mask = FANOTIFY_MARK_MASK_RENAME;
                    let fanotify_fd = l.fanotify_fd;
                    let err = unix::fanotify_mark(
                        fanotify_fd,
                        FANOTIFY_MARK_ADD_FLAGS,
                        FANOTIFY_MARK_MASK_RENAME,
                        unix::AT_FDCWD,
                        &w.physical_dir,
                    );
                    match err {
                        Ok(()) => {
                            // B5: pair the probe Add with a matching Remove. If
                            // Remove fails (rare; only EINTR or kernel resource
                            // pressure realistically) we leave the probe mark
                            // attached for the life of the process, but since
                            // markDir below will Add the real mask with the same
                            // flags the kernel just merges them. The probe is the
                            // only failure path we explicitly retry.
                            loop {
                                let rm_err = unix::fanotify_mark(
                                    fanotify_fd,
                                    unix::FAN_MARK_REMOVE | unix::FAN_MARK_ONLYDIR,
                                    FANOTIFY_MARK_MASK_RENAME,
                                    unix::AT_FDCWD,
                                    &w.physical_dir,
                                );
                                match rm_err {
                                    Ok(()) => break,
                                    Err(rm_err) => {
                                        if !errors::is(&rm_err, &errors::from_value(unix::EINTR)) {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(err)
                            if errors::is(&err, &errors::from_value(unix::EINVAL))
                                || errors::is(&err, &errors::from_value(unix::EOPNOTSUPP)) =>
                        {
                            l.mark_mask = FANOTIFY_MARK_MASK_MOVED_FROM_TO;
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        if !w.recursive {
            if let Err(err) = self.mark_dir(w, &w.dir, &w.physical_dir) {
                return Err(DirWatchError {
                    err: errors::errorf(
                        format!("fanotify_mark on '{}' failed: {}", w.dir, err.error()),
                        vec![err],
                    ),
                    dir_watch: w.clone(),
                }
                .to_go_error());
            }
            return Ok(());
        }
        if let Err(err) = walk_dir(
            &w.physical_dir,
            true,
            Some(
                &mut |watch_path: &str, is_dir: bool| -> Result<(), GoError> {
                    if !is_dir {
                        return Ok(());
                    }
                    let path = &w.display_path(watch_path);
                    if let Err(err) = self.mark_dir(w, path, watch_path) {
                        return Err(DirWatchError {
                            err: errors::errorf(
                                format!("fanotify_mark on '{}' failed: {}", path, err.error()),
                                vec![err],
                            ),
                            dir_watch: w.clone(),
                        }
                        .to_go_error());
                    }
                    Ok(())
                },
            ),
        ) {
            let _ = self.close_watch(w);
            return Err(err);
        }
        Ok(())
    }

    // Go: fanotify_linux.go:736 fanotifyBackend.closeWatch
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        let mut l = self.locked.lock().unwrap();
        let fanotify_fd = l.fanotify_fd;
        let mark_mask = l.mark_mask;
        l.subscriptions.retain(|_key, list| {
            let mut removed_any = false;
            let mut removed_path = String::new();
            list.retain(|s| {
                if Arc::ptr_eq(&s.dir_watch, w) {
                    removed_any = true;
                    removed_path = s.watch_path.clone();
                    return false;
                }
                true
            });
            if !removed_any {
                return true;
            }
            if list.is_empty() {
                // Try to unmark. Skip the call entirely when markMask is
                // still 0 (closeWatch racing with a shutdown that happened
                // before subscribe ever set markMask); fanotify_mark with
                // mask=0 is undocumented. Ignore ENOENT (directory may have
                // been deleted) and EBADF (fanotify fd may already be
                // closed during shutdown).
                if mark_mask != 0 {
                    let _ = unix::fanotify_mark(
                        fanotify_fd,
                        unix::FAN_MARK_REMOVE,
                        mark_mask,
                        unix::AT_FDCWD,
                        &removed_path,
                    );
                }
                return false;
            }
            true
        });
        Ok(())
    }

    fn base(&self) -> &WatcherBase {
        &self.base
    }
}

impl FanotifyBackend {
    // Go: fanotify_linux.go:265 fanotifyBackend.closeFDs
    pub fn close_fds(&self) {
        let _b = self.base.mu.lock().unwrap();
        let mut l = self.locked.lock().unwrap();
        if l.pipe_fds[0] >= 0 {
            let _ = unix::close(l.pipe_fds[0]);
            l.pipe_fds[0] = -1;
        }
        let fd = self.pipe_write_fd.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            let _ = unix::close(fd);
        }
        l.pipe_fds[1] = -1;
        if l.fanotify_fd >= 0 {
            let _ = unix::close(l.fanotify_fd);
            l.fanotify_fd = -1;
        }
    }

    // Go: fanotify_linux.go:351 fanotifyBackend.markDir
    pub fn mark_dir(&self, w: &Arc<DirWatch>, path: &str, mark_path: &str) -> Result<(), GoError> {
        let (fanotify_fd, mark_mask) = {
            let l = self.locked.lock().unwrap();
            (l.fanotify_fd, l.mark_mask)
        };
        if let Err(err) = unix::fanotify_mark(
            fanotify_fd,
            FANOTIFY_MARK_ADD_FLAGS,
            mark_mask,
            unix::AT_FDCWD,
            mark_path,
        ) {
            return Err(maybe_wrap_unsupported_filesystem(err));
        }
        let handle = match unix::name_to_handle_at(unix::AT_FDCWD, mark_path, 0) {
            Ok((handle, _)) => handle,
            Err(err) => {
                // Unmark since we can't track this directory without a handle.
                let _ = unix::fanotify_mark(
                    fanotify_fd,
                    unix::FAN_MARK_REMOVE | unix::FAN_MARK_ONLYDIR,
                    mark_mask,
                    unix::AT_FDCWD,
                    mark_path,
                );
                return Err(maybe_wrap_unsupported_filesystem(errors::errorf(
                    format!("name_to_handle_at: {}", err.error()),
                    vec![err],
                )));
            }
        };
        let mut st = unix::Statfs_t::default();
        if let Err(err) = unix::statfs(mark_path, &mut st) {
            let _ = unix::fanotify_mark(
                fanotify_fd,
                unix::FAN_MARK_REMOVE | unix::FAN_MARK_ONLYDIR,
                mark_mask,
                unix::AT_FDCWD,
                mark_path,
            );
            return Err(errors::errorf(
                format!("statfs: {}", err.error()),
                vec![err],
            ));
        }
        let key = make_fanotify_handle_key(st.fsid.val, handle.type_(), handle.bytes());
        let sub = Arc::new(FanotifySubscription {
            path: path.to_string(),
            watch_path: mark_path.to_string(),
            dir_watch: w.clone(),
            key: key.clone(),
        });
        self.locked
            .lock()
            .unwrap()
            .subscriptions
            .entry(key)
            .or_default()
            .push(sub);
        Ok(())
    }

    // Go: fanotify_linux.go:380 fanotifyBackend.handleEvents
    /// handleEvents reads and dispatches fanotify events from the fd.
    pub fn handle_events(&self) -> Result<(), GoError> {
        let mut buf = self.read_buf.lock().unwrap();
        let mut watchers_touched = self.watchers_touched.lock().unwrap();

        loop {
            let fanotify_fd = self.locked.lock().unwrap().fanotify_fd;
            let n = match unix::read(fanotify_fd, &mut buf) {
                Ok(n) => n,
                Err(err) => {
                    if errors::is(&err, &errors::from_value(unix::EAGAIN))
                        || errors::is(&err, &errors::from_value(unix::EWOULDBLOCK))
                    {
                        break;
                    }
                    return Err(errors::errorf(
                        format!("Error reading from fanotify: {}", err.error()),
                        vec![err],
                    ));
                }
            };
            if n == 0 {
                break;
            }

            let meta_size = std::mem::size_of::<unix::FanotifyEventMetadata>();
            let mut data: &[u8] = &buf[..n as usize];
            while data.len() >= meta_size {
                let meta = unix::FanotifyEventMetadata::from_ne_bytes(data);
                if meta.vers != unix::FANOTIFY_METADATA_VERSION {
                    return Err(errors::errorf(
                        format!("unsupported fanotify metadata version: {}", meta.vers),
                        vec![],
                    ));
                }
                let event_len = meta.event_len as usize;
                if event_len < meta.metadata_len as usize || event_len > data.len() {
                    break;
                }

                // FID mode: fd should be FAN_NOFD, but close if somehow set.
                if meta.fd >= 0 {
                    let _ = unix::close(meta.fd);
                }

                if meta.mask & unix::FAN_Q_OVERFLOW != 0 {
                    self.handle_overflow(&mut watchers_touched);
                    data = &data[event_len..];
                    continue;
                }

                let info_data = &data[meta.metadata_len as usize..event_len];
                let (primary, rename_to) = parse_fanotify_dfid_names(info_data);
                if meta.mask & unix::FAN_RENAME != 0 {
                    if primary.is_some() || rename_to.is_some() {
                        self.handle_rename_event(
                            meta.mask,
                            primary.as_ref(),
                            rename_to.as_ref(),
                            &mut watchers_touched,
                        );
                    }
                } else if let Some(primary) = &primary {
                    self.handle_parsed_event(meta.mask, primary, &mut watchers_touched);
                }
                data = &data[event_len..];
            }
        }

        for w in watchers_touched.values() {
            w.notify();
        }
        watchers_touched.clear();
        Ok(())
    }

    // Go: fanotify_linux.go:439 fanotifyBackend.handleOverflow
    pub fn handle_overflow(&self, touched: &mut FxHashMap<usize, Arc<DirWatch>>) {
        let _b = self.base.mu.lock().unwrap();
        let l = self.locked.lock().unwrap();
        let mut seen: FxHashSet<usize> = FxHashSet::default();
        for subs in l.subscriptions.values() {
            for s in subs {
                let key = Arc::as_ptr(&s.dir_watch) as usize;
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                s.dir_watch.events.set_error(ERR_OVERFLOW.clone());
                touched.insert(key, s.dir_watch.clone());
            }
        }
    }

    // Go: fanotify_linux.go:455 fanotifyBackend.handleRenameEvent
    pub fn handle_rename_event(
        &self,
        mask: u64,
        dfid_old: Option<&FanotifyDfidName>,
        dfid_new: Option<&FanotifyDfidName>,
        touched: &mut FxHashMap<usize, Arc<DirWatch>>,
    ) {
        let _b = self.base.mu.lock().unwrap();

        let is_dir = mask & unix::FAN_ONDIR != 0;

        // Remove from old location.
        if let Some(dfid_old) = dfid_old {
            if !dfid_old.name.is_empty() && dfid_old.name != "." {
                // PORT: Go ranges over the slice value while
                // dropSubsForPathAndDescendantsLocked may filter the same
                // backing array in place. The port ranges over a copy.
                let subs = self.subscriptions_for(&dfid_old.key);
                for s in &subs {
                    let old_path = format!("{}/{}", s.path, dfid_old.name);
                    // If the renamed item is a dir, drop its subscriptions and
                    // all descendant subscriptions. The kernel marks themselves
                    // leak when the destination is outside our watched tree:
                    // fanotify has no path-independent unmark and we don't
                    // keep fds open for marked directories.
                    if is_dir {
                        self.drop_subs_for_path_and_descendants_locked(&old_path);
                    }
                    s.dir_watch.events.remove(&old_path);
                    touched.insert(Arc::as_ptr(&s.dir_watch) as usize, s.dir_watch.clone());
                }
            }
        }

        // Create at new location.
        if let Some(dfid_new) = dfid_new {
            if !dfid_new.name.is_empty() && dfid_new.name != "." {
                let subs = self.subscriptions_for(&dfid_new.key);
                for s in &subs {
                    let new_path = format!("{}/{}", s.path, dfid_new.name);
                    s.dir_watch.events.create(&new_path);
                    if is_dir && s.dir_watch.recursive {
                        let _ = walk_dir(
                            &s.dir_watch.physical_path(&new_path),
                            true,
                            Some(&mut |p: &str, p_is_dir: bool| -> Result<(), GoError> {
                                if !p_is_dir {
                                    return Ok(());
                                }
                                let _ =
                                    self.mark_dir(&s.dir_watch, &s.dir_watch.display_path(p), p);
                                Ok(())
                            }),
                        );
                    }
                    touched.insert(Arc::as_ptr(&s.dir_watch) as usize, s.dir_watch.clone());
                }
            }
        }
    }

    // PORT: Go `b.subscriptions[key]` (nil when missing), copied out of the
    // lock so the caller can change the map while it ranges.
    fn subscriptions_for(&self, key: &FanotifyHandleKey) -> Vec<Arc<FanotifySubscription>> {
        self.locked
            .lock()
            .unwrap()
            .subscriptions
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    // Go: fanotify_linux.go:497 fanotifyBackend.handleParsedEvent
    pub fn handle_parsed_event(
        &self,
        mask: u64,
        dfid: &FanotifyDfidName,
        touched: &mut FxHashMap<usize, Arc<DirWatch>>,
    ) {
        let _b = self.base.mu.lock().unwrap();

        // b.subscriptions[key] holds at most one entry per *fanotifySubscription
        // pointer (markDir always appends a fresh struct), so no dedup is
        // necessary.
        //
        // PORT: the port ranges over a copy of the list (see
        // handleRenameEvent).
        let subs = self.subscriptions_for(&dfid.key);
        for s in &subs {
            if self.handle_subscription(mask, dfid, s) {
                touched.insert(Arc::as_ptr(&s.dir_watch) as usize, s.dir_watch.clone());
            }
        }
    }

    // Go: fanotify_linux.go:511 fanotifyBackend.handleSubscription
    pub fn handle_subscription(
        &self,
        mask: u64,
        dfid: &FanotifyDfidName,
        sub: &FanotifySubscription,
    ) -> bool {
        let w = &sub.dir_watch;

        // Compute full path. Self-events (name empty or ".") use the
        // watch path directly.
        let is_self_event = dfid.name.is_empty() || dfid.name == ".";
        let mut path = sub.path.clone();
        if !is_self_event {
            path = format!("{}/{}", sub.path, dfid.name);
        }

        let is_dir = mask & unix::FAN_ONDIR != 0;
        let mut touched = false;

        let has_delete = mask & (unix::FAN_DELETE | unix::FAN_MOVED_FROM) != 0;
        let has_create = mask & (unix::FAN_CREATE | unix::FAN_MOVED_TO) != 0;

        // Fanotify can merge consecutive events on the same object into a
        // single event with multiple mask bits. When both create and delete
        // bits are set, we can't tell the temporal order from the mask alone.
        // Stat the path: if it exists, the last op was create (delete→create
        // = "update"); if gone, the last op was delete (create→delete =
        // cancel out).
        if has_create && has_delete && !is_self_event {
            let mut st = unix::Stat_t::default();
            if unix::lstat(&path, &mut st).is_err() {
                // File was created then deleted: record both so they cancel.
                w.events.create(&path);
                w.events.remove(&path);
                return true;
            }
            // File exists: was deleted then recreated. Fall through to the
            // normal delete-first processing which produces "update".
        }

        // Process delete/move-from FIRST so that a merged DELETE+CREATE
        // coalesces to "update" via the eventList's rapid-recreate logic.
        if mask
            & (unix::FAN_DELETE
                | unix::FAN_DELETE_SELF
                | unix::FAN_MOVED_FROM
                | unix::FAN_MOVE_SELF)
            != 0
        {
            let is_self_mask = mask & (unix::FAN_DELETE_SELF | unix::FAN_MOVE_SELF) != 0;
            // Ignore delete/move self events unless this is the watch root.
            if !(is_self_mask && path != w.dir) {
                // If the deleted/moved item is a dir, drop subscriptions
                // for both the path itself and every descendant; otherwise
                // later events for the (now-moved) inodes would be reported
                // against stale paths. For FAN_MOVED_FROM that takes the
                // inode out of our watched tree the kernel mark on the
                // inode itself unfortunately leaks: fanotify has no
                // path-independent way to unmark and the destination is
                // outside everything we can resolve.
                // Self events may not have FAN_ONDIR set (like inotify).
                if is_self_mask || is_dir {
                    self.drop_subs_for_path_and_descendants_locked(&path);
                } else {
                    self.drop_subs_for_path_locked(&path);
                }
                w.events.remove(&path);
                touched = true;
                // Root-of-watch deletion: the kernel has dropped the mark.
                // Surface ErrWatchTerminated alongside the delete so callers
                // know to clean up; no more events will arrive for w.
                if is_self_mask && path == w.dir {
                    w.events.set_error(errors::errorf(
                        format!(
                            "{}: watched directory removed",
                            ERR_WATCH_TERMINATED.error()
                        ),
                        vec![ERR_WATCH_TERMINATED.clone()],
                    ));
                }
            }
        }

        if has_create {
            w.events.create(&path);
            if is_dir && w.recursive {
                let _ = walk_dir(
                    &w.physical_path(&path),
                    true,
                    Some(&mut |p: &str, p_is_dir: bool| -> Result<(), GoError> {
                        if !p_is_dir {
                            return Ok(());
                        }
                        let _ = self.mark_dir(w, &w.display_path(p), p);
                        Ok(())
                    }),
                );
            }
            touched = true;
        }

        if mask & unix::FAN_MODIFY != 0 {
            w.events.update(&path);
            touched = true;
        }

        touched
    }

    // Go: fanotify_linux.go:696 fanotifyBackend.dropSubsForPathLocked
    /// dropSubsForPathLocked removes every subscription whose s.path equals
    /// path, regardless of which fanotify handle key it lives under. Must be
    /// called with b.mu held.
    pub fn drop_subs_for_path_locked(&self, path: &str) {
        let mut l = self.locked.lock().unwrap();
        l.subscriptions.retain(|_key, list| {
            list.retain(|s| s.path != path);
            !list.is_empty()
        });
    }

    // Go: fanotify_linux.go:719 fanotifyBackend.dropSubsForPathAndDescendantsLocked
    /// dropSubsForPathAndDescendantsLocked removes every subscription whose
    /// s.path equals path or lives strictly under path. The kernel mark on
    /// the moved-out inode itself remains active (fanotify provides no
    /// path-independent unmark) but dropping the bookkeeping prevents later
    /// events from being reported against the no-longer-valid path.
    /// Must be called with b.mu held.
    pub fn drop_subs_for_path_and_descendants_locked(&self, path: &str) {
        let mut l = self.locked.lock().unwrap();
        l.subscriptions.retain(|_key, list| {
            list.retain(|s| {
                let sp = s.path.as_bytes();
                let p = path.as_bytes();
                !(sp == p || (sp.len() > p.len() && sp[p.len()] == b'/' && &sp[..p.len()] == p))
            });
            !list.is_empty()
        });
    }
}

// Go: fanotify_linux.go:372 maybeWrapUnsupportedFilesystem
// PORT: a free function after the `fanotifyBackend` methods (Go puts it
// after `markDir`).
pub fn maybe_wrap_unsupported_filesystem(err: GoError) -> GoError {
    if errors::is(&err, &errors::from_value(unix::EOPNOTSUPP))
        || errors::is(&err, &errors::from_value(unix::ENOTSUP))
        || errors::is(&err, &errors::from_value(unix::ENODEV))
    {
        return errors::errorf(
            format!("{}: {}", err.error(), ERR_FILESYSTEM_UNSUPPORTED.error()),
            vec![err, ERR_FILESYSTEM_UNSUPPORTED.clone()],
        );
    }
    err
}

// Go: fanotify_linux.go:602 parseFanotifyDfidNames
/// parseFanotifyDfidNames extracts DFID_NAME info records from the event's
/// info record area. Returns a primary record (DFID_NAME or OLD_DFID_NAME)
/// and an optional second record (NEW_DFID_NAME, for FAN_RENAME events).
pub fn parse_fanotify_dfid_names(
    data: &[u8],
) -> (Option<FanotifyDfidName>, Option<FanotifyDfidName>) {
    const INFO_HDR_SIZE: usize = 4; // fanotify_event_info_header
    const FSID_SIZE: usize = 8; // __kernel_fsid_t
    const FH_HDR_SIZE: usize = 8; // file_handle header (handle_bytes + handle_type)
    const MIN_BODY_SIZE: usize = FSID_SIZE + FH_HDR_SIZE;
    let mut primary: Option<FanotifyDfidName> = None;
    let mut rename: Option<FanotifyDfidName> = None;
    let mut offset = 0usize;
    while offset + INFO_HDR_SIZE <= data.len() {
        let info_type = data[offset];
        let info_len = u16::from_ne_bytes([data[offset + 2], data[offset + 3]]) as usize;
        if info_len < INFO_HDR_SIZE || offset + info_len > data.len() {
            break;
        }

        match info_type {
            unix::FAN_EVENT_INFO_TYPE_DFID_NAME | unix::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => {
                if let Some(parsed) =
                    parse_fanotify_fid_record(&data[offset..offset + info_len], true)
                {
                    primary = Some(parsed);
                }
            }

            unix::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => {
                if let Some(parsed) =
                    parse_fanotify_fid_record(&data[offset..offset + info_len], true)
                {
                    rename = Some(parsed);
                }
            }

            unix::FAN_EVENT_INFO_TYPE_DFID => {
                // DFID without name: the handle identifies the directory itself.
                // Use as fallback if we haven't found a DFID_NAME record.
                if primary.is_none() {
                    if let Some(parsed) =
                        parse_fanotify_fid_record(&data[offset..offset + info_len], false)
                    {
                        primary = Some(parsed);
                    }
                }
            }

            _ => {}
        }

        if primary.is_some() && rename.is_some() {
            return (primary, rename);
        }

        offset += info_len;
    }
    (primary, rename)
}

// Go: fanotify_linux.go:649 parseFanotifyFidRecord
/// parseFanotifyFidRecord parses a single fanotify_event_info_fid record.
pub fn parse_fanotify_fid_record(data: &[u8], has_name: bool) -> Option<FanotifyDfidName> {
    const INFO_HDR_SIZE: usize = 4;
    const FSID_SIZE: usize = 8;
    const FH_HDR_SIZE: usize = 8;
    const MIN_SIZE: usize = INFO_HDR_SIZE + FSID_SIZE + FH_HDR_SIZE;
    if data.len() < MIN_SIZE {
        return None;
    }
    let body = &data[INFO_HDR_SIZE..];

    let mut fsid = [0i32; 2];
    fsid[0] = u32::from_ne_bytes([body[0], body[1], body[2], body[3]]) as i32;
    fsid[1] = u32::from_ne_bytes([body[4], body[5], body[6], body[7]]) as i32;

    let handle_bytes = u32::from_ne_bytes([body[8], body[9], body[10], body[11]]) as usize;
    let handle_type = u32::from_ne_bytes([body[12], body[13], body[14], body[15]]) as i32;

    let handle_start = FSID_SIZE + FH_HDR_SIZE;
    if handle_start + handle_bytes > body.len() {
        return None;
    }
    let handle_data = &body[handle_start..handle_start + handle_bytes];
    let key = make_fanotify_handle_key(fsid, handle_type, handle_data);

    let mut name = String::new();
    if has_name {
        let name_start = handle_start + handle_bytes;
        if name_start < body.len() {
            let mut name_data = &body[name_start..];
            if let Some(i) = name_data.iter().position(|&c| c == 0) {
                name_data = &name_data[..i];
            }
            // PORT: Go names are bytes; the port form keeps them (see
            // walkdir_unix.rs `read_dir_entries`).
            name = go_string_from_os(OsStr::from_bytes(name_data));
        }
    }

    Some(FanotifyDfidName { key, name })
}
