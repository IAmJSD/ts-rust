//! Go `internal/project/parsecache.go`.

use crate::project::prelude::*;

use crate::contentmapper;
use crate::frontend::core_ext::ensure_script_kind_from_file_name;
use crate::frontend::parser;
use xxhash_rust::xxh3::xxh3_128;

// Go: project/parsecache.go:10 ParseCacheKey
// PORT: Go embeds `ast.SourceFileParseOptions` (with its
// `ExternalModuleIndicatorOptions`). The parser structs do not derive `Hash`
// and the plan keeps the parser unchanged, so the key copies their fields:
// `file_name`, `path` and the `jsx` and `force` fields of
// `ExternalModuleIndicatorOptions`. `source_file_parse_options` rebuilds the
// Go embedded value. Go `xxh3.Uint128` is `u128`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ParseCacheKey {
    pub file_name: String,
    pub path: tspath::Path,
    pub jsx: bool,
    pub force: bool,
    pub script_kind: ScriptKind,
    pub hash: u128,
}

impl ParseCacheKey {
    /// Go `key.SourceFileParseOptions` (the embedded value).
    pub fn source_file_parse_options(&self) -> parser::SourceFileParseOptions {
        parser::SourceFileParseOptions {
            file_name: self.file_name.clone(),
            path: self.path.clone(),
            external_module_indicator_options: parser::ExternalModuleIndicatorOptions {
                jsx: self.jsx,
                force: self.force,
            },
        }
    }
}

// Go: project/parsecache.go:16 NewParseCacheKey
// PORT: Go passes the options by value; here by reference.
pub fn new_parse_cache_key(
    options: &parser::SourceFileParseOptions,
    hash: u128,
    mut script_kind: ScriptKind,
) -> ParseCacheKey {
    if script_kind == ScriptKind::UNKNOWN {
        script_kind = ensure_script_kind_from_file_name(&options.file_name);
    }
    ParseCacheKey {
        file_name: options.file_name.clone(),
        path: options.path.clone(),
        jsx: options.external_module_indicator_options.jsx,
        force: options.external_module_indicator_options.force,
        hash,
        script_kind,
    }
}

// Go: project/parsecache.go:36 ContentMappedParseCacheKey (tsgo#4712)
// ContentMappedParseCacheKey identifies the complete output bundle for one mapped input. Hash folds the
// original content, mapper transform identity, and diagnostic locale together.
// PORT: the embedded `ast.SourceFileParseOptions` is copied field by field,
// as in `ParseCacheKey`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ContentMappedParseCacheKey {
    pub file_name: String,
    pub path: tspath::Path,
    pub jsx: bool,
    pub force: bool,
    pub hash: u128,
}

impl ContentMappedParseCacheKey {
    /// Go `ContentMappedParseCacheKey{SourceFileParseOptions: options, Hash: hash}`.
    pub fn new(options: &parser::SourceFileParseOptions, hash: u128) -> ContentMappedParseCacheKey {
        ContentMappedParseCacheKey {
            file_name: options.file_name.clone(),
            path: options.path.clone(),
            jsx: options.external_module_indicator_options.jsx,
            force: options.external_module_indicator_options.force,
            hash,
        }
    }

    /// Go `key.SourceFileParseOptions` (the embedded value).
    pub fn source_file_parse_options(&self) -> parser::SourceFileParseOptions {
        parser::SourceFileParseOptions {
            file_name: self.file_name.clone(),
            path: self.path.clone(),
            external_module_indicator_options: parser::ExternalModuleIndicatorOptions {
                jsx: self.jsx,
                force: self.force,
            },
        }
    }
}

// Go: project/parsecache.go:43 contentMappedParseCacheKey (tsgo#4712)
// PORT: Go `xxh3.Uint128` `Hi` is the high 64 bits of the `u128`, `Lo` the
// low 64 bits.
pub fn content_mapped_parse_cache_key(
    options: &parser::SourceFileParseOptions,
    raw_hash: u128,
    transform_identity: u128,
    diagnostic_locale: &locale::Locale,
) -> ContentMappedParseCacheKey {
    let diagnostic_locale = diagnostic_locale.string();
    let mut buf = Vec::with_capacity(32 + diagnostic_locale.len());
    buf.extend_from_slice(&((raw_hash >> 64) as u64).to_le_bytes());
    buf.extend_from_slice(&(raw_hash as u64).to_le_bytes());
    buf.extend_from_slice(&((transform_identity >> 64) as u64).to_le_bytes());
    buf.extend_from_slice(&(transform_identity as u64).to_le_bytes());
    buf.extend_from_slice(diagnostic_locale.as_bytes());
    ContentMappedParseCacheKey::new(options, xxh3_128(&buf))
}

// Go: project/parsecache.go:54 parseCacheKeyForFile (tsgo#4712)
// parseCacheKeyForFile reconstructs the ordinary parse-cache key for a source file held by a program.
pub fn parse_cache_key_for_file(file: &parser::ParsedSourceFile) -> ParseCacheKey {
    new_parse_cache_key(file.parse_options(), file.source_hash(), file.script_kind)
}

// Go: project/parsecache.go:58 contentMappedParseCacheKeyForFile (tsgo#4712)
pub fn content_mapped_parse_cache_key_for_file(
    file: &parser::ParsedSourceFile,
) -> ContentMappedParseCacheKey {
    ContentMappedParseCacheKey::new(file.content_mapper_parse_options(), file.source_hash())
}

// Go: project/parsecache.go:63 parseCacheKeyForDuplicate (tsgo#4712)
// parseCacheKeyForDuplicate reconstructs an ordinary parse-cache key for a deduplicated source file.
pub fn parse_cache_key_for_duplicate(file: &compiler::DuplicateSourceFile) -> ParseCacheKey {
    new_parse_cache_key(&file.parse_options, file.source_hash(), file.script_kind)
}

// Go: project/parsecache.go:67 contentMappedParseCacheKeyForDuplicate (tsgo#4712)
pub fn content_mapped_parse_cache_key_for_duplicate(
    file: &compiler::DuplicateSourceFile,
) -> ContentMappedParseCacheKey {
    ContentMappedParseCacheKey::new(&file.content_mapper_parse_options, file.source_hash())
}

/// The value the parse cache holds: Go `*ast.SourceFile` after
/// `file.Hash = fh.Hash()`. The file has the hash too
/// (`ParsedSourceFile::hash`).
#[derive(Clone, Debug)]
pub struct HashedSourceFile {
    pub file: Rc<parser::ParsedSourceFile>,
    pub hash: u128,
}

// Go: project/parsecache.go:28 ParseCache
pub type ParseCache = RefCountCache<ParseCacheKey, HashedSourceFile, Rc<dyn FileHandle>>;

// Go: project/parsecache.go:30 NewParseCache
pub fn new_parse_cache(options: RefCountCacheOptions) -> Rc<ParseCache> {
    new_ref_count_cache(
        options,
        |key: &ParseCacheKey, fh: Rc<dyn FileHandle>| -> HashedSourceFile {
            // Program versions share the parse, so its nodes belong to the thread.
            let _base = crate::ast::enter_base_synthetic_owner();
            // Not in Go: a new version of a published path can be freed
            // (lsshells M3a). Its parse keeps its nodes in its store, not in
            // the leaked AST arena, so they are freed with it (M3c). A
            // prefetched parse keeps its leaked nodes.
            let freeable = crate::ast::freeable_path(&key.path.0);
            let _owned_nodes = freeable.then(crate::ast::enter_freeable_parse);
            let opts = key.source_file_parse_options();
            let content = fh.content();
            // PORT: during a program load a parse worker (`FilesParser`
            // prefetch) may have read and parsed this text already, as in
            // compiler/host.rs. A worker result is used only for the same
            // text, and its parse only when it equals the parse below.
            let prefetched =
                compiler::take_prefetched(&opts, key.script_kind, Some(content.as_str()));
            let file = match prefetched {
                compiler::Prefetched::Parse(file) => file,
                // The worker text has the same bytes and is already leaked.
                compiler::Prefetched::Text(text) => {
                    parser::parse_source_file(&opts, text, key.script_kind)
                }
                compiler::Prefetched::Nothing => {
                    // The text of a freeable version is shared with its
                    // store and goes with the version; another text is
                    // leaked, as in compiler/host.rs (`FileText::new`).
                    let text = FileText::new(content, freeable);
                    parser::parse_source_file(&opts, text, key.script_kind)
                }
            };
            // Not in Go: a new version of a published path can be freed. The
            // holders of the parse (programs, cache entries) keep it alive
            // (lsshells M3a, `ast/file_version.rs`).
            if freeable {
                assert!(
                    file.version
                        .set(crate::ast::FileVersion::new(file.store))
                        .is_ok()
                );
            }
            // Go: file.Hash = fh.Hash()
            let hash = fh.hash();
            file.hash.set(Some(hash));
            let file = Rc::new(file);
            // PORT: the next program version publishes the file's store. A
            // version that does not include the file (a package duplicate, an
            // auto-import entrypoint) must still publish its parser fields,
            // so a later version can share the file.
            crate::program::note_parsed_source_file(&file);
            // Go: binder.BindSourceFile(file) (ts#63952). PORT: not here, see
            // `acquire_bound`. The binder lineage gets each file when a
            // program binds its files in file order (`program::bind_all`),
            // or when the auto-import alias resolver reads it. A bind here
            // would bind in parse order and change the lineage ids. There is
            // one dispatch thread, so the Go race (two programs binding one
            // shared file at once) does not exist here.
            HashedSourceFile { file, hash }
        },
    )
}

/// Go `ParseCache.Acquire` with the bind of Go `NewParseCache`
/// (parsecache.go:80, ts#63952): the file is published with no program and
/// bound into the binder lineage before it is returned, on a new entry and
/// on a reused one. `current_directory` is the caller host's. Use it for a
/// file that a caller reads outside a program load (Go
/// `SnapshotHost.AcquireSourceFile`).
// PORT: a program load and the auto-import registry use `acquire`, and
// their files bind later in the order that keeps the lineage ids (see
// `new_parse_cache`). A file that is bound already is not bound again (Go
// `BindOnce`), so a later program that includes the file gives the same
// result.
pub fn acquire_bound(
    cache: &ParseCache,
    key: ParseCacheKey,
    fh: Rc<dyn FileHandle>,
    current_directory: &str,
) -> HashedSourceFile {
    let result = cache.acquire(key, fh);
    crate::program::publish_parsed_files(current_directory);
    crate::program::bind_file_outside_program(result.file.root);
    result
}

// Go: project/parsecache.go:84 ContentMappedParseCache (tsgo#4712)
// PORT: Go embeds `*RefCountCache`; the type alias gives the same methods.
// One reference owns the canonical file and all supplemental files as a
// bundle (`contentmapper::SourceFiles`). Callers ref and deref the
// canonical file only.
pub type ContentMappedParseCache =
    RefCountCache<ContentMappedParseCacheKey, contentmapper::SourceFiles, ()>;

// Go: project/parsecache.go:90 NewContentMappedParseCache (tsgo#4712)
pub fn new_content_mapped_parse_cache(
    options: RefCountCacheOptions,
) -> Rc<ContentMappedParseCache> {
    new_ref_count_cache(
        options,
        |_: &ContentMappedParseCacheKey, (): ()| -> contentmapper::SourceFiles {
            crate::core::go_panic(
                "content-mapped source files must be produced with AcquireOrError".to_string(),
            )
        },
    )
}

/// Go `file.Hash = hash` for a file that the content-mapped parse cache
/// holds (project/compilerhost.go GetContentMappedSourceFiles). A
/// content-mapped file's Go `Hash` is the hash of its cache key, not of its
/// text.
pub fn set_source_file_hash(file: &parser::ParsedSourceFile, hash: u128) {
    file.hash.set(Some(hash));
}

/// Go `contentMappedParseCache.Deref(key)` for a program file or a
/// duplicate source file.
pub fn deref_content_mapped_file(
    cache: &ContentMappedParseCache,
    key: &ContentMappedParseCacheKey,
) {
    // PORT: called by path so `std::ops::Deref::deref` can not win.
    ContentMappedParseCache::deref(cache, key);
}

/// Go `parseCache.Ref(NewParseCacheKey(file.ParseOptions(), file.Hash,
/// file.ScriptKind))` for a program file or a duplicate source file.
/// `hash` is Go `file.Hash` (`ParsedSourceFile::source_hash`).
pub fn ref_program_file(
    cache: &ParseCache,
    options: &parser::SourceFileParseOptions,
    hash: u128,
    script_kind: ScriptKind,
) {
    cache.ref_(&new_parse_cache_key(options, hash, script_kind));
}

/// Go `parseCache.Deref(NewParseCacheKey(file.ParseOptions(), file.Hash,
/// file.ScriptKind))` for a program file or a duplicate source file.
pub fn deref_program_file(
    cache: &ParseCache,
    options: &parser::SourceFileParseOptions,
    hash: u128,
    script_kind: ScriptKind,
) {
    let key = new_parse_cache_key(options, hash, script_kind);
    // PORT: called by path so `std::ops::Deref::deref` can not win.
    ParseCache::deref(cache, &key);
}

/// One slot of `ProgramFileRefs`, for one file of `program.source_files()`.
enum FileRef {
    /// Go takes no reference (a content-mapper failure stub or a
    /// supplemental file).
    None,
    /// The parse cache entry of a plain file. The program holds one count
    /// on it, so it stays the entry that the cache has for the file's key.
    Entry(Rc<RefCountCacheEntry<HashedSourceFile>>),
    /// A content-mapped file, or a plain file whose entry was not in the
    /// cache: refs and derefs go through the key, as in Go.
    Key,
}

/// Not in Go: the parse cache entries that one project program holds a
/// count on, one slot per file of `program.source_files()`. Go (and the
/// port before) builds a key, hashes it and looks it up for every program
/// file twice per edit: once to ref the file in the cloned program
/// (project.go CreateProgram) and once to deref it when the old snapshot is
/// disposed (snapshot.go dispose). With the entry kept here, a file that the
/// cloned program shares with the old program is counted through the old
/// program's slot: only the changed files need a key. The counts stay as in
/// Go. Duplicate source files still go through the key.
pub struct ProgramFileRefs {
    slots: Vec<FileRef>,
}

impl ProgramFileRefs {
    /// Collects the entries of `files`. With `take_refs`, also refs each
    /// file except `acquired` (Go CreateProgram for a cloned program:
    /// `UpdateProgram` acquired only the changed file). Without it, the
    /// program's own loads hold the counts already. `old` is the program
    /// that this one was cloned from, with its refs: a file at the same
    /// index that is the same parse uses the old entry.
    pub fn new(
        parse_cache: &ParseCache,
        content_mapped_parse_cache: &ContentMappedParseCache,
        files: &[Rc<parser::ParsedSourceFile>],
        take_refs: bool,
        acquired: Option<&Rc<parser::ParsedSourceFile>>,
        old: Option<(&[Rc<parser::ParsedSourceFile>], &ProgramFileRefs)>,
    ) -> ProgramFileRefs {
        let slots = files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                if file.is_content_mapper_failure_stub() || file.is_content_mapper_supplemental() {
                    return FileRef::None;
                }
                // Use pointer identity: `acquired` is the exact instance UpdateProgram acquired,
                // and it is the only file whose refcount is already accounted for.
                let take_ref = take_refs && !acquired.is_some_and(|f| Rc::ptr_eq(f, file));
                if !file.content_mapper().is_empty() {
                    if take_ref {
                        content_mapped_parse_cache
                            .ref_(&content_mapped_parse_cache_key_for_file(file));
                    }
                    return FileRef::Key;
                }
                let shared = old.and_then(|(old_files, old_refs)| {
                    match (old_files.get(index), old_refs.slots.get(index)) {
                        (Some(old_file), Some(FileRef::Entry(entry)))
                            if Rc::ptr_eq(old_file, file) =>
                        {
                            Some(entry.clone())
                        }
                        _ => None,
                    }
                });
                match shared {
                    Some(entry) if take_ref && entry.ref_count.get() > 0 => {
                        // Go: parseCache.Ref(key), with the entry found.
                        entry.ref_count.set(entry.ref_count.get() + 1);
                        FileRef::Entry(entry)
                    }
                    _ => {
                        let key = parse_cache_key_for_file(file);
                        if take_ref {
                            parse_cache.ref_(&key);
                        }
                        match parse_cache.entries.borrow().get(&key) {
                            Some(entry) => FileRef::Entry(entry.clone()),
                            None => FileRef::Key,
                        }
                    }
                }
            })
            .collect();
        ProgramFileRefs { slots }
    }

    /// Go snapshot.go dispose: deref each file of the program. `files` is
    /// `program.source_files()` of the program these refs were made for.
    pub fn release(
        &self,
        parse_cache: &ParseCache,
        content_mapped_parse_cache: &ContentMappedParseCache,
        files: &[Rc<parser::ParsedSourceFile>],
    ) {
        // `zip` stops at the shorter list: a length mismatch would skip
        // derefs, so it must be the same program's files.
        debug_assert_eq!(
            files.len(),
            self.slots.len(),
            "ProgramFileRefs::release: files are not the files these refs were made for"
        );
        for (file, slot) in files.iter().zip(&self.slots) {
            match slot {
                FileRef::None => {}
                // Go: parseCache.Deref(key), with the entry found.
                FileRef::Entry(entry) => {
                    entry.ref_count.set(entry.ref_count.get() - 1);
                    if entry.ref_count.get() <= 0 && !parse_cache.options.disable_deletion {
                        parse_cache
                            .entries
                            .borrow_mut()
                            .remove(&parse_cache_key_for_file(file));
                    }
                }
                FileRef::Key if !file.content_mapper().is_empty() => {
                    deref_content_mapped_file(
                        content_mapped_parse_cache,
                        &content_mapped_parse_cache_key_for_file(file),
                    );
                }
                FileRef::Key => {
                    deref_program_file(
                        parse_cache,
                        file.parse_options(),
                        file.source_hash(),
                        file.script_kind,
                    );
                }
            }
        }
    }
}
