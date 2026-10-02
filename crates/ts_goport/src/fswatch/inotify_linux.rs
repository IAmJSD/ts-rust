//! Go: internal/fswatch/inotify_linux.go (the Linux inotify backend).
//!
//! PORT: D-W1 (no `libc`, no `unsafe`). The syscalls go through the `unix`
//! shim, which calls rustix. Records are read with
//! `unix::InotifyEvent::from_ne_bytes` instead of Go's `unsafe.Pointer` cast.
//!
//! PORT: locks. Go guards the backend fields with the embedded
//! `watcherBase.mu` (`b.mu`). The port takes `self.base.mu` where Go takes
//! `b.mu` and keeps the fields in `self.locked`, a second mutex that is only
//! taken after `base.mu` (or by the start thread before it shares them) and
//! never held across a call that takes it again.

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
// inotify_linux.go: Linux inotify backend
//
// Uses the kernel's inotify(7) subsystem to watch directory trees. A single
// inotify instance serves all subscriptions for the process lifetime.
//
//	┌───────────────────────────────────────────────────────────┐
//	│                    inotifyBackend                         │
//	│                                                           │
//	│  ┌───────────┐        poll(2)        ┌─────────────────┐  │
//	│  │ pipe[0]   ├──────────────────────►│                 │  │
//	│  │ (wakeup)  │                       │  start()        │  │
//	│  └───────────┘                       │  goroutine      │  │
//	│  ┌───────────┐                       │  (event loop)   │  │
//	│  │ inotify   ├──────────────────────►│                 │  │
//	│  │ fd        │                       └────────┬────────┘  │
//	│  └───────────┘                                │           │
//	│                                      handleEvents()       │
//	│                                               │           │
//	│                                               ▼           │
//	│                               ┌─────────────────────────┐ │
//	│                               │ subscriptions           │ │
//	│                               │ map[wd] → []sub         │ │
//	│                               │  sub.dirWatch.events    │ │
//	│                               └─────────────────────────┘ │
//	└───────────────────────────────────────────────────────────┘
//
// Goroutines and threading:
//   - One long-lived goroutine (start), launched by watcherBase.run(). It
//     owns the poll(2) loop and runs for the process lifetime. All event
//     reading and dispatch (handleEvents, handleEvent, handleSubscription)
//     execute on this goroutine, under b.mu.
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
// WatchDirectory flow:
//  1. Walk the target directory (caller goroutine).
//  2. For every directory found, call inotify_add_watch to obtain a
//     watch descriptor (wd). Map wd → inotifySubscription.
//
// Event dispatch (handleEvents → handleSubscription, on start goroutine):
//   - IN_CREATE / IN_MOVED_TO  → events.create (→ EventUpdate); if the new
//     entry is a directory (IN_ISDIR), recursively walk and watch it.
//   - IN_MODIFY                → events.update.
//   - IN_DELETE* / IN_MOVE*    → events.remove; drop inotify subscriptions
//     for the removed path and any descendants.
//   - IN_Q_OVERFLOW            → set ErrOverflow on every active dirWatch.
//   After processing all buffered events, call dirWatch.notify() on each
//   touched dirWatch to trigger the debouncer.
//
// Shutdown:
//   Write a byte to pipe[1] → poll sees POLLIN on pipe[0] → loop exits →
//   deferred closeFDs closes inotify fd, pipe fds, and signals endedSignal.
// ---------------------------------------------------------------------------

// Go: inotify_linux.go:77 inotifyMask
pub const INOTIFY_MASK: u32 = unix::IN_CREATE
    | unix::IN_DELETE
    | unix::IN_DELETE_SELF
    | unix::IN_MODIFY
    | unix::IN_MOVE_SELF
    | unix::IN_MOVED_FROM
    | unix::IN_MOVED_TO
    | unix::IN_DONT_FOLLOW
    | unix::IN_ONLYDIR
    | unix::IN_EXCL_UNLINK;
// Go: inotify_linux.go:87 inotifyBufferSize
pub const INOTIFY_BUFFER_SIZE: usize = 8192;

// Go: inotify_linux.go:91 inotifySubscription
/// inotifySubscription.
pub struct InotifySubscription {
    pub path: String,
    pub watch_path: String,
    pub dir_watch: Arc<DirWatch>,
    pub wd: i32,
}

// Go: inotify_linux.go:99 inotifyBackend
/// inotifyBackend.
pub struct InotifyBackend {
    pub base: WatcherBase,

    // pipeWriteFD shadows pipeFDs[1] as an atomic so shutdown (any goroutine)
    // can safely race against the start goroutine's deferred closeFDs.
    // Sentinel -1 once closed.
    pub pipe_write_fd: AtomicI32,
    pub ended_signal: SignalChan,

    // Persistent buffers reused across handleEvents calls. Only accessed
    // from the start goroutine, so no synchronization needed.
    // PORT: Rust needs a lock to share them; only the start thread takes it.
    pub read_buf: Mutex<Vec<u8>>,
    pub watchers_touched: Mutex<FxHashMap<usize, Arc<DirWatch>>>,

    /// PORT: the fields Go guards with `b.mu` (see the file comment).
    pub locked: Mutex<InotifyLocked>,
}

/// PORT: the `inotifyBackend` fields that Go guards with `b.mu`.
pub struct InotifyLocked {
    pub pipe_fds: [i32; 2],
    pub inotify: i32,
    /// multimap<wd, sub>
    pub subscriptions: FxHashMap<i32, Vec<Arc<InotifySubscription>>>,
}

// Go: inotify_linux.go:117 init
// PORT: Go's `init()` sets the factory on the package var
// `inotifyWatcher`; the port's package var calls this when it is built.
pub fn init(inotify_watcher: &mut WatcherStruct) {
    let factory: WatcherFactory = || -> Arc<dyn WatcherImpl> { new_inotify_backend() };
    inotify_watcher.factory = Some(factory);
}

// Go: inotify_linux.go:121 newInotifyBackend
pub fn new_inotify_backend() -> Arc<InotifyBackend> {
    Arc::new_cyclic(|self_: &Weak<InotifyBackend>| {
        let b = InotifyBackend {
            base: WatcherBase::default(),
            pipe_write_fd: AtomicI32::new(-1),
            ended_signal: SignalChan::new(),
            read_buf: Mutex::new(vec![0u8; INOTIFY_BUFFER_SIZE]),
            watchers_touched: Mutex::new(FxHashMap::default()),
            locked: Mutex::new(InotifyLocked {
                pipe_fds: [-1, -1],
                inotify: -1,
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
struct InotifyStartDefer<'a>(&'a InotifyBackend);

impl Drop for InotifyStartDefer<'_> {
    fn drop(&mut self) {
        self.0.close_fds();
        self.0.ended_signal.close();
    }
}

impl WatcherImpl for InotifyBackend {
    // Go: inotify_linux.go:136 inotifyBackend.start
    /// start mirrors `inotifyBackend::start`.
    fn start(&self) -> Result<(), GoError> {
        // Create a pipe so we can wake the poll(2) loop on shutdown.
        let mut pipe_fds = self.locked.lock().unwrap().pipe_fds;
        if let Err(err) = unix::pipe2(&mut pipe_fds, unix::O_CLOEXEC | unix::O_NONBLOCK) {
            return Err(errors::errorf(
                format!("unable to open pipe: {}", err.error()),
                vec![err],
            ));
        }
        self.locked.lock().unwrap().pipe_fds = pipe_fds;
        self.pipe_write_fd.store(pipe_fds[1], Ordering::SeqCst);
        let _defer = InotifyStartDefer(self);
        let fd = match unix::inotify_init1(unix::IN_NONBLOCK | unix::IN_CLOEXEC) {
            Ok(fd) => fd,
            Err(err) => {
                return Err(errors::errorf(
                    format!("unable to initialize inotify: {}", err.error()),
                    vec![err],
                ));
            }
        };
        self.locked.lock().unwrap().inotify = fd;

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

    // Go: inotify_linux.go:204 inotifyBackend.shutdown
    /// shutdown is the equivalent of the destructor's pipe-write+wait.
    /// Called by removeSharedBackend when the last watch drops. Reads
    /// the pipe write fd via atomic so it's safe to race against the start
    /// goroutine's deferred closeFDs.
    fn shutdown(&self) {
        let fd = self.pipe_write_fd.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        let _ = unix::write(fd, b"X");
        self.ended_signal.wait();
    }

    // Go: inotify_linux.go:215 inotifyBackend.subscribe
    /// subscribe mirrors `inotifyBackend::subscribe`. Called via the watcherBase
    /// virtual dispatch under b.mu (so it's serialized against handleEvent).
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        if !w.recursive {
            if let Err(err) = self.watch_dir(w, &w.dir, &w.physical_dir) {
                return Err(DirWatchError {
                    err: errors::errorf(
                        format!("inotify_add_watch on '{}' failed: {}", w.dir, err.error()),
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
                    if let Err(err) = self.watch_dir(w, path, watch_path) {
                        return Err(DirWatchError {
                            err: errors::errorf(
                                format!("inotify_add_watch on '{}' failed: {}", path, err.error()),
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

    // Go: inotify_linux.go:399 inotifyBackend.closeWatch
    /// closeWatch mirrors `inotifyBackend::closeWatch`. Iterates every wd that
    /// referenced w and removes the matching subscriptions. If a kernel
    /// InotifyRmWatch fails we keep processing remaining wds and return the
    /// first error encountered; bailing early would leave the internal state
    /// half-cleaned and the caller's dirWatch hanging off other wds.
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        let mut first_err: Option<GoError> = None;
        let mut l = self.locked.lock().unwrap();
        let inotify = l.inotify;
        l.subscriptions.retain(|wd, list| {
            let before = list.len();
            list.retain(|s| !Arc::ptr_eq(&s.dir_watch, w));
            let removed_any = list.len() != before;
            if !removed_any {
                return true;
            }
            if list.is_empty() {
                if let Err(err) = unix::inotify_rm_watch(inotify, *wd as u32) {
                    if first_err.is_none() {
                        first_err = Some(
                            DirWatchError {
                                err: errors::errorf(
                                    format!("unable to remove dirWatch: {}", err.error()),
                                    vec![err],
                                ),
                                dir_watch: w.clone(),
                            }
                            .to_go_error(),
                        );
                    }
                }
                return false;
            }
            true
        });
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn base(&self) -> &WatcherBase {
        &self.base
    }
}

impl InotifyBackend {
    // Go: inotify_linux.go:183 inotifyBackend.closeFDs
    /// closeFDs runs in the start goroutine after the poll loop exits. Takes
    /// b.mu so the writes to b.inotify / b.pipeFDs synchronize-against the
    /// reads in closeWatch / subscribe (both of which run under b.mu).
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
        if l.inotify >= 0 {
            let _ = unix::close(l.inotify);
            l.inotify = -1;
        }
    }

    // Go: inotify_linux.go:246 inotifyBackend.watchDir
    /// watchDir registers an inotify watch on path and records the resulting
    /// subscription. Returns the kernel watch descriptor on success.
    pub fn watch_dir(
        &self,
        w: &Arc<DirWatch>,
        path: &str,
        watch_path: &str,
    ) -> Result<i32, GoError> {
        let inotify = self.locked.lock().unwrap().inotify;
        let wd = unix::inotify_add_watch(inotify, watch_path, INOTIFY_MASK)?;
        let sub = Arc::new(InotifySubscription {
            path: path.to_string(),
            watch_path: watch_path.to_string(),
            dir_watch: w.clone(),
            wd,
        });
        self.locked
            .lock()
            .unwrap()
            .subscriptions
            .entry(wd)
            .or_default()
            .push(sub);
        Ok(wd)
    }

    // Go: inotify_linux.go:257 inotifyBackend.handleEvents
    /// handleEvents mirrors `inotifyBackend::handleEvents`.
    pub fn handle_events(&self) -> Result<(), GoError> {
        let mut buf = self.read_buf.lock().unwrap();
        let mut watchers_touched = self.watchers_touched.lock().unwrap();

        loop {
            let inotify = self.locked.lock().unwrap().inotify;
            let n = match unix::read(inotify, &mut buf) {
                Ok(n) => n,
                Err(err) => {
                    if errors::is(&err, &errors::from_value(unix::EAGAIN))
                        || errors::is(&err, &errors::from_value(unix::EWOULDBLOCK))
                    {
                        break;
                    }
                    return Err(errors::errorf(
                        format!("Error reading from inotify: {}", err.error()),
                        vec![err],
                    ));
                }
            };
            if n == 0 {
                break;
            }
            let n = n as usize;
            // Walk the buffer.
            let mut offset = 0;
            while offset < n {
                let ev = unix::InotifyEvent::from_ne_bytes(&buf[offset..]);
                let record_size = unix::SIZEOF_INOTIFY_EVENT + ev.len as usize;
                let mut name = String::new();
                if ev.len > 0 {
                    // Name is NUL-terminated; trim trailing zeros.
                    let mut name_bytes =
                        &buf[offset + unix::SIZEOF_INOTIFY_EVENT..offset + record_size];
                    if let Some(i) = name_bytes.iter().position(|&c| c == 0) {
                        name_bytes = &name_bytes[..i];
                    }
                    // PORT: Go names are bytes; the port form keeps them
                    // (see walkdir_unix.rs `read_dir_entries`).
                    name = go_string_from_os(OsStr::from_bytes(name_bytes));
                }

                if ev.mask & unix::IN_Q_OVERFLOW != 0 {
                    {
                        let _b = self.base.mu.lock().unwrap();
                        let l = self.locked.lock().unwrap();
                        for subs in l.subscriptions.values() {
                            for sub in subs {
                                sub.dir_watch.events.set_error(ERR_OVERFLOW.clone());
                                watchers_touched.insert(
                                    Arc::as_ptr(&sub.dir_watch) as usize,
                                    sub.dir_watch.clone(),
                                );
                            }
                        }
                    }
                    offset += record_size;
                    continue;
                }

                self.handle_event(&ev, &name, &mut watchers_touched);
                offset += record_size;
            }
        }
        for w in watchers_touched.values() {
            w.notify();
        }
        watchers_touched.clear();
        Ok(())
    }

    // Go: inotify_linux.go:314 inotifyBackend.handleEvent
    /// handleEvent mirrors `inotifyBackend::handleEvent`.
    pub fn handle_event(
        &self,
        ev: &unix::InotifyEvent,
        name: &str,
        touched: &mut FxHashMap<usize, Arc<DirWatch>>,
    ) {
        let _b = self.base.mu.lock().unwrap();

        // b.subscriptions[wd] holds at most one entry per *inotifySubscription
        // pointer (watchDir always appends a fresh struct), so no dedup is
        // necessary; the upstream C++ used an unordered_set keyed by
        // shared_ptr identity but the equivalent Go invariant is structural.
        //
        // PORT: Go ranges over the slice value while handleSubscription may
        // filter the same backing array in place. The port ranges over a
        // copy of the list.
        let subs: Vec<Arc<InotifySubscription>> = self
            .locked
            .lock()
            .unwrap()
            .subscriptions
            .get(&ev.wd)
            .cloned()
            .unwrap_or_default();
        for s in &subs {
            if self.handle_subscription(ev, name, s) {
                touched.insert(Arc::as_ptr(&s.dir_watch) as usize, s.dir_watch.clone());
            }
        }
    }

    // Go: inotify_linux.go:330 inotifyBackend.handleSubscription
    /// handleSubscription mirrors `inotifyBackend::handleSubscription`.
    pub fn handle_subscription(
        &self,
        ev: &unix::InotifyEvent,
        name: &str,
        sub: &InotifySubscription,
    ) -> bool {
        let w = &sub.dir_watch;
        let mut path = sub.path.clone();
        let mut watch_path = sub.watch_path.clone();
        let is_dir = ev.mask & unix::IN_ISDIR != 0;
        if !name.is_empty() {
            path = format!("{path}/{name}");
            watch_path = format!("{watch_path}/{name}");
        }

        if ev.mask & (unix::IN_CREATE | unix::IN_MOVED_TO) != 0 {
            w.events.create(&path);
            if is_dir && w.recursive {
                let _ = walk_dir(
                    &watch_path,
                    true,
                    Some(&mut |p: &str, p_is_dir: bool| -> Result<(), GoError> {
                        if !p_is_dir {
                            return Ok(());
                        }
                        let _ = self.watch_dir(w, &w.display_path(p), p);
                        Ok(())
                    }),
                );
            }
        } else if ev.mask & unix::IN_MODIFY != 0 {
            w.events.update(&path);
        } else if ev.mask
            & (unix::IN_DELETE | unix::IN_DELETE_SELF | unix::IN_MOVED_FROM | unix::IN_MOVE_SELF)
            != 0
        {
            let is_self_event = ev.mask & (unix::IN_DELETE_SELF | unix::IN_MOVE_SELF) != 0;
            // Ignore delete/move self events unless this is the watch root.
            if is_self_event && path != w.dir {
                return false;
            }
            // If deleted item is a dir, drop matching subscriptions.
            // XXX: self events don't have IN_ISDIR set.
            if is_self_event || is_dir {
                let mut l = self.locked.lock().unwrap();
                let inotify = l.inotify;
                l.subscriptions.retain(|wd, list| {
                    list.retain(|s| !is_path_or_descendant(&s.path, &path));
                    if list.is_empty() {
                        let _ = unix::inotify_rm_watch(inotify, *wd as u32);
                        return false;
                    }
                    true
                });
            }
            w.events.remove(&path);
            // If the watched root itself is gone the kernel has already
            // auto-removed every wd associated with this dirWatch and no
            // further events will fire. Surface ErrWatchTerminated so the
            // caller knows to clean up; the delete event above still
            // flows through the same callback.
            if is_self_event && path == w.dir {
                w.events.set_error(errors::errorf(
                    format!(
                        "{}: watched directory removed",
                        ERR_WATCH_TERMINATED.error()
                    ),
                    vec![ERR_WATCH_TERMINATED.clone()],
                ));
            }
        }
        true
    }
}

// PORT: Go's inline test `s.path == path || (len(s.path) > len(path) &&
// s.path[len(path)] == '/' && s.path[:len(path)] == path)` on bytes.
fn is_path_or_descendant(s_path: &str, path: &str) -> bool {
    let s = s_path.as_bytes();
    let p = path.as_bytes();
    s == p || (s.len() > p.len() && s[p.len()] == b'/' && &s[..p.len()] == p)
}
