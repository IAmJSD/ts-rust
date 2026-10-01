//! Go `compiler` package (Program, checker pool, diagnostics pipeline) plus
//! the parser-side `ast.SourceFile` fields that the checker reads.
//!
//! The graph (files, parse trees, module resolution) comes from the Go
//! frontend loader (`go_frontend`). This file has the Go `Program` methods
//! as free functions, the Go checker pool and the tsc diagnostics pipeline
//! and non-pretty formatter.
//!
//! Use: `let program = load(config_path); bind_all();` then the diagnostics
//! functions below. `load` installs the program for the process; the
//! loading thread also keeps the frontend program and the checker pool.
//!
//! A multi-program process (watch, language server, tests) loads program
//! versions with `try_load_version` and `update_program_version`, reads one
//! inside `core::enter_program`, and frees its checker pool with
//! `release_program`. The loading thread keeps the frontend program and the
//! checker pool of each version, by program id.

use crate::execute::tsc::compile::CompileTimes;
use crate::frontend::tspath;
use crate::gostd::{Context, context};
use crate::prelude::*;
use std::borrow::Cow;
use std::ops::Deref;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

mod go_frontend;
pub mod ls_program;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Go `ast.PragmaArgument`.
#[derive(Clone, Debug, Default)]
pub struct PragmaArgument {
    pub name: String,
    pub value: String,
    pub range: TextRange,
}

/// Go `ast.Pragma`. `kind` is the comment kind (Go `CommentRange.Kind`).
#[derive(Clone, Debug)]
pub struct Pragma {
    pub name: String,
    pub args: IndexMap<String, PragmaArgument>,
    pub range: TextRange,
    pub kind: SyntaxKind,
}

/// Go `ast.CheckJsDirective`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckJsDirective {
    pub enabled: bool,
    pub range: TextRange,
}

/// Go `ast.FileReference`.
#[derive(Clone, Debug, Default)]
pub struct FileReference {
    pub range: TextRange,
    pub file_name: String,
    pub resolution_mode: ResolutionMode,
    pub preserve: bool,
}

/// Go `ast.CommentDirective`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommentDirective {
    pub loc: TextRange,
    pub kind: CommentDirectiveKind,
}

/// Go `ast.SourceFileMetaData`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceFileMetaData {
    pub package_json_type: String,
    pub package_json_directory: String,
    pub implied_node_format: ModuleKind,
}

/// Go `module.PackageId`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PackageId {
    pub name: String,
    pub sub_module_name: String,
    pub version: String,
    pub peer_dependencies: String,
}

/// Go `module.ResolvedModule`.
#[derive(Clone, Default)]
pub struct ResolvedModule {
    pub resolved_file_name: String,
    pub original_path: String,
    pub extension: String,
    pub resolved_using_ts_extension: bool,
    /// #4712: the resolution used a content mapper extension (Go
    /// `ResolvedUsingExtraExtensions`).
    pub resolved_using_extra_extensions: bool,
    pub package_id: PackageId,
    pub is_external_library_import: bool,
    pub alternate_result: String,
    pub resolution_diagnostics: Vec<Diagnostic>,
}

impl ResolvedModule {
    // Go: module/types.go IsResolved
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        !self.resolved_file_name.is_empty()
    }
}

/// Go `*tsoptions.ParsedCommandLine` for a resolved project reference.
#[derive(Clone)]
pub struct ResolvedProjectReference {
    compiler_options: CompilerOptions,
    common_source_directory: String,
}

impl ResolvedProjectReference {
    /// A copy of the parts of a referenced project's command line that the
    /// checker reads.
    pub(crate) fn new(compiler_options: CompilerOptions, common_source_directory: String) -> Self {
        ResolvedProjectReference {
            compiler_options,
            common_source_directory,
        }
    }

    // Go: tsoptions/parsedcommandline.go CompilerOptions
    #[must_use]
    pub fn compiler_options(&self) -> &CompilerOptions {
        &self.compiler_options
    }

    // Go: tsoptions/parsedcommandline.go CommonSourceDirectory
    #[must_use]
    pub fn common_source_directory(&self) -> &str {
        &self.common_source_directory
    }
}

/// Go `tsoptions.SourceOutputAndProjectReference`.
#[derive(Clone)]
pub struct SourceOutputAndProjectReference {
    pub source: String,
    pub output_dts: String,
    /// PORT: Go shares one `*ParsedCommandLine` between the entries of a
    /// referenced project, so this is an `Arc`.
    pub resolved: Arc<ResolvedProjectReference>,
}

/// Go `ast.SourceFile` fields set by the parser and the program.
///
/// The fields that walk the tree (external module indicator, imports, ...)
/// are in `LateSourceFileInfo`, reached through `Deref`. The Go frontend
/// sets both parts when it builds the file (`go_frontend`).
// PORT: the program sets Go `SourceFile.Metadata` and the default library
// flag, and program versions that share a file version can differ in them,
// so they are in `VersionTables::file_meta`.
pub struct SourceFileInfo {
    pub file_name: String,
    pub path: String,
    pub is_declaration_file: bool,
    pub language_variant: LanguageVariant,
    pub script_kind: ScriptKind,
    pub pragmas: Vec<Pragma>,
    pub check_js_directive: Option<CheckJsDirective>,
    pub referenced_files: Vec<FileReference>,
    pub type_reference_directives: Vec<FileReference>,
    pub lib_reference_directives: Vec<FileReference>,
    pub comment_directives: Vec<CommentDirective>,
    // PERF: the diagnostic lists of a static file borrow the parsed file of
    // the Go frontend instead of copying it. The publish of the file keeps
    // that parse for good (see `go_files_of_unpublished_stores`). A freeable
    // file version (lsshells M3b) keeps no parse and owns copies, which are
    // freed with it.
    pub diagnostics: KeptData<[Diagnostic]>,
    pub js_diagnostics: KeptData<[Diagnostic]>,
    pub jsdoc_diagnostics: KeptData<[Diagnostic]>,
    /// True when a JSDoc cache miss means "not parsed" (Go parses lazily).
    pub has_lazy_js_doc: bool,
    late: OnceLock<LateSourceFileInfo>,
}

/// A parse list that a `SourceFileInfo` keeps (lsshells M3b): borrowed for
/// good from the parse that a static publish keeps (or leaked), or owned by
/// the `GoFile` of a freeable file version. It derefs to the list.
pub enum KeptData<T: ?Sized + 'static> {
    Borrowed(&'static T),
    Owned(Box<T>),
}

impl<T: ?Sized + 'static> Deref for KeptData<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        match self {
            KeptData::Borrowed(value) => value,
            KeptData::Owned(value) => value,
        }
    }
}

impl Deref for SourceFileInfo {
    type Target = LateSourceFileInfo;

    fn deref(&self) -> &LateSourceFileInfo {
        self.late
            .get()
            .expect("SourceFileInfo tree fields read before they are set")
    }
}

/// Go `ast.SourceFile` fields that need the installed tree.
pub struct LateSourceFileInfo {
    pub file_index: usize,
    pub external_module_indicator: Node,
    // PERF: like the diagnostic lists, `reparsed_clones` and `jsdoc_cache`
    // of a static file borrow the kept parse; a freeable file version owns
    // copies. The JSDoc cache has one list per host node, so a copy costs
    // one allocation per entry.
    pub reparsed_clones: KeptData<[Node]>,
    pub imports: Vec<Node>,
    pub module_augmentations: Vec<Node>,
    pub ambient_module_names: Vec<String>,
    pub uses_uri_style_node_core_modules: Tristate,
    /// Go `SourceFile.jsdocCache`: parsed JSDoc nodes by host node.
    pub jsdoc_cache: KeptData<FxHashMap<Node, Vec<Node>>>,
    post_bind: OnceLock<PostBindInfo>,
}

/// The JSDoc cache of a file with no eager entries.
static EMPTY_JSDOC_CACHE: FxHashMap<Node, Vec<Node>> =
    FxHashMap::with_hasher(rustc_hash::FxBuildHasher);

/// Go `ast.SourceFile` fields that the binder sets but that live on the
/// SourceFile in Go.
pub struct PostBindInfo {
    pub common_js_module_indicator: Node,
}

static NOT_BOUND: PostBindInfo = PostBindInfo {
    common_js_module_indicator: Node::NIL,
};

impl Deref for LateSourceFileInfo {
    type Target = PostBindInfo;

    // PORT: Go `SourceFile.CommonJSModuleIndicator` is set by the binder
    // (binder.go:924-943). The Rust binder stores it in `FileBindData`.
    // Before the file is bound the value is nil and not cached.
    fn deref(&self) -> &PostBindInfo {
        if let Some(info) = self.post_bind.get() {
            return info;
        }
        let indicator = crate::ast::with_go_file(self.file_index, |file| {
            file.file_bind
                .get()
                .map(|file_bind| file_bind.common_js_module_indicator)
        });
        let Some(common_js_module_indicator) = indicator else {
            return &NOT_BOUND;
        };
        self.post_bind.get_or_init(|| PostBindInfo {
            common_js_module_indicator,
        })
    }
}

/// Go `checkerPool` (compiler pool). Checkers are created on first use.
/// Each checker lives on its own worker thread; the pool holds the job
/// queue of each worker. Only the loading thread has pools, one for each
/// program version (`POOLS`).
struct CheckerPool {
    workers: Vec<std::sync::mpsc::Sender<Job>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// The program's emit pool, made on its first job
    /// (`send_emit_pool_jobs`). It stops with the checkers.
    emit: Option<EmitPool>,
}

impl CheckerPool {
    /// Stops the workers: each drops its checker and frees its synthetic
    /// nodes, then it closes their job queues and waits for each thread to
    /// end, so the checkers and the nodes they made are freed on return.
    fn shut_down(self) {
        for thread in self.stop() {
            // A job panic stays in its job result, so a worker ends normally.
            let _ = thread.join();
        }
    }

    /// `shut_down` without the wait: the workers drop their checkers, free
    /// their synthetic nodes and end while the caller goes on. Nothing waits
    /// for them; at process exit a worker that is still freeing just stops.
    fn shut_down_in_background(self) {
        drop(self.stop());
    }

    /// Sends each worker the job that drops its checker and frees its
    /// synthetic nodes, closes the job queues and returns the worker
    /// threads, with the threads of the emit pool.
    fn stop(self) -> Vec<std::thread::JoinHandle<()>> {
        let CheckerPool {
            workers,
            mut threads,
            emit,
        } = self;
        // A pool that only ends with the process forgets its checkers and
        // synthetic nodes (see `create_checkers`); a released program frees
        // them here.
        for worker in &workers {
            let _ = worker.send(Box::new(|| {
                stop_dts_twin();
                drop(WORKER_CHECKER.with(|slot| slot.borrow_mut().take()));
                free_synthetic_nodes();
                WORKER_RELEASED.with(|released| released.set(true));
            }));
        }
        drop(workers);
        if let Some(emit) = emit {
            threads.extend(emit.stop());
        }
        threads
    }
}

/// PORT: not in Go. The emit pool of a program: threads with no checker
/// that run the JS part of each file whose JS transforms make no checker
/// call (`emitter::emitter::js_emit_needs_checker`). Go runs every file's
/// emit on its own goroutine (`Program.Emit` queues one task per file on a
/// `core.WorkGroup`) and locks the checker only for each resolver call. A
/// Rust checker stays on its thread, so the rest of the emit stays there.
///
/// The threads take jobs from one shared queue, in the order they are
/// sent. Each thread starts from a `WorkerSeed`, like a checker worker.
struct EmitPool {
    queue: std::sync::mpsc::Sender<Job>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// Set by `stop`: when the queue closes, the threads free their
    /// synthetic nodes. Else they end with the process and leak them, as
    /// the checker workers do.
    released: Arc<std::sync::atomic::AtomicBool>,
    /// The jobs sent so far (`emit_pool_job_count`).
    jobs: usize,
}

impl EmitPool {
    /// Closes the queue and returns the threads. Each thread ends after the
    /// jobs already sent and frees its synthetic nodes.
    fn stop(self) -> Vec<std::thread::JoinHandle<()>> {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        drop(self.queue);
        self.threads
    }
}

/// The most threads of an emit pool.
const MAX_EMIT_THREADS: usize = 32;

/// The threads of a new emit pool of the current program: one per core (Go
/// runs the emit on GOMAXPROCS goroutines), at most `MAX_EMIT_THREADS`, but
/// 0 (no pool) when the cores are not more than the checkers. With no spare
/// core the pool only competes with the checker threads, and each d.ts part
/// loses the caches that its JS part warmed (effect at 4 cores and 4
/// checkers: +1.6% wall, +30 MiB). When the process may run on
/// `WIDE_CORES` CPUs or more and they hold SMT siblings (fewer physical
/// cores than CPUs), the pool gets the physical cores that the checkers do
/// not use. `GOPORT_EMIT_THREADS` sets the count at any core count (same
/// maximum); 0 turns the pool off, so every emit runs on the checker
/// threads as before the pool.
// PERF (perf11 effecttail E9, dbook, effect emit, perf10 release tsgo, 30
// paired rounds): at 32 threads (16 cores) a pool of 12 took 0.95% and
// 1.25% less wall time than 32 (-7 to -9 ms, peak RSS -29 MiB); the 32
// pool threads shared cores and L3 with the checkers. At 16 threads on 16
// cores 12 was neutral, so the rule leaves CPUs without siblings alone.
// perf10 found a pool of 4 17 ms faster on mini-743d (16 threads, 8 cores).
fn emit_thread_count() -> usize {
    static SET: OnceLock<Option<usize>> = OnceLock::new();
    let set = *SET.get_or_init(|| {
        std::env::var("GOPORT_EMIT_THREADS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
    });
    set.unwrap_or_else(|| {
        let cores = available_cores();
        let checkers = checker_count();
        if cores <= checkers {
            return 0;
        }
        if cores >= WIDE_CORES
            && let Some(physical) = physical_core_count()
            && physical < cores
        {
            return physical.saturating_sub(checkers);
        }
        cores
    })
    .min(MAX_EMIT_THREADS)
}

/// Makes the emit pool of the current program with `count` threads.
fn create_emit_pool(count: usize) -> EmitPool {
    let (queue, receiver) = std::sync::mpsc::channel::<Job>();
    let receiver = Arc::new(Mutex::new(receiver));
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let threads = (0..count)
        .map(|index| {
            let seed = WorkerSeed::take();
            let receiver = Arc::clone(&receiver);
            let released = Arc::clone(&released);
            std::thread::Builder::new()
                .name(format!("emit-{index}"))
                .stack_size(crate::gostd::stack::max_stack_size())
                .spawn(move || {
                    seed.install();
                    loop {
                        // Hold the lock only to take a job.
                        let job = receiver
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .recv();
                        let Ok(job) = job else { break };
                        job();
                    }
                    // Like a checker worker (see `create_checkers`): a
                    // released program frees the synthetic nodes, the end of
                    // the process leaks them.
                    if released.load(std::sync::atomic::Ordering::Acquire) {
                        free_synthetic_nodes();
                    } else {
                        forget_synthetic_nodes();
                    }
                })
                .unwrap_or_else(|err| crate::core::go_fatal_newosproc(&err))
        })
        .collect();
    EmitPool {
        queue,
        threads,
        released,
        jobs: 0,
    }
}

/// True when this emit can send JS parts to the emit pool: the pool is on
/// (`emit_thread_count`), the program is not `--singleThreaded` (Go's
/// single-threaded work group runs the emits last-queued-first on one
/// goroutine), no trace is written (a trace keeps each file's emit events
/// on one thread, as today), and the caller is not a checker thread.
/// Only the compile path emits through `program_emit`; the language server
/// does not.
pub fn emit_pool_enabled() -> bool {
    emit_thread_count() > 0
        && !single_threaded()
        && crate::tracing::get().is_none()
        && worker_index().is_none()
}

/// The result of a job on the emit pool (`send_emit_pool_jobs`) or on a
/// d.ts twin (`send_dts_twin_job`).
pub struct EmitPoolJob<R>(std::sync::mpsc::Receiver<JobResult<R>>);

impl<R> EmitPoolJob<R> {
    /// Waits for the job. `Err` holds the payload of its panic.
    pub fn join(self) -> std::thread::Result<R> {
        self.0.recv().expect("emit thread stopped")
    }
}

/// PORT: not in Go (perf). The d.ts twin of a checker worker: a thread that
/// prints the d.ts parts whose declaration transforms ran on the checker
/// (`program_emit`), so the checker can go on with its next file. The
/// transforms call the emit resolver, so they stay on the checker, and each
/// checker's d.ts text depends on its own check. The print needs no checker:
/// the only print handler of Go `emitDeclarationFile` maps declaration map
/// positions through the span map of a content-mapped source file. Go runs
/// each file's emit on its own goroutine and locks the checker only for each
/// resolver call.
///
/// The twin shares the checker's synthetic chunk numbers
/// (`share_synthetic_chunks`), so a handle names the same node on both
/// threads, and each job brings the nodes that its print reads
/// (`PrintPack`). The pool threads cannot print these trees: their chunk
/// numbers and their thread-local maps keyed by node are their own. The
/// twin runs its jobs in the order they are sent. It is made on the first
/// job of its checker, and it stops with the checker pool
/// (`CheckerPool::stop`).
struct DtsTwin {
    queue: std::sync::mpsc::Sender<Job>,
    thread: std::thread::JoinHandle<()>,
    /// Set by `stop_dts_twin`: the twin frees its synthetic nodes when its
    /// queue closes. Else it ends with the process and leaks them, like its
    /// checker.
    released: Arc<std::sync::atomic::AtomicBool>,
}

/// The jobs sent to d.ts twins in this process (`dts_twin_job_count`).
static DTS_TWIN_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Makes the d.ts twin of this checker worker. It starts from the current
/// program and its tables, and from an empty synthetic arena that shares
/// the chunk counters of this thread.
fn create_dts_twin() -> DtsTwin {
    let index = worker_index().expect("a d.ts twin belongs to a checker thread");
    let chunks = share_synthetic_chunks();
    let program = prog();
    let tables = current_tables();
    let (queue, receiver) = std::sync::mpsc::channel::<Job>();
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_released = Arc::clone(&released);
    let thread = std::thread::Builder::new()
        .name(format!("dts-twin-{index}"))
        .stack_size(crate::gostd::stack::max_stack_size())
        .spawn(move || {
            crate::core::set_thread_program(Some(program));
            if let Some(tables) = tables {
                TABLES.with(|cache| *cache.borrow_mut() = Some(tables));
            }
            install_twin_synthetic_arena(chunks);
            for job in receiver {
                job();
            }
            if thread_released.load(std::sync::atomic::Ordering::Acquire) {
                free_synthetic_nodes();
            } else {
                forget_synthetic_nodes();
            }
        })
        .unwrap_or_else(|err| crate::core::go_fatal_newosproc(&err));
    DtsTwin {
        queue,
        thread,
        released,
    }
}

/// Sends `f` to the d.ts twin of this checker worker, which is made on the
/// first call, and returns where its result arrives. Checker threads only.
pub fn send_dts_twin_job<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> EmitPoolJob<R> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let job: Job = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let _ = sender.send(result);
    });
    DTS_TWIN.with(|twin| {
        twin.borrow_mut()
            .get_or_insert_with(create_dts_twin)
            .queue
            .send(job)
            .expect("d.ts twin stopped");
    });
    DTS_TWIN_JOBS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    EmitPoolJob(receiver)
}

/// Stops the d.ts twin of this checker worker, if it has one: the twin ends
/// after the jobs already sent and frees its synthetic nodes, and this
/// waits for it.
fn stop_dts_twin() {
    let Some(twin) = DTS_TWIN.with(|twin| twin.borrow_mut().take()) else {
        return;
    };
    twin.released
        .store(true, std::sync::atomic::Ordering::Release);
    drop(twin.queue);
    let _ = twin.thread.join();
}

/// The number of jobs sent to d.ts twins in this process so far. Tests use
/// it to see that the twins ran.
pub fn dts_twin_job_count() -> usize {
    DTS_TWIN_JOBS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sends `jobs` to the emit pool of the current program, in order, and
/// returns where each result arrives. The first call makes the pool (and
/// the checker pool, which binds the program) with one thread per job, up
/// to `emit_thread_count`. A job must not use a checker. Loading thread
/// only.
pub fn send_emit_pool_jobs<R: Send + 'static>(
    jobs: Vec<impl FnOnce() -> R + Send + 'static>,
) -> Vec<EmitPoolJob<R>> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let id = prog().id;
    POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        let pool = pools.entry(id).or_insert_with(create_checkers);
        let emit = pool
            .emit
            .get_or_insert_with(|| create_emit_pool(emit_thread_count().clamp(1, jobs.len())));
        emit.jobs += jobs.len();
        jobs.into_iter()
            .map(|f| {
                let (sender, receiver) = std::sync::mpsc::channel();
                let job: Job = Box::new(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
                    let _ = sender.send(result);
                });
                emit.queue.send(job).expect("emit thread stopped");
                EmitPoolJob(receiver)
            })
            .collect()
    })
}

/// The number of jobs sent to the emit pool of the current program so far.
/// Tests use it to see that the pool ran.
pub fn emit_pool_job_count() -> usize {
    let id = prog().id;
    POOLS.with(|pools| {
        pools
            .borrow()
            .get(&id)
            .and_then(|pool| pool.emit.as_ref())
            .map_or(0, |emit| emit.jobs)
    })
}

/// Work for one checker worker. It runs on the worker thread, where
/// `with_checker_at` reaches that worker's checker.
type Job = Box<dyn FnOnce() + Send>;

/// The result of a job, or the payload of its panic.
type JobResult<R> = std::thread::Result<R>;

/// Program-level state that `GoProgram` does not hold. One per program
/// version, in `GoProgram::state`; read it with `state()`. It is leaked with
/// its `GoProgram`, so it holds only small values: its strings are interned
/// (`Name`), one copy per distinct value. The per-version tables are in
/// `VersionTables` (`with_tables`), which a release frees.
pub(crate) struct ProgramState {
    cwd: &'static str,
    use_case_sensitive_file_names: bool,
    /// Go `Program.CommonSourceDirectory`. The Go frontend sets it when it
    /// builds the program; an alias resolver program has none.
    common_source_directory: Option<&'static str>,
    /// True for the program of an autoimport alias resolver
    /// (`new_alias_resolver_program`). Its resolver is in `ALIAS_RESOLVERS`.
    alias_resolver: bool,
    tables: TablesSlot,
}

/// The per-version program tables. Read the ones of the current program
/// with `with_tables`.
// PORT: Go frees them with the program. `GoProgram` and `ProgramState`
// stay leaked (checker code holds `&'static` borrows of them), so the
// tables are behind an `Arc` that `release_program` drops.
// PERF: a version that replaces files of the version it was updated from
// in place (Go `ReuseProgram`) starts from that version's tables and
// changes only the entries of the replaced files (`build_program` in
// `go_frontend`). So the tables that hold paths are by slot, and the slot
// of a replaced file does not change (editfast1).
pub(crate) struct VersionTables {
    /// File ids in Go `Program.SourceFiles()` order
    /// (`GoProgram::source_file_order`). The slot of a program file is its
    /// position here.
    source_file_order: Vec<usize>,
    /// The ids of the files that `file_by_path` names and that are not
    /// program files: slot `source_file_order.len() + i` is file
    /// `other_files[i]`.
    other_files: Vec<usize>,
    /// The slot of the file at each path (`file_at_path`). Versions with
    /// the same paths in the same slots share it.
    file_by_path: Arc<FxHashMap<String, usize>>,
    /// The Go `SourceFile` fields that the program sets, for each program
    /// file, by slot. Versions share it while it is equal.
    file_meta: Arc<Vec<FileProgramMeta>>,
    /// Checker index for each file index (Go `fileAssociations`). Set when
    /// the checker pool is made.
    file_associations: OnceLock<Vec<usize>>,
    /// Go `Program.declarationDiagnosticCache`.
    declaration_diagnostic_cache: Mutex<FxHashMap<Node, Vec<Diagnostic>>>,
    /// The thread-safe copy of the Go frontend data that the checker reads.
    /// None for an alias resolver program. The frontend program itself is
    /// in `FRONTENDS`, on the loading thread only.
    go: Option<go_frontend::GoSharedState>,
    /// The freeable file versions of the program files (lsshells M3a), in
    /// no order. A thread that holds the tables keeps them alive, so a
    /// worker thread can read its program files after the frontend program
    /// is freed.
    file_versions: Vec<Arc<crate::ast::FileVersion>>,
    /// The binder symbols of the program (`bind_all`, `bound_symbols`): a
    /// copy of the binder lineage after the program files are bound. Each
    /// checker copies it. It shares the lineage chunks, so it keeps the
    /// chunks of a dead file version until the release frees it (lsshells
    /// M2c, M3d).
    bound_symbols: OnceLock<SymbolArena>,
}

impl VersionTables {
    /// Tables with only the file order and the files by path set.
    /// `file_by_path` gives the file id at each path; a later entry for a
    /// path replaces an earlier one.
    fn new(
        source_file_order: Vec<usize>,
        file_by_path: impl IntoIterator<Item = (String, usize)>,
    ) -> Self {
        let mut slots = SlotsBuilder::new(&source_file_order);
        let file_by_path = file_by_path
            .into_iter()
            .map(|(path, file)| (path, slots.slot_of(file)))
            .collect();
        let other_files = slots.other_files;
        Self::from_parts(
            source_file_order,
            other_files,
            Arc::new(file_by_path),
            Arc::default(),
        )
    }

    /// Tables with the file order, the files by path and the program-set
    /// fields set (see the fields).
    fn from_parts(
        source_file_order: Vec<usize>,
        other_files: Vec<usize>,
        file_by_path: Arc<FxHashMap<String, usize>>,
        file_meta: Arc<Vec<FileProgramMeta>>,
    ) -> Self {
        VersionTables {
            source_file_order,
            other_files,
            file_by_path,
            file_meta,
            file_associations: OnceLock::new(),
            declaration_diagnostic_cache: Mutex::new(FxHashMap::default()),
            go: None,
            file_versions: Vec::new(),
            bound_symbols: OnceLock::new(),
        }
    }

    /// The id of the file in `slot` of `file_by_path`.
    fn file_at_slot(&self, slot: usize) -> usize {
        match self.source_file_order.get(slot) {
            Some(&file) => file,
            None => self.other_files[slot - self.source_file_order.len()],
        }
    }

    /// The id of the file at `path` (Go `filesByPath`), or None.
    fn file_at_path(&self, path: &str) -> Option<usize> {
        self.file_by_path
            .get(path)
            .map(|&slot| self.file_at_slot(slot))
    }

    /// The program-set fields of the file at `path`, or None when `path` is
    /// not a program file.
    fn file_meta_by_path(&self, path: &str) -> Option<&FileProgramMeta> {
        self.file_by_path
            .get(path)
            .and_then(|&slot| self.file_meta.get(slot))
    }
}

/// Gives the files of `VersionTables::file_by_path` their slots: a program
/// file its position in the file order, and any other file the next slot
/// after the program files.
struct SlotsBuilder<'a> {
    source_file_order: &'a [usize],
    /// The slot of each file id, made on first use.
    slots: Option<FxHashMap<usize, usize>>,
    /// `VersionTables::other_files`.
    other_files: Vec<usize>,
}

impl<'a> SlotsBuilder<'a> {
    fn new(source_file_order: &'a [usize]) -> Self {
        SlotsBuilder {
            source_file_order,
            slots: None,
            other_files: Vec::new(),
        }
    }

    /// The slot of file `file`.
    fn slot_of(&mut self, file: usize) -> usize {
        let order = self.source_file_order;
        let slots = self.slots.get_or_insert_with(|| {
            order
                .iter()
                .enumerate()
                .map(|(slot, &file)| (file, slot))
                .collect()
        });
        *slots.entry(file).or_insert_with(|| {
            self.other_files.push(file);
            order.len() + self.other_files.len() - 1
        })
    }
}

/// Where a program keeps its `VersionTables`.
enum TablesSlot {
    /// The program of a one-program process. It is never released, so its
    /// tables are leaked and read with no lock.
    Leaked(&'static VersionTables),
    /// A program version of a multi-program process. `release_program`
    /// takes the tables; they are freed when no thread holds a copy.
    Version(Mutex<Option<Arc<VersionTables>>>),
}

impl TablesSlot {
    /// `Leaked` for the program of a one-program process, else `Version`.
    fn new(tables: VersionTables, one_program: bool) -> Self {
        if one_program {
            TablesSlot::Leaked(Box::leak(Box::new(tables)))
        } else {
            TablesSlot::Version(Mutex::new(Some(Arc::new(tables))))
        }
    }
}

/// True when `GOPORT_KEEP_VERSION_TABLES=1`: a released program version
/// keeps its tables, as before lsshells M2a (for A/B runs, and as a field
/// fallback). Read once.
fn keep_version_tables() -> bool {
    static KEEP: OnceLock<bool> = OnceLock::new();
    *KEEP.get_or_init(|| std::env::var("GOPORT_KEEP_VERSION_TABLES").is_ok_and(|v| v == "1"))
}

thread_local! {
    /// This thread's copy of the tables of one program version: the one it
    /// read last, or the one from its `WorkerSeed`. A thread that holds the
    /// tables of a released version keeps them alive until it lets go.
    static TABLES: RefCell<Option<(u32, Arc<VersionTables>)>> = const { RefCell::new(None) };
}

/// Runs `f` with the tables of the current program (`prog()`). Panics when
/// the program is released and this thread holds no copy of its tables:
/// a stale read panics and never reads other data.
// PERF: a hit is a thread-local borrow and an id compare. `f` must not
// enter another program: a read of another program inside `f` locks that
// program's slot, because the cache is borrowed.
fn with_tables<R>(f: impl FnOnce(&VersionTables) -> R) -> R {
    let program = prog();
    let slot = match &state_of(program).tables {
        TablesSlot::Leaked(tables) => return f(tables),
        TablesSlot::Version(slot) => slot,
    };
    TABLES.with(|cache| {
        if let Ok(cached) = cache.try_borrow()
            && let Some((id, tables)) = &*cached
            && *id == program.id
        {
            return f(tables);
        }
        let tables = slot_tables(program.id, slot);
        if let Ok(mut cached) = cache.try_borrow_mut() {
            *cached = Some((program.id, Arc::clone(&tables)));
        }
        f(&tables)
    })
}

/// The tables in `slot` of program version `id`. Panics when the version
/// is released.
fn slot_tables(id: u32, slot: &Mutex<Option<Arc<VersionTables>>>) -> Arc<VersionTables> {
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| panic!("program version {id} is released"))
}

/// A copy of the tables of the current program, for a new thread
/// (`WorkerSeed`). None for the program of a one-program process.
fn current_tables() -> Option<(u32, Arc<VersionTables>)> {
    let program = prog();
    match held_tables(program) {
        HeldTables::Leaked(_) => None,
        HeldTables::Version(tables) => Some((program.id, tables)),
    }
}

/// The tables of one program version, held: the leaked tables of a
/// one-program process, or a copy of the `Arc` of a program version.
enum HeldTables {
    Leaked(&'static VersionTables),
    Version(Arc<VersionTables>),
}

impl Deref for HeldTables {
    type Target = VersionTables;

    fn deref(&self) -> &VersionTables {
        match self {
            HeldTables::Leaked(tables) => tables,
            HeldTables::Version(tables) => tables,
        }
    }
}

/// The tables of `program`: this thread's copy when it holds one, else the
/// ones in its slot. Panics when the program is released and this thread
/// holds no copy, as `with_tables`.
fn held_tables(program: &'static GoProgram) -> HeldTables {
    let slot = match &state_of(program).tables {
        TablesSlot::Leaked(tables) => return HeldTables::Leaked(tables),
        TablesSlot::Version(slot) => slot,
    };
    let cached = TABLES.with(|cache| {
        cache
            .try_borrow()
            .ok()?
            .as_ref()
            .filter(|(id, _)| *id == program.id)
            .map(|(_, tables)| Arc::clone(tables))
    });
    HeldTables::Version(cached.unwrap_or_else(|| slot_tables(program.id, slot)))
}

impl GoProgram {
    /// Go `Program.SourceFiles()`: the program files in Go order. Each guard
    /// pins a freeable file version while it lives (see `ast::go_file`).
    /// The iterator holds the program tables (`source_file_order`).
    pub fn source_files(&'static self) -> impl Iterator<Item = crate::ast::FileRef<GoFile>> {
        let order = self.source_file_order();
        (0..order.len()).map(move |i| crate::ast::go_file(order[i]))
    }

    /// The file ids of the program in Go `Program.SourceFiles()` order. They
    /// are in the program tables, which a release frees (lsshells M2c), so
    /// this panics when the program is released and this thread holds no
    /// copy of its tables, as `with_tables`.
    #[must_use]
    pub fn source_file_order(&'static self) -> SourceFileOrder {
        SourceFileOrder(held_tables(self))
    }
}

/// The file ids of a program in Go order (`GoProgram::source_file_order`).
/// The guard holds the program tables while it lives.
pub struct SourceFileOrder(HeldTables);

impl Deref for SourceFileOrder {
    type Target = [usize];

    #[inline]
    fn deref(&self) -> &[usize] {
        &self.0.source_file_order
    }
}

/// The leaked copy of `options` that `GoProgram::options` keeps: one per
/// distinct value in the process, so the program versions of a language
/// server session share one copy. Two values are the same only when
/// `deep_equal` says so, so a copy keeps the `paths` order of its program.
// PERF: a linear search, once per program. A process has one options value
// per project config.
pub(crate) fn intern_compiler_options(options: &CompilerOptions) -> &'static CompilerOptions {
    static INTERNED: Mutex<Vec<&'static CompilerOptions>> = Mutex::new(Vec::new());
    let mut interned = INTERNED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(&found) = interned.iter().find(|found| found.deep_equal(options)) {
        return found;
    }
    let leaked: &'static CompilerOptions = Box::leak(Box::new(options.clone()));
    interned.push(leaked);
    leaked
}

/// `text` interned (`Name`): a program string that is leaked with the
/// program shell, once per distinct value.
fn intern_program_str(text: &str) -> &'static str {
    crate::core::Name::from(text).as_str()
}

/// The binder symbols of a program version (Go: the symbols that the bound
/// files of the program point to): a copy of the binder lineage after the
/// program files are bound (`bind_all`). It derefs to the arena.
// PORT: Go symbols live with their files. Here they are in the program's
// `VersionTables` (lsshells M2c), so a release frees the copy with the
// tables, and the lineage chunks of a dead file version go when the last
// copy lets go (M3d). The guard holds the tables while it lives: keep it
// only as long as the symbols are read.
pub struct BoundSymbols(HeldTables);

impl Deref for BoundSymbols {
    type Target = SymbolArena;

    #[inline]
    fn deref(&self) -> &SymbolArena {
        self.0.bound_symbols.get().expect("program is not bound")
    }
}

/// The binder symbols of the current program (`prog()`). Panics when it is
/// not bound (`bind_all`) or released.
#[must_use]
pub fn bound_symbols() -> BoundSymbols {
    bound_symbols_of(prog()).expect("program is not bound")
}

/// The binder symbols of `program`, or None when it is not bound
/// (`bind_all`). Panics when it is released and this thread holds no copy
/// of its tables.
#[must_use]
pub fn bound_symbols_of(program: &'static GoProgram) -> Option<BoundSymbols> {
    let tables = held_tables(program);
    tables.bound_symbols.get()?;
    Some(BoundSymbols(tables))
}

/// Frees the tables of program `id` when no other thread holds them: takes
/// them from the slot and from this thread's copy. With
/// `keep_version_tables` they stay, and only the declaration diagnostic
/// cache is emptied, as before.
/// Takes the tables of program `id` out of its slot and out of this
/// thread's cache. The caller frees them when it drops the result (other
/// threads can still hold them).
fn release_tables(id: u32, state: &ProgramState) -> Option<Arc<VersionTables>> {
    let slot = match &state.tables {
        TablesSlot::Version(slot) if !keep_version_tables() => slot,
        TablesSlot::Version(slot) => {
            let tables = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(tables) = tables {
                clear_declaration_diagnostic_cache(&tables);
            }
            return None;
        }
        TablesSlot::Leaked(tables) => {
            clear_declaration_diagnostic_cache(tables);
            return None;
        }
    };
    let taken = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    let cached = TABLES.with(|cache| {
        let mut cache = cache.try_borrow_mut().ok()?;
        cache.as_ref().filter(|(cached, _)| *cached == id)?;
        cache.take()
    });
    // Dropped after the lock and the borrow end.
    drop(cached);
    taken
}

fn clear_declaration_diagnostic_cache(tables: &VersionTables) {
    drop(std::mem::take(
        &mut *tables
            .declaration_diagnostic_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    ));
}

/// A weak handle to the tables of a program version. Tests use it to see
/// that a release frees them.
pub struct VersionTablesProbe(std::sync::Weak<VersionTables>);

impl VersionTablesProbe {
    /// True once no thread holds the tables.
    #[must_use]
    pub fn is_freed(&self) -> bool {
        self.0.strong_count() == 0
    }
}

/// A probe of the tables of `program`. None for the program of a
/// one-program process, or when `program` is released.
#[must_use]
pub fn version_tables_probe(program: &'static GoProgram) -> Option<VersionTablesProbe> {
    let TablesSlot::Version(slot) = &state_of(program).tables else {
        return None;
    };
    let tables = slot.lock().unwrap_or_else(PoisonError::into_inner);
    Some(VersionTablesProbe(Arc::downgrade(tables.as_ref()?)))
}

/// Go `SourceFile.IsDefaultLibrary` (read through the program) and
/// `SourceFile.Metadata` of one program file.
#[derive(Clone, Debug, PartialEq)]
struct FileProgramMeta {
    meta_data: SourceFileMetaData,
    is_default_library: bool,
}

thread_local! {
    /// The Go frontend program of each program version, by `GoProgram::id`.
    /// Only the thread that loaded a program has it: the frontend data is
    /// not thread-safe. The checker reads `GoSharedState`.
    static FRONTENDS: RefCell<FxHashMap<u32, Rc<crate::frontend::compiler::NewProgram>>> =
        RefCell::new(FxHashMap::default());
}

/// The state of the current program (`prog()`).
fn state() -> &'static ProgramState {
    state_of(prog())
}

/// The state of `program`.
fn state_of(program: &'static GoProgram) -> &'static ProgramState {
    program.state.get().expect("program not loaded")
}

/// The Go frontend program, or None for an alias resolver program. Panics
/// on a checker worker thread, which must use the copies in
/// `VersionTables::go`.
fn go_frontend() -> Option<Rc<crate::frontend::compiler::NewProgram>> {
    let id = prog().id;
    let frontend = FRONTENDS.with(|frontends| frontends.borrow().get(&id).cloned());
    if frontend.is_none() && with_tables(|tables| tables.go.is_some()) {
        panic!("the Go frontend program is read on the loading thread only");
    }
    frontend
}

/// Runs `f` with the Go frontend data of the current program
/// (`VersionTables::go`). Panics for an alias resolver program, which has
/// none.
fn with_go<R>(f: impl FnOnce(&go_frontend::GoSharedState) -> R) -> R {
    with_tables(|tables| {
        f(tables
            .go
            .as_ref()
            .expect("Go frontend data of an alias resolver program"))
    })
}

// ---------------------------------------------------------------------------
// Alias resolver programs (Go ls/autoimport/aliasresolver.go)
// ---------------------------------------------------------------------------

/// The Go `checker.Program` methods of the autoimport alias resolver
/// (`ls/autoimport/aliasresolver.go`) that read the files and module
/// resolutions it adds while its checker runs. Go gives the other methods a
/// constant or panics, and the `program.rs` functions do the same for an
/// alias resolver program.
pub trait AliasResolverProgram {
    /// Go `GetSourceFile`.
    fn source_file(&self, file_name: &str) -> Node;
    /// Go `GetSourceFileForResolvedModule`.
    fn source_file_for_resolved_module(&self, file_name: &str) -> Node;
    /// Go `GetResolvedModule`.
    fn resolved_module(
        &self,
        file: Node,
        module_reference: &str,
        mode: ResolutionMode,
    ) -> Arc<ResolvedModule>;
}

thread_local! {
    /// The resolver of each alias resolver program of this thread, by
    /// `GoProgram::id`, while its `AliasResolverProgramScope` lives.
    static ALIAS_RESOLVERS: RefCell<FxHashMap<u32, Rc<dyn AliasResolverProgram>>> =
        RefCell::new(FxHashMap::default());
}

/// The resolver of the current program when it is an alias resolver program.
fn alias_resolver() -> Option<Rc<dyn AliasResolverProgram>> {
    if !state().alias_resolver {
        return None;
    }
    let id = prog().id;
    let resolver = ALIAS_RESOLVERS.with(|resolvers| resolvers.borrow().get(&id).cloned());
    Some(resolver.expect("an alias resolver program is read on its thread while its scope lives"))
}

/// Go `panic("unimplemented")`: the alias resolver's `checker.Program`
/// methods that Go does not implement (aliasresolver.go:141-230).
#[track_caller]
fn alias_resolver_unimplemented() {
    if state().alias_resolver {
        go_panic("unimplemented".to_string());
    }
}

/// From `new_alias_resolver_program`. The program is current on this thread
/// while the scope lives (an `ls_program::ProgramGuard`). On drop the
/// program forgets its resolver; do not use its checker after that.
pub struct AliasResolverProgramScope {
    program: &'static GoProgram,
    _guard: ls_program::ProgramGuard,
}

impl AliasResolverProgramScope {
    /// The alias resolver program.
    #[must_use]
    pub fn program(&self) -> &'static GoProgram {
        self.program
    }
}

impl Drop for AliasResolverProgramScope {
    fn drop(&mut self) {
        let id = self.program.id;
        let resolver = ALIAS_RESOLVERS.with(|resolvers| resolvers.borrow_mut().remove(&id));
        drop(resolver);
        release_tables(id, state_of(self.program));
    }
}

/// Go `checker.NewChecker(aliasResolver, nil)` (ls/autoimport): makes the
/// program that the checker reads and makes it current until the scope
/// drops. `root_files` are Go `aliasResolver.SourceFiles()`. `files` are
/// every file that the checker can read (the root files too); they must be
/// published, and they are bound here if they are not yet. `options` are Go
/// `aliasResolver.Options()`, `current_directory` and
/// `use_case_sensitive_file_names` come from the resolver's host, and
/// `resolver` answers the lazy methods (`AliasResolverProgram`).
// PORT: Go needs no program: the resolver is the `checker.Program`. A
// checker here reads its program version (`prog()`), and it copies the
// binder lineage when it is made (`SymbolArena::for_checker`). So the
// program copies the lineage after `files` are bound, and a file bound
// later is not in its checkers' arenas. Go `GetResolvedModules` is nil, so
// the program has no resolved modules. The program shell stays leaked like
// other program versions (multi-program M2, M3); the scope frees its
// tables, with its copy of the lineage.
pub fn new_alias_resolver_program(
    options: CompilerOptions,
    root_files: &[Node],
    files: &[Node],
    current_directory: &str,
    use_case_sensitive_file_names: bool,
    resolver: Rc<dyn AliasResolverProgram>,
) -> AliasResolverProgramScope {
    let file_by_path = files
        .iter()
        .map(|&file| (source_file_info(file).path.clone(), file.file_index()));
    let source_file_order = root_files.iter().map(|file| file.file_index()).collect();
    let program: &'static GoProgram = Box::leak(Box::new(GoProgram {
        id: next_program_id(),
        options: intern_compiler_options(&options),
        state: OnceLock::new(),
    }));
    let program_state = ProgramState {
        cwd: intern_program_str(current_directory),
        use_case_sensitive_file_names,
        common_source_directory: None,
        alias_resolver: true,
        // The files stay alive while the alias resolver program reads them.
        tables: TablesSlot::new(
            VersionTables {
                file_versions: crate::ast::live_file_versions(
                    files.iter().map(|file| file.file_index()),
                ),
                ..VersionTables::new(source_file_order, file_by_path)
            },
            false,
        ),
    };
    assert!(program.state.set(program_state).is_ok());
    register_program_version(program);
    let bound_symbols = with_lineage(|lineage| {
        for &file in files {
            lineage.bind(file);
        }
        lineage.symbols.clone()
    });
    assert!(
        held_tables(program)
            .bound_symbols
            .set(bound_symbols)
            .is_ok()
    );
    ALIAS_RESOLVERS.with(|resolvers| resolvers.borrow_mut().insert(program.id, resolver));
    AliasResolverProgramScope {
        program,
        _guard: ls_program::enter_version(program),
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Loads the config graph at `config_path`, builds the Go files and
/// installs the program for the process. Panics on a load error.
pub fn load(config_path: &str) -> &'static GoProgram {
    match try_load(config_path) {
        Ok(program) => program,
        Err(message) => panic!("cannot load program {config_path}: {message}"),
    }
}

/// `load` with the error returned. The program is leaked: it lives for the
/// rest of the process, like the Go program in `tsc`.
pub fn try_load(config_path: &str) -> Result<&'static GoProgram, String> {
    try_load_with(config_path, |_| {})
}

/// `try_load` with a hook that edits the compiler options before the files
/// are built. Use it for command line overrides, for example `--noEmit`.
pub fn try_load_with(
    config_path: &str,
    edit_options: impl FnOnce(&mut CompilerOptions),
) -> Result<&'static GoProgram, String> {
    try_load_timed(config_path, edit_options, &mut CompileTimes::default())
}

/// `try_load_with` that also records the Go `CompileTimes.ConfigTime` and
/// `ParseTime` (execute/tsc.go:214 and :306) in `times`.
pub fn try_load_timed(
    config_path: &str,
    edit_options: impl FnOnce(&mut CompilerOptions),
    times: &mut CompileTimes,
) -> Result<&'static GoProgram, String> {
    go_frontend::try_load_with(config_path, edit_options, times)
}

/// The binder lineage: the symbol arena of every file version bound so
/// far, in any program version. Ids only grow and are never used again, so
/// the symbol ids of a file version stay valid in every program that shares
/// the file (Go `SourceFile.BindOnce`).
///
/// lsshells M3d: a freeable file version (`ast::FileVersion`) binds into
/// whole chunks of its own (`add_file`), and the lineage keeps their range.
/// After the version dies, the next use of the lineage frees those chunks
/// (`free_dead`): its ids become holes, and a read of one panics. Each
/// thread then frees its `get_symbol_id` ids of those chunks
/// (`ast::free_lineage_symbol_ids`). A static file keeps its symbols until
/// exit. The program copies (`VersionTables::bound_symbols`) and checker
/// arenas that share a freed chunk keep it until they drop.
struct Lineage {
    symbols: SymbolArena,
    /// The arena range of each freeable file version bound here, by file
    /// id, until the version dies, with its symbol indexes.
    freeable: FxHashMap<usize, (ArenaMark, ArenaMark, std::ops::Range<usize>)>,
    /// `ast::dead_file_versions` when `free_dead` last looked.
    seen_dead: usize,
}

static LINEAGE: Mutex<Option<Lineage>> = Mutex::new(None);

/// Runs `f` with the binder lineage locked, after it frees the chunks of
/// the file versions that died since its last use.
// PORT: a dying `FileVersion` does not lock the lineage: it can drop on a
// thread that holds the lock (the last pin of a dead version can drop in a
// bind). So the lineage frees its chunks here, at the next bind, and they
// stay one edit longer.
fn with_lineage<R>(f: impl FnOnce(&mut Lineage) -> R) -> R {
    let mut lineage = LINEAGE.lock().unwrap_or_else(PoisonError::into_inner);
    let lineage = lineage.get_or_insert_with(|| Lineage {
        symbols: SymbolArena::new(),
        freeable: FxHashMap::default(),
        seen_dead: 0,
    });
    lineage.free_dead();
    f(lineage)
}

impl Lineage {
    /// Frees the chunks of the freeable versions that died since the last
    /// call.
    // PERF: one atomic load when no version died.
    fn free_dead(&mut self) {
        if crate::ast::dead_file_versions() == self.seen_dead {
            return;
        }
        let (dead, seen) = crate::ast::dead_files_since(self.seen_dead);
        self.seen_dead = seen;
        for file in dead {
            if let Some((start, end, symbols)) = self.freeable.remove(&file) {
                self.symbols.free_range(start, end);
                crate::ast::free_lineage_symbol_ids(file, symbols);
            }
        }
    }

    /// Runs `add`, which adds the symbols and tables of file `file` to the
    /// lineage. A freeable version (`freeable`) starts and ends on chunk
    /// starts (`SymbolArena::end_chunk`), so its chunks hold no other
    /// symbols, and its range is kept for `free_dead`. Its ids and the ids
    /// of the files after it move to the chunk start; a static file binds
    /// as before.
    fn add_file<R>(
        &mut self,
        file: usize,
        freeable: bool,
        add: impl FnOnce(&mut SymbolArena) -> R,
    ) -> R {
        // Checkers copy the lineage; publish what the file added, so they
        // share it (`SymbolArena::freeze_since` and `share_since`).
        if !freeable {
            let mark = self.symbols.mark();
            let result = add(&mut self.symbols);
            self.symbols.freeze_since(mark);
            return result;
        }
        self.symbols.end_chunk();
        let start = self.symbols.mark();
        let first = self.symbols.symbol_count();
        let result = add(&mut self.symbols);
        self.symbols.end_chunk();
        self.symbols.share_since(start);
        let symbols = first..self.symbols.symbol_count();
        self.freeable
            .insert(file, (start, self.symbols.mark(), symbols));
        result
    }

    /// Go `binder.BindSourceFile` into the lineage (`bind_source_file`),
    /// with the chunks of a freeable version kept apart (`add_file`). A
    /// file that is bound already is skipped.
    fn bind(&mut self, file: Node) {
        let freeable = {
            let go_file = crate::ast::go_file(file.file_index());
            if go_file.file_bind.get().is_some() {
                return;
            }
            go_file.version().is_some()
        };
        self.add_file(file.file_index(), freeable, |symbols| {
            bind_source_file(file, symbols);
        });
    }
}

/// The lineage chunks that hold symbols or tables, after the chunks of the
/// dead file versions are freed. Tests use it (lsshells M3d).
#[must_use]
pub fn lineage_live_chunks() -> usize {
    with_lineage(|lineage| lineage.symbols.live_chunk_count())
}

// Go: compiler/program.go:445 BindSourceFiles
// PORT: Go binds files in parallel into per-file symbol tables. Here every
// file binds into the shared `LINEAGE` arena, and the program's binder
// symbols (`bound_symbols`, in its `VersionTables`) are a copy of it after
// the program files are bound. `Checker::new` uses the same initializer,
// so the first of the two to run binds. Files bind in parallel, each into
// its own arena (`bind_files_parallel`), and join the lineage in file order
// with the ids a serial bind gives. With `--singleThreaded` they bind
// last-queued-first (`bind_files_last_queued_first`). A file that an
// earlier program version bound is not bound again.
pub fn bind_all() {
    let program = prog();
    held_tables(program).bound_symbols.get_or_init(|| {
        with_lineage(|lineage| {
            if single_threaded() {
                bind_files_last_queued_first(lineage);
            } else {
                bind_files_parallel(lineage);
            }
            for file in program.source_files() {
                // Go: program.go:450 traces the files that are not bound yet.
                let _trace = if file.file_bind.get().is_none() {
                    trace_bind_source_file(file.root)
                } else {
                    None
                };
                lineage.bind(file.root);
            }
            // Checkers clone the copy. Each file published what it added
            // (`Lineage::add_file`).
            lineage.symbols.clone()
        })
    });
}

/// Go program.go:445 with `--singleThreaded`: `core.singleThreadedWorkGroup`
/// runs the queued binds last-queued-first (core/workgroup.go:67), so the
/// files that are not bound yet bind, and trace, in reverse file order. Each
/// file binds into its own arena, and the arenas join the lineage arena in
/// file order, so the ids are those of a serial bind in file order (see
/// `bind_files_parallel`). This thread keeps the state that binding makes.
fn bind_files_last_queued_first(lineage: &mut Lineage) {
    let queued: Vec<(Node, bool)> = prog()
        .source_files()
        .filter(|file| file.file_bind.get().is_none())
        .map(|file| (file.root, file.version().is_some()))
        .collect();
    let mut bound: Vec<(BoundFile, SymbolArena, bool)> = queued
        .into_iter()
        .rev()
        .map(|(file, freeable)| {
            let _trace = trace_bind_source_file(file);
            let mut file_symbols = SymbolArena::new();
            let bound = bind_source_file_detached(file, &mut file_symbols);
            (bound, file_symbols, freeable)
        })
        .collect();
    bound.reverse();
    for (mut file, file_symbols, freeable) in bound {
        let offsets = lineage.add_file(file.file.file_index(), freeable, |symbols| {
            symbols.append_file_arena(file_symbols, freeable)
        });
        file.remap(offsets);
        file.install();
    }
}

/// Go program.go:450: the "bindSourceFile" event of one file, when tracing.
/// PORT: a file whose parallel bind is dropped (see `bind_files_parallel`)
/// is bound again serially and gets a second event.
fn trace_bind_source_file(file: Node) -> Option<crate::tracing::Pop> {
    crate::tracing::get().map(|tr| {
        tr.push(
            crate::tracing::Phase::Bind,
            "bindSourceFile",
            vec![("path", source_file_info(file).path.clone().into())],
            true,
        )
    })
}

/// The state of this thread that binding must not change: synthetic nodes,
/// ids and lazy JSDoc. A bind thread starts from a copy of the loading
/// thread's state, so anything it adds would be lost. The early emit
/// (`execute::incremental::Program::start_emit`) also reads it:
/// an emit pool starts from a copy of the same state.
pub(crate) fn bind_thread_fingerprint() -> (usize, (u64, u64), usize) {
    (
        synthetic_slot_count(),
        next_ids(),
        go_frontend::lazy_jsdoc_count(),
    )
}

/// Number of bind threads for `files` files to bind:
/// `ThreadBudget::bind_threads` of the last program load on this thread
/// (`note_program_load`). On `WIDE_CORES` physical cores (`wide_cores`), a
/// load with at least `WIDE_BIND_FILES` files to bind binds as a large load.
/// `GOPORT_BIND_THREADS` sets it (below 2 binds serially).
// PERF (perf11 W5, tscb `ab32`, dbook 32 threads, 20 paired rounds): the
// first program of the query chain build (`tsc -b`, 445 files, 27 root
// tasks) bound in 5.6 ms less wall time on 16 bind threads than on 4.
fn bind_thread_count(files: usize) -> usize {
    if let Some(count) = std::env::var("GOPORT_BIND_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return count;
    }
    let large = LARGE_LOAD.get() || files >= WIDE_BIND_FILES && wide_cores();
    ThreadBudget::current().bind_threads(large)
}

/// From this many files to bind on, a load binds as a large load on
/// `WIDE_CORES` physical cores (`bind_thread_count`). Query (`-p`, 186
/// files) stays below it: there 8 bind threads were faster than 16.
const WIDE_BIND_FILES: usize = 300;

/// A program load with at least this many root tasks (root files, `lib`
/// entries and the automatic type directive task) is large.
// PERF (perf9 round 3, effect R3-E1): root tasks are query 27, hono 190,
// elysia 241, zod 324 and effect 459. At 16 threads, 7 parse workers
// parsed effect 12 ms and zod 9 ms faster than 4, and hono in the same
// time. With glibc malloc, query gained nothing from more parse threads
// (lib.dom bounds its parse), and its peak RSS was near the 1.15x rule.
// With jemalloc, a load that is not large parses on 8 threads too
// (`ThreadBudget::one_program`).
const LARGE_LOAD_ROOT_TASKS: usize = 128;

thread_local! {
    /// Whether the last program load on this thread was large
    /// (`note_program_load`). The bind of that program reads it.
    static LARGE_LOAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Records whether a program load on this thread with `root_tasks` root
/// tasks is large, and returns it. A large load gets more parse threads
/// (`ThreadBudget::parse_threads`) and bind threads
/// (`ThreadBudget::bind_threads`). Call it when the load starts.
pub(crate) fn note_program_load(root_tasks: usize) -> bool {
    let large = root_tasks >= LARGE_LOAD_ROOT_TASKS;
    LARGE_LOAD.set(large);
    large
}

/// The cores that this process may run on: Go `GOMAXPROCS`
/// (`gostd::runtime::gomaxprocs`, read once), so the `GOMAXPROCS` variable
/// and a cgroup CPU limit size the pools as they size Go's.
pub fn available_cores() -> usize {
    crate::gostd::runtime::gomaxprocs()
}

/// From this many physical cores on (`wide_cores`), a large program load
/// of a one-program process parses and binds on this many threads
/// (`ThreadBudget::one_program`).
const WIDE_CORES: usize = 16;

/// True when this process may run on at least `WIDE_CORES` CPUs and
/// `WIDE_CORES` physical cores. The physical cores are read only when
/// `available_cores` is at least `WIDE_CORES`.
fn wide_cores() -> bool {
    available_cores() >= WIDE_CORES
        && physical_core_count().is_some_and(|cores| cores >= WIDE_CORES)
}

/// `physical_cores`, read once: it reads a sysfs file per core.
fn physical_core_count() -> Option<usize> {
    static PHYSICAL: OnceLock<Option<usize>> = OnceLock::new();
    *PHYSICAL.get_or_init(physical_cores)
}

/// The physical cores that this process may run on: the CPUs of
/// `Cpus_allowed_list` in `/proc/self/status`, where the SMT siblings of one
/// core (`topology/thread_siblings_list` in sysfs) count once. None when a
/// file cannot be read or does not parse (not Linux, no sysfs).
fn physical_cores() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?;
    let mut covered: Vec<usize> = Vec::new();
    let mut cores = 0;
    for cpu in parse_cpu_list(allowed.trim())? {
        if covered.contains(&cpu) {
            continue;
        }
        let siblings = std::fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        ))
        .ok()?;
        covered.extend(parse_cpu_list(siblings.trim())?);
        covered.push(cpu);
        cores += 1;
    }
    Some(cores)
}

/// The CPUs of a Linux CPU list such as `0-3,8,10-11`. None when it does
/// not parse or names a CPU at or above 65,536.
fn parse_cpu_list(list: &str) -> Option<Vec<usize>> {
    const MAX_CPU: usize = 1 << 16;
    let mut cpus = Vec::new();
    for part in list.split(',') {
        let (first, last): (usize, usize) = match part.split_once('-') {
            Some((first, last)) => (first.parse().ok()?, last.parse().ok()?),
            None => {
                let cpu = part.parse().ok()?;
                (cpu, cpu)
            }
        };
        if first > last || last >= MAX_CPU {
            return None;
        }
        cpus.extend(first..=last);
    }
    Some(cpus)
}

/// The most parse threads (the loading thread included) and bind threads
/// of a program load, and the glibc malloc arenas of the process, in one
/// budget. A large program load (`note_program_load`) has its own limits.
///
/// glibc gives each thread that mallocs its own arena until `arena_max`
/// arenas exist. A new thread first takes the arena of a thread that
/// ended, if there is one. When `arena_max` arenas exist, a later thread
/// shares the arena of another thread, and when both malloc at once they
/// wait on its lock. Each arena in use also adds peak RSS, because freed
/// per-file data stays in it. The parse mallocs most (a quarter of the
/// parse thread cycles), so the parse threads must fit the arenas: with 7
/// arenas at 16 cores, the 7 parse workers of effect made 3,763 futex waits
/// (19 at 4 cores). The bind threads malloc little (about 5% of their
/// cycles), so they can share arenas: 8 bind threads bind effect in the
/// same time with 7 arenas as with 16. In the check, the checkers and the
/// loading thread malloc. Main, and in `tsgo` the signal and text hash
/// threads, hold an arena too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadBudget {
    /// The most parse threads, the loading thread included.
    pub parse: usize,
    /// The most parse threads of a large program load.
    pub parse_large: usize,
    /// The most bind threads of a large program load.
    pub bind: usize,
    /// The most bind threads of other program loads.
    pub bind_small: usize,
    /// `glibc.malloc.arena_max`.
    pub arena_max: usize,
}

static THREAD_BUDGET: OnceLock<ThreadBudget> = OnceLock::new();

/// Go's default checker count (`checker_count`).
const DEFAULT_CHECKERS: usize = 4;

impl ThreadBudget {
    /// A process that loads several programs at once (`goport_build`: about
    /// 20 threads per program), and the counts of a process that installs
    /// no budget.
    pub const WIDE: ThreadBudget = ThreadBudget {
        parse: 8,
        parse_large: 8,
        bind: 8,
        bind_small: 8,
        arena_max: 16,
    };

    /// The budget of a one-program process (`tsgo`, `goport`) on the cores
    /// of this process. `arena_max` gives one arena to each thread alive
    /// while the checkers run (the checkers, main, the loading thread and
    /// `extra`; `tsgo` has 1 extra, its signal thread), and one to each
    /// parse worker that only a large load adds (the spare arenas).
    /// - With glibc malloc (a build without the `jemalloc` feature), a load
    ///   that is not large parses on as many threads as the check (4
    ///   workers and the loading thread), which fit the check arenas. With
    ///   spare arenas, it binds on as many threads as it had parse workers.
    ///   The bind threads then take the arenas of the parse workers, which
    ///   ended, and make no new ones.
    /// - With jemalloc (the default feature and the shipped build), which
    ///   does not read `arena_max`, a load that is not large parses and
    ///   binds on up to 8 threads (7 parse workers and the loading thread).
    /// - A large load parses on up to 8 threads (with glibc malloc, the 3
    ///   workers more use the spare arenas) and binds on 8. When the
    ///   process may run on `WIDE_CORES` physical cores or more
    ///   (`wide_cores`), a large load parses and binds on `WIDE_CORES`
    ///   threads (15 parse workers and the loading thread). The arenas do
    ///   not change: `tsgo` and `goport` use jemalloc.
    // PERF (perf9 round 2, env-only runs on cup2 and zbook): with 8 parse
    // threads at 8 and 16 cores, the parse threads shared arena locks. With
    // 5, the query parse on cup2 took 21 ms instead of 29 (wall 8% to 12%
    // less) and query peak RSS was 135 MB instead of 136 to 137 (Go 119).
    // 7 parse threads with 10 arenas were faster on effect, but query then
    // needs 143 MB, over the 1.15x RSS rule. 4 bind threads cost zod 13 ms
    // and effect 18 ms of bind.
    // PERF (perf9 round 3, effect R3-E1): so only a large program starts
    // more parse workers (7 workers with 12 arenas at 16 threads: effect
    // parse -12 ms, zod -9 ms; 9 or 11 workers gave no more). tsgo query
    // at 16 threads on zbook, env runs: `arena_max` 7 with 8 bind threads
    // (round 2) makes 7 arenas and 131 MB peak RSS; `arena_max` 10 with 8
    // bind threads makes 10 arenas and 135 MB, with 4 bind threads 7 arenas
    // and 129 MB (5 bind threads: 8 arenas). At 4 cores nothing changes:
    // the counts are the core count there, so there are no spare arenas.
    // PERF (perf10 fix #4, effect-highcore.md section 8, env runs on dbook,
    // check mode): at 32 threads on 16 cores, 15 parse workers
    // (`GOPORT_PARSE_THREADS=15`) cut effect's parse from 38 to 31 ms and 16
    // bind threads its changes step from 20 to 16 ms (wall -6 to -10 ms).
    // At 16 threads on 8 cores (mini-743d) more threads gave nothing, so the
    // rule counts physical cores. Query is not a large load.
    // PERF (perf11 Q12, qprof `knobs2`, dbook, 60 rounds, env-only on the
    // perf10 release tsgo): the glibc arena locks of perf9 round 2 do not
    // apply to jemalloc. Query (not large) with 7 parse workers and 8 bind
    // threads: at 8 threads check -2.69 ms [-3.21, -2.07] and emit -2.47
    // [-2.94, -2.08], at 16 and 32 threads -2.4 to -2.7 ms, peak RSS -1.5
    // MiB. 15 workers and 16 bind threads were slower there than 7 and 8
    // (-1.6 against -2.6 ms). At 4 cores the counts do not change.
    pub fn one_program(extra: usize) -> Self {
        let cores = available_cores();
        let parse = DEFAULT_CHECKERS + 1;
        let parse_large = parse + 3;
        let spare = cores.min(parse_large) - cores.min(parse);
        let wide = wide_cores();
        let jemalloc = cfg!(feature = "jemalloc");
        ThreadBudget {
            parse: if jemalloc { parse_large } else { parse },
            parse_large: if wide { WIDE_CORES } else { parse_large },
            bind: if wide { WIDE_CORES } else { 8 },
            bind_small: if spare > 0 && !jemalloc { parse - 1 } else { 8 },
            arena_max: DEFAULT_CHECKERS + 2 + extra + spare,
        }
    }

    /// Parse threads of a program load, the loading thread included: one
    /// per core, up to `parse_large` for a large load and `parse` for others.
    pub fn parse_threads(&self, large: bool) -> usize {
        available_cores().min(if large { self.parse_large } else { self.parse })
    }

    /// Bind threads of a program: one per core, up to `bind` for a large
    /// load and `bind_small` for others.
    pub fn bind_threads(&self, large: bool) -> usize {
        available_cores().min(if large { self.bind } else { self.bind_small })
    }

    /// Makes this the budget of the program loads of this process. The
    /// first install wins.
    pub fn install(self) {
        let _ = THREAD_BUDGET.set(self);
    }

    /// The installed budget, or `WIDE`.
    pub fn current() -> Self {
        THREAD_BUDGET.get().copied().unwrap_or(Self::WIDE)
    }

    /// The `GLIBC_TUNABLES` value of this budget (see `bin/goport.rs`
    /// `set_malloc_tunables`), or `None` to set none.
    /// - glibc malloc (a build without the `jemalloc` feature): the huge
    ///   page settings, `arena_max` and the top pad.
    /// - jemalloc: glibc malloc serves only glibc itself (the thread-local
    ///   destructor list and the attributes of each thread), so it needs
    ///   nothing with no limit.
    ///
    /// Under an address space or data limit (`gostd::stack::memory_limit`),
    /// `arena_max` is 1. Each thread that calls glibc malloc gets its own
    /// arena, up to 8 per core, and each arena reserves 64 MiB of address
    /// space, which the limit counts in full. With jemalloc, effect at
    /// `ulimit -v 2G` then gives Go's output (without it: out of memory).
    pub fn glibc_tunables(&self) -> Option<String> {
        let limited = crate::gostd::stack::memory_limit().is_some();
        if cfg!(feature = "jemalloc") {
            return limited.then(|| String::from("glibc.malloc.arena_max=1"));
        }
        let arena_max = if limited { 1 } else { self.arena_max };
        Some(format!(
            "glibc.malloc.hugetlb=1:glibc.malloc.arena_max={arena_max}:glibc.malloc.top_pad=67108864"
        ))
    }
}

/// One file bound on a bind thread, with its ids already moved to program
/// ids, or None when binding it made thread-local state or panicked.
type ParallelBind = (usize, Option<(BoundFile, PreparedFileArena)>);

/// The work queue of the bind threads (`bind_files_parallel`).
struct BindQueue {
    state: Mutex<BindQueueState>,
    /// Signals new known offsets, a finished bind and `stop`.
    changed: std::sync::Condvar,
}

struct BindQueueState {
    /// The next file to bind, as an index into the bind order.
    next: usize,
    /// The number of files being bound.
    binding: usize,
    /// Set by the loading thread when it stops at a failed file.
    stop: bool,
    /// The arena counts of each bound file, by file index.
    counts: Vec<Option<ArenaMark>>,
    /// The id offsets of file `i` at index `i`. They are known when every
    /// earlier file is bound: each file adds its counts to the offsets of the
    /// file before it.
    offsets: Vec<ArenaOffsets>,
    /// Whether file `i` is a freeable file version. It and the file after
    /// it start at a chunk start (`ArenaOffsets::aligned`), as
    /// `Lineage::add_file` joins them (lsshells M3d).
    freeable: Vec<bool>,
    /// Bound files that wait for their offsets, by file index.
    waiting: std::collections::BTreeMap<usize, (BoundFile, SymbolArena)>,
}

impl BindQueue {
    /// The queue of files whose freeable flags are `freeable`, the first
    /// at the offsets `first` (or at the next chunk start when it is
    /// freeable).
    fn new(freeable: Vec<bool>, first: ArenaOffsets) -> Self {
        let first = if freeable.first() == Some(&true) {
            first.aligned()
        } else {
            first
        };
        BindQueue {
            state: Mutex::new(BindQueueState {
                next: 0,
                binding: 0,
                stop: false,
                counts: vec![None; freeable.len()],
                offsets: vec![first],
                freeable,
                waiting: std::collections::BTreeMap::new(),
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BindQueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stops the bind threads.
    fn stop(&self) {
        self.lock().stop = true;
        self.changed.notify_all();
    }
}

impl BindQueueState {
    /// Records the arena counts of bound file `i`. Returns true when that
    /// made the offsets of more files known.
    fn record(&mut self, i: usize, counts: ArenaMark) -> bool {
        self.counts[i] = Some(counts);
        let known = self.offsets.len();
        while let Some(&Some(file_counts)) = self.counts.get(self.offsets.len() - 1) {
            let file = self.offsets.len() - 1;
            let mut next = self.offsets[file].after(file_counts);
            if self.freeable[file] || self.freeable.get(file + 1) == Some(&true) {
                next = next.aligned();
            }
            self.offsets.push(next);
        }
        self.offsets.len() > known
    }

    /// The waiting file with the lowest index, when its offsets are known.
    fn take_ready(&mut self) -> Option<(usize, BoundFile, SymbolArena, ArenaOffsets)> {
        let (&i, _) = self.waiting.first_key_value()?;
        let offsets = *self.offsets.get(i)?;
        let (_, (bound, file_symbols)) = self.waiting.pop_first()?;
        Some((i, bound, file_symbols, offsets))
    }
}

/// Binds the program files that are not bound yet on several threads, each
/// file into its own arena, and joins the arenas into the lineage in file
/// order (a freeable file version on chunks of its own,
/// `Lineage::add_file`). It stops at the first file that made thread-local
/// state while binding (for example a lazy JSDoc parse) or panicked;
/// `bind_all` binds that file and the rest serially, which gives the same
/// result as a serial bind of every file.
// PERF: a file's ids move to their program values on a bind thread, as soon
// as every earlier file is bound, because its offsets are the sums of the
// earlier file counts. The threads stay while files wait for offsets, so
// when the last large file (lib.dom) is bound they move the waiting files in
// parallel. The loading thread only appends chunks, in file order. The ids
// are the ones that a join on the loading thread gives.
fn bind_files_parallel(lineage: &mut Lineage) {
    // Each file with whether it is a freeable file version.
    let files: Vec<(Node, bool)> = prog()
        .source_files()
        .filter(|file| file.file_bind.get().is_none())
        .map(|file| (file.root, file.version().is_some()))
        .collect();
    let threads = bind_thread_count(files.len()).min(files.len());
    if single_threaded() || threads < 2 {
        return;
    }
    let unported = unported_report();
    // PERF: the threads take the largest files first (by node count), so the
    // largest file (lib.dom) starts at once and the small files fill the other
    // threads while it binds. The join order stays the file order.
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(files[i].0.go_file().parser_flags.len()));
    let queue = BindQueue::new(
        files.iter().map(|&(_, freeable)| freeable).collect(),
        lineage.symbols.next_file_offsets(),
    );
    let (sender, receiver) = std::sync::mpsc::channel::<ParallelBind>();
    let complete = std::thread::scope(|scope| {
        for _ in 0..threads {
            let seed = WorkerSeed::take();
            let (files, order, queue, sender) = (&files, &order, &queue, sender.clone());
            std::thread::Builder::new()
                .stack_size(crate::gostd::stack::max_stack_size())
                .spawn_scoped(scope, move || {
                    seed.install();
                    let mut state = queue.lock();
                    loop {
                        if state.stop {
                            break;
                        }
                        // Moving ids first: the loading thread waits for it.
                        if let Some((i, mut bound, file_symbols, offsets)) = state.take_ready() {
                            drop(state);
                            bound.remap(offsets);
                            let prepared = file_symbols.prepare_file_arena(offsets, files[i].1);
                            let _ = sender.send((i, Some((bound, prepared))));
                            state = queue.lock();
                            continue;
                        }
                        if let Some(&i) = order.get(state.next) {
                            let (file, _) = files[i];
                            state.next += 1;
                            state.binding += 1;
                            drop(state);
                            let before = bind_thread_fingerprint();
                            let result = std::panic::catch_unwind(|| {
                                let _trace = trace_bind_source_file(file);
                                let mut file_symbols = SymbolArena::new();
                                let bound = bind_source_file_detached(file, &mut file_symbols);
                                (bound, file_symbols)
                            })
                            .ok()
                            .filter(|_| bind_thread_fingerprint() == before);
                            state = queue.lock();
                            state.binding -= 1;
                            let Some((bound, file_symbols)) = result else {
                                drop(state);
                                // Later files never get offsets now; waiting
                                // threads check whether to stay.
                                queue.changed.notify_all();
                                let _ = sender.send((i, None));
                                break;
                            };
                            let more = state.record(i, file_symbols.mark());
                            state.waiting.insert(i, (bound, file_symbols));
                            if more || state.binding == 0 {
                                queue.changed.notify_all();
                            }
                            continue;
                        }
                        // Nothing to bind. Stay while a waiting file can
                        // still get its offsets from a bind in progress.
                        if state.waiting.is_empty() || state.binding == 0 {
                            break;
                        }
                        state = queue
                            .changed
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                })
                // Go starts its threads on demand and fails the same way.
                .unwrap_or_else(|err| crate::core::go_fatal_newosproc(&err));
        }
        drop(sender);
        // Join the files in order as they arrive.
        let mut pending = FxHashMap::default();
        let mut joined = 0;
        for (i, result) in &receiver {
            pending.insert(i, result);
            while let Some(result) = pending.remove(&joined) {
                let Some((bound, prepared)) = result else {
                    queue.stop();
                    return false;
                };
                lineage.add_file(bound.file.file_index(), files[joined].1, |symbols| {
                    symbols.append_prepared_file_arena(prepared)
                });
                bound.install();
                joined += 1;
            }
        }
        true
    });
    if !complete {
        // The serial bind counts the hits of the file that stopped here.
        restore_unported(&unported);
    }
}

// ---------------------------------------------------------------------------
// Resolution modes (Go compiler/fileloader.go)
// ---------------------------------------------------------------------------

// Go: compiler/fileloader.go:718 getDefaultResolutionModeForFile
fn get_default_resolution_mode_for_file_worker(
    file_name: &str,
    meta: &SourceFileMetaData,
    options: &CompilerOptions,
) -> ResolutionMode {
    if import_syntax_affects_module_resolution(options) {
        get_implied_node_format_for_emit_worker(file_name, options.get_emit_module_kind(), meta)
    } else {
        RESOLUTION_MODE_NONE
    }
}

// Go: compiler/fileloader.go:726 getModeForUsageLocation
fn get_mode_for_usage_location_worker(
    file_name: &str,
    meta: &SourceFileMetaData,
    usage: Node,
    options: &CompilerOptions,
) -> ResolutionMode {
    let parent = usage.parent();
    if is_import_declaration(parent)
        || parent.kind() == SyntaxKind::JsImportDeclaration
        || is_export_declaration(parent)
        || parent.kind() == SyntaxKind::JsDocImportTag
    {
        let is_type_only = is_exclusively_type_only_import_or_export(parent);
        if is_type_only {
            let (override_, ok) = parent.attributes().get_resolution_mode_override(None);
            if ok {
                return override_;
            }
        }
    }
    if is_literal_type_node(parent) && is_import_type_node(parent.parent()) {
        let (override_, ok) = parent
            .parent()
            .attributes()
            .get_resolution_mode_override(None);
        if ok {
            return override_;
        }
    }
    if import_syntax_affects_module_resolution(options) {
        return get_emit_syntax_for_usage_location_worker(file_name, meta, usage, options);
    }
    RESOLUTION_MODE_NONE
}

// Go: compiler/fileloader.go:758 importSyntaxAffectsModuleResolution
fn import_syntax_affects_module_resolution(options: &CompilerOptions) -> bool {
    let module_resolution = options.get_module_resolution_kind();
    ModuleResolutionKind::NODE16 <= module_resolution
        && module_resolution <= ModuleResolutionKind::NODE_NEXT
        || options.get_resolve_package_json_exports()
        || options.get_resolve_package_json_imports()
}

// Go: compiler/fileloader.go:764 getEmitSyntaxForUsageLocationWorker
fn get_emit_syntax_for_usage_location_worker(
    file_name: &str,
    meta: &SourceFileMetaData,
    usage: Node,
    options: &CompilerOptions,
) -> ResolutionMode {
    let parent = usage.parent();
    if is_require_call(parent, false)
        || is_external_module_reference(parent) && is_import_equals_declaration(parent.parent())
    {
        return ModuleKind::COMMON_JS;
    }
    let file_emit_mode = get_emit_module_format_of_file_worker(file_name, options, meta);
    if is_import_call(walk_up_parenthesized_expressions(parent)) {
        return if should_transform_import_call(file_name, options, file_emit_mode) {
            ModuleKind::COMMON_JS
        } else {
            ModuleKind::ES_NEXT
        };
    }
    // If we're in --module preserve on an input file, we know that an import
    // is an import. But if this is a declaration file, we'd prefer to use the
    // impliedNodeFormat. Since we want things to be consistent between the two,
    // we need to issue errors when the user writes ESM syntax in a definitely-CJS
    // file, until/unless declaration emit can indicate a true ESM import. On the
    // other hand, writing CJS syntax in a definitely-ESM file is fine, since declaration
    // emit preserves the CJS syntax.
    if file_emit_mode == ModuleKind::COMMON_JS {
        return ModuleKind::COMMON_JS;
    }
    if file_emit_mode.is_non_node_esm() || file_emit_mode == ModuleKind::PRESERVE {
        return ModuleKind::ES_NEXT;
    }
    ModuleKind::NONE
}

// ---------------------------------------------------------------------------
// Program methods (Go compiler/program.go). The program is the current
// `prog()`; these are free functions.
// PORT: Go `projectReferenceFileMapper.getCompilerOptionsForFile` returns the
// program options when there are no project references, which is always the
// case here.
// ---------------------------------------------------------------------------

/// Lazy JSDoc of `node` in `file` (Go `SourceFile.resolveJSDoc`).
// PORT: the files of an alias resolver program are in no program, or in
// another program version; `go_frontend` keeps their parser inputs. So
// does a read with no current program: the api encodes a leased source
// file (api/session.go encodeLeasedSourceFile, ts#64434) outside any
// program, and Go's `SourceFile` resolves its JSDoc by itself.
pub fn resolve_lazy_js_doc(file: Node, node: Node) -> Option<&'static [Node]> {
    if crate::core::try_prog().is_none() || state().alias_resolver {
        return go_frontend::resolve_js_doc_outside_program(file, node);
    }
    Some(with_go(|go| go.resolve_js_doc(file, node)))
}

// Go: compiler/program.go:122 FileExists
// Go: ls/autoimport/aliasresolver.go:158 FileExists (unimplemented)
pub fn file_exists(path: &str) -> bool {
    alias_resolver_unimplemented();
    go_frontend::file_exists(path)
}

// Go: compiler/program.go:127 GetCurrentDirectory
pub fn get_current_directory() -> &'static str {
    state().cwd
}

// Go: compiler/program.go:215 UseCaseSensitiveFileNames
pub fn use_case_sensitive_file_names() -> bool {
    state().use_case_sensitive_file_names
}

// Go: compiler/program.go:219 UsesUriStyleNodeCoreModules
// Go never assigns the program field (only UpdateProgram copies it), so the
// value is always unknown.
pub fn uses_uri_style_node_core_modules() -> Tristate {
    Tristate::Unknown
}

// Go: compiler/program.go:173 GetProjectReferenceFromSource
// PORT: the Go frontend program has the port.
pub fn get_project_reference_from_source(
    path: &str,
) -> Option<Arc<SourceOutputAndProjectReference>> {
    // Go: ls/autoimport/aliasresolver.go:193 (unimplemented)
    alias_resolver_unimplemented();
    with_go(|go| go.get_project_reference_from_source(path))
}

// Go: compiler/program.go:178 IsSourceFromProjectReference
pub fn is_source_from_project_reference(path: &str) -> bool {
    // Go: ls/autoimport/aliasresolver.go:223 (unimplemented)
    alias_resolver_unimplemented();
    with_go(|go| go.is_source_from_project_reference(path))
}

// Go: compiler/program.go:182 GetProjectReferenceFromOutputDts
// PORT: see `get_project_reference_from_source`.
pub fn get_project_reference_from_output_dts(
    path: &str,
) -> Option<Arc<SourceOutputAndProjectReference>> {
    // Go: ls/autoimport/aliasresolver.go:188 (unimplemented)
    alias_resolver_unimplemented();
    with_go(|go| go.get_project_reference_from_output_dts(path))
}

// Go: compiler/program.go:190 GetRedirectForResolution
// PORT: see `get_project_reference_from_source`.
pub fn get_redirect_for_resolution(file: Node) -> Option<Arc<ResolvedProjectReference>> {
    // Go: ls/autoimport/aliasresolver.go:198 (unimplemented)
    alias_resolver_unimplemented();
    with_go(|go| go.get_redirect_for_resolution(file).cloned())
}

// Go: compiler/projectreferencefilemapper.go:80 getCompilerOptionsForFile
// Go: module/resolver.go:145 GetCompilerOptionsWithRedirect
// Go: compiler/program.go:1519 GetSourceFileMetaData
// Runs `f` with the options of the project reference that owns the file
// (else the root options) and the metadata of the file (the default
// metadata when it is not a program file). The per-file mode queries below
// use it for each import, so neither value is copied.
fn with_file_options_and_meta<R>(
    file: Node,
    f: impl FnOnce(&CompilerOptions, &SourceFileMetaData) -> R,
) -> R {
    static MISSING: std::sync::LazyLock<SourceFileMetaData> =
        std::sync::LazyLock::new(SourceFileMetaData::default);
    let path = &source_file_info(file).path;
    with_tables(|tables| {
        let options = tables
            .go
            .as_ref()
            .and_then(|go| go.get_redirect_for_resolution(file))
            .map_or(prog().options, |redirect| redirect.compiler_options());
        let meta = tables
            .file_meta_by_path(path)
            .map_or(&*MISSING, |meta| &meta.meta_data);
        f(options, meta)
    })
}

// Go: compiler/program.go:199 GetResolvedProjectReferences
// PORT: see `get_project_reference_from_source`. A reference that did not
// load is None (Go nil).
pub fn get_resolved_project_references() -> Vec<Option<Arc<ResolvedProjectReference>>> {
    with_tables(|tables| {
        tables
            .go
            .as_ref()
            .map(go_frontend::GoSharedState::get_resolved_project_references)
            .unwrap_or_default()
    })
}

// Go: compiler/program.go:2017 GetSymlinkCache
// PORT: the Go frontend program has the port, and this is its value.
pub fn get_go_symlink_cache() -> Option<Arc<crate::modulespecifiers::symlinks::KnownSymlinks>> {
    // Go: ls/autoimport/aliasresolver.go:143 (unimplemented)
    alias_resolver_unimplemented();
    Some(with_go(go_frontend::GoSharedState::known_symlinks))
}

// Go: compiler/program.go:165 GetSourceOfProjectReferenceIfOutputIncluded
pub fn get_source_of_project_reference_if_output_included(file: Node) -> String {
    // Go: ls/autoimport/aliasresolver.go:213 (unimplemented)
    alias_resolver_unimplemented();
    let info = source_file_info(file);
    with_go(|go| {
        go.get_source_of_project_reference_if_output_included(&info.path)
            .map_or_else(|| info.file_name.clone(), str::to_string)
    })
}

/// Go `compiler.NewProgram` for a config that is already parsed, in a
/// one-program process (`tsc`, `goport`). It installs the program for the
/// process, so call it once. `opts.host` carries the trace writer.
pub fn install_new_program(
    opts: crate::frontend::compiler::ProgramOptions,
) -> Result<&'static GoProgram, String> {
    go_frontend::install_new_program(opts)
}

/// Go `compiler.NewProgram` for a multi-program process (watch, language
/// server, tests), with the Go frontend. It loads a new program version and
/// does not make it current: read it inside `core::enter_program`. Call it
/// on the loading thread, which keeps the frontend and the checker pool of
/// the version. `edit_options` is as in `try_load_with`. The version is
/// leaked; `release_program` frees its checker pool.
pub fn try_load_version(
    config_path: &str,
    edit_options: impl FnOnce(&mut CompilerOptions),
) -> Result<&'static GoProgram, String> {
    go_frontend::try_load_version(config_path, edit_options)
}

/// Go `Program.UpdateProgram`: a new version of `old` after an edit of
/// `changed_file` (a file name, relative to the current directory or
/// absolute). It reads `changed_file` from disk again. When the edit keeps
/// the imports and references, the new version shares every other file
/// version with `old` and the second value is true. Else every file is
/// parsed again. `old` stays usable. Loading thread only. With
/// `GOPORT_FREE_FILE_VERSIONS=1`, each new parse of a path that was
/// published before is a freeable file version (lsshells M3b), as in the
/// language server.
pub fn update_program_version(
    old: &'static GoProgram,
    changed_file: &str,
) -> (&'static GoProgram, bool) {
    go_frontend::update_program_version(old, changed_file)
}

/// Go `compiler.NewProgram` and `Program.UpdateProgram` for a
/// multi-program process whose caller builds the frontend program `np`
/// itself (the language server, watch mode). It builds the Go files of the
/// stores that `np` parsed, publishes them and makes the program version of
/// `np`. It does not make it current: read it inside `core::enter_program`.
/// `previous` is the version that `np` was updated from, if it is still
/// loaded; the new version shares its copies of unchanged frontend data.
/// Call it on the loading thread, after `np` is built with no current
/// program.
pub fn new_program_version(
    np: &Rc<crate::frontend::compiler::NewProgram>,
    previous: Option<&'static GoProgram>,
) -> &'static GoProgram {
    go_frontend::new_program_version(np, previous)
}

/// Records a source file that this thread parsed outside a program load
/// (the language server parse cache). When a program version publishes
/// the file's store but does not include the file, the store still gets
/// the file's Go file (parser fields), so a later version can share it.
pub fn note_parsed_source_file(file: &Rc<crate::frontend::parser::ParsedSourceFile>) {
    go_frontend::note_parsed_source_file(file);
}

/// Publishes this thread's unpublished stores with no program: the files
/// that `note_parsed_source_file` recorded get their Go files, and other
/// stores (config files) the name only. Then their nodes can be bound
/// (`bind_file_outside_program`). `cwd` is the current directory of the
/// caller's host.
pub fn publish_parsed_files(cwd: &str) {
    go_frontend::publish_parsed_files(cwd);
}

/// Go `binder.BindSourceFile` for a published file that no program
/// includes (Go `BindOnce`: a program that includes it later does not bind
/// it again). The file joins the binder lineage of the process.
pub fn bind_file_outside_program(file: Node) {
    with_lineage(|lineage| lineage.bind(file));
}

/// Frees what the loading thread keeps for `program`: it stops the checker
/// pool (and waits for its workers, which free their checkers and synthetic
/// nodes), removes the frontend program from this thread and takes the
/// program tables (`VersionTables`), which are freed when no other thread
/// holds them. Do not use `program` after this: a read of its tables
/// panics. The frontend program is freed with its last `Rc` holder. The
/// `GoProgram` and the static file versions stay leaked; a freeable file
/// version is freed with its last holder (`ast/file_version.rs`). Panics
/// when `program` is current on this thread.
pub fn release_program(program: &'static GoProgram) {
    drop(release_program_with(program, CheckerPool::shut_down));
}

/// `release_program` that frees the frontend program and the tables only
/// when the result drops. The language server keeps the result until it
/// has sent the answer (`ls_program::release_now`), so these frees are not
/// in the answer time. The checker pool stops at once, as in
/// `release_program`, so the old checkers do not add to the memory of the
/// next program. Watch mode uses `release_program_in_background`.
pub fn release_program_later(program: &'static GoProgram) -> ReleasedProgram {
    release_program_with(program, CheckerPool::shut_down)
}

/// What a released program version keeps until it drops: its frontend
/// program (freed here when no other holder has it) and its tables (freed
/// when no other thread holds them). The version cannot be read in the
/// meantime: its slot is empty.
#[must_use = "dropping it frees the released program at once"]
pub struct ReleasedProgram {
    frontend: Option<Rc<crate::frontend::compiler::NewProgram>>,
    tables: Option<Arc<VersionTables>>,
}

impl Drop for ReleasedProgram {
    // lsshells M3b: this thread drops its file version pins, and the other
    // threads drop theirs at their next pinned read, so a freeable file
    // version of the released program dies with its other holders (the
    // fields below, the parse cache entry). The live versions are pinned
    // again when they are read.
    fn drop(&mut self) {
        crate::ast::release_file_version_pins();
    }
}

/// `release_program` that does not wait for the checker workers: they free
/// their checkers and synthetic nodes on their own threads while the caller
/// goes on (Go frees a program in the background GC), and the program
/// tables go when the last of them ends. `tsc -b` and watch mode use it,
/// so the next project or rebuild does not wait for the free.
pub fn release_program_in_background(program: &'static GoProgram) {
    drop(release_program_with(
        program,
        CheckerPool::shut_down_in_background,
    ));
}

/// `release_program_in_background` that frees the frontend program only
/// when the result drops. The tables go at once, as in
/// `release_program_in_background`: the checker threads keep them until
/// they end, so the last of them frees them in the background. `tsc -b`
/// drops the result when its orchestrator thread would wait anyway.
pub fn release_program_in_background_later(program: &'static GoProgram) -> ReleasedProgram {
    let mut released = release_program_with(program, CheckerPool::shut_down_in_background);
    drop(released.tables.take());
    released
}

/// `release_program` with the pool stop that `shut_down` names. The caller
/// picks when the result drops.
fn release_program_with(
    program: &'static GoProgram,
    shut_down: fn(CheckerPool),
) -> ReleasedProgram {
    assert!(
        !try_prog().is_some_and(|current| std::ptr::eq(current, program)),
        "program {} is released while it is current",
        program.id
    );
    let pool = POOLS.with(|pools| pools.borrow_mut().remove(&program.id));
    if let Some(pool) = pool {
        shut_down(pool);
    }
    // The map borrow ends before the frontend program can be freed.
    let frontend = FRONTENDS.with(|frontends| frontends.borrow_mut().remove(&program.id));
    let tables = program
        .state
        .get()
        .and_then(|state| release_tables(program.id, state));
    ReleasedProgram { frontend, tables }
}

/// The Go frontend program, or None for an alias resolver program. Loading
/// thread only.
pub fn go_frontend_program() -> Option<Rc<crate::frontend::compiler::NewProgram>> {
    go_frontend()
}

// Go: compiler/program.go:1841 ExplainFiles
pub fn explain_files(w: &mut String, locale: &crate::locale::Locale) {
    go_frontend()
        .expect("explain files of an alias resolver program")
        .explain_files(w, locale);
}

// Go: compiler/program.go:397 SourceFiles
pub fn source_files() -> Vec<Node> {
    prog().source_files().map(|file| file.root).collect()
}

// Go: compiler/program.go:399 Options
pub fn options() -> &'static CompilerOptions {
    &prog().options
}

// Go: compiler/program.go:528 ContentMapperExtensions (#4712)
// PORT: the Go frontend program copies the extensions of its command line
// (`GoSharedState`), so checker threads can read them. An alias resolver
// program has none.
pub fn content_mapper_extensions() -> Vec<String> {
    with_tables(|tables| {
        tables
            .go
            .as_ref()
            .map(|go| go.content_mapper_extensions().to_vec())
            .unwrap_or_default()
    })
}

/// Go `Program.contentMapperDiagnostics` (#4712): the program diagnostics
/// that the loader reports when a content mapper fails for good (Go
/// `processedFiles.contentMapperDiagnostics`). An alias resolver program
/// has none. Loading thread only.
fn content_mapper_diagnostics() -> Vec<Diagnostic> {
    go_frontend()
        .map(|go| go.content_mapper_diagnostics.clone())
        .unwrap_or_default()
}

/// Go `Program.contentMapperOptionDiagnostics` (#4712), made by
/// `collectContentMapperOptionDiagnostics` (see `go_frontend`). An alias
/// resolver program has none.
fn content_mapper_option_diagnostics() -> Vec<Diagnostic> {
    with_tables(|tables| {
        tables
            .go
            .as_ref()
            .map(|go| go.content_mapper_option_diagnostics().to_vec())
            .unwrap_or_default()
    })
}

// Go: compiler/program.go:403 GetConfigFileParsingDiagnostics
// PORT: an alias resolver program has no config, so its list is empty. Go
// has no such method on the alias resolver.
pub fn get_config_file_parsing_diagnostics() -> Vec<Diagnostic> {
    let Some(go) = go_frontend() else {
        return Vec::new();
    };
    go.get_config_file_parsing_diagnostics()
}

// Go: compiler/program.go:441 SingleThreaded
pub fn single_threaded() -> bool {
    prog().options.single_threaded.is_true()
}

// Go: compiler/program.go:494 GetResolvedModule
// PERF: the stored resolution is shared, not copied.
pub fn get_resolved_module(
    file: Node,
    module_reference: &str,
    mode: ResolutionMode,
) -> Option<Arc<ResolvedModule>> {
    // Go: ls/autoimport/aliasresolver.go:116 GetResolvedModule (never nil)
    if let Some(resolver) = alias_resolver() {
        return Some(resolver.resolved_module(file, module_reference, mode));
    }
    with_go(|go| {
        go.get_resolved_module(file, module_reference, mode)
            .cloned()
    })
}

// Go: compiler/program.go:503 GetResolvedModuleFromModuleSpecifier
pub fn get_resolved_module_from_module_specifier(
    file: Node,
    module_specifier: Node,
) -> Option<ResolvedModule> {
    // Go: ls/autoimport/aliasresolver.go:208 (unimplemented)
    alias_resolver_unimplemented();
    if !is_string_literal_like(module_specifier) {
        panic!("moduleSpecifier must be a StringLiteralLike");
    }
    let mode = get_mode_for_usage_location(file, module_specifier);
    get_resolved_module(file, module_specifier.text(), mode).map(Arc::unwrap_or_clone)
}

// Go: compiler/program.go:511 GetResolvedModules
// Go: ls/autoimport/aliasresolver.go:135 GetResolvedModules (nil)
// PORT: only an alias resolver program reads this map, and it is empty. The
// Go frontend program keeps its map in `GoSharedState` (`get_packages_map`).
pub fn get_resolved_modules()
-> &'static IndexMap<String, IndexMap<(String, ResolutionMode), ResolvedModule>> {
    static EMPTY: OnceLock<IndexMap<String, IndexMap<(String, ResolutionMode), ResolvedModule>>> =
        OnceLock::new();
    assert!(
        state().alias_resolver,
        "resolved modules of a program that is not an alias resolver"
    );
    EMPTY.get_or_init(IndexMap::new)
}

// Go: compiler/program.go:1519 GetSourceFileMetaData
pub fn get_source_file_meta_data(path: &str) -> SourceFileMetaData {
    // Go: ls/autoimport/aliasresolver.go:148 (unimplemented)
    alias_resolver_unimplemented();
    with_tables(|tables| {
        tables
            .file_meta_by_path(path)
            .map(|meta| meta.meta_data.clone())
            .unwrap_or_default()
    })
}

// Go: compiler/program.go:1523 GetEmitModuleFormatOfFile
pub fn get_emit_module_format_of_file(source_file: Node) -> ModuleKind {
    // Go: ls/autoimport/aliasresolver.go:96 GetEmitModuleFormatOfFile
    if state().alias_resolver {
        return ModuleKind::ES_NEXT;
    }
    let info = source_file_info(source_file);
    with_file_options_and_meta(source_file, |options, meta| {
        get_emit_module_format_of_file_worker(&info.file_name, options, meta)
    })
}

// Go: compiler/program.go:1527 GetEmitSyntaxForUsageLocation
pub fn get_emit_syntax_for_usage_location(source_file: Node, location: Node) -> ResolutionMode {
    // Go: ls/autoimport/aliasresolver.go:101 GetEmitSyntaxForUsageLocation
    if state().alias_resolver {
        return ModuleKind::ES_NEXT;
    }
    let info = source_file_info(source_file);
    with_file_options_and_meta(source_file, |options, meta| {
        get_emit_syntax_for_usage_location_worker(&info.file_name, meta, location, options)
    })
}

// Go: compiler/program.go:1531 GetImpliedNodeFormatForEmit
pub fn get_implied_node_format_for_emit(source_file: Node) -> ResolutionMode {
    // Go: ls/autoimport/aliasresolver.go:106 GetImpliedNodeFormatForEmit
    if state().alias_resolver {
        return ModuleKind::ES_NEXT;
    }
    let info = source_file_info(source_file);
    with_file_options_and_meta(source_file, |options, meta| {
        get_implied_node_format_for_emit_worker(
            &info.file_name,
            options.get_emit_module_kind(),
            meta,
        )
    })
}

// Go: compiler/program.go:1535 GetModeForUsageLocation
pub fn get_mode_for_usage_location(source_file: Node, location: Node) -> ResolutionMode {
    // Go: ls/autoimport/aliasresolver.go:111 GetModeForUsageLocation
    if state().alias_resolver {
        return ModuleKind::ES_NEXT;
    }
    let info = source_file_info(source_file);
    with_file_options_and_meta(source_file, |options, meta| {
        get_mode_for_usage_location_worker(&info.file_name, meta, location, options)
    })
}

// Go: compiler/program.go:1539 GetDefaultResolutionModeForFile
pub fn get_default_resolution_mode_for_file(source_file: Node) -> ResolutionMode {
    // Go: ls/autoimport/aliasresolver.go:91 GetDefaultResolutionModeForFile
    if state().alias_resolver {
        return ModuleKind::ES_NEXT;
    }
    let info = source_file_info(source_file);
    with_file_options_and_meta(source_file, |options, meta| {
        get_default_resolution_mode_for_file_worker(&info.file_name, meta, options)
    })
}

// Go: compiler/program.go:1543 IsSourceFileDefaultLibrary
pub fn is_source_file_default_library(path: &str) -> bool {
    with_tables(|tables| {
        tables
            .file_meta_by_path(path)
            .is_some_and(|meta| meta.is_default_library)
    })
}

// Go: compiler/program.go:1562 CommonSourceDirectory
// PORT: the Go frontend sets it when it builds the program
// (`go_frontend::common_source_directory_of`).
pub fn common_source_directory() -> &'static str {
    // Go: ls/autoimport/aliasresolver.go:153 (unimplemented)
    alias_resolver_unimplemented();
    state()
        .common_source_directory
        .expect("the Go frontend sets the common source directory")
}

// Go: compiler/program.go:1912 IsSourceFileFromExternalLibrary
pub fn is_source_file_from_external_library(file: Node) -> bool {
    let path = &source_file_info(file).path;
    with_go(|go| go.is_source_file_from_external_library(path))
}

// Go: compiler/program.go:1225 IsEmitBlocked
pub fn is_emit_blocked(emit_file_name: &str) -> bool {
    with_go(|go| go.is_emit_blocked(emit_file_name))
}

// Go: compiler/program.go:2210 SourceFileMayBeEmitted
pub fn source_file_may_be_emitted(source_file: Node, force_dts_emit: bool) -> bool {
    // Go: ls/autoimport/aliasresolver.go:228 (unimplemented)
    alias_resolver_unimplemented();
    source_file_may_be_emitted_worker(source_file, force_dts_emit, false)
}

// Go: compiler/emitter.go:493 sourceFileMayBeEmitted
fn source_file_may_be_emitted_worker(
    source_file: Node,
    force_dts_emit: bool,
    force_js_emit: bool,
) -> bool {
    let options = &prog().options;
    let info = source_file_info(source_file);
    // Js files are emitted only if option is enabled
    if !force_js_emit && options.no_emit_for_js_files.is_true() && is_source_file_js(source_file) {
        return false;
    }
    // Declaration files are not emitted
    if info.is_declaration_file {
        return false;
    }
    // #4712
    // Runtime output for content-mapped files is owned by the external content mapper or build tool. Only
    // include them in the emit set when their transformed TypeScript can produce declarations.
    if !source_file_content_mapper(source_file).is_empty()
        && !force_dts_emit
        && !options.get_emit_declarations()
    {
        return false;
    }
    // Source file from node_modules are not emitted
    if is_source_file_from_external_library(source_file) {
        return false;
    }
    // forcing dts emit => file needs to be emitted
    if force_dts_emit || force_js_emit {
        return true;
    }
    // Source files from referenced projects are not emitted
    if get_project_reference_from_source(&info.path).is_some() {
        return false;
    }
    // Any non json file should be emitted
    if !is_json_source_file(source_file) {
        return true;
    }
    json_file_may_be_emitted(
        &info.file_name,
        options,
        get_current_directory(),
        use_case_sensitive_file_names(),
    )
}

// Go: compiler/emitter.go:504-521 (the JSON file part of sourceFileMayBeEmitted)
// PORT: its own function, so the tests below can run it without a loaded
// program. Like `frontend::compiler::source_file_may_be_emitted`, it uses the
// Go ports in `frontend::outputpaths` and `frontend::tspath`. Go result to
// keep: `GetNormalizedAbsolutePath` removes the trailing separator of the
// common directory (except for a root), so a file under it gets a rooted
// output path that is never its own path, even when outDir is the common
// directory.
fn json_file_may_be_emitted(
    file_name: &str,
    options: &CompilerOptions,
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> bool {
    use crate::frontend::outputpaths;
    use crate::frontend::tspath::{
        ComparePathsOptions, compare_paths, get_normalized_absolute_path,
    };

    // Json file is not emitted if outDir is not specified
    if options.out_dir.is_empty() {
        return false;
    }

    // Otherwise, if rootDir is specified or a config file exists, we know the common source directory and can check if the file would be emitted in the same location
    if !options.root_dir.is_empty() || !options.config_file_path.is_empty() {
        let common_dir = get_normalized_absolute_path(
            &outputpaths::get_common_source_directory(
                options,
                Vec::new,
                current_directory,
                use_case_sensitive_file_names,
                None,
            ),
            current_directory,
        );
        let output_path = outputpaths::get_source_file_path_in_new_dir_worker(
            file_name,
            &options.out_dir,
            current_directory,
            &common_dir,
            use_case_sensitive_file_names,
        );
        if compare_paths(
            file_name,
            &output_path,
            &ComparePathsOptions {
                use_case_sensitive_file_names,
                current_directory: current_directory.to_string(),
            },
        ) == 0
        {
            return false;
        }
    }

    true
}

// Go: compiler/program.go:1792 GetSourceFile
pub fn get_source_file(file_name: &str) -> Node {
    // Go: ls/autoimport/aliasresolver.go:80 GetSourceFile
    if let Some(resolver) = alias_resolver() {
        return resolver.source_file(file_name);
    }
    let path = tspath::to_path(
        file_name,
        get_current_directory(),
        use_case_sensitive_file_names(),
    );
    get_source_file_by_path(path.as_str())
}

// Go: compiler/program.go:1812 GetSourceFileByPath
pub fn get_source_file_by_path(path: &str) -> Node {
    with_tables(|tables| tables.file_at_path(path))
        .map_or(Node::NIL, |index| crate::ast::go_file(index).root)
}

/// A memo of `get_source_file_for_resolved_module` by file name, for the
/// program with id `program`.
struct ResolvedModuleFiles {
    program: u32,
    files: FxHashMap<String, Node>,
}

thread_local! {
    /// Memo of `get_source_file_for_resolved_module` for the current program
    /// of this thread. Program ids start at 1, so 0 is no program.
    static RESOLVED_MODULE_FILES: RefCell<ResolvedModuleFiles> = const {
        RefCell::new(ResolvedModuleFiles {
            program: 0,
            files: FxHashMap::with_hasher(rustc_hash::FxBuildHasher),
        })
    };
}

// Go: compiler/program.go:1797 GetSourceFileForResolvedModule
// PORT: the answer is memoized per thread and program. The files, their
// paths and the redirects of a program do not change after load, so a name
// always gives the same file. The checker asks again on each alias and
// default-import check, and each lookup canonicalizes the path. Another
// program version can give another file (an edited file has a new id), so
// the memo is emptied when the current program changes.
pub fn get_source_file_for_resolved_module(file_name: &str) -> Node {
    // Go: ls/autoimport/aliasresolver.go:130 GetSourceFileForResolvedModule
    if let Some(resolver) = alias_resolver() {
        return resolver.source_file_for_resolved_module(file_name);
    }
    let program = prog().id;
    let hit = RESOLVED_MODULE_FILES.with_borrow_mut(|memo| {
        if memo.program != program {
            memo.program = program;
            memo.files.clear();
            return None;
        }
        memo.files.get(file_name).copied()
    });
    if let Some(file) = hit {
        return file;
    }
    let mut file = get_source_file(file_name);
    if file.is_nil()
        && let Some(redirect) =
            with_go(|go| go.get_parse_file_redirect(file_name).map(str::to_string))
    {
        file = get_source_file(&redirect);
    }
    RESOLVED_MODULE_FILES.with_borrow_mut(|memo| memo.files.insert(file_name.to_string(), file));
    file
}

// Go: compiler/program.go:157 GetRedirectTargets
pub fn get_redirect_targets(path: &crate::frontend::tspath::Path) -> Vec<String> {
    // Go: ls/autoimport/aliasresolver.go:203 (unimplemented)
    alias_resolver_unimplemented();
    with_go(|go| go.get_redirect_targets(&path.0))
}

// Go: compiler/program.go:226 GetSourceFileFromReference
// PORT: the Go frontend program has the port, and its answers for the
// preserved references are copied. Go has no such method on the alias
// resolver.
pub fn get_source_file_from_reference(origin: Node, r: &FileReference) -> Node {
    alias_resolver_unimplemented();
    with_go(|go| go.get_source_file_from_reference(origin, r))
}

// Go: outputpaths/outputpaths.go:42 GetOutputPathsFor, called by
// compiler/emitHost.go:94 emitHost.GetOutputPathsFor and Program.Emit with
// the program options.
// PORT: Go reads three fields of the source file (#4712: the content
// mapper). It is named for the emit host method so it does not clash with
// the frontend `get_output_paths_for` in the frontend prelude.
pub fn get_output_paths_for_source_file(
    file: Node,
    host: &dyn crate::frontend::outputpaths::OutputPathsHost,
    force: crate::frontend::outputpaths::ForceEmitPaths,
) -> crate::frontend::outputpaths::OutputPaths {
    let info = source_file_info(file);
    crate::frontend::outputpaths::get_output_paths_for_file(
        &info.file_name,
        info.script_kind,
        source_file_content_mapper(file),
        options(),
        host,
        force,
    )
}

// Go: compiler/program.go:1916 GetJSXRuntimeImportSpecifier
// Go: compiler/fileloader.go:550 (the value the loader records)
// PORT: the Go frontend loader records the value and its synthetic import
// (Go `createSyntheticImport`).
pub fn get_jsx_runtime_import_specifier(path: &str) -> (String, Node) {
    // Go: ls/autoimport/aliasresolver.go:178 GetJSXRuntimeImportSpecifier
    // (no specifier, ts#64417)
    if state().alias_resolver {
        return (String::new(), Node::NIL);
    }
    with_go(|go| go.get_jsx_runtime_import_specifier(path))
}

// Go: compiler/program.go:1923 GetImportHelpersImportSpecifier
// PORT: the Go frontend loader records the synthetic imports.
pub fn get_import_helpers_import_specifier(path: &str) -> Node {
    // Go: ls/autoimport/aliasresolver.go:168 (unimplemented)
    alias_resolver_unimplemented();
    with_go(|go| go.get_import_helpers_import_specifier(path))
}

// Go: compiler/program.go:517 GetPackagesMap
// Go: checker/utilities.go:1645 getPackagesMap (the same body)
// PORT: Go caches the map on the program (and on the checker); this builds
// it on each call. Go ranges over `GetResolvedModules()`, the program's own
// map. With the Go frontend that is the version's map in `GoSharedState`,
// borrowed, so no owned copy of every resolution stays with the version.
// An alias resolver program (no resolutions) uses `get_resolved_modules`.
// The result does not depend on the order: each value is an OR.
pub fn get_packages_map() -> FxHashMap<String, bool> {
    let mut packages_map: FxHashMap<String, bool> = FxHashMap::default();
    let mut add = |module: &ResolvedModule| {
        if !module.package_id.name.is_empty() {
            let previous = packages_map
                .get(&module.package_id.name)
                .copied()
                .unwrap_or(false);
            packages_map.insert(
                module.package_id.name.clone(),
                previous || module.extension == ".d.ts",
            );
        }
    };
    let is_go = with_tables(|tables| {
        let go = tables.go.as_ref()?;
        go.resolved_modules().for_each(&mut add);
        Some(())
    });
    if is_go.is_none() {
        for resolved_modules_in_file in get_resolved_modules().values() {
            resolved_modules_in_file.values().for_each(&mut add);
        }
    }
    packages_map
}

// ---------------------------------------------------------------------------
// Checker pool (Go compiler/checkerpool.go)
// PORT: Go runs one goroutine task per checker on a work group. A Rust
// checker is not Send (it holds Rc and thread-local state), so each checker
// is made on its own worker thread and stays there. The loading thread
// sends jobs to the workers and waits for the results, which it merges in
// file order. Each checker sees only its own files, in file order, so the
// results match the Go grouping and do not depend on thread timing.
// ---------------------------------------------------------------------------

thread_local! {
    /// The checker pools of the loading thread, by `GoProgram::id`.
    static POOLS: RefCell<FxHashMap<u32, CheckerPool>> = RefCell::new(FxHashMap::default());
    /// The checker of a worker thread.
    static WORKER_CHECKER: RefCell<Option<Checker>> = const { RefCell::new(None) };
    /// The pool index of the checker of a worker thread.
    static WORKER_INDEX: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Set on a worker thread when `CheckerPool::stop` freed its checker and
    /// synthetic nodes.
    static WORKER_RELEASED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The d.ts twin of a worker thread (`send_dts_twin_job`).
    static DTS_TWIN: RefCell<Option<DtsTwin>> = const { RefCell::new(None) };
}

/// The thread-local state that a checker worker starts from: the current
/// program and its tables, and the synthetic nodes, ids and lazy JSDoc of
/// the loading thread when the pool is made. The language server's
/// cross-project search threads start from it too (`ls/search_thread.rs`).
/// The thread keeps its copy of the program tables until it ends, so it can
/// finish its work after the program is released, as a Go goroutine that
/// holds the program does. The tables hold the freeable file versions of
/// the program files (`VersionTables::file_versions`), so the thread keeps
/// those alive too.
pub(crate) struct WorkerSeed {
    program: &'static GoProgram,
    tables: Option<(u32, Arc<VersionTables>)>,
    synthetic: SyntheticSeed,
    ids: IdSeed,
    lazy_jsdoc: PerFileMap<&'static [Node]>,
}

impl WorkerSeed {
    pub(crate) fn take() -> Self {
        Self {
            program: prog(),
            tables: current_tables(),
            synthetic: synthetic_seed(),
            ids: id_seed(),
            lazy_jsdoc: go_frontend::lazy_jsdoc_seed(),
        }
    }

    pub(crate) fn install(self) {
        crate::core::set_thread_program(Some(self.program));
        if let Some(tables) = self.tables {
            TABLES.with(|cache| *cache.borrow_mut() = Some(tables));
        }
        install_synthetic_seed(self.synthetic);
        install_id_seed(self.ids);
        go_frontend::install_lazy_jsdoc_seed(self.lazy_jsdoc);
    }
}

/// Runs `f` on a new thread that starts from this thread's state and has
/// the Go stack size, as a checker, bind, emit or search thread does
/// (`WorkerSeed`). Tests use it.
pub fn spawn_seeded_thread<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> std::thread::JoinHandle<R> {
    let seed = WorkerSeed::take();
    std::thread::Builder::new()
        .stack_size(crate::gostd::stack::max_stack_size())
        .spawn(move || {
            seed.install();
            f()
        })
        .unwrap_or_else(|err| crate::core::go_fatal_newosproc(&err))
}

// Go: compiler/checkerpool.go:40 newCheckerPoolWithTracing (the count)
fn checker_count() -> usize {
    let program = prog();
    let mut checker_count: i64 = 4;
    if single_threaded() {
        checker_count = 1;
    } else if let Some(count) = program.options.checkers {
        checker_count = i64::from(count);
    }
    checker_count
        .min(program.source_file_order().len() as i64)
        .min(256)
        .max(1) as usize
}

// Go: compiler/checkerpool.go:98 createCheckers
// PORT: binding runs first on this thread, so no worker binds and every
// worker starts from the same bound program and thread-local state.
// The file associations (#4313) come from the Go frontend program
// (`ls_program::get_checker_associations`), whose files are in
// `source_file_order` order. An alias resolver program has no frontend, so
// it keeps the round-robin order; Go gives it no checker pool.
fn create_checkers() -> CheckerPool {
    bind_all();
    let count = checker_count();
    let program = prog();
    let source_file_order = program.source_file_order();
    let associations = match go_frontend_program() {
        Some(np) => ls_program::get_checker_associations(&np, count),
        None => (0..source_file_order.len()).map(|i| i % count).collect(),
    };
    // One entry per file id up to the last program file.
    let len = source_file_order.iter().max().map_or(0, |&last| last + 1);
    let mut file_associations = vec![0; len];
    for (i, &file_index) in source_file_order.iter().enumerate() {
        file_associations[file_index] = associations[i];
    }
    drop(source_file_order);
    assert!(
        with_tables(|tables| tables.file_associations.set(file_associations).is_ok()),
        "checker pool made twice"
    );
    // PERF (perfplan4 R7): with `--singleThreaded` the one checker allocates
    // in the jemalloc arena of the loading thread, which waits for it. It
    // then reuses the pages that the parse and the bind freed, where it
    // faulted new ones in an arena of its own (spawn1 t1 and t4, stable
    // builds: query check -2.9% to -3.6%, hono check -1.9% to -2.4%). With
    // more checkers each keeps its own arena: they run at once.
    let arena = if single_threaded() {
        jemalloc_thread_arena()
    } else {
        None
    };
    let (workers, threads): (Vec<_>, Vec<_>) = (0..count)
        .map(|index| {
            let (sender, receiver) = std::sync::mpsc::channel::<Job>();
            let seed = WorkerSeed::take();
            let thread = std::thread::Builder::new()
                .name(format!("checker-{index}"))
                .stack_size(crate::gostd::stack::max_stack_size())
                .spawn(move || {
                    if let Some(arena) = arena {
                        set_jemalloc_thread_arena(arena);
                    }
                    seed.install();
                    let checker = Checker::new(index);
                    WORKER_CHECKER.with(|slot| *slot.borrow_mut() = Some(checker));
                    WORKER_INDEX.with(|slot| slot.set(Some(index)));
                    for job in receiver {
                        job();
                    }
                    // In a one-program process the queue closes only when
                    // the loading thread ends, after every job sent its
                    // result, so this runs once per checker, at the end of
                    // the process. Like Go, which never frees a checker, the
                    // checker and the synthetic nodes are not dropped:
                    // freeing the checker arenas at thread exit cost 0.83%
                    // of query CPU. `release_program` frees both first
                    // (`CheckerPool::stop`), so a released program does not
                    // leak them.
                    if !WORKER_RELEASED.with(std::cell::Cell::get) {
                        std::mem::forget(WORKER_CHECKER.with(|slot| slot.borrow_mut().take()));
                        forget_synthetic_nodes();
                    }
                })
                .unwrap_or_else(|err| crate::core::go_fatal_newosproc(&err));
            (sender, thread)
        })
        .unzip();
    CheckerPool {
        workers,
        threads,
        emit: None,
    }
}

/// PORT: not in Go (perf). Sends each checker thread of the current
/// program a job that borrows no checker and drops a value that `signal`
/// makes. It runs after the jobs sent to that thread before, so when every
/// value has dropped, those jobs are done; a value also drops when its job
/// can no longer run (the thread ended). Returns how many it sent: 0 when
/// the program has no checker pool on this thread. `tsc -b` learns this way
/// that the check and emit that a task started are done
/// (build/orchestrator.rs `build_all_tasks`).
pub fn send_checker_barrier<T: Send + 'static>(signal: impl Fn() -> T) -> usize {
    let id = prog().id;
    POOLS.with(|pools| {
        let pools = pools.borrow();
        let Some(pool) = pools.get(&id) else {
            return 0;
        };
        for worker in &pool.workers {
            let value = signal();
            worker
                .send(Box::new(move || drop(value)))
                .expect("checker thread stopped");
        }
        pool.workers.len()
    })
}

/// Starts `f` with checker `index` on its thread and returns where the
/// result arrives. Jobs for one checker run in the order they are sent.
fn send_job<R: Send + 'static>(
    index: usize,
    f: impl FnOnce(&mut Checker) -> R + Send + 'static,
) -> std::sync::mpsc::Receiver<JobResult<R>> {
    send_thread_job(index, move || with_checker_at(index, f))
}

/// `send_job` for work that borrows the checker itself (`with_checker_at`)
/// when it needs it.
fn send_thread_job<R: Send + 'static>(
    index: usize,
    f: impl FnOnce() -> R + Send + 'static,
) -> std::sync::mpsc::Receiver<JobResult<R>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let job: Job = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let _ = sender.send(result);
    });
    let id = prog().id;
    POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        let pool = pools.entry(id).or_insert_with(create_checkers);
        pool.workers[index]
            .send(job)
            .expect("checker thread stopped");
    });
    receiver
}

/// Waits for a job result. A panic in the job continues on this thread.
fn wait_job<R>(receiver: &std::sync::mpsc::Receiver<JobResult<R>>) -> R {
    match receiver.recv().expect("checker thread stopped") {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Waits for every job, then returns the results in order. The first
/// panic, in job order, continues on this thread after all jobs end.
fn wait_jobs<R>(receivers: Vec<std::sync::mpsc::Receiver<JobResult<R>>>) -> Vec<R> {
    let results: Vec<JobResult<R>> = receivers
        .iter()
        .map(|receiver| receiver.recv().expect("checker thread stopped"))
        .collect();
    results
        .into_iter()
        .map(|result| result.unwrap_or_else(|payload| std::panic::resume_unwind(payload)))
        .collect()
}

/// True once the checker pool of the current program exists.
/// `crate::tracing` dumps the checkers' types only then, so that stopping a
/// trace does not make the pool.
pub fn checker_pool_created() -> bool {
    let created = |tables: &VersionTables| tables.file_associations.get().is_some();
    let Some(program_state) = crate::core::try_prog().and_then(|program| program.state.get())
    else {
        return false;
    };
    match &program_state.tables {
        TablesSlot::Leaked(tables) => created(tables),
        TablesSlot::Version(slot) => slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            .is_some_and(created),
    }
}

/// The jemalloc arena of this thread (`thread.arena`), or None in a build
/// without jemalloc.
fn jemalloc_thread_arena() -> Option<u32> {
    #[cfg(all(feature = "jemalloc", not(windows)))]
    {
        use tikv_jemalloc_ctl::{Access, AsName};
        b"thread.arena\0".name().read().ok()
    }
    #[cfg(not(all(feature = "jemalloc", not(windows))))]
    None
}

/// Makes this thread allocate in jemalloc arena `arena` (`thread.arena`,
/// from `jemalloc_thread_arena`). A failure keeps its arena.
fn set_jemalloc_thread_arena(arena: u32) {
    #[cfg(all(feature = "jemalloc", not(windows)))]
    {
        use tikv_jemalloc_ctl::{Access, AsName};
        let _ = b"thread.arena\0".name().write(arena);
    }
    #[cfg(not(all(feature = "jemalloc", not(windows))))]
    let _ = arena;
}

/// The pool index of this thread's checker, or None off the worker threads.
fn worker_index() -> Option<usize> {
    WORKER_INDEX.with(std::cell::Cell::get)
}

/// The checker index of `file` (Go `fileAssociations[file]`).
fn checker_index_for_file(file: Node) -> usize {
    let index = |tables: &VersionTables| Some(tables.file_associations.get()?[file.file_index()]);
    if let Some(index) = with_tables(index) {
        return index;
    }
    let id = prog().id;
    POOLS.with(|pools| {
        pools.borrow_mut().entry(id).or_insert_with(create_checkers);
    });
    with_tables(index).expect("checker pool not made")
}

// Go: compiler/checkerpool.go:77 getCheckerForFileNonExclusive
// PORT: Go returns the checker and a release function. Here the checker is
// lent to `f` for the call, on the checker's own thread.
pub fn with_type_checker_for_file<R: Send + 'static>(
    file: Node,
    f: impl FnOnce(&mut Checker) -> R + Send + 'static,
) -> R {
    let index = checker_index_for_file(file);
    if worker_index().is_some() {
        return with_checker_at(index, f);
    }
    wait_job(&send_job(index, f))
}

/// A job sent to the checker thread of a file by `send_type_checker_job_for_file`.
/// `CheckerJob::Inline` holds the result when the caller is itself a checker
/// thread, where the job ran at once.
pub enum CheckerJob<R> {
    Sent(std::sync::mpsc::Receiver<JobResult<R>>),
    Inline(R),
}

impl<R> CheckerJob<R> {
    /// Waits for the result. A panic in the job continues on this thread.
    pub fn wait(self) -> R {
        match self {
            CheckerJob::Sent(receiver) => wait_job(&receiver),
            CheckerJob::Inline(value) => value,
        }
    }
}

/// `with_type_checker_for_file` without the wait: jobs for several files can
/// run on their checker threads at the same time (Go runs such loops in a
/// `WorkGroup`). Jobs for one checker still run in the order they are sent.
pub fn send_type_checker_job_for_file<R: Send + 'static>(
    file: Node,
    f: impl FnOnce(&mut Checker) -> R + Send + 'static,
) -> CheckerJob<R> {
    let index = checker_index_for_file(file);
    if worker_index().is_some() {
        return CheckerJob::Inline(with_checker_at(index, f));
    }
    CheckerJob::Sent(send_job(index, f))
}

// PORT: replaces EmitResolver.checkerMu. Lends checker `index` to `f`. Only
// the worker thread of that checker can do this; the checker is borrowed
// for the call, so `f` must not ask for it again.
pub fn with_checker_at<R>(index: usize, f: impl FnOnce(&mut Checker) -> R) -> R {
    let Some(worker) = worker_index() else {
        panic!("checker {index} used off its worker thread");
    };
    assert!(
        worker == index,
        "checker {index} used on the thread of checker {worker}"
    );
    WORKER_CHECKER.with(|slot| f(slot.borrow_mut().as_mut().expect("worker checker")))
}

// Go: compiler/checkerpool.go:123 forEachCheckerParallel
pub fn for_each_checker_parallel<R: Send + 'static>(cb: fn(usize, &mut Checker) -> R) -> Vec<R> {
    let count = checker_count();
    let receivers = (0..count)
        .map(|index| send_job(index, move |checker| cb(index, checker)))
        .collect();
    wait_jobs(receivers)
}

// Go: compiler/checkerpool.go:462 GetGlobalDiagnostics
fn pool_get_global_diagnostics() -> Vec<Diagnostic> {
    let receivers = (0..checker_count())
        .map(|index| send_job(index, |checker| checker.get_global_diagnostics()))
        .collect();
    sort_and_deduplicate_diagnostics(wait_jobs(receivers).into_iter().flatten().collect())
}

// Go: compiler/checkerpool.go:148 forEachCheckerGroupDo
// PORT: returns the results of `cb` by file position instead of passing the
// position to `cb`. A file with no result keeps an empty list.
fn for_each_checker_group_do(
    files: &[Node],
    cb: fn(&mut Checker, Node) -> Vec<Diagnostic>,
) -> Vec<Vec<Diagnostic>> {
    start_checker_group_do(files, cb).wait()
}

/// `for_each_checker_group_do` whose jobs are sent and not waited for yet.
// PORT: not in Go. A Go caller that does not wait runs the group on its
// own goroutine. Here the loading thread sends the jobs and reads the
// results later. Jobs for one checker run in the order they are sent, so
// the results are the same as with an immediate wait.
struct PendingCheckerGroup {
    files: Arc<Vec<Node>>,
    receivers: Vec<std::sync::mpsc::Receiver<JobResult<Vec<(usize, Vec<Diagnostic>)>>>>,
}

impl PendingCheckerGroup {
    /// Waits for every checker and returns the results of `cb` by file
    /// position (see `for_each_checker_group_do`).
    fn wait(self) -> Vec<Vec<Diagnostic>> {
        let mut diagnostics = vec![Vec::new(); self.files.len()];
        for (i, result) in wait_jobs(self.receivers).into_iter().flatten() {
            diagnostics[i] = result;
        }
        diagnostics
    }
}

/// The first half of `for_each_checker_group_do`: sends one job to each
/// checker of the current program and returns without waiting.
fn start_checker_group_do(
    files: &[Node],
    cb: fn(&mut Checker, Node) -> Vec<Diagnostic>,
) -> PendingCheckerGroup {
    let count = checker_count();
    let files: Arc<Vec<Node>> = Arc::new(files.to_vec());
    let receivers = (0..count)
        .map(|checker_index| {
            let files = Arc::clone(&files);
            send_job(checker_index, move |checker| {
                let mine: Vec<(usize, Node)> = with_tables(|tables| {
                    let associations = tables
                        .file_associations
                        .get()
                        .expect("checker pool not made");
                    files
                        .iter()
                        .copied()
                        .enumerate()
                        .filter(|(_, file)| associations[file.file_index()] == checker_index)
                        .collect()
                });
                mine.into_iter()
                    .map(|(i, file)| (i, cb(checker, file)))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    PendingCheckerGroup { files, receivers }
}

// ---------------------------------------------------------------------------
// Diagnostics (Go compiler/program.go)
// ---------------------------------------------------------------------------

// Go: compiler/program.go:534 collectDiagnostics
// PORT: the per-file work runs serially (see the checker pool note).
fn collect_diagnostics(
    file: Node,
    collect: &mut dyn FnMut(Node) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    let result = if file.is_some() {
        collect(file)
    } else {
        prog()
            .source_files()
            .flat_map(|f| collect(f.root))
            .collect()
    };
    // #4712
    filter_and_sort_diagnostics(result)
}

// Go: compiler/program.go:562 collectCheckerDiagnostics
/// Collects diagnostics for one file (or all files when `file` is nil) with
/// the checker that owns each file. The bin uses this to guard each file.
pub fn collect_checker_diagnostics_with(
    file: Node,
    collect: fn(&mut Checker, Node) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    if file.is_some() {
        if skip_type_checking(file, false) {
            return Vec::new();
        }
        let result = with_type_checker_for_file(file, move |c| collect(c, file));
        // #4712
        return filter_and_sort_diagnostics(result);
    }
    let files = source_files();
    let diagnostics = collect_checker_diagnostics_from_files(&files, collect);
    filter_and_sort_diagnostics(diagnostics.into_iter().flatten().collect())
}

// Go: compiler/program.go:708 filterAndSortDiagnostics (#4712)
fn filter_and_sort_diagnostics(mut diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diags.retain(|diag| {
        let file = diag.file;
        if !diag.reports_unnecessary || file.is_nil() || !diag.source().is_empty() {
            return true;
        }
        let Some(span_map) = source_file_span_map(file) else {
            return true;
        };
        let (_, fidelity) = crate::spanmap::SpanMap::virtual_to_original_span(
            Some(span_map),
            TextRange::new(diag.pos, diag.end),
        );
        fidelity != crate::spanmap::Fidelity::NONE
    });
    sort_and_deduplicate_diagnostics(diags)
}

// Go: compiler/program.go:576 collectCheckerDiagnosticsFromFiles
fn collect_checker_diagnostics_from_files(
    source_files: &[Node],
    collect: fn(&mut Checker, Node) -> Vec<Diagnostic>,
) -> Vec<Vec<Diagnostic>> {
    for_each_checker_group_do(source_files, collect)
}

// Go: compiler/program.go:599 GetSyntacticDiagnostics
pub fn get_syntactic_diagnostics(source_file: Node) -> Vec<Diagnostic> {
    let options = &prog().options;
    collect_diagnostics(source_file, &mut |file| {
        let info = source_file_info(file);
        let mut diags: Vec<Diagnostic> = info
            .diagnostics
            .iter()
            .chain(info.js_diagnostics.iter())
            .cloned()
            .collect();
        // For JS files that won't be checked by the checker (no checkJs/ts-check), we need
        // program-level syntactic checks that require compiler options. This mirrors Strada's
        // getJSSyntacticDiagnosticsForFile in program.ts.
        if is_source_file_js(file) && !is_check_js_enabled_for_file(file, options) {
            diags.extend(get_additional_js_syntactic_diagnostics(file, options));
        }
        diags
    })
}

// Go: compiler/program.go:618 getAdditionalJSSyntacticDiagnostics
fn get_additional_js_syntactic_diagnostics(
    file: Node,
    options: &CompilerOptions,
) -> Vec<Diagnostic> {
    if options.experimental_decorators.is_true() {
        return Vec::new();
    }
    let mut diags = Vec::new();
    // Parameter decorators are only valid with experimentalDecorators. Without it,
    // the checker would report this, but the checker doesn't run on unchecked JS files.
    fn walk(node: Node, file: Node, diags: &mut Vec<Diagnostic>) -> bool {
        if !node
            .subtree_facts()
            .intersects(SubtreeFacts::SUBTREE_CONTAINS_DECORATORS)
        {
            return false;
        }
        if node.kind() == SyntaxKind::Parameter && has_decorators(node) {
            if let Some(decorator) = node.modifier_nodes().into_iter().find(|n| is_decorator(*n)) {
                diags.push(new_diagnostic(
                    file,
                    decorator.loc(),
                    diag::Decorators_are_not_valid_here,
                    Vec::new(),
                ));
            }
        }
        node.for_each_child(|child| walk(child, file, diags));
        false
    }
    file.for_each_child(|child| walk(child, file, &mut diags));
    diags
}

// Go: compiler/program.go:643 GetBindDiagnostics
// PORT: Go binds one file when given one. The Rust binder binds every file
// into one shared arena, so this always binds all files.
pub fn get_bind_diagnostics(source_file: Node) -> Vec<Diagnostic> {
    bind_all();
    collect_diagnostics(source_file, &mut |file| {
        file_bind_data(file).bind_diagnostics.clone()
    })
}

// Go: compiler/program.go:654 GetSemanticDiagnostics
// PORT: the compile path has no context; Go tsc passes context.Background()
// (execute/tsc/emit.go:75). The same holds for the two functions below.
pub fn get_semantic_diagnostics(source_file: Node) -> Vec<Diagnostic> {
    collect_checker_diagnostics_with(source_file, |c, f| {
        get_semantic_diagnostics_with_checker(&context::background(), c, f)
    })
}

// Go: compiler/program.go:804 GetSemanticDiagnosticsForIncremental
// GetSemanticDiagnosticsForIncremental includes newly discovered globals in each
// file's cached diagnostics and leaves noEmit filtering to the builder.
pub fn get_semantic_diagnostics_for_incremental(
    source_files: &[Node],
) -> FxHashMap<Node, Vec<Diagnostic>> {
    start_semantic_diagnostics_for_incremental(source_files).wait()
}

/// A `get_semantic_diagnostics_for_incremental` check that runs on the
/// checker threads while the caller goes on.
pub struct PendingSemanticDiagnostics(PendingCheckerGroup);

impl PendingSemanticDiagnostics {
    /// The files that are checked, in the order they were given.
    #[must_use]
    pub fn files(&self) -> &[Node] {
        &self.0.files
    }

    /// Waits for the check. Same result as
    /// `get_semantic_diagnostics_for_incremental`.
    #[must_use]
    pub fn wait(self) -> FxHashMap<Node, Vec<Diagnostic>> {
        let files = Arc::clone(&self.0.files);
        files
            .iter()
            .zip(self.0.wait())
            // #4712
            .map(|(&file, diags)| (file, filter_and_sort_diagnostics(diags)))
            .collect()
    }
}

/// Sends the `get_semantic_diagnostics_for_incremental` check of
/// `source_files` to the checkers of the current program and returns
/// without waiting (Go `collectCheckerDiagnosticsFromFiles` with
/// `getBindAndCheckDiagnosticsWithChecker(.., true /*includeDeferredGlobals*/)`).
pub fn start_semantic_diagnostics_for_incremental(
    source_files: &[Node],
) -> PendingSemanticDiagnostics {
    PendingSemanticDiagnostics(start_checker_group_do(source_files, |c, f| {
        get_bind_and_check_diagnostics_with_checker(
            &context::background(),
            c,
            f,
            true, /*includeDeferredGlobals*/
        )
    }))
}

// Go: compiler/program.go:667 GetSuggestionDiagnostics
pub fn get_suggestion_diagnostics(source_file: Node) -> Vec<Diagnostic> {
    collect_checker_diagnostics_with(source_file, |c, f| {
        get_suggestion_diagnostics_with_checker(&context::background(), c, f)
    })
}

// Go: compiler/program.go:671 GetProgramDiagnostics
// PORT: an alias resolver program has no frontend program, so its list is
// empty. Go has no such method on the alias resolver.
pub fn get_program_diagnostics() -> Vec<Diagnostic> {
    let Some(go) = go_frontend() else {
        return Vec::new();
    };
    let mut diagnostics = go.program_diagnostics.clone();
    // #4712
    diagnostics.extend(content_mapper_diagnostics());
    diagnostics.extend(content_mapper_option_diagnostics());
    diagnostics.extend(
        go.include_processor
            .get_diagnostics(&go)
            .borrow_mut()
            .get_global_diagnostics(),
    );
    sort_and_deduplicate_diagnostics(diagnostics)
}

// Go: compiler/program.go:678 GetIncludeProcessorDiagnostics
// PORT: an alias resolver program has no include diagnostics.
pub fn get_include_processor_diagnostics(source_file: Node) -> Vec<Diagnostic> {
    if skip_type_checking(source_file, false) {
        return Vec::new();
    }
    let diagnostics = with_tables(|tables| match &tables.go {
        Some(go) => go.get_include_processor_diagnostics(source_file),
        None => Vec::new(),
    });
    let (filtered, _) = get_diagnostics_with_preceding_directives(source_file, diagnostics);
    filtered
}

// Go: compiler/program.go:686 SkipTypeChecking
pub fn skip_type_checking(source_file: Node, ignore_no_check: bool) -> bool {
    let options = &prog().options;
    let info = source_file_info(source_file);
    (!ignore_no_check && options.no_check.is_true())
        || options.skip_lib_check.is_true() && info.is_declaration_file
        || options.skip_default_lib_check.is_true() && is_source_file_default_library(&info.path)
        || is_source_from_project_reference(&info.path)
        || !can_include_bind_and_check_diagnostics(source_file)
}

// Go: compiler/program.go:694 canIncludeBindAndCheckDiagnostics
fn can_include_bind_and_check_diagnostics(source_file: Node) -> bool {
    let options = &prog().options;
    let info = source_file_info(source_file);
    if info.check_js_directive.is_some_and(|d| !d.enabled) {
        return false;
    }
    // #4712: no ScriptKindExternal.
    if info.script_kind == ScriptKind::TS || info.script_kind == ScriptKind::TSX {
        return true;
    }
    let is_js = info.script_kind == ScriptKind::JS || info.script_kind == ScriptKind::JSX;
    let is_check_js = is_js && is_check_js_enabled_for_file(source_file, options);
    let is_plain_js = is_plain_js_file(source_file, options.check_js);
    // By default, only type-check .ts, .tsx, plain JS, and checked JS
    // - plain JS: .js files with no // ts-check and checkJs: undefined
    // - check JS: .js files with either // ts-check or checkJs: true
    // #4712: no ScriptKindDeferred.
    is_plain_js || is_check_js
}

// Go: compiler/program.go:1455 GetGlobalDiagnostics
/// Sends one job to each checker of the current program and waits for them.
/// Each checker runs it after the jobs sent to it before, so the read sees
/// what those jobs added. Loading thread only.
pub fn get_global_diagnostics() -> Vec<Diagnostic> {
    if prog().source_file_order().is_empty() {
        return Vec::new();
    }
    pool_get_global_diagnostics()
}

// Go: compiler/program.go:1302 GetDeclarationDiagnostics
// PORT: each file's work runs on the thread of the file's checker, where its
// emit resolver can reach that checker. Files of different checkers run in
// parallel; the results merge in file order.
pub fn get_declaration_diagnostics(source_file: Node) -> Vec<Diagnostic> {
    let files = if source_file.is_some() {
        vec![source_file]
    } else {
        source_files()
    };
    if worker_index().is_some() {
        let result = files
            .into_iter()
            .flat_map(get_declaration_diagnostics_for_file)
            .collect();
        return sort_and_deduplicate_diagnostics(result);
    }
    let receivers = files
        .into_iter()
        .map(|file| {
            send_thread_job(checker_index_for_file(file), move || {
                get_declaration_diagnostics_for_file(file)
            })
        })
        .collect();
    sort_and_deduplicate_diagnostics(wait_jobs(receivers).into_iter().flatten().collect())
}

/// `get_declaration_diagnostics` for one file without the wait: the job runs
/// on the file's checker thread while the caller sends more (see
/// `CheckerJob`). `CheckerJob::wait` gives the diagnostics before
/// `sort_and_deduplicate_diagnostics`.
pub fn send_declaration_diagnostics_job(source_file: Node) -> CheckerJob<Vec<Diagnostic>> {
    if worker_index().is_some() {
        return CheckerJob::Inline(get_declaration_diagnostics_for_file(source_file));
    }
    CheckerJob::Sent(send_thread_job(
        checker_index_for_file(source_file),
        move || get_declaration_diagnostics_for_file(source_file),
    ))
}

// Go: compiler/program.go:1394 getDeclarationDiagnosticsForFile
fn get_declaration_diagnostics_for_file(source_file: Node) -> Vec<Diagnostic> {
    if source_file_info(source_file).is_declaration_file {
        return Vec::new();
    }

    if let Some(cached) =
        with_declaration_diagnostic_cache(|cache| cache.get(&source_file).cloned())
    {
        return cached;
    }

    let host = new_emit_host(source_file);
    let diagnostics = get_declaration_diagnostics_worker(host, source_file);
    // Go `LoadOrStore`: keep the first stored value.
    with_declaration_diagnostic_cache(|cache| {
        cache.entry(source_file).or_insert(diagnostics).clone()
    })
}

/// Runs `f` with Go `Program.declarationDiagnosticCache`, locked.
fn with_declaration_diagnostic_cache<R>(
    f: impl FnOnce(&mut FxHashMap<Node, Vec<Diagnostic>>) -> R,
) -> R {
    with_tables(|tables| {
        f(&mut tables
            .declaration_diagnostic_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    })
}

// Go: compiler/emitter.go:506 getSourceFilesToEmit
// PORT: Go takes a `SourceFileMayBeEmittedHost`; the program functions are that host.
// Go `Program.getSourceFilesToEmit` caches the result for nil targets and no
// force; this computes it each time. Go nil `targetSourceFiles` is `None`.
pub(crate) fn get_source_files_to_emit(
    target_source_files: Option<&[Node]>,
    force_dts_emit: bool,
    force_js_emit: bool,
) -> Vec<Node> {
    let target_source_files = match target_source_files {
        Some(files) => files.to_vec(),
        None => source_files(),
    };
    target_source_files
        .into_iter()
        .filter(|&source_file| {
            source_file_may_be_emitted_worker(source_file, force_dts_emit, force_js_emit)
        })
        .collect()
}

// Go: compiler/emitter.go:518 isSourceFileNotJson
fn is_source_file_not_json(file: Node) -> bool {
    !is_json_source_file(file)
}

// Go: compiler/emitter.go:522 getDeclarationDiagnostics
// PORT: renamed from Go `getDeclarationDiagnostics`, because the exported
// `GetDeclarationDiagnostics` above already has the snake name.
fn get_declaration_diagnostics_worker(host: Rc<EmitHost>, file: Node) -> Vec<Diagnostic> {
    // TODO: use p.getSourceFilesToEmit cache
    // Go `core.SingleElementSlice(file)`: nil for a nil file.
    let target = [file];
    let target_source_files = file.is_some().then_some(&target[..]);
    let full_files: Vec<Node> = get_source_files_to_emit(target_source_files, false, false)
        .into_iter()
        .filter(|&f| is_source_file_not_json(f))
        .collect();
    if !full_files.iter().any(|&f| f == file) {
        return Vec::new();
    }
    // PORT: Go calls host.Options(), which returns host.program.Options()
    // (emitHost.go:107). The trait method borrows `host`, but the transformer
    // needs `&'static`, so read the program options directly.
    let options = options();
    let mut transform =
        crate::declarations::new_declaration_transformer(host.clone(), None, options, "", "");
    transform.transform_source_file_root(file);
    transform.get_diagnostics()
}

// Go: compiler/emitHost.go:33 emitHost
// NOTE: emitHost operations must be thread-safe
pub struct EmitHost {
    emit_resolver: Rc<dyn crate::printer::EmitResolver>,
}

// Go: compiler/emitHost.go:38 newEmitHost
// PORT: Go gets the file's checker and a `done` func that releases it. The
// checker is lent only for the `GetEmitResolver` call here; the resolver
// must reach its checker itself. Call it on the thread of the file's checker.
pub fn new_emit_host(file: Node) -> Rc<EmitHost> {
    let checker_index = checker_index_for_file(file);
    let emit_resolver: Rc<dyn crate::printer::EmitResolver> =
        with_checker_at(checker_index, Checker::get_emit_resolver);
    Rc::new(EmitHost { emit_resolver })
}

/// PORT: not in Go. The emit host of a JS part on the emit pool
/// (`send_emit_pool_jobs`), which has no checker. Its emit resolver panics
/// on every call (`emitter::no_checker`).
pub fn new_emit_host_without_checker() -> Rc<EmitHost> {
    Rc::new(EmitHost {
        emit_resolver: Rc::new(crate::emitter::no_checker::NoCheckerEmitResolver),
    })
}

impl EmitHost {
    /// Go `host.GetEmitResolver()` without the trait object.
    #[must_use]
    pub fn emit_resolver(&self) -> Rc<dyn crate::printer::EmitResolver> {
        self.emit_resolver.clone()
    }
}

impl crate::frontend::outputpaths::OutputPathsHost for EmitHost {
    // Go: compiler/emitHost.go:110 emitHost.CommonSourceDirectory
    fn common_source_directory(&self) -> String {
        common_source_directory().to_string()
    }

    // Go: compiler/emitHost.go:112 emitHost.ContentMapperExtensions (#4712)
    fn content_mapper_extensions(&self) -> Vec<String> {
        content_mapper_extensions()
    }

    // Go: compiler/emitHost.go:109 emitHost.GetCurrentDirectory
    fn get_current_directory(&self) -> String {
        get_current_directory().to_string()
    }

    // Go: compiler/emitHost.go:112 emitHost.UseCaseSensitiveFileNames
    fn use_case_sensitive_file_names(&self) -> bool {
        use_case_sensitive_file_names()
    }
}

// PORT: Go `emitHost` also implements `modulespecifiers.ModuleSpecifierGenerationHost`
// (GetModeForUsageLocation, GetResolvedModuleFromModuleSpecifier,
// GetDefaultResolutionModeForFile, FileExists, GetGlobalTypingsCacheLocation,
// GetNearestAncestorDirectoryWithPackageJson, GetPackageJsonInfo,
// GetSourceOfProjectReferenceIfOutputIncluded, GetProjectReferenceFromSource,
// GetRedirectTargets, GetSymlinkCache, ResolveModuleName). That interface is
// not ported, so those methods are left out until it is.
impl crate::declarations::DeclarationEmitHost for EmitHost {
    // Go: compiler/emitHost.go:109 emitHost.GetCurrentDirectory
    fn get_current_directory(&self) -> String {
        get_current_directory().to_string()
    }

    // Go: compiler/emitHost.go:112 emitHost.UseCaseSensitiveFileNames
    fn use_case_sensitive_file_names(&self) -> bool {
        use_case_sensitive_file_names()
    }

    // Go: compiler/emitHost.go:103 emitHost.GetSourceFileFromReference
    fn get_source_file_from_reference(&self, origin: Node, r#ref: &FileReference) -> Node {
        get_source_file_from_reference(origin, r#ref)
    }

    // Go: compiler/emitHost.go:94 emitHost.GetOutputPathsFor
    fn get_output_paths_for(
        &self,
        file: Node,
        force_dts_paths: bool,
    ) -> Box<dyn crate::declarations::OutputPaths> {
        // TODO: cache
        Box::new(get_output_paths_for_source_file(
            file,
            self,
            // #4699: Go `outputpaths.ForceEmitPaths{Dts: forceDtsPaths}`.
            crate::frontend::outputpaths::ForceEmitPaths {
                dts: force_dts_paths,
                js: false,
                declaration_map: false,
            },
        ))
    }

    // Go: compiler/emitHost.go:99 emitHost.SourceFileMayBeEmitted (#4712)
    fn source_file_may_be_emitted(&self, file: Node, force_dts_emit: bool) -> bool {
        source_file_may_be_emitted_worker(file, force_dts_emit, false)
    }

    // Go: compiler/emitHost.go:90 emitHost.GetEffectiveDeclarationFlags
    fn get_effective_declaration_flags(&self, node: Node, flags: ModifierFlags) -> ModifierFlags {
        self.emit_resolver
            .get_effective_declaration_flags(node, flags)
    }

    // Go: compiler/emitHost.go:124 emitHost.GetEmitResolver
    fn get_emit_resolver(&self) -> Rc<dyn crate::printer::EmitResolver> {
        self.emit_resolver.clone()
    }
}

impl crate::printer::EmitHost for EmitHost {
    // Go: compiler/emitHost.go:107 emitHost.Options
    fn options(&self) -> &CompilerOptions {
        options()
    }

    // Go: compiler/emitHost.go:108 emitHost.SourceFiles
    fn source_files(&self) -> Vec<Node> {
        source_files()
    }

    // Go: compiler/emitHost.go:112 emitHost.UseCaseSensitiveFileNames
    fn use_case_sensitive_file_names(&self) -> bool {
        use_case_sensitive_file_names()
    }

    // Go: compiler/emitHost.go:109 emitHost.GetCurrentDirectory
    fn get_current_directory(&self) -> String {
        get_current_directory().to_string()
    }

    // Go: compiler/emitHost.go:110 emitHost.CommonSourceDirectory
    fn common_source_directory(&self) -> String {
        common_source_directory().to_string()
    }

    // Go: compiler/emitHost.go:116 emitHost.IsEmitBlocked
    fn is_emit_blocked(&self, file: &str) -> bool {
        is_emit_blocked(file)
    }

    // Go: compiler/emitHost.go:120 emitHost.WriteFile
    // PORT: Go writes through the program host file system. Here the emit
    // caller always passes `EmitOptions.write_file`; without one the
    // write fails instead of touching the disk.
    fn write_file(&self, file_name: &str, _text: &str) -> Result<(), String> {
        Err(format!("no WriteFile callback for {file_name}"))
    }

    // Go: compiler/emitHost.go:58 emitHost.GetEmitModuleFormatOfFile
    fn get_emit_module_format_of_file(&self, file: Node) -> ModuleKind {
        get_emit_module_format_of_file(crate::emitter::emitter::parsed_source_file(file))
    }

    // Go: compiler/emitHost.go:124 emitHost.GetEmitResolver
    fn get_emit_resolver(&self) -> Rc<dyn crate::printer::EmitResolver> {
        self.emit_resolver.clone()
    }

    // Go: compiler/emitHost.go:128 emitHost.IsSourceFileFromExternalLibrary
    fn is_source_file_from_external_library(&self, file: Node) -> bool {
        is_source_file_from_external_library(file)
    }
}

// Go: compiler/program.go:1306 FilterNoEmitSemanticDiagnostics
pub fn filter_no_emit_semantic_diagnostics(
    mut diagnostics: Vec<Diagnostic>,
    options: &CompilerOptions,
) -> Vec<Diagnostic> {
    if !options.no_emit.is_true() {
        return diagnostics;
    }
    diagnostics.retain(|d| !d.skipped_on_no_emit());
    diagnostics
}

// Go: compiler/program.go:1480 getSemanticDiagnosticsWithChecker
pub fn get_semantic_diagnostics_with_checker(
    ctx: &Context,
    c: &mut Checker,
    source_file: Node,
) -> Vec<Diagnostic> {
    let mut diags = filter_no_emit_semantic_diagnostics(
        get_bind_and_check_diagnostics_with_checker(
            ctx,
            c,
            source_file,
            false, /*includeDeferredGlobals*/
        ),
        &prog().options,
    );
    diags.extend(get_include_processor_diagnostics(source_file));
    diags
}

// Go: compiler/program.go:1490 getBindAndCheckDiagnosticsWithChecker
// getBindAndCheckDiagnosticsWithChecker gets semantic diagnostics for a single file using a
// caller-provided checker, including bind diagnostics, checker diagnostics, and handling
// of @ts-ignore/@ts-expect-error directives.
pub fn get_bind_and_check_diagnostics_with_checker(
    ctx: &Context,
    file_checker: &mut Checker,
    source_file: Node,
    include_deferred_globals: bool,
) -> Vec<Diagnostic> {
    let compiler_options = &prog().options;
    if skip_type_checking(source_file, false) {
        return Vec::new();
    }
    let previous_globals = if include_deferred_globals {
        file_checker.get_global_diagnostics()
    } else {
        Vec::new()
    };

    // Checker creation forces binding, so bind diagnostics will be populated.
    bind_all();
    let mut diags = file_bind_data(source_file).bind_diagnostics.clone();
    diags.extend(file_checker.get_diagnostics_exported(ctx, source_file));

    if include_deferred_globals {
        if file_checker.was_canceled() {
            return Vec::new();
        }
        let current_globals = file_checker.get_global_diagnostics();
        if current_globals.len() > previous_globals.len() {
            for diagnostic in current_globals {
                let (_, found) = crate::gostd::slices::binary_search_func(
                    &previous_globals,
                    &diagnostic,
                    |previous: &Diagnostic, diagnostic: &&Diagnostic| {
                        compare_diagnostics(previous, diagnostic)
                    },
                );
                if !found {
                    diags.push(diagnostic);
                }
            }
        }
    }

    let is_plain_js = is_plain_js_file(source_file, compiler_options.check_js);
    if is_plain_js {
        diags.retain(|d| is_plain_js_error(d.code));
        return diags;
    }

    let info = source_file_info(source_file);
    let is_js = info.script_kind == ScriptKind::JS || info.script_kind == ScriptKind::JSX;
    let is_check_js = is_js && is_check_js_enabled_for_file(source_file, compiler_options);
    if is_check_js {
        diags.extend(info.jsdoc_diagnostics.iter().cloned());
    }

    let (mut filtered, directives_by_line) =
        get_diagnostics_with_preceding_directives(source_file, diags);
    for directive in directives_by_line.values() {
        // Above we changed all used directive kinds to @ts-ignore, so any @ts-expect-error directives that
        // remain are unused and thus errors.
        if directive.kind == CommentDirectiveKind::EXPECT_ERROR {
            filtered.push(new_diagnostic(
                source_file,
                directive.loc,
                diag::Unused_ts_expect_error_directive,
                Vec::new(),
            ));
        }
    }
    // #4712
    apply_content_mapper_diagnostic_directives(source_file, filtered)
}

// Go: compiler/program.go:1543 applyContentMapperDiagnosticDirectives (#4712)
fn apply_content_mapper_diagnostic_directives(
    source_file: Node,
    diags: Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    let directives = source_file_diagnostic_directives(source_file);
    if directives.is_empty() {
        return diags;
    }
    let mut used = vec![false; directives.len()];
    let mut mark_used = |diag: &Diagnostic| -> bool {
        if diag.file != source_file || !diag.source().is_empty() {
            return false;
        }
        for (i, directive) in directives.iter().enumerate() {
            if diag.pos >= directive.virtual_range.pos() && diag.pos < directive.virtual_range.end()
            {
                used[i] = true;
                return true;
            }
        }
        false
    };
    let mut filtered: Vec<Diagnostic> = diags.into_iter().filter(|diag| !mark_used(diag)).collect();
    for (i, directive) in directives.iter().enumerate() {
        if directive.policy == crate::ast::MappedDiagnosticDirectivePolicy::EXPECT && !used[i] {
            filtered.push(crate::ast::new_external_diagnostic(
                source_file,
                directive.original_range,
                &directive.source,
                crate::diagnostics::Category::Error,
                directive.unused_code,
                &directive.unused_message_text,
            ));
        }
    }
    filtered
}

// Go: compiler/program.go:1579 getDiagnosticsWithPrecedingDirectives
// PORT: Go returns a map by line; its iteration order is random and the
// caller sorts the result later. A BTreeMap gives a fixed order.
fn get_diagnostics_with_preceding_directives(
    source_file: Node,
    diags: Vec<Diagnostic>,
) -> (
    Vec<Diagnostic>,
    std::collections::BTreeMap<i32, CommentDirective>,
) {
    let mut directives_by_line = std::collections::BTreeMap::new();
    let info = source_file_info(source_file);
    if info.comment_directives.is_empty() {
        return (diags, directives_by_line);
    }
    // Build map of directives by line number
    for directive in &info.comment_directives {
        let line = get_ecma_line_of_position(source_file, directive.loc.pos());
        directives_by_line.insert(line, *directive);
    }
    let line_starts = &*get_ecma_line_starts(source_file);
    let text = source_file_text(source_file);
    let mut filtered = Vec::with_capacity(diags.len());
    for diagnostic in diags {
        let mut ignore_diagnostic = false;
        if diagnostic.file != source_file {
            filtered.push(diagnostic);
            continue;
        }
        let mut line = compute_line_of_position(line_starts, diagnostic.pos) - 1;
        while line >= 0 {
            // If line contains a @ts-ignore or @ts-expect-error directive, ignore this diagnostic and change
            // the directive kind to @ts-ignore to indicate it was used.
            if let Some(directive) = directives_by_line.get_mut(&line) {
                ignore_diagnostic = true;
                directive.kind = CommentDirectiveKind::IGNORE;
                break;
            }
            // Stop searching backwards when we encounter a line that isn't blank or a comment.
            if !is_comment_or_blank_line(&text, line_starts[line as usize] as usize) {
                break;
            }
            line -= 1;
        }
        if !ignore_diagnostic {
            filtered.push(diagnostic);
        }
    }
    (filtered, directives_by_line)
}

// Go: compiler/program.go:1410 getSuggestionDiagnosticsWithChecker
fn get_suggestion_diagnostics_with_checker(
    ctx: &Context,
    file_checker: &mut Checker,
    source_file: Node,
) -> Vec<Diagnostic> {
    if skip_type_checking(source_file, false) {
        return Vec::new();
    }

    // #4776: no bind suggestion diagnostics.
    file_checker.get_suggestion_diagnostics(ctx, source_file)
}

// Go: compiler/program.go:1422 isCommentOrBlankLine
fn is_comment_or_blank_line(text: &str, mut pos: usize) -> bool {
    let text = text.as_bytes();
    while pos < text.len() && (text[pos] == b' ' || text[pos] == b'\t') {
        pos += 1;
    }
    pos == text.len()
        || pos < text.len() && (text[pos] == b'\r' || text[pos] == b'\n')
        || pos + 1 < text.len() && text[pos] == b'/' && text[pos + 1] == b'/'
}

// Go: compiler/program.go:1431 SortAndDeduplicateDiagnostics
pub fn sort_and_deduplicate_diagnostics(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    // Go: compiler/program.go:1599 slices.SortFunc(diagnostics, ast.CompareDiagnostics)
    crate::gostd::slices::sort_func(&mut diagnostics, compare_diagnostics);
    compact_and_merge_related_infos(diagnostics)
}

// Go: compiler/program.go:1439 compactAndMergeRelatedInfos
// Remove duplicate diagnostics and, for sequences of diagnostics that differ only by related information,
// create a single diagnostic with sorted and deduplicated related information.
fn compact_and_merge_related_infos(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    if diagnostics.len() < 2 {
        return diagnostics;
    }
    let mut result = Vec::with_capacity(diagnostics.len());
    let mut i = 0;
    while i < diagnostics.len() {
        let d = &diagnostics[i];
        let mut n = 1;
        while i + n < diagnostics.len() && equal_diagnostics_no_related_info(d, &diagnostics[i + n])
        {
            n += 1;
        }
        let mut merged = d.clone();
        if n > 1 {
            let mut related_infos: Vec<Diagnostic> = diagnostics[i..i + n]
                .iter()
                .flat_map(|x| x.related_information.iter().cloned())
                .collect();
            // PORT: Go tests `relatedInfos != nil`; appending empty slices
            // keeps it nil, so an empty list means "leave d alone".
            if !related_infos.is_empty() {
                // Go: compiler/program.go:1623 slices.SortFunc(relatedInfos, ast.CompareDiagnostics)
                crate::gostd::slices::sort_func(&mut related_infos, compare_diagnostics);
                related_infos.dedup_by(|b, a| equal_diagnostics(a, b));
                merged.set_related_info(related_infos);
            }
        }
        result.push(merged);
        i += n;
    }
    result
}

// Go: compiler/program.go:1470 LineCount
pub fn line_count() -> i32 {
    let mut count = 0;
    for file in prog().source_files() {
        count += get_ecma_line_starts(file.root).len() as i32;
    }
    count
}

// Go: compiler/program.go:1478 IdentifierCount
pub fn identifier_count() -> i32 {
    let go = go_frontend().expect("identifier count of an alias resolver program");
    let mut count = 0;
    for file in go.source_files() {
        count += file.identifier_count;
    }
    count
}

// Go: compiler/program.go:1486 SymbolCount
// PORT: an unbound file (the program had syntactic errors) has the Go zero
// value.
pub fn symbol_count() -> i32 {
    let mut count: u32 = 0;
    for file in prog().source_files() {
        count += file
            .file_bind
            .get()
            .map_or(0, |data| data.symbol_count as u32);
    }
    for value in for_each_checker_parallel(|_, c| c.symbol_count) {
        count = count.wrapping_add(value);
    }
    count as i32
}

// Go: compiler/program.go:1499 TypeCount
pub fn type_count() -> i32 {
    let mut val: u32 = 0;
    for value in for_each_checker_parallel(|_, c| c.type_count) {
        val = val.wrapping_add(value);
    }
    val as i32
}

// Go: compiler/program.go:1507 InstantiationCount
pub fn instantiation_count() -> i32 {
    let mut val: u32 = 0;
    for value in for_each_checker_parallel(|_, c| c.total_instantiation_count) {
        val = val.wrapping_add(value);
    }
    val as i32
}

// Go: compiler/program.go:2010 GetDiagnosticsOfAnyProgram
// PORT: Go calls `program.GetGlobalDiagnostics` and
// `program.GetDeclarationDiagnostics` directly. They are callbacks here so a
// caller can guard them the same way as the bind and semantic callbacks.
// Go nil `files` is `None` (#4699). `is_compiler_program` is the Go type
// assertion `program.(*Program)`: true for a plain program, false for an
// incremental one.
pub fn get_diagnostics_of_any_program(
    files: Option<&[Node]>,
    skip_no_emit_check_for_dts_diagnostics: bool,
    get_bind_diagnostics: &mut dyn FnMut(Node) -> Vec<Diagnostic>,
    get_semantic_diagnostics: &mut dyn FnMut(Node) -> Vec<Diagnostic>,
    get_global_diagnostics: &mut dyn FnMut() -> Vec<Diagnostic>,
    get_declaration_diagnostics: &mut dyn FnMut(Node) -> Vec<Diagnostic>,
    is_compiler_program: bool,
) -> Vec<Diagnostic> {
    // Go `appendDiagnosticsForAllFiles` (a closure over `files`).
    fn append_diagnostics_for_all_files(
        files: Option<&[Node]>,
        diagnostics: &mut Vec<Diagnostic>,
        get_diagnostics: &mut dyn FnMut(Node) -> Vec<Diagnostic>,
    ) {
        match files {
            None => diagnostics.extend(get_diagnostics(Node::NIL)),
            Some(files) => {
                for &file in files {
                    diagnostics.extend(get_diagnostics(file));
                }
            }
        }
    }

    let options = &prog().options;
    let mut all_diagnostics = get_config_file_parsing_diagnostics();
    let config_file_parsing_diagnostics_length = all_diagnostics.len();

    // #4712
    let mut syntactic_diagnostics = Vec::new();
    append_diagnostics_for_all_files(
        files,
        &mut syntactic_diagnostics,
        &mut get_syntactic_diagnostics,
    );
    if !syntactic_diagnostics.is_empty() {
        // Per-file content mapper failures are syntactic diagnostics, but the locationless diagnostic
        // that disables a repeatedly failing mapper must still be reported.
        all_diagnostics.extend(content_mapper_diagnostics());
    }
    all_diagnostics.extend(syntactic_diagnostics);

    // If we didn't have any syntactic errors, then also try getting the program (options),
    // global and semantic errors.
    if all_diagnostics.len() == config_file_parsing_diagnostics_length {
        all_diagnostics.extend(get_program_diagnostics());

        // Do binding early so we can track the time.
        append_diagnostics_for_all_files(files, &mut Vec::new(), get_bind_diagnostics);

        if options.list_files_only.is_false_or_unknown() {
            all_diagnostics.extend(get_global_diagnostics());

            if all_diagnostics.len() == config_file_parsing_diagnostics_length {
                append_diagnostics_for_all_files(
                    files,
                    &mut all_diagnostics,
                    get_semantic_diagnostics,
                );
                if is_compiler_program {
                    // Incremental programs cache checking globals with file diagnostics;
                    // a late sweep would also collect incidental signature-generation globals.
                    all_diagnostics.extend(get_global_diagnostics());
                }
            }

            if (skip_no_emit_check_for_dts_diagnostics || options.no_emit.is_true())
                && options.get_emit_declarations()
                && all_diagnostics.len() == config_file_parsing_diagnostics_length
            {
                append_diagnostics_for_all_files(
                    files,
                    &mut all_diagnostics,
                    get_declaration_diagnostics,
                );
            }
        }
    }
    all_diagnostics
}

// Go: compiler/program.go plainJSErrors
// PORT: built on each call from the generated message statics (a static set
// cannot read them at compile time). It is only used for plain JS files.
fn is_plain_js_error(code: i32) -> bool {
    let messages: [&'static crate::diagnostics::Message; 91] = [
        // binder errors
        diag::Cannot_redeclare_block_scoped_variable_0,
        diag::A_module_cannot_have_multiple_default_exports,
        diag::Another_export_default_is_here,
        diag::The_first_export_default_is_here,
        diag::Identifier_expected_0_is_a_reserved_word_at_the_top_level_of_a_module,
        diag::Identifier_expected_0_is_a_reserved_word_in_strict_mode_Modules_are_automatically_in_strict_mode,
        diag::Identifier_expected_0_is_a_reserved_word_that_cannot_be_used_here,
        diag::X_constructor_is_a_reserved_word,
        diag::X_delete_cannot_be_called_on_an_identifier_in_strict_mode,
        diag::Code_contained_in_a_class_is_evaluated_in_JavaScript_s_strict_mode_which_does_not_allow_this_use_of_0_For_more_information_see_https_Colon_Slash_Slashdeveloper_mozilla_org_Slashen_US_Slashdocs_SlashWeb_SlashJavaScript_SlashReference_SlashStrict_mode,
        diag::Invalid_use_of_0_Modules_are_automatically_in_strict_mode,
        diag::Invalid_use_of_0_in_strict_mode,
        diag::A_label_is_not_allowed_here,
        diag::X_with_statements_are_not_allowed_in_strict_mode,
        // grammar errors
        diag::A_break_statement_can_only_be_used_within_an_enclosing_iteration_or_switch_statement,
        diag::A_break_statement_can_only_jump_to_a_label_of_an_enclosing_statement,
        diag::A_class_declaration_without_the_default_modifier_must_have_a_name,
        diag::A_class_member_cannot_have_the_0_keyword,
        diag::A_comma_expression_is_not_allowed_in_a_computed_property_name,
        diag::A_continue_statement_can_only_be_used_within_an_enclosing_iteration_statement,
        diag::A_continue_statement_can_only_jump_to_a_label_of_an_enclosing_iteration_statement,
        diag::A_default_clause_cannot_appear_more_than_once_in_a_switch_statement,
        diag::A_default_export_must_be_at_the_top_level_of_a_file_or_module_declaration,
        diag::A_definite_assignment_assertion_is_not_permitted_in_this_context,
        diag::A_destructuring_declaration_must_have_an_initializer,
        diag::A_get_accessor_cannot_have_parameters,
        diag::A_rest_element_cannot_contain_a_binding_pattern,
        diag::A_rest_element_cannot_have_a_property_name,
        diag::A_rest_element_cannot_have_an_initializer,
        diag::A_rest_element_must_be_last_in_a_destructuring_pattern,
        diag::A_rest_parameter_cannot_have_an_initializer,
        diag::A_rest_parameter_must_be_last_in_a_parameter_list,
        diag::A_rest_parameter_or_binding_pattern_may_not_have_a_trailing_comma,
        diag::A_return_statement_cannot_be_used_inside_a_class_static_block,
        diag::A_set_accessor_cannot_have_rest_parameter,
        diag::A_set_accessor_must_have_exactly_one_parameter,
        diag::An_export_declaration_can_only_be_used_at_the_top_level_of_a_module,
        diag::An_export_declaration_cannot_have_modifiers,
        diag::An_import_declaration_can_only_be_used_at_the_top_level_of_a_module,
        diag::An_import_declaration_cannot_have_modifiers,
        diag::An_object_member_cannot_be_declared_optional,
        diag::Argument_of_dynamic_import_cannot_be_spread_element,
        diag::Cannot_assign_to_private_method_0_Private_methods_are_not_writable,
        diag::Cannot_redeclare_identifier_0_in_catch_clause,
        diag::Catch_clause_variable_cannot_have_an_initializer,
        diag::Class_decorators_can_t_be_used_with_static_private_identifier_Consider_removing_the_experimental_decorator,
        diag::Classes_can_only_extend_a_single_class,
        diag::Classes_may_not_have_a_field_named_constructor,
        diag::Did_you_mean_to_use_a_Colon_An_can_only_follow_a_property_name_when_the_containing_object_literal_is_part_of_a_destructuring_pattern,
        diag::Duplicate_label_0,
        diag::Dynamic_imports_can_only_accept_a_module_specifier_and_an_optional_set_of_attributes_as_arguments,
        diag::X_for_await_loops_cannot_be_used_inside_a_class_static_block,
        diag::JSX_attributes_must_only_be_assigned_a_non_empty_expression,
        diag::JSX_elements_cannot_have_multiple_attributes_with_the_same_name,
        diag::JSX_expressions_may_not_use_the_comma_operator_Did_you_mean_to_write_an_array,
        diag::JSX_property_access_expressions_cannot_include_JSX_namespace_names,
        diag::Jump_target_cannot_cross_function_boundary,
        diag::Line_terminator_not_permitted_before_arrow,
        diag::Modifiers_cannot_appear_here,
        diag::Only_a_single_variable_declaration_is_allowed_in_a_for_in_statement,
        diag::Only_a_single_variable_declaration_is_allowed_in_a_for_of_statement,
        diag::Private_identifiers_are_not_allowed_outside_class_bodies,
        diag::Private_identifiers_are_only_allowed_in_class_bodies_and_may_only_be_used_as_part_of_a_class_member_declaration_property_access_or_on_the_left_hand_side_of_an_in_expression,
        diag::Property_0_is_not_accessible_outside_class_1_because_it_has_a_private_identifier,
        diag::Tagged_template_expressions_are_not_permitted_in_an_optional_chain,
        diag::The_left_hand_side_of_a_for_of_statement_may_not_be_async,
        diag::The_variable_declaration_of_a_for_in_statement_cannot_have_an_initializer,
        diag::The_variable_declaration_of_a_for_of_statement_cannot_have_an_initializer,
        diag::Trailing_comma_not_allowed,
        diag::Variable_declaration_list_cannot_be_empty,
        diag::X_0_and_1_operations_cannot_be_mixed_without_parentheses,
        diag::X_0_expected,
        diag::X_0_is_not_a_valid_meta_property_for_keyword_1_Did_you_mean_2,
        diag::X_0_list_cannot_be_empty,
        diag::X_0_modifier_already_seen,
        diag::X_0_modifier_cannot_appear_on_a_constructor_declaration,
        diag::X_0_modifier_cannot_appear_on_a_module_or_namespace_element,
        diag::X_0_modifier_cannot_appear_on_a_parameter,
        diag::X_0_modifier_cannot_appear_on_class_elements_of_this_kind,
        diag::X_0_modifier_cannot_be_used_here,
        diag::X_0_modifier_must_precede_1_modifier,
        diag::X_0_declarations_can_only_be_declared_inside_a_block,
        diag::X_0_declarations_must_be_initialized,
        diag::X_extends_clause_already_seen,
        diag::X_let_is_not_allowed_to_be_used_as_a_name_in_let_or_const_declarations,
        diag::Class_constructor_may_not_be_a_generator,
        diag::Class_constructor_may_not_be_an_accessor,
        diag::X_await_expressions_are_only_allowed_within_async_functions_and_at_the_top_levels_of_modules,
        diag::X_await_using_statements_are_only_allowed_within_async_functions_and_at_the_top_levels_of_modules,
        diag::Private_field_0_must_be_declared_in_an_enclosing_class,
        // Type errors
        diag::This_condition_will_always_return_0_since_JavaScript_compares_objects_by_reference_not_value,
    ];
    messages.iter().any(|m| m.code() as i32 == code)
}

// ---------------------------------------------------------------------------
// Output (Go diagnosticwriter/diagnosticwriter.go, non-pretty)
// ---------------------------------------------------------------------------

// Go: diagnosticwriter/diagnosticwriter.go:571 WriteFormatDiagnostic
// PORT: Go writes to an io.Writer; this returns the text.
// PORT: Go wraps the diagnostic in `ASTDiagnostic`, whose `File` and `Pos`
// go through `resolve` (#4712): see `resolve_diagnostic_location`.
pub fn format_diagnostic(diagnostic: &Diagnostic) -> String {
    let mut output = String::new();
    if diagnostic.file.is_some() {
        let resolved = resolve_diagnostic_location(diagnostic);
        let (line, character) = if resolved.use_original {
            // Go `newOriginalTextFile`: the position is in the original text.
            ecma_line_and_utf16_character_of_text_position(
                &source_file_original_text(diagnostic.file),
                resolved.loc.pos(),
            )
        } else {
            get_ecma_line_and_utf16_character_of_position(diagnostic.file, resolved.loc.pos())
        };
        let file_name = &source_file_info(diagnostic.file).file_name;
        let compare_options = tspath::ComparePathsOptions {
            use_case_sensitive_file_names: use_case_sensitive_file_names(),
            current_directory: get_current_directory().to_string(),
        };
        output.push_str(&format!(
            "{}({},{}): ",
            tspath::convert_to_relative_path(file_name, &compare_options),
            line + 1,
            character + 1
        ));
    }
    output.push_str(&format!(
        "{} {}{}: ",
        diagnostic.category.name(),
        diagnostic_prefix(diagnostic),
        diagnostic.code
    ));
    write_flattened_diagnostic_message(&mut output, diagnostic, "\n");
    output.push('\n');
    output
}

// Go: diagnosticwriter/diagnosticwriter.go:83 resolvedLocation (#4712)
// resolvedLocation describes how a diagnostic on a content-mapped file should be reported.
struct ResolvedLocation {
    loc: TextRange,
    use_original: bool, // render against the file's original, untransformed text
    synthesized: bool,  // the range is in virtual code with no corresponding original location
}

// Go: diagnosticwriter/diagnosticwriter.go:97 (*ASTDiagnostic).resolve (#4712)
// resolve determines where and against which text a diagnostic should be reported. A content mapper's
// own diagnostics already carry original ranges. A compiler diagnostic on a content-mapped file has its
// virtual range mapped back to the original; if it falls entirely within synthesized code, there is no
// original location, so it is shown against the virtual text and flagged as synthesized.
fn resolve_diagnostic_location(d: &Diagnostic) -> ResolvedLocation {
    let loc = TextRange::new(d.pos, d.end);
    let mut resolved = ResolvedLocation {
        loc,
        use_original: false,
        synthesized: false,
    };
    if d.file.is_nil() {
        return resolved;
    }
    if !d.source().is_empty() {
        resolved.use_original = true;
    } else if let Some(span_map) = source_file_span_map(d.file) {
        let (mapped, fidelity) =
            crate::spanmap::SpanMap::virtual_to_original_span(Some(span_map), loc);
        if fidelity == crate::spanmap::Fidelity::NONE {
            resolved.synthesized = true;
        } else {
            resolved.loc = mapped;
            resolved.use_original = true;
        }
    }
    resolved
}

/// Go `scanner.GetECMALineAndUTF16CharacterOfPosition` on the Go
/// `originalTextFile` of a content-mapped file (#4712), whose line map is
/// `core.ComputeECMALineStarts` of its original text.
// PORT: `get_ecma_line_and_utf16_character_of_position` reads the text of a
// file node; the original text has no node, so this is the same code on
// the text.
fn ecma_line_and_utf16_character_of_text_position(text: &str, pos: i32) -> (i32, i32) {
    let line_map = compute_ecma_line_starts(text);
    let line = compute_line_of_position(&line_map, pos);
    let end = pos as usize;
    let mut boundary = end;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let character =
        utf16_len(&text[line_map[line as usize] as usize..boundary]) + (end - boundary) as i32;
    (line, character)
}

// Go: diagnosticwriter/diagnosticwriter.go:380 diagnosticPrefix (#4712)
// diagnosticPrefix returns the prefix shown before a diagnostic's code, e.g. "TS" for compiler
// diagnostics or a content mapper's custom source for its diagnostics.
fn diagnostic_prefix(diagnostic: &Diagnostic) -> &str {
    let source = diagnostic.source();
    if !source.is_empty() {
        return source;
    }
    "TS"
}

// Go: diagnosticwriter/diagnosticwriter.go:565 WriteFormatDiagnostics
pub fn write_format_diagnostics(output: &mut String, diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        output.push_str(&format_diagnostic(diagnostic));
    }
}

// Go: diagnosticwriter/diagnosticwriter.go:358 WriteFlattenedDiagnosticMessage
// PORT: this writer has no Go `FormattingOptions`, so the locale is
// Go `locale.Default` and the text is English (see execute/tsc/diagnostics.rs
// `write_format_diagnostic`).
fn write_flattened_diagnostic_message(writer: &mut String, diagnostic: &Diagnostic, newline: &str) {
    writer.push_str(&diagnostic.localize(&crate::locale::DEFAULT));
    for chain in ast_diagnostic_message_chain(diagnostic).iter() {
        flatten_diagnostic_message_chain(writer, chain, newline, 1);
    }
}

// Go: diagnosticwriter/diagnosticwriter.go:146 (*ASTDiagnostic).MessageChain
// PORT: Go wraps each chain entry in `ASTDiagnostic`; the entries are the
// diagnostics themselves here.
fn ast_diagnostic_message_chain(d: &Diagnostic) -> Cow<'_, [Diagnostic]> {
    // #4712
    if !resolve_diagnostic_location(d).synthesized {
        return Cow::Borrowed(&d.message_chain);
    }
    let mut result = Vec::with_capacity(d.message_chain.len() + 1);
    result.extend(d.message_chain.iter().cloned());
    // The diagnostic points into synthesized virtual code; make clear the shown location is not in the
    // original file, and which content mapper produced it.
    result.push(new_compiler_diagnostic(
        diag::This_location_is_in_virtual_code_produced_by_the_content_mapper_0_and_has_no_corresponding_location_in_the_original_file,
        args![source_file_content_mapper(d.file)],
    ));
    Cow::Owned(result)
}

// Go: diagnosticwriter/diagnosticwriter.go:366 flattenDiagnosticMessageChain
fn flatten_diagnostic_message_chain(
    writer: &mut String,
    chain: &Diagnostic,
    new_line: &str,
    level: usize,
) {
    writer.push_str(new_line);
    for _ in 0..level {
        writer.push_str("  ");
    }
    writer.push_str(&chain.localize(&crate::locale::DEFAULT));
    for child in ast_diagnostic_message_chain(chain).iter() {
        flatten_diagnostic_message_chain(writer, child, new_line, level + 1);
    }
}

// Emit support: run work on a file's checker thread without holding the checker.

/// Runs `f(file)` for each file on the thread of the file's checker, with
/// no checker borrowed, and returns the results in file order. The emit
/// resolver borrows its checker itself (`with_checker_at`), so emit runs
/// through this instead of `with_type_checker_for_file`.
pub fn run_on_checker_threads_for_files<R: Send + 'static>(
    files: &[Node],
    f: impl Fn(Node) -> R + Send + Sync + 'static,
) -> Vec<R> {
    send_on_checker_threads_for_files(files, f).wait()
}

/// `run_on_checker_threads_for_files` without the wait: sends `f(file)` for
/// each file to the thread of the file's checker and returns. Each checker
/// thread runs its jobs in the order they are sent, after the jobs sent to
/// it before. On a checker thread the jobs run here, at once.
pub fn send_on_checker_threads_for_files<R: Send + 'static>(
    files: &[Node],
    f: impl Fn(Node) -> R + Send + Sync + 'static,
) -> PendingCheckerJobs<R> {
    if worker_index().is_some() {
        return PendingCheckerJobs(
            files
                .iter()
                .map(|&file| CheckerJob::Inline(f(file)))
                .collect(),
        );
    }
    let f = Arc::new(f);
    PendingCheckerJobs(
        files
            .iter()
            .map(|&file| {
                let f = Arc::clone(&f);
                CheckerJob::Sent(send_thread_job(checker_index_for_file(file), move || {
                    f(file)
                }))
            })
            .collect(),
    )
}

/// The jobs of `send_on_checker_threads_for_files`, one per file.
pub struct PendingCheckerJobs<R>(Vec<CheckerJob<R>>);

impl<R> PendingCheckerJobs<R> {
    /// Waits for every job, then returns the results in file order. The
    /// first panic, in file order, continues on this thread after all jobs
    /// end (as `wait_jobs`).
    pub fn wait(self) -> Vec<R> {
        self.wait_all()
            .into_iter()
            .map(|result| result.unwrap_or_else(|payload| std::panic::resume_unwind(payload)))
            .collect()
    }

    /// `wait` that returns each job's result or the payload of its panic,
    /// in file order, and continues no panic.
    pub fn wait_all(self) -> Vec<std::thread::Result<R>> {
        self.0
            .into_iter()
            .map(|job| match job {
                CheckerJob::Sent(receiver) => receiver.recv().expect("checker thread stopped"),
                CheckerJob::Inline(value) => Ok(value),
            })
            .collect()
    }
}

/// The pool index of the checker for `file` (Go `fileAssociations[file]`).
pub fn checker_index_of_file(file: Node) -> usize {
    checker_index_for_file(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Options that differ only in the `paths` order are different values:
    // each program keeps its own order.
    #[test]
    fn interned_options_keep_the_paths_order() {
        let with_paths = |keys: [&str; 2]| CompilerOptions {
            config_file_path: "/interned_options_keep_the_paths_order/tsconfig.json".into(),
            paths: Some(
                keys.iter()
                    .map(|&key| (key.to_string(), Some(vec![format!("./{key}")])))
                    .collect(),
            ),
            ..CompilerOptions::default()
        };
        let a = with_paths(["a/*", "*"]);
        let b = with_paths(["*", "a/*"]);
        assert_eq!(a, b, "the derived == ignores the order");
        assert!(!a.deep_equal(&b));
        assert!(a.deep_equal(&a.clone()));

        let interned_a = intern_compiler_options(&a);
        let interned_b = intern_compiler_options(&b);
        assert!(!std::ptr::eq(interned_a, interned_b));
        let keys = |options: &CompilerOptions| {
            options
                .paths
                .as_ref()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(interned_a), ["a/*", "*"]);
        assert_eq!(keys(interned_b), ["*", "a/*"]);
        let again = intern_compiler_options(&b.clone());
        assert!(std::ptr::eq(again, interned_b));

        // The watch mode config check (Go `reflect.DeepEqual`) sees it too.
        use crate::frontend::tsoptions::parsed_options::ParsedOptions;
        let parsed = |options: CompilerOptions| ParsedOptions {
            compiler_options: std::rc::Rc::new(options),
            ..ParsedOptions::default()
        };
        assert!(parsed(a.clone()) != parsed(b));
        assert!(parsed(a.clone()) == parsed(a));
    }

    // Go: compiler/emitter.go:504-521. No Go test covers this part. The
    // expected values come from Go at pin B (16c25522e123): the same calls in
    // a Go test, and for the cases marked "oracle" also a `noEmit`
    // incremental tsgo run, where a JSON file that Go may emit is in
    // `affectedFilesPendingEmit`.
    #[test]
    fn json_file_may_be_emitted_matches_go() {
        // (file, outDir, rootDir, config file path, may be emitted)
        let cases = [
            // No outDir.
            ("/proj/src/data.json", "", "/proj/src", "", false),
            // No rootDir and no config file: the common directory is unknown.
            ("/proj/src/data.json", "/proj/dist", "", "", true),
            // oracle: under rootDir.
            ("/proj/src/data.json", "/proj/dist", "/proj/src", "", true),
            // oracle: outside rootDir, the output path is the file itself.
            ("/proj/data.json", "/proj/dist", "/proj/src", "", false),
            // oracle: outDir is rootDir.
            ("/proj/src/data.json", "/proj/src", "/proj/src", "", true),
            // oracle: outDir is the config file directory.
            (
                "/proj/src/data.json",
                "/proj",
                "",
                "/proj/tsconfig.json",
                true,
            ),
            // oracle: the path starts with rootDir, but not at a separator.
            (
                "/proj/src-data/data.json",
                "/proj/dist",
                "/proj/src",
                "",
                true,
            ),
            // A root keeps its separator.
            ("/data.json", "/", "/", "", false),
        ];
        for (file, out_dir, root_dir, config_file_path, expected) in cases {
            let options = CompilerOptions {
                out_dir: out_dir.to_string(),
                root_dir: root_dir.to_string(),
                config_file_path: config_file_path.to_string(),
                ..Default::default()
            };
            assert_eq!(
                json_file_may_be_emitted(file, &options, "/proj", true),
                expected,
                "{file} outDir {out_dir:?} rootDir {root_dir:?} config {config_file_path:?}"
            );
        }
    }

    #[test]
    fn json_file_may_be_emitted_case_insensitive() {
        let options = CompilerOptions {
            out_dir: "/proj/dist".to_string(),
            root_dir: "/proj/SRC".to_string(),
            ..Default::default()
        };
        assert!(json_file_may_be_emitted(
            "/proj/src/data.json",
            &options,
            "/proj",
            false
        ));
        assert!(!json_file_may_be_emitted(
            "/proj/src/data.json",
            &options,
            "/proj",
            true
        ));
    }
}
