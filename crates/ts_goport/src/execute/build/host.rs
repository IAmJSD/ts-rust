//! Go: execute/build/host.go, execute/build/compilerHost.go, and the
//! `ExtendedConfigCache` of execute/tsc/extendedconfigcache.go.
//!
//! PORT: Go `host` keeps a pointer to its `*Orchestrator` and reads
//! `opts.Sys`, `opts.Command` and `toPath` through it. Here the host keeps
//! those values itself, so it needs no reference back to the orchestrator.
//! The orchestrator owns the host as `Rc<BuildHost>` and passes clones
//! where Go passes `o.host`.
//!
//! PORT: Go `time.Time` is `Option<SystemTime>` (`None` = zero) and
//! `time.Duration` is `Duration`, as in build_task.rs.

use crate::contentmapper::{self, Mapper, Project, SourceFiles};
use crate::execute::build::command_line::ParsedBuildCommandLine;
use crate::execute::build::config_prefetch::ConfigPrefetch;
use crate::execute::build::orchestrator::MTimePrefetch;
use crate::execute::build::parse_cache::ParseCache;
use crate::execute::incremental::incremental;
use crate::execute::tsc::compile::System;
use crate::frontend::compiler::host::parse_source_file_text;
use crate::frontend::prelude::*;
use crate::gostd::GoError;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

// Go: tsc/extendedconfigcache.go:15 ExtendedConfigCache
// PORT: the Go type is `tsc.ExtendedConfigCache`; the Rust name adds `Tsc`
// because the `tsoptions.ExtendedConfigCache` interface already has the
// plain name. The per-entry mutex is dropped (one thread, see
// parse_cache.rs). The map borrow is not held while the config parses, so
// a nested `extends` can use the cache.
#[derive(Default)]
pub struct TscExtendedConfigCache {
    m: RefCell<FxHashMap<Path, Rc<ExtendedConfigCacheEntry>>>,
}

impl ExtendedConfigCache for TscExtendedConfigCache {
    // Go: tsc/extendedconfigcache.go:27 (*ExtendedConfigCache).GetExtendedConfig
    fn get_extended_config(
        &self,
        file_name: &str,
        path: &Path,
        resolution_stack: &[Path],
        host: &dyn ParseConfigHost,
    ) -> Rc<ExtendedConfigCacheEntry> {
        if let Some(entry) = self.m.borrow().get(path) {
            return entry.clone();
        }
        let entry = Rc::new(parse_extended_config(
            file_name,
            path.clone(),
            resolution_stack,
            host,
            Some(self),
        ));
        self.m
            .borrow_mut()
            .entry(path.clone())
            .or_insert(entry)
            .clone()
    }
}

impl TscExtendedConfigCache {
    // PORT: Go assigns a new cache (`o.host.extendedConfigCache =
    // tsc.ExtendedConfigCache{}`, orchestrator.go:276). The programs of a
    // build keep the host `Rc`, so the cache is emptied in place.
    pub fn reset(&self) {
        self.m.borrow_mut().clear();
    }
}

// PORT: Go keys the source file cache by `ast.SourceFileParseOptions`, a
// comparable struct. The Rust struct has no `Hash`, so this key hashes the
// same fields.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceFileCacheKey(pub SourceFileParseOptions);

impl Hash for SourceFileCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.file_name.hash(state);
        self.0.path.hash(state);
        self.0.external_module_indicator_options.jsx.hash(state);
        self.0.external_module_indicator_options.force.hash(state);
    }
}

// Go: vfs/cachedvfs/cachedvfs.go FS, the file system of the build host
// (`cachedvfs.From(sys.FS())`, orchestrator.go:1004).
// PORT: the same cache as `CachedFs` (always enabled: the build host
// never disables it), but the `FileExists`, `DirectoryExists`,
// `Realpath` and `GetAccessibleEntries` lookups live in a
// `BuildStatCache` that the parse workers of each program load read too
// (`CompilerHost::stat_cache`), as Go parse tasks share the host's
// cachedvfs. This cache lasts for the whole build, and a write does not
// update it (cachedvfs.go:144), so a later program can find a lookup here
// that an earlier program made before the build wrote that path. It holds
// only the lookups that Go makes: the workers' own lookups stay out of it
// unless the loader uses them (see `BuildStatCache`). The builder threads
// of a parallel build (builders.rs) each have a `BuildCachedFs` over the
// same caches.
pub struct BuildCachedFs {
    fs: Rc<dyn Fs>,
    stats: Arc<BuildStatCache>,
    stat_cache: StatResults,
}

/// The `Stat` results of a `BuildCachedFs`, by path.
type StatResults = Arc<Mutex<FxHashMap<String, Option<FileInfo>>>>;

impl BuildCachedFs {
    // Go: cachedvfs.go:24 From
    fn new(fs: Rc<dyn Fs>) -> BuildCachedFs {
        BuildCachedFs {
            fs,
            stats: Arc::default(),
            stat_cache: Arc::default(),
        }
    }

    // Go: cachedvfs.go:40 ClearCache
    pub fn clear_cache(&self) {
        self.stats.clear();
        lock(&self.stat_cache).clear();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fs for BuildCachedFs {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.use_case_sensitive_file_names()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.stats.file_exists(path, || self.fs.file_exists(path))
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
        a_time: Option<SystemTime>,
        m_time: Option<SystemTime>,
    ) -> Result<(), FsError> {
        self.fs.chtimes(path, a_time, m_time)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.stats
            .directory_exists(path, || self.fs.directory_exists(path))
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.stats
            .entries(path, || self.fs.get_accessible_entries(path))
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        if let Some(ret) = lock(&self.stat_cache).get(path) {
            return ret.clone();
        }
        let ret = self.fs.stat(path);
        lock(&self.stat_cache)
            .entry(path.to_string())
            .or_insert(ret)
            .clone()
    }

    fn realpath(&self, path: &str) -> String {
        self.stats.realpath(path, || self.fs.realpath(path))
    }
}

// Go: build/host.go:18 host
pub struct BuildHost {
    // PORT: in place of Go `orchestrator *Orchestrator` (see top).
    sys: Rc<dyn System>,
    command: Rc<ParsedBuildCommandLine>,
    compare_paths_options: ComparePathsOptions,

    host: Rc<dyn CompilerHost>,
    // PORT: the `*cachedvfs.FS` of `host`, kept for `resetCaches`. Go
    // reaches it as `o.host.host.FS().(*cachedvfs.FS)` (orchestrator.go:272).
    pub cached_fs: Rc<BuildCachedFs>,

    // Caches that last only for build cycle and then cleared out
    pub extended_config_cache: TscExtendedConfigCache,
    pub source_files: ParseCache<SourceFileCacheKey, Rc<ParsedSourceFile>>,
    // PORT: not in Go. The references of each parse in `source_files`, by
    // file name, made when a program load first needs them
    // (`cached_source_file_refs`).
    cached_refs: RefCell<FxHashMap<String, (std::rc::Weak<ParsedSourceFile>, Arc<FileRefs>)>>,
    pub config_times: RefCell<FxHashMap<Path, Duration>>,

    // caches that stay as long as they are needed
    pub resolved_references: ParseCache<Path, Rc<ParsedCommandLine>>,
    // PORT: not in Go (perf). The threads that parse the configs of the
    // graph ahead of `get_resolved_project_reference` (config_prefetch.rs).
    pub config_prefetch: RefCell<Option<ConfigPrefetch>>,
    // PORT: Go `*collections.SyncMap`. The task `writeFile` stores into it
    // from the checker threads.
    pub m_times: Arc<Mutex<FxHashMap<Path, Option<SystemTime>>>>,
    // PORT: not in Go (perf). The mtimes that the build info threads read
    // for the up-to-date checks of this build cycle (orchestrator.rs
    // `BuildInfoPrefetch`). `load_or_store_m_time` takes one where it would
    // read the file system.
    pub m_time_prefetch: RefCell<Option<MTimePrefetch>>,
    // PORT: not in Go (perf). On a builder thread (builders.rs), what it
    // shares with the build host and the other builders (`BuilderShared`).
    builder: Option<BuilderShared>,
}

/// PORT: not in Go (perf). What the host of a builder thread of a parallel
/// `tsc -b` (builders.rs) shares with the build host and the other
/// builders, so the builds of the tasks read as if they used one host, as
/// in Go:
/// - the cached lookups and `Stat` results of the file system and the
///   mtimes (`BuildCachedFs`, `m_times`); each builder keeps the lookups of
///   its own load apart (`BuildStatCache::for_builder`);
/// - the `.d.ts` and `.json` parses of the first program of the build
///   (`parses`, `SharedParses`). Go keeps the first parse of these files
///   for the whole build (`host.sourceFiles`), and so does each builder
///   that takes them;
/// - the text of each other `.d.ts` and `.json` file that a load of the
///   build read first (`first_reads`). Each builder parses its own, from
///   that text, so a later load gets the text that Go's cache would give
///   it, also after the build wrote the file. No task writes while a load
///   runs (the read rule of builders.rs), so two loads that read a file
///   first at the same time read the same text;
/// - the configs of the build (`configs`), each with its parse time: the
///   build host parsed them all when it made the graph.
#[derive(Clone)]
pub struct BuilderShared {
    stats: Arc<BuildStatCache>,
    stat_cache: StatResults,
    m_times: Arc<Mutex<FxHashMap<Path, Option<SystemTime>>>>,
    parses: Arc<SharedParses>,
    first_reads: Arc<Mutex<FxHashMap<SourceFileCacheKey, FileText>>>,
    configs: Arc<FxHashMap<Path, (Option<SendParsedCommandLine>, Duration)>>,
}

/// PORT: not in Go (perf). The cached `.d.ts` and `.json` parses of the
/// first program that a builder of a parallel build makes (the lib files
/// and the shared declaration files), for the other builders. Their stores
/// are published, so any thread reads their nodes, and their binds join
/// the lineage of the process once (`program::bind_all`). A builder that
/// takes them parses and binds none of them again: without them each
/// builder parsed and bound its own lib files, which cost a wide build
/// with 4 light rebuilds about 2x CPU and 45% more memory.
#[derive(Default)]
pub(crate) struct SharedParses {
    /// None until the first program shares its parses (`share`).
    parses: Mutex<Option<Vec<(SourceFileCacheKey, ParsedSourceFile)>>>,
    shared: std::sync::Condvar,
}

impl BuildHost {
    // PORT: Go builds the host inline in `NewOrchestrator`
    // (orchestrator.go:1004): `compiler.NewCachedFSCompilerHost(cwd, sys.FS(),
    // sys.DefaultLibraryPath(), nil, nil, nil)` and an empty mTimes map.
    // `NewCachedFSCompilerHost` is written out (compiler/host.go:44) to keep
    // the cached file system.
    pub fn new(
        sys: Rc<dyn System>,
        command: Rc<ParsedBuildCommandLine>,
        compare_paths_options: ComparePathsOptions,
    ) -> BuildHost {
        let base = sys.fs();
        let cached_fs = Rc::new(BuildCachedFs::new(base.clone()));
        // PORT: Go `NewCompilerHost`. The host sees through the cache to
        // `sys.FS()`, so on the OS file system the parse workers read and
        // resolve for it (`CompilerHost::is_plain_os_fs`), as for `tsc -p`.
        let host = new_compiler_host_over(
            &sys.get_current_directory(),
            cached_fs.clone(),
            &base,
            &sys.default_library_path(),
        );
        BuildHost {
            sys,
            command,
            compare_paths_options,
            host,
            cached_fs,
            extended_config_cache: TscExtendedConfigCache::default(),
            source_files: ParseCache::default(),
            cached_refs: RefCell::default(),
            config_times: RefCell::new(FxHashMap::default()),
            resolved_references: ParseCache::default(),
            config_prefetch: RefCell::new(None),
            m_times: Arc::default(),
            m_time_prefetch: RefCell::new(None),
            builder: None,
        }
    }

    /// What the hosts of builder threads share with this host
    /// (`BuilderShared`), with the configs that it parsed. None when a
    /// config has content mappers.
    pub(crate) fn builder_shared(&self) -> Option<BuilderShared> {
        let mut configs = FxHashMap::default();
        let times = self.config_times.borrow();
        let mut sendable = true;
        self.resolved_references.for_each_entry(|path, config| {
            let config = config.map(|config| config.to_send());
            sendable &= !matches!(config, Some(None));
            let time = times.get(path).copied().unwrap_or_default();
            configs.insert(path.clone(), (config.flatten(), time));
        });
        sendable.then(|| BuilderShared {
            stats: self.cached_fs.stats.clone(),
            stat_cache: self.cached_fs.stat_cache.clone(),
            m_times: self.m_times.clone(),
            parses: Arc::default(),
            first_reads: Arc::default(),
            configs: Arc::new(configs),
        })
    }

    /// On a builder thread, shares the published parses of this host's
    /// cache with the other builders (`SharedParses`), once per build: the
    /// first program of the build calls it when it is made. A later call
    /// shares nothing.
    pub(crate) fn share_parses(&self) {
        let Some(shared) = &self.builder else {
            return;
        };
        let mut parses = lock(&shared.parses.parses);
        if parses.is_some() {
            return;
        }
        let mut files = Vec::new();
        self.source_files.for_each_stored(|key, file| {
            if crate::ast::is_published(file.store) {
                files.push((key.clone(), ParsedSourceFile::clone(file)));
            }
        });
        *parses = Some(files);
        shared.parses.shared.notify_all();
    }

    /// On a builder thread, waits until the first program of the build has
    /// shared its parses (`share_parses`), and puts them into this host's
    /// cache where it has no entry.
    pub(crate) fn take_shared_parses(&self) {
        if let Some(shared) = &self.builder {
            self.take_parses_of(shared);
        }
    }

    /// Waits until the first program of the build of `shared` has shared
    /// its parses (`share_parses`), and puts them into this host's cache
    /// where it has no entry. The build host takes them before it compiles
    /// a task beside the first one (orchestrator.rs
    /// `later_tasks_use_builders`).
    pub(crate) fn take_parses_of(&self, shared: &BuilderShared) {
        let mut parses = lock(&shared.parses.parses);
        let parses = loop {
            if let Some(parses) = &*parses {
                break parses;
            }
            parses = shared
                .parses
                .shared
                .wait(parses)
                .unwrap_or_else(PoisonError::into_inner);
        };
        for (key, file) in parses {
            self.source_files
                .load_or_store(key.clone(), |_| Some(Rc::new(file.clone())), false);
        }
    }

    /// The host of a builder thread (builders.rs) on `sys`, a system of
    /// that thread, over the parts in `shared`.
    pub(crate) fn new_builder(
        sys: Rc<dyn System>,
        command: Rc<ParsedBuildCommandLine>,
        compare_paths_options: ComparePathsOptions,
        shared: BuilderShared,
    ) -> BuildHost {
        let base = sys.fs();
        let cached_fs = Rc::new(BuildCachedFs {
            fs: base.clone(),
            stats: Arc::new(shared.stats.for_builder()),
            stat_cache: shared.stat_cache.clone(),
        });
        let host = new_compiler_host_over(
            &sys.get_current_directory(),
            cached_fs.clone(),
            &base,
            &sys.default_library_path(),
        );
        BuildHost {
            sys,
            command,
            compare_paths_options,
            host,
            cached_fs,
            extended_config_cache: TscExtendedConfigCache::default(),
            source_files: ParseCache::default(),
            cached_refs: RefCell::default(),
            config_times: RefCell::default(),
            resolved_references: ParseCache::default(),
            config_prefetch: RefCell::new(None),
            m_times: shared.m_times.clone(),
            m_time_prefetch: RefCell::new(None),
            builder: Some(shared),
        }
    }

    /// The parse of the `.d.ts` or `.json` file of `opts` for the cache
    /// (`get_source_file`). On a builder thread it is made from the text
    /// that the first load of the build read (`BuilderShared`), and a first
    /// read keeps its text there.
    fn parse_for_cache(&self, opts: &SourceFileParseOptions) -> Option<Rc<ParsedSourceFile>> {
        let Some(shared) = &self.builder else {
            return self.host.get_source_file(opts);
        };
        let key = SourceFileCacheKey(opts.clone());
        let first = lock(&shared.first_reads).get(&key).cloned();
        if let Some(text) = first {
            return Some(parse_source_file_text(opts, text));
        }
        let file = self.host.get_source_file(opts)?;
        let first = match lock(&shared.first_reads).entry(key) {
            std::collections::hash_map::Entry::Occupied(first) => first.get().clone(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(file.text.clone());
                return Some(file);
            }
        };
        // Another builder read the file first, at the same time.
        Some(if first == file.text {
            file
        } else {
            parse_source_file_text(opts, first)
        })
    }

    // Go: build/host.go:72, the raw command line options of
    // `GetResolvedProjectReference`: wrapped in a "compilerOptions" key to
    // match the tsconfig.json structure.
    pub fn command_line_raw(&self) -> Option<IndexMap<String, CompilerOptionsValue>> {
        match &self.command.raw {
            CompilerOptionsValue::Map(raw) => {
                let mut wrapped = IndexMap::default();
                wrapped.insert(
                    "compilerOptions".to_string(),
                    CompilerOptionsValue::Map(raw.clone()),
                );
                Some(wrapped)
            }
            _ => None,
        }
    }

    // Go: orchestrator.go:97 (*Orchestrator).toPath, as the host reaches it.
    pub fn to_path(&self, file_name: &str) -> Path {
        to_path(
            file_name,
            &self.compare_paths_options.current_directory,
            self.compare_paths_options.use_case_sensitive_file_names,
        )
    }

    // Go: build/host.go:94 (*host).GetMTime
    pub fn get_m_time(&self, file: &str) -> Option<SystemTime> {
        self.load_or_store_m_time(file, None, true)
    }

    /// PORT: not in Go (perf). `get_m_time` of `file`, whose `toPath` is
    /// `path`.
    pub fn get_m_time_of_path(&self, file: &str, path: &Path) -> Option<SystemTime> {
        self.load_or_store_m_time_of_path(file, path.clone(), None, true)
    }

    // Go: build/host.go:98 (*host).SetMTime
    pub fn set_m_time(&self, file: &str, m_time: Option<SystemTime>) -> Result<(), FsError> {
        CompilerHost::fs(self).chtimes(file, None, m_time)
    }

    // Go: build/host.go:102 (*host).loadOrStoreMTime
    pub fn load_or_store_m_time(
        &self,
        file: &str,
        old_cache: Option<&FxHashMap<Path, Option<SystemTime>>>,
        store: bool,
    ) -> Option<SystemTime> {
        self.load_or_store_m_time_of_path(file, self.to_path(file), old_cache, store)
    }

    // Go: build/host.go:102 (*host).loadOrStoreMTime, with
    // `h.orchestrator.toPath(file)` computed by the caller.
    fn load_or_store_m_time_of_path(
        &self,
        file: &str,
        path: Path,
        old_cache: Option<&FxHashMap<Path, Option<SystemTime>>>,
        store: bool,
    ) -> Option<SystemTime> {
        // PORT: Go `Load`, then `LoadOrStore` below. The lock is not held
        // while `get_m_time` reads the file system; else it is held from
        // the load to the store.
        let lock = || self.m_times.lock().unwrap_or_else(PoisonError::into_inner);
        let mut m_times = lock();
        if let Some(existing) = m_times.get(&path) {
            return *existing;
        }
        let mut found = false;
        let mut m_time = None;
        if let Some(old_cache) = old_cache {
            if let Some(old) = old_cache.get(&path) {
                m_time = *old;
                found = true;
            }
        }
        if !found {
            // PORT: perf. An mtime that a build info thread read.
            let prefetched = self.m_time_prefetch.borrow().as_ref().and_then(|m_times| {
                m_times
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&path)
            });
            m_time = match prefetched {
                Some(m_time) => m_time,
                None => {
                    drop(m_times);
                    let m_time = incremental::get_m_time(&*self.host, file);
                    m_times = lock();
                    m_time
                }
            };
        }
        if store {
            m_time = *m_times.entry(path).or_insert(m_time);
        }
        m_time
    }

    // Go: build/host.go:121 (*host).storeMTime
    pub fn store_m_time(&self, file: &str, m_time: Option<SystemTime>) {
        let path = self.to_path(file);
        self.m_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, m_time);
    }

    // Go: build/host.go:126 (*host).storeMTimeFromOldCache
    pub fn store_m_time_from_old_cache(
        &self,
        file: &str,
        old_cache: &FxHashMap<Path, Option<SystemTime>>,
    ) {
        let path = self.to_path(file);
        if let Some(m_time) = old_cache.get(&path) {
            self.m_times
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path, *m_time);
        }
    }

    // Go: build/host.go:87 (*host).ReadBuildInfo
    // PORT: Go reads the build info cache of the config's task
    // (`loadOrStoreBuildInfo`). Its only caller is `ReadBuildInfoProgram` in
    // `compileAndEmit`, with the config of the task that compiles, so
    // `BuildTask::compile_and_emit_start` reads its own cache (`build_info_program`)
    // and the host does not implement `incremental.BuildInfoReader`.
}

impl CompilerHost for BuildHost {
    // Go: build/host.go:38 (*host).FS
    fn fs(&self) -> Rc<dyn Fs> {
        self.host.fs()
    }

    // Go: build/host.go:42 (*host).DefaultLibraryPath
    fn default_library_path(&self) -> String {
        self.host.default_library_path()
    }

    // Go: build/host.go:46 (*host).GetCurrentDirectory
    fn get_current_directory(&self) -> String {
        self.host.get_current_directory()
    }

    // Go: build/host.go:50 (*host).Trace
    fn trace(&self, _msg: &'static Message, _args: Vec<String>) {
        panic!(
            "build.Orchestrator.host does not support tracing; use a different host for tracing"
        );
    }

    // Go: build/host.go:54 (*host).GetSourceFile
    fn get_source_file(&self, opts: &SourceFileParseOptions) -> Option<Rc<ParsedSourceFile>> {
        if is_declaration_file_name(&opts.file_name)
            || file_extension_is(&opts.file_name, EXTENSION_JSON)
        {
            // Cache dts and json files as they will be reused
            // PORT: a parse that the cache keeps can be left out of one
            // program (a deduplicated package, or a file that only such a
            // package imports) and be a program file of a later one. Go
            // keeps the whole `*ast.SourceFile`. The note makes the publish
            // of the first program give the store its complete Go file, so
            // the later program can use it.
            return self.source_files.load_or_store(
                SourceFileCacheKey(opts.clone()),
                |key| {
                    let file = self.parse_for_cache(&key.0);
                    if let Some(file) = &file {
                        crate::program::note_parsed_source_file(file);
                    }
                    file
                },
                false, /* allowZero */
            );
        }
        self.host.get_source_file(opts)
    }

    // Go: build/host.go:62 (*host).GetContentMappedSourceFiles (tsgo#4712)
    fn get_content_mapped_source_files(
        &self,
        _parse_options: &SourceFileParseOptions,
        _mapper: &Rc<Mapper>,
    ) -> Result<SourceFiles, GoError> {
        Err(contentmapper::ERR_PROJECT_UNAVAILABLE.clone())
    }

    // Go: build/host.go:66 (*host).ContentMapperProject (tsgo#4712)
    fn content_mapper_project(&self) -> Option<Rc<dyn Project>> {
        panic!(
            "build.Orchestrator.host does not support content mapper project; use an individual project's compiler host instead"
        );
    }

    // PORT: not in Go (see `CompilerHost::is_plain_os_fs`).
    fn is_plain_os_fs(&self) -> bool {
        self.host.is_plain_os_fs()
    }

    // PORT: not in Go (see `CompilerHost::stat_cache`).
    fn stat_cache(&self) -> Option<Arc<BuildStatCache>> {
        Some(self.cached_fs.stats.clone())
    }

    // PORT: not in Go (see `CompilerHost::cached_source_file_refs`). The
    // `.d.ts` and `.json` files that `get_source_file` keeps. The
    // references of each parse are made once (`cached_refs`).
    fn cached_source_file_refs(&self) -> FxHashMap<String, Arc<FileRefs>> {
        let mut refs = FxHashMap::default();
        let mut memo = self.cached_refs.borrow_mut();
        self.source_files.for_each_stored(|key, file| {
            let (parse, file_refs) = memo
                .entry(key.0.file_name.clone())
                .or_insert_with(|| (Rc::downgrade(file), Arc::new(FileRefs::of_file(file))));
            if !parse.ptr_eq(&Rc::downgrade(file)) {
                *parse = Rc::downgrade(file);
                *file_refs = Arc::new(FileRefs::of_file(file));
            }
            refs.insert(key.0.file_name.clone(), file_refs.clone());
        });
        refs
    }

    // Go: build/host.go:70 (*host).GetResolvedProjectReference
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: &Path,
    ) -> Option<Rc<ParsedCommandLine>> {
        self.resolved_references.load_or_store(
            path.clone(),
            |path| {
                // PORT: a builder thread takes the build host's parse.
                if let Some((config, time)) = self
                    .builder
                    .as_ref()
                    .and_then(|shared| shared.configs.get(path))
                {
                    self.config_times.borrow_mut().insert(path.clone(), *time);
                    return config.as_ref().map(|config| Rc::new(config.to_local()));
                }
                let config_start = self.sys.now();
                // Wrap command line options in "compilerOptions" key to match tsconfig.json structure
                let command_line_raw = self.command_line_raw();
                let (command_line, _) = get_parsed_command_line_of_config_file_path(
                    file_name,
                    path.clone(),
                    Some(&self.command.compiler_options),
                    command_line_raw.as_ref(),
                    self,
                    Some(&self.extended_config_cache),
                );
                let config_time = self
                    .sys
                    .now()
                    .duration_since(config_start)
                    .unwrap_or_default();
                self.config_times
                    .borrow_mut()
                    .insert(path.clone(), config_time);
                command_line.map(Rc::new)
            },
            true, /* allowZero */
        )
    }
}

// PORT: Go passes the `*host` as a `tsoptions.ParseConfigHost` (it has
// `FS()` and `GetCurrentDirectory()`). Rust needs the explicit impl.
impl ParseConfigHost for BuildHost {
    fn fs(&self) -> Rc<dyn Fs> {
        self.host.fs()
    }

    fn get_current_directory(&self) -> String {
        self.host.get_current_directory()
    }

    // PORT: not in Go (perf). The file names that a config thread matched
    // (config_prefetch.rs), else Go `getFileNamesFromConfigSpecs`.
    fn get_file_names_from_config_specs(
        &self,
        config_file_name: &str,
        config_file_specs: &ConfigFileSpecs,
        base_path: &str,
        options: Option<&CompilerOptions>,
        extra_extensions: &[String],
    ) -> (Vec<String>, i32) {
        let fs = ParseConfigHost::fs(self);
        match &*self.config_prefetch.borrow() {
            Some(prefetch) => prefetch.get_file_names_from_config_specs(
                config_file_name,
                config_file_specs,
                base_path,
                options,
                extra_extensions,
                &*fs,
                &self.cached_fs.stats,
            ),
            None => get_file_names_from_config_specs(
                config_file_specs,
                base_path,
                options,
                &*fs,
                extra_extensions,
            ),
        }
    }
}

// Go: build/host.go:35 `_ incremental.Host = (*host)(nil)`
impl incremental::Host for BuildHost {
    fn fs(&self) -> Rc<dyn Fs> {
        CompilerHost::fs(self)
    }

    fn get_m_time(&self, file_name: &str) -> Option<SystemTime> {
        BuildHost::get_m_time(self, file_name)
    }

    fn set_m_time(&self, file_name: &str, m_time: Option<SystemTime>) -> Result<(), FsError> {
        BuildHost::set_m_time(self, file_name, m_time)
    }
}

// Go: build/compilerHost.go:13 compilerHost
// PORT: the host that the build task gives `compiler.NewProgram`: the
// build host with the task's trace writer. Go nil `contentMapperProject`
// is `None`.
pub struct BuildCompilerHost {
    pub host: Rc<BuildHost>,
    pub trace: TraceFn,
    pub content_mapper_project: Option<Rc<dyn Project>>,
}

impl CompilerHost for BuildCompilerHost {
    // Go: build/compilerHost.go:21 (*compilerHost).FS
    fn fs(&self) -> Rc<dyn Fs> {
        CompilerHost::fs(&*self.host)
    }

    // Go: build/compilerHost.go:25 (*compilerHost).DefaultLibraryPath
    fn default_library_path(&self) -> String {
        self.host.default_library_path()
    }

    // Go: build/compilerHost.go:29 (*compilerHost).GetCurrentDirectory
    fn get_current_directory(&self) -> String {
        CompilerHost::get_current_directory(&*self.host)
    }

    // Go: build/compilerHost.go:33 (*compilerHost).Trace
    fn trace(&self, msg: &'static Message, args: Vec<String>) {
        (self.trace)(msg, args);
    }

    // Go: build/compilerHost.go:37 (*compilerHost).GetSourceFile
    fn get_source_file(&self, opts: &SourceFileParseOptions) -> Option<Rc<ParsedSourceFile>> {
        self.host.get_source_file(opts)
    }

    // Go: build/compilerHost.go:41 (*compilerHost).GetContentMappedSourceFiles (tsgo#4712)
    // PORT: Go returns `(files, err)`; a file that cannot be read is `Ok`
    // with no canonical file, as in the compiler host.
    fn get_content_mapped_source_files(
        &self,
        parse_options: &SourceFileParseOptions,
        mapper: &Rc<Mapper>,
    ) -> Result<SourceFiles, GoError> {
        let Some(project) = self.content_mapper_project() else {
            return Err(contentmapper::ERR_PROJECT_UNAVAILABLE.clone());
        };
        let fs = CompilerHost::fs(self);
        let (content, ok) = fs.read_file(&parse_options.file_name);
        if !ok {
            return Ok(SourceFiles::default());
        }
        let files = contentmapper::transform_and_parse(parse_options, &content, mapper, &*project)?;
        contentmapper::check_supplemental_file_name_collisions(&files, &|name: &str| {
            fs.file_exists(name)
        })?;
        Ok(files)
    }

    // Go: build/compilerHost.go:56 (*compilerHost).ContentMapperProject (tsgo#4712)
    fn content_mapper_project(&self) -> Option<Rc<dyn Project>> {
        self.content_mapper_project.clone()
    }

    // Go: build/compilerHost.go:60 (*compilerHost).GetResolvedProjectReference
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: &Path,
    ) -> Option<Rc<ParsedCommandLine>> {
        self.host.get_resolved_project_reference(file_name, path)
    }

    // PORT: not in Go (see `CompilerHost::is_plain_os_fs`).
    fn is_plain_os_fs(&self) -> bool {
        self.host.is_plain_os_fs()
    }

    // PORT: not in Go (see `CompilerHost::stat_cache`).
    fn stat_cache(&self) -> Option<Arc<BuildStatCache>> {
        self.host.stat_cache()
    }

    // PORT: not in Go (see `CompilerHost::cached_source_file_refs`).
    fn cached_source_file_refs(&self) -> FxHashMap<String, Arc<FileRefs>> {
        self.host.cached_source_file_refs()
    }
}
