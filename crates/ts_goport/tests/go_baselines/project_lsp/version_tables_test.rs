//! PORT: no Go counterpart. Go's GC frees a program with its tables when no
//! snapshot, request or goroutine holds it. The port keeps the program
//! shell leaked and frees the per-version tables
//! (`program::VersionTables`) at the release (lsshells M2a). A thread that
//! was seeded from the program before the release (a checker, bind, emit
//! or search thread, `program::WorkerSeed`) keeps its copy until it ends.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ts_goport::core::{GoProgram, enter_program};
use ts_goport::frontend::compiler::NewProgram;
use ts_goport::program::{self, VersionTablesProbe, ls_program};
use ts_goport::project::Session;

use super::projecttestutil::{FileMap, files, wrapped_map_fs};
use super::util::*;

const CONFIG: &str = "/home/projects/TS/p1/tsconfig.json";
const INDEX_URI: &str = "file:///home/projects/TS/p1/index.ts";
const INDEX_TEXT: &str = "import { a } from './a';\nexport const x = a + 1;";
const A_FILE: &str = "/home/projects/TS/p1/a.ts";

fn p1_files() -> FileMap {
    files(&[
        (CONFIG, "{}"),
        ("/home/projects/TS/p1/index.ts", INDEX_TEXT),
        (A_FILE, "export const a = 1;"),
        ("/home/projects/TS/p1/b.ts", "export const b = 1;"),
    ])
}

/// A session with index.ts open and its program loaded.
fn open_p1() -> Rc<Session> {
    let session = bare_session(p1_files());
    open(&session, INDEX_URI, INDEX_TEXT);
    let _ = language_service(&session, INDEX_URI);
    session
}

/// Replaces the `1` of `INDEX_TEXT` (a body edit, which clones the
/// program) and loads the program. The snapshot change releases the
/// program before, once the background tasks that hold the old snapshot
/// ran.
fn body_edit(session: &Rc<Session>, version: i32, digit: &str) {
    edit(session, INDEX_URI, version, (1, 21), (1, 22), digit);
    let _ = language_service(session, INDEX_URI);
    session.wait_for_background_tasks();
}

/// Adds an import to index.ts (a new program load) and loads the program.
fn import_edit(session: &Rc<Session>, version: i32) {
    edit(
        session,
        INDEX_URI,
        version,
        (0, 0),
        (0, 0),
        "import { b } from './b';\n",
    );
    let _ = language_service(session, INDEX_URI);
    session.wait_for_background_tasks();
}

/// The program version of `p` and a probe of its tables.
fn version_and_probe(p: &NewProgram) -> (&'static GoProgram, VersionTablesProbe) {
    let version = ls_program::program_version(p);
    let probe = program::version_tables_probe(version).expect("a program version has tables");
    (version, probe)
}

/// Waits until `probe` sees the tables freed.
fn wait_freed(probe: &VersionTablesProbe) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !probe.is_freed() {
        assert!(
            Instant::now() < deadline,
            "the tables were not freed in 60 s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

child_test! {
    // A released load and a released clone free their tables. The live
    // program keeps its tables and still answers.
    fn released_program_versions_free_their_tables() {
        let session = open_p1();
        let (_, load) = version_and_probe(&program(&session, INDEX_URI));

        body_edit(&session, 2, "2");
        let (_, clone) = version_and_probe(&program(&session, INDEX_URI));
        assert!(load.is_freed(), "the released load keeps its tables");
        assert!(!clone.is_freed());

        import_edit(&session, 3);
        let p3 = program(&session, INDEX_URI);
        let (_, live) = version_and_probe(&p3);
        assert!(clone.is_freed(), "the released clone keeps its tables");
        assert!(!live.is_freed());
        assert_eq!(sem_diag_count(&p3, "/home/projects/TS/p1/index.ts"), 0);
        let _program = ls_program::enter(&p3);
        assert!(program::get_source_file(A_FILE).is_some());
    }
}

child_test! {
    // A read of a released version's tables on the dispatch thread panics
    // with the version id. It does not read another version's data.
    fn read_of_released_program_version_panics() {
        let session = open_p1();
        let (version, probe) = version_and_probe(&program(&session, INDEX_URI));
        body_edit(&session, 2, "2");
        assert!(probe.is_freed());

        let _scope = enter_program(Some(version));
        let read = std::panic::catch_unwind(|| program::get_source_file(A_FILE));
        let payload = read.expect_err("a read of a released program version must panic");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert_eq!(message, format!("program version {} is released", version.id));
    }
}

child_test! {
    // A thread seeded before the release (as a search thread is) keeps the
    // tables and reads them after the release. They are freed when it ends.
    fn seeded_thread_keeps_tables_after_release() {
        let session = open_p1();
        let p1 = program(&session, INDEX_URI);
        let (_, probe) = version_and_probe(&p1);
        let (start, started) = mpsc::channel::<()>();
        let reader = {
            let _program = ls_program::enter(&p1);
            program::spawn_seeded_thread(move || {
                started.recv().expect("the test thread sends start");
                program::get_source_file(A_FILE).is_some()
            })
        };

        body_edit(&session, 2, "2");
        assert!(!probe.is_freed(), "the seeded thread holds the tables");
        start.send(()).expect("the seeded thread waits");
        assert!(reader.join().expect("the seeded thread reads the released version"));
        assert!(probe.is_freed());
    }
}

child_test! {
    // `release_program_in_background` (tsc -b) does not wait for the
    // checker workers. They hold the tables until they end, and so does a
    // seeded thread; the tables are freed after the last one ends.
    fn background_release_frees_tables_after_workers_end() {
        let _fs = wrapped_map_fs(p1_files(), false);
        let version = program::try_load_version(CONFIG, |_| {})
            .unwrap_or_else(|error| panic!("cannot load {CONFIG}: {error}"));
        let probe = program::version_tables_probe(version).expect("a program version has tables");
        let (start, started) = mpsc::channel::<()>();
        let reader = {
            let _scope = enter_program(Some(version));
            // Makes the checker pool; each worker gets the tables in its seed.
            let _ = program::get_global_diagnostics();
            program::spawn_seeded_thread(move || {
                started.recv().expect("the test thread sends start");
                program::get_source_file(A_FILE).is_some()
            })
        };

        program::release_program_in_background(version);
        assert!(!probe.is_freed(), "the seeded thread holds the tables");
        start.send(()).expect("the seeded thread waits");
        assert!(reader.join().expect("the seeded thread reads the released version"));
        wait_freed(&probe);
    }
}

child_test! {
    env &[("GOPORT_CHECK_VERSION_TABLES", "1")];
    // editfast1: a body edit replaces index.ts in place (Go `ReuseProgram`),
    // so the tables of the new version start from the old version's. An
    // edit that adds a program file (an import of a file that the config
    // does not list) builds them from the files alone, and the next body
    // edit starts from those. With GOPORT_CHECK_VERSION_TABLES=1 each
    // reused build is also compared with a full build.
    fn version_tables_reuse_only_files_replaced_in_place() {
        const B_FILE: &str = "/home/projects/TS/p1/b.ts";
        const INDEX_FILE: &str = "/home/projects/TS/p1/index.ts";
        let session = bare_session(files(&[
            (CONFIG, r#"{"files": ["index.ts"]}"#),
            (INDEX_FILE, INDEX_TEXT),
            (A_FILE, "export const a = 1;"),
            (B_FILE, "export const b = 1;"),
        ]));
        open(&session, INDEX_URI, INDEX_TEXT);
        let reused = |p: &NewProgram| {
            ls_program::version_tables_reused(ls_program::program_version(p))
        };
        let p1 = program(&session, INDEX_URI);
        let count = p1.source_files().len();
        assert!(!reused(&p1), "a first load builds its tables");

        body_edit(&session, 2, "2");
        let p2 = program(&session, INDEX_URI);
        assert_eq!(p2.source_files().len(), count);
        assert!(reused(&p2), "a body edit starts from the old tables");

        import_edit(&session, 3);
        let p3 = program(&session, INDEX_URI);
        assert_eq!(p3.source_files().len(), count + 1, "b.ts joins the program");
        assert!(!reused(&p3), "a new program file gives a full build");
        assert_eq!(sem_diag_count(&p3, INDEX_FILE), 0);
        {
            let _program = ls_program::enter(&p3);
            assert!(program::get_source_file(B_FILE).is_some());
        }

        // `export const x = a + 1;` is line 2 after the import edit.
        edit(&session, INDEX_URI, 4, (2, 21), (2, 22), "3");
        let p4 = program(&session, INDEX_URI);
        assert_eq!(p4.source_files().len(), count + 1);
        assert!(reused(&p4), "a body edit after a full build starts from its tables");
        assert_eq!(sem_diag_count(&p4, INDEX_FILE), 0);
        let _program = ls_program::enter(&p4);
        assert!(program::get_source_file(B_FILE).is_some());
    }
}
