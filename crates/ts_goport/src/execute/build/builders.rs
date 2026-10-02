//! PORT: not in Go (perf). Parallel program loads for `tsc -b` (design D4,
//! studies/parload1). Go builds the tasks of a build on up to `numRoutines`
//! goroutines, so the programs of tasks that run at the same time load at
//! the same time. Here a program and the frontend data of its load are not
//! `Send`, so a program lives on the thread that loaded it. In a parallel
//! build each task that compiles gets a builder thread for the whole life
//! of its program: the load, the check and emit, the finish (its writes)
//! and the release (`BuildTask::compile_and_emit_start`,
//! `compile_and_emit_finish`, `release_task_program`). The orchestrator
//! keeps the rest on its thread, as in a serial build: the schedule, the
//! up-to-date checks, the order of the finishes, the downstream updates and
//! the reports (orchestrator.rs `build_all_tasks`).
//!
//! What a builder reads:
//! - Each builder has its own system and build host. Its host shares the
//!   file system caches, the mtimes, the configs and the parse cache of the
//!   `.d.ts` and `.json` files with the build host (host.rs
//!   `BuilderShared`), so the tasks read the file system as through Go's
//!   one host, and each such file is parsed and bound once in the build
//!   (host.rs `SharedSourceFiles`, Go's `host.sourceFiles`). The loads of
//!   the builders run at the same time.
//! - A task gets a copy of what its compile reads from the orchestrator's
//!   task (`CompileJob`), and the orchestrator's task takes what the
//!   compile set (`CompileResult`).
//! - The read rule: a builder writes a file other than its task's build
//!   info, or changes an mtime, only when no load runs (`LoadGate`), and
//!   the orchestrator thread finishes a task of its own only when no
//!   builder loads (`Builders::wait_for_loads`). The orchestrator finishes
//!   one task at a time and starts no task while it finishes one, so a task
//!   that starts before a finish loads before the finish writes, and a task
//!   that starts after it loads after, as in a serial build (SLOTS in
//!   orchestrator.rs `build_all_tasks`).
//! - Each builder takes its file ids in runs (`ast::reserve_file_ids`), so
//!   loads can publish at the same time. The output does not depend on the
//!   ids.
//!
//! The orchestrator uses builders only where the output is the serial
//! output (`Orchestrator::builders_setting`): for a first task that
//! compiles as a light rebuild, and for the later ones only when two tasks
//! that can compile at the same time are light rebuilds and no task is a
//! heavy one (`Orchestrator::first_task_uses_builder`,
//! `later_tasks_use_builders`). Else the later ones compile on the
//! orchestrator thread with the parse cache of the build, beside the loads
//! of the builders. A task that compiles while every builder is busy
//! compiles on the orchestrator thread too. The builders of the earlier
//! tasks write only when the orchestrator finishes them, so the read rule
//! holds for them.

use crate::execute::build::build_task::*;
use crate::execute::build::command_line::{ParsedBuildCommandLine, SendBuildCommandLine};
use crate::execute::build::host::{BuildHost, BuilderShared};
use crate::execute::build::orchestrator::task_reporter;
use crate::execute::incremental::build_info::BuildInfo;
use crate::execute::incremental::incremental::new_build_info_reader;
use crate::execute::incremental::program::Program as IncrementalProgram;
use crate::execute::tsc::compile::{OsSystem, System};
use crate::execute::tsc::diagnostics::{
    create_builder_status_reporter, create_diagnostic_reporter,
};
use crate::frontend::prelude::*;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The loads that run on the builder threads of a build: the orchestrator
/// counts a load when it sends a task to a builder, and the builder ends it
/// when the task's program is made (`BuildTask::compile_and_emit_start`).
/// A builder waits for the loads before it writes (the read rule above).
#[derive(Default)]
pub struct LoadGate {
    loads: Mutex<usize>,
    ended: Condvar,
}

impl LoadGate {
    fn start(&self) {
        *lock(&self.loads) += 1;
    }

    fn end(&self) {
        let mut loads = lock(&self.loads);
        *loads -= 1;
        if *loads == 0 {
            self.ended.notify_all();
        }
    }

    /// Waits until no load runs.
    pub(crate) fn wait_for_loads(&self) {
        let mut loads = lock(&self.loads);
        while *loads > 0 {
            loads = self
                .ended
                .wait(loads)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Ends a load of a `LoadGate` when it drops, also when the load panics.
struct LoadEnd<'a>(&'a LoadGate);

impl Drop for LoadEnd<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// What a builder thread needs to start.
pub(crate) struct BuilderSetup {
    pub(crate) shared: BuilderShared,
    pub(crate) command: SendBuildCommandLine,
    pub(crate) compare_paths_options: ComparePathsOptions,
    /// The current directory and the default library path of the
    /// orchestrator's system, and when it started (`System::since_start`).
    pub(crate) cwd: String,
    pub(crate) default_library_path: String,
    pub(crate) start: Instant,
    /// The process ends after the build (`Orchestrator::ends_process`):
    /// then a builder frees nothing at the end.
    pub(crate) ends_process: bool,
}

enum Message {
    /// Compile the task at this build order index.
    Compile(usize, CompileJob),
    /// Finish the task that the builder compiles.
    Finish,
}

struct Builder {
    messages: Sender<Message>,
    busy: bool,
}

/// The builder threads of one build, on the orchestrator thread. A task
/// goes to the first idle builder (`compile`), so a chain of tasks stays on
/// the first builder and its parse cache. A builder starts with its first
/// task. There are at most `max` builders (`Orchestrator::start_builders`).
/// When all of them are busy, a task that compiles loads on the
/// orchestrator thread (`all_busy`).
pub(crate) struct Builders {
    setup: Arc<BuilderSetup>,
    max: usize,
    builders: Vec<Builder>,
    gate: Arc<LoadGate>,
    /// Gets the build order index of a task when its check and emit are
    /// done (`build_all_tasks`).
    ready: Sender<usize>,
    finished: Receiver<CompiledTask>,
    finished_sender: Sender<CompiledTask>,
    /// The builder of each task that a builder compiles, by build order
    /// index.
    builder_of: FxHashMap<usize, usize>,
}

impl Builders {
    /// At most `max` builders, which send each task's index to `ready` when
    /// its check and emit are done.
    pub(crate) fn new(setup: BuilderSetup, max: usize, ready: Sender<usize>) -> Self {
        let (finished_sender, finished) = channel();
        Builders {
            setup: Arc::new(setup),
            max,
            builders: Vec::new(),
            gate: Arc::default(),
            ready,
            finished,
            finished_sender,
            builder_of: FxHashMap::default(),
        }
    }

    /// Sends the task at build order index `index` to an idle builder. The
    /// builder sends `index` to `ready` once when the task's check and emit
    /// are done, or when its compile start ended without a program.
    pub(crate) fn compile(&mut self, index: usize, job: CompileJob) {
        let builder = match self.builders.iter().position(|builder| !builder.busy) {
            Some(builder) => builder,
            None => {
                assert!(self.builders.len() < self.max, "no idle builder");
                self.builders.push(self.start_builder());
                self.builders.len() - 1
            }
        };
        self.gate.start();
        if self.builders[builder]
            .messages
            .send(Message::Compile(index, job))
            .is_err()
        {
            panic!("builder {builder} ended");
        }
        self.builders[builder].busy = true;
        self.builder_of.insert(index, builder);
    }

    /// True when every builder is busy and no other can start: then a task
    /// that compiles loads on the orchestrator thread (orchestrator.rs
    /// `build_all_tasks`).
    pub(crate) fn all_busy(&self) -> bool {
        self.builders.len() >= self.max && self.builders.iter().all(|builder| builder.busy)
    }

    /// True when the task at build order index `index` compiles on a
    /// builder and is not finished.
    pub(crate) fn compiles(&self, index: usize) -> bool {
        self.builder_of.contains_key(&index)
    }

    /// Waits until no program loads on a builder: the orchestrator thread
    /// finishes a task of its own only then (the read rule above).
    pub(crate) fn wait_for_loads(&self) {
        self.gate.wait_for_loads();
    }

    /// Finishes the task at `index` on its builder, after its index came
    /// to `ready`, and returns how its compile ended.
    pub(crate) fn finish(&mut self, index: usize) -> CompiledTask {
        let builder = self
            .builder_of
            .remove(&index)
            .expect("the task is on a builder");
        if self.builders[builder]
            .messages
            .send(Message::Finish)
            .is_err()
        {
            panic!("builder {builder} ended");
        }
        let compiled = self
            .finished
            .recv()
            .unwrap_or_else(|_| panic!("builder {builder} ended"));
        self.builders[builder].busy = false;
        compiled
    }

    fn start_builder(&self) -> Builder {
        let (messages, receive) = channel();
        let setup = self.setup.clone();
        let gate = self.gate.clone();
        let ready = self.ready.clone();
        let finished = self.finished_sender.clone();
        // A builder loads programs, as the orchestrator thread does, so it
        // gets the Go stack size.
        std::thread::Builder::new()
            .name("goport-builder".to_string())
            .stack_size(crate::gostd::stack::max_stack_size())
            .spawn(move || run_builder(&setup, gate, &receive, &ready, &finished))
            .expect("failed to start a builder thread");
        Builder {
            messages,
            busy: false,
        }
    }
}

/// A builder thread: compiles the tasks that come in `messages` until the
/// orchestrator drops its `Builders`.
fn run_builder(
    setup: &BuilderSetup,
    gate: Arc<LoadGate>,
    messages: &Receiver<Message>,
    ready: &Sender<usize>,
    finished: &Sender<CompiledTask>,
) {
    crate::ast::reserve_file_ids();
    let sys: Rc<dyn System> = Rc::new(OsSystem::for_thread(
        setup.cwd.clone(),
        setup.default_library_path.clone(),
        setup.start,
    ));
    let command = Rc::new(setup.command.to_local());
    let host = Rc::new(BuildHost::new_builder(
        sys.clone(),
        command.clone(),
        setup.compare_paths_options.clone(),
        setup.shared.clone(),
    ));
    let builder = BuilderContext {
        command,
        compare_paths_options: setup.compare_paths_options.clone(),
        sys,
        host,
        gate,
    };
    // The released programs of the finished tasks. They free when the
    // builder has nothing else to do, as the orchestrator frees its own.
    let mut released = VecDeque::new();
    loop {
        let message = match messages.try_recv() {
            Ok(message) => message,
            Err(TryRecvError::Empty) => {
                if released.pop_front().is_some() {
                    continue;
                }
                match messages.recv() {
                    Ok(message) => message,
                    Err(_) => break,
                }
            }
            Err(TryRecvError::Disconnected) => break,
        };
        let Message::Compile(index, job) = message else {
            panic!("a builder got a finish with no task");
        };
        let compiled = builder.compile(job, || {
            let _ = ready.send(index);
            matches!(messages.recv(), Ok(Message::Finish))
        });
        let Some((compiled, program)) = compiled else {
            break;
        };
        if finished.send(compiled).is_err() {
            break;
        }
        released.extend(program.map(release_task_program));
    }
    if setup.ends_process {
        // As in the orchestrator (`start_exported`): the process ends, so
        // nothing is freed. The thread waits for the end.
        std::mem::forget(released);
        std::mem::forget(builder);
        loop {
            std::thread::park();
        }
    }
}

/// The orchestrator of the tasks on a builder thread: its own system and
/// host (see the top).
struct BuilderContext {
    command: Rc<ParsedBuildCommandLine>,
    compare_paths_options: ComparePathsOptions,
    sys: Rc<dyn System>,
    host: Rc<BuildHost>,
    gate: Arc<LoadGate>,
}

impl BuilderContext {
    /// Compiles the task of `job`: makes its program and starts its check
    /// and emit (`compile_and_emit_start`), ends the load, and waits for
    /// the check and emit. Then `finish_now` tells the orchestrator and
    /// waits for its turn: false when the build ended (then this returns
    /// None). Then the task finishes (`compile_and_emit_finish`). Returns
    /// how the compile ended and the task's program.
    fn compile(
        &self,
        job: CompileJob,
        finish_now: impl FnOnce() -> bool,
    ) -> Option<(CompiledTask, Option<IncrementalProgram>)> {
        let path = job.path.clone();
        let load = LoadEnd(&self.gate);
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let resolved = self.host.get_resolved_project_reference(&job.config, &path);
            let mut task = BuildTask::from_compile_job(job, resolved, self.task_result());
            let compiles = task.compile_and_emit_start(self, &path);
            (task, compiles)
        }));
        drop(load);
        let (mut task, compiles) = match started {
            Ok(started) => started,
            Err(payload) => {
                return finish_now().then_some((CompiledTask::StartPanicked(payload), None));
            }
        };
        if compiles {
            // Each checker thread drops its sender when the check and
            // emit that the task started are done.
            let (signal, signals) = channel::<()>();
            task.notify_when_compiled(|| signal.clone());
            drop(signal);
            let _ = signals.recv();
        }
        if !finish_now() {
            return None;
        }
        if compiles {
            let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.compile_and_emit_finish(self);
            }));
            if let Err(payload) = finished {
                return Some((CompiledTask::FinishPanicked(payload), None));
            }
        }
        let program = task
            .result
            .as_mut()
            .and_then(|result| result.program.take());
        Some((
            CompiledTask::Done(Box::new(task.into_compile_result())),
            program,
        ))
    }

    /// The result of a task of this builder, with its reporters (Go
    /// `buildOrCleanProject`, orchestrator.go:575).
    fn task_result(&self) -> TaskResult {
        TaskResult::new(
            task_reporter(|w| {
                create_builder_status_reporter(
                    self.sys.clone(),
                    w,
                    &self.command.locale(),
                    &self.command.compiler_options,
                    None,
                )
            }),
            task_reporter(|w| {
                create_diagnostic_reporter(
                    &*self.sys,
                    w,
                    &self.command.locale(),
                    &self.command.compiler_options,
                )
            }),
        )
    }
}

impl BuildTaskOrchestrator for BuilderContext {
    fn command(&self) -> &ParsedBuildCommandLine {
        &self.command
    }

    fn compare_paths_options(&self) -> &ComparePathsOptions {
        &self.compare_paths_options
    }

    fn relative_file_name(&self, file_name: &str) -> String {
        convert_to_relative_path(file_name, &self.compare_paths_options)
    }

    fn to_path(&self, file_name: &str) -> Path {
        self.host.to_path(file_name)
    }

    fn now(&self) -> SystemTime {
        self.sys.now()
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

    // The read rule (see the top): an mtime changes when no load runs.
    fn set_m_time(&self, file: &str, m_time: SystemTime) -> Result<(), FsError> {
        self.gate.wait_for_loads();
        self.host.set_m_time(file, Some(m_time))
    }

    fn store_m_time(&self, file: &str, m_time: SystemTime) {
        self.host.store_m_time(file, Some(m_time));
    }

    fn read_build_info_file(&self, config: &ParsedCommandLine) -> Option<Arc<BuildInfo>> {
        new_build_info_reader(self.host.clone() as Rc<dyn CompilerHost>)
            .read_build_info(config)
            .map(Arc::new)
    }

    fn sys(&self) -> Rc<dyn System> {
        self.sys.clone()
    }

    fn host(&self) -> Rc<BuildHost> {
        self.host.clone()
    }

    fn content_mapper_host(&self) -> Option<Rc<dyn crate::contentmapper::Host>> {
        None
    }

    fn load_gate(&self) -> Option<Arc<LoadGate>> {
        Some(self.gate.clone())
    }
}
