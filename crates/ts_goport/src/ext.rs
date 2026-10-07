//! Extension hooks: the Rust form of the hook points that Effect-TS/tsgo
//! patches into typescript-go (`_patches/typescript/*.patch` of
//! `@effect/tsgo`). Go keeps one package-level callback per hook, set from
//! the `init()` of the Effect packages. Here one process-wide `Extension`
//! is installed at start (`install`, from `crate::effect::install`), and
//! each hook site asks `get()`.
//!
//! Rules for every hook site:
//! - With no extension installed, the site does what plain tsgo does.
//! - A program-scoped hook runs only when the options carry extension
//!   options (`CompilerOptions::ext` is `Some`), that is when the extension
//!   claimed the project in `parse_plugins` or `merge_options`. A project
//!   that lists no plugin of the extension gets plain tsgo output, at the
//!   cost of one atomic load and a branch per site. The Effect callbacks
//!   that Go calls for every project return their input unchanged when
//!   `CompilerOptions.Effect` is nil, so this gate changes no output.
//!   `parse_plugins` (which makes those options), `message_by_key`,
//!   `advertises_refactor_rewrite` and `command` are not program-scoped.
//! - `crate::effect` uses only this module and the public checker and LS
//!   APIs.
//!
//! The 27 patches (Effect-TS/tsgo d1e539c, `@effect/tsgo` 0.48.1). T is a
//! method of `Extension`, F a field, E a public method, G a core edit gated
//! as above, S skipped.
//! - 001 `cmd/tsc/main.go` init imports, `--effect-cli-diagnostics`: T
//!   `command`; `crate::effect::install` in `bin/tsgo.rs` and
//!   `cmd/tsgo/main.rs` `run_main`.
//! - 002 `checker.go` `AfterCheckSourceFileCallback`: T
//!   `after_check_source_file`. E: Go `AddDiagnostic` is
//!   `checker.diagnostics.add`, Go `Program()` is `checker.program`,
//!   `Checker::get_relation_errors`, and
//!   `program::is_source_file_from_external_library` (plus the alias
//!   resolver's `is_source_file_from_external_library`).
//! - 003 `exports.go` `GetTypeArgumentsForResolvedSignature`: E.
//! - 004 `relater.go` `reportRelationError` collects relation errors: G.
//! - 005 `types.go` `SourceFileLinks.relationErrors`: F `relation_errors`.
//! - 006 `program.go` 377xxx codes ignore `@ts-ignore`: T
//!   `unsuppressible_code`.
//! - 007 `compileroptions.go` `Effect`: F `CompilerOptions::ext`, never in
//!   JSON (`options_json.rs`).
//! - 008 `diagnostics/generate.go` merges the Effect messages into the
//!   message table: T `message_by_key` (the Effect module keeps its own
//!   messages; a diagnostic read back from build info finds its message by
//!   key).
//! - 009 `execute/tsc.go` `EnterCommandLineMode`, `emit.go` and
//!   `program.go` diagnostic filters: `enter_mode`, T `counts_for_exit_code`
//!   and `blocks_emit_on_error`.
//! - 010 fourslash helpers: S (test only).
//! - 011 `codeactions.go` external code fix and refactor providers,
//!   `CodeAction.Kind`: T `code_fix_providers`, `refactor_actions`; F
//!   `CodeAction::kind`.
//! - 012 `hover.go` `AfterQuickInfoCallback`: T `after_quick_info`.
//! - 013 `parsinghelpers.go` plugin options and merge: T `parse_plugins`,
//!   `merge_options`.
//! - 014 `ast/utilities.go` nil check: S (a `Node` handle is never a nil
//!   pointer at those call sites).
//! - 015 `inlay_hints.go` `AfterInlayHintsCallback`: T `after_inlay_hints`.
//! - 017 `lsp/server.go` `refactor.rewrite` code action kind: T
//!   `advertises_refactor_rewrite` (initialize is compared by the LSP
//!   oracle, so the extension adds it only when asked).
//! - 018 `ls/autoimport` style policy: T `auto_import_fix_transformer`; G
//!   for `UsagePosition` (it is serialized into completion data).
//! - 021 `core/version.go` version suffix: S (it changes `--version`,
//!   `serverInfo.version` and build info for every project).
//! - 022 `completions.go` `AfterCompletionCallback`: T `after_completion`.
//! - 023 `checker.go` `Checker.EffectLinks`: F `Checker::ext_links`.
//! - 024 `symbols.go` `AfterDocumentSymbolsCallback`: T
//!   `after_document_symbols`.
//! - 025 `tsconfigparsing.go` config path and base path for the merge: the
//!   `merge_options` arguments.
//! - 027 `autoimport/view.go` transformer per view: with 018.
//! - 028 `buildInfo.go` `effect` key, `declscompiler.go` recheck on change:
//!   T `buildinfo_options`, `options_from_buildinfo`; G in
//!   `compiler_options_affect_semantic_diagnostics`.
//! - 029 `lsconv/converters.go` old names: S.
//! - 030 `tsconfigparsing.go` `ValidateCompilerOptionsCallback`: T
//!   `validate_options`.
//! - 031 `@stability` JSDoc node flag: S (it changes the API node flags and
//!   the lib blobs for every project).

use crate::diagnostics::Message;
use crate::frontend::compiler::NewProgram;
use crate::frontend::tsoptions::CompilerOptionsValue;
use crate::gostd::{Context, GoError};
use crate::ls::autoimport::{Export, Fix};
use crate::ls::lsconv::Converters;
use crate::ls::lsutil::InlayHintsPreferences;
use crate::ls::{CodeAction, CodeFixProvider, LanguageService};
use crate::lsp::lsproto;
use crate::modulespecifiers::UserPreferences;
use crate::prelude::{Checker, CompilerOptions, Diagnostic, Node, SymbolId, TextRange};
use indexmap::IndexMap;
use std::any::Any;
use std::fmt::Debug;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

/// The hooks of one extension. Every method has a default that does what
/// plain tsgo does. See the module header for when each one runs.
pub trait Extension: Sync {
    /// Effect patch 001: a first command line argument that the extension
    /// runs itself (Go `case "--effect-cli-diagnostics"` in `runMain`). The
    /// command gets the arguments after `name` and returns the exit code.
    fn command(&self, name: &str) -> Option<fn(Vec<String>) -> i32> {
        None
    }

    /// Effect patch 002: runs after `check_source_file` type checks a file,
    /// before the unused identifier check. Add diagnostics with
    /// `c.diagnostics.add`.
    fn after_check_source_file(&self, ctx: &Context, c: &mut Checker, source_file: Node) {}

    /// Effect patch 006: a diagnostic with this code is never suppressed by
    /// `@ts-ignore` or `@ts-expect-error`.
    fn unsuppressible_code(&self, code: i32) -> bool {
        false
    }

    /// Effect patch 008: the extension's message with this key, for a
    /// diagnostic read back from build info (Go finds it in the message
    /// table, which has the Effect messages).
    fn message_by_key(&self, key: &str) -> Option<&'static Message> {
        None
    }

    /// Effect patch 009 (`FilterDiagnosticsForExitCodeCallback`): false
    /// keeps `diagnostic` out of the exit code. It is still reported.
    fn counts_for_exit_code(&self, options: &CompilerOptions, diagnostic: &Diagnostic) -> bool {
        true
    }

    /// Effect patch 009 (`FilterDiagnosticsForNoEmitOnErrorCallback`): false
    /// drops `diagnostic` from the diagnostics that block emit under
    /// `noEmitOnError`.
    fn blocks_emit_on_error(&self, options: &CompilerOptions, diagnostic: &Diagnostic) -> bool {
        true
    }

    /// Effect patch 011: code fix providers after the built-in ones.
    fn code_fix_providers(&self) -> &'static [&'static CodeFixProvider] {
        &[]
    }

    /// Effect patch 011 (`RefactorProvider.GetRefactorActions`): refactors
    /// for `span` of `file`. An action with no `kind` is
    /// `refactor.rewrite`.
    fn refactor_actions(
        &self,
        ctx: &Context,
        file: Node,
        span: TextRange,
        program: &NewProgram,
        ls: &LanguageService,
    ) -> Result<Vec<CodeAction>, GoError> {
        Ok(Vec::new())
    }

    /// Effect patch 012 (`AfterQuickInfoCallback`): can change the hover
    /// text. A returned node replaces the range node of the hover.
    fn after_quick_info(
        &self,
        program: &NewProgram,
        c: &mut Checker,
        file: Node,
        node: Node,
        symbol: SymbolId,
        quick_info: &mut String,
        documentation: &mut String,
        is_markdown: bool,
    ) -> Option<Node> {
        None
    }

    /// Effect patch 013: the extension options from `compilerOptions.plugins`
    /// (Go `etscore.ParseFromPlugins`). `None` when no plugin of the
    /// extension is listed.
    fn parse_plugins(&self, value: &CompilerOptionsValue) -> Option<ExtOptions> {
        None
    }

    /// Effect patches 013 and 025 (`MergeCompilerOptionsCallback`): merges the
    /// extension options of `source` into `target` after the plain merge.
    /// `source_config_path` is the config that `source` came from and
    /// `base_path` the directory of the config being parsed. Runs only when
    /// `source.ext` is `Some`.
    fn merge_options(
        &self,
        target: &mut CompilerOptions,
        source: &CompilerOptions,
        raw_source: Option<&IndexMap<String, CompilerOptionsValue>>,
        source_config_path: &str,
        base_path: &str,
    ) {
    }

    /// Effect patch 015 (`AfterInlayHintsCallback`): can change the hints
    /// of `span` of `file`.
    fn after_inlay_hints(
        &self,
        program: &NewProgram,
        c: &mut Checker,
        file: Node,
        span: TextRange,
        preferences: &InlayHintsPreferences,
        hints: &mut Vec<lsproto::InlayHint>,
        converters: &Converters,
    ) {
    }

    /// Effect patch 017: the server lists `refactor.rewrite` in its code
    /// action kinds.
    fn advertises_refactor_rewrite(&self) -> bool {
        false
    }

    /// Effect patches 018 and 027 (`RegisterAutoImportFixTransformer`): the
    /// fix transformer of an auto-import view, or `None` for none.
    fn auto_import_fix_transformer(
        &self,
        preferences: &UserPreferences,
        program: &Rc<NewProgram>,
        importing_file: Node,
    ) -> Option<FixTransformer> {
        None
    }

    /// Effect patch 022 (`AfterCompletionCallback`): can add or change the
    /// completion items at `position` of `file`.
    fn after_completion(
        &self,
        ctx: &Context,
        file: Node,
        position: i32,
        items: &mut Vec<lsproto::CompletionItem>,
        program: &NewProgram,
        ls: &LanguageService,
    ) {
    }

    /// Effect patch 024 (`AfterDocumentSymbolsCallback`): can change the
    /// symbol tree of `file` after the expando merge.
    fn after_document_symbols(
        &self,
        ctx: &Context,
        file: Node,
        symbols: &mut Vec<lsproto::DocumentSymbol>,
        program: &NewProgram,
        ls: &LanguageService,
    ) {
    }

    /// Effect patch 028: the JSON text of the `effect` key of the build
    /// info, or `None` to leave it out (Go `omitzero`).
    fn buildinfo_options(&self, options: &ExtOptions) -> Option<String> {
        None
    }

    /// Effect patch 028: the extension options from the JSON text of the
    /// `effect` key of a build info.
    fn options_from_buildinfo(&self, json: &str) -> Option<ExtOptions> {
        None
    }

    /// Effect patch 030 (`ValidateCompilerOptionsCallback`): diagnostics for
    /// the final options of a config, after `extends` and the command line
    /// options are merged. `config_file` is the tsconfig source file, or nil.
    fn validate_options(&self, options: &CompilerOptions, config_file: Node) -> Vec<Diagnostic> {
        Vec::new()
    }
}

/// Go `autoimport.FixTransformer` (Effect patch 018): rewrites the fixes of
/// one export. An empty result drops them.
pub type FixTransformer = Box<dyn Fn(&Export, Vec<Rc<Fix>>) -> Vec<Rc<Fix>>>;

static EXTENSION: OnceLock<&'static dyn Extension> = OnceLock::new();

/// Installs the extension of the process. Only the first call counts (Go
/// registers each callback once, from `init()`).
pub fn install(extension: &'static dyn Extension) {
    let _ = EXTENSION.set(extension);
}

/// The installed extension, if any.
#[inline]
pub fn get() -> Option<&'static dyn Extension> {
    EXTENSION.get().copied()
}

/// Options that an extension parsed from `compilerOptions.plugins` (Go
/// `CompilerOptions.Effect`, Effect patch 007). The core only stores and
/// compares them. Equality is the value's own `==`, as Go
/// `reflect.DeepEqual` on the pointer.
#[derive(Clone, Debug)]
pub struct ExtOptions(Arc<dyn ExtOptionsValue>);

/// The value inside `ExtOptions`: any `Debug + PartialEq` type.
pub trait ExtOptionsValue: Any + Debug + Send + Sync {
    fn eq_value(&self, other: &dyn Any) -> bool;
}

impl<T: Any + Debug + PartialEq + Send + Sync> ExtOptionsValue for T {
    fn eq_value(&self, other: &dyn Any) -> bool {
        other.downcast_ref::<T>() == Some(self)
    }
}

impl ExtOptions {
    pub fn new<T: ExtOptionsValue>(value: T) -> ExtOptions {
        ExtOptions(Arc::new(value))
    }

    /// The value, when it is a `T`.
    pub fn get<T: Any>(&self) -> Option<&T> {
        let value: &dyn Any = &*self.0;
        value.downcast_ref()
    }
}

impl PartialEq for ExtOptions {
    fn eq(&self, other: &ExtOptions) -> bool {
        let other: &dyn Any = &*other.0;
        self.0.eq_value(other)
    }
}

impl Eq for ExtOptions {}

/// The front end that the process runs. Go keeps only the command line
/// flag (Effect patch 009, `etscore.EnterCommandLineMode`). The Effect
/// default (editor only: the LSP and the API language service, not tsc and
/// not the API answers) also needs the other two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// No front end entered (library use, tests).
    None = 0,
    /// `execute::command_line` (tsc and `tsc -b`).
    CommandLine = 1,
    /// `--lsp`.
    Lsp = 2,
    /// `--api`.
    Api = 3,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::None as u8);

/// Enters `mode` until the guard drops, then restores the mode before (Go
/// `restore := etscore.EnterCommandLineMode(); defer restore()`).
#[must_use]
pub fn enter_mode(mode: Mode) -> ModeGuard {
    ModeGuard(MODE.swap(mode as u8, Ordering::AcqRel))
}

/// The current mode (Go `etscore.IsCommandLineMode` is
/// `mode() == Mode::CommandLine`).
pub fn mode() -> Mode {
    match MODE.load(Ordering::Acquire) {
        1 => Mode::CommandLine,
        2 => Mode::Lsp,
        3 => Mode::Api,
        _ => Mode::None,
    }
}

/// Restores the mode before `enter_mode` when it drops.
pub struct ModeGuard(u8);

impl Drop for ModeGuard {
    fn drop(&mut self) {
        MODE.store(self.0, Ordering::Release);
    }
}
