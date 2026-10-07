//! The extension hooks (`ts_goport::ext`, the Effect-TS/tsgo patch hooks).
//! A recording extension sees each hook for a project whose tsconfig lists
//! its plugin, and no program hook for a project that does not. Before the
//! install no hook runs, and the output of the project without the plugin
//! is the same before and after the install.
//!
//! Its own test binary: the extension is process-wide and installed once.
//! One test, so the phases run in order. The projects are real files in a
//! temporary directory. tsc runs through `execute::command_line` on the OS
//! file system, each run in a child process of this binary (a process
//! installs one tsc program). The language service runs in this process
//! through a `project::Session`.
//! Not covered here: `Extension::command`, which only the bins call.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use ts_goport::checker::new_diagnostic_for_node;
use ts_goport::diagnostics::{Category, Message};
use ts_goport::execute::execute_tsc::{GoTsc, command_line};
use ts_goport::execute::tsc::new_os_system;
use ts_goport::ext::{self, ExtOptions, Extension, FixTransformer, Mode};
use ts_goport::frontend::bundled;
use ts_goport::frontend::compiler::NewProgram;
use ts_goport::frontend::tsoptions::CompilerOptionsValue;
use ts_goport::frontend::vfs::osvfs;
use ts_goport::gostd::{Context, GoError, context, errors};
use ts_goport::ls::autoimport::{Export, Fix};
use ts_goport::ls::lsconv::Converters;
use ts_goport::ls::lsutil::{IncludeInlayParameterNameHints, InlayHintsPreferences};
use ts_goport::ls::{self, CodeAction, CodeFixContext, CodeFixProvider, LanguageService};
use ts_goport::lsp::lsproto;
use ts_goport::modulespecifiers::UserPreferences;
use ts_goport::prelude::{
    Checker, CompilerOptions, Diagnostic, Node, SymbolId, TextRange, source_file_file_name,
};
use ts_goport::program::ls_program;
use ts_goport::project::{self, CheckerPoolOptions, SessionInit, SessionOptions};

/// The code of the recorder's diagnostic (in Effect's 377xxx range).
const CODE: i32 = 377_999;
static MESSAGE: Message = Message::new(
    CODE as u32,
    Category::Error,
    "rec_diagnostic_377999",
    "rec diagnostic",
    false,
    false,
    false,
);
const PLUGIN: &str = "rec-plugin";

static EVENTS: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

fn record(name: &str) {
    *EVENTS.lock().unwrap().entry(name.to_string()).or_default() += 1;
}

/// The recorded hook names since the last call.
fn take_events() -> Vec<String> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
        .into_keys()
        .collect()
}

#[derive(Debug, PartialEq)]
struct RecOptions;

#[allow(clippy::unnecessary_wraps)] // the `CodeFixProvider` signature
fn rec_code_actions(
    _ctx: &Context,
    _fix_context: &CodeFixContext<'_>,
) -> Result<Vec<CodeAction>, GoError> {
    record("code_fix_provider");
    Ok(Vec::new())
}

static PROVIDER: LazyLock<CodeFixProvider> = LazyLock::new(|| CodeFixProvider {
    error_codes: vec![CODE],
    get_code_actions: rec_code_actions,
    fix_ids: Vec::new(),
    get_all_code_actions: None,
});
static PROVIDERS: LazyLock<[&'static CodeFixProvider; 1]> = LazyLock::new(|| [&*PROVIDER]);

/// Records each hook. Returns its input unchanged, except where a phase
/// checks the effect: a diagnostic after the check, a refactor, a hover
/// suffix and the `refactor.rewrite` kind.
struct Recorder;

impl Extension for Recorder {
    fn after_check_source_file(&self, ctx: &Context, c: &mut Checker, source_file: Node) {
        record("after_check_source_file");
        if ext::mode() == Mode::CommandLine {
            record("mode_command_line");
        }
        if !c.get_relation_errors(ctx, source_file).is_empty() {
            record("relation_errors");
        }
        if c.ext_links.is_none() {
            c.ext_links = Some(Box::new(RecOptions));
        }
        // On the statement after `// @ts-ignore` in a.ts.
        if source_file_file_name(source_file).ends_with("/a.ts") {
            let statement = source_file.statements().into_iter().next().unwrap();
            c.diagnostics
                .add(new_diagnostic_for_node(statement, &MESSAGE, Vec::new()));
        }
    }

    fn message_by_key(&self, key: &str) -> Option<&'static Message> {
        record("message_by_key");
        (key == MESSAGE.key()).then_some(&MESSAGE)
    }

    fn unsuppressible_code(&self, code: i32) -> bool {
        record("unsuppressible_code");
        code == CODE
    }

    fn counts_for_exit_code(&self, _options: &CompilerOptions, diagnostic: &Diagnostic) -> bool {
        record("counts_for_exit_code");
        diagnostic.code != CODE
    }

    fn blocks_emit_on_error(&self, _options: &CompilerOptions, diagnostic: &Diagnostic) -> bool {
        record("blocks_emit_on_error");
        diagnostic.code != CODE
    }

    fn code_fix_providers(&self) -> &'static [&'static CodeFixProvider] {
        record("code_fix_providers");
        &*PROVIDERS
    }

    fn refactor_actions(
        &self,
        _ctx: &Context,
        _file: Node,
        _span: TextRange,
        _program: &NewProgram,
        _ls: &LanguageService,
    ) -> Result<Vec<CodeAction>, GoError> {
        record("refactor_actions");
        Ok(vec![CodeAction {
            description: "rec refactor".to_string(),
            ..Default::default()
        }])
    }

    fn after_quick_info(
        &self,
        _program: &NewProgram,
        _c: &mut Checker,
        _file: Node,
        _node: Node,
        _symbol: SymbolId,
        _quick_info: &mut String,
        documentation: &mut String,
        _is_markdown: bool,
    ) -> Option<Node> {
        record("after_quick_info");
        documentation.push_str(" rec-hover");
        None
    }

    fn parse_plugins(&self, value: &CompilerOptionsValue) -> Option<ExtOptions> {
        record("parse_plugins");
        let CompilerOptionsValue::List(plugins) = value else {
            return None;
        };
        plugins
            .iter()
            .any(|plugin| {
                matches!(plugin, CompilerOptionsValue::Map(map)
                    if matches!(map.get("name"), Some(CompilerOptionsValue::String(name)) if name == PLUGIN))
            })
            .then(|| ExtOptions::new(RecOptions))
    }

    fn merge_options(
        &self,
        target: &mut CompilerOptions,
        source: &CompilerOptions,
        _raw_source: Option<&IndexMap<String, CompilerOptionsValue>>,
        source_config_path: &str,
        _base_path: &str,
    ) {
        record("merge_options");
        if source_config_path.ends_with("base.json") {
            record("merge_options_source_path");
        }
        target.ext.clone_from(&source.ext);
    }

    fn after_inlay_hints(
        &self,
        _program: &NewProgram,
        _c: &mut Checker,
        _file: Node,
        _span: TextRange,
        _preferences: &InlayHintsPreferences,
        _hints: &mut Vec<lsproto::InlayHint>,
        _converters: &Converters,
    ) {
        record("after_inlay_hints");
    }

    fn advertises_refactor_rewrite(&self) -> bool {
        record("advertises_refactor_rewrite");
        true
    }

    fn auto_import_fix_transformer(
        &self,
        _preferences: &UserPreferences,
        _program: &Rc<NewProgram>,
        _importing_file: Node,
    ) -> Option<FixTransformer> {
        record("auto_import_fix_transformer");
        Some(Box::new(|_export: &Export, fixes: Vec<Rc<Fix>>| {
            record("fix_transformer");
            fixes
        }))
    }

    fn after_completion(
        &self,
        _ctx: &Context,
        _file: Node,
        _position: i32,
        _items: &mut Vec<lsproto::CompletionItem>,
        _program: &NewProgram,
        _ls: &LanguageService,
    ) {
        record("after_completion");
    }

    fn after_document_symbols(
        &self,
        _ctx: &Context,
        _file: Node,
        _symbols: &mut Vec<lsproto::DocumentSymbol>,
        _program: &NewProgram,
        _ls: &LanguageService,
    ) {
        record("after_document_symbols");
    }

    fn buildinfo_options(&self, options: &ExtOptions) -> Option<String> {
        record("buildinfo_options");
        options
            .get::<RecOptions>()
            .map(|_| r#"{"rec":true}"#.to_string())
    }

    fn options_from_buildinfo(&self, json: &str) -> Option<ExtOptions> {
        record("options_from_buildinfo");
        (json == r#"{"rec":true}"#).then(|| ExtOptions::new(RecOptions))
    }

    fn validate_options(&self, _options: &CompilerOptions, _config_file: Node) -> Vec<Diagnostic> {
        record("validate_options");
        Vec::new()
    }
}

static RECORDER: Recorder = Recorder;

const A_TS: &str = "// @ts-ignore\nexport const a: number = \"s\";\nfunction f(x: number) { return x; }\nf(1);\nexport const n = f;\n";
const B_TS: &str = "export const fooBarBaz = 1;\n";

/// Writes a project: `base.json` (the plugin when `plugin`), `tsconfig.json`
/// that extends it, `a.ts` and `b.ts`.
fn write_project(dir: &Path, plugin: bool) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let plugins = if plugin {
        format!(r#","plugins": [{{"name": "{PLUGIN}"}}]"#)
    } else {
        String::new()
    };
    std::fs::write(
        dir.join("base.json"),
        format!(r#"{{"compilerOptions": {{"strict": true{plugins}}}}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("tsconfig.json"),
        r#"{"extends": "./base.json", "compilerOptions": {"noEmitOnError": true, "incremental": true, "outDir": "out", "tsBuildInfoFile": "out/tsbuildinfo", "types": [], "skipLibCheck": true}, "files": ["a.ts", "b.ts"]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("a.ts"), A_TS).unwrap();
    std::fs::write(dir.join("b.ts"), B_TS).unwrap();
}

/// The tsc output of `tsc`.
struct Buffer(Rc<RefCell<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs tsc on the project in `dir` and returns its exit code and output.
fn tsc(dir: &Path) -> (i32, String) {
    let out: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
    let sys = new_os_system()
        .ok()
        .unwrap()
        .with_writer(Rc::new(RefCell::new(Buffer(out.clone()))));
    let args = vec![
        "-p".to_string(),
        dir.join("tsconfig.json").to_string_lossy().into_owned(),
        "--pretty".to_string(),
        "false".to_string(),
    ];
    let status = command_line(&context::background(), Rc::new(sys), &args, &GoTsc).status;
    let text = String::from_utf8_lossy(&out.borrow()).into_owned();
    (status.code(), text)
}

/// A language service session on the OS file system, in `cwd`.
fn new_session(cwd: &str) -> Rc<project::Session> {
    project::new_session(&SessionInit {
        background_ctx: context::background(),
        options: Rc::new(SessionOptions {
            current_directory: cwd.to_string(),
            default_library_path: bundled::lib_path_exported(),
            typings_location: String::new(),
            position_encoding: lsproto::PositionEncodingKind::UTF8,
            watch_enabled: false,
            logging_enabled: false,
            telemetry_enabled: false,
            push_diagnostics_enabled: false,
            run_external_code: false,
            debounce_delay: Duration::ZERO,
            checker_pool_options: CheckerPoolOptions::default(),
        }),
        fs: bundled::wrap_fs_exported(osvfs::osvfs_fs()),
        client: None,
        logger: None,
        npm_executor: None,
        spawner: None,
        content_mapper_logger: None,
        parse_cache: None,
        content_mapped_parse_cache: None,
    })
}

/// The answers of the language service requests that reach every LS hook,
/// as text, for the project in `dir`.
fn ls_answers(dir: &Path) -> Vec<String> {
    let cwd = dir.to_string_lossy().into_owned();
    let session = new_session(&cwd);
    let ctx = context::background();
    let uri = lsproto::DocumentUri(format!("file://{cwd}/a.ts"));
    session.did_open_file(&ctx, &uri, 1, A_TS, &lsproto::LanguageKind::TYPE_SCRIPT);
    let mut ls = session.get_language_service(&ctx, &uri).unwrap();
    ls.active_config
        .inlay_hints
        .include_inlay_parameter_name_hints = IncludeInlayParameterNameHints::ALL;
    let file = ls
        .program
        .get_source_file(&format!("{cwd}/a.ts"))
        .unwrap()
        .root;
    let position = |line: u32, character: u32| lsproto::Position { line, character };
    let range = |start, end| lsproto::Range { start, end };
    let text_document = lsproto::TextDocumentIdentifier { uri: uri.clone() };
    let mut answers = Vec::new();

    let diagnostics = ls_program::get_semantic_diagnostics(&ls.program, &ctx, file);
    answers.push(format!(
        "diagnostics {:?}",
        diagnostics.iter().map(|d| d.code).collect::<Vec<_>>()
    ));

    let hover = ls
        .provide_hover(
            &ctx,
            &lsproto::HoverParams {
                text_document: text_document.clone(),
                position: position(2, 9),
                ..Default::default()
            },
        )
        .unwrap();
    answers.push(format!("hover {hover:?}"));

    let hints = ls
        .provide_inlay_hint(
            &ctx,
            &lsproto::InlayHintParams {
                text_document: text_document.clone(),
                range: range(position(0, 0), position(5, 0)),
                ..Default::default()
            },
        )
        .unwrap();
    answers.push(format!("inlay hints {hints:?}"));

    let symbols = ls.provide_document_symbols(&ctx, &uri).unwrap();
    answers.push(format!("symbols {symbols:?}"));

    let diagnostic = lsproto::Diagnostic {
        range: range(position(1, 0), position(1, 6)),
        code: Some(lsproto::IntegerOrString {
            integer: Some(CODE),
            ..Default::default()
        }),
        ..Default::default()
    };
    let actions = ls
        .provide_code_actions(
            &ctx,
            &lsproto::CodeActionParams {
                text_document: text_document.clone(),
                range: range(position(1, 0), position(1, 6)),
                context: Some(lsproto::CodeActionContext {
                    diagnostics: vec![Some(diagnostic)],
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
    answers.push(format!("code actions {actions:?}"));

    // After `f` on the last line of a.ts: the auto import of `fooBarBaz` from b.ts.
    let at = position(4, 18);
    let completion = match ls.provide_completion(&ctx, &uri, at, None) {
        Err(err) if errors::is(&err, &ls::ERR_NEEDS_AUTO_IMPORTS) => {
            let ls = session
                .get_language_service_with_auto_imports(&ctx, &session.snapshot(), &uri)
                .unwrap();
            ls.provide_completion(&ctx, &uri, at, None).unwrap()
        }
        result => result.unwrap(),
    };
    let item = completion
        .list
        .as_ref()
        .and_then(|list| list.items.iter().find(|item| item.label == "fooBarBaz"))
        .expect("an auto-import completion for fooBarBaz");
    answers.push(format!("completion {item:?}"));
    session.close();
    answers
}

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ts-ext-hooks-{}-{name}", std::process::id()))
}

const TEST: &str = "a_recording_extension_sees_each_hook_only_for_a_project_with_its_plugin";

/// One tsc run in a child process of this test binary (a process installs
/// one tsc program), with the recorder installed when `with_ext`. Returns
/// the exit code, the output and the recorded hooks.
fn tsc_in_child(dir: &Path, with_ext: bool) -> (i32, String, Vec<String>) {
    let result = dir.with_extension("result");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--test-threads=1"])
        .env("EXT_HOOKS_CHILD", if with_ext { "ext" } else { "none" })
        .env("EXT_HOOKS_DIR", dir)
        .env("EXT_HOOKS_RESULT", &result)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let text = std::fs::read_to_string(&result).unwrap();
    let _ = std::fs::remove_file(&result);
    let mut lines = text.splitn(3, '\n');
    let code = lines.next().unwrap().parse().unwrap();
    let events = lines
        .next()
        .unwrap()
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    (code, lines.next().unwrap_or_default().to_string(), events)
}

/// The child side of `tsc_in_child`.
fn child(mode: &str) {
    if mode == "ext" {
        ext::install(&RECORDER);
    }
    let dir = PathBuf::from(std::env::var("EXT_HOOKS_DIR").unwrap());
    let (code, output) = tsc(&dir);
    let events = take_events().join(",");
    std::fs::write(
        std::env::var("EXT_HOOKS_RESULT").unwrap(),
        format!("{code}\n{events}\n{output}"),
    )
    .unwrap();
}

#[test]
fn a_recording_extension_sees_each_hook_only_for_a_project_with_its_plugin() {
    if let Ok(mode) = std::env::var("EXT_HOOKS_CHILD") {
        child(&mode);
        return;
    }
    let plain = temp_dir("plain");
    let with_plugin = temp_dir("plugin");
    write_project(&plain, false);
    write_project(&with_plugin, true);

    // No extension: no hook runs, and the plugin changes nothing.
    assert!(ext::get().is_none());
    let plain_before = tsc_in_child(&plain, false);
    let plugin_before = tsc_in_child(&with_plugin, false);
    let plain_ls_before = ls_answers(&plain);
    assert_eq!(plain_before.0, 0, "{}", plain_before.1);
    assert_eq!(plain_before, plugin_before);
    assert_eq!(plain_before.2, Vec::<String>::new());
    assert_eq!(take_events(), Vec::<String>::new());

    ext::install(&RECORDER);

    // A project without the plugin: no program hook runs, same output.
    write_project(&plain, false);
    assert_eq!(tsc_in_child(&plain, true), plain_before);
    assert_eq!(ls_answers(&plain), plain_ls_before);
    assert_eq!(take_events(), Vec::<String>::new());

    // The project with the plugin: every hook runs.
    write_project(&with_plugin, true);
    let (code, output, mut events) = tsc_in_child(&with_plugin, true);
    // The diagnostic stays despite `@ts-ignore` and counts for neither the
    // exit code nor `noEmitOnError`.
    assert!(
        output.contains("error TS377999: rec diagnostic"),
        "{output}"
    );
    assert_eq!(code, 0, "{output}");
    assert!(with_plugin.join("out/a.js").exists());
    let build_info = std::fs::read_to_string(with_plugin.join("out/tsbuildinfo")).unwrap();
    assert!(
        build_info.contains(r#""effect":{"rec":true}"#),
        "{build_info}"
    );
    // Again: the options come back from the build info.
    let (code_again, output_again, events_again) = tsc_in_child(&with_plugin, true);
    assert_eq!(code_again, 0, "{output_again}");
    events.extend(events_again);

    let answers = ls_answers(&with_plugin);
    let all = answers.join("\n");
    assert!(answers[0].contains("377999"), "{all}");
    assert!(answers[1].contains("rec-hover"), "{all}");
    assert!(answers[4].contains("rec refactor"), "{all}");
    assert!(answers[4].contains("refactor.rewrite"), "{all}");
    assert!(answers[5].contains("usage_position: Some"), "{all}");
    let plain_completion = &plain_ls_before[5];
    assert!(
        plain_completion.contains("usage_position: None"),
        "{plain_completion}"
    );

    let kinds = ts_goport::lsp::supported_code_action_kinds();
    assert_eq!(kinds[1], lsproto::CodeActionKind::REFACTOR_REWRITE);

    events.extend(take_events());
    events.sort();
    events.dedup();
    let want = [
        "advertises_refactor_rewrite",
        "after_check_source_file",
        "after_completion",
        "after_document_symbols",
        "after_inlay_hints",
        "after_quick_info",
        "auto_import_fix_transformer",
        "blocks_emit_on_error",
        "buildinfo_options",
        "code_fix_provider",
        "code_fix_providers",
        "counts_for_exit_code",
        "fix_transformer",
        "merge_options",
        "merge_options_source_path",
        "message_by_key",
        "mode_command_line",
        "options_from_buildinfo",
        "parse_plugins",
        "refactor_actions",
        "relation_errors",
        "unsuppressible_code",
        "validate_options",
    ];
    assert_eq!(events, want);

    let _ = std::fs::remove_dir_all(&plain);
    let _ = std::fs::remove_dir_all(&with_plugin);
}
