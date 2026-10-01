//! PORT: not in Go (perf). Go parses the configs of a build in parallel:
//! `createBuildTasks` queues the parse of each config and of its
//! references on a work group (orchestrator.go:174). Here the configs parse
//! on the orchestrator thread, because `ParsedCommandLine` is not `Send`.
//! Most of a parse is the match of the `include` specs against the file
//! system (Go `getFileNamesFromConfigSpecs`). So threads parse the configs
//! of the build ahead of the orchestrator, each on the OS file system of
//! its thread with its own caches, and keep the file names that each
//! config's specs match. The orchestrator's parse of a config takes them
//! (`BuildHost`'s `ParseConfigHost::get_file_names_from_config_specs`)
//! when it matches the same specs from the same base path with the same
//! extensions, and waits for a thread that is still matching them. The
//! file names are a function of these inputs and of the file system, and
//! the build writes nothing before its graph is made. A config that no
//! thread has started yet is the orchestrator's own: it matches the specs
//! itself and queues the references it finds.
//!
//! Go caches the lookups of each match (`GetAccessibleEntries`,
//! `Realpath`) in the build host's `cachedvfs` for the rest of the build,
//! and a later program reads them: for example the listing of a typeRoots
//! directory for its automatic type directives, after the build wrote
//! into that directory. So a thread records the lookups of its match, and
//! when the orchestrator takes the match, they go into the host's cache
//! (`BuildStatCache::add`), as if the orchestrator had made them. The
//! thread made them before the build wrote anything, as Go does.
//!
//! The config threads are the threads of a `PrefetchPool`. When the graph
//! is made, the same threads read the build info files ahead of the
//! up-to-date checks (orchestrator.rs `BuildInfoPrefetch`), so a build
//! starts its prefetch threads once.

use crate::execute::build::host::TscExtendedConfigCache;
use crate::frontend::prelude::*;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// The most threads of a `PrefetchPool`.
const MAX_PREFETCH_THREADS: usize = 8;

/// A job of a `PrefetchPool` thread.
type PoolJob = Box<dyn FnOnce() + Send>;

struct PoolQueue {
    jobs: VecDeque<PoolJob>,
    /// Set when the `PrefetchPool` drops: then a thread with no job ends.
    closed: bool,
}

struct PoolShared {
    queue: Mutex<PoolQueue>,
    ready: Condvar,
}

/// PORT: not in Go (perf). The threads that read ahead of the
/// orchestrator: the config parses (`ConfigPrefetch`), then the build info
/// reads (orchestrator.rs `BuildInfoPrefetch`). Each thread runs the queued
/// jobs in order. Dropping the pool lets each thread end when the queue is
/// empty; the queued jobs still run.
// PERF (perfplan4 build fix 2b): one set of threads instead of one per
// prefetch saves up to 8 thread starts (about 72 µs each on the
// orchestrator thread).
pub struct PrefetchPool {
    shared: Arc<PoolShared>,
    threads: usize,
}

impl PrefetchPool {
    /// Starts up to `MAX_PREFETCH_THREADS` threads (at most the cores).
    /// None when no thread starts.
    pub fn start() -> Option<Self> {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(PoolQueue {
                jobs: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        });
        let mut threads = 0;
        for _ in 0..MAX_PREFETCH_THREADS.min(crate::program::available_cores()) {
            let shared = shared.clone();
            // A config parse and the JSON parse of a build info file are
            // recursive (nested JSON values, `extends` chains), so the
            // thread gets the Go stack size, as a parse worker does.
            let spawned = std::thread::Builder::new()
                .name("goport-prefetch".to_string())
                .stack_size(crate::gostd::stack::max_stack_size())
                .spawn(move || run_pool_thread(&shared));
            threads += usize::from(spawned.is_ok());
        }
        (threads > 0).then_some(PrefetchPool { shared, threads })
    }

    /// The number of threads.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Queues `count` runs of `job`. A job that is to run on every thread
    /// at once (a loop over a shared queue) is queued `threads()` times.
    pub fn run(&self, count: usize, job: impl Fn() + Send + Sync + 'static) {
        let job = Arc::new(job);
        let mut queue = lock(&self.shared.queue);
        for _ in 0..count {
            let job = job.clone();
            queue.jobs.push_back(Box::new(move || job()));
        }
        drop(queue);
        for _ in 0..count {
            self.shared.ready.notify_one();
        }
    }
}

impl Drop for PrefetchPool {
    fn drop(&mut self) {
        lock(&self.shared.queue).closed = true;
        self.shared.ready.notify_all();
    }
}

/// A `PrefetchPool` thread: runs the queued jobs until the pool drops and
/// the queue is empty.
fn run_pool_thread(shared: &PoolShared) {
    loop {
        let job = {
            let mut queue = lock(&shared.queue);
            loop {
                if let Some(job) = queue.jobs.pop_front() {
                    break job;
                }
                if queue.closed {
                    return;
                }
                queue = shared
                    .ready
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        job();
    }
}

/// What `get_file_names_from_config_specs` reads, other than the file
/// system: two matches with equal inputs give the same file names.
#[derive(Clone, PartialEq, Eq)]
struct MatchInputs {
    base_path: String,
    files: Vec<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    /// `get_supported_extensions` and its `WithJsonIfResolveJsonModule`
    /// form: the only parts of the options that the match reads.
    extensions: Vec<Vec<String>>,
    extensions_with_json: Vec<Vec<String>>,
}

impl MatchInputs {
    /// The inputs of a match. `None` without options (the match panics
    /// there, and then the orchestrator's own match panics too).
    fn of(
        specs: &ConfigFileSpecs,
        base_path: &str,
        options: Option<&CompilerOptions>,
        extra_extensions: &[String],
    ) -> Option<Self> {
        let options = options?;
        let extensions = get_supported_extensions(options, extra_extensions);
        let extensions_with_json = get_supported_extensions_with_json_if_resolve_json_module(
            Some(options),
            extensions.clone(),
        );
        Some(MatchInputs {
            base_path: base_path.to_string(),
            files: specs.validated_files_spec.clone(),
            include: specs.validated_include_specs.clone(),
            exclude: specs.validated_exclude_specs.clone(),
            extensions,
            extensions_with_json,
        })
    }
}

/// The file names that a thread matched for one config.
struct MatchedFileNames {
    inputs: MatchInputs,
    file_names: Vec<String>,
    literal_file_names_len: i32,
    /// The cached lookups that the match made (`RecordingFs`).
    lookups: StatCache,
}

enum SlotState {
    /// No thread has started the config.
    Queued,
    /// A thread parses the config.
    Running,
    /// The thread's match, `None` when the parse matched nothing (no
    /// config file, no options) or panicked.
    Done(Option<MatchedFileNames>),
    /// The orchestrator took the slot.
    Taken,
}

struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

struct Queue {
    /// Configs (file name and path) that no thread has taken, in the
    /// order they were found.
    pending: VecDeque<(String, Path, Arc<Slot>)>,
    closed: bool,
}

/// The parse options of the build: what `BuildHost::get_resolved_project_reference`
/// passes to `get_parsed_command_line_of_config_file_path`.
struct ParseOptions {
    compiler_options: CompilerOptions,
    command_line_raw: Option<IndexMap<String, CompilerOptionsValue>>,
    current_directory: String,
    use_case_sensitive_file_names: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    /// Every config that was queued, by path.
    slots: Mutex<FxHashMap<Path, Arc<Slot>>>,
    options: ParseOptions,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The config parses of one graph ahead of the orchestrator, on the
/// threads of a `PrefetchPool`. Dropping it closes the queue; each thread
/// takes its next pool job after the config it parses.
pub struct ConfigPrefetch {
    shared: Arc<Shared>,
}

impl ConfigPrefetch {
    /// Starts the config parses on every thread of `pool`.
    pub fn start(
        pool: &PrefetchPool,
        compiler_options: CompilerOptions,
        command_line_raw: Option<IndexMap<String, CompilerOptionsValue>>,
        compare_paths_options: &ComparePathsOptions,
    ) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                pending: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            slots: Mutex::new(FxHashMap::default()),
            options: ParseOptions {
                compiler_options,
                command_line_raw,
                current_directory: compare_paths_options.current_directory.clone(),
                use_case_sensitive_file_names: compare_paths_options.use_case_sensitive_file_names,
            },
        });
        let thread_shared = shared.clone();
        pool.run(pool.threads(), move || run_config_thread(&thread_shared));
        ConfigPrefetch { shared }
    }

    /// Queues the parse of each config in `configs` that was not queued
    /// before.
    pub fn queue(&self, configs: &[String]) {
        queue_configs(&self.shared, configs);
    }

    /// The match of a thread for the config at `path`, when its inputs
    /// equal those of the orchestrator's match (`inputs`). Waits for the
    /// thread that parses the config. A config that no thread has started
    /// becomes the orchestrator's: None, and no thread starts it.
    fn take(&self, path: &Path, inputs: &MatchInputs) -> Option<MatchedFileNames> {
        let slot = lock(&self.shared.slots).get(path).cloned()?;
        let mut state = lock(&slot.state);
        loop {
            match std::mem::replace(&mut *state, SlotState::Taken) {
                SlotState::Queued | SlotState::Taken => return None,
                SlotState::Running => {
                    *state = SlotState::Running;
                    state = slot
                        .done
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                SlotState::Done(matched) => {
                    return matched.filter(|matched| matched.inputs == *inputs);
                }
            }
        }
    }

    /// Go `getFileNamesFromConfigSpecs` for the orchestrator's parse of
    /// the config `config_file_name`: the file names that a thread
    /// matched, whose lookups go into `stats`, the cache of `fs`; or else
    /// its own match on `fs`.
    #[allow(clippy::too_many_arguments)]
    pub fn get_file_names_from_config_specs(
        &self,
        config_file_name: &str,
        config_file_specs: &ConfigFileSpecs,
        base_path: &str,
        options: Option<&CompilerOptions>,
        extra_extensions: &[String],
        fs: &dyn Fs,
        stats: &BuildStatCache,
    ) -> (Vec<String>, i32) {
        let path = to_path(
            config_file_name,
            &self.shared.options.current_directory,
            self.shared.options.use_case_sensitive_file_names,
        );
        if let Some(inputs) =
            MatchInputs::of(config_file_specs, base_path, options, extra_extensions)
            && let Some(matched) = self.take(&path, &inputs)
        {
            stats.add(&matched.lookups);
            return (matched.file_names, matched.literal_file_names_len);
        }
        get_file_names_from_config_specs(
            config_file_specs,
            base_path,
            options,
            fs,
            extra_extensions,
        )
    }
}

impl Drop for ConfigPrefetch {
    fn drop(&mut self) {
        let mut queue = lock(&self.shared.queue);
        queue.closed = true;
        queue.pending.clear();
        drop(queue);
        self.shared.ready.notify_all();
    }
}

fn queue_configs(shared: &Shared, configs: &[String]) {
    let mut added = 0;
    {
        let mut slots = lock(&shared.slots);
        let mut queue = lock(&shared.queue);
        if queue.closed {
            return;
        }
        for config in configs {
            let path = to_path(
                config,
                &shared.options.current_directory,
                shared.options.use_case_sensitive_file_names,
            );
            if slots.contains_key(&path) {
                continue;
            }
            let slot = Arc::new(Slot {
                state: Mutex::new(SlotState::Queued),
                done: Condvar::new(),
            });
            slots.insert(path.clone(), slot.clone());
            queue.pending.push_back((config.clone(), path, slot));
            added += 1;
        }
    }
    for _ in 0..added {
        shared.ready.notify_one();
    }
}

/// The config job of a pool thread: parses queued configs in order until
/// the queue closes, and queues the references of each.
fn run_config_thread(shared: &Shared) {
    // Go: sys.FS() is bundled.WrapFS(osvfs.FS()), and the build host
    // caches it (`cachedvfs.From`).
    let host = ThreadConfigHost {
        fs: cachedvfs_from(crate::frontend::bundled::wrap_fs(
            crate::frontend::vfs::osvfs_fs(),
        )),
        current_directory: shared.options.current_directory.clone(),
        matched: RefCell::new(None),
    };
    let extended_config_cache = TscExtendedConfigCache::default();
    loop {
        let (config, path, slot) = {
            let mut queue = lock(&shared.queue);
            loop {
                if queue.closed {
                    return;
                }
                if let Some(next) = queue.pending.pop_front() {
                    break next;
                }
                queue = shared
                    .ready
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        {
            let mut state = lock(&slot.state);
            if !matches!(*state, SlotState::Queued) {
                continue;
            }
            *state = SlotState::Running;
        }
        // A parse that panics is left to the orchestrator, which panics
        // on it too.
        let references = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            host.matched.borrow_mut().take();
            let (parsed, _) = get_parsed_command_line_of_config_file_path(
                &config,
                path.clone(),
                Some(&shared.options.compiler_options),
                shared.options.command_line_raw.as_ref(),
                &host,
                Some(&extended_config_cache),
            );
            parsed.map(|parsed| parsed.resolved_project_reference_paths().to_vec())
        }));
        let matched = match &references {
            Ok(_) => host
                .matched
                .borrow_mut()
                .take()
                .filter(|(name, _)| *name == config)
                .map(|(_, matched)| matched),
            Err(_) => None,
        };
        *lock(&slot.state) = SlotState::Done(matched);
        slot.done.notify_all();
        if let Ok(Some(references)) = references {
            queue_configs(shared, &references);
        }
    }
}

/// The parse config host of a config thread: records the match of the
/// config it parses.
struct ThreadConfigHost {
    fs: Rc<dyn Fs>,
    current_directory: String,
    /// The config name and match of the last `get_file_names_from_config_specs`.
    matched: RefCell<Option<(String, MatchedFileNames)>>,
}

impl ParseConfigHost for ThreadConfigHost {
    fn fs(&self) -> Rc<dyn Fs> {
        self.fs.clone()
    }

    fn get_current_directory(&self) -> String {
        self.current_directory.clone()
    }

    fn get_file_names_from_config_specs(
        &self,
        config_file_name: &str,
        config_file_specs: &ConfigFileSpecs,
        base_path: &str,
        options: Option<&CompilerOptions>,
        extra_extensions: &[String],
    ) -> (Vec<String>, i32) {
        let recording = RecordingFs {
            fs: &*self.fs,
            lookups: StatCache::default(),
        };
        let (file_names, literal_file_names_len) = get_file_names_from_config_specs(
            config_file_specs,
            base_path,
            options,
            &recording,
            extra_extensions,
        );
        *self.matched.borrow_mut() =
            MatchInputs::of(config_file_specs, base_path, options, extra_extensions).map(
                |inputs| {
                    (
                        config_file_name.to_string(),
                        MatchedFileNames {
                            inputs,
                            file_names: file_names.clone(),
                            literal_file_names_len,
                            lookups: recording.lookups,
                        },
                    )
                },
            );
        (file_names, literal_file_names_len)
    }
}

/// The file system of a config thread's match: `fs`, with each cached
/// lookup (Go `cachedvfs`: all but `Stat`) recorded in `lookups`.
struct RecordingFs<'a> {
    fs: &'a dyn Fs,
    lookups: StatCache,
}

impl Fs for RecordingFs<'_> {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.use_case_sensitive_file_names()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.lookups.file_exists(path, || self.fs.file_exists(path))
    }

    fn read_file(&self, path: &str) -> (String, bool) {
        self.fs.read_file(path)
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), FsError> {
        self.fs.write_file(path, data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), FsError> {
        self.fs.append_file(path, data)
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        self.fs.remove(path)
    }

    fn chtimes(
        &self,
        path: &str,
        a_time: Option<std::time::SystemTime>,
        m_time: Option<std::time::SystemTime>,
    ) -> Result<(), FsError> {
        self.fs.chtimes(path, a_time, m_time)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.lookups
            .directory_exists(path, || self.fs.directory_exists(path))
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.lookups
            .entries(path, || self.fs.get_accessible_entries(path))
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.fs.stat(path)
    }

    fn realpath(&self, path: &str) -> String {
        self.lookups.realpath(path, || self.fs.realpath(path))
    }
}
