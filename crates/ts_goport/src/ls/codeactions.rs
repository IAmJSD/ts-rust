use crate::ls::prelude::*;

use crate::spanmap::Feature;

// Port of Go `ls/codeactions.go`.
//
// PORT (whole file):
// - Go `*CodeFixProvider` values are package vars; here they are
//   `pub static X: LazyLock<CodeFixProvider>` and a provider pointer is
//   `&'static CodeFixProvider` (pointer identity is `std::ptr::eq`).
// - Go `*CodeFixContext` borrows the language service, the diagnostic and
//   the params for one call, so it is `CodeFixContext<'a>` with references.
// - Go `[]*CodeAction` holds pointers that are never changed after they are
//   made; here `Vec<CodeAction>` values (cloned where Go shares a pointer).
// - Go `map[...]` values that reach output use `IndexMap` in insertion
//   order. Go map order is random; the oracle compares those outputs
//   without order.

// Go: ls/codeactions.go:21 CodeFixProvider
// CodeFixProvider represents a provider for a specific type of code fix
// PORT: Go func fields are plain `fn` pointers, so the statics are `Sync`.
// A nil Go `GetAllCodeActions` is `None`.
pub struct CodeFixProvider {
    pub error_codes: Vec<i32>,
    pub get_code_actions:
        fn(ctx: &Context, fix_context: &CodeFixContext<'_>) -> Result<Vec<CodeAction>, GoError>,
    pub fix_ids: Vec<String>,
    pub get_all_code_actions: Option<
        fn(
            ctx: &Context,
            fix_context: &CodeFixContext<'_>,
        ) -> Result<Option<CombinedCodeActions>, GoError>,
    >,
}

// Go: ls/codeactions.go:29 CodeFixContext
// CodeFixContext contains the context needed to generate code fixes
// PORT: Go nil `Diagnostic` and `Params` are `None`. Go leaves `Span` and
// `ErrorCode` at their zero values in the fix-all contexts; Rust callers
// write `TextRange::default()` and `0`.
pub struct CodeFixContext<'a> {
    pub source_file: Node,
    pub span: TextRange,
    pub error_code: i32,
    pub program: &'a compiler::NewProgram,
    pub ls: &'a LanguageService,
    pub diagnostic: Option<&'a lsproto::Diagnostic>,
    pub params: Option<&'a lsproto::CodeActionParams>,
}

// Go: ls/codeactions.go:40 CodeAction
// CodeAction represents a single code action fix
// PORT: `kind` is Effect patch 011 (Go `Kind lsproto.CodeActionKind`,
// optional; used by refactors to specify the code action kind). Go "" is
// `None`.
#[derive(Clone, Debug, Default)]
pub struct CodeAction {
    pub description: String,
    pub changes: Vec<lsproto::TextEdit>,
    pub fix_id: String,
    pub kind: Option<lsproto::CodeActionKind>,
    pub fix_all_description: String,
}

impl CodeAction {
    // Go: ls/codeactions.go:48 (*CodeAction).Compare
    // Compare defines a total ordering for CodeAction values, comparing description
    // then text edits lexicographically. Used with slices.BinarySearchFunc.
    // PORT: Go `strings.Compare` is a byte-wise comparison, like `str::cmp`.
    pub fn compare(&self, b: &CodeAction) -> i32 {
        let a = self;
        let c = a.description.as_str().cmp(b.description.as_str()) as i32;
        if c != 0 {
            return c;
        }
        let c = a.changes.len().cmp(&b.changes.len()) as i32;
        if c != 0 {
            return c;
        }
        for (i, edit) in a.changes.iter().enumerate() {
            let c = edit.compare(&b.changes[i]);
            if c != 0 {
                return c;
            }
        }
        0
    }
}

// Go: ls/codeactions.go:65 CombinedCodeActions
// CombinedCodeActions represents combined code actions for fix-all scenarios
#[derive(Clone, Debug, Default)]
pub struct CombinedCodeActions {
    pub description: String,
    pub changes: Vec<lsproto::TextEdit>,
}

// Go: ls/codeactions.go:71 codeFixProviders
// codeFixProviders is the list of all registered code fix providers
// PORT: a Go package var; the list is rebuilt from the provider statics on
// each call, in Go order.
fn code_fix_providers() -> [&'static CodeFixProvider; 3] {
    [
        &*IMPORT_FIX_PROVIDER,
        &*ISOLATED_DECLARATIONS_FIX_PROVIDER,
        &*FIX_CLASS_INCORRECTLY_IMPLEMENTS_INTERFACE_PROVIDER,
        // Add more code fix providers here as they are implemented
    ]
}

impl LanguageService {
    // Go: ls/codeactions.go:79 ProvideCodeActions
    // ProvideCodeActions returns code actions for the given range and context
    pub fn provide_code_actions(
        &self,
        ctx: &Context,
        params: &lsproto::CodeActionParams,
    ) -> Result<lsproto::CodeActionResponse, GoError> {
        let (program, file) = self.get_program_and_file(&params.text_document.uri);

        let mut actions: Vec<lsproto::CommandOrCodeAction> = Vec::new();

        if let Some(context) = &params.context
            && let Some(only) = &context.only
        {
            for kind in only {
                let matching_kinds = get_organize_imports_actions_for_kind(kind);
                for matching_kind in matching_kinds {
                    let organize_action =
                        self.create_organize_imports_action(ctx, program, file, matching_kind);
                    actions.push(organize_action);
                }

                if is_fix_all_kind(kind) {
                    let fix_all_action =
                        self.create_fix_all_action(ctx, program, file, &params.text_document.uri)?;
                    if let Some(fix_all_action) = fix_all_action {
                        actions.push(fix_all_action);
                    }
                }
            }
        }

        // PORT: Go also tests `params.Context.Diagnostics != nil`. The Rust
        // `Vec` has no nil state; an empty list adds no action and no fix-all
        // entry, the same result as Go's nil.
        if let Some(context) = &params.context
            && wants_quick_fixes(context.only.as_deref())
        {
            // PORT: Go map; `IndexMap` keeps insertion order (Go order is random).
            let mut fix_id_seen: IndexMap<String, &'static CodeFixProvider> = IndexMap::new();

            let mut seen: Vec<CodeAction> = Vec::new(); // sorted for binary search dedup, dedup across all diagnostics and providers so if multiple diags produce the same codefix, only one is returned

            // PORT: the extension's providers only for a program with
            // extension options (see `crate::ext`).
            let ext_providers: &[&'static CodeFixProvider] = match crate::ext::get() {
                Some(ext) if program.options().ext.is_some() => ext.code_fix_providers(),
                _ => &[],
            };

            for diag in &context.diagnostics {
                // Go: diag.Code (a nil element panics)
                let diag = diag
                    .as_ref()
                    .unwrap_or_else(|| crate::core::go_nil_dereference());
                let Some(code) = &diag.code else {
                    continue;
                };
                let Some(error_code) = code.integer else {
                    continue;
                };

                // Effect patch 011: check all code fix providers (built-in +
                // the extension's).
                for provider in code_fix_providers()
                    .into_iter()
                    .chain(ext_providers.iter().copied())
                {
                    if !code_fix_provider_matches_lsp_diagnostic(provider, diag) {
                        continue;
                    }

                    for mapped in lsconv::from_lsp_range_for_source_file(
                        &self.converters,
                        file,
                        diag.range,
                        Feature::CODE_ACTIONS,
                    ) {
                        let fix_context = CodeFixContext {
                            source_file: mapped.script,
                            span: mapped.span,
                            error_code,
                            program,
                            ls: self,
                            diagnostic: Some(diag),
                            params: Some(params),
                        };

                        let provider_actions = (provider.get_code_actions)(ctx, &fix_context)?;
                        for action in provider_actions {
                            let (i, found) = crate::gostd::slices::binary_search_func(
                                &seen,
                                &action,
                                |a: &CodeAction, b: &&CodeAction| a.compare(b),
                            );
                            if found {
                                continue;
                            }
                            seen.insert(i, action.clone());
                            actions.push(convert_to_lsp_code_action(
                                &action,
                                diag,
                                &params.text_document.uri,
                            ));
                            if !action.fix_id.is_empty() {
                                fix_id_seen.insert(action.fix_id.clone(), provider);
                            }
                        }
                    }
                }
            }

            let fix_all_actions = self.get_fix_all_quick_fixes(
                ctx,
                program,
                file,
                &params.text_document.uri,
                &fix_id_seen,
            )?;
            actions.extend(fix_all_actions);
        }

        // Effect patch 011: run refactor providers when the request includes
        // refactor.* kinds or no filter.
        // PORT: the extension is the one provider, called only for a program
        // with extension options (see `crate::ext`).
        if let Some(ext) = crate::ext::get()
            && program.options().ext.is_some()
            && should_run_refactors(params)
        {
            for mapped in lsconv::from_lsp_range_for_source_file(
                &self.converters,
                file,
                params.range,
                Feature::CODE_ACTIONS,
            ) {
                let refactor_actions =
                    ext.refactor_actions(ctx, mapped.script, mapped.span, program, self)?;
                for action in &refactor_actions {
                    actions.push(convert_refactor_to_lsp_code_action(
                        action,
                        &params.text_document.uri,
                    ));
                }
            }
        }

        Ok(lsproto::CommandOrCodeActionArrayOrNull {
            command_or_code_action_array: Some(actions),
        })
    }

    // Go: ls/codeactions.go:163 getFixAllQuickFixes
    // getFixAllQuickFixes returns per-provider "Fix all in file" quickfix entries for providers
    // that matched at least 2 diagnostics in the full file.
    // PORT: Go ranges over the `fixIdSeen` map in random order; this uses
    // its insertion order.
    fn get_fix_all_quick_fixes(
        &self,
        ctx: &Context,
        program: &compiler::NewProgram,
        file: Node,
        uri: &lsproto::DocumentUri,
        fix_id_seen: &IndexMap<String, &'static CodeFixProvider>,
    ) -> Result<Vec<lsproto::CommandOrCodeAction>, GoError> {
        let mut actions: Vec<lsproto::CommandOrCodeAction> = Vec::new();

        // Deduplicate providers; multiple fixIds may map to the same provider.
        // PORT: Go `collections.Set[*CodeFixProvider]` keys by pointer.
        let mut seen: FxHashSet<*const CodeFixProvider> = FxHashSet::default();
        for (_, &provider) in fix_id_seen {
            let key = provider as *const CodeFixProvider;
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);

            let Some(get_all_code_actions) = provider.get_all_code_actions else {
                continue;
            };

            if !has_multiple_fixable_diagnostics(ctx, program, file, &provider.error_codes) {
                continue;
            }

            let fix_context = CodeFixContext {
                source_file: file,
                span: TextRange::default(),
                error_code: 0,
                program,
                ls: self,
                diagnostic: None,
                params: None,
            };
            let combined = get_all_code_actions(ctx, &fix_context)?;
            if let Some(combined) = combined
                && !combined.changes.is_empty()
            {
                let kind = lsproto::CodeActionKind::QUICK_FIX;
                let mut changes: IndexMap<lsproto::DocumentUri, Vec<Option<lsproto::TextEdit>>> =
                    IndexMap::new();
                changes.insert(
                    uri.clone(),
                    combined.changes.into_iter().map(Some).collect(),
                );
                actions.push(lsproto::CommandOrCodeAction {
                    code_action: Some(lsproto::CodeAction {
                        title: combined.description,
                        kind: Some(kind),
                        edit: Some(lsproto::WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }

        Ok(actions)
    }
}

// Go: ls/codeactions.go:218 hasMultipleFixableDiagnostics
// hasMultipleFixableDiagnostics returns true if the file has at least 2 diagnostics
// matching the given error codes. Checks all diagnostic sources (semantic,
// syntactic, suggestion, declaration) to match ProvideDiagnostics.
fn has_multiple_fixable_diagnostics(
    ctx: &Context,
    program: &compiler::NewProgram,
    file: Node,
    error_codes: &[i32],
) -> bool {
    let all_diags = get_all_diagnostics(ctx, program, file);
    let mut count = 0;
    for d in &all_diags {
        if is_fixable_diagnostic(d, error_codes) {
            count += 1;
            if count >= 2 {
                return true;
            }
        }
    }
    false
}

// Go: ls/codeactions.go:232 codeFixProviderMatchesLSPDiagnostic
fn code_fix_provider_matches_lsp_diagnostic(
    provider: &CodeFixProvider,
    diagnostic: &lsproto::Diagnostic,
) -> bool {
    if diagnostic
        .source
        .as_deref()
        .is_some_and(|source| source != "ts")
    {
        return false;
    }
    diagnostic
        .code
        .as_ref()
        .and_then(|code| code.integer)
        .is_some_and(|code| contains_error_code(&provider.error_codes, code))
}

// Go: ls/codeactions.go:239 isFixableDiagnostic
pub fn is_fixable_diagnostic(diagnostic: &Diagnostic, error_codes: &[i32]) -> bool {
    diagnostic.source().is_empty() && contains_error_code(error_codes, diagnostic.code())
}

// Go: ls/codeactions.go:244 isFixAllKind
// isFixAllKind returns true if the requested kind matches source.fixAll
fn is_fix_all_kind(kind: &lsproto::CodeActionKind) -> bool {
    kind.contains(&lsproto::CodeActionKind::SOURCE_FIX_ALL_TS)
}

// Go: ls/codeactions.go:250 wantsQuickFixes
// wantsQuickFixes returns true if the Only filter is nil/empty (meaning all kinds are wanted)
// or explicitly includes the quickfix kind.
// PORT: Go `*[]lsproto.CodeActionKind`; nil is `None`.
fn wants_quick_fixes(only: Option<&[lsproto::CodeActionKind]>) -> bool {
    let Some(only) = only else {
        return true;
    };
    if only.is_empty() {
        return true;
    }
    for kind in only {
        if kind.contains(&lsproto::CodeActionKind::QUICK_FIX) {
            return true;
        }
    }
    false
}

impl LanguageService {
    // Go: ls/codeactions.go:264 createFixAllAction
    // createFixAllAction creates a source.fixAll code action that applies all auto-fixable
    // code fixes across the file.
    // PORT: Go returns a nil `*lsproto.CommandOrCodeAction` as `None`.
    fn create_fix_all_action(
        &self,
        ctx: &Context,
        program: &compiler::NewProgram,
        file: Node,
        uri: &lsproto::DocumentUri,
    ) -> Result<Option<lsproto::CommandOrCodeAction>, GoError> {
        let kind = lsproto::CodeActionKind::SOURCE_FIX_ALL_TS;
        let mut lsp_changes: IndexMap<lsproto::DocumentUri, Vec<Option<lsproto::TextEdit>>> =
            IndexMap::new();

        for provider in code_fix_providers() {
            let Some(get_all_code_actions) = provider.get_all_code_actions else {
                continue;
            };

            let fix_context = CodeFixContext {
                source_file: file,
                span: TextRange::default(),
                error_code: 0,
                program,
                ls: self,
                diagnostic: None,
                params: None,
            };

            let combined = get_all_code_actions(ctx, &fix_context)?;
            if let Some(combined) = combined
                && !combined.changes.is_empty()
            {
                lsp_changes
                    .entry(uri.clone())
                    .or_default()
                    .extend(combined.changes.into_iter().map(Some));
            }
        }

        if lsp_changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(lsproto::CommandOrCodeAction {
            code_action: Some(lsproto::CodeAction {
                title: crate::diagnostics_loc::message_localize(
                    diag::Fix_All,
                    &locale::from_context(ctx),
                    &args![],
                ),
                kind: Some(kind),
                edit: Some(lsproto::WorkspaceEdit {
                    changes: Some(lsp_changes),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

// Go: ls/codeactions.go:307 getOrganizeImportsActionTitle
// getOrganizeImportsActionTitle returns the appropriate title for the given organize imports kind
fn get_organize_imports_action_title(ctx: &Context, kind: &lsproto::CodeActionKind) -> String {
    let loc = locale::from_context(ctx);
    if *kind == lsproto::CodeActionKind::SOURCE_REMOVE_UNUSED_IMPORTS_TS {
        crate::diagnostics_loc::message_localize(diag::Remove_Unused_Imports, &loc, &args![])
    } else if *kind == lsproto::CodeActionKind::SOURCE_SORT_IMPORTS_TS {
        crate::diagnostics_loc::message_localize(diag::Sort_Imports, &loc, &args![])
    } else {
        crate::diagnostics_loc::message_localize(diag::Organize_Imports, &loc, &args![])
    }
}

// Go: ls/codeactions.go:321 getOrganizeImportsActionsForKind
// getOrganizeImportsActionsForKind returns the organize imports code action kinds that should be
// returned for the given requested kind.
fn get_organize_imports_actions_for_kind(
    requested_kind: &lsproto::CodeActionKind,
) -> Vec<lsproto::CodeActionKind> {
    let organize_imports_kinds = [
        lsproto::CodeActionKind::SOURCE_ORGANIZE_IMPORTS_TS,
        lsproto::CodeActionKind::SOURCE_REMOVE_UNUSED_IMPORTS_TS,
        lsproto::CodeActionKind::SOURCE_SORT_IMPORTS_TS,
    ];

    let mut result: Vec<lsproto::CodeActionKind> = Vec::new();
    for organize_kind in organize_imports_kinds {
        if requested_kind.contains(&organize_kind) {
            result.push(organize_kind);
        }
    }

    if result.contains(requested_kind) {
        return vec![requested_kind.clone()];
    }

    result
}

impl LanguageService {
    // Go: ls/codeactions.go:343 createOrganizeImportsAction
    // createOrganizeImportsAction creates the organize imports code action
    // PORT: Go returns a `*lsproto.CommandOrCodeAction` that is never nil;
    // here the value.
    fn create_organize_imports_action(
        &self,
        ctx: &Context,
        program: &compiler::NewProgram,
        file: Node,
        kind: lsproto::CodeActionKind,
    ) -> lsproto::CommandOrCodeAction {
        let title = get_organize_imports_action_title(ctx, &kind);
        let changes = self.organize_imports(ctx, file, program, &kind);
        if changes.is_empty() {
            return lsproto::CommandOrCodeAction {
                code_action: Some(lsproto::CodeAction {
                    title,
                    kind: Some(kind),
                    edit: Some(lsproto::WorkspaceEdit {
                        changes: Some(IndexMap::new()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
        }

        // PORT: Go ranges over the `changes` map in random order; this uses
        // the tracker's insertion order.
        let mut lsp_changes: IndexMap<lsproto::DocumentUri, Vec<Option<lsproto::TextEdit>>> =
            IndexMap::new();
        for (file_name, edits) in changes {
            let file_uri = lsconv::file_name_to_document_uri(&file_name);
            lsp_changes.insert(file_uri, edits.into_iter().map(Some).collect());
        }

        lsproto::CommandOrCodeAction {
            code_action: Some(lsproto::CodeAction {
                title,
                kind: Some(kind),
                edit: Some(lsproto::WorkspaceEdit {
                    changes: Some(lsp_changes),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

// Go: ls/codeactions.go:382 containsErrorCode
// containsErrorCode checks if the error code is in the list
pub fn contains_error_code(codes: &[i32], code: i32) -> bool {
    codes.contains(&code)
}

// Effect patch 011: shouldRunRefactors returns true when refactor providers
// should be invoked. Refactors run when no filter is specified or when the
// filter includes a refactor.* kind.
fn should_run_refactors(params: &lsproto::CodeActionParams) -> bool {
    let Some(only) = params.context.as_ref().and_then(|c| c.only.as_ref()) else {
        return true;
    };
    only.iter().any(|kind| kind.0.starts_with("refactor"))
}

// Effect patch 011: convertRefactorToLSPCodeAction converts a refactor
// CodeAction to an LSP CommandOrCodeAction.
fn convert_refactor_to_lsp_code_action(
    action: &CodeAction,
    uri: &lsproto::DocumentUri,
) -> lsproto::CommandOrCodeAction {
    let kind = action
        .kind
        .clone()
        .unwrap_or(lsproto::CodeActionKind::REFACTOR_REWRITE);
    let mut changes: IndexMap<lsproto::DocumentUri, Vec<Option<lsproto::TextEdit>>> =
        IndexMap::new();
    changes.insert(
        uri.clone(),
        action.changes.iter().cloned().map(Some).collect(),
    );
    lsproto::CommandOrCodeAction {
        code_action: Some(lsproto::CodeAction {
            title: action.description.clone(),
            kind: Some(kind),
            edit: Some(lsproto::WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// Go: ls/codeactions.go:387 convertToLSPCodeAction
// convertToLSPCodeAction converts an internal CodeAction to an LSP CodeAction
fn convert_to_lsp_code_action(
    action: &CodeAction,
    diag: &lsproto::Diagnostic,
    uri: &lsproto::DocumentUri,
) -> lsproto::CommandOrCodeAction {
    let kind = lsproto::CodeActionKind::QUICK_FIX;
    let mut changes: IndexMap<lsproto::DocumentUri, Vec<Option<lsproto::TextEdit>>> =
        IndexMap::new();
    changes.insert(
        uri.clone(),
        action.changes.iter().cloned().map(Some).collect(),
    );
    let diagnostics: Vec<Option<lsproto::Diagnostic>> = vec![Some(diag.clone())];

    lsproto::CommandOrCodeAction {
        code_action: Some(lsproto::CodeAction {
            title: action.description.clone(),
            kind: Some(kind),
            edit: Some(lsproto::WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }),
            diagnostics: Some(diagnostics),
            ..Default::default()
        }),
        ..Default::default()
    }
}
