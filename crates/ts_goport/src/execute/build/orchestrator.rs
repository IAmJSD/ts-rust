//! Go: execute/build/orchestrator.go, and `tscBuildCompilation` of
//! execute/tsc.go:90 (the `tsc -b` entry).
//!
//! The watch part (`Watch`, `updateWatch`, `resetCaches`,
//! `checkTasksForEventChanges`, `computeDesiredWatches`, `DoCycle`) is in
//! orchestrator_watch.rs.
//!
//! PORT: concurrency. Go runs `buildOrCleanProject` for the tasks of the
//! build order (`order`, or the part of it that an API build asks for,
//! ts#64158) on up to `numRoutines` goroutines (`rangeTasks`); each
//! goroutine takes the next task, waits for its upstream tasks, builds, and
//! closes the task's `built` channel. One more goroutine reports the tasks
//! in the same order, each when it is built (ts#64220). Go computes
//! `scheduleOrder` with the graph, but since ts#64158 the builders take the
//! tasks in build order. Tasks here are
//! `Rc<RefCell<BuildTask>>` on this thread, which makes, emits and releases
//! the program of every task (see build_task.rs). `build_all_tasks` keeps
//! the Go schedule: at most `numRoutines` tasks are taken and not yet
//! built, and tasks are taken and report in build order.
//! A taken task starts when its upstream tasks are done, and a task that
//! compiles makes its program at once (`build_project_start`) and starts
//! its check on the program's checker threads, so the checkers of the
//! started tasks work at the same time, as the Go goroutines do. Each
//! checker emits when its check ends, and the emit keeps its writes in
//! memory. The started tasks write their outputs one at a time
//! (`build_project_finish`), in the order their check and emit end, as each
//! Go builder writes when its own task ends. PORT (determinism): when tasks
//! can see each other's writes (shared_outputs.rs), all tasks finish in
//! build order instead. Tasks start before a started task writes, so a task
//! that runs beside others in Go reads the file system before they write
//! their outputs. Every task uses `o.host` and its caches (parsed `.d.ts` and
//! `.json` files, configs, the cached file system, the mtimes), as in Go.
//! Outside tests each program is released when its task is built, as Go
//! drops it there; in tests when its task reports. Its checker threads
//! free it in the background. In a build where the output does not depend
//! on it, a first task that compiles as a light rebuild compiles on a
//! builder thread instead (`first_task_uses_builder`), which also checks,
//! emits, writes and releases its program (builders.rs). When every task
//! that compiles is a light rebuild (`later_tasks_use_builders`), the
//! later ones do too, and load at the same time with the parses of the
//! first program; else they compile on this thread with those parses. The
//! schedule above does not change, and a finish waits for the loads that
//! run on builders. Where Go does task
//! work on its goroutines that needs no task state, threads do it ahead of
//! this thread: the file name match of each config (config_prefetch.rs),
//! and the build info read, its check parts and the source mtimes of each
//! task (`BuildInfoPrefetch`).
//!
//! PORT: the task keeps its project statistics (see build_task.rs), and
//! `report_task` adds them to the aggregate `--diagnostics` and
//! `--extendedDiagnostics` statistics.
//!
//! PORT: testing. `opts.testing` is `None` outside tests. A test compiles
//! each started task to the end at once, in build order, so the one test
//! file system and clock see one ordered sequence (see `build_all_tasks`).

use crate::contentmapper;
use crate::execute::build::build_task::*;
use crate::execute::build::builders::{BuilderSetup, Builders};
use crate::execute::build::command_line::ParsedBuildCommandLine;
use crate::execute::build::config_prefetch::{ConfigPrefetch, PrefetchPool};
use crate::execute::build::host::BuildHost;
use crate::execute::build::shared_outputs::{PathKeys, outputs_overlap};
use crate::execute::build::up_to_date_status::UpToDateStatusType;
use crate::execute::incremental::build_info::{BuildInfo, is_build_info_file_name_default_library};
use crate::execute::incremental::incremental::{new_build_info_reader, parse_build_info};
use crate::execute::tsc::compile::{
    CommandLineResult, ExitStatus, System, Watcher, Writer, new_content_mapper_host, write_str,
};
use crate::execute::tsc::diagnostics::{
    DiagnosticReporter, DiagnosticsReporter, create_builder_status_reporter,
    create_diagnostic_reporter, create_report_error_summary, create_watch_status_reporter,
};
use crate::execute::tsc::statistics::Statistics;
use crate::execute::watchmanager::{WatchManager, new_watch_manager};
use crate::frontend::prelude::*;
use crate::gostd::Context;
// PORT: testing
use crate::execute::tsc::compile::CommandLineTesting;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::SystemTime;

// Go: build/orchestrator.go:27 Options
pub struct Options {
    pub sys: Rc<dyn System>,
    pub command: Rc<ParsedBuildCommandLine>,
    // PORT: testing. `None` outside tests.
    pub testing: Option<Rc<dyn CommandLineTesting>>,
}

// Go: build/orchestrator.go:32 OrchestratorResult (ts#64158)
// PORT: Go `FilesToDelete` is nil until a task adds a file, so an empty
// `Vec` is the Go nil.
#[derive(Default)]
pub struct OrchestratorResult {
    pub result: CommandLineResult,
    pub errors: Vec<Diagnostic>,
    pub statistics: Statistics,
    pub files_to_delete: Vec<String>,
}

impl OrchestratorResult {
    /// Go `&OrchestratorResult{Result: tsc.CommandLineResult{Status: status}}`.
    fn with_status(status: ExitStatus) -> Self {
        OrchestratorResult {
            result: CommandLineResult {
                status,
                watcher: None,
            },
            ..OrchestratorResult::default()
        }
    }

    // Go: build/orchestrator.go:39 (*OrchestratorResult).report
    fn report(&mut self, o: &Orchestrator) {
        self.report_with_files_to_delete(o, true);
    }

    // Go: build/orchestrator.go:43 (*OrchestratorResult).reportWithFilesToDelete (ts#64158)
    fn report_with_files_to_delete(&mut self, o: &Orchestrator, report_files_to_delete: bool) {
        if o.opts.command.compiler_options.watch.is_true() {
            let message = if self.errors.len() == 1 {
                diag::Found_1_error_Watching_for_file_changes
            } else {
                diag::Found_0_errors_Watching_for_file_changes
            };
            (o.watch_status_reporter
                .as_ref()
                .expect("watch status reporter"))(&new_compiler_diagnostic(
                message,
                args![self.errors.len()],
            ));
        } else {
            (o.error_summary_reporter
                .as_ref()
                .expect("error summary reporter"))(&self.errors);
        }
        if report_files_to_delete && !self.files_to_delete.is_empty() {
            (o.create_builder_status_reporter())(&new_compiler_diagnostic(
                diag::A_non_dry_build_would_delete_the_following_files_Colon_0,
                args![
                    self.files_to_delete
                        .iter()
                        .map(|f| format!("\r\n * {f}"))
                        .collect::<String>()
                ],
            ));
        }
        if !o.opts.command.compiler_options.diagnostics.is_true()
            && !o
                .opts
                .command
                .compiler_options
                .extended_diagnostics
                .is_true()
        {
            return;
        }
        self.statistics.set_total_time(o.opts.sys.since_start());
        self.statistics
            .report_to(&o.opts.sys.writer(), o.opts.testing.clone());
    }
}

// Go: build/orchestrator.go:66 Orchestrator
// PORT: Go `*SyncMap` of tasks is a plain map; tasks are
// `Rc<RefCell<BuildTask>>`. Go `wm *watchmanager.WatchManager` is
// `Rc<RefCell<WatchManager>>`, so the watch loop can run while `DoCycle`
// borrows the orchestrator (see orchestrator_watch.rs).
pub struct Orchestrator {
    pub(crate) opts: Options,
    pub(crate) compare_paths_options: ComparePathsOptions,
    pub(crate) host: Rc<BuildHost>,

    // contentMapperHost transforms content-mapped files; it is created once per build session (when
    // enabled) and shared across all projects so mapper processes are consolidated. It closes itself when
    // the session context is cancelled (see contentmapper.New).
    // PORT: Go nil is `None`.
    pub(crate) content_mapper_host: Option<Rc<dyn contentmapper::Host>>,

    // order generation result
    tasks: FxHashMap<Path, Rc<RefCell<BuildTask>>>,
    pub(crate) order: Vec<String>,
    errors: Vec<Diagnostic>,
    graph_generated: bool,

    error_summary_reporter: Option<DiagnosticsReporter>,
    pub(crate) watch_status_reporter: Option<DiagnosticReporter>,

    // fswatch event-based watching
    pub(crate) wm: Rc<RefCell<WatchManager>>,
    // order sorted by dependency depth, to reduce how often builders block on upstream projects
    pub(crate) schedule_order: Vec<String>,

    // PORT: not in Go (perf). The build info files that threads read ahead
    // of the up-to-date checks of this build cycle (`BuildInfoPrefetch`).
    build_info_prefetch: RefCell<Option<BuildInfoPrefetch>>,
    // PORT: not in Go (perf). The threads that parsed the configs of the
    // graph (`start_config_prefetch`), until the build info reads take them
    // (`start_build_info_prefetch`) or the build ends.
    prefetch_pool: RefCell<Option<PrefetchPool>>,
    // PORT: not in Go (perf). The released programs of built tasks whose
    // frontend programs are not freed yet (`release_task_program`), oldest
    // first. Go's GC frees them in the background. Their `Rc` data frees on
    // this thread: when it would wait for a task (`build_all_tasks`), or
    // when more than `MAX_KEPT_RELEASED` wait.
    released: RefCell<VecDeque<crate::program::ReleasedProgram>>,
    // PORT: not in Go (perf). True when the process ends after this `tsc -b`
    // build (`start_exported`, not in watch mode or a test), so what the
    // build keeps is not freed.
    ends_process: std::cell::Cell<bool>,
    // PORT: not in Go (perf). The check parts that a thread made from the
    // build info that `read_build_info_file` gave last, and its file name.
    status_prefetch: RefCell<Option<(String, StatusPrefetch)>>,
}

impl Orchestrator {
    // Go: build/orchestrator.go:93 (*Orchestrator).relativeFileName
    pub fn relative_file_name(&self, file_name: &str) -> String {
        convert_to_relative_path(file_name, &self.compare_paths_options)
    }

    // Go: build/orchestrator.go:97 (*Orchestrator).toPath
    pub fn to_path(&self, file_name: &str) -> Path {
        to_path(
            file_name,
            &self.compare_paths_options.current_directory,
            self.compare_paths_options.use_case_sensitive_file_names,
        )
    }

    // Go: build/orchestrator.go:101 (*Orchestrator).resolveBuildInfoFileName
    pub fn resolve_build_info_file_name(&self, file_name: &str, build_info_dir: &str) -> String {
        if is_build_info_file_name_default_library(file_name) {
            return combine_paths(
                &CompilerHost::default_library_path(&*self.host),
                &[file_name],
            );
        }
        get_normalized_absolute_path(file_name, build_info_dir)
    }

    // Go: build/orchestrator.go:108 (*Orchestrator).Order
    pub fn order(&self) -> &[String] {
        &self.order
    }

    // Go: build/orchestrator.go:113 (*Orchestrator).ScheduleOrder (ts#64220)
    // ScheduleOrder is the order in which builders pick up projects: Order() stably sorted by dependency depth.
    pub fn schedule_order(&self) -> &[String] {
        &self.schedule_order
    }

    // Go: build/orchestrator.go:126 (*Orchestrator).computeScheduleOrder (ts#64220)
    // computeScheduleOrder sorts the build order by dependency depth (projects with no
    // upstream first, then their dependents, and so on). Builders take projects from this
    // order and block until upstream projects are done, so with the plain depth-first order
    // a builder that picks the root of a long chain sits idle while another builder works
    // through the chain, even when unrelated projects are ready to build. Depth order reduces
    // that avoidable blocking but does not eliminate it: a shallower project that has been
    // picked up may not be done yet, so a builder can take a dependent of a slow project and
    // wait on that project while a later project's upstream has already finished. The stable
    // sort preserves the original order within a depth, and reporting still follows Order().
    // PORT: Go keys `depths` by `*BuildTask`; the key is the task's `Rc`
    // pointer. A missing key is Go's zero depth.
    fn compute_schedule_order(&self) -> Vec<String> {
        struct ScheduleEntry {
            config: String,
            depth: i32,
        }
        let mut entries: Vec<ScheduleEntry> = Vec::with_capacity(self.order.len());
        let mut depths: FxHashMap<*const RefCell<BuildTask>, i32> =
            FxHashMap::with_capacity_and_hasher(self.order.len(), Default::default());
        for config in &self.order {
            let task = self.get_task(&self.to_path(config));
            let mut depth = 0;
            for upstream in &task.borrow().up_stream {
                let upstream_depth = depths
                    .get(&Rc::as_ptr(&upstream.task))
                    .copied()
                    .unwrap_or(0);
                depth = depth.max(upstream_depth + 1);
            }
            depths.insert(Rc::as_ptr(&task), depth);
            entries.push(ScheduleEntry {
                config: config.clone(),
                depth,
            });
        }
        // Go `slices.SortStableFunc`; `sort_by` is stable.
        entries.sort_by(|a, b| a.depth.cmp(&b.depth));
        entries.into_iter().map(|entry| entry.config).collect()
    }

    // Go: build/orchestrator.go:150 (*Orchestrator).Upstream
    pub fn upstream(&self, config_name: &str) -> Vec<String> {
        let path = self.to_path(config_name);
        let task = self.get_task(&path);
        let task = task.borrow();
        task.up_stream
            .iter()
            .map(|t| t.task.borrow().config.clone())
            .collect()
    }

    // Go: build/orchestrator.go:158 (*Orchestrator).Downstream
    pub fn downstream(&self, config_name: &str) -> Vec<String> {
        let path = self.to_path(config_name);
        let task = self.get_task(&path);
        let task = task.borrow();
        task.down_stream
            .iter()
            .map(|t| t.borrow().config.clone())
            .collect()
    }

    // Go: build/orchestrator.go:166 (*Orchestrator).getTask
    pub fn get_task(&self, path: &Path) -> Rc<RefCell<BuildTask>> {
        match self.tasks.get(path) {
            Some(task) => task.clone(),
            None => panic!("No build task found for {}", path.as_str()),
        }
    }

    // Go: build/orchestrator.go:174 (*Orchestrator).createBuildTasks
    // PORT: Go parses the configs in parallel on a work group; here they
    // parse depth first on one thread. The task map and each task's
    // `resolved` are the same, because a path is taken by the first
    // `LoadOrStore` in both. Threads match the file names of the configs
    // ahead of this thread (`start_config_prefetch`).
    fn create_build_tasks(
        &mut self,
        old_tasks: Option<&FxHashMap<Path, Rc<RefCell<BuildTask>>>>,
        configs: &[String],
    ) {
        for config in configs {
            let path = self.to_path(config);
            let mut task: Option<Rc<RefCell<BuildTask>>> = None;
            let mut build_info: Option<BuildInfoEntry> = None;
            if let Some(old_tasks) = old_tasks {
                if let Some(existing) = old_tasks.get(&path) {
                    if !existing.borrow().dirty {
                        // Reuse existing task if config is same
                        task = Some(existing.clone());
                    } else {
                        if let Some(project) = &existing.borrow().content_mapper_project {
                            let _ = project.close();
                        }
                        build_info = existing.borrow().build_info_entry.clone();
                    }
                }
            }
            let task = task.unwrap_or_else(|| {
                let mut task = BuildTask::new(config.clone(), old_tasks.is_none());
                task.build_info_entry = build_info;
                Rc::new(RefCell::new(task))
            });
            if self.tasks.contains_key(&path) {
                continue;
            }
            self.tasks.insert(path.clone(), task.clone());
            let resolved = self.host.get_resolved_project_reference(config, &path);
            {
                let mut task = task.borrow_mut();
                task.resolved = resolved.clone();
                task.up_stream = Vec::new();
            }
            if let Some(resolved) = resolved {
                let references = resolved.resolved_project_reference_paths().to_vec();
                if old_tasks.is_none() {
                    self.start_config_prefetch(&references);
                }
                self.create_build_tasks(old_tasks, &references);
            }
        }
    }

    /// PORT: not in Go (perf). Queues the parses of `references` on the
    /// config threads (config_prefetch.rs), and starts the threads when
    /// there are two references or more. Go parses the configs on a work
    /// group that is parallel unless `--singleThreaded`. Only for the first
    /// graph of a build that is not a watch (a watch parses changed configs
    /// again), and only on the OS file system (not in tests).
    fn start_config_prefetch(&self, references: &[String]) {
        if let Some(prefetch) = &*self.host.config_prefetch.borrow() {
            prefetch.queue(references);
            return;
        }
        let options = &self.opts.command.compiler_options;
        if references.len() < 2
            || options.watch.is_true()
            || options.single_threaded.is_true()
            || self.opts.testing.is_some()
            || !is_wrapped_os_fs(&self.opts.sys.fs())
        {
            return;
        }
        let Some(pool) = PrefetchPool::start() else {
            return;
        };
        let prefetch = ConfigPrefetch::start(
            &pool,
            (**options).clone(),
            self.host.command_line_raw(),
            &self.compare_paths_options,
        );
        prefetch.queue(references);
        *self.host.config_prefetch.borrow_mut() = Some(prefetch);
        *self.prefetch_pool.borrow_mut() = Some(pool);
    }

    // Go: build/orchestrator.go:210 (*Orchestrator).setupBuildTask
    // PORT: the Go `built` and `done` channels are dropped (see top).
    fn setup_build_task(
        &mut self,
        config_name: &str,
        down_stream: Option<&Rc<RefCell<BuildTask>>>,
        in_circular_context: bool,
        completed: &mut FxHashSet<Path>,
        analyzing: &mut FxHashSet<Path>,
        circularity_stack: &mut Vec<String>,
    ) -> Option<Rc<RefCell<BuildTask>>> {
        let path = self.to_path(config_name);
        let task = self.get_task(&path);
        if !completed.contains(&path) {
            if analyzing.contains(&path) {
                if !in_circular_context {
                    self.errors.push(new_compiler_diagnostic(
                        diag::Project_references_may_not_form_a_circular_graph_Cycle_detected_Colon_0,
                        args![circularity_stack.join("\n")],
                    ));
                }
                return None;
            }
            analyzing.insert(path.clone());
            circularity_stack.push(config_name.to_string());
            let resolved = task.borrow().resolved.clone();
            if let Some(resolved) = resolved {
                let references = resolved.resolved_project_reference_paths().to_vec();
                for (index, sub_reference) in references.iter().enumerate() {
                    let upstream = self.setup_build_task(
                        sub_reference,
                        Some(&task),
                        in_circular_context || resolved.project_references()[index].circular,
                        completed,
                        analyzing,
                        circularity_stack,
                    );
                    if let Some(upstream) = upstream {
                        task.borrow_mut().up_stream.push(UpstreamTask {
                            task: upstream,
                            ref_index: index,
                        });
                    }
                }
            }
            circularity_stack.pop();
            completed.insert(path);
            self.order.push(config_name.to_string());
        }
        if self.opts.command.compiler_options.watch.is_true() {
            if let Some(down_stream) = down_stream {
                task.borrow_mut().down_stream.push(down_stream.clone());
            }
        }
        Some(task)
    }

    // Go: build/orchestrator.go:252 (*Orchestrator).GenerateGraphReusingOldTasks
    pub fn generate_graph_reusing_old_tasks(&mut self) {
        let tasks = std::mem::take(&mut self.tasks);
        self.order = Vec::new();
        self.errors = Vec::new();
        self.generate_graph(Some(&tasks));
    }

    // Go: build/orchestrator.go:260 (*Orchestrator).GenerateGraph (ts#64220, ts#64158)
    pub fn generate_graph(&mut self, old_tasks: Option<&FxHashMap<Path, Rc<RefCell<BuildTask>>>>) {
        let projects = self.opts.command.resolved_project_paths().to_vec();
        // Parse all config files in parallel
        self.create_build_tasks(old_tasks, &projects);
        // The config threads stop after the config they parse.
        self.host.config_prefetch.borrow_mut().take();

        // Generate the graph
        let mut completed = FxHashSet::default();
        let mut analyzing = FxHashSet::default();
        let mut circularity_stack = Vec::new();
        for project in &projects {
            self.setup_build_task(
                project,
                None,
                false,
                &mut completed,
                &mut analyzing,
                &mut circularity_stack,
            );
        }
        self.schedule_order = self.compute_schedule_order();
        if let Some(old_tasks) = old_tasks {
            for (path, old_task) in old_tasks {
                if self
                    .tasks
                    .get(path)
                    .is_some_and(|task| Rc::ptr_eq(task, old_task))
                {
                    continue;
                }
                if let Some(project) = &old_task.borrow().content_mapper_project {
                    let _ = project.close();
                }
            }
        }
        self.graph_generated = true;
    }

    // Go: build/orchestrator.go:290 (*Orchestrator).Start
    // tsc -b entrypoint
    // PORT: Go `start` sets `result.Result.Watcher = o` in watch mode. The
    // watcher is the boxed orchestrator, so `Start` sets it after `start`.
    // `Watch` blocks in the watch loop until `ctx` ends
    // (orchestrator_watch.rs).
    pub fn start_exported(mut self: Box<Self>, ctx: &Context) -> CommandLineResult {
        // PORT: not in Go (perf). The process ends after `tsc -b`, and Go
        // does not free at exit: the orchestrator, its caches and the kept
        // released programs (`released`) are not freed. With a content
        // mapper host the orchestrator drops, as its projects close then.
        let ends_process =
            !self.opts.command.compiler_options.watch.is_true() && self.opts.testing.is_none();
        self.ends_process.set(ends_process);
        let mut result = self.start(ctx, "", false /*onlyReferences*/).result;
        if self.opts.command.compiler_options.watch.is_true() {
            result.watcher = Some(self as Box<dyn Watcher>);
        } else if ends_process && self.content_mapper_host.is_none() {
            std::mem::forget(self);
        }
        result
    }

    // Go: build/orchestrator.go:295 (*Orchestrator).Build (ts#64158)
    // orchestrator.Build() entrypoint for api
    // PORT: in watch mode the result has no watcher (see `start_exported`).
    pub fn build(&mut self, ctx: &Context, project: &str) -> OrchestratorResult {
        self.recheck_all_projects(project);
        self.start(ctx, project, false /*onlyReferences*/)
    }

    // Go: build/orchestrator.go:301 (*Orchestrator).BuildReferences (ts#64158)
    // orchestrator.BuildReferences() entrypoint for api
    pub fn build_references(&mut self, ctx: &Context, project: &str) -> OrchestratorResult {
        self.recheck_all_projects(project);
        self.start(ctx, project, true /*onlyReferences*/)
    }

    // Go: build/orchestrator.go:306 (*Orchestrator).start (ts#64158)
    // PORT: Go `defer o.contentMapperHost.Close()` runs at each return; the
    // returns break out of the `'start` block, and the close follows it.
    // Go also sets `result.Result.Watcher = o` (see `start_exported`).
    fn start(&mut self, ctx: &Context, project: &str, only_references: bool) -> OrchestratorResult {
        self.content_mapper_host =
            new_content_mapper_host(ctx, &self.opts.sys, &self.opts.command.compiler_options);
        let close_content_mapper_host = self.content_mapper_host.clone().filter(|_| {
            !self.opts.command.compiler_options.watch.is_true() || self.opts.testing.is_none()
        });
        let result = 'start: {
            if self.opts.command.compiler_options.watch.is_true() {
                (self
                    .watch_status_reporter
                    .as_ref()
                    .expect("watch status reporter"))(&new_compiler_diagnostic(
                    diag::Starting_compilation_in_watch_mode,
                    args![],
                ));
            }
            if self.graph_generated {
                self.generate_graph_reusing_old_tasks();
            } else {
                self.generate_graph(None);
            }
            let (mut order, ok) = self.get_build_order_for(project);
            if !ok {
                break 'start OrchestratorResult::with_status(
                    ExitStatus::InvalidProjectOutputsSkipped,
                );
            }
            if only_references && self.errors.is_empty() {
                if project.is_empty() {
                    break 'start OrchestratorResult::with_status(
                        ExitStatus::InvalidProjectOutputsSkipped,
                    );
                }
                // Go `order[:len(order)-1]`: the project itself is last.
                order.pop();
            }
            let result = self.build_or_clean_order(&order);
            if self.opts.command.compiler_options.watch.is_true() {
                self.watch(ctx);
            }
            result
        };
        if let Some(host) = close_content_mapper_host {
            let _ = host.close();
        }
        result
    }

    // Go: build/orchestrator.go:337 (*Orchestrator).recheckAllProjects (ts#64158)
    fn recheck_all_projects(&self, project: &str) {
        if !self.graph_generated {
            return;
        }
        let (order, ok) = self.get_build_order_for(project);
        if !ok {
            return;
        }
        self.range_tasks(
            &order,
            &mut |_path: &Path, task: &Rc<RefCell<BuildTask>>| {
                let mut task = task.borrow_mut();
                task.reset_status();
                let path = self.to_path(&task.config);
                task.reset_config(self, &path);
            },
        );
        *self
            .host
            .m_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = FxHashMap::default();
        self.reset_caches();
    }

    // Go: build/orchestrator.go:354 (*Orchestrator).Clean (ts#64158)
    // orchestrator.Clean() entrypoint for api
    pub fn clean_exported(&mut self, project: &str) -> OrchestratorResult {
        self.clean(project, false)
    }

    // Go: build/orchestrator.go:359 (*Orchestrator).CleanReferences (ts#64158)
    // orchestrator.CleanReferences() entrypoint for api
    pub fn clean_references(&mut self, project: &str) -> OrchestratorResult {
        self.clean(project, true)
    }

    // Go: build/orchestrator.go:363 (*Orchestrator).clean (ts#64158)
    // PORT: Go `task.buildInfoEntryMu` guards the entry; the task is on
    // this thread (build_task.rs).
    fn clean(&mut self, project: &str, only_references: bool) -> OrchestratorResult {
        if !self.graph_generated {
            self.generate_graph(None);
        }
        if !self.errors.is_empty() {
            let mut result = OrchestratorResult {
                result: CommandLineResult {
                    status: ExitStatus::ProjectReferenceCycleOutputsSkipped,
                    watcher: None,
                },
                errors: self.errors.clone(),
                ..OrchestratorResult::default()
            };
            result.report_with_files_to_delete(self, true);
            return result;
        }

        let (mut order, ok) = self.get_build_order_for(project);
        if !ok {
            return OrchestratorResult::with_status(ExitStatus::InvalidProjectOutputsSkipped);
        }
        if only_references {
            // Go `order[:len(order)-1]` panics on an empty order.
            assert!(!order.is_empty(), "slice bounds out of range [:-1]");
            order.pop();
        }

        let mut result = OrchestratorResult::default();
        result.statistics.projects = order.len() as i32;
        let dry = self.opts.command.build_options.dry.is_true();
        let report_diagnostic = self.create_diagnostic_reporter();
        for config in &order {
            let task = self.get_task(&self.to_path(config));
            let mut task = task.borrow_mut();
            let Some(resolved) = task.resolved.clone() else {
                let diagnostic =
                    new_compiler_diagnostic(diag::File_0_not_found, args![task.config.clone()]);
                report_diagnostic(&diagnostic);
                result.errors.push(diagnostic);
                continue;
            };

            let inputs: FxHashSet<Path> = resolved
                .file_names()
                .iter()
                .map(|file_name| self.to_path(file_name))
                .collect();
            let project_outputs = resolved.get_output_file_names();
            let mut deleted = false;
            for output_file in &project_outputs {
                deleted = self.clean_project_output(
                    output_file,
                    &inputs,
                    dry,
                    &mut result.files_to_delete,
                    &report_diagnostic,
                ) || deleted;
            }
            deleted = self.clean_project_output(
                &resolved.get_build_info_file_name(),
                &inputs,
                dry,
                &mut result.files_to_delete,
                &report_diagnostic,
            ) || deleted;
            if deleted {
                task.reset_status();
                task.build_info_entry = None;
            }
        }

        result.report_with_files_to_delete(self, dry);
        result
    }

    // Go: build/orchestrator.go:416 (*Orchestrator).getBuildOrderFor (ts#64158)
    // PORT: Go returns `o.order` itself for an empty project; this clones it.
    fn get_build_order_for(&self, project: &str) -> (Vec<String>, bool) {
        if project.is_empty() {
            return (self.order.clone(), true);
        }

        let config = resolve_config_file_name_of_project_reference(&resolve_path(
            &self.opts.sys.get_current_directory(),
            &[project],
        ));
        let Some(target) = self.tasks.get(&self.to_path(&config)).cloned() else {
            return (Vec::new(), false);
        };

        let mut projects: FxHashSet<Path> = FxHashSet::default();
        fn add_project_and_references(
            o: &Orchestrator,
            projects: &mut FxHashSet<Path>,
            task: &Rc<RefCell<BuildTask>>,
        ) {
            let task = task.borrow();
            let path = o.to_path(&task.config);
            if projects.contains(&path) {
                return;
            }
            projects.insert(path);
            for upstream in &task.up_stream {
                add_project_and_references(o, projects, &upstream.task);
            }
        }
        add_project_and_references(self, &mut projects, &target);

        let mut order: Vec<String> = Vec::with_capacity(projects.len());
        for config in &self.order {
            if projects.contains(&self.to_path(config)) {
                order.push(config.clone());
            }
        }
        (order, true)
    }

    // Go: build/orchestrator.go:452 (*Orchestrator).cleanProjectOutput (ts#64158)
    fn clean_project_output(
        &self,
        output_file: &str,
        inputs: &FxHashSet<Path>,
        dry: bool,
        files_to_delete: &mut Vec<String>,
        report_diagnostic: &DiagnosticReporter,
    ) -> bool {
        let fs = CompilerHost::fs(&*self.host);
        if output_file.is_empty()
            || inputs.contains(&self.to_path(output_file))
            || !fs.file_exists(output_file)
        {
            return false;
        }
        files_to_delete.push(output_file.to_string());
        if dry {
            return false;
        }
        if fs.remove(output_file).is_err() {
            report_diagnostic(&new_compiler_diagnostic(
                diag::Failed_to_delete_file_0,
                args![output_file],
            ));
            return false;
        }
        true
    }

    // Go: build/orchestrator.go:869 (*Orchestrator).buildOrClean
    pub(crate) fn build_or_clean(&mut self) -> CommandLineResult {
        let order = self.order.clone();
        self.build_or_clean_order(&order).result
    }

    // Go: build/orchestrator.go:873 (*Orchestrator).buildOrCleanOrder (ts#64158)
    fn build_or_clean_order(&mut self, order: &[String]) -> OrchestratorResult {
        if !self.opts.command.build_options.clean.is_true()
            && self.opts.command.build_options.verbose.is_true()
        {
            (self.create_builder_status_reporter())(&new_compiler_diagnostic(
                diag::Projects_in_this_build_Colon_0,
                args![
                    order
                        .iter()
                        .map(|p| format!("\r\n    * {}", self.relative_file_name(p)))
                        .collect::<String>()
                ],
            ));
        }
        let mut build_result = OrchestratorResult::default();
        if self.errors.is_empty() {
            build_result.statistics.projects = order.len() as i32;
            self.build_all_tasks(order, &mut build_result);
        } else {
            // Circularity errors prevent any project from being built
            build_result.result.status = ExitStatus::ProjectReferenceCycleOutputsSkipped;
            let report_diagnostic = self.create_diagnostic_reporter();
            for err in &self.errors {
                report_diagnostic(err);
            }
            build_result.errors = self.errors.clone();
        }
        build_result.report(self);
        build_result
    }

    // Go: build/orchestrator.go:924 the numRoutines part of (*Orchestrator).rangeTasks
    pub(crate) fn num_routines(&self) -> i64 {
        let mut num_routines = 4;
        if self.opts.command.compiler_options.single_threaded.is_true() {
            num_routines = 1;
        } else if let Some(builders) = self.opts.command.build_options.builders {
            num_routines = builders;
        }
        num_routines
    }

    // Go: build/orchestrator.go:923 (*Orchestrator).rangeTasks over `order`
    // with build/orchestrator.go:959 (*Orchestrator).buildOrCleanProject, and
    // the reporting goroutine of build/orchestrator.go:873 buildOrCleanOrder
    // (ts#64220, ts#64158).
    // PORT: see the top comment for the schedule. Go `numRoutines <= 0`
    // starts no builder, so no task runs; that is kept.
    fn build_all_tasks(&self, order: &[String], build_result: &mut OrchestratorResult) {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum State {
            NotTaken,
            Waiting,
            Compiling,
            Done,
        }
        let num_routines = self.num_routines();
        if num_routines <= 0 {
            return;
        }
        let num_routines = num_routines as usize;
        let clean = self.opts.command.build_options.clean.is_true();
        // PORT: testing (see the top comment)
        let testing = self.opts.testing.is_some();
        let paths: Vec<Path> = order.iter().map(|c| self.to_path(c)).collect();
        // PORT: not in Go (perf). Whether the programs can load on builder
        // threads (builders.rs), made when the first task compiles.
        let builders_setting = self.builders_setting(num_routines);
        let mut builders: Option<Builders> = None;
        // PORT: not in Go (perf). With builders, whether the tasks after the
        // first one that compiles go to them too, known when the second one
        // compiles (`later_tasks_use_builders`). With `Some(false)` they
        // compile on this thread.
        let mut later_on_builders = None;
        if !clean {
            *self.build_info_prefetch.borrow_mut() =
                self.start_build_info_prefetch(&paths, builders_setting == BuildersSetting::Light);
        }
        // The threads of the config parses end once their queued reads
        // are done.
        self.prefetch_pool.borrow_mut().take();
        let index_of: FxHashMap<Path, usize> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();
        let mut states = vec![State::NotTaken; paths.len()];
        // PORT: perf. Each checker thread of a compiling task's program
        // drops a `ReadySignal` of the task when the check and emit that it
        // started are done (`BuildTask::notify_when_compiled`); `signals`
        // counts the signals that each task still waits for. `compiled`
        // holds the compiling tasks whose signals have all arrived, in the
        // order their last signal arrived: the order in which their checks
        // and emits ended.
        let (ready, ready_calls) = std::sync::mpsc::channel::<usize>();
        let mut signals = vec![0usize; paths.len()];
        let mut compiled = VecDeque::new();
        fn signal_arrived(signals: &mut [usize], compiled: &mut VecDeque<usize>, index: usize) {
            signals[index] -= 1;
            if signals[index] == 0 {
                compiled.push_back(index);
            }
        }
        // PORT: not in Go (determinism). True when the tasks can see each
        // other's writes (`outputs_overlap`), so they finish in build order.
        // It is found when the first task compiles: until then every task
        // was done when it started. With one builder the order is the same.
        let mut in_build_order = false;
        let mut overlap_checked = false;
        // Tasks taken (Go `currentTaskIndex`), taken and not built, and
        // reported. The tasks before `next_report` are built.
        let mut next_take = 0;
        let mut in_flight = 0;
        let mut next_report = 0;
        while next_report < paths.len() {
            // Each free builder takes the next task in order.
            while in_flight < num_routines && next_take < paths.len() {
                states[next_take] = State::Waiting;
                next_take += 1;
                in_flight += 1;
            }
            // A taken task starts once its upstream tasks are done
            // (Go `waitOnUpstream`; `cleanProject` does not wait). A task
            // that compiles makes its program now. A built task frees its
            // builder (Go `close(task.built)`).
            let mut progressed = false;
            for index in next_report..next_take {
                if states[index] != State::Waiting {
                    continue;
                }
                let task = self.get_task(&paths[index]);
                if !clean {
                    let upstream_done = task.borrow().up_stream.iter().all(|upstream| {
                        let path = self.to_path(&upstream.task.borrow().config);
                        index_of
                            .get(&path)
                            .is_none_or(|&i| states[i] == State::Done)
                    });
                    if !upstream_done {
                        continue;
                    }
                }
                let compiles = {
                    let mut task = task.borrow_mut();
                    task.result = Some(TaskResult::new(
                        self.create_task_builder_status_reporter(),
                        self.create_task_diagnostic_reporter(),
                    ));
                    if clean {
                        task.clean_project(self, &paths[index]);
                        false
                    } else {
                        task.build_project_check(self, &paths[index])
                    }
                };
                if compiles && !testing && num_routines > 1 {
                    if !overlap_checked {
                        overlap_checked = true;
                        in_build_order = self.outputs_overlap(&paths);
                        if !in_build_order
                            && self.first_task_uses_builder(builders_setting, &task.borrow())
                        {
                            // A forecast that is known already decides for
                            // every task now.
                            later_on_builders =
                                self.known_later_tasks_use_builders(builders_setting);
                            if later_on_builders != Some(false) {
                                builders = self.start_builders(num_routines, &ready);
                            }
                        }
                        if builders.is_none() || later_on_builders.is_some() {
                            self.end_forecast();
                        }
                    } else if builders.is_some() && later_on_builders.is_none() {
                        later_on_builders = Some(self.later_tasks_use_builders(builders_setting));
                        self.end_forecast();
                    }
                }
                // The read rule (builders.rs): beside builders, this thread
                // loads a program only when no builder loads one.
                if compiles
                    && later_on_builders == Some(false)
                    && let Some(builders) = &builders
                {
                    builders.wait_for_loads();
                }
                let mut task = task.borrow_mut();
                states[index] = if !compiles {
                    State::Done
                } else if let Some(builders) = builders
                    .as_mut()
                    .filter(|_| later_on_builders != Some(false))
                {
                    builders.compile(index, task.compile_job(&paths[index]));
                    signals[index] = 1;
                    State::Compiling
                } else if !task.build_project_compile(self, &paths[index]) {
                    State::Done
                } else if testing {
                    task.build_project_finish(self, &paths[index]);
                    State::Done
                } else {
                    signals[index] = task.notify_when_compiled(|| ReadySignal {
                        index,
                        ready: ready.clone(),
                    });
                    if signals[index] == 0 {
                        compiled.push_back(index);
                    }
                    State::Compiling
                };
                if states[index] == State::Done {
                    self.task_built(&mut task);
                    in_flight -= 1;
                    progressed = true;
                }
            }
            // Tasks report in order, each when it is built.
            while next_report < paths.len() && states[next_report] == State::Done {
                let task = self.get_task(&paths[next_report]);
                self.report_task(&mut task.borrow_mut(), build_result);
                next_report += 1;
                progressed = true;
            }
            if progressed {
                continue;
            }
            // No task can start or report, so a taken task compiles (the
            // first task that is not built, `next_report`, has its upstream
            // tasks done). A Go builder writes the outputs of its task
            // when the task's check ends, and then takes the next task. So
            // the task whose started check and emit ended first finishes
            // now: it writes its outputs, and its builder takes the next
            // task. When the outputs overlap, only the first task that is not
            // built finishes, as in Go when the tasks end in build order.
            // When no task can finish yet, this waits for a signal, and frees
            // a kept released program first.
            let index = loop {
                while let Ok(index) = ready_calls.try_recv() {
                    signal_arrived(&mut signals, &mut compiled, index);
                }
                let can_finish = |&index: &usize| !in_build_order || index == next_report;
                if let Some(at) = compiled.iter().position(can_finish) {
                    break compiled.remove(at).expect("the position is in the queue");
                }
                if !self.free_released() {
                    let index = ready_calls.recv().expect("this thread keeps a sender");
                    signal_arrived(&mut signals, &mut compiled, index);
                }
            };
            let task = self.get_task(&paths[index]);
            let mut task = task.borrow_mut();
            match builders
                .as_mut()
                .filter(|builders| builders.compiles(index))
            {
                Some(builders) => {
                    let compiled = builders.finish(index);
                    task.finish_compile_job(self, &paths[index], compiled);
                }
                None => task.build_project_finish(self, &paths[index]),
            }
            states[index] = State::Done;
            self.task_built(&mut task);
            in_flight -= 1;
        }
        // The builders end; they free their programs unless the process
        // ends after this build.
        drop(builders);
        self.host.end_shared_parses();
        // The kept released programs free now, unless the process ends
        // after this build (`start_exported`).
        if !self.ends_process.get() {
            while self.free_released() {}
        }
        // A task that did not read its build info leaves its read unused.
        self.build_info_prefetch.borrow_mut().take();
        self.status_prefetch.borrow_mut().take();
        self.host.m_time_prefetch.borrow_mut().take();
    }

    // Go: build/orchestrator.go:959 (*Orchestrator).buildOrCleanProject,
    // after the build (ts#64220).
    fn task_built(&self, task: &mut BuildTask) {
        if self.opts.testing.is_none() {
            // The program is only needed by Testing.OnProgram at report time; drop it now so a task
            // that has finished but is not yet reported does not keep its program alive.
            if let Some(program) = task
                .result
                .as_mut()
                .and_then(|result| result.program.take())
            {
                self.keep_released(release_task_program(program));
            }
        }
    }

    /// PORT: not in Go (determinism). `outputs_overlap` for the tasks at
    /// `paths` (the build order of `build_all_tasks`).
    fn outputs_overlap(&self, paths: &[Path]) -> bool {
        let configs: Vec<_> = paths
            .iter()
            .map(|path| self.get_task(path).borrow().resolved.clone())
            .collect();
        // The file system without the build host's cache: that cache keeps
        // each lookup for the whole build, and this one looks up output
        // directories that do not exist yet.
        outputs_overlap(&configs, &self.opts.sys.fs(), &self.compare_paths_options)
    }

    /// PORT: not in Go (perf). Whether the tasks of this build can compile
    /// on builder threads (builders.rs). `Off` where the output of builders
    /// could differ from the output of this thread alone, or where they
    /// gain nothing:
    /// - one routine (`--singleThreaded`, `--builders 1`);
    /// - tests, watch mode, `--clean`, and content mappers;
    /// - a file system other than the OS one, or writes that only this
    ///   thread can make (`System::emit_writes_through_osvfs`);
    /// - a build host that keeps parses of an earlier build;
    /// - `GOPORT_TSCB_BUILDERS=0` (for A/B runs, and as a fallback).
    /// Else `Always` with `GOPORT_TSCB_BUILDERS=1`, and `Light` otherwise.
    /// Tasks that can see each other's writes (`outputs_overlap`) also
    /// compile on this thread; that is found when the first task compiles.
    fn builders_setting(&self, num_routines: usize) -> BuildersSetting {
        let setting = std::env::var_os("GOPORT_TSCB_BUILDERS");
        let options = &self.opts.command.compiler_options;
        let mut host_has_parses = false;
        self.host
            .source_files
            .for_each_stored(|_, _| host_has_parses = true);
        if num_routines < 2
            || self.opts.testing.is_some()
            || options.watch.is_true()
            || self.opts.command.build_options.clean.is_true()
            || self.content_mapper_host.is_some()
            || !is_wrapped_os_fs(&self.opts.sys.fs())
            || !self.opts.sys.emit_writes_through_osvfs()
            || host_has_parses
            || setting.as_ref().is_some_and(|value| value == "0")
        {
            BuildersSetting::Off
        } else if setting.is_some_and(|value| value == "1") {
            BuildersSetting::Always
        } else {
            BuildersSetting::Light
        }
    }

    /// PORT: not in Go (perf). True when the first task that compiles,
    /// `first`, compiles on a builder thread (builders.rs). With
    /// `BuildersSetting::Light`: when it only reports the errors of its
    /// build info or makes its pending emit. Unless the forecast knows the
    /// answer for the later tasks already, their decision waits for the
    /// second task that compiles (`later_tasks_use_builders`), behind the
    /// load of the first one.
    fn first_task_uses_builder(&self, setting: BuildersSetting, first: &BuildTask) -> bool {
        match setting {
            BuildersSetting::Off => false,
            BuildersSetting::Always => true,
            BuildersSetting::Light => first.status.as_ref().is_some_and(|status| {
                matches!(
                    status.kind,
                    UpToDateStatusType::OutOfDateBuildInfoWithErrors
                        | UpToDateStatusType::OutOfDateBuildInfoWithPendingEmit
                )
            }),
        }
    }

    /// PORT: not in Go (perf). True when the tasks that compile after the
    /// first one compile on builder threads too, decided when the second
    /// one has its status. With `BuildersSetting::Light`: when the build
    /// info threads forecast no heavy rebuild (no task with changed inputs)
    /// and light ones in two tasks that can compile at the same time
    /// (`RebuildForecast::light`). Else they compile on this thread, after
    /// the first program loads, with the parse cache of the build
    /// (host.rs `SharedSourceFiles`), as in a serial build.
    // PERF (tscbpar1, stable bins): parallel loads cut hono-b noop (3 light
    // rebuilds) from about 122 to 101 ms on the minis. Where a task checks,
    // the checkers keep the cores busy, and before the builders shared the
    // parses of the first program (tscbpar1 round c) each builder parsed the
    // shared `.d.ts` files of its loads again. With builders
    // in every build, hono-b cold was 6.5% and wide cold 4% slower. With
    // builders after a light first compile only, a hono-b rebuild after an
    // edit of a test file of one later project used 50% to 90% more CPU and
    // 20% to 45% more memory, for the same time or 1% to 2% more. One light
    // task alone gains nothing, and neither do light tasks of which no two
    // can compile at the same time (a chain): there builders cost 0.5 to
    // 0.9 ms (tscbpar1 round c). Before the decision waited for the
    // second task that compiles (tscbpar1 round c), it waited at the first
    // one for the forecast: about 1 ms in wide-1err.
    fn later_tasks_use_builders(&self, setting: BuildersSetting) -> bool {
        match setting {
            BuildersSetting::Off => false,
            BuildersSetting::Always => true,
            BuildersSetting::Light => self
                .build_info_prefetch
                .borrow()
                .as_ref()
                .is_some_and(BuildInfoPrefetch::light_rebuilds),
        }
    }

    /// PORT: not in Go (perf). `later_tasks_use_builders` when the forecast
    /// is known without a wait, else None.
    fn known_later_tasks_use_builders(&self, setting: BuildersSetting) -> Option<bool> {
        match setting {
            BuildersSetting::Off => Some(false),
            BuildersSetting::Always => Some(true),
            BuildersSetting::Light => self
                .build_info_prefetch
                .borrow()
                .as_ref()
                .map_or(Some(false), BuildInfoPrefetch::known_light_rebuilds),
        }
    }

    /// PORT: not in Go (perf). The builders are decided: the build info
    /// threads forecast nothing more (`RebuildForecast::end`).
    fn end_forecast(&self) {
        if let Some(prefetch) = &*self.build_info_prefetch.borrow() {
            prefetch.end_forecast();
        }
    }

    /// PORT: not in Go (perf). The builder threads of a parallel build
    /// (builders.rs), at most `num_routines`, which send the index of each
    /// task whose check and emit are done to `ready`
    /// (`first_task_uses_builder`).
    /// None when a config cannot go to a builder (`BuildHost::builder_shared`).
    /// The output is the same with or without builders.
    /// This thread then publishes the stores of the configs that it parsed,
    /// so the builders can read them, and takes its file ids in runs
    /// (`ast::reserve_file_ids`) from now on, as the builders do.
    fn start_builders(
        &self,
        num_routines: usize,
        ready: &std::sync::mpsc::Sender<usize>,
    ) -> Option<Builders> {
        let shared = self.host.builder_shared()?;
        let cwd = self.opts.sys.get_current_directory();
        crate::program::publish_parsed_files(&cwd);
        crate::ast::reserve_file_ids();
        let setup = BuilderSetup {
            shared,
            command: self.opts.command.to_send(),
            compare_paths_options: self.compare_paths_options.clone(),
            cwd,
            default_library_path: self.opts.sys.default_library_path(),
            start: std::time::Instant::now()
                .checked_sub(self.opts.sys.since_start())
                .unwrap_or_else(std::time::Instant::now),
            ends_process: self.ends_process.get(),
        };
        Some(Builders::new(setup, num_routines, ready.clone()))
    }

    /// PORT: not in Go (perf). Keeps `released` to free later (see
    /// `released`).
    fn keep_released(&self, released: crate::program::ReleasedProgram) {
        let oldest = {
            let mut kept = self.released.borrow_mut();
            kept.push_back(released);
            (kept.len() > MAX_KEPT_RELEASED).then(|| kept.pop_front())
        };
        drop(oldest);
    }

    /// PORT: not in Go (perf). Frees the oldest kept released program.
    /// False when none is kept.
    fn free_released(&self) -> bool {
        let oldest = self.released.borrow_mut().pop_front();
        oldest.is_some()
    }

    /// PORT: not in Go (perf). Starts reading the build info files that the
    /// up-to-date checks of the tasks at `paths` will read (see
    /// `BuildInfoPrefetch`), on the threads of the config parses
    /// (`prefetch_pool`), or on new ones when there are none. None
    /// when there is nothing to gain or the read could differ from the task's
    /// own read: one routine (`--singleThreaded` or `--builders 1`: Go
    /// checks one task at a time), `--force` (no check reads the build
    /// info), a file system other than the OS one (tests), or fewer than
    /// two files. A solution (Go `upToDateStatusTypeSolution`) and a task
    /// that keeps the build info of an earlier cycle (watch) read nothing.
    /// A build info file that two tasks name is left out, so no task of the
    /// build writes a prefetched file before its task reads it. Names are
    /// compared by key (`PathKeys::build_info_key`, as `outputs_overlap`
    /// does), so two names of one file through a symbolic link, even one
    /// that does not resolve before the build writes the file, are one file.
    /// When a build info key cannot be trusted (a file with more than one
    /// hard link, or a link target with `..` after a name), that file can
    /// be any build info file of the build, so nothing is read.
    /// The threads also make the check parts of each build info
    /// (`StatusPrefetch`) and read the mtimes of its task's TypeScript
    /// sources (`BuildHost::m_time_prefetch`). With `forecast`, they also
    /// forecast the rebuild of each task (`RebuildForecast`).
    fn start_build_info_prefetch(
        &self,
        paths: &[Path],
        forecast: bool,
    ) -> Option<BuildInfoPrefetch> {
        let num_routines = usize::try_from(self.num_routines()).unwrap_or(0);
        if num_routines < 2
            || self.opts.command.build_options.force.is_true()
            || !is_wrapped_os_fs(&self.opts.sys.fs())
        {
            return None;
        }
        // The file system without the build host's cache (see
        // `outputs_overlap`).
        let fs = self.opts.sys.fs();
        let keys = PathKeys::new(&fs, &self.compare_paths_options);
        let mut named: FxHashMap<String, usize> = FxHashMap::default();
        // Each read with its build info key and the index of its task.
        let mut reads: Vec<(String, usize, BuildInfoRead)> = Vec::new();
        // A task that can compile and has no read: then no forecast is
        // light.
        let mut unread = false;
        for (index, path) in paths.iter().enumerate() {
            let task = self.get_task(path);
            let task = task.borrow();
            let Some(resolved) = &task.resolved else {
                continue;
            };
            let solution = resolved.file_names().is_empty() && resolved.has_project_references();
            let name = resolved.get_build_info_file_name();
            if name.is_empty() {
                unread |= !solution;
                continue;
            }
            let build_info_key = keys.build_info_key(&name)?;
            *named.entry(build_info_key.clone()).or_default() += 1;
            let build_info_path = self.to_path(&name);
            let keeps = task
                .build_info_entry
                .as_ref()
                .is_some_and(|entry| entry.path == build_info_path);
            unread |= keeps;
            if !solution && !keeps {
                // Go's check compares the config files with the outputs too.
                let config_files = if forecast {
                    std::iter::once(task.config.clone())
                        .chain(resolved.extended_source_files().iter().cloned())
                        .collect()
                } else {
                    Vec::new()
                };
                reads.push((
                    build_info_key,
                    index,
                    BuildInfoRead {
                        name,
                        input_files: resolved.file_names().to_vec(),
                        check: StatusCheckOptions::new(resolved.compiler_options()),
                        config_files,
                    },
                ));
            }
        }
        let read_count = reads.len();
        let (tasks, reads): (Vec<usize>, Vec<BuildInfoRead>) = reads
            .into_iter()
            .filter_map(|(key, task, read)| (named[&key] == 1).then_some((task, read)))
            .unzip();
        unread |= reads.len() < read_count;
        if reads.len() < 2 {
            return None;
        }
        // The checks store about this many mtimes; the map grows once.
        let inputs: usize = reads.iter().map(|read| read.input_files.len()).sum();
        self.host
            .m_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reserve(inputs);
        let m_times: MTimePrefetch = Arc::default();
        let pool = match self.prefetch_pool.borrow_mut().take() {
            Some(pool) => pool,
            None => PrefetchPool::start()?,
        };
        // Two light rebuilds need two tasks that can compile, each with a
        // read, and that can compile at the same time. A build with no such
        // two tasks (a chain) forecasts nothing, as a serial build.
        let forecast = if forecast && !unread {
            let graph = TaskGraph::new(tasks, self.upstream_sets(paths));
            graph
                .has_independent_pair()
                .then(|| Arc::new(RebuildForecast::new(graph)))
        } else {
            None
        };
        let prefetch = BuildInfoPrefetch::start(
            &pool,
            reads,
            self.compare_paths_options.clone(),
            m_times.clone(),
            forecast,
        );
        drop(pool);
        *self.host.m_time_prefetch.borrow_mut() = Some(m_times);
        Some(prefetch)
    }

    /// PORT: not in Go (perf). For each task of `paths` (a build order, so
    /// upstream tasks come first), the tasks of `paths` that it waits for,
    /// directly or through others (`TaskGraph`).
    fn upstream_sets(&self, paths: &[Path]) -> Vec<TaskSet> {
        let index_of: FxHashMap<&Path, usize> =
            paths.iter().enumerate().map(|(i, p)| (p, i)).collect();
        let mut sets: Vec<TaskSet> = Vec::with_capacity(paths.len());
        for path in paths {
            let mut set = TaskSet::new(paths.len());
            for upstream in &self.get_task(path).borrow().up_stream {
                let upstream = self.to_path(&upstream.task.borrow().config);
                if let Some(&index) = index_of.get(&upstream) {
                    set.insert(index);
                    if let Some(of_upstream) = sets.get(index) {
                        set.union_with(of_upstream);
                    }
                }
            }
            sets.push(set);
        }
        sets
    }

    // Go: build/buildtask.go:119 (*BuildTask).report, the orchestrator part
    // (see `BuildTask::report`).
    fn report_task(&self, task: &mut BuildTask, build_result: &mut OrchestratorResult) {
        let (result, errors) = task.report();
        if !errors.is_empty() {
            build_result.errors.extend(errors);
        }
        write_str(&self.opts.sys.writer(), &result.builder);
        if result.exit_status.code() > build_result.result.status.code() {
            build_result.result.status = result.exit_status;
        }
        if let Some(statistics) = &result.statistics {
            build_result.statistics.aggregate(statistics);
        }
        // If we built the program, or updated timestamps, or had errors, we need to
        // delete files that are no longer needed
        match result.build_kind {
            BuildKind::Program => {
                // PORT: testing. The program is current for the call, as
                // the test reads its files.
                if let (Some(testing), Some(program)) = (&self.opts.testing, &result.program) {
                    let _scope = crate::core::enter_program(Some(program.get_program()));
                    testing.on_program(program);
                }
                build_result.statistics.projects_built += 1
            }
            BuildKind::Pseudo => build_result.statistics.timestamp_updates += 1,
            BuildKind::None => {}
        }
        build_result.files_to_delete.extend(result.files_to_delete);
        // Go drops `t.result` here (`t.result = nil`).
        if let Some(program) = result.program {
            self.keep_released(release_task_program(program));
        }
    }

    // Go: build/orchestrator.go:976 (*Orchestrator).getWriter with a nil task
    fn writer(&self) -> Writer {
        self.opts.sys.writer()
    }

    // Go: build/orchestrator.go:983 (*Orchestrator).createBuilderStatusReporter(nil)
    fn create_builder_status_reporter(&self) -> DiagnosticReporter {
        create_builder_status_reporter(
            self.opts.sys.clone(),
            self.writer(),
            &self.opts.command.locale(),
            &self.opts.command.compiler_options,
            self.opts.testing.clone(),
        )
    }

    // Go: build/orchestrator.go:987 (*Orchestrator).createDiagnosticReporter(nil)
    fn create_diagnostic_reporter(&self) -> DiagnosticReporter {
        create_diagnostic_reporter(
            &*self.opts.sys,
            self.writer(),
            &self.opts.command.locale(),
            &self.opts.command.compiler_options,
        )
    }

    // Go: build/orchestrator.go:983 (*Orchestrator).createBuilderStatusReporter(task)
    fn create_task_builder_status_reporter(&self) -> TaskDiagnosticReporter {
        task_reporter(|w| {
            create_builder_status_reporter(
                self.opts.sys.clone(),
                w,
                &self.opts.command.locale(),
                &self.opts.command.compiler_options,
                self.opts.testing.clone(),
            )
        })
    }

    // Go: build/orchestrator.go:987 (*Orchestrator).createDiagnosticReporter(task)
    fn create_task_diagnostic_reporter(&self) -> TaskDiagnosticReporter {
        task_reporter(|w| {
            create_diagnostic_reporter(
                &*self.opts.sys,
                w,
                &self.opts.command.locale(),
                &self.opts.command.compiler_options,
            )
        })
    }
}

// PORT: Go `getWriter(task)` gives the reporter `&task.result.builder`. A
// `TaskDiagnosticReporter` gets the builder on each call instead (see
// build_task.rs), so the tsc reporter writes into its own buffer, and the
// buffer moves into the builder after each call.
pub(crate) fn task_reporter(
    make: impl FnOnce(Writer) -> DiagnosticReporter,
) -> TaskDiagnosticReporter {
    let buffer: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
    let reporter = make(buffer.clone());
    Box::new(move |builder: &mut String, diagnostic: &Diagnostic| {
        reporter(diagnostic);
        let bytes = std::mem::take(&mut *buffer.borrow_mut());
        builder.push_str(&String::from_utf8_lossy(&bytes));
    })
}

impl BuildTaskOrchestrator for Orchestrator {
    fn command(&self) -> &ParsedBuildCommandLine {
        &self.opts.command
    }

    fn compare_paths_options(&self) -> &ComparePathsOptions {
        &self.compare_paths_options
    }

    fn relative_file_name(&self, file_name: &str) -> String {
        Orchestrator::relative_file_name(self, file_name)
    }

    fn to_path(&self, file_name: &str) -> Path {
        Orchestrator::to_path(self, file_name)
    }

    fn now(&self) -> SystemTime {
        self.opts.sys.now()
    }

    fn fs(&self) -> Rc<dyn Fs> {
        CompilerHost::fs(&*self.host)
    }

    fn get_m_time(&self, file: &str) -> Option<SystemTime> {
        self.host.get_m_time(file)
    }

    fn get_m_time_of_path(&self, file: &str, path: &Path) -> Option<SystemTime> {
        self.host.get_m_time_of_path(file, path)
    }

    fn set_m_time(&self, file: &str, m_time: SystemTime) -> Result<(), FsError> {
        self.host.set_m_time(file, Some(m_time))
    }

    fn store_m_time(&self, file: &str, m_time: SystemTime) {
        self.host.store_m_time(file, Some(m_time));
    }

    fn read_build_info_file(&self, config: &ParsedCommandLine) -> Option<Arc<BuildInfo>> {
        let name = config.get_build_info_file_name();
        let prefetched = self
            .build_info_prefetch
            .borrow_mut()
            .as_mut()
            .and_then(|prefetch| prefetch.take(&name));
        if let Some((build_info, status_prefetch)) = prefetched {
            *self.status_prefetch.borrow_mut() = status_prefetch.map(|status| (name, status));
            return build_info.map(Arc::new);
        }
        new_build_info_reader(self.host.clone() as Rc<dyn CompilerHost>)
            .read_build_info(config)
            .map(Arc::new)
    }

    fn take_status_prefetch(&self, build_info_file_name: &str) -> Option<StatusPrefetch> {
        let (name, status_prefetch) = self.status_prefetch.borrow_mut().take()?;
        (name == build_info_file_name).then_some(status_prefetch)
    }

    fn sys(&self) -> Rc<dyn System> {
        self.opts.sys.clone()
    }

    fn host(&self) -> Rc<BuildHost> {
        self.host.clone()
    }

    // PORT: testing
    fn testing(&self) -> Option<Rc<dyn CommandLineTesting>> {
        self.opts.testing.clone()
    }

    fn content_mapper_host(&self) -> Option<Rc<dyn contentmapper::Host>> {
        self.content_mapper_host.clone()
    }
}

/// PORT: not in Go (perf). Go checks whether up to `numRoutines` projects
/// are up to date at the same time, each on its goroutine, and each reads
/// and unmarshals its build info file there (`loadOrStoreBuildInfo`). The
/// orchestrator here checks one task at a time, so threads read and parse
/// the build info files first, in build order, and a task takes the parse
/// of its file (`take`), waiting for it when a thread has not finished
/// it. A thread reads with the OS file system of its thread, as the host
/// does (`ReadBuildInfo`: read the file, then `parse_build_info`).
///
/// After the parse, the thread does the other parts of the check that need
/// no task state: the root info reader and the paths of the file names
/// (`StatusPrefetch`), and the mtimes of the task's TypeScript sources
/// that are not declaration files, the root files and the files of the
/// build info (`MTimePrefetch`). No task of a build writes such a file, so
/// the mtime is the one that the check would read later. When the build
/// info shows that the check returns before these parts (errors, pending
/// emit: `StatusCheckOptions::reads_input_times`), the thread skips them,
/// as Go reads no input mtime there.
///
/// In a build that can use builder threads, each thread also forecasts
/// the rebuild of the task (`RebuildForecast`).
struct BuildInfoPrefetch {
    slots: FxHashMap<String, Arc<BuildInfoSlot>>,
    forecast: Option<Arc<RebuildForecast>>,
}

/// One build info file that the threads read, the root files of its task
/// (`resolved.FileNames()`) and the options that its check reads. For a
/// forecast, the config file of the task and its extended config files.
struct BuildInfoRead {
    name: String,
    input_files: Vec<String>,
    check: StatusCheckOptions,
    config_files: Vec<String>,
}

/// PORT: not in Go (perf). Whether a build can compile its tasks on
/// builder threads (`Orchestrator::builders_setting`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BuildersSetting {
    Off,
    /// For a first task that compiles as a light rebuild
    /// (`Orchestrator::first_task_uses_builder`), and for the later ones
    /// when every task that compiles is a light rebuild
    /// (`Orchestrator::later_tasks_use_builders`).
    Light,
    /// In every build that can (`GOPORT_TSCB_BUILDERS=1`).
    Always,
}

/// PORT: not in Go (perf). How the task of a build info file rebuilds, as
/// far as the mtimes of its build info, root and config files and its build
/// info tell (`unchanged_inputs`, `forecast_rebuild`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rebuild {
    /// The check finds the task up to date, unless an upstream task
    /// changes its outputs.
    None,
    /// The task compiles to report the errors of its build info or to make
    /// its pending emit, and no root or config file is newer than its
    /// build info: its program loads, and there is little to check.
    Light,
    /// The task builds with changed inputs, without a build info, or with
    /// another version or options, or the thread cannot tell.
    Heavy,
}

/// PORT: not in Go (perf). A set of tasks of a build, by their index in
/// the build order.
#[derive(Clone)]
struct TaskSet(Vec<u64>);

impl TaskSet {
    /// An empty set for a build of `tasks` tasks.
    fn new(tasks: usize) -> Self {
        TaskSet(vec![0; tasks.div_ceil(64)])
    }

    fn insert(&mut self, task: usize) {
        self.0[task / 64] |= 1 << (task % 64);
    }

    fn contains(&self, task: usize) -> bool {
        self.0
            .get(task / 64)
            .is_some_and(|word| word & (1 << (task % 64)) != 0)
    }

    fn union_with(&mut self, other: &TaskSet) {
        for (word, other) in self.0.iter_mut().zip(&other.0) {
            *word |= other;
        }
    }
}

/// PORT: not in Go (perf). The tasks of the reads of a forecast and the
/// tasks that each task of the build waits for (`upstream_sets`), to tell
/// which two reads can compile at the same time: neither waits for the
/// other.
struct TaskGraph {
    /// The task of each read.
    tasks: Vec<usize>,
    /// By task: the tasks that it waits for.
    upstream: Vec<TaskSet>,
}

impl TaskGraph {
    fn new(tasks: Vec<usize>, upstream: Vec<TaskSet>) -> Self {
        TaskGraph { tasks, upstream }
    }

    /// True when the tasks of reads `a` and `b` can compile at the same
    /// time.
    fn independent(&self, a: usize, b: usize) -> bool {
        let (a, b) = (self.tasks[a], self.tasks[b]);
        !self.upstream[a].contains(b) && !self.upstream[b].contains(a)
    }

    /// True when the tasks of two reads can compile at the same time.
    fn has_independent_pair(&self) -> bool {
        (0..self.tasks.len()).any(|a| (a + 1..self.tasks.len()).any(|b| self.independent(a, b)))
    }
}

/// PORT: not in Go (perf). The rebuilds that the build info threads
/// forecast for the tasks of a build (`Rebuild`), for
/// `Orchestrator::later_tasks_use_builders`. Builders gain only where the
/// loads of two or more tasks run at the same time and little is checked:
/// two light rebuilds whose tasks can compile at the same time (`graph`),
/// and no heavy one. A changed input shows in the mtimes alone
/// (`unchanged_inputs`), so it is known before a large build info is
/// parsed. Once the answer is "no", the forecast ends itself (`ended`), so
/// the threads do no more forecast work than a serial build.
struct RebuildForecast {
    state: Mutex<ForecastState>,
    changed: Condvar,
    /// The answer is "no", or the orchestrator made its decision (`end`):
    /// the mtimes jobs read nothing more.
    ended: std::sync::atomic::AtomicBool,
    graph: TaskGraph,
}

struct ForecastState {
    /// The reads whose mtimes are not read yet.
    unstated: usize,
    /// The reads whose build info is not parsed yet (`add_class`).
    unclassed: usize,
    /// The reads whose rebuild is not known yet (`add_parse`).
    unparsed: usize,
    /// By read: whether its build info makes a light rebuild, once parsed.
    classes: Vec<Option<bool>>,
    /// The reads with a light rebuild.
    light: Vec<usize>,
    /// Two reads of `light` can compile at the same time.
    pair: bool,
    /// A rebuild is heavy.
    heavy: bool,
}

impl ForecastState {
    /// `RebuildForecast::light` from what is known now, or None when it
    /// waits.
    fn light_now(&self) -> Option<bool> {
        if self.heavy || (!self.pair && self.unclassed == 0) {
            Some(false)
        } else if self.pair && self.unstated == 0 {
            Some(true)
        } else {
            None
        }
    }
}

impl RebuildForecast {
    fn new(graph: TaskGraph) -> Self {
        let reads = graph.tasks.len();
        RebuildForecast {
            state: Mutex::new(ForecastState {
                unstated: reads,
                unclassed: reads,
                unparsed: reads,
                classes: vec![None; reads],
                light: Vec::new(),
                pair: false,
                heavy: false,
            }),
            changed: Condvar::new(),
            ended: std::sync::atomic::AtomicBool::new(false),
            graph,
        }
    }

    /// The decision is made: the mtimes jobs that have not started stop.
    fn end(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Runs `update` on the state, ends the forecast when the answer is
    /// "no", and wakes the waits.
    fn update(&self, update: impl FnOnce(&mut ForecastState)) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        update(&mut state);
        if state.light_now() == Some(false) {
            self.end();
        }
        self.changed.notify_all();
    }

    /// The mtimes of a read (`unchanged_inputs`).
    fn add_mtimes(&self, unchanged: bool) {
        self.update(|state| {
            state.unstated -= 1;
            state.heavy |= !unchanged;
        });
    }

    /// Whether the build info of read `read` makes a light rebuild, as soon
    /// as it is parsed: before the thread reads the source mtimes of a
    /// check that reads them (`forecast_rebuild` tells the rest).
    fn add_class(&self, read: usize, light: bool) {
        self.update(|state| {
            state.unclassed -= 1;
            state.classes[read] = Some(light);
            if light {
                state.pair |= state
                    .light
                    .iter()
                    .any(|&other| self.graph.independent(read, other));
                state.light.push(read);
            }
        });
    }

    /// Whether the build info of read `read` makes a light rebuild, when it
    /// is parsed.
    fn class(&self, read: usize) -> Option<bool> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .classes[read]
    }

    /// The rebuild of a read, from its build info (`add_class` counted a
    /// light one).
    fn add_parse(&self, rebuild: Rebuild) {
        self.update(|state| {
            state.unparsed -= 1;
            state.heavy |= rebuild == Rebuild::Heavy;
        });
    }

    /// True when no rebuild is heavy and two light ones can compile at the
    /// same time. Waits until a rebuild is heavy, until every build info is
    /// parsed with no such two light rebuilds, or until there are two and
    /// the mtimes of every read are read. So a build with no such two
    /// light rebuilds waits for no mtime.
    /// A heavy rebuild that only a later parse shows (another version or
    /// options with no newer config file) is missed; that task compiles on
    /// a builder thread with the same output.
    fn light(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(light) = state.light_now() {
                return light;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// `light` when it would not wait, else None.
    fn known_light(&self) -> Option<bool> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .light_now()
    }
}

/// The mtimes that the build info threads read ahead of the checks, by
/// path. `BuildHost::load_or_store_m_time` takes an mtime from here where it
/// would read it from the file system.
pub(crate) type MTimePrefetch = Arc<Mutex<FxHashMap<Path, Option<SystemTime>>>>;

/// What a thread made for one build info file: Go `ReadBuildInfo`'s result
/// (`None` when the file cannot be read or parsed) and the check parts of
/// a parsed build info.
type BuildInfoResult = (Option<BuildInfo>, Option<StatusPrefetch>);

/// The result for one file: `None` until a thread has read it, then
/// `Some(None)` when the thread panicked, else `Some(Some(result))`.
#[derive(Default)]
struct BuildInfoSlot {
    result: Mutex<Option<Option<BuildInfoResult>>>,
    done: Condvar,
}

/// True for a TypeScript file that is not a declaration file. A build
/// never writes one (its outputs are JavaScript, declaration, map, JSON
/// and build info files).
fn is_typescript_source(file_name: &str) -> bool {
    file_extension_is_one_of(
        file_name,
        &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_MTS, EXTENSION_CTS],
    ) && !is_declaration_file_name(file_name)
}

/// The most released programs that `Orchestrator::released` keeps.
const MAX_KEPT_RELEASED: usize = 4;

/// PORT: not in Go (perf). Sends the build order index of its task when it
/// drops (see `build_all_tasks`).
struct ReadySignal {
    index: usize,
    ready: std::sync::mpsc::Sender<usize>,
}

impl Drop for ReadySignal {
    fn drop(&mut self) {
        let _ = self.ready.send(self.index);
    }
}

impl BuildInfoPrefetch {
    /// Starts reading and parsing the files of `reads`, in order, on the
    /// threads of `pool` (at most one per file), and puts the mtimes they
    /// read into `m_times`. With `forecast`, the threads also forecast the
    /// rebuild of each task: after the parses, a second job per file reads
    /// the mtimes of its task's files (`unchanged_inputs`), and each parse
    /// tells the rest (`forecast_rebuild`).
    // PORT: the threads only read, so their count changes no output; it
    // is not Go's `numRoutines`.
    fn start(
        pool: &PrefetchPool,
        reads: Vec<BuildInfoRead>,
        compare_paths_options: ComparePathsOptions,
        m_times: MTimePrefetch,
        forecast: Option<Arc<RebuildForecast>>,
    ) -> Self {
        let reads: Vec<Arc<PrefetchRead>> = reads
            .into_iter()
            .enumerate()
            .map(|(index, read)| {
                Arc::new(PrefetchRead {
                    read,
                    index,
                    slot: Arc::default(),
                    roots: RootTimes::default(),
                })
            })
            .collect();
        let slots = reads
            .iter()
            .map(|read| (read.read.name.clone(), read.slot.clone()))
            .collect();
        let mut jobs: Vec<PrefetchJob> = reads.iter().cloned().map(PrefetchJob::Parse).collect();
        if forecast.is_some() {
            // The reads with the fewest root files first: a changed input of
            // a small task ends the forecast before a large task reads its
            // root mtimes.
            let mut by_size = reads.clone();
            by_size.sort_by_key(|read| read.read.input_files.len());
            jobs.extend(by_size.into_iter().map(PrefetchJob::Mtimes));
        }
        let threads = pool.threads().min(reads.len());
        let queue = Mutex::new(jobs.into_iter());
        let thread_forecast = forecast.clone();
        pool.run(threads, move || {
            let fs = crate::frontend::bundled::wrap_fs(crate::frontend::vfs::osvfs_fs());
            loop {
                let next = queue.lock().unwrap_or_else(PoisonError::into_inner).next();
                match next {
                    None => break,
                    Some(PrefetchJob::Parse(read)) => parse_build_info_job(
                        &*fs,
                        &read,
                        &compare_paths_options,
                        &m_times,
                        thread_forecast.as_deref(),
                    ),
                    Some(PrefetchJob::Mtimes(read)) => {
                        let Some(forecast) = &thread_forecast else {
                            continue;
                        };
                        let unchanged = forecast.ended()
                            || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                unchanged_inputs(&*fs, &read, forecast)
                            }))
                            .unwrap_or(false);
                        forecast.add_mtimes(unchanged);
                    }
                }
            }
        });
        BuildInfoPrefetch { slots, forecast }
    }

    /// The build info of `name` that a thread read, and its check parts,
    /// once: `None` when it is not prefetched, was taken, or its read
    /// panicked; then the caller reads it. Waits for the thread that reads
    /// it.
    fn take(&mut self, name: &str) -> Option<BuildInfoResult> {
        let slot = self.slots.remove(name)?;
        let mut result = slot.result.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(read) = result.take() {
                return read;
            }
            result = slot
                .done
                .wait(result)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// True when the threads forecast a light rebuild for two or more tasks
    /// and no heavy one (`RebuildForecast::light`). False without a
    /// forecast.
    fn light_rebuilds(&self) -> bool {
        self.forecast
            .as_ref()
            .is_some_and(|forecast| forecast.light())
    }

    /// `light_rebuilds` when the threads know it already, else None.
    fn known_light_rebuilds(&self) -> Option<bool> {
        self.forecast
            .as_ref()
            .map_or(Some(false), |forecast| forecast.known_light())
    }

    /// The orchestrator made its decision: the forecast reads nothing more
    /// (`RebuildForecast::end`).
    fn end_forecast(&self) {
        if let Some(forecast) = &self.forecast {
            forecast.end();
        }
    }
}

/// One build info file of a `BuildInfoPrefetch`, and what its two jobs
/// share.
struct PrefetchRead {
    read: BuildInfoRead,
    /// The index of the read in the forecast (`RebuildForecast::add_class`).
    index: usize,
    slot: Arc<BuildInfoSlot>,
    /// With a forecast: the mtimes of the root files, read once by the
    /// first of the two jobs that needs them.
    roots: RootTimes,
}

/// A job of the build info threads. The parses come first, in build order,
/// so a forecast does not delay them.
enum PrefetchJob {
    /// Read and parse the build info, make its check parts and read the
    /// mtimes that the check reads; with a forecast, then forecast the
    /// rebuild from the build info (`forecast_rebuild`).
    Parse(Arc<PrefetchRead>),
    /// For a forecast: read the mtimes of the build info, config and root
    /// files (`unchanged_inputs`).
    Mtimes(Arc<PrefetchRead>),
}

/// The parse job of `read` (`PrefetchJob::Parse`).
fn parse_build_info_job(
    fs: &dyn Fs,
    read: &PrefetchRead,
    compare_paths_options: &ComparePathsOptions,
    m_times: &MTimePrefetch,
    forecast: Option<&RebuildForecast>,
) {
    let roots = forecast.map(|_| &read.roots);
    let index = read.index;
    // A read that panics is left to the task, which panics on it too.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let read = &read.read;
        let (data, ok) = fs.read_file(&read.name);
        let build_info = if ok { parse_build_info(&data) } else { None };
        let early = build_info
            .as_ref()
            .map(|build_info| read.check.early_return(build_info));
        if let Some(forecast) = forecast {
            forecast.add_class(index, early == Some(EarlyReturn::ErrorsOrPendingEmit));
        }
        // A check that returns before the input mtimes reads neither the
        // check parts nor the mtimes.
        let mut sources = None;
        let status = build_info
            .as_ref()
            .filter(|_| early == Some(EarlyReturn::No))
            .map(|build_info| {
                let status = StatusPrefetch::new(
                    build_info,
                    &read.name,
                    &read.input_files,
                    compare_paths_options,
                );
                sources = Some(prefetch_m_times(fs, read, &status, m_times, roots));
                status
            });
        // An ended forecast reads nothing more (`RebuildForecast::ended`).
        let rebuild = forecast.map(|forecast| {
            if forecast.ended() {
                Rebuild::None
            } else {
                forecast_rebuild(fs, read, early, sources)
            }
        });
        ((build_info, status), rebuild)
    }));
    let (result, rebuild) = match result {
        Ok((result, rebuild)) => (Some(result), rebuild),
        Err(_) => (None, Some(Rebuild::Heavy)),
    };
    *read
        .slot
        .result
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(result);
    read.slot.done.notify_all();
    if let Some(forecast) = forecast {
        forecast.add_parse(rebuild.unwrap_or(Rebuild::Heavy));
    }
}

/// The mtimes of the root files of a read, in order, read once by the
/// first of its two jobs that needs them (`get`).
#[derive(Default)]
struct RootTimes {
    state: Mutex<RootRead>,
    read: Condvar,
}

#[derive(Default)]
enum RootRead {
    #[default]
    Unread,
    /// A job reads them now.
    Reading,
    Read(Arc<[Option<SystemTime>]>),
}

/// Sets `RootTimes` to `Unread` when a read stops without a result, also
/// when it panics, and wakes the jobs that wait.
struct UnreadOnStop<'a>(&'a RootTimes, Option<Arc<[Option<SystemTime>]>>);

impl Drop for UnreadOnStop<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = match self.1.take() {
            Some(times) => RootRead::Read(times),
            None => RootRead::Unread,
        };
        self.0.read.notify_all();
    }
}

/// How many root files a read with `stop` reads between two checks.
const ROOT_STOP_CHECK: usize = 64;

impl RootTimes {
    /// The mtimes of `files`, the root files: read now by the first job
    /// that asks; a job that asks while another reads them waits for it.
    /// With `stop`, the read stops when `stop` is true between files and
    /// gives None; then the next job that asks reads them.
    fn get(
        &self,
        fs: &dyn Fs,
        files: &[String],
        stop: Option<&dyn Fn() -> bool>,
    ) -> Option<Arc<[Option<SystemTime>]>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            match &*state {
                RootRead::Read(times) => return Some(times.clone()),
                RootRead::Reading => {
                    state = self
                        .read
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                RootRead::Unread => break,
            }
        }
        *state = RootRead::Reading;
        drop(state);
        let mut done = UnreadOnStop(self, None);
        let mut times = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            if index % ROOT_STOP_CHECK == 0 && stop.is_some_and(|stop| stop()) {
                return None;
            }
            times.push(fs.stat(file).and_then(|stat| stat.mod_time()));
        }
        let times: Arc<[Option<SystemTime>]> = times.into();
        done.1 = Some(times.clone());
        Some(times)
    }
}

/// The newest of some mtimes, and whether a file had none.
#[derive(Clone, Copy, Default)]
struct NewestTime {
    newest: Option<SystemTime>,
    missing: bool,
}

impl NewestTime {
    fn add(&mut self, m_time: Option<SystemTime>) {
        match m_time {
            Some(m_time) => self.newest = self.newest.max(Some(m_time)),
            None => self.missing = true,
        }
    }

    fn newer_than(self, time: SystemTime) -> bool {
        self.missing || self.newest.is_some_and(|newest| newest > time)
    }
}

/// Reads the mtimes of the TypeScript sources (`is_typescript_source`) of
/// `read` (its root files and the files of its build info, `status`) into
/// `m_times`, as `BuildHost::get_m_time` reads them (`incremental.GetMTime`).
/// Each path is read once: the build info lists the root files too, and the
/// check keeps the first mtime of a path. With a forecast, the root mtimes
/// come from `roots`, which the mtimes job of the read shares. Returns the
/// newest mtime read.
fn prefetch_m_times(
    fs: &dyn Fs,
    read: &BuildInfoRead,
    status: &StatusPrefetch,
    m_times: &MTimePrefetch,
    roots: Option<&RootTimes>,
) -> NewestTime {
    let root_times = roots.and_then(|roots| roots.get(fs, &read.input_files, None));
    let roots = read
        .input_files
        .iter()
        .zip(&status.input_paths)
        .enumerate()
        .map(|(index, (file, path))| {
            let known = root_times
                .as_ref()
                .and_then(|times| times.get(index).copied());
            (file, path, known)
        });
    let files = status
        .file_names
        .iter()
        .map(|(file, path)| (file, path, None));
    let mut seen: FxHashSet<&Path> =
        FxHashSet::with_capacity_and_hasher(read.input_files.len(), Default::default());
    let read: Vec<(Path, Option<SystemTime>)> = roots
        .chain(files)
        .filter(|(file, path, _)| is_typescript_source(file) && seen.insert(path))
        .map(|(file, path, known)| {
            let m_time = known.unwrap_or_else(|| fs.stat(file).and_then(|stat| stat.mod_time()));
            (path.clone(), m_time)
        })
        .collect();
    let mut newest = NewestTime::default();
    let mut m_times = m_times.lock().unwrap_or_else(PoisonError::into_inner);
    for (path, m_time) in read {
        newest.add(m_time);
        m_times.entry(path).or_insert(m_time);
    }
    newest
}

/// PORT: not in Go (perf). The mtimes job of a forecast
/// (`PrefetchJob::Mtimes`): true when the build info of `read` exists and
/// no config or root file of its task is missing or newer than it. Else the
/// task builds with changed inputs. Go's check of a light rebuild reads no
/// root mtime (it returns first); the forecast reads them, on a thread.
/// The parse job of a read that is not a light rebuild reads the source
/// mtimes for its check, and `forecast_rebuild` compares them, so this
/// job reads no root mtime there. It stops when the forecast ends (true:
/// the answer no longer depends on it).
fn unchanged_inputs(fs: &dyn Fs, read: &PrefetchRead, forecast: &RebuildForecast) -> bool {
    let m_time = |file: &str| fs.stat(file).and_then(|stat| stat.mod_time());
    let Some(build_info_time) = m_time(&read.read.name) else {
        return false;
    };
    let unchanged =
        |m_time: Option<SystemTime>| m_time.is_some_and(|m_time| m_time <= build_info_time);
    if !read
        .read
        .config_files
        .iter()
        .all(|file| unchanged(m_time(file)))
    {
        return false;
    }
    if forecast.class(read.index) == Some(false) {
        return true;
    }
    let stop = || forecast.ended();
    read.roots
        .get(fs, &read.read.input_files, Some(&stop))
        .is_none_or(|roots| roots.iter().all(|root| unchanged(*root)))
}

/// PORT: not in Go (perf). The rebuild of the task of `read` as its build
/// info tells (`Rebuild`; the mtimes job finds changed root and config
/// files). `early` is where the task's check returns before it reads the
/// input mtimes (`None`: no build info), and `sources` the newest of the
/// TypeScript source mtimes that the thread read for a check that reads
/// them (`prefetch_m_times`, which also covers the files of the build info
/// that are not root files).
/// The forecast misses a change that only Go's later parts of the check
/// see (a package.json file, an upstream output), or, for a light rebuild,
/// a changed file of the build info that is not a root file. Such a task
/// compiles on a builder thread with the same output.
fn forecast_rebuild(
    fs: &dyn Fs,
    read: &BuildInfoRead,
    early: Option<EarlyReturn>,
    sources: Option<NewestTime>,
) -> Rebuild {
    match early {
        None | Some(EarlyReturn::OutOfDate) => Rebuild::Heavy,
        Some(EarlyReturn::ErrorsOrPendingEmit) => Rebuild::Light,
        Some(EarlyReturn::No) => {
            let build_info_time = fs.stat(&read.name).and_then(|stat| stat.mod_time());
            let newer = match (sources, build_info_time) {
                (Some(sources), Some(time)) => sources.newer_than(time),
                _ => true,
            };
            if newer { Rebuild::Heavy } else { Rebuild::None }
        }
    }
}

// Go: build/orchestrator.go:991 NewOrchestrator
pub fn new_orchestrator(opts: Options) -> Orchestrator {
    // PORT: Go passes the method value `opts.Sys.FS().DirectoryExists`.
    let fs = opts.sys.fs();
    let wm = new_watch_manager(
        opts.sys.writer(),
        Box::new(move |path: &str| fs.directory_exists(path)),
    );
    // Go: the `comparePathsOptions` field of the `Orchestrator` literal.
    let compare_paths_options = ComparePathsOptions {
        current_directory: opts.sys.get_current_directory(),
        use_case_sensitive_file_names: opts.sys.fs().use_case_sensitive_file_names(),
    };
    let host = Rc::new(BuildHost::new(
        opts.sys.clone(),
        opts.command.clone(),
        compare_paths_options.clone(),
    ));
    let mut orchestrator = Orchestrator {
        opts,
        compare_paths_options,
        host,
        content_mapper_host: None,
        tasks: FxHashMap::default(),
        order: Vec::new(),
        errors: Vec::new(),
        error_summary_reporter: None,
        watch_status_reporter: None,
        wm: Rc::new(RefCell::new(wm)),
        schedule_order: Vec::new(),
        graph_generated: false,
        build_info_prefetch: RefCell::new(None),
        prefetch_pool: RefCell::new(None),
        released: RefCell::default(),
        ends_process: std::cell::Cell::new(false),
        status_prefetch: RefCell::new(None),
    };
    if orchestrator.opts.command.compiler_options.watch.is_true() {
        orchestrator.watch_status_reporter = Some(create_watch_status_reporter(
            orchestrator.opts.sys.clone(),
            &orchestrator.opts.command.locale(),
            orchestrator.opts.command.compiler_options.clone(),
            orchestrator.opts.testing.clone(),
        ));
        // Go: if t, ok := opts.Testing.(CommandLineTestingWithWatchBackend); ok { wm.SetBackend(t.WatchBackend()) }
        // PORT: the test backend comes from `watcher::set_test_watch_backend`.
        if let Some(backend) = crate::execute::watcher::test_watch_backend() {
            orchestrator.wm.borrow_mut().set_backend(backend);
        }
    } else {
        orchestrator.error_summary_reporter = Some(create_report_error_summary(
            &*orchestrator.opts.sys,
            &orchestrator.opts.command.locale(),
            Some(&orchestrator.opts.command.compiler_options),
        ));
    }
    orchestrator
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The graph of `reads` reads whose tasks wait for no other task.
    fn independent(reads: usize) -> TaskGraph {
        TaskGraph::new((0..reads).collect(), vec![TaskSet::new(reads); reads])
    }

    /// `RebuildForecast::light`: builders start only with no heavy rebuild
    /// and two light ones that can compile at the same time.
    #[test]
    fn rebuild_forecast_needs_two_light_rebuilds_and_no_heavy_one() {
        let forecast = |mtimes: &[bool], parses: &[Rebuild]| {
            let forecast = RebuildForecast::new(independent(3));
            mtimes
                .iter()
                .for_each(|&unchanged| forecast.add_mtimes(unchanged));
            for (read, &rebuild) in parses.iter().enumerate() {
                forecast.add_class(read, rebuild == Rebuild::Light);
                forecast.add_parse(rebuild);
            }
            forecast.light()
        };
        let unchanged = [true; 3];
        // Two light rebuilds decide before the third build info is parsed.
        assert!(forecast(&unchanged, &[Rebuild::Light, Rebuild::Light]));
        assert!(!forecast(
            &unchanged,
            &[Rebuild::Light, Rebuild::None, Rebuild::None]
        ));
        assert!(!forecast(&unchanged, &[Rebuild::Light, Rebuild::Heavy]));
        // A changed input decides before any parse, and ends the forecast.
        let changed = RebuildForecast::new(independent(3));
        changed.add_mtimes(true);
        changed.add_mtimes(false);
        assert!(changed.ended() && !changed.light());
        // Fewer than two light rebuilds decide without the mtimes.
        assert!(!forecast(
            &[],
            &[Rebuild::Light, Rebuild::None, Rebuild::None]
        ));
    }

    /// A chain (each task waits for the one before it) forecasts nothing,
    /// and light rebuilds count only when two of them can compile at the
    /// same time.
    #[test]
    fn rebuild_forecast_counts_light_rebuilds_that_can_compile_together() {
        // Task 1 and 2 wait for task 0; task 2 waits for task 1.
        let mut upstream = vec![TaskSet::new(3); 3];
        upstream[1].insert(0);
        upstream[2].insert(0);
        upstream[2].insert(1);
        let chain = TaskGraph::new(vec![0, 1, 2], upstream.clone());
        assert!(!chain.has_independent_pair());
        // A fan: tasks 1 and 2 wait for task 0 only.
        upstream[2] = TaskSet::new(3);
        upstream[2].insert(0);
        let fan = TaskGraph::new(vec![0, 1, 2], upstream);
        assert!(fan.has_independent_pair());
        let forecast = RebuildForecast::new(fan);
        (0..3).for_each(|_| forecast.add_mtimes(true));
        // Task 0 and task 1 cannot compile together.
        forecast.add_class(0, true);
        forecast.add_class(1, true);
        assert_eq!(forecast.known_light(), None);
        forecast.add_class(2, true);
        assert!(forecast.light());
    }
}
