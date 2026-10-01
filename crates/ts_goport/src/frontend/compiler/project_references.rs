//! Port of compiler/projectreferenceparser.go, projectreferencefilemapper.go
//! and projectreferencedtsfakinghost.go: loading referenced projects, the
//! source <-> output .d.ts maps, and the resolution host that fakes the
//! output .d.ts files of referenced projects.

use crate::frontend::prelude::*;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// projectreferenceparser.go
// ---------------------------------------------------------------------------

/// Shared handle to a parse task. Go shares `*projectReferenceParseTask`.
pub type ProjectReferenceParseTaskRef = Rc<RefCell<ProjectReferenceParseTask>>;

// Go: projectreferenceparser.go:13 projectReferenceParseTask
#[derive(Default)]
pub struct ProjectReferenceParseTask {
    pub config_name: String,
    pub resolved: Option<Rc<ParsedCommandLine>>,
    pub sub_tasks: Vec<ProjectReferenceParseTaskRef>,
}

impl ProjectReferenceParseTask {
    // Go: projectreferenceparser.go:19 (*projectReferenceParseTask).parse
    pub fn parse(&mut self, project_reference_parser: &ProjectReferenceParser<'_>) {
        let loader = &*project_reference_parser.loader;
        let _trace = crate::tracing::get().map(|tr| {
            tr.push(
                crate::tracing::Phase::Parse,
                "parseJsonSourceFileConfigFileContent",
                vec![("path", self.config_name.clone().into())],
                false,
            )
        });
        self.resolved = loader
            .opts
            .host
            .get_resolved_project_reference(&self.config_name, &loader.to_path(&self.config_name));
        let Some(resolved) = &self.resolved else {
            return;
        };
        resolved.parse_input_output_names();
        let sub_references = resolved.resolved_project_reference_paths();
        if !sub_references.is_empty() {
            self.sub_tasks = create_project_reference_parse_tasks(&sub_references);
        }
    }
}

// Go: projectreferenceparser.go:34 createProjectReferenceParseTasks
#[must_use]
pub fn create_project_reference_parse_tasks(
    project_references: &[String],
) -> Vec<ProjectReferenceParseTaskRef> {
    project_references
        .iter()
        .map(|config_name| {
            Rc::new(RefCell::new(ProjectReferenceParseTask {
                config_name: config_name.clone(),
                ..Default::default()
            }))
        })
        .collect()
}

// Go: projectreferenceparser.go:42 projectReferenceParser
// PORT: single-threaded. Go `core.WorkGroup` in single-threaded mode is a
// LIFO stack of closures; `queue` holds the queued tasks in the same order.
// Go `collections.SyncMap` is a plain map.
pub struct ProjectReferenceParser<'a> {
    pub loader: &'a mut FileLoader,
    pub queue: Vec<ProjectReferenceParseTaskRef>,
    pub tasks_by_file_name: FxHashMap<Path, ProjectReferenceParseTaskRef>,
    /// Go `core.NewWorkGroup(singleThreaded)`: false when Go runs each
    /// queued task on its own goroutine (see `run_and_wait`).
    single_threaded: bool,
}

impl<'a> ProjectReferenceParser<'a> {
    // PORT: Go builds the parser with a struct literal and
    // `core.NewWorkGroup(singleThreaded)`. The port runs the queue of the
    // single-threaded work group in both modes; `single_threaded` keeps
    // Go's goroutine boundary for panics (`run_and_wait`).
    pub fn new(loader: &'a mut FileLoader, single_threaded: bool) -> Self {
        ProjectReferenceParser {
            loader,
            queue: Vec::new(),
            tasks_by_file_name: FxHashMap::default(),
            single_threaded,
        }
    }

    // Go: projectreferenceparser.go:48 (*projectReferenceParser).parse
    // PORT: Go `mapper.loader = p.loader`; see `MapperLoader`.
    pub fn parse(&mut self, mut tasks: Vec<ProjectReferenceParseTaskRef>) {
        let mapper_loader = MapperLoader::new(self.loader);
        self.loader
            .project_reference_file_mapper
            .borrow_mut()
            .loader = Some(mapper_loader);
        self.start(&mut tasks);
        self.run_and_wait();
        self.init_mapper(&tasks);
    }

    // Go: core/workgroup.go:67 (*singleThreadedWorkGroup).RunAndWait
    // PORT: each popped entry is the Go queued closure
    // `task.parse(p); p.start(task.subTasks)`. Unless single threaded, Go
    // runs it on its own goroutine (core/workgroup.go:38
    // `parallelWorkGroup.Queue`, `sync.WaitGroup.Go`), where no caller's
    // `recover()` sees a panic: with `go_work_group_task` a Go panic in it
    // ends the run as it does in Go, as in `FilesParser::run_queue`.
    fn run_and_wait(&mut self) {
        let single_threaded = self.single_threaded;
        while let Some(task) = self.queue.pop() {
            let mut run = || {
                task.borrow_mut().parse(self);
                let mut sub_tasks = std::mem::take(&mut task.borrow_mut().sub_tasks);
                self.start(&mut sub_tasks);
                task.borrow_mut().sub_tasks = sub_tasks;
            };
            if single_threaded {
                run();
            } else {
                crate::core::go_work_group_task(run);
            }
        }
    }

    // Go: projectreferenceparser.go:55 (*projectReferenceParser).start
    pub fn start(&mut self, tasks: &mut [ProjectReferenceParseTaskRef]) {
        for task in tasks.iter_mut() {
            let path = self.loader.to_path(&task.borrow().config_name);
            if let Some(loaded_task) = self.tasks_by_file_name.get(&path) {
                // dedup tasks to ensure correct file order, regardless of which task would be started first
                *task = loaded_task.clone();
            } else {
                self.tasks_by_file_name.insert(path, task.clone());
                self.queue.push(task.clone());
            }
        }
    }

    // Go: projectreferenceparser.go:70 (*projectReferenceParser).initMapper
    pub fn init_mapper(&mut self, tasks: &[ProjectReferenceParseTaskRef]) {
        let total_references = self.tasks_by_file_name.len() + 1;
        {
            let mut mapper = self.loader.project_reference_file_mapper.borrow_mut();
            mapper.config_to_project_reference =
                FxHashMap::with_capacity_and_hasher(total_references, Default::default());
            mapper.references_in_config_file =
                FxHashMap::with_capacity_and_hasher(total_references, Default::default());
            mapper.source_to_project_reference = FxHashMap::default();
            mapper.output_dts_to_project_reference = FxHashMap::default();
        }
        let config_path = self
            .loader
            .project_reference_file_mapper
            .borrow()
            .root_config_path();
        let mut seen = FxHashSet::default();
        let references = self.init_mapper_worker(tasks, &mut seen);
        self.loader
            .project_reference_file_mapper
            .borrow_mut()
            .references_in_config_file
            .insert(config_path, references);
        let needs_faking_host = {
            let mapper = self.loader.project_reference_file_mapper.borrow();
            mapper.opts.can_use_project_reference_source()
                && !mapper.output_dts_to_project_reference.is_empty()
        };
        if needs_faking_host {
            let host = new_project_reference_dts_faking_host(self.loader);
            self.loader.project_reference_file_mapper.borrow_mut().host = Some(host);
        }
    }

    // Go: projectreferenceparser.go:82 (*projectReferenceParser).initMapperWorker
    // PORT: Go `collections.Set[*projectReferenceParseTask]` keys by pointer;
    // `seen` keys by the `Rc` pointer.
    pub fn init_mapper_worker(
        &mut self,
        tasks: &[ProjectReferenceParseTaskRef],
        seen: &mut FxHashSet<*const RefCell<ProjectReferenceParseTask>>,
    ) -> Vec<Path> {
        if tasks.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::with_capacity(tasks.len());
        for task in tasks {
            let path = self.loader.to_path(&task.borrow().config_name);
            results.push(path.clone());
            // ensure we only walk each task once
            if !seen.insert(Rc::as_ptr(task)) {
                continue;
            }
            let resolved = task.borrow().resolved.clone();
            let can_use_project_reference_source = {
                let mut mapper = self.loader.project_reference_file_mapper.borrow_mut();
                mapper
                    .config_to_project_reference
                    .insert(path.clone(), resolved.clone());
                mapper.opts.can_use_project_reference_source()
            };
            if let Some(resolved) = &resolved {
                // PORT: Go compares `*TsConfigSourceFile` pointers. The config
                // source file node identifies the parsed config file.
                let is_root_config = {
                    let mapper = self.loader.project_reference_file_mapper.borrow();
                    mapper
                        .opts
                        .config
                        .config_file
                        .as_ref()
                        .map(|c| c.source_file)
                        == resolved.config_file.as_ref().map(|c| c.source_file)
                };
                if !is_root_config {
                    // Map current task's files first, before recursing into subtasks.
                    // This matches TypeScript's behavior where child project references
                    // overwrite parent entries when a file belongs to multiple projects.
                    {
                        let mut mapper = self.loader.project_reference_file_mapper.borrow_mut();
                        for (k, v) in resolved.source_to_project_reference().into_iter().flatten() {
                            mapper
                                .source_to_project_reference
                                .insert(k.clone(), v.clone());
                        }
                        for (k, v) in resolved
                            .output_dts_to_project_reference()
                            .into_iter()
                            .flatten()
                        {
                            mapper
                                .output_dts_to_project_reference
                                .insert(k.clone(), v.clone());
                        }
                    }
                    if can_use_project_reference_source {
                        let options = resolved.compiler_options();
                        let mut decl_dir = options.declaration_dir.clone();
                        if decl_dir.is_empty() {
                            decl_dir = options.out_dir.clone();
                        }
                        if !decl_dir.is_empty() {
                            let decl_dir_path = self.loader.to_path(&decl_dir);
                            self.loader.dts_directories.insert(decl_dir_path);
                        }
                    }
                }
            }
            let sub_tasks = task.borrow().sub_tasks.clone();
            let references_in_config = self.init_mapper_worker(&sub_tasks, seen);
            self.loader
                .project_reference_file_mapper
                .borrow_mut()
                .references_in_config_file
                .insert(path, references_in_config);
        }
        results
    }
}

// ---------------------------------------------------------------------------
// projectreferencefilemapper.go
// ---------------------------------------------------------------------------

/// Go `f func(path, config, parent, index) bool` for the range helpers.
pub type RangeResolvedProjectReferenceFn<'f> = dyn FnMut(&Path, Option<&Rc<ParsedCommandLine>>, Option<&Rc<ParsedCommandLine>>, usize) -> bool
    + 'f;

/// The part of Go `*fileLoader` that the mapper uses: the non-nil check and
/// `loader.toPath`.
// PORT: Go keeps a `*fileLoader` in the mapper, and the loader keeps the
// mapper. This keeps only the `toPath` inputs, so there is no cycle.
// `fileLoader.toPath` reads `opts.Host` each time; the host values do not
// change while the loader is present.
#[derive(Clone, Debug)]
pub struct MapperLoader {
    pub current_directory: String,
    pub use_case_sensitive_file_names: bool,
}

impl MapperLoader {
    #[must_use]
    pub fn new(loader: &FileLoader) -> Self {
        MapperLoader {
            current_directory: loader.opts.host.get_current_directory(),
            use_case_sensitive_file_names: loader.opts.host.fs().use_case_sensitive_file_names(),
        }
    }

    // Go: fileloader.go:187 (*fileLoader).toPath
    #[must_use]
    pub fn to_path(&self, file: &str) -> Path {
        to_path(
            file,
            &self.current_directory,
            self.use_case_sensitive_file_names,
        )
    }
}

// Go: projectreferencefilemapper.go:14 projectReferenceFileMapper
// PORT: Go `loader *fileLoader` is only used as a non-nil flag and for
// `loader.toPath`; see `MapperLoader`.
// PORT: the Go nil `*ParsedCommandLine` map values are `None`. Go
// `collections.SyncMap` is a `RefCell` map, so lookups take `&self`.
// PORT: `host` is `None` for the Go nil. Go starts it as `opts.Host` (a
// `CompilerHost`, which is a `module.ResolutionHost`);
// `CompilerResolutionHost` wraps it.
pub struct ProjectReferenceFileMapper {
    pub opts: ProgramOptions,
    pub host: Option<Rc<dyn ResolutionHost>>,
    // Only present during populating the mapper and parsing, released after that
    pub loader: Option<MapperLoader>,

    // All the resolved references needed
    pub config_to_project_reference: FxHashMap<Path, Option<Rc<ParsedCommandLine>>>,
    // Map of config file to its references
    pub references_in_config_file: FxHashMap<Path, Vec<Path>>,
    pub source_to_project_reference: FxHashMap<Path, Rc<SourceOutputAndProjectReference>>,
    pub output_dts_to_project_reference: FxHashMap<Path, Rc<SourceOutputAndProjectReference>>,

    // Store all the realpath from dts in node_modules to source file from project reference needed during parsing so it can be used later
    pub realpath_dts_to_source:
        RefCell<FxHashMap<Path, Option<Rc<SourceOutputAndProjectReference>>>>,
}

impl ProjectReferenceFileMapper {
    // PORT: Go `&projectReferenceFileMapper{opts: p.opts, host: p.opts.Host}`
    // (fileloader.go addProjectReferenceTasks).
    #[must_use]
    pub fn new(opts: ProgramOptions, host: Rc<dyn CompilerHost>) -> Self {
        let host: Rc<dyn ResolutionHost> = Rc::new(CompilerResolutionHost::new(host));
        ProjectReferenceFileMapper {
            opts,
            host: Some(host),
            loader: None,
            config_to_project_reference: FxHashMap::default(),
            references_in_config_file: FxHashMap::default(),
            source_to_project_reference: FxHashMap::default(),
            output_dts_to_project_reference: FxHashMap::default(),
            realpath_dts_to_source: RefCell::new(FxHashMap::default()),
        }
    }

    // Go: projectreferencefilemapper.go:28 (*projectReferenceFileMapper).rootConfigPath
    pub(crate) fn root_config_path(&self) -> Path {
        match self.opts.config.config_file.as_ref() {
            None => Path::default(),
            Some(config_file) => config_file.path.clone(),
        }
    }

    // Go: projectreferencefilemapper.go:28 (*projectReferenceFileMapper).getParseFileRedirect
    #[must_use]
    pub fn get_parse_file_redirect(&self, file: &dyn HasFileName) -> String {
        if self.opts.can_use_project_reference_source() {
            // Map to source file from project reference
            let mut source = self.get_project_reference_from_output_dts(&file.path());
            if source.is_none() {
                source = self.get_source_to_dts_if_symlink(file);
            }
            if let Some(source) = source {
                return source.source.clone();
            }
        } else {
            // Map to dts file from project reference
            let output = self.get_project_reference_from_source(&file.path());
            if let Some(output) = output
                && !output.output_dts.is_empty()
            {
                return output.output_dts.clone();
            }
        }
        String::new()
    }

    // Go: projectreferencefilemapper.go:48 (*projectReferenceFileMapper).getResolvedProjectReferences
    #[must_use]
    pub fn get_resolved_project_references(&self) -> Vec<Option<Rc<ParsedCommandLine>>> {
        let mut result = Vec::new();
        if let Some(refs) = self.references_in_config_file.get(&self.root_config_path()) {
            result.reserve(refs.len());
            for ref_path in refs {
                let ref_config = self
                    .config_to_project_reference
                    .get(ref_path)
                    .cloned()
                    .flatten();
                result.push(ref_config);
            }
        }
        result
    }

    // Go: projectreferencefilemapper.go:64 (*projectReferenceFileMapper).getProjectReferenceFromSource
    #[must_use]
    pub fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<Rc<SourceOutputAndProjectReference>> {
        self.source_to_project_reference.get(path).cloned()
    }

    // Go: projectreferencefilemapper.go:68 (*projectReferenceFileMapper).getProjectReferenceFromOutputDts
    #[must_use]
    pub fn get_project_reference_from_output_dts(
        &self,
        path: &Path,
    ) -> Option<Rc<SourceOutputAndProjectReference>> {
        self.output_dts_to_project_reference.get(path).cloned()
    }

    // Go: projectreferencefilemapper.go:72 (*projectReferenceFileMapper).isSourceFromProjectReference
    #[must_use]
    pub fn is_source_from_project_reference(&self, path: &Path) -> bool {
        self.opts.can_use_project_reference_source()
            && self.get_project_reference_from_source(path).is_some()
    }

    // Go: projectreferencefilemapper.go:76 (*projectReferenceFileMapper).getCompilerOptionsForFile
    #[must_use]
    pub fn get_compiler_options_for_file(&self, file: &dyn HasFileName) -> Rc<CompilerOptions> {
        let redirect = self.get_redirect_parsed_command_line_for_resolution(file);
        get_compiler_options_with_redirect(
            self.opts.config.compiler_options(),
            redirect
                .as_deref()
                .map(|r| r as &dyn ModuleResolvedProjectReference),
        )
    }

    // Go: projectreferencefilemapper.go:81 (*projectReferenceFileMapper).getRedirectParsedCommandLineForResolution
    #[must_use]
    pub fn get_redirect_parsed_command_line_for_resolution(
        &self,
        file: &dyn HasFileName,
    ) -> Option<Rc<ParsedCommandLine>> {
        let (redirect, _) = self.get_redirect_for_resolution(file);
        redirect
    }

    // Go: projectreferencefilemapper.go:86 (*projectReferenceFileMapper).getRedirectForResolution
    #[must_use]
    pub fn get_redirect_for_resolution(
        &self,
        file: &dyn HasFileName,
    ) -> (Option<Rc<ParsedCommandLine>>, String) {
        let path = file.path();
        // Check if outputdts of source file from project reference
        if let Some(output) = self.get_project_reference_from_source(&path) {
            return (output.resolved.upgrade(), output.source.clone());
        }

        // Source file from project reference
        if let Some(result_from_dts) = self.get_project_reference_from_output_dts(&path) {
            return (
                result_from_dts.resolved.upgrade(),
                result_from_dts.source.clone(),
            );
        }

        if let Some(realpath_dts_to_source) = self.get_source_to_dts_if_symlink(file) {
            return (
                realpath_dts_to_source.resolved.upgrade(),
                realpath_dts_to_source.source.clone(),
            );
        }
        (None, file.file_name())
    }

    // Go: projectreferencefilemapper.go:107 (*projectReferenceFileMapper).getResolvedReferenceFor
    #[must_use]
    pub fn get_resolved_reference_for(&self, path: &Path) -> (Option<Rc<ParsedCommandLine>>, bool) {
        match self.config_to_project_reference.get(path) {
            Some(config) => (config.clone(), true),
            None => (None, false),
        }
    }

    // Go: projectreferencefilemapper.go:112 (*projectReferenceFileMapper).rangeResolvedProjectReference
    // PORT: Go `index int` is `usize`.
    pub fn range_resolved_project_reference(
        &self,
        mut f: impl FnMut(
            &Path,
            Option<&Rc<ParsedCommandLine>>,
            Option<&Rc<ParsedCommandLine>>,
            usize,
        ) -> bool,
    ) -> bool {
        if self.opts.config.project_references().is_empty() {
            return false;
        }
        let mut seen_ref = FxHashSet::with_capacity_and_hasher(
            self.references_in_config_file.len(),
            Default::default(),
        );
        let root_config_path = self.root_config_path();
        seen_ref.insert(root_config_path.clone());
        let refs = self
            .references_in_config_file
            .get(&root_config_path)
            .cloned()
            .unwrap_or_default();
        self.range_resolved_reference_worker(&refs, &mut f, Some(&self.opts.config), &mut seen_ref)
    }

    // Go: projectreferencefilemapper.go:124 (*projectReferenceFileMapper).rangeResolvedReferenceWorker
    pub fn range_resolved_reference_worker(
        &self,
        references: &[Path],
        f: &mut RangeResolvedProjectReferenceFn<'_>,
        parent: Option<&Rc<ParsedCommandLine>>,
        seen_ref: &mut FxHashSet<Path>,
    ) -> bool {
        for (index, path) in references.iter().enumerate() {
            if !seen_ref.insert(path.clone()) {
                continue;
            }
            let config = self
                .config_to_project_reference
                .get(path)
                .cloned()
                .flatten();
            if !f(path, config.as_ref(), parent, index) {
                return false;
            }
            let child_refs = self
                .references_in_config_file
                .get(path)
                .cloned()
                .unwrap_or_default();
            if !self.range_resolved_reference_worker(&child_refs, f, config.as_ref(), seen_ref) {
                return false;
            }
        }
        true
    }

    // Go: projectreferencefilemapper.go:145 (*projectReferenceFileMapper).rangeResolvedProjectReferenceInChildConfig
    // PORT: Go `childConfig` can be nil. All callers pass a config, so it is
    // a reference here; the `ConfigFile == nil` check stays.
    pub fn range_resolved_project_reference_in_child_config(
        &self,
        child_config: &Rc<ParsedCommandLine>,
        mut f: impl FnMut(
            &Path,
            Option<&Rc<ParsedCommandLine>>,
            Option<&Rc<ParsedCommandLine>>,
            usize,
        ) -> bool,
    ) -> bool {
        let Some(child_config_file) = child_config.config_file.as_ref() else {
            return false;
        };
        let child_path = child_config_file.path.clone();
        let mut seen_ref = FxHashSet::with_capacity_and_hasher(
            self.references_in_config_file.len(),
            Default::default(),
        );
        seen_ref.insert(child_path.clone());
        let refs = self
            .references_in_config_file
            .get(&child_path)
            .cloned()
            .unwrap_or_default();
        self.range_resolved_reference_worker(&refs, &mut f, Some(&self.opts.config), &mut seen_ref)
    }

    // Go: projectreferencefilemapper.go:158 (*projectReferenceFileMapper).getSourceToDtsIfSymlink
    #[must_use]
    pub fn get_source_to_dts_if_symlink(
        &self,
        file: &dyn HasFileName,
    ) -> Option<Rc<SourceOutputAndProjectReference>> {
        // If preserveSymlinks is true, module resolution wont jump the symlink
        // but the resolved real path may be the .d.ts from project reference
        // Note:: Currently we try the real path only if the
        // file is from node_modules to avoid having to run real path on all file paths
        let path = file.path();
        if let Some(realpath_dts_to_source) = self.realpath_dts_to_source.borrow().get(&path) {
            return realpath_dts_to_source.clone();
        }
        if let Some(loader) = &self.loader
            && self.opts.config.compiler_options().preserve_symlinks == Tristate::True
        {
            let file_name = file.file_name();
            if !file_name.contains("/node_modules/") {
                self.realpath_dts_to_source.borrow_mut().insert(path, None);
            } else {
                let host = self
                    .host
                    .as_ref()
                    .expect("project reference mapper host is released");
                let real_declaration_path = loader.to_path(&host.fs().realpath(&file_name));
                if real_declaration_path == path {
                    self.realpath_dts_to_source.borrow_mut().insert(path, None);
                } else {
                    let realpath_dts_to_source =
                        self.get_project_reference_from_output_dts(&real_declaration_path);
                    if realpath_dts_to_source.is_some() {
                        self.realpath_dts_to_source
                            .borrow_mut()
                            .insert(path, realpath_dts_to_source.clone());
                        return realpath_dts_to_source;
                    }
                    self.realpath_dts_to_source.borrow_mut().insert(path, None);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// projectreferencedtsfakinghost.go
// ---------------------------------------------------------------------------

/// A `CompilerHost` used as a `module.ResolutionHost`.
// PORT: Go passes the `CompilerHost` interface value where a
// `module.ResolutionHost` is expected. `ResolutionHost` returns borrowed
// values, so this keeps the host's `FS()` and current directory.
pub struct CompilerResolutionHost {
    pub host: Rc<dyn CompilerHost>,
    pub fs: Rc<dyn Fs>,
    pub current_directory: String,
}

impl CompilerResolutionHost {
    #[must_use]
    pub fn new(host: Rc<dyn CompilerHost>) -> Self {
        let fs = host.fs();
        let current_directory = host.get_current_directory();
        CompilerResolutionHost {
            host,
            fs,
            current_directory,
        }
    }
}

impl ResolutionHost for CompilerResolutionHost {
    fn fs(&self) -> &dyn Fs {
        &*self.fs
    }

    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
}

// Go: projectreferencedtsfakinghost.go:17 projectReferenceDtsFakingHost
// PORT: `current_directory` caches Go `h.host.GetCurrentDirectory()`,
// because `ResolutionHost` returns `&str`. The host value does not change.
pub struct ProjectReferenceDtsFakingHost {
    pub host: Rc<dyn CompilerHost>,
    pub fs: Rc<CachedFs>,
    pub current_directory: String,
}

// Go: projectreferencedtsfakinghost.go:24 newProjectReferenceDtsFakingHost
// PORT: Go copies `loader.dtsDirectories` by value. The set is complete
// when this runs (end of `initMapper`), so a clone is the same.
#[must_use]
pub fn new_project_reference_dts_faking_host(loader: &FileLoader) -> Rc<dyn ResolutionHost> {
    // Create a new host that will fake the dts files
    let vfs: Rc<dyn Fs> = Rc::new(ProjectReferenceDtsFakingVfs {
        project_reference_file_mapper: loader.project_reference_file_mapper.clone(),
        dts_directories: loader.dts_directories.clone(),
        known_symlinks: RefCell::new(KnownSymlinks::default()),
    });
    let host = ProjectReferenceDtsFakingHost {
        host: loader.opts.host.clone(),
        fs: cachedvfs_from(vfs),
        current_directory: loader.opts.host.get_current_directory(),
    };
    Rc::new(host)
}

impl ResolutionHost for ProjectReferenceDtsFakingHost {
    // Go: projectreferencedtsfakinghost.go:38 (*projectReferenceDtsFakingHost).FS
    fn fs(&self) -> &dyn Fs {
        &*self.fs
    }

    // Go: projectreferencedtsfakinghost.go:43 (*projectReferenceDtsFakingHost).GetCurrentDirectory
    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
}

// Go: projectreferencedtsfakinghost.go:47 projectReferenceDtsFakingVfs
// PORT: the mapper is shared (`Rc<RefCell<..>>`). The loader clears
// `mapper.host` after loading, which breaks the mapper -> host -> vfs ->
// mapper cycle as in Go. `known_symlinks` is mutated from `&self` methods,
// so it is a `RefCell`.
pub struct ProjectReferenceDtsFakingVfs {
    pub project_reference_file_mapper: Rc<RefCell<ProjectReferenceFileMapper>>,
    pub dts_directories: FxHashSet<Path>,
    pub known_symlinks: RefCell<KnownSymlinks>,
}

impl ProjectReferenceDtsFakingVfs {
    fn host_fs(&self) -> Rc<dyn Fs> {
        self.project_reference_file_mapper.borrow().opts.host.fs()
    }

    // Go: projectreferencedtsfakinghost.go:131 (*projectReferenceDtsFakingVfs).toPath
    fn to_path(&self, path: &str) -> Path {
        let current_directory = self
            .project_reference_file_mapper
            .borrow()
            .opts
            .host
            .get_current_directory();
        to_path(
            path,
            &current_directory,
            self.use_case_sensitive_file_names(),
        )
    }

    // Go: projectreferencedtsfakinghost.go:135 (*projectReferenceDtsFakingVfs).handleDirectoryCouldBeSymlink
    fn handle_directory_could_be_symlink(&self, directory: &str) {
        if contains_ignored_path(directory) {
            return;
        }

        // Because we already watch node_modules, handle symlinks in there
        if !directory.contains("/node_modules/") {
            return;
        }

        let directory_path = Path(ensure_trailing_directory_separator(
            &self.to_path(directory),
        ));
        if self
            .known_symlinks
            .borrow()
            .directories()
            .contains_key(&directory_path)
        {
            return;
        }

        let real_directory = self.realpath(directory);
        if real_directory == directory {
            // not symlinked
            return;
        }
        let real_path = Path(ensure_trailing_directory_separator(
            &self.to_path(&real_directory),
        ));
        if real_path == directory_path {
            // not symlinked
            return;
        }
        self.known_symlinks.borrow_mut().set_directory(
            directory,
            directory_path,
            Some(KnownDirectoryLink {
                real: ensure_trailing_directory_separator(&real_directory),
                real_path,
            }),
        );
    }

    // Go: projectreferencedtsfakinghost.go:166 (*projectReferenceDtsFakingVfs).fileOrDirectoryExistsUsingSource
    // PORT: Go `SyncMap.Range` stops at the first match. The link list is
    // copied first so `set_file` can borrow the symlink cache mutably.
    fn file_or_directory_exists_using_source(
        &self,
        file_or_directory: &str,
        is_file: bool,
    ) -> bool {
        let file_or_directory_exists_using_source = |name: &str| {
            if is_file {
                self.file_exists_if_project_reference_dts(name)
            } else {
                self.directory_exists_if_project_reference_decl_dir(name)
            }
        };
        // Check current directory or file
        let result = file_or_directory_exists_using_source(file_or_directory);
        if result != Tristate::Unknown {
            return result == Tristate::True;
        }

        let file_or_directory_path = self.to_path(file_or_directory);
        if !file_or_directory_path.contains("/node_modules/") {
            return false;
        }
        // Check if the directory or file is a symlinked package
        let package_root = parse_node_module_from_path(file_or_directory, true /*isFolder*/);
        if !package_root.is_empty() {
            self.handle_directory_could_be_symlink(&package_root);
        }
        // PORT: Go stores `*KnownDirectoryLink`; a nil link would panic at
        // the `RealPath` read, and this code never stores one.
        let known_directory_links: Vec<(Path, KnownDirectoryLink)> = self
            .known_symlinks
            .borrow()
            .directories()
            .iter()
            .map(|(path, link)| {
                (
                    path.clone(),
                    link.clone().expect("known directory link is set"),
                )
            })
            .collect();
        if known_directory_links.is_empty() {
            return false;
        }
        if is_file
            && self
                .known_symlinks
                .borrow()
                .files()
                .contains_key(&file_or_directory_path)
        {
            return true;
        }

        // If it contains node_modules check if its one of the symlinked path we know of
        let mut exists = false;
        for (directory_path, known_directory_link) in &known_directory_links {
            let Some(relative) = file_or_directory_path.strip_prefix(directory_path.as_str())
            else {
                continue;
            };
            exists = file_or_directory_exists_using_source(&format!(
                "{}{}",
                known_directory_link.real_path.as_str(),
                relative
            ))
            .is_true();
            if exists {
                if is_file {
                    // Store the real path for the file
                    let current_directory = self
                        .project_reference_file_mapper
                        .borrow()
                        .opts
                        .host
                        .get_current_directory();
                    let absolute_path =
                        get_normalized_absolute_path(file_or_directory, &current_directory);
                    let real = format!(
                        "{}{}",
                        known_directory_link.real,
                        &absolute_path[directory_path.len()..]
                    );
                    self.known_symlinks.borrow_mut().set_file(
                        &absolute_path,
                        file_or_directory_path.clone(),
                        &real,
                    );
                }
                break;
            }
        }
        exists
    }

    // Go: projectreferencedtsfakinghost.go:213 (*projectReferenceDtsFakingVfs).fileExistsIfProjectReferenceDts
    fn file_exists_if_project_reference_dts(&self, file: &str) -> Tristate {
        let source = self
            .project_reference_file_mapper
            .borrow()
            .get_project_reference_from_output_dts(&self.to_path(file));
        if let Some(source) = source {
            return if self.host_fs().file_exists(&source.source) {
                Tristate::True
            } else {
                Tristate::False
            };
        }
        Tristate::Unknown
    }

    // Go: projectreferencedtsfakinghost.go:221 (*projectReferenceDtsFakingVfs).directoryExistsIfProjectReferenceDeclDir
    // PORT: Go ranges over the set keys in map order; the result does not
    // depend on the order.
    fn directory_exists_if_project_reference_decl_dir(&self, dir: &str) -> Tristate {
        let dir_path = self.to_path(dir);
        for decl_dir_path in &self.dts_directories {
            if dir_path.contains_path(decl_dir_path) || decl_dir_path.contains_path(&dir_path) {
                return Tristate::True;
            }
        }
        Tristate::Unknown
    }
}

impl Fs for ProjectReferenceDtsFakingVfs {
    // Go: projectreferencedtsfakinghost.go:56 (*projectReferenceDtsFakingVfs).UseCaseSensitiveFileNames
    fn use_case_sensitive_file_names(&self) -> bool {
        self.host_fs().use_case_sensitive_file_names()
    }

    // Go: projectreferencedtsfakinghost.go:61 (*projectReferenceDtsFakingVfs).FileExists
    fn file_exists(&self, path: &str) -> bool {
        if self.host_fs().file_exists(path) {
            return true;
        }
        if !is_declaration_file_name(path) {
            return false;
        }
        // Project references go to source file instead of .d.ts file
        self.file_or_directory_exists_using_source(path, /*isFile*/ true)
    }

    // Go: projectreferencedtsfakinghost.go:73 (*projectReferenceDtsFakingVfs).ReadFile
    fn read_file(&self, path: &str) -> (String, bool) {
        // Dont need to override as we cannot mimick read file
        self.host_fs().read_file(path)
    }

    // Go: projectreferencedtsfakinghost.go:79 (*projectReferenceDtsFakingVfs).WriteFile
    fn write_file(&self, _path: &str, _data: &str) -> Result<(), FsError> {
        panic!("should not be called by resolver")
    }

    // Go: projectreferencedtsfakinghost.go:84 (*projectReferenceDtsFakingVfs).AppendFile
    fn append_file(&self, _path: &str, _data: &str) -> Result<(), FsError> {
        panic!("should not be called by resolver")
    }

    // Go: projectreferencedtsfakinghost.go:89 (*projectReferenceDtsFakingVfs).Remove
    fn remove(&self, _path: &str) -> Result<(), FsError> {
        panic!("should not be called by resolver")
    }

    // Go: projectreferencedtsfakinghost.go:94 (*projectReferenceDtsFakingVfs).Chtimes
    fn chtimes(
        &self,
        _path: &str,
        _a_time: Option<SystemTime>,
        _m_time: Option<SystemTime>,
    ) -> Result<(), FsError> {
        panic!("should not be called by resolver")
    }

    // Go: projectreferencedtsfakinghost.go:99 (*projectReferenceDtsFakingVfs).DirectoryExists
    fn directory_exists(&self, path: &str) -> bool {
        if self.host_fs().directory_exists(path) {
            self.handle_directory_could_be_symlink(path);
            return true;
        }
        self.file_or_directory_exists_using_source(path, /*isFile*/ false)
    }

    // Go: projectreferencedtsfakinghost.go:108 (*projectReferenceDtsFakingVfs).GetAccessibleEntries
    fn get_accessible_entries(&self, _path: &str) -> Entries {
        panic!("should not be called by resolver")
    }

    // Go: projectreferencedtsfakinghost.go:113 (*projectReferenceDtsFakingVfs).Stat
    fn stat(&self, _path: &str) -> Option<FileInfo> {
        panic!("should not be called by resolver")
    }

    // Go: projectreferencedtsfakinghost.go:123 (*projectReferenceDtsFakingVfs).Realpath
    fn realpath(&self, path: &str) -> String {
        if let Some(result) = self
            .known_symlinks
            .borrow()
            .files()
            .get(&self.to_path(path))
        {
            return result.clone();
        }
        self.host_fs().realpath(path)
    }
}
