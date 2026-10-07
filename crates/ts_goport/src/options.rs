//! Port of Go `core/compileroptions.go`, `core/tristate.go`,
//! `core/languagevariant.go` and `core/scriptkind.go` (plus their stringers).
//!
//! The Go enum types (`ScriptTarget`, `ModuleKind`, `ModuleResolutionKind`,
//! `ModuleDetectionKind`, `NewLineKind`, `JsxEmit`, `LanguageVariant`,
//! `ScriptKind`) are defined in `crate::flags` with the Go values. Their Go
//! methods are added here as inherent impls.

use crate::frontend::tspath;
use crate::prelude::*;

// ---------------------------------------------------------------------------
// core/tristate.go
// ---------------------------------------------------------------------------

/// Go `core.Tristate`. Values match Go (`TSUnknown` = 0, `TSFalse` = 1,
/// `TSTrue` = 2).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Tristate {
    #[default]
    Unknown = 0,
    False = 1,
    True = 2,
}

impl Tristate {
    // Go: core/tristate.go:15 IsTrue
    #[must_use]
    pub fn is_true(self) -> bool {
        self == Tristate::True
    }

    // Go: core/tristate.go:19 IsTrueOrUnknown
    #[must_use]
    pub fn is_true_or_unknown(self) -> bool {
        self == Tristate::True || self == Tristate::Unknown
    }

    // Go: core/tristate.go:23 IsFalse
    #[must_use]
    pub fn is_false(self) -> bool {
        self == Tristate::False
    }

    // Go: core/tristate.go:27 IsFalseOrUnknown
    #[must_use]
    pub fn is_false_or_unknown(self) -> bool {
        self == Tristate::False || self == Tristate::Unknown
    }

    // Go: core/tristate.go:31 IsUnknown
    #[must_use]
    pub fn is_unknown(self) -> bool {
        self == Tristate::Unknown
    }

    // Go: core/tristate.go:35 DefaultIfUnknown
    #[must_use]
    pub fn default_if_unknown(self, value: Tristate) -> Tristate {
        if self == Tristate::Unknown {
            return value;
        }
        self
    }

    // Go: core/tristate.go:42 UnmarshalJSON
    pub fn unmarshal_json(&mut self, data: &[u8]) {
        *self = match data {
            b"true" => Tristate::True,
            b"false" => Tristate::False,
            _ => Tristate::Unknown,
        };
    }

    // Go: core/tristate.go:54 MarshalJSON
    #[must_use]
    pub fn marshal_json(self) -> &'static [u8] {
        match self {
            Tristate::True => b"true",
            Tristate::False => b"false",
            Tristate::Unknown => b"null",
        }
    }

    // Go: core/tristate_stringer_generated.go String
    #[must_use]
    pub fn string(self) -> String {
        match self {
            Tristate::Unknown => "TSUnknown",
            Tristate::False => "TSFalse",
            Tristate::True => "TSTrue",
        }
        .to_string()
    }
}

impl std::fmt::Display for Tristate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

// Go: core/tristate.go:65 BoolToTristate
#[must_use]
pub fn bool_to_tristate(b: bool) -> Tristate {
    if b {
        return Tristate::True;
    }
    Tristate::False
}

// ---------------------------------------------------------------------------
// core/compileroptions.go
// ---------------------------------------------------------------------------

// Go: core/compileroptions.go:14 PluginImport (ts#64397)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginImport {
    pub name: String,
}

/// Go `core.CompilerOptions`. Field names are the Go names in snake case.
/// CompilerOptions contains the compiler options exposed by the API.
// Go: core/compileroptions.go:19 CompilerOptions
// PORT: Go `noCopy` is dropped. Go `[]string` fields are
// `Option<Vec<String>>`: a nil slice is `None` and an empty non-nil slice is
// `Some(vec![])`. `mergeCompilerOptions` copies an empty non-nil slice (so
// `"types": []` overrides the parent), and some readers test `!= nil`.
// Go `*collections.OrderedMap` is `Option<IndexMap>` and Go `*int` is
// `Option<i64>` (Go `int` is 64-bit).
// PORT: the derived `==` ignores the `paths` key order, because `IndexMap`
// equality does. Go `reflect.DeepEqual` sees it: use `deep_equal` for that.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompilerOptions {
    pub allow_js: Tristate,
    pub allow_arbitrary_extensions: Tristate,
    pub allow_importing_ts_extensions: Tristate,
    pub allow_non_ts_extensions: Tristate,
    pub allow_umd_global_access: Tristate,
    pub allow_unreachable_code: Tristate,
    pub allow_unused_labels: Tristate,
    pub assume_changes_only_affect_direct_dependencies: Tristate,
    pub check_js: Tristate,
    pub custom_conditions: Option<Vec<String>>,
    pub composite: Tristate,
    pub emit_declaration_only: Tristate,
    pub emit_bom: Tristate,
    pub emit_decorator_metadata: Tristate,
    pub declaration: Tristate,
    pub declaration_dir: String,
    pub declaration_map: Tristate,
    pub deduplicate_packages: Tristate,
    pub disable_size_limit: Tristate,
    pub disable_source_of_project_reference_redirect: Tristate,
    pub disable_solution_searching: Tristate,
    pub disable_referenced_project_load: Tristate,
    pub erasable_syntax_only: Tristate,
    pub exact_optional_property_types: Tristate,
    pub experimental_decorators: Tristate,
    pub force_consistent_casing_in_file_names: Tristate,
    pub isolated_modules: Tristate,
    pub isolated_declarations: Tristate,
    pub ignore_config: Tristate,
    pub ignore_deprecations: String,
    pub import_helpers: Tristate,
    pub inline_source_map: Tristate,
    pub inline_sources: Tristate,
    pub init: Tristate,
    pub incremental: Tristate,
    pub jsx: JsxEmit,
    pub jsx_factory: String,
    pub jsx_fragment_factory: String,
    pub jsx_import_source: String,
    pub lib: Option<Vec<String>>,
    pub lib_replacement: Tristate,
    pub locale: String,
    pub map_root: String,
    pub module: ModuleKind,
    pub module_resolution: ModuleResolutionKind,
    pub module_suffixes: Option<Vec<String>>,
    pub module_detection: ModuleDetectionKind,
    pub new_line: NewLineKind,
    pub no_emit: Tristate,
    pub no_check: Tristate,
    pub no_error_truncation: Tristate,
    pub no_fallthrough_cases_in_switch: Tristate,
    pub no_implicit_any: Tristate,
    pub no_implicit_this: Tristate,
    pub no_implicit_returns: Tristate,
    pub no_emit_helpers: Tristate,
    pub no_lib: Tristate,
    pub no_property_access_from_index_signature: Tristate,
    pub no_unchecked_indexed_access: Tristate,
    pub no_emit_on_error: Tristate,
    pub no_unused_locals: Tristate,
    pub no_unused_parameters: Tristate,
    pub no_resolve: Tristate,
    pub no_implicit_override: Tristate,
    pub no_unchecked_side_effect_imports: Tristate,
    pub out_dir: String,
    /// Go `*OrderedMap[string, []string]`; a nil substitution slice is `None`.
    pub paths: Option<IndexMap<String, Option<Vec<String>>>>,
    // Plugins are parsed only so tools can report that native TypeScript does not support them.
    pub plugins: Option<Vec<PluginImport>>,
    pub preserve_const_enums: Tristate,
    pub preserve_symlinks: Tristate,
    pub project: String,
    pub resolve_json_module: Tristate,
    pub resolve_package_json_exports: Tristate,
    pub resolve_package_json_imports: Tristate,
    pub remove_comments: Tristate,
    pub rewrite_relative_import_extensions: Tristate,
    pub react_namespace: String,
    pub root_dir: String,
    pub root_dirs: Option<Vec<String>>,
    pub skip_lib_check: Tristate,
    pub stable_type_ordering: Tristate,
    pub strict: Tristate,
    pub strict_bind_call_apply: Tristate,
    pub strict_builtin_iterator_return: Tristate,
    pub strict_function_types: Tristate,
    pub strict_null_checks: Tristate,
    pub strict_property_initialization: Tristate,
    pub strip_internal: Tristate,
    pub skip_default_lib_check: Tristate,
    pub source_map: Tristate,
    pub source_root: String,
    pub suppress_output_path_check: Tristate,
    pub target: ScriptTarget,
    pub trace_resolution: Tristate,
    pub ts_build_info_file: String,
    pub type_roots: Option<Vec<String>>,
    pub types: Option<Vec<String>>,
    pub use_define_for_class_fields: Tristate,
    pub use_unknown_in_catch_variables: Tristate,
    pub verbatim_module_syntax: Tristate,
    pub max_node_module_js_depth: Option<i64>,

    // Deprecated: Do not use outside of options parsing and validation.
    pub allow_synthetic_default_imports: Tristate,
    // Deprecated: Do not use outside of options parsing and validation.
    pub always_strict: Tristate,
    // Deprecated: Do not use outside of options parsing and validation.
    pub base_url: String,
    // Deprecated: Do not use outside of options parsing and validation.
    pub downlevel_iteration: Tristate,
    // Deprecated: Do not use outside of options parsing and validation.
    pub es_module_interop: Tristate,
    // Deprecated: Do not use outside of options parsing and validation.
    pub out_file: String,

    // Internal fields
    // PORT: tsgo#4915 tags these `internal:"true"` (and the deprecated fields
    // above `deprecated:"true"`) for the TS API generator. The JSON is the
    // same, so the port has no tags.
    pub config_file_path: String, // internal, but intentionally exposed via API
    pub no_dts_resolution: Tristate,
    pub paths_base_path: String,
    pub diagnostics: Tristate,
    pub extended_diagnostics: Tristate,
    pub generate_cpu_profile: String,
    pub generate_trace: String,
    pub list_emitted_files: Tristate,
    pub list_files: Tristate,
    pub explain_files: Tristate,
    pub list_files_only: Tristate,
    pub no_emit_for_js_files: Tristate,
    pub preserve_watch_output: Tristate,
    pub pretty: Tristate,
    pub version: Tristate,
    pub watch: Tristate,
    pub show_config: Tristate,
    pub build: Tristate,
    pub help: Tristate,
    pub all: Tristate,
    // tsgo#4712
    pub run_external_code: Tristate,

    pub pprof_dir: String,
    pub single_threaded: Tristate,
    pub quiet: Tristate,
    pub checkers: Option<i64>,

    /// Effect patch 007 (Go `Effect *etscore.EffectPluginOptions`): the
    /// options that the extension (`crate::ext`) parsed from `plugins`. Go
    /// nil is `None`. It has no JSON form (`options_json.rs`), and
    /// `merge_compiler_options` leaves it to the extension.
    pub ext: Option<crate::ext::ExtOptions>,
}

static EMPTY_COMPILER_OPTIONS: std::sync::OnceLock<CompilerOptions> = std::sync::OnceLock::new();

// Go: core/compileroptions.go:178 EmptyCompilerOptions
#[must_use]
pub fn empty_compiler_options() -> &'static CompilerOptions {
    EMPTY_COMPILER_OPTIONS.get_or_init(CompilerOptions::default)
}

impl CompilerOptions {
    // Go: core/compileroptions.go:183 Clone
    // Clone creates a shallow copy of the CompilerOptions.
    // PORT: Go copies every exported field by reflection. The derived
    // `Clone::clone` copies every field, which is the same set, so Go
    // `options.Clone()` ports to `options.clone()` with no inherent method.

    /// Go `reflect.DeepEqual` of two option sets: `==`, and the same `paths`
    /// key order. Go `OrderedMap` keeps its keys in a slice, and module
    /// resolution and module specifiers try the `paths` patterns in order.
    #[must_use]
    pub fn deep_equal(&self, other: &Self) -> bool {
        self == other
            && self.paths.as_ref().map(IndexMap::as_slice)
                == other.paths.as_ref().map(IndexMap::as_slice)
    }

    // Go: core/compileroptions.go:199 GetEmitScriptTarget
    #[must_use]
    pub fn get_emit_script_target(&self) -> ScriptTarget {
        if self.target != ScriptTarget::NONE {
            return self.target;
        }
        ScriptTarget::LATEST_STANDARD
    }

    // Go: core/compileroptions.go:206 GetEmitModuleKind
    #[must_use]
    pub fn get_emit_module_kind(&self) -> ModuleKind {
        if self.module != ModuleKind::NONE {
            return self.module;
        }

        let target = self.get_emit_script_target();
        if target == ScriptTarget::ES_NEXT {
            return ModuleKind::ES_NEXT;
        }
        if target >= ScriptTarget::ES2022 {
            return ModuleKind::ES2022;
        }
        if target >= ScriptTarget::ES2020 {
            return ModuleKind::ES2020;
        }
        if target >= ScriptTarget::ES2015 {
            return ModuleKind::ES2015;
        }
        ModuleKind::COMMON_JS
    }

    // Go: core/compileroptions.go:227 GetModuleResolutionKind
    #[must_use]
    pub fn get_module_resolution_kind(&self) -> ModuleResolutionKind {
        match self.module_resolution {
            ModuleResolutionKind::UNKNOWN
            | ModuleResolutionKind::CLASSIC
            | ModuleResolutionKind::NODE10 => match self.get_emit_module_kind() {
                ModuleKind::NODE16 | ModuleKind::NODE18 | ModuleKind::NODE20 => {
                    ModuleResolutionKind::NODE16
                }
                ModuleKind::NODE_NEXT => ModuleResolutionKind::NODE_NEXT,
                _ => ModuleResolutionKind::BUNDLER,
            },
            _ => self.module_resolution,
        }
    }

    // Go: core/compileroptions.go:243 GetEmitModuleDetectionKind
    #[must_use]
    pub fn get_emit_module_detection_kind(&self) -> ModuleDetectionKind {
        if self.module_detection != ModuleDetectionKind::NONE {
            return self.module_detection;
        }
        let module_kind = self.get_emit_module_kind();
        if ModuleKind::NODE16 <= module_kind && module_kind <= ModuleKind::NODE_NEXT {
            return ModuleDetectionKind::FORCE;
        }
        ModuleDetectionKind::AUTO
    }

    // Go: core/compileroptions.go:254 GetResolvePackageJsonExports
    #[must_use]
    pub fn get_resolve_package_json_exports(&self) -> bool {
        self.resolve_package_json_exports.is_true_or_unknown()
    }

    // Go: core/compileroptions.go:258 GetResolvePackageJsonImports
    #[must_use]
    pub fn get_resolve_package_json_imports(&self) -> bool {
        self.resolve_package_json_imports.is_true_or_unknown()
    }

    // Go: core/compileroptions.go:262 GetAllowImportingTsExtensions
    #[must_use]
    pub fn get_allow_importing_ts_extensions(&self) -> bool {
        self.allow_importing_ts_extensions.is_true()
            || self.rewrite_relative_import_extensions.is_true()
    }

    // Go: core/compileroptions.go:266 AllowImportingTsExtensionsFrom
    #[must_use]
    pub fn allow_importing_ts_extensions_from(&self, file_name: &str) -> bool {
        self.get_allow_importing_ts_extensions() || tspath_is_declaration_file_name(file_name)
    }

    // Go: core/compileroptions.go:270 GetResolveJsonModule
    #[must_use]
    pub fn get_resolve_json_module(&self) -> bool {
        if self.resolve_json_module != Tristate::Unknown {
            return self.resolve_json_module == Tristate::True;
        }
        match self.get_emit_module_kind() {
            // TODO in 6.0: add Node16/Node18
            ModuleKind::NODE20 | ModuleKind::NODE_NEXT => return true,
            _ => {}
        }
        self.get_module_resolution_kind() == ModuleResolutionKind::BUNDLER
    }

    // Go: core/compileroptions.go:282 ShouldPreserveConstEnums
    #[must_use]
    pub fn should_preserve_const_enums(&self) -> bool {
        self.preserve_const_enums == Tristate::True || self.get_isolated_modules()
    }

    // Go: core/compileroptions.go:286 GetAllowJS
    #[must_use]
    pub fn get_allow_js(&self) -> bool {
        if self.allow_js != Tristate::Unknown {
            return self.allow_js == Tristate::True;
        }
        self.check_js == Tristate::True
    }

    // Go: core/compileroptions.go:293 GetJSXTransformEnabled
    #[must_use]
    pub fn get_jsx_transform_enabled(&self) -> bool {
        let jsx = self.jsx;
        jsx == JsxEmit::REACT || jsx == JsxEmit::REACT_JSX || jsx == JsxEmit::REACT_JSX_DEV
    }

    // Go: core/compileroptions.go:298 GetStrictOptionValue
    #[must_use]
    pub fn get_strict_option_value(&self, value: Tristate) -> bool {
        if value != Tristate::Unknown {
            return value == Tristate::True;
        }
        self.strict != Tristate::False
    }

    // Go: core/compileroptions.go:305 GetEffectiveTypeRoots
    /// Returns `(result, fromConfig)`.
    #[must_use]
    pub fn get_effective_type_roots(&self, current_directory: &str) -> (Vec<String>, bool) {
        if let Some(type_roots) = &self.type_roots {
            return (type_roots.clone(), true);
        }
        let base_dir: String;
        if !self.config_file_path.is_empty() {
            base_dir = tspath::get_directory_path(&self.config_file_path);
        } else {
            base_dir = current_directory.to_string();
            if base_dir.is_empty() {
                // This was accounted for in the TS codebase, but only for third-party API usage
                // where the module resolution host does not provide a getCurrentDirectory().
                panic!(
                    "cannot get effective type roots without a config file path or current directory"
                );
            }
        }

        let mut type_roots: Vec<String> = Vec::with_capacity(base_dir.matches('/').count());
        tspath_for_each_ancestor_directory(&base_dir, &mut |dir: &str| {
            type_roots.push(tspath::combine_paths(dir, &["node_modules", "@types"]));
            false
        });
        (type_roots, false)
    }

    // Go: core/compileroptions.go:330 UsesWildcardTypes
    // UsesWildcardTypes returns true if this option's types array includes "*"
    #[must_use]
    pub fn uses_wildcard_types(&self) -> bool {
        self.types.iter().flatten().any(|t| t == "*")
    }

    // Go: core/compileroptions.go:334 GetIsolatedModules
    #[must_use]
    pub fn get_isolated_modules(&self) -> bool {
        self.isolated_modules == Tristate::True || self.verbatim_module_syntax == Tristate::True
    }

    // Go: core/compileroptions.go:338 IsIncremental
    #[must_use]
    pub fn is_incremental(&self) -> bool {
        self.incremental.is_true() || self.composite.is_true()
    }

    // Go: core/compileroptions.go:342 GetEmitStandardClassFields
    #[must_use]
    pub fn get_emit_standard_class_fields(&self) -> bool {
        self.use_define_for_class_fields != Tristate::False
            && self.get_emit_script_target() >= ScriptTarget::ES2022
    }

    // Go: core/compileroptions.go:346 GetUseDefineForClassFields
    #[must_use]
    pub fn get_use_define_for_class_fields(&self) -> bool {
        if self.use_define_for_class_fields == Tristate::Unknown {
            return self.get_emit_script_target() >= ScriptTarget::ES2022;
        }
        self.use_define_for_class_fields == Tristate::True
    }

    // Go: core/compileroptions.go:353 GetEmitDeclarations
    #[must_use]
    pub fn get_emit_declarations(&self) -> bool {
        self.declaration.is_true() || self.composite.is_true()
    }

    // Go: core/compileroptions.go:357 GetAreDeclarationMapsEnabled
    #[must_use]
    pub fn get_are_declaration_maps_enabled(&self) -> bool {
        self.declaration_map == Tristate::True && self.get_emit_declarations()
    }

    // Go: core/compileroptions.go:361 HasJsonModuleEmitEnabled
    #[must_use]
    pub fn has_json_module_emit_enabled(&self) -> bool {
        match self.get_emit_module_kind() {
            ModuleKind::SYSTEM | ModuleKind::UMD => return false,
            _ => {}
        }
        true
    }

    // Go: core/compileroptions.go:369 GetPathsBasePath
    #[must_use]
    pub fn get_paths_base_path(&self, current_directory: &str) -> String {
        // Go `Paths.Size()` is 0 for a nil map.
        if self.paths.as_ref().map_or(0, IndexMap::len) == 0 {
            return String::new();
        }
        if !self.paths_base_path.is_empty() {
            return self.paths_base_path.clone();
        }
        current_directory.to_string()
    }
}

impl ModuleKind {
    // Go: core/compileroptions.go:430 ResolutionModeESM
    // PORT: Go `ResolutionMode` is an alias of `ModuleKind`, so
    // `core.ResolutionModeESM` is also reachable as `ResolutionMode::ESM`.
    // `ResolutionModeNone` and `ResolutionModeCommonJS` are
    // `ResolutionMode::NONE` and `ResolutionMode::COMMON_JS`.
    pub const ESM: Self = Self::ES_NEXT;

    // Go: core/compileroptions.go:415 IsNonNodeESM
    #[must_use]
    pub fn is_non_node_esm(self) -> bool {
        self >= ModuleKind::ES2015 && self <= ModuleKind::ES_NEXT
    }

    // Go: core/compileroptions.go:419 SupportsImportAttributes
    #[must_use]
    pub fn supports_import_attributes(self) -> bool {
        ModuleKind::NODE18 <= self && self <= ModuleKind::NODE_NEXT
            || self == ModuleKind::PRESERVE
            || self == ModuleKind::ES_NEXT
    }

    // Go: core/modulekind_stringer_generated.go String
    #[must_use]
    pub fn string(self) -> String {
        let name = match self {
            ModuleKind::NONE => "None",
            ModuleKind::COMMON_JS => "CommonJS",
            ModuleKind::AMD => "AMD",
            ModuleKind::UMD => "UMD",
            ModuleKind::SYSTEM => "System",
            ModuleKind::ES2015 => "ES2015",
            ModuleKind::ES2020 => "ES2020",
            ModuleKind::ES2022 => "ES2022",
            ModuleKind::ES_NEXT => "ESNext",
            ModuleKind::NODE16 => "Node16",
            ModuleKind::NODE18 => "Node18",
            ModuleKind::NODE20 => "Node20",
            ModuleKind::NODE_NEXT => "NodeNext",
            ModuleKind::PRESERVE => "Preserve",
            _ => return format!("ModuleKind({})", self.0),
        };
        name.to_string()
    }
}

impl std::fmt::Display for ModuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

/// Go `core.ResolutionMode` (`ModuleKindNone | ModuleKindCommonJS | ModuleKindESNext`).
// Go: core/compileroptions.go:425 ResolutionMode
pub type ResolutionMode = ModuleKind;

// Go: core/compileroptions.go:428 ResolutionModeNone
pub const RESOLUTION_MODE_NONE: ResolutionMode = ModuleKind::NONE;
// Go: core/compileroptions.go:429 ResolutionModeCommonJS
pub const RESOLUTION_MODE_COMMON_JS: ResolutionMode = ModuleKind::COMMON_JS;
// Go: core/compileroptions.go:430 ResolutionModeESM
pub const RESOLUTION_MODE_ESM: ResolutionMode = ModuleKind::ES_NEXT;

// Go: core/compileroptions.go:451 ModuleKindToModuleResolutionKind
/// Go map lookup `ModuleKindToModuleResolutionKind[kind]` as `(value, ok)`.
#[must_use]
pub fn module_kind_to_module_resolution_kind(kind: ModuleKind) -> (ModuleResolutionKind, bool) {
    match kind {
        ModuleKind::NODE16 => (ModuleResolutionKind::NODE16, true),
        ModuleKind::NODE_NEXT => (ModuleResolutionKind::NODE_NEXT, true),
        _ => (ModuleResolutionKind::UNKNOWN, false),
    }
}

impl ModuleResolutionKind {
    // Go: core/compileroptions.go:457 String
    // We don't use stringer on this for now, because these values
    // are user-facing in --traceResolution, and stringer currently
    // lacks the ability to remove the "ModuleResolutionKind" prefix
    // when generating code for multiple types into the same output
    // file. Additionally, since there's no TS equivalent of
    // `ModuleResolutionKindUnknown`, we want to panic on that case,
    // as it probably represents a mistake when porting TS to Go.
    #[must_use]
    pub fn string(self) -> String {
        match self {
            ModuleResolutionKind::UNKNOWN => {
                panic!("should not use zero value of ModuleResolutionKind")
            }
            ModuleResolutionKind::CLASSIC => "Classic".to_string(),
            ModuleResolutionKind::NODE10 => "Node10".to_string(),
            ModuleResolutionKind::NODE16 => "Node16".to_string(),
            ModuleResolutionKind::NODE_NEXT => "NodeNext".to_string(),
            ModuleResolutionKind::BUNDLER => "Bundler".to_string(),
            _ => panic!("unhandled case in ModuleResolutionKind.String"),
        }
    }
}

impl std::fmt::Display for ModuleResolutionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

// Go: core/compileroptions.go:490 GetNewLineKind
#[must_use]
pub fn get_new_line_kind(s: &str) -> NewLineKind {
    match s {
        "\r\n" => NewLineKind::CRLF,
        "\n" => NewLineKind::LF,
        _ => NewLineKind::NONE,
    }
}

impl NewLineKind {
    // Go: core/compileroptions.go:501 GetNewLineCharacter
    #[must_use]
    pub fn get_new_line_character(self) -> &'static str {
        match self {
            NewLineKind::CRLF => "\r\n",
            _ => "\n",
        }
    }
}

impl ScriptTarget {
    // Go: core/scripttarget_stringer_generated.go String
    #[must_use]
    pub fn string(self) -> String {
        let name = match self.0 {
            0 => "None",
            1 => "ES5",
            2 => "ES2015",
            3 => "ES2016",
            4 => "ES2017",
            5 => "ES2018",
            6 => "ES2019",
            7 => "ES2020",
            8 => "ES2021",
            9 => "ES2022",
            10 => "ES2023",
            11 => "ES2024",
            12 => "ES2025",
            13 => "ES2026",
            99 => "ESNext",
            100 => "JSON",
            _ => return format!("ScriptTarget({})", self.0),
        };
        name.to_string()
    }
}

impl std::fmt::Display for ScriptTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

impl JsxEmit {
    // Go: core/compileroptions.go:538 String
    #[must_use]
    pub fn string(self) -> String {
        match self {
            JsxEmit::NONE => panic!("should not use zero value of JsxEmit"),
            JsxEmit::PRESERVE => "preserve".to_string(),
            JsxEmit::REACT_NATIVE => "react-native".to_string(),
            JsxEmit::REACT => "react".to_string(),
            JsxEmit::REACT_JSX => "react-jsx".to_string(),
            JsxEmit::REACT_JSX_DEV => "react-jsxdev".to_string(),
            _ => panic!("unhandled case in JsxEmit.String"),
        }
    }
}

impl std::fmt::Display for JsxEmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

// ---------------------------------------------------------------------------
// core/languagevariant.go, core/scriptkind.go
// ---------------------------------------------------------------------------
// The types and constants live in `crate::flags` (`LanguageVariant`,
// `ScriptKind`). Only the generated stringers are ported here.

impl LanguageVariant {
    // Go: core/languagevariant_stringer_generated.go String
    #[must_use]
    pub fn string(self) -> String {
        match self {
            LanguageVariant::STANDARD => "LanguageVariantStandard".to_string(),
            LanguageVariant::JSX => "LanguageVariantJSX".to_string(),
            _ => format!("LanguageVariant({})", self.0),
        }
    }
}

impl std::fmt::Display for LanguageVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

impl ScriptKind {
    // Go: core/scriptkind_stringer_generated.go:26 String
    // tsgo#4712: values 5 (formerly ScriptKindExternal) and 7 (formerly
    // ScriptKindDeferred) are reserved and print as "ScriptKind(5)" and
    // "ScriptKind(7)".
    #[must_use]
    pub fn string(self) -> String {
        let name = match self {
            ScriptKind::UNKNOWN => "ScriptKindUnknown",
            ScriptKind::JS => "ScriptKindJS",
            ScriptKind::JSX => "ScriptKindJSX",
            ScriptKind::TS => "ScriptKindTS",
            ScriptKind::TSX => "ScriptKindTSX",
            ScriptKind::JSON => "ScriptKindJSON",
            _ => return format!("ScriptKind({})", self.0),
        };
        name.to_string()
    }
}

impl std::fmt::Display for ScriptKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

// ---------------------------------------------------------------------------
// tspath helpers used above. Private so they do not clash with a tspath port
// elsewhere in the crate. `GetDirectoryPath` and `GetBaseFileName` are the
// `frontend::tspath` ports.
// ---------------------------------------------------------------------------

// Go: tspath/extension.go:121 GetDeclarationFileExtension
fn tspath_get_declaration_file_extension(file_name: &str) -> String {
    let base = tspath::get_base_file_name(file_name);
    // Go: tspath.SupportedDeclarationExtensions
    for ext in [".d.ts", ".d.cts", ".d.mts"] {
        if base.ends_with(ext) {
            return ext.to_string();
        }
    }
    if base.ends_with(".ts") {
        if let Some(index) = base.find(".d.") {
            return base[index..].to_string();
        }
    }
    String::new()
}

// Go: tspath/extension.go:113 IsDeclarationFileName
fn tspath_is_declaration_file_name(file_name: &str) -> bool {
    !tspath_get_declaration_file_extension(file_name).is_empty()
}

// Go: tspath/path.go:1116 ForEachAncestorDirectory
// PORT: the callback returns only `stop`; the only caller here has no result.
fn tspath_for_each_ancestor_directory(
    directory: &str,
    callback: &mut dyn FnMut(&str) -> bool,
) -> bool {
    let mut directory = directory.to_string();
    loop {
        if callback(&directory) {
            return true;
        }

        let parent_path = tspath::get_directory_path(&directory);
        if parent_path == directory {
            return false;
        }

        directory = parent_path;
    }
}
