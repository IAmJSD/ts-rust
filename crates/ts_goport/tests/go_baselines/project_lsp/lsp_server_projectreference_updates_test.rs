//! Port of Go `internal/lsp/server_projectreference_updates_test.go`.

use ts_goport::ls::lsconv;
use ts_goport::lsp::lsproto;

use super::lsp_server_completion_test::init_completion_client;

child_test! {
    // Go: server_projectreference_updates_test.go:67 TestReferencesAfterAncestorProjectConfigDeletion1
    fn references_after_ancestor_project_config_deletion1() {
        // Go: initMutableLSPClient (server_projectreference_updates_test.go:19) is
        // initCompletionClient with Cwd "/root" and the map FS kept for edits.
        let client = init_completion_client(
            "/root",
            &[
                (
                    "/root/tsconfig.json",
                    r#"{
			"files": [],
			"references": [{ "path": "./project" }]
		}"#,
                ),
                (
                    "/root/project/tsconfig.json",
                    r#"{
			"compilerOptions": { "composite": true },
			"include": ["src/**/*.ts"]
		}"#,
                ),
                ("/root/project/src/main.ts", "export function helloWorld() {}\nhelloWorld()\n"),
            ],
        );
        let fs = super::projecttestutil::current_map_fs_for_test();

        let main_uri = lsconv::file_name_to_document_uri("/root/project/src/main.ts");
        client.send_notification(
            &lsproto::TEXT_DOCUMENT_DID_OPEN_INFO,
            lsproto::DidOpenTextDocumentParams {
                text_document: Some(lsproto::TextDocumentItem {
                    uri: main_uri.clone(),
                    language_id: lsproto::LanguageKind::TYPE_SCRIPT,
                    text: "export function helloWorld() {}\nhelloWorld()\n".to_string(),
                    ..Default::default()
                }),
            },
        );

        // Prime the child project so opening a file creates the ancestor configured-project placeholder.
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_DOCUMENT_SYMBOL_INFO,
            lsproto::DocumentSymbolParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);

        fs.remove("root/tsconfig.json").unwrap();
        client.send_notification(
            &lsproto::WORKSPACE_DID_CHANGE_WATCHED_FILES_INFO,
            lsproto::DidChangeWatchedFilesParams {
                changes: vec![Some(lsproto::FileEvent {
                    uri: lsconv::file_name_to_document_uri("/root/tsconfig.json"),
                    type_: lsproto::FileChangeType::DELETED,
                })],
            },
        );

        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_REFERENCES_INFO,
            lsproto::ReferenceParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                position: lsproto::Position { line: 1, character: 3 },
                context: Some(lsproto::ReferenceContext {
                    include_declaration: true,
                }),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let locations = resp.expect("expected response").locations.expect("resp.Locations");
        assert_eq!(locations.len(), 2);
        let location = |sl, sc, el, ec| lsproto::Location {
            uri: main_uri.clone(),
            range: lsproto::Range {
                start: lsproto::Position { line: sl, character: sc },
                end: lsproto::Position { line: el, character: ec },
            },
        };
        assert_eq!(locations, vec![location(0, 16, 0, 26), location(1, 0, 1, 10)]);
    }
}

child_test! {
    // PORT: not in Go. A solution tsconfig (`files: []` and references) is a
    // configured project without a program. Rename and references in a
    // project outside the solution walk the loaded project trees; Go skips a
    // project without a program there (ls/crossproject.go:252). The port read
    // that project's missing host and panicked (editfuzz5 X1).
    fn rename_and_references_skip_project_without_program() {
        const MAIN: &str = "export function bFn(n: number) {\n  return n;\n}\nexport const r = bFn(1);\n";
        let client = init_completion_client(
            "/root",
            &[
                ("/root/tsconfig.json", r#"{"files": [], "references": [{"path": "./a"}]}"#),
                (
                    "/root/a/tsconfig.json",
                    r#"{"compilerOptions": {"composite": true, "strict": true}, "include": ["src"]}"#,
                ),
                ("/root/a/src/lib.ts", "export const aValue = 1;\n"),
                (
                    "/root/b/tsconfig.json",
                    r#"{"compilerOptions": {"strict": true, "noEmit": true}, "include": ["src"]}"#,
                ),
                ("/root/b/src/main.ts", MAIN),
            ],
        );
        let lib_uri = lsconv::file_name_to_document_uri("/root/a/src/lib.ts");
        let main_uri = lsconv::file_name_to_document_uri("/root/b/src/main.ts");
        for (uri, text) in [(&lib_uri, "export const aValue = 1;\n"), (&main_uri, MAIN)] {
            client.send_notification(
                &lsproto::TEXT_DOCUMENT_DID_OPEN_INFO,
                lsproto::DidOpenTextDocumentParams {
                    text_document: Some(lsproto::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: lsproto::LanguageKind::TYPE_SCRIPT,
                        text: text.to_string(),
                        ..Default::default()
                    }),
                },
            );
        }
        let range = |sl, sc, el, ec| lsproto::Range {
            start: lsproto::Position { line: sl, character: sc },
            end: lsproto::Position { line: el, character: ec },
        };
        let bfn_ranges = vec![range(0, 16, 0, 19), range(3, 17, 3, 20)];

        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_RENAME_INFO,
            lsproto::RenameParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                position: lsproto::Position { line: 0, character: 17 },
                new_name: "x2".to_string(),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let changes = resp
            .expect("expected response")
            .workspace_edit
            .expect("resp.WorkspaceEdit")
            .changes
            .expect("WorkspaceEdit.Changes");
        assert_eq!(changes.keys().collect::<Vec<_>>(), vec![&main_uri]);
        let edit_ranges: Vec<lsproto::Range> =
            changes[&main_uri].iter().flatten().map(|edit| edit.range).collect();
        assert_eq!(edit_ranges, bfn_ranges);

        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_REFERENCES_INFO,
            lsproto::ReferenceParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                position: lsproto::Position { line: 0, character: 17 },
                context: Some(lsproto::ReferenceContext {
                    include_declaration: true,
                }),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let locations = resp.expect("expected response").locations.expect("resp.Locations");
        let reference_ranges: Vec<lsproto::Range> =
            locations.iter().map(|location| location.range).collect();
        assert_eq!(reference_ranges, bfn_ranges);
        assert!(locations.iter().all(|location| location.uri == main_uri));
    }
}

child_test! {
    // PORT: not in Go. Go searches each project of a references request with
    // a query checker of that project's pool (ls/crossproject.go:46
    // handleCrossProject, project/checkerpool.go:252 getQueryChecker), so
    // later requests in that project see the state the search left. Here
    // the search of `description` in the lib project instantiates the
    // members of `EnumOptions<T>` before semanticTokens resolves the
    // declared type, so the hover writes the mapped type argument `<T>`
    // (checker.go:21090 instantiateSymbol, nodebuilderimpl.go:1036
    // lookupTypeParameterNodes). On a fresh checker the form is
    // `EnumOptions<T extends object = any>`. The port ran the search on a
    // search thread with its own checker and showed the fresh form
    // (lspsweep2 G2, lschk1). The hover text is Go N's
    // (tsgo-oracle-673a5f17d713, lschk1 repro v9).
    fn hover_after_cross_project_references_uses_the_searched_checker() {
        const REG: &str = "import type { ArgsOptions } from '../types';\n\
            export type Both = ArgsOptions;\n\
            export interface EnumOptions<T extends object = any> {\n  name: string;\n  description?: string;\n}\n\
            export function registerEnumType<T extends object = any>(enumRef: T, options?: EnumOptions<T>) {\n  \
            if (!options || typeof options.name !== 'string') { throw new Error(''); }\n  \
            return { ref: enumRef, description: options.description };\n}\n";
        const TYPES: &str =
            "export type ArgsOptions<T = any> = {\n  name?: string;\n  description?: string;\n};\n";
        const CONFIG: &str = r#"{"compilerOptions": {"strict": true, "target": "es2020"}, "include": ["#;
        let client = init_completion_client(
            "/home/projects",
            &[
                ("/home/projects/tsconfig.json", format!(r#"{CONFIG}"types.ts"]}}"#).as_str()),
                ("/home/projects/types.ts", TYPES),
                ("/home/projects/lib/tsconfig.json", format!(r#"{CONFIG}"*.ts"]}}"#).as_str()),
                ("/home/projects/lib/reg.ts", REG),
            ],
        );
        let reg_uri = lsconv::file_name_to_document_uri("/home/projects/lib/reg.ts");
        let types_uri = lsconv::file_name_to_document_uri("/home/projects/types.ts");
        for (uri, text) in [(&reg_uri, REG), (&types_uri, TYPES)] {
            client.send_notification(
                &lsproto::TEXT_DOCUMENT_DID_OPEN_INFO,
                lsproto::DidOpenTextDocumentParams {
                    text_document: Some(lsproto::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: lsproto::LanguageKind::TYPE_SCRIPT,
                        text: text.to_string(),
                        ..Default::default()
                    }),
                },
            );
        }

        // References of `description` in types.ts: the lib project includes
        // types.ts, so the search runs in both projects.
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_REFERENCES_INFO,
            lsproto::ReferenceParams {
                text_document: lsproto::TextDocumentIdentifier { uri: types_uri },
                position: lsproto::Position { line: 2, character: 3 },
                context: Some(lsproto::ReferenceContext {
                    include_declaration: true,
                }),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);

        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_SEMANTIC_TOKENS_FULL_INFO,
            lsproto::SemanticTokensParams {
                text_document: lsproto::TextDocumentIdentifier { uri: reg_uri.clone() },
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);

        // Hover on `description` of `options.description` in lib/reg.ts.
        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_HOVER_INFO,
            lsproto::HoverParams {
                text_document: lsproto::TextDocumentIdentifier { uri: reg_uri },
                position: lsproto::Position { line: 8, character: 50 },
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let text = resp
            .and_then(|resp| resp.hover)
            .and_then(|hover| hover.contents.markup_content)
            .expect("hover MarkupContent")
            .value;
        assert!(
            text.contains("(property) EnumOptions<T>.description?: string | undefined"),
            "{text}"
        );
    }
}
