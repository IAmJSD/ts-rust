//! Go: internal/fswatch/kqueue.go (the kqueue backend for darwin, FreeBSD,
//! OpenBSD, NetBSD and DragonFly).
//!
//! PORT: D-W1 (no `libc`, no `unsafe`). The syscalls go through the `unix`
//! shim of these targets (unix_bsd.rs: `nix` for kqueue and kevent, `rustix`
//! and `std` for the rest).
//!
//! PORT: sharing. Go shares `*dirEntry` values between the per-subscribe
//! `entries` maps and `fdToEntry`, and shares each `entries` map between the
//! subscriptions of one subscribe call. The port shares them through `Arc`:
//! a `DirEntry` keeps `isDir` and `state` (the open fd, -1 for Go `nil`) in
//! atomics, and an `entries` map is an `Arc<Mutex<..>>` (`Entries`). Go
//! compares map pointers (`&sub.entries == entriesPtr`); the port compares
//! the `Arc`s.
//!
//! PORT: locks. kqueue.go's `b.mu` is the backend's own `mu` (it shadows
//! `watcherBase.mu`), here `self.mu` over `KqueueLocked`. Go reads and
//! writes the `entries` maps without a lock, from the event loop (and from
//! subscribe before it publishes a map). The port takes an entries lock for
//! each access, always after `self.mu` when both are held, and never holds
//! it across a call that takes `self.mu`.

use crate::fswatch::prelude::*;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::frontend::vfs::osvfs::go_string_from_os;
use crate::fswatch::unix;
use crate::fswatch::walkdir_unix::walk_dir;
use crate::gostd::errors;

// ---------------------------------------------------------------------------
// kqueue.go: kqueue backend (macOS, FreeBSD, OpenBSD, NetBSD, DragonFlyBSD)
//
// Uses the kernel's kqueue/kevent mechanism to watch individual files and
// directories via EVFILT_VNODE. Unlike inotify, kqueue requires an open file
// descriptor per watched path, not just per directory. On macOS, O_EVTONLY
// opens files for event monitoring only; on other BSDs, O_RDONLY is used.
//
//	┌──────────────────────────────────────────────────────────────┐
//	│                       kqueueBackend                          │
//	│                                                              │
//	│  ┌───────────┐       kevent(2)        ┌──────────────────┐   │
//	│  │ pipe[0]   ├───────────────────────►│                  │   │
//	│  │ (wakeup)  │                        │  start()         │   │
//	│  └───────────┘                        │  goroutine       │   │
//	│  ┌───────────┐      EVFILT_VNODE      │  (event loop)    │   │
//	│  │ kqueue    ├───────────────────────►│                  │   │
//	│  │ fd        │                        └────────┬─────────┘   │
//	│  └───────────┘                                 │             │
//	│                                                ▼             │
//	│             ┌──────────────────────────────────────────────┐ │
//	│             │  fdToEntry:  map[fd]   → *dirEntry           │ │
//	│             │  subsByPath: map[path] → []*kqueueSub        │ │
//	│             │                                              │ │
//	│             │  Each dirEntry.state stores the open fd      │ │
//	│             └──────────────────────────────────────────────┘ │
//	└──────────────────────────────────────────────────────────────┘
//
// Goroutines and threading:
//   - One long-lived goroutine (start), launched by watcherBase.run(). It
//     owns the kevent(2) loop and runs for the process lifetime. All event
//     dispatch (compareDir, handleFileEvent) executes on this goroutine.
//     compareDir and handleFileEvent acquire b.mu for watch/fd lookups.
//   - subscribe/closeWatch run on the caller's goroutine under
//     watcherBase.mu. watchPath acquires b.mu to register fd mappings.
//
// Callback delivery:
//   dirWatch.notify() posts to the shared process-wide debouncer. After a
//   coalescing window (50 ms min / 500 ms max), the debouncer invokes all
//   registered WatchCallbacks on its own dedicated goroutine; never on
//   the caller's goroutine or the event-loop goroutine.
//
// WatchDirectory flow:
//  1. Walk the target directory, building a path→dirEntry map (caller goroutine).
//  2. For every entry (file or directory), open an fd and register it with
//     kqueue for EVFILT_VNODE events (NOTE_DELETE, NOTE_WRITE, NOTE_EXTEND,
//     NOTE_ATTRIB, NOTE_RENAME, NOTE_REVOKE). Store the fd↔dirEntry mapping.
//
// Event dispatch (on the start goroutine):
//   - NOTE_WRITE on a directory → compareDir: re-read the directory from
//     disk, diff against the in-memory tree, emit update events for new
//     entries (opening + watching them) and delete events for removed ones
//     (closing their fds).
//   - NOTE_DELETE / NOTE_RENAME / NOTE_REVOKE → close the stale fd. For a
//     pure NOTE_DELETE on a file, tryRewatchLocked checks whether the path
//     was immediately recreated (atomic-save pattern) and emits update
//     instead of delete if so. Otherwise emit delete and remove from the
//     tree. Directories skip tryRewatchLocked to avoid spurious updates
//     during RemoveAll races.
//   - NOTE_WRITE / NOTE_ATTRIB / NOTE_EXTEND on a file → emit update.
//   After processing all returned kevents, call dirWatch.notify() on each
//   touched dirWatch to trigger the debouncer.
//
// Shutdown:
//   Write a byte to pipe[1] → kevent sees the pipe fd → loop exits →
//   close all tracked fds, the kqueue fd, and the pipe.
// ---------------------------------------------------------------------------

// Go: filepath.Separator (kqueue.go builds only on unix targets).
const SEPARATOR: char = '/';

// PORT: Go writes this fflags value at each of its three EVFILT_VNODE
// registrations.
const VNODE_FFLAGS: u32 = unix::NOTE_DELETE
    | unix::NOTE_WRITE
    | unix::NOTE_EXTEND
    | unix::NOTE_ATTRIB
    | unix::NOTE_RENAME
    | unix::NOTE_REVOKE;

// Go: kqueue.go:88 openForEvents
/// openForEvents opens a path for kqueue event monitoring. On darwin, O_EVTONLY
/// opens the file for event notification without granting read access. On other
/// BSDs, falls back to O_RDONLY.
///
/// PORT: Go `runtime.GOOS == "darwin"` is `target_os = "macos"`.
pub fn open_for_events(path: &str) -> Result<i32, GoError> {
    let mut flags = unix::O_RDONLY;
    if cfg!(target_os = "macos") {
        flags = 0x8000; // O_EVTONLY, darwin-only
    }
    unix::open(path, flags, 0)
}

// Go: kqueue.go:97 dirEntry
/// dirEntry tracks a watched path for kqueue's fd↔path mapping.
///
/// PORT: `is_dir` and `state` are atomics (see the file comment); `state`
/// is the open fd, or -1 for Go `nil`.
pub struct DirEntry {
    pub path: String,
    pub watch_path: String,
    pub is_dir: AtomicBool,
    pub state: AtomicI32, // stores the open fd
}

impl DirEntry {
    // PORT: Go `&dirEntry{path: .., watchPath: .., isDir: ..}` (state nil).
    pub fn new(path: String, watch_path: String, is_dir: bool) -> Arc<DirEntry> {
        Arc::new(DirEntry {
            path,
            watch_path,
            is_dir: AtomicBool::new(is_dir),
            state: AtomicI32::new(-1),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir.load(Ordering::SeqCst)
    }

    /// PORT: Go `fd, ok := entry.state.(int)`: `Some(fd)` when set.
    pub fn fd(&self) -> Option<i32> {
        let fd = self.state.load(Ordering::SeqCst);
        (fd >= 0).then_some(fd)
    }
}

/// PORT: Go `map[string]*dirEntry`, shared by pointer (see the file comment).
pub type Entries = Arc<Mutex<FxHashMap<String, Arc<DirEntry>>>>;

// Go: kqueue.go:105 kqueueSubscription
/// kqueueSubscription.
pub struct KqueueSubscription {
    pub dir_watch: Arc<DirWatch>,
    pub path: String,
    pub entries: Entries,
    pub fd: i32,
}

// Go: kqueue.go:114 kqueueBackend
/// kqueueBackend. It embeds treeReaderBackend (via Go
/// composition) just like the inheritance hierarchy.
pub struct KqueueBackend {
    pub base: WatcherBase,

    /// local lock for kqueue-specific maps
    /// PORT: Go `mu` guards the fields in `KqueueLocked`.
    pub mu: Mutex<KqueueLocked>,
    /// PORT: Go `kq int`, written by the start goroutine.
    pub kq: AtomicI32,
    // pipeFDs[0] is read in the Start goroutine only. pipeFDs[1] is written
    // by Shutdown (any goroutine) to wake the loop, so it lives in
    // pipeWriteFD as an atomic with a sentinel of -1 once closed.
    // PORT: Rust needs a lock to share `pipe_fds`; only the start thread
    // takes it.
    pub pipe_fds: Mutex<[i32; 2]>,
    pub pipe_write_fd: AtomicI32,
    pub ended_signal: SignalChan,

    // Persistent buffer reused across event batches. Only accessed
    // from the start goroutine, so no synchronization needed.
    // PORT: Rust needs a lock to share it; only the start thread takes it.
    pub watchers_touched: Mutex<FxHashMap<usize, Arc<DirWatch>>>,
}

/// PORT: the `kqueueBackend` fields that Go guards with its local `b.mu`.
#[derive(Default)]
pub struct KqueueLocked {
    /// multimap<path, sub>
    pub subs_by_path: FxHashMap<String, Vec<Arc<KqueueSubscription>>>,
    pub fd_to_entry: FxHashMap<i32, Arc<DirEntry>>,
}

// Go: kqueue.go:133 init
// PORT: Go's `init()` sets the factory on the package var `kqueueWatcher`;
// the port's package var calls this when it is built.
pub fn init(kqueue_watcher: &mut WatcherStruct) {
    let factory: WatcherFactory = || -> Arc<dyn WatcherImpl> { new_kqueue_backend() };
    kqueue_watcher.factory = Some(factory);
}

// Go: kqueue.go:137 newKqueueBackend
pub fn new_kqueue_backend() -> Arc<KqueueBackend> {
    Arc::new_cyclic(|self_: &Weak<KqueueBackend>| {
        let b = KqueueBackend {
            base: WatcherBase::default(),
            mu: Mutex::new(KqueueLocked::default()),
            kq: AtomicI32::new(-1),
            pipe_fds: Mutex::new([-1, -1]),
            pipe_write_fd: AtomicI32::new(-1),
            ended_signal: SignalChan::new(),
            watchers_touched: Mutex::new(FxHashMap::default()),
        };
        let self_impl: Weak<dyn WatcherImpl> = self_.clone();
        b.base.init(self_impl);
        b
    })
}

// PORT: Go `defer func() { b.closeSubscriptions(); b.closeFDs();
// close(b.endedSignal) }()` in start. The guard also runs when start
// unwinds, as a Go defer does in a panic.
struct KqueueStartDefer<'a>(&'a KqueueBackend);

impl Drop for KqueueStartDefer<'_> {
    fn drop(&mut self) {
        self.0.close_subscriptions();
        self.0.close_fds();
        self.0.ended_signal.close();
    }
}

// PORT: Go `touched[w] = struct{}{}` on a `map[*dirWatch]struct{}`.
fn touch(touched: &mut FxHashMap<usize, Arc<DirWatch>>, w: &Arc<DirWatch>) {
    touched.insert(Arc::as_ptr(w) as usize, w.clone());
}

// PORT: Go builds this kevent at each EVFILT_VNODE registration.
fn vnode_kevent(fd: i32) -> unix::Kevent_t {
    let mut ev = unix::Kevent_t::default();
    unix::set_kevent(
        &mut ev,
        fd,
        unix::EVFILT_VNODE,
        unix::EV_ADD | unix::EV_CLEAR | unix::EV_ENABLE,
    );
    ev.fflags = VNODE_FFLAGS;
    ev
}

// PORT: Go `len(p) > len(path) && p[len(path)] == filepath.Separator &&
// p[:len(path)] == path`.
fn is_strict_descendant(p: &str, path: &str) -> bool {
    p.len() > path.len() && p.as_bytes()[path.len()] == SEPARATOR as u8 && p.starts_with(path)
}

impl WatcherImpl for KqueueBackend {
    // Go: kqueue.go:151 kqueueBackend.start
    fn start(&self) -> Result<(), GoError> {
        let kq = match unix::kqueue() {
            Ok(kq) => kq,
            Err(err) => {
                return Err(errors::errorf(
                    format!("unable to open kqueue: {}", err.error()),
                    vec![err],
                ));
            }
        };
        self.kq.store(kq, Ordering::SeqCst);
        let _defer = KqueueStartDefer(self);

        let mut pipe_fds = [-1, -1];
        if let Err(err) = unix::pipe(&mut pipe_fds) {
            return Err(errors::errorf(
                format!("unable to open pipe: {}", err.error()),
                vec![err],
            ));
        }
        *self.pipe_fds.lock().unwrap() = pipe_fds;
        self.pipe_write_fd.store(pipe_fds[1], Ordering::SeqCst);

        // WatchDirectory kqueue to the read side of the pipe so we can break the
        // loop on shutdown. SetKevent handles the per-arch Ident type
        // (uint64 on 64-bit, uint32 on 386/arm).
        let mut pipe_ev = unix::Kevent_t::default();
        unix::set_kevent(
            &mut pipe_ev,
            pipe_fds[0],
            unix::EVFILT_READ,
            unix::EV_ADD | unix::EV_CLEAR,
        );
        if let Err(err) = unix::kevent(kq, &[pipe_ev], &mut [], None) {
            return Err(errors::errorf(
                format!("unable to watch pipe: {}", err.error()),
                vec![err],
            ));
        }

        self.base.notify_started();

        let mut events = vec![unix::Kevent_t::default(); 128];
        loop {
            let n = match unix::kevent(kq, &[], &mut events, None) {
                Ok(n) => n,
                Err(err) => {
                    if errors::is(&err, &errors::from_value(unix::EINTR)) {
                        continue;
                    }
                    return Err(errors::errorf(
                        format!("kevent error: {}", err.error()),
                        vec![err],
                    ));
                }
            };

            let mut watchers_touched = self.watchers_touched.lock().unwrap();
            let mut stop = false;
            for ev in &events[..n as usize] {
                let mut fflags = ev.fflags;
                let flags = ev.flags;
                let fd = ev.ident as i32;
                if fd == pipe_fds[0] {
                    stop = true;
                    break;
                }

                // EV_ERROR indicates kevent couldn't apply a changelist
                // entry or that the kernel rejected the registration.
                // Data carries the errno. Skip dispatching as a normal
                // event since fflags are not meaningful in this case.
                if flags & unix::EV_ERROR != 0 {
                    continue;
                }

                let entry = self.mu.lock().unwrap().fd_to_entry.get(&fd).cloned();
                let Some(entry) = entry else {
                    continue;
                };

                if fflags & unix::NOTE_WRITE != 0 && entry.is_dir() {
                    self.compare_dir(fd, &entry.path, &mut watchers_touched);
                    // NOTE_WRITE on a dir already ran compareDir above.
                    // On DragonFlyBSD, rename-over coalesces NOTE_DELETE
                    // with NOTE_WRITE on the parent directory (rather than
                    // firing NOTE_DELETE on the replaced file's fd).
                    // Skip handleFileEvent so we don't misinterpret the
                    // coalesced NOTE_DELETE as the directory itself being
                    // removed.
                    fflags &= !unix::NOTE_DELETE;
                }
                if fflags & !unix::NOTE_WRITE != 0 || !entry.is_dir() {
                    self.handle_file_event(fflags, &entry, &mut watchers_touched);
                }
            }

            for w in watchers_touched.values() {
                w.notify();
            }
            watchers_touched.clear();
            if stop {
                break;
            }
        }

        Ok(())
    }

    // Go: kqueue.go:278 kqueueBackend.shutdown
    fn shutdown(&self) {
        let fd = self.pipe_write_fd.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        let _ = unix::write(fd, b"X");
        self.ended_signal.wait();
    }

    // Go: kqueue.go:457 kqueueBackend.subscribe
    /// subscribe mirrors `kqueueBackend::subscribe`. Called under watcherBase.mu
    /// via watchAdd.
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        // Build the entries map without registering any watches or
        // subscriptions. This avoids a data race: registering a subscription
        // publishes the entries map to the event loop (via subsByPath),
        // which could read it via compareDir while we're still populating it.
        let mut entries: FxHashMap<String, Arc<DirEntry>> = FxHashMap::default();
        walk_dir(
            &w.physical_dir,
            w.recursive,
            Some(
                &mut |watch_path: &str, is_dir: bool| -> Result<(), GoError> {
                    let path = w.display_path(watch_path);
                    entries.insert(
                        path.clone(),
                        DirEntry::new(path, watch_path.to_string(), is_dir),
                    );
                    Ok(())
                },
            ),
        )?;

        // Open fds, register kevents, and publish subscriptions under b.mu.
        // Holding the lock for the entire block ensures that the event loop
        // cannot see a partially-built entries map, and that fds are always
        // tracked in fdToEntry (no leak on early return).
        let mut l = self.mu.lock().unwrap();
        let kq = self.kq.load(Ordering::SeqCst);

        // PORT: Go ranges over the map while it deletes from it; the port
        // ranges over the keys.
        let paths: Vec<String> = entries.keys().cloned().collect();
        for path in paths {
            let entry = entries[&path].clone();
            let fd = match open_for_events(&entry.watch_path) {
                Ok(fd) => fd,
                Err(err) => {
                    if path == w.dir {
                        Self::cleanup_entries_locked(&mut l, &entries);
                        return Err(watch_error(w, err));
                    }
                    entries.remove(&path);
                    continue;
                }
            };
            if let Err(err) = unix::kevent(kq, &[vnode_kevent(fd)], &mut [], None) {
                let _ = unix::close(fd);
                if path == w.dir {
                    Self::cleanup_entries_locked(&mut l, &entries);
                    return Err(watch_error(w, err));
                }
                entries.remove(&path);
                continue;
            }
            entry.state.store(fd, Ordering::SeqCst);
            l.fd_to_entry.insert(fd, entry);
        }

        let entries: Entries = Arc::new(Mutex::new(entries));
        for (path, entry) in entries.lock().unwrap().iter() {
            let fd = entry.state.load(Ordering::SeqCst);
            let sub = Arc::new(KqueueSubscription {
                dir_watch: w.clone(),
                path: path.clone(),
                entries: entries.clone(),
                fd,
            });
            l.subs_by_path.entry(path.clone()).or_default().push(sub);
        }
        Ok(())
    }

    // Go: kqueue.go:746 kqueueBackend.closeWatch
    /// closeWatch mirrors `kqueueBackend::closeWatch`.
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), GoError> {
        let mut l = self.mu.lock().unwrap();
        let l = &mut *l;
        l.subs_by_path.retain(|_path, list| {
            // PORT: Go filters in place (`kept := list[:0]`), so `list[0]`
            // below is still the first subscription before the filter.
            let first_fd = list[0].fd;
            let before = list.len();
            list.retain(|s| !Arc::ptr_eq(&s.dir_watch, w));
            let removed_any = list.len() != before;
            if !removed_any {
                return true;
            }
            if list.is_empty() {
                // Closing the file descriptor automatically unwatches it in kqueue.
                let _ = unix::close(first_fd);
                l.fd_to_entry.remove(&first_fd);
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

// PORT: Go `&dirWatchError{err: fmt.Errorf("error watching %s: %w", w.dir,
// err), dirWatch: w}`.
fn watch_error(w: &Arc<DirWatch>, err: GoError) -> GoError {
    DirWatchError {
        err: errors::errorf(
            format!("error watching {}: {}", w.dir, err.error()),
            vec![err],
        ),
        dir_watch: w.clone(),
    }
    .to_go_error()
}

impl KqueueBackend {
    // Go: kqueue.go:243 kqueueBackend.closeFDs
    pub fn close_fds(&self) {
        let mut pipe_fds = self.pipe_fds.lock().unwrap();
        if pipe_fds[0] >= 0 {
            let _ = unix::close(pipe_fds[0]);
            pipe_fds[0] = -1;
        }
        let fd = self.pipe_write_fd.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            let _ = unix::close(fd);
        }
        pipe_fds[1] = -1;
        let kq = self.kq.swap(-1, Ordering::SeqCst);
        if kq >= 0 {
            let _ = unix::close(kq);
        }
    }

    // Go: kqueue.go:258 kqueueBackend.closeSubscriptions
    pub fn close_subscriptions(&self) {
        let mut l = self.mu.lock().unwrap();
        let mut seen_fds: FxHashSet<i32> = FxHashSet::default();
        for list in l.subs_by_path.values() {
            for sub in list {
                if sub.fd < 0 {
                    continue;
                }
                if !seen_fds.insert(sub.fd) {
                    continue;
                }
                let _ = unix::close(sub.fd);
            }
        }
        l.subs_by_path = FxHashMap::default();
        l.fd_to_entry = FxHashMap::default();
    }

    // Go: kqueue.go:287 kqueueBackend.handleFileEvent
    pub fn handle_file_event(
        &self,
        fflags: u32,
        entry: &Arc<DirEntry>,
        touched: &mut FxHashMap<usize, Arc<DirWatch>>,
    ) {
        let mut l = self.mu.lock().unwrap();
        let subs = Self::find_subscriptions_locked(&l, &entry.path);

        if fflags & (unix::NOTE_DELETE | unix::NOTE_RENAME | unix::NOTE_REVOKE) != 0 {
            // Close the stale fd; the watched inode is gone.
            if let Some(old_fd) = entry.fd() {
                let _ = unix::close(old_fd);
                l.fd_to_entry.remove(&old_fd);
                entry.state.store(-1, Ordering::SeqCst);
            }

            let mut recreated = false;
            if fflags & unix::NOTE_DELETE != 0
                && fflags & (unix::NOTE_RENAME | unix::NOTE_REVOKE) == 0
                && !entry.is_dir()
            {
                recreated = self.try_rewatch_locked(&mut l, entry);
            }

            for sub in &subs {
                touch(touched, &sub.dir_watch);
                if recreated {
                    sub.dir_watch.events.update(&sub.path);
                } else {
                    sub.dir_watch.events.remove(&sub.path);
                    // If we lost a directory, walk the entries map and
                    // close every fd we had open for descendants. Some
                    // kernels (OpenBSD in particular) deliver only the
                    // parent's NOTE_DELETE/NOTE_RENAME and never fire
                    // NOTE_DELETE on the children; without this cleanup,
                    // modifying a file inside the moved tree later
                    // surfaces an event against the descendant's stale
                    // (pre-rename) path. We also emit a delete for each
                    // descendant we close, so callers don't miss those
                    // removals if the kernel didn't fire per-child events.
                    // (When the kernel does fire them, our follow-up
                    // handleFileEvent finds the fd already gone and is a
                    // no-op, so events.create's coalescing handles dups.)
                    if entry.is_dir() {
                        Self::close_descendant_fds_locked(
                            &mut l,
                            &sub.dir_watch,
                            &sub.entries,
                            &sub.path,
                        );
                    }
                    remove_entry_and_descendants(&mut sub.entries.lock().unwrap(), &sub.path);
                    // Root-of-watch deletion: no more events can fire
                    // for this dirWatch. Tell the caller.
                    if sub.path == sub.dir_watch.dir {
                        sub.dir_watch.events.set_error(errors::errorf(
                            format!(
                                "{}: watched directory removed",
                                ERR_WATCH_TERMINATED.error()
                            ),
                            vec![ERR_WATCH_TERMINATED.clone()],
                        ));
                    }
                }
            }
            if !recreated {
                l.subs_by_path.remove(&entry.path);
            }
            return;
        }

        for sub in &subs {
            touch(touched, &sub.dir_watch);
            if fflags & (unix::NOTE_WRITE | unix::NOTE_ATTRIB | unix::NOTE_EXTEND) != 0 {
                sub.dir_watch.events.update(&sub.path);
            }
        }
    }

    // Go: kqueue.go:356 kqueueBackend.closeDescendantFDsLocked
    /// closeDescendantFDsLocked closes every fd attached to an entry whose
    /// path lives strictly under root, removing the kevent registration and
    /// the corresponding b.subsByPath / b.fdToEntry bookkeeping, and emits a
    /// delete event for each. Used when a directory's parent is lost
    /// (deleted, renamed away) and the kernel didn't propagate the loss to
    /// children. eventList coalesces against any per-child NOTE_DELETE that
    /// arrives later.
    pub fn close_descendant_fds_locked(
        l: &mut KqueueLocked,
        w: &DirWatch,
        entries: &Entries,
        root: &str,
    ) {
        let prefix = format!("{root}{SEPARATOR}");
        for (path, e) in entries.lock().unwrap().iter() {
            if !path.starts_with(&prefix) {
                continue;
            }
            if let Some(fd) = e.fd() {
                let _ = unix::close(fd);
                l.fd_to_entry.remove(&fd);
                e.state.store(-1, Ordering::SeqCst);
            }
            l.subs_by_path.remove(path);
            w.events.remove(path);
        }
    }

    // Go: kqueue.go:375 kqueueBackend.tryRewatchLocked
    /// tryRewatchLocked checks whether a deleted path was immediately recreated
    /// with the same type. If so, it opens a new fd, registers a kqueue watch,
    /// and returns true. The caller should emit update instead of delete.
    pub fn try_rewatch_locked(&self, l: &mut KqueueLocked, entry: &Arc<DirEntry>) -> bool {
        let mut st = unix::Stat_t::default();
        if unix::lstat(&entry.watch_path, &mut st).is_err() {
            return false;
        }

        // Only fast-path when the recreated path has the same type;
        // a file→dir change needs a full tree rebuild via compareDir.
        let new_is_dir = st.mode & unix::S_IFMT == unix::S_IFDIR;
        if new_is_dir != entry.is_dir() {
            return false;
        }

        let Ok(fd) = open_for_events(&entry.watch_path) else {
            return false;
        };

        let kq = self.kq.load(Ordering::SeqCst);
        if unix::kevent(kq, &[vnode_kevent(fd)], &mut [], None).is_err() {
            let _ = unix::close(fd);
            return false;
        }

        entry.state.store(fd, Ordering::SeqCst);

        l.fd_to_entry.insert(fd, entry.clone());
        true
    }

    // Go: kqueue.go:408 kqueueBackend.closeEntryLocked
    pub fn close_entry_locked(l: &mut KqueueLocked, entry: &DirEntry) {
        if let Some(fd) = entry.fd() {
            let _ = unix::close(fd);
            l.fd_to_entry.remove(&fd);
            entry.state.store(-1, Ordering::SeqCst);
        }
    }

    // Go: kqueue.go:416 kqueueBackend.removeSubsForEntriesLocked
    pub fn remove_subs_for_entries_locked(l: &mut KqueueLocked, path: &str, entries: &Entries) {
        let Some(list) = l.subs_by_path.get_mut(path) else {
            return;
        };
        list.retain(|sub| !Arc::ptr_eq(&sub.entries, entries));
        if list.is_empty() {
            l.subs_by_path.remove(path);
        }
    }

    // Go: kqueue.go:432 kqueueBackend.removeEntryAndDescendantsLocked
    pub fn remove_entry_and_descendants_locked(
        l: &mut KqueueLocked,
        entries: &Entries,
        path: &str,
        include_root: bool,
    ) {
        let mut m = entries.lock().unwrap();
        let descendants: Vec<String> = m.keys().cloned().collect();
        for descendant in descendants {
            if descendant == path {
                if !include_root {
                    continue;
                }
            } else if !is_strict_descendant(&descendant, path) {
                continue;
            }
            if let Some(e) = m.remove(&descendant) {
                Self::close_entry_locked(l, &e);
            }
            Self::remove_subs_for_entries_locked(l, &descendant, entries);
        }
    }

    // Go: kqueue.go:448 kqueueBackend.findSubscriptionsLocked
    pub fn find_subscriptions_locked(l: &KqueueLocked, path: &str) -> Vec<Arc<KqueueSubscription>> {
        l.subs_by_path.get(path).cloned().unwrap_or_default()
    }

    // Go: kqueue.go:521 kqueueBackend.cleanupEntriesLocked
    /// cleanupEntriesLocked closes fds for all entries that have been opened.
    /// Called on subscribe failure to avoid fd leaks. Must be called under b.mu.
    pub fn cleanup_entries_locked(
        l: &mut KqueueLocked,
        entries: &FxHashMap<String, Arc<DirEntry>>,
    ) {
        for e in entries.values() {
            if let Some(fd) = e.fd() {
                let _ = unix::close(fd);
                l.fd_to_entry.remove(&fd);
                e.state.store(-1, Ordering::SeqCst);
            }
        }
    }

    // Go: kqueue.go:532 kqueueBackend.watchPath
    /// watchPath corresponds to `kqueueBackend::watchDir`.
    pub fn watch_path(&self, w: &Arc<DirWatch>, path: &str, entries: &Entries) -> bool {
        let entry = entries.lock().unwrap().get(path).cloned();
        let Some(entry) = entry else {
            return false;
        };
        let mut l = self.mu.lock().unwrap();

        if entry.fd().is_none() {
            let Ok(fd) = open_for_events(&entry.watch_path) else {
                return false;
            };
            let kq = self.kq.load(Ordering::SeqCst);
            if unix::kevent(kq, &[vnode_kevent(fd)], &mut [], None).is_err() {
                let _ = unix::close(fd);
                return false;
            }
            entry.state.store(fd, Ordering::SeqCst);
            l.fd_to_entry.insert(fd, entry.clone());
        }
        let sub = Arc::new(KqueueSubscription {
            dir_watch: w.clone(),
            path: path.to_string(),
            entries: entries.clone(),
            fd: entry.state.load(Ordering::SeqCst),
        });
        l.subs_by_path
            .entry(path.to_string())
            .or_default()
            .push(sub);
        true
    }

    // Go: kqueue.go:565 kqueueBackend.compareDir
    /// compareDir mirrors `kqueueBackend::compareDir`. Triggered when a watched
    /// directory has NOTE_WRITE: list the dir, diff against the tree, emit
    /// create/remove events.
    pub fn compare_dir(
        &self,
        _fd: i32,
        path: &str,
        touched: &mut FxHashMap<usize, Arc<DirWatch>>,
    ) -> bool {
        let subs = Self::find_subscriptions_locked(&self.mu.lock().unwrap(), path);

        // For non-recursive subscriptions, only compareDir on the root dir.
        // NOTE_WRITE on a child dir means something changed inside it, but
        // non-recursive mode shouldn't report those changes. Emit an update
        // for the child dir itself (its metadata changed) and return.
        let mut filtered_subs: Vec<Arc<KqueueSubscription>> = Vec::new();
        for s in subs {
            if !s.dir_watch.recursive && path != s.dir_watch.dir {
                s.dir_watch.events.update(path);
                touch(touched, &s.dir_watch);
            } else {
                filtered_subs.push(s);
            }
        }
        if filtered_subs.is_empty() {
            return true;
        }
        let subs = filtered_subs;

        let dir_start = format!("{path}{SEPARATOR}");
        struct DiskSnapshot {
            entries: Vec<ReadEntry>,
            current_display_paths: FxHashSet<String>,
        }
        let mut snapshots: FxHashMap<String, DiskSnapshot> = FxHashMap::default();

        // Each subscription has its own entries map (built in subscribe).
        // Multiple subs at the same path arise from multiple dirWatches
        // covering overlapping subtrees; their maps are always distinct, so
        // we iterate subs directly rather than trying to dedup by map identity.
        for sub in &subs {
            let base_entry = sub.entries.lock().unwrap().get(path).cloned();
            let Some(base_entry) = base_entry else {
                continue;
            };
            let watch_path = base_entry.watch_path.clone();
            let watch_dir_start = format!("{watch_path}{SEPARATOR}");

            if !snapshots.contains_key(&watch_path) {
                let Ok(disk_entries) = read_entries(&watch_path) else {
                    continue;
                };
                let current_display_paths = disk_entries
                    .iter()
                    .map(|ent| format!("{dir_start}{}", ent.name))
                    .collect();
                snapshots.insert(
                    watch_path.clone(),
                    DiskSnapshot {
                        entries: disk_entries,
                        current_display_paths,
                    },
                );
            }
            let snapshot = &snapshots[&watch_path];

            let entries = &sub.entries;
            for ent in &snapshot.entries {
                let full_path = format!("{dir_start}{}", ent.name);
                let full_watch_path = format!("{watch_dir_start}{}", ent.name);

                let existing = entries.lock().unwrap().get(&full_path).cloned();
                if let Some(existing) = existing {
                    // Check if the fd still refers to the same inode as
                    // the path on disk. On DragonFlyBSD, rename-over
                    // doesn't fire NOTE_DELETE on the replaced file's fd,
                    // leaving a stale entry whose fd points to the old
                    // (now unlinked) inode.
                    if let Some(fd) = existing.fd() {
                        let mut fd_st = unix::Stat_t::default();
                        let mut path_st = unix::Stat_t::default();
                        if unix::fstat(fd, &mut fd_st).is_ok()
                            && unix::lstat(&full_watch_path, &mut path_st).is_ok()
                            && (fd_st.dev != path_st.dev || fd_st.ino != path_st.ino)
                        {
                            // Inode changed: path was replaced.
                            let mut l = self.mu.lock().unwrap();
                            Self::close_entry_locked(&mut l, &existing);
                            Self::remove_subs_for_entries_locked(&mut l, &full_path, entries);
                            if existing.is_dir() {
                                Self::remove_entry_and_descendants_locked(
                                    &mut l, entries, &full_path, false,
                                );
                            }
                            existing.is_dir.store(ent.is_dir, Ordering::SeqCst);
                        }
                    }
                    if existing.fd().is_some() {
                        continue;
                    }
                    // Entry exists but fd is stale: the file was replaced.
                    // Re-watch it and emit an update.
                    if !self.watch_path(&sub.dir_watch, &full_path, entries) {
                        continue;
                    }
                    sub.dir_watch.events.update(&full_path);
                    touch(touched, &sub.dir_watch);
                    if ent.is_dir && sub.dir_watch.recursive {
                        self.watch_new_tree(sub, &full_watch_path);
                    }
                    continue;
                }
                let e = DirEntry::new(full_path.clone(), full_watch_path.clone(), ent.is_dir);
                entries.lock().unwrap().insert(full_path.clone(), e);
                if !self.watch_path(&sub.dir_watch, &full_path, entries) {
                    entries.lock().unwrap().remove(&full_path);
                    continue;
                }
                sub.dir_watch.events.create(&full_path);
                touch(touched, &sub.dir_watch);

                // For recursive subscriptions, walk into the new directory
                // to catch pre-populated subdirectories (e.g. a directory
                // tree moved into the watched area).
                if ent.is_dir && sub.dir_watch.recursive {
                    self.watch_new_tree(sub, &full_watch_path);
                }
            }

            // Detect removals: entries directly under dirStart that no longer
            // exist on disk.
            let to_remove: Vec<String> = entries
                .lock()
                .unwrap()
                .keys()
                .filter(|p| {
                    let Some(rest) = p.strip_prefix(&dir_start) else {
                        return false;
                    };
                    !rest.contains(SEPARATOR) && !snapshot.current_display_paths.contains(*p)
                })
                .cloned()
                .collect();
            for p in &to_remove {
                sub.dir_watch.events.remove(p);
                touch(touched, &sub.dir_watch);
                {
                    let mut l = self.mu.lock().unwrap();
                    for (descendant, e) in entries.lock().unwrap().iter() {
                        if descendant != p && !is_strict_descendant(descendant, p) {
                            continue;
                        }
                        if let Some(fd) = e.fd() {
                            let _ = unix::close(fd);
                            l.fd_to_entry.remove(&fd);
                        }
                        l.subs_by_path.remove(descendant);
                    }
                }
                remove_entry_and_descendants(&mut entries.lock().unwrap(), p);
            }
        }
        true
    }

    // PORT: the `walkDir` callback that compareDir runs at its two places
    // (a replaced directory and a new one): add, report and watch every
    // entry below `full_watch_path`.
    fn watch_new_tree(&self, sub: &KqueueSubscription, full_watch_path: &str) {
        let _ = walk_dir(
            full_watch_path,
            true,
            Some(&mut |p: &str, p_is_dir: bool| -> Result<(), GoError> {
                if p == full_watch_path {
                    return Ok(()); // already handled above
                }
                let display_path = sub.dir_watch.display_path(p);
                let e = DirEntry::new(display_path.clone(), p.to_string(), p_is_dir);
                sub.entries.lock().unwrap().insert(display_path.clone(), e);
                sub.dir_watch.events.create(&display_path);
                self.watch_path(&sub.dir_watch, &display_path, &sub.entries);
                Ok(())
            }),
        );
    }
}

/// PORT: the parts of a Go `os.DirEntry` that compareDir reads.
pub struct ReadEntry {
    pub name: String,
    pub is_dir: bool,
}

// Go: kqueue.go:741 readEntries
/// readEntries lists directory entries (excluding "." and "..") at path.
///
/// PORT: Go `os.ReadDir`: `std::fs::read_dir`, sorted by name as
/// `os.ReadDir` sorts. `IsDir` is the dirent type without following links
/// (`DirEntry::file_type`, as walkdir.rs reads it).
pub fn read_entries(path: &str) -> Result<Vec<ReadEntry>, std::io::Error> {
    let mut entries: Vec<std::fs::DirEntry> = Vec::new();
    for e in std::fs::read_dir(path)? {
        entries.push(e?);
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries
        .iter()
        .map(|e| ReadEntry {
            name: go_string_from_os(e.file_name()),
            is_dir: e.file_type().is_ok_and(|t| t.is_dir()),
        })
        .collect())
}

// Go: kqueue.go:777 removeEntryAndDescendants
/// removeEntryAndDescendants removes path and all paths prefixed with
/// path + separator from the entries map.
pub fn remove_entry_and_descendants(entries: &mut FxHashMap<String, Arc<DirEntry>>, path: &str) {
    entries.remove(path);
    entries.retain(|k, _| !is_strict_descendant(k, path));
}
