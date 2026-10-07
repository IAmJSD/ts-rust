//! Port of Go `ls/crossproject.go`.
//!
//! PORT notes for the whole file:
//! - Go runs the per-project searches on a parallel `core.WorkGroup` and
//!   shares `results`, `defaultDefinition`, `err` and `panicsOccurred` under
//!   mutexes. The shared locals are `RefCell` fields of one
//!   `CrossProjectState` value on the dispatch thread. The Go closures
//!   `canSearchProject`, `enqueueItem` and the queued function are its
//!   methods.
//! - The queued function of an item runs in 3 phases:
//!   1. open (dispatch thread): the item's language service, Go
//!      `GetLanguageServiceForProjectWithFile`;
//!   2. search: `provideSymbolsAndEntries`, the original definition locations
//!      and `symbolAndEntriesToResp`, with no session call. Every item runs
//!      it on the dispatch thread, with a checker of its project's pool, as
//!      Go does (the item's language service takes the pool's query checker,
//!      Go `project/checkerpool.go:252 getQueryChecker`). Later requests in
//!      that project get the same checker and see the state the search left
//!      (for example `EnumOptions<T>` against
//!      `EnumOptions<T extends object = any>` in a hover, Go
//!      `checker.go:21090 instantiateSymbol`). Go runs the items at the same
//!      time; here they run one at a time, in Go start order. Each project
//!      has its own pool, so the order changes no checker's state;
//!   3. commit (dispatch thread): for each location in search order, Go
//!      `GetProjectsForFile` and `enqueueItem`; then the response, or the
//!      first error.
//!   Items get their number when they leave the queue, and commit strictly
//!   in that order. So the enqueue calls, `results` (keys, order, winning
//!   positions) and the first error are those of a serial run in Go start
//!   order. An item opens only after every earlier item committed.
//! - A `search` function runs phase 2 of the items other than item 0 on the
//!   search thread of their program (`search_thread.rs`), in parallel. No
//!   request passes one now: a search thread has its own checker, so the
//!   project's pool stays cold and later requests show other type forms
//!   than Go (lschk1). The code stays until a later change removes it.
//! - Go `collections.SyncMap.Range` order is random. `results` is an
//!   `IndexMap` in insertion order. Multi-project result order can differ
//!   from Go; the oracle compares it without order.
//! - Go `iter.Seq[Resp]` passed to `combineResults` is the slice of the
//!   values the iterator yields, in the same order. Go restarts the iterator
//!   when a combiner ranges over it again; here the combiner reads the slice
//!   again.

use crate::ls::prelude::*;

use crate::frontend::compiler;
use crate::frontend::tspath;
use crate::gostd::{Context, GoError};
use crate::ls::search_thread;
use crate::lsp::lsproto;
use crate::lsp::lsproto::{HasLocation, HasLocations, HasTextDocumentPosition, HasTextDocumentURI};
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc;

// Go: ls/crossproject.go:17 Project
// PORT: Go `GetProgram()` is nil for a project without a program (a
// solution tsconfig with `files: []` and references); the port gives `None`.
pub trait Project {
    fn id(&self) -> String;
    fn get_program(&self) -> Option<Rc<compiler::NewProgram>>;
    fn has_file(&self, file_name: &str) -> bool;
}

// Go: ls/crossproject.go:23 projectAndTextDocumentPosition
// PORT: Go `Project` values are `Rc<dyn Project>`; Go interface equality is
// `Rc::ptr_eq`. Go `ls *LanguageService` is set only for the default
// project (the caller's language service), so it is a borrow.
pub struct ProjectAndTextDocumentPosition<'l> {
    pub project: Rc<dyn Project>,
    pub ls: Option<&'l LanguageService>,
    pub uri: lsproto::DocumentUri,
    pub position: lsproto::Position,
    /// Go `symbolData *SymbolAndEntriesData`; nil is `None`. Only the
    /// default project's item (item 0, which runs on the dispatch thread)
    /// can carry it.
    pub symbol_data: Option<SymbolAndEntriesData>,
    pub for_original_location: bool,
}

// Go: ls/crossproject.go:32 response
#[derive(Clone, Debug, Default)]
pub struct Response<Resp> {
    pub complete: bool,
    pub result: Resp,
    pub for_original_location: bool,
}

// Go: ls/crossproject.go:38 CrossProjectOrchestrator
// PORT: Go `*LanguageService` results are new language services owned by
// the caller (`Option` for nil). Go `iter.Seq[Project]` is a push iterator:
// `yield_` gets each project and returns false to stop.
pub trait CrossProjectOrchestrator {
    fn get_default_project(&self) -> Rc<dyn Project>;
    fn get_all_projects_for_initial_request(&self) -> Vec<Rc<dyn Project>>;
    fn get_language_service_for_project_with_file(
        &self,
        ctx: &Context,
        project: &Rc<dyn Project>,
        uri: &lsproto::DocumentUri,
    ) -> Option<LanguageService>;
    fn get_projects_for_file(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
    ) -> Result<Vec<Rc<dyn Project>>, GoError>;
    fn get_projects_loading_project_tree(
        &self,
        ctx: &Context,
        requested_project_trees: &FxHashSet<tspath::Path>,
        yield_: &mut dyn FnMut(Rc<dyn Project>) -> bool,
    );
}

/// Go `symbolAndEntriesToResp func(*LanguageService, context.Context, Req, SymbolAndEntriesData, symbolEntryTransformOptions) (Resp, error)`.
pub type SymbolAndEntriesToResp<Req, Resp> = fn(
    &LanguageService,
    &Context,
    &Req,
    SymbolAndEntriesData,
    SymbolEntryTransformOptions,
) -> Result<Resp, GoError>;

/// A request whose searches in other projects can run on search threads
/// (references, implementations, rename). `to_resp` is the request's Go
/// `symbolAndEntriesToResp` for any program view.
/// `search_thread::start_search::<K>` is the `search` argument of
/// `handle_cross_project` for it. No request uses it now (see the file
/// header).
pub trait CrossProjectSearch: 'static {
    type Req: HasTextDocumentPosition + Clone + Send + 'static;
    type Resp: Clone + Default + Send + 'static;

    fn to_resp<P: ProgramView>(
        ls: &LanguageService<P>,
        ctx: &Context,
        params: &Self::Req,
        data: SymbolAndEntriesData,
        options: SymbolEntryTransformOptions,
    ) -> Result<Self::Resp, GoError>;
}

// Go: ls/crossproject.go:46 (*LanguageService).handleCrossProject
// PORT: Go 1.27 makes this a generic method of the default language
// service (ts#63902); here it stays a function whose first parameter is
// `default_ls`.
// PORT: Go `params Req` is a pointer type: `&Req`. Go `orchestrator` can be
// nil: `Option<&dyn CrossProjectOrchestrator>`. Go
// `combineResults func(iter.Seq[Resp]) Resp` takes the yielded values as a
// slice (see the file header). `search` starts phase 2 of an item on a
// search thread (`search_thread::start_search::<K>`); with `None`, every item
// runs on the dispatch thread with its project's pool checker. Every caller
// passes `None`, which is Go's model (see the file header).
#[allow(clippy::too_many_arguments)]
pub fn handle_cross_project<Req, Resp>(
    default_ls: &LanguageService,
    ctx: &Context,
    params: &Req,
    orchestrator: Option<&dyn CrossProjectOrchestrator>,
    symbol_and_entries_to_resp: SymbolAndEntriesToResp<Req, Resp>,
    search: Option<search_thread::StartSearch<Req, Resp>>,
    combine_results: fn(&[Resp]) -> Resp,
    is_rename: bool,
    implementations: bool,
    options: SymbolEntryTransformOptions,
    default_project_data: Option<SymbolAndEntriesData>,
) -> Result<Resp, GoError>
where
    Req: HasTextDocumentPosition,
    Resp: Clone + Default,
{
    let mut resp = Resp::default();

    // Single project
    let Some(orchestrator) = orchestrator else {
        let data = match default_project_data {
            Some(default_project_data) => default_project_data,
            None => {
                default_ls
                    .provide_symbols_and_entries(
                        ctx,
                        &params.text_document_uri(),
                        params.text_document_position(),
                        is_rename,
                        implementations,
                    )
                    .0
            }
        };
        return symbol_and_entries_to_resp(default_ls, ctx, params, data, options);
    };

    let default_project = orchestrator.get_default_project();
    let all_projects = orchestrator.get_all_projects_for_initial_request();
    let state = CrossProjectState {
        ctx,
        params,
        orchestrator,
        symbol_and_entries_to_resp,
        search,
        is_rename,
        implementations,
        options,
        default_project,
        all_projects,
        results: RefCell::new(IndexMap::new()),
        default_definition: RefCell::new(None),
        wg: RefCell::new(VecDeque::new()),
        err: RefCell::new(None),
        panics_occurred: RefCell::new(None),
    };

    // Initial set of projects and locations in the queue, starting with default project
    let mut initial_item = ProjectAndTextDocumentPosition {
        project: Rc::clone(&state.default_project),
        ls: Some(default_ls),
        uri: params.text_document_uri(),
        position: params.text_document_position(),
        symbol_data: None,
        for_original_location: false,
    };
    initial_item.symbol_data = default_project_data;
    state.enqueue_item(initial_item);
    for project in &state.all_projects {
        if !Rc::ptr_eq(project, &state.default_project) {
            state.enqueue_item(ProjectAndTextDocumentPosition {
                project: Rc::clone(project),
                ls: None,
                // TODO!! symlinks need to change the URI
                uri: params.text_document_uri(),
                position: params.text_document_position(),
                symbol_data: None,
                for_original_location: false,
            });
        }
    }

    // Outer loop - to complete work if more is added after completing existing queue
    loop {
        // Process existing known projects first
        state.run_and_wait();
        // No need to use mu here since we are not in parallel at this point
        if let Some(panics_occurred) = state.panics_occurred.borrow().as_ref() {
            // PORT: Go `%v` of a `[]string`.
            crate::core::go_panic(format!(
                "Panics occurred during cross-project handling: [{}]",
                panics_occurred.join(" ")
            ));
        }
        if let Some(err) = ctx.err() {
            return Err(err);
        }
        if let Some(err) = state.err.borrow().clone() {
            return Err(err);
        }

        // Go: wg = core.NewWorkGroup(false)
        // PORT: the queue is empty after `run_and_wait`.
        let mut has_more_work = false;
        if state.default_definition.borrow().is_some() {
            let mut requested_project_trees: FxHashSet<tspath::Path> = FxHashSet::default();
            for (key, response) in state.results.borrow().iter() {
                if response.borrow().complete {
                    requested_project_trees.insert(tspath::Path(key.clone()));
                }
            }

            // Load more projects based on default definition found
            // PORT: Go returns from inside the range loop; the push iterator
            // stops (`false`) and the error is returned after it.
            let mut ctx_err: Option<GoError> = None;
            orchestrator.get_projects_loading_project_tree(
                ctx,
                &requested_project_trees,
                &mut |loaded_project: Rc<dyn Project>| {
                    if let Some(err) = ctx.err() {
                        ctx_err = Some(err);
                        return false;
                    }

                    // Can loop forever without this (enqueue here, dequeue above, repeat)
                    if !state.can_search_project(&loaded_project)
                        || loaded_project.get_program().is_none()
                    {
                        return true;
                    }

                    // Enqueue the project and location for further processing
                    let default_definition = state.default_definition.borrow();
                    let default_definition = default_definition
                        .as_ref()
                        .unwrap_or_else(|| crate::core::go_nil_dereference());
                    if loaded_project.has_file(&default_definition.text_document_uri().file_name())
                    {
                        state.enqueue_item(ProjectAndTextDocumentPosition {
                            project: loaded_project,
                            ls: None,
                            uri: default_definition.text_document_uri(),
                            position: default_definition.text_document_position(),
                            symbol_data: None,
                            for_original_location: false,
                        });
                        has_more_work = true;
                    } else if let Some(source_pos) = (default_definition.get_source_position)()
                        && loaded_project.has_file(&source_pos.text_document_uri().file_name())
                    {
                        state.enqueue_item(ProjectAndTextDocumentPosition {
                            project: loaded_project,
                            ls: None,
                            uri: source_pos.text_document_uri(),
                            position: source_pos.text_document_position(),
                            symbol_data: None,
                            for_original_location: false,
                        });
                        has_more_work = true;
                    } else if let Some(generated_pos) =
                        (default_definition.get_generated_position)()
                        && loaded_project.has_file(&generated_pos.text_document_uri().file_name())
                    {
                        state.enqueue_item(ProjectAndTextDocumentPosition {
                            project: loaded_project,
                            ls: None,
                            uri: generated_pos.text_document_uri(),
                            position: generated_pos.text_document_position(),
                            symbol_data: None,
                            for_original_location: false,
                        });
                        has_more_work = true;
                    }
                    true
                },
            );
            if let Some(err) = ctx_err {
                return Err(err);
            }
        }
        if !has_more_work {
            break;
        }
    }

    let results_size = state.results.borrow().len();
    if results_size > 1 {
        resp = combine_results(&state.get_results_iterator());
    } else {
        // Single result, return that directly
        if let Some(value) = state.get_results_iterator().into_iter().next() {
            resp = value;
        }
    }
    Ok(resp)
}

// Go: ls/crossproject.go:96 steps 3 to 5 of the queued function
// Phase 2 of an item (see the file header): the search, the original
// definition locations of each entry (collected into `locations` for the
// commit, where Go makes the session calls in its callback) and the
// response. `on_entry` runs first for each entry (the default definition of
// item 0). Returns None where Go returns early because the request was
// canceled.
#[allow(clippy::too_many_arguments)]
pub fn search_item<P: ProgramView, Req, Resp>(
    ls: &LanguageService<P>,
    ctx: &Context,
    params: &Req,
    uri: &lsproto::DocumentUri,
    position: lsproto::Position,
    is_rename: bool,
    implementations: bool,
    options: SymbolEntryTransformOptions,
    to_resp: impl FnOnce(
        &LanguageService<P>,
        &Context,
        &Req,
        SymbolAndEntriesData,
        SymbolEntryTransformOptions,
    ) -> Result<Resp, GoError>,
    on_entry: &mut dyn FnMut(&Rc<RefCell<SymbolAndEntries>>),
    locations: &mut Vec<(lsproto::DocumentUri, lsproto::Position)>,
) -> Option<Result<Resp, GoError>> {
    search_item_with_data(
        ls,
        ctx,
        params,
        uri,
        position,
        is_rename,
        implementations,
        options,
        None, /*symbolData*/
        to_resp,
        on_entry,
        locations,
    )
}

// Go: ls/crossproject.go:96 steps 3 to 5 of the queued function
// PORT: `search_item` for an item that can carry Go `item.symbolData`
// (`symbol_data`; only item 0, on the dispatch thread).
#[allow(clippy::too_many_arguments)]
pub fn search_item_with_data<P: ProgramView, Req, Resp>(
    ls: &LanguageService<P>,
    ctx: &Context,
    params: &Req,
    uri: &lsproto::DocumentUri,
    position: lsproto::Position,
    is_rename: bool,
    implementations: bool,
    options: SymbolEntryTransformOptions,
    symbol_data: Option<SymbolAndEntriesData>,
    to_resp: impl FnOnce(
        &LanguageService<P>,
        &Context,
        &Req,
        SymbolAndEntriesData,
        SymbolEntryTransformOptions,
    ) -> Result<Resp, GoError>,
    on_entry: &mut dyn FnMut(&Rc<RefCell<SymbolAndEntries>>),
    locations: &mut Vec<(lsproto::DocumentUri, lsproto::Position)>,
) -> Option<Result<Resp, GoError>> {
    let (data, ok) = match symbol_data {
        Some(symbol_data) => (symbol_data, true),
        None => ls.provide_symbols_and_entries(ctx, uri, position, is_rename, implementations),
    };
    if ctx.err().is_some() {
        return None;
    }
    if ok {
        for entry in &data.symbols_and_entries {
            on_entry(entry);
            ls.for_each_original_definition_location(ctx, entry, &mut |uri, position| {
                locations.push((uri, position));
            });
        }
    }
    Some(to_resp(ls, ctx, params, data, options))
}

/// Go `fmt.Sprintf("panic handling request: %v\n%s", r, debug.Stack())`, the
/// text that the deferred recover of the queued function keeps (Go
/// `panicOccurred`). The name keeps the old spelling because
/// `search_thread.rs` calls it.
pub fn panic_occured_text(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = panic_payload_text(&*payload);
    // PORT: Go `debug.Stack()`; the text is only logged.
    let stack = std::backtrace::Backtrace::force_capture();
    format!("panic handling request: {text}\n{stack}")
}

/// Go `%v` of a recovered panic value.
pub fn panic_payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(p) = payload.downcast_ref::<crate::core::GoPanic>() {
        p.message.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{payload:?}")
    }
}

/// The locals of Go `handleCrossProject` that its closures share.
struct CrossProjectState<'a, Req, Resp> {
    ctx: &'a Context,
    params: &'a Req,
    orchestrator: &'a dyn CrossProjectOrchestrator,
    symbol_and_entries_to_resp: SymbolAndEntriesToResp<Req, Resp>,
    search: Option<search_thread::StartSearch<Req, Resp>>,
    is_rename: bool,
    implementations: bool,
    options: SymbolEntryTransformOptions,
    default_project: Rc<dyn Project>,
    all_projects: Vec<Rc<dyn Project>>,
    /// Go `results collections.SyncMap[string, *response[Resp]]`.
    results: RefCell<IndexMap<String, Rc<RefCell<Response<Resp>>>>>,
    /// Go `defaultDefinition *nonLocalDefinition`.
    default_definition: RefCell<Option<NonLocalDefinition<'a>>>,
    /// Go `wg`: the queued items with the response each one fills.
    wg: RefCell<
        VecDeque<(
            ProjectAndTextDocumentPosition<'a>,
            Rc<RefCell<Response<Resp>>>,
        )>,
    >,
    /// Go `err` (under `errMu`).
    err: RefCell<Option<GoError>>,
    /// Go `panicsOccurred` (under `panicMu`); `None` is Go's nil slice.
    panics_occurred: RefCell<Option<Vec<String>>>,
}

/// The language service of an item: the caller's for item 0, or the one
/// that phase 1 made.
enum ItemLs<'a> {
    Caller(&'a LanguageService),
    Owned(LanguageService),
}

impl ItemLs<'_> {
    fn get(&self) -> &LanguageService {
        match self {
            ItemLs::Caller(ls) => ls,
            ItemLs::Owned(ls) => ls,
        }
    }
}

/// An item that left the queue and has not committed yet.
struct Slot<'a, Resp> {
    /// Queue order: items commit in this order.
    number: usize,
    item: ProjectAndTextDocumentPosition<'a>,
    response: Rc<RefCell<Response<Resp>>>,
    state: SlotState<'a, Resp>,
}

enum SlotState<'a, Resp> {
    /// Phases 1 and 2 run on the dispatch thread when the item is first in
    /// the window.
    Inline,
    /// Phase 2 runs on a search thread. The language service answers the
    /// thread's host reads and keeps the program alive until the commit.
    Running(LanguageService),
    /// Phases 1 and 2 ended; the item commits when it is first.
    Done {
        ls: Option<ItemLs<'a>>,
        outcome: search_thread::ItemOutcome<Resp>,
    },
}

#[derive(Clone, Copy)]
enum Phase {
    Inline,
    Running,
    Done,
}

impl<Resp> SlotState<'_, Resp> {
    fn phase(&self) -> Phase {
        match self {
            SlotState::Inline => Phase::Inline,
            SlotState::Running(_) => Phase::Running,
            SlotState::Done { .. } => Phase::Done,
        }
    }
}

impl<'a, Req, Resp> CrossProjectState<'a, Req, Resp>
where
    Req: HasTextDocumentPosition,
    Resp: Clone + Default,
{
    // Go: ls/crossproject.go:75 canSearchProject (closure)
    fn can_search_project(&self, project: &Rc<dyn Project>) -> bool {
        let searched = self.results.borrow().contains_key(&project.id());
        !searched
    }

    // Go: ls/crossproject.go:84 enqueueItem (closure)
    fn enqueue_item(&self, item: ProjectAndTextDocumentPosition<'a>) {
        let response = Rc::new(RefCell::new(Response::<Resp>::default()));
        {
            // Go: results.LoadOrStore(item.project.Id(), &response)
            let mut results = self.results.borrow_mut();
            let id = item.project.id();
            if results.contains_key(&id) {
                return;
            }
            results.insert(id, Rc::clone(&response));
        }
        // Go: wg.Queue(func() { ... })
        self.wg.borrow_mut().push_back((item, response));
    }

    // Go: wg.RunAndWait()
    // PORT: runs the queued items, including items queued while it runs, in
    // the 3 phases of the file header, and returns when every item committed.
    // At most `search_thread::max_in_flight()` searches run on search
    // threads at once (Go: GOMAXPROCS).
    fn run_and_wait(&self) {
        let limit = search_thread::max_in_flight();
        let (sender, receiver) = mpsc::channel::<search_thread::ToDispatch<Resp>>();
        let mut window: VecDeque<Slot<'a, Resp>> = VecDeque::new();
        let mut next_number = 0usize;
        let mut in_flight = 0usize;
        loop {
            // Take items from the queue in order. An item for a search thread
            // opens at once while fewer than `limit` searches run. An item
            // for the dispatch thread waits until it is first in the window.
            loop {
                let inline = match self.wg.borrow().front() {
                    None => break,
                    Some((item, _)) => self.search.is_none() || item.ls.is_some(),
                };
                if (inline && !window.is_empty()) || (!inline && in_flight >= limit) {
                    break;
                }
                let (item, response) = self
                    .wg
                    .borrow_mut()
                    .pop_front()
                    .expect("the queue has a first item");
                let number = next_number;
                next_number += 1;
                let state = if inline {
                    SlotState::Inline
                } else {
                    self.start_thread_item(number, &item, &sender)
                };
                if matches!(state, SlotState::Running(_)) {
                    in_flight += 1;
                }
                window.push_back(Slot {
                    number,
                    item,
                    response,
                    state,
                });
            }

            match window.front().map(|slot| slot.state.phase()) {
                None => return,
                Some(Phase::Inline) => {
                    let front = window.front_mut().expect("the window has a first item");
                    front.state = self.run_inline_item(&front.item);
                    continue;
                }
                Some(Phase::Done) => {
                    let slot = window.pop_front().expect("the window has a first item");
                    self.commit_item(slot);
                    continue;
                }
                Some(Phase::Running) => {}
            }

            // The first item runs on a search thread: wait for a message.
            let message = receiver
                .recv()
                .expect("the dispatch thread holds a sender of the channel");
            match message {
                search_thread::ToDispatch::Query { item, query, reply } => {
                    let answer = match window.iter().find(|slot| slot.number == item) {
                        Some(Slot {
                            state: SlotState::Running(ls),
                            ..
                        }) => search_thread::answer_query(ls, query),
                        _ => search_thread::HostAnswer::Gone,
                    };
                    let _ = reply.send(answer);
                }
                search_thread::ToDispatch::Done { item, outcome } => {
                    if let Some(slot) = window.iter_mut().find(|slot| slot.number == item) {
                        let state = std::mem::replace(&mut slot.state, SlotState::Inline);
                        slot.state = match state {
                            SlotState::Running(ls) => {
                                in_flight -= 1;
                                SlotState::Done {
                                    ls: Some(ItemLs::Owned(ls)),
                                    outcome,
                                }
                            }
                            state => state,
                        };
                    }
                }
            }
        }
    }

    // Go: ls/crossproject.go:83 the queued function, phase 1 for an item
    // whose phase 2 runs on a search thread.
    fn start_thread_item(
        &self,
        number: usize,
        item: &ProjectAndTextDocumentPosition<'a>,
        sender: &mpsc::Sender<search_thread::ToDispatch<Resp>>,
    ) -> SlotState<'a, Resp> {
        if self.ctx.err().is_some() {
            return SlotState::Done {
                ls: None,
                outcome: search_thread::ItemOutcome::skipped(),
            };
        }
        let start_search = self
            .search
            .expect("an item for a search thread has a search function");
        // Go: defer func() { if r := recover(); r != nil { ... } }()
        let opened = std::panic::catch_unwind(AssertUnwindSafe(|| -> Option<LanguageService> {
            // Get it now
            let ls = self
                .orchestrator
                .get_language_service_for_project_with_file(self.ctx, &item.project, &item.uri)?;
            let search = search_thread::SearchItem {
                ctx: self.ctx.clone(),
                uri: item.uri.clone(),
                position: item.position,
                is_rename: self.is_rename,
                implementations: self.implementations,
                options: self.options,
            };
            start_search(self.params, &ls, number, search, sender.clone());
            Some(ls)
        }));
        match opened {
            Ok(Some(ls)) => SlotState::Running(ls),
            Ok(None) => SlotState::Done {
                ls: None,
                outcome: search_thread::ItemOutcome::skipped(),
            },
            Err(payload) => SlotState::Done {
                ls: None,
                outcome: search_thread::ItemOutcome::panicked(panic_occured_text(payload)),
            },
        }
    }

    // Go: ls/crossproject.go:83 the queued function, phases 1 and 2 on the
    // dispatch thread.
    fn run_inline_item(&self, item: &ProjectAndTextDocumentPosition<'a>) -> SlotState<'a, Resp> {
        let ctx = self.ctx;
        if ctx.err().is_some() {
            return SlotState::Done {
                ls: None,
                outcome: search_thread::ItemOutcome::skipped(),
            };
        }
        let is_default_project = Rc::ptr_eq(&item.project, &self.default_project);
        let mut item_ls: Option<ItemLs<'a>> = None;
        let mut locations: Vec<(lsproto::DocumentUri, lsproto::Position)> = Vec::new();
        // Go: defer func() { if r := recover(); r != nil { ... } }()
        let searched =
            std::panic::catch_unwind(AssertUnwindSafe(|| -> Option<Result<Resp, GoError>> {
                // Process the item
                let ls = match item.ls {
                    Some(ls) => item_ls.insert(ItemLs::Caller(ls)),
                    None => {
                        // Get it now
                        let ls = self
                            .orchestrator
                            .get_language_service_for_project_with_file(
                                ctx,
                                &item.project,
                                &item.uri,
                            )?;
                        item_ls.insert(ItemLs::Owned(ls))
                    }
                }
                .get();
                // PORT: other language services can be alive (the items that
                // run on search threads); make this one's program current.
                let _program = ls.enter_program();
                search_item_with_data(
                    ls,
                    ctx,
                    self.params,
                    &item.uri,
                    item.position,
                    self.is_rename,
                    self.implementations,
                    self.options,
                    item.symbol_data.clone(),
                    |ls, ctx, params, data, options| {
                        (self.symbol_and_entries_to_resp)(ls, ctx, params, data, options)
                    },
                    &mut |entry| {
                        // Find the default definition that can be in another project
                        // Later we will use this load ancestor tree that references this location and expand search
                        if is_default_project && self.default_definition.borrow().is_none() {
                            // PORT: `NonLocalDefinition` borrows the language
                            // service that made it. `results` keeps one item per
                            // project id and the default project's item is
                            // queued first with `defaultLs`, so it is `item.ls`.
                            let default_ls = item.ls.expect(
                                "the default project item carries the default language service",
                            );
                            let default_definition =
                                default_ls.get_non_local_definition(ctx, entry);
                            *self.default_definition.borrow_mut() = default_definition;
                        }
                    },
                    &mut locations,
                )
            }));
        let outcome = match searched {
            Ok(result) => search_thread::ItemOutcome {
                locations,
                result,
                panic: None,
            },
            Err(payload) => search_thread::ItemOutcome {
                locations,
                result: None,
                panic: Some(panic_occured_text(payload)),
            },
        };
        SlotState::Done {
            ls: item_ls,
            outcome,
        }
    }

    // Go: ls/crossproject.go:102 the session calls of the queued function
    // (step 4) and the response (step 5): phase 3 of an item.
    fn commit_item(&self, slot: Slot<'a, Resp>) {
        let Slot {
            item,
            response,
            state,
            ..
        } = slot;
        let SlotState::Done { ls, outcome } = state else {
            unreachable!("an item commits after its search");
        };
        let search_thread::ItemOutcome {
            locations,
            result,
            panic,
        } = outcome;
        // The item's program is current, as in Go's callback, which runs
        // inside the item's search.
        let _program = ls.as_ref().map(|ls| ls.get().enter_program());
        let committed = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for (uri, position) in locations {
                // Get default configured project for this file
                let def_projects = match self.orchestrator.get_projects_for_file(self.ctx, &uri) {
                    Ok(def_projects) => def_projects,
                    Err(_) => continue,
                };
                for def_project in def_projects {
                    // Optimization: don't enqueue if will be discarded
                    if self.can_search_project(&def_project) {
                        self.enqueue_item(ProjectAndTextDocumentPosition {
                            project: def_project,
                            ls: None,
                            uri: uri.clone(),
                            position,
                            symbol_data: None,
                            for_original_location: true,
                        });
                    }
                }
            }
        }));
        let panic_occurred = match committed {
            Err(payload) => Some(panic_occured_text(payload)),
            Ok(()) => panic,
        };
        if let Some(panic_occurred) = panic_occurred {
            self.panics_occurred
                .borrow_mut()
                .get_or_insert_with(Vec::new)
                .push(panic_occurred);
            return;
        }
        match result {
            None => {}
            Some(Ok(result)) => {
                let mut response = response.borrow_mut();
                response.complete = true;
                response.result = result;
                response.for_original_location = item.for_original_location;
            }
            Some(Err(err_search)) => {
                let mut err = self.err.borrow_mut();
                if err.is_none() {
                    *err = Some(err_search);
                }
            }
        }
    }

    // Go: ls/crossproject.go:184 getResultsIterator (closure)
    // PORT: returns the values the Go iterator yields, in order.
    fn get_results_iterator(&self) -> Vec<Resp> {
        let mut yielded: Vec<Resp> = Vec::new();
        let results = self.results.borrow();
        let mut seen_projects: FxHashSet<String> = FxHashSet::default();
        if let Some(response) = results.get(&self.default_project.id()) {
            let response = response.borrow();
            if response.complete {
                yielded.push(response.result.clone());
            }
        }
        seen_projects.insert(self.default_project.id());
        for project in &self.all_projects {
            if seen_projects.insert(project.id())
                && let Some(response) = results.get(&project.id())
            {
                let response = response.borrow();
                if response.complete {
                    yielded.push(response.result.clone());
                }
            }
        }
        // Prefer the searches from locations for default definition
        for (key, response) in results.iter() {
            let response = response.borrow();
            if !response.for_original_location
                && seen_projects.insert(key.clone())
                && response.complete
            {
                yielded.push(response.result.clone());
            }
        }
        // Then the searches from original locations
        for (key, response) in results.iter() {
            let response = response.borrow();
            if response.for_original_location
                && seen_projects.insert(key.clone())
                && response.complete
            {
                yielded.push(response.result.clone());
            }
        }
        yielded
    }
}

// Go: ls/crossproject.go:298 combineLocationArray
// PORT: Go `locations *[]T` is read only: `&[T]`.
pub fn combine_location_array<T: HasLocation + Clone>(
    mut combined: Vec<T>,
    locations: &[T],
    seen: &mut FxHashSet<lsproto::Location>,
) -> Vec<T> {
    for loc in locations {
        if seen.insert(loc.get_location()) {
            combined.push(loc.clone());
        }
    }
    combined
}

// Go: ls/crossproject.go:311 combineResponseLocations
// PORT: Go returns a non-nil `*[]lsproto.Location`: always `Some`.
pub fn combine_response_locations<T: HasLocations>(
    results: &[T],
) -> Option<Vec<lsproto::Location>> {
    let mut combined: Vec<lsproto::Location> = Vec::new();
    let mut seen_locations: FxHashSet<lsproto::Location> = FxHashSet::default();
    for resp in results {
        if let Some(locations) = resp.get_locations() {
            combined = combine_location_array(combined, locations, &mut seen_locations);
        }
    }
    Some(combined)
}

// Go: ls/crossproject.go:322 combineReferences
pub fn combine_references(results: &[lsproto::ReferencesResponse]) -> lsproto::ReferencesResponse {
    lsproto::LocationsOrNull {
        locations: combine_response_locations(results),
    }
}

// Go: ls/crossproject.go:326 combineVSReferences
pub fn combine_vs_references(
    results: &[lsproto::VSReferencesResponse],
) -> lsproto::VSReferencesResponse {
    let mut combined: Vec<lsproto::VSReferenceItem> = Vec::new();
    // Re-number IDs across projects to maintain unique IDs and correct definition references
    let mut next_id: i32 = 0;
    for resp in results {
        let Some(vs_reference_items) = &resp.vs_reference_items else {
            continue;
        };
        // Map old IDs to new IDs for this batch
        let mut id_map: FxHashMap<i32, i32> = FxHashMap::default();
        for item in vs_reference_items {
            let old_id = item.vs_id;
            let new_id = next_id;
            id_map.insert(old_id, new_id);
            next_id += 1;

            let mut new_item = item.clone();
            new_item.vs_id = new_id;
            if let Some(vs_definition_id) = item.vs_definition_id {
                let new_def_id = id_map.get(&vs_definition_id).copied().unwrap_or_default();
                new_item.vs_definition_id = Some(new_def_id);
            }
            combined.push(new_item);
        }
    }
    lsproto::VSReferenceItemsOrNull {
        vs_reference_items: Some(combined),
    }
}

// Go: ls/crossproject.go:354 combineImplementations
pub fn combine_implementations(
    results: &[lsproto::ImplementationResponse],
) -> lsproto::ImplementationResponse {
    let mut combined: Vec<lsproto::LocationLink> = Vec::new();
    let mut seen_locations: FxHashSet<lsproto::Location> = FxHashSet::default();
    for resp in results {
        if let Some(definition_links) = &resp.definition_links {
            combined = combine_location_array(combined, definition_links, &mut seen_locations);
        } else if resp.locations.is_some() {
            return lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
                locations: combine_response_locations(results),
                ..Default::default()
            };
        }
    }
    lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
        definition_links: Some(combined),
        ..Default::default()
    }
}

// Go: ls/crossproject.go:367 combineRenameResponse
// PORT: Go `combined` is a Go map, which the response marshals in random
// order. It is an `IndexMap` in first-insert order. Go ranges over each
// response's `Changes` map in random order; this uses its insertion order.
pub fn combine_rename_response(results: &[lsproto::RenameResponse]) -> lsproto::RenameResponse {
    let mut combined: IndexMap<lsproto::DocumentUri, Vec<Option<lsproto::TextEdit>>> =
        IndexMap::new();
    let mut seen_changes: FxHashMap<lsproto::DocumentUri, FxHashSet<lsproto::Range>> =
        FxHashMap::default();
    let mut document_changes: Vec<lsproto::TextDocumentEditOrCreateFileOrRenameFileOrDeleteFile> =
        Vec::new();
    let mut seen_renames: FxHashSet<[lsproto::DocumentUri; 2]> = FxHashSet::default();

    for resp in results {
        if let Some(workspace_edit) = &resp.workspace_edit
            && let Some(changes) = &workspace_edit.document_changes
        {
            for change in changes {
                match &change.rename_file {
                    Some(rename_file) => {
                        let key = [rename_file.old_uri.clone(), rename_file.new_uri.clone()];
                        if seen_renames.insert(key) {
                            document_changes.push(change.clone());
                        }
                    }
                    None => {
                        document_changes.push(change.clone());
                    }
                }
            }
        }
        if let Some(workspace_edit) = &resp.workspace_edit
            && let Some(changes_by_doc) = &workspace_edit.changes
        {
            for (doc, changes) in changes_by_doc {
                let seen_set = seen_changes.entry(doc.clone()).or_default();
                let mut changes_for_doc = combined.get(doc).cloned().unwrap_or_default();
                for change in changes {
                    // Go reads `change.Range` of a nil element.
                    let range = change
                        .as_ref()
                        .unwrap_or_else(|| crate::core::go_nil_dereference())
                        .range;
                    if !seen_set.contains(&range) {
                        seen_set.insert(range);
                        changes_for_doc.push(change.clone());
                    }
                }
                combined.insert(doc.clone(), changes_for_doc);
            }
        }
    }
    if !document_changes.is_empty() || !combined.is_empty() {
        let mut workspace_edit = lsproto::WorkspaceEdit::default();
        if !document_changes.is_empty() {
            workspace_edit.document_changes = Some(document_changes);
        }
        if !combined.is_empty() {
            workspace_edit.changes = Some(combined);
        }
        return lsproto::WorkspaceEditOrNull {
            workspace_edit: Some(workspace_edit),
        };
    }
    lsproto::WorkspaceEditOrNull::default()
}

// Go: ls/crossproject.go:423 combineIncomingCalls
pub fn combine_incoming_calls(
    results: &[lsproto::CallHierarchyIncomingCallsResponse],
) -> lsproto::CallHierarchyIncomingCallsResponse {
    let mut combined: Vec<lsproto::CallHierarchyIncomingCall> = Vec::new();
    let mut seen_calls: FxHashSet<lsproto::Location> = FxHashSet::default();
    for resp in results {
        if let Some(call_hierarchy_incoming_calls) = &resp.call_hierarchy_incoming_calls {
            for call in call_hierarchy_incoming_calls {
                if seen_calls.insert(
                    call.from
                        .as_ref()
                        .unwrap_or_else(|| crate::core::go_nil_dereference())
                        .get_location(),
                ) {
                    combined.push(call.clone());
                }
            }
        }
    }
    lsproto::CallHierarchyIncomingCallsOrNull {
        call_hierarchy_incoming_calls: Some(combined),
    }
}
