//! Port of typescript-go `internal/checker/utilities.go` lines 1-903.
//!
//! PORT: Several Go `checker` package functions in this range have the same
//! snake name as an exported `ast` function that is already ported
//! (`ast.IsBinaryOperator`, `ast.IsEmptyObjectLiteral`,
//! `ast.EntityNameToString`, ...). Both modules are glob-exported through the
//! prelude, so a second `pub fn` with the same name would make every use
//! ambiguous. Those checker functions get a `checker_` prefix here (the same
//! choice `checker_p18.rs` made for `entityNameToString`).

use crate::prelude::*;

// Go: checker/utilities.go:22 NewDiagnosticForNode
pub fn new_diagnostic_for_node(
    node: Node,
    message: &'static crate::diagnostics::Message,
    args: Vec<String>,
) -> Diagnostic {
    let mut file = Node::NIL;
    let mut loc = TextRange::new(0, 0);
    if node.is_some() {
        file = get_source_file_of_node(node);
        loc = get_error_range_for_node(file, node);
    }
    new_diagnostic(file, loc, message, args)
}

// Go: checker/utilities.go:32 NewDiagnosticChainForNode
pub fn new_diagnostic_chain_for_node(
    chain: Option<Diagnostic>,
    node: Node,
    message: &'static crate::diagnostics::Message,
    args: Vec<String>,
) -> Diagnostic {
    if chain.is_some() {
        return new_diagnostic_chain(chain, message, args);
    }
    new_diagnostic_for_node(node, message, args)
}

// Go: checker/utilities.go:39 findInMap
// PORT: Go returns the zero value of V when nothing matches; here `V::default()`.
pub fn find_in_map<K, V: Default + Clone>(
    m: &FxHashMap<K, V>,
    mut predicate: impl FnMut(&V) -> bool,
) -> V {
    for value in m.values() {
        if predicate(value) {
            return value.clone();
        }
    }
    V::default()
}

// Go: checker/utilities.go:48 tokenIsIdentifierOrKeyword
pub fn token_is_identifier_or_keyword(token: SyntaxKind) -> bool {
    (token as u16) >= (SyntaxKind::Identifier as u16)
}

// Go: checker/utilities.go:52 tokenIsIdentifierOrKeywordOrGreaterThan
pub fn token_is_identifier_or_keyword_or_greater_than(token: SyntaxKind) -> bool {
    token == SyntaxKind::GreaterThanToken || token_is_identifier_or_keyword(token)
}

// Go: checker/utilities.go:56 hasOverrideModifier
pub fn has_override_modifier(node: Node) -> bool {
    has_syntactic_modifier(node, ModifierFlags::OVERRIDE)
}

// Go: checker/utilities.go:60 hasAsyncModifier
pub fn has_async_modifier(node: Node) -> bool {
    has_syntactic_modifier(node, ModifierFlags::ASYNC)
}

// Go: checker/utilities.go:64 getSelectedModifierFlags
pub fn get_selected_modifier_flags(node: Node, flags: ModifierFlags) -> ModifierFlags {
    node.modifier_flags() & flags
}

// Go: checker/utilities.go:68 hasReadonlyModifier
pub fn has_readonly_modifier(node: Node) -> bool {
    has_modifier(node, ModifierFlags::READONLY)
}

impl Checker {
    // Go: checker/utilities.go:72 isStaticPrivateIdentifierProperty
    pub fn is_static_private_identifier_property(&self, s: SymbolId) -> bool {
        let sym = self.sym(s);
        // PERF: the binder names the symbol of a private-identifier class
        // element `INTERNAL_SYMBOL_NAME_PREFIX` + "#..." (or
        // `INTERNAL_SYMBOL_NAME_MISSING` outside a class, or "default" when
        // the element has a `default` modifier, see `declare_symbol_ex`).
        // Checker copies (instantiated, transient, merged, union and spread
        // properties) keep the name of a symbol that has that value
        // declaration. Any other name gives false without a declaration load.
        // PERF: effect P7-2. Both tests read only the name id: the id holds
        // the internal bit, and "default" has a fixed id.
        if !sym.name.is_internal() && !sym.name.is_default_symbol_name() {
            return false;
        }
        let value_declaration = sym.value_declaration;
        value_declaration.is_some()
            && is_private_identifier_class_element_declaration(value_declaration)
            && is_static(value_declaration)
    }
}

// Go: checker/utilities.go:76 isEmptyObjectLiteral
// PORT: renamed; `is_empty_object_literal` is `ast.IsEmptyObjectLiteral` (same body).
pub fn checker_is_empty_object_literal(expression: Node) -> bool {
    is_object_literal_expression(expression) && expression.properties().len() == 0
}

// PORT: Go `type AssignmentKind int32` with its consts is `flags::AssignmentKind`
// (`AssignmentKind::NONE`, `DEFINITE`, `COMPOUND`). Go `AssignmentTarget` is a
// `*ast.Node` alias, so it is `Node`.

// Go: checker/utilities.go:90 getAssignmentTargetKind
pub fn get_assignment_target_kind(node: Node) -> AssignmentKind {
    let target = get_assignment_target(node);
    if target.is_nil() {
        return AssignmentKind::NONE;
    }
    match target.kind() {
        SyntaxKind::BinaryExpression => {
            let binary_operator = target.operator_token().kind();
            if binary_operator == SyntaxKind::EqualsToken
                || is_logical_or_coalescing_assignment_operator(binary_operator)
            {
                return AssignmentKind::DEFINITE;
            }
            return AssignmentKind::COMPOUND;
        }
        SyntaxKind::PrefixUnaryExpression | SyntaxKind::PostfixUnaryExpression => {
            return AssignmentKind::COMPOUND;
        }
        SyntaxKind::ForInStatement | SyntaxKind::ForOfStatement => {
            return AssignmentKind::DEFINITE;
        }
        _ => {}
    }
    panic!("Unhandled case in getAssignmentTargetKind")
}

// Go: checker/utilities.go:110 isDeleteTarget
pub fn is_delete_target(node: Node) -> bool {
    if !is_access_expression(node) {
        return false;
    }
    let node = walk_up_parenthesized_expressions(node.parent());
    node.is_some() && node.kind() == SyntaxKind::DeleteExpression
}

// Go: checker/utilities.go:118 isInCompoundLikeAssignment
pub fn is_in_compound_like_assignment(node: Node) -> bool {
    let target = get_assignment_target(node);
    target.is_some()
        && is_assignment_expression(target, true /*excludeCompoundAssignment*/)
        && is_compound_like_assignment(target)
}

// Go: checker/utilities.go:123 isCompoundLikeAssignment
pub fn is_compound_like_assignment(assignment: Node) -> bool {
    let right = skip_parentheses(assignment.right());
    right.kind() == SyntaxKind::BinaryExpression
        && checker_is_shift_operator_or_higher(right.operator_token().kind())
}

// Go: checker/utilities.go:128 isConstTypeReference
// PORT: renamed; `is_const_type_reference` is `ast.IsConstTypeReference` (same body).
pub fn checker_is_const_type_reference(node: Node) -> bool {
    is_type_reference_node(node)
        && node.type_arguments().len() == 0
        && is_identifier(node.type_name())
        && node.type_name().text() == "const"
}

// Go: checker/utilities.go:134 isConstTypeReferenceName
// isConstTypeReferenceName reports whether node is the `const` type name of a `const`
// assertion (`x as const` / `<const>x`), which must not be resolved as a real name.
pub fn is_const_type_reference_name(node: Node) -> bool {
    node.is_some()
        && is_identifier(node)
        && node.parent().is_some()
        && is_const_type_reference(node.parent())
        && node.parent().parent().is_some()
        && is_assertion_expression(node.parent().parent())
}

// Go: checker/utilities.go:144 isExportAssignmentExpressionName
// isExportAssignmentExpressionName reports whether node is (the root entity name of) the
// expression of an `export =` / `export default` assignment. Referencing a namespace or
// type-only name there is legal, and checkExportAssignment decides whether it is an error,
// so checkIdentifier must not report a value-usage error for it.
pub fn is_export_assignment_expression_name(node: Node) -> bool {
    if node.is_nil() {
        return false;
    }
    let mut current = node;
    while current.parent().is_some() && is_property_access_or_qualified_name(current.parent()) {
        current = current.parent();
    }
    current.parent().is_some()
        && is_export_assignment(current.parent())
        && current.parent().expression() == current
}

// Go: checker/utilities.go:132 GetSingleVariableOfVariableStatement
pub fn get_single_variable_of_variable_statement(node: Node) -> Node {
    if !is_variable_statement(node) {
        return Node::NIL;
    }
    let declarations = node.declaration_list().declarations().nodes();
    if declarations.is_empty() {
        return Node::NIL;
    }
    declarations.get(0)
}

// Go: checker/utilities.go:139 isTypeReferenceIdentifier
pub fn is_type_reference_identifier(mut node: Node) -> bool {
    while node.parent().kind() == SyntaxKind::QualifiedName {
        node = node.parent();
    }
    is_type_reference_node(node.parent())
}

// Go: checker/utilities.go:146 IsInTypeQuery
pub fn is_in_type_query(node: Node) -> bool {
    // TypeScript 1.0 spec (April 2014): 3.6.3
    // A type query consists of the keyword typeof followed by an expression.
    // The expression is restricted to a single identifier or a sequence of identifiers separated by periods
    find_ancestor_or_quit(node, |n: Node| match n.kind() {
        SyntaxKind::TypeQuery => FindAncestorResult::FIND_ANCESTOR_TRUE,
        SyntaxKind::Identifier | SyntaxKind::QualifiedName => {
            FindAncestorResult::FIND_ANCESTOR_FALSE
        }
        _ => FindAncestorResult::FIND_ANCESTOR_QUIT,
    })
    .is_some()
}

// Go: checker/utilities.go:160 canHaveLocals
pub fn can_have_locals(node: Node) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::ArrowFunction
            | SyntaxKind::Block
            | SyntaxKind::CallSignature
            | SyntaxKind::CaseBlock
            | SyntaxKind::CatchClause
            | SyntaxKind::ClassStaticBlockDeclaration
            | SyntaxKind::ConditionalType
            | SyntaxKind::Constructor
            | SyntaxKind::ConstructorType
            | SyntaxKind::ConstructSignature
            | SyntaxKind::ForStatement
            | SyntaxKind::ForInStatement
            | SyntaxKind::ForOfStatement
            | SyntaxKind::FunctionDeclaration
            | SyntaxKind::FunctionExpression
            | SyntaxKind::FunctionType
            | SyntaxKind::GetAccessor
            | SyntaxKind::IndexSignature
            | SyntaxKind::JsDocSignature
            | SyntaxKind::MappedType
            | SyntaxKind::MethodDeclaration
            | SyntaxKind::MethodSignature
            | SyntaxKind::ModuleDeclaration
            | SyntaxKind::SetAccessor
            | SyntaxKind::SourceFile
            | SyntaxKind::TypeAliasDeclaration
            | SyntaxKind::JsTypeAliasDeclaration
    )
}

impl Checker {
    // Go: checker/utilities.go:174 isShorthandAmbientModuleSymbol
    pub fn is_shorthand_ambient_module_symbol(&self, module_symbol: SymbolId) -> bool {
        is_shorthand_ambient_module(self.sym(module_symbol).value_declaration)
    }
}

// Go: checker/utilities.go:178 isShorthandAmbientModule
pub fn is_shorthand_ambient_module(node: Node) -> bool {
    // The only kind of module that can be missing a body is a shorthand ambient module.
    node.is_some() && node.kind() == SyntaxKind::ModuleDeclaration && node.body().is_nil()
}

// Go: checker/utilities.go:183 getAliasDeclarationFromName
pub fn get_alias_declaration_from_name(node: Node) -> Node {
    match node.parent().kind() {
        SyntaxKind::ImportClause
        | SyntaxKind::ImportSpecifier
        | SyntaxKind::NamespaceImport
        | SyntaxKind::ExportSpecifier
        | SyntaxKind::ExportAssignment
        | SyntaxKind::ImportEqualsDeclaration
        | SyntaxKind::NamespaceExport => node.parent(),
        SyntaxKind::QualifiedName => get_alias_declaration_from_name(node.parent()),
        _ => Node::NIL,
    }
}

// Go: checker/utilities.go:195 entityNameToString
// PORT: renamed; `entity_name_to_string` is `ast.EntityNameToString(name, getTextOfNode)`.
pub fn checker_entity_name_to_string(name: Node) -> String {
    entity_name_to_string(name, Some(&get_text_of_node))
}

// Go: checker/utilities.go:199 getContainingQualifiedNameNode
pub fn get_containing_qualified_name_node(mut node: Node) -> Node {
    while is_qualified_name(node.parent()) {
        node = node.parent();
    }
    node
}

// Go: checker/utilities.go:206 isSideEffectImport
pub fn is_side_effect_import(node: Node) -> bool {
    let ancestor = find_ancestor(node, is_import_declaration);
    ancestor.is_some() && ancestor.import_clause().is_nil()
}

// Go: checker/utilities.go:211 getExternalModuleRequireArgument
pub fn get_external_module_require_argument(node: Node) -> Node {
    if is_variable_declaration_initialized_to_require(node) {
        return node.initializer().arguments().get(0);
    }
    Node::NIL
}

// Go: checker/utilities.go:218 isRightSideOfAccessExpression
pub fn is_right_side_of_access_expression(node: Node) -> bool {
    node.parent().is_some()
        && (is_property_access_expression(node.parent()) && node.parent().name() == node
            || is_element_access_expression(node.parent())
                && node.parent().argument_expression() == node)
}

// Go: checker/utilities.go:223 isTopLevelInExternalModuleAugmentation
pub fn is_top_level_in_external_module_augmentation(node: Node) -> bool {
    node.is_some()
        && node.parent().is_some()
        && is_module_block(node.parent())
        && is_external_module_augmentation(node.parent().parent())
}

// Go: checker/utilities.go:227 isSyntacticDefault
pub fn is_syntactic_default(node: Node) -> bool {
    (is_export_assignment(node) && !node.is_export_equals())
        || has_syntactic_modifier(node, ModifierFlags::DEFAULT)
        || is_export_specifier(node)
        || is_namespace_export(node)
}

impl Checker {
    // Go: checker/utilities.go:234 hasExportAssignmentSymbol
    pub fn has_export_assignment_symbol(&self, module_symbol: SymbolId) -> bool {
        self.symbols
            .get(
                self.sym(module_symbol).exports,
                INTERNAL_SYMBOL_NAME_EXPORT_EQUALS,
            )
            .is_some()
    }
}

// Go: checker/utilities.go:238 isTypeAlias
pub fn is_type_alias(node: Node) -> bool {
    is_type_or_js_type_alias_declaration(node)
}

// Go: checker/utilities.go:242 hasOnlyExpressionInitializer
pub fn has_only_expression_initializer(node: Node) -> bool {
    matches!(
        node.kind(),
        SyntaxKind::VariableDeclaration
            | SyntaxKind::Parameter
            | SyntaxKind::BindingElement
            | SyntaxKind::PropertyDeclaration
            | SyntaxKind::PropertyAssignment
            | SyntaxKind::EnumMember
    )
}

// Go: checker/utilities.go:250 hasDotDotDotToken
pub fn has_dot_dot_dot_token(node: Node) -> bool {
    match node.kind() {
        SyntaxKind::Parameter => node.dot_dot_dot_token().is_some(),
        SyntaxKind::BindingElement => node.dot_dot_dot_token().is_some(),
        SyntaxKind::NamedTupleMember => node.dot_dot_dot_token().is_some(),
        SyntaxKind::JsxExpression => node.dot_dot_dot_token().is_some(),
        _ => false,
    }
}

impl Checker {
    // Go: checker/utilities.go:264 IsTypeAny
    pub fn is_type_any(&self, t: TypeId) -> bool {
        t.is_some() && self.ty(t).flags.intersects(TypeFlags::ANY)
    }
}

// Go: checker/utilities.go:268 isJSDocOptionalParameter
pub fn is_js_doc_optional_parameter(node: Node) -> bool {
    false // !!!
}

// Go: checker/utilities.go:272 isExclamationToken
pub fn is_exclamation_token(node: Node) -> bool {
    node.is_some() && node.kind() == SyntaxKind::ExclamationToken
}

// Go: checker/utilities.go:276 isOptionalDeclaration
pub fn is_optional_declaration(declaration: Node) -> bool {
    has_question_token(declaration)
}

impl Checker {
    // Go: checker/utilities.go:280 isOptionalParameter
    pub fn is_optional_parameter(&mut self, node: Node) -> bool {
        // !!! TODO: JSDoc support
        if is_parameter_declaration(node) && node.question_token().is_some() {
            return true;
        }
        if !is_parameter_declaration(node) {
            return false;
        }
        if node.initializer().is_some() {
            let signature = self.get_signature_from_declaration(node.parent());
            let parameter_index = find_parameter_index(node);
            debug_assert!(parameter_index >= 0);
            // Only consider syntactic or instantiated parameters as optional, not `void` parameters as this function is used
            // in grammar checks and checking for `void` too early results in parameter types widening too early
            // and causes some noImplicitAny errors to be lost.
            return parameter_index
                >= self.get_min_argument_count_ex(
                    signature,
                    MinArgumentCountFlags::STRONG_ARITY_FOR_UNTYPED_JS
                        | MinArgumentCountFlags::VOID_IS_NON_OPTIONAL,
                );
        }
        let iife = get_immediately_invoked_function_expression(node.parent());
        if iife.is_some() {
            let parameter_index = find_parameter_index(node);
            return node.type_().is_nil()
                && node.dot_dot_dot_token().is_nil()
                && parameter_index >= self.get_effective_call_arguments(iife).len() as i32;
        }
        false
    }
}

// PORT: Go `core.FindIndex(node.Parent.Parameters(), func(p) bool { return p == node })`
// used twice in isOptionalParameter. Returns -1 when not found, like Go.
fn find_parameter_index(node: Node) -> i32 {
    let parameters = node.parent().parameters();
    for i in 0..parameters.len() {
        if parameters.get(i) == node {
            return i as i32;
        }
    }
    -1
}

// Go: checker/utilities.go:307 isEmptyArrayLiteral
// PORT: renamed; `is_empty_array_literal` is `ast.IsEmptyArrayLiteral` (same body).
pub fn checker_is_empty_array_literal(expression: Node) -> bool {
    is_array_literal_expression(expression) && expression.elements().len() == 0
}

// Go: checker/utilities.go:311 declarationBelongsToPrivateAmbientMember
// PORT: Go also has a `Checker` method with this name (checker.go:17940,
// ported in checker_p20.rs); methods and free functions do not clash in Rust.
pub fn declaration_belongs_to_private_ambient_member(declaration: Node) -> bool {
    let root = get_root_declaration(declaration);
    let mut member_declaration = root;
    if root.kind() == SyntaxKind::Parameter {
        member_declaration = root.parent();
    }
    is_private_within_ambient(member_declaration)
}

// Go: checker/utilities.go:320 isPrivateWithinAmbient
pub fn is_private_within_ambient(node: Node) -> bool {
    (has_modifier(node, ModifierFlags::PRIVATE)
        || is_private_identifier_class_element_declaration(node))
        && !node.parser_flags(NodeFlags::AMBIENT).is_empty()
}

// Go: checker/utilities.go:324 isTypeAssertion
// PORT: renamed; `is_type_assertion` is `ast.IsTypeAssertion`, which only
// checks for `KindTypeAssertionExpression`. This checker function differs:
// it skips parentheses and accepts any assertion expression.
pub fn checker_is_type_assertion(node: Node) -> bool {
    is_assertion_expression(skip_parentheses(node))
}

impl Checker {
    // Go: checker/utilities.go:328 createSymbolTable
    pub fn create_symbol_table(&mut self, symbols: &[SymbolId]) -> SymbolTable {
        if symbols.is_empty() {
            return SymbolTable::NIL;
        }
        let result = self.symbols.new_table_with_capacity(symbols.len());
        for &symbol in symbols {
            let name = self.sym(symbol).name.clone();
            self.symbols.set(result, name, symbol);
        }
        result
    }

    // Go: checker/utilities.go:339 sortSymbols
    // PORT: Go sorts with `c.compareSymbols`, which is always
    // `c.compareSymbolsWorker` ("closure optimization"), so this needs only
    // `&self`. Each symbol's first declaration, file index and position are
    // read once into a `SymbolSortKey`; `compare_symbol_sort_keys` is
    // `compareSymbolsWorker` on those cached values.
    pub fn sort_symbols(&self, symbols: &mut [SymbolId]) {
        if symbols.len() < 2 {
            return;
        }
        // Consecutive symbols mostly come from one file, so the last file
        // index lookup is reused.
        let mut last_file = (Node::NIL, 0);
        let mut keys: Vec<SymbolSortKey> = symbols
            .iter()
            .map(|&s| self.symbol_sort_key(s, &mut last_file))
            .collect();
        self.sort_symbol_sort_keys(&mut keys);
        for (slot, key) in symbols.iter_mut().zip(&keys) {
            *slot = key.symbol;
        }
    }

    /// `sort_symbols` on keys that are already built. `get_named_members`
    /// uses it with its reusable key buffer. The sort and the comparator are
    /// the ones `sort_symbols` uses, so the comparisons (and the lazy
    /// `get_symbol_id` calls in them) happen in the same order.
    // Go: checker/utilities.go:363 slices.SortFunc(symbols, c.compareSymbols)
    // PORT: the comparator is not a total order. A file that is not in
    // `file_index_map` reads as index 0, as in Go, so it ties with the file at
    // index 0 and the order is not transitive (the auto-import registry
    // checks package files that are not in the program). Rust std `sort_by`
    // can panic on that; `gostd::slices::sort_func` is Go's pdqsort, which
    // does not, and gives Go's order (and Go's order of the `get_symbol_id`
    // calls) for any comparator.
    pub(crate) fn sort_symbol_sort_keys(&self, keys: &mut [SymbolSortKey]) {
        crate::gostd::slices::sort_func(keys, |a, b| {
            // PERF: two different packed orders give the sign that the full
            // comparator gets from `compare_nodes` (see `SymbolSortKey::order`),
            // and the full comparator returns there before its name and id
            // steps. So every comparison has the same sign, the sort makes the
            // same comparisons, and `get_symbol_id` runs in the same order.
            if a.order != b.order && a.order != NO_SORT_ORDER && b.order != NO_SORT_ORDER {
                let r = a.order.cmp(&b.order);
                debug_assert_eq!(r, self.compare_symbol_sort_keys(a, b).cmp(&0));
                return r as i32;
            }
            self.compare_symbol_sort_keys(a, b)
        });
    }

    /// The `compareSymbolsWorker` inputs of one symbol.
    /// `last_file` caches the last `(file, file_index_map[file])` lookup,
    /// with -1 for a file that is not in the map.
    // PERF: always inlined into `get_named_members` and `sort_symbols`, so
    // the key is written straight into its `Vec` slot. Out of line, the
    // callee stored the 48-byte key through the return pointer and the
    // caller read it back with wider loads that could not forward.
    #[inline(always)]
    pub(crate) fn symbol_sort_key(
        &self,
        symbol: SymbolId,
        last_file: &mut (Node, i32),
    ) -> SymbolSortKey {
        if symbol.is_nil() {
            return SymbolSortKey {
                symbol,
                has_declaration: false,
                declaration: Node::NIL,
                file: Node::NIL,
                file_index: 0,
                pos: 0,
                order: NO_SORT_ORDER,
                name: Name::default(),
            };
        }
        let sym = self.sym(symbol);
        let has_declaration = !sym.declarations.is_empty();
        let declaration = sym.declarations.first().copied().unwrap_or(Node::NIL);
        let (file, file_index, pos, order) = if declaration.is_some() {
            let file = get_source_file_of_node(declaration);
            if file != last_file.0 || file.is_nil() {
                *last_file = (file, self.file_index_map.get(&file).copied().unwrap_or(-1));
            }
            let pos = declaration.pos();
            if last_file.1 >= 0 {
                // The sign bit flip maps `i32` order onto `u32` order.
                let order =
                    (u64::from(last_file.1 as u32) << 32) | u64::from(pos as u32 ^ 0x8000_0000);
                (file, last_file.1, pos, order)
            } else {
                // A map miss reads as index 0 (Go map zero value).
                (file, 0, pos, NO_SORT_ORDER)
            }
        } else {
            (Node::NIL, 0, 0, NO_SORT_ORDER)
        };
        SymbolSortKey {
            symbol,
            has_declaration,
            declaration,
            file,
            file_index,
            pos,
            order,
            name: sym.name.clone(),
        }
    }

    /// `compare_symbols_worker` on cached keys. The symbol id fallback stays
    /// lazy so ids are assigned in the same order as before.
    fn compare_symbol_sort_keys(&self, k1: &SymbolSortKey, k2: &SymbolSortKey) -> i32 {
        if k1.symbol == k2.symbol {
            return 0;
        }
        if k1.symbol.is_nil() {
            return 1;
        }
        if k2.symbol.is_nil() {
            return -1;
        }
        if k1.has_declaration && k2.has_declaration {
            // compare_nodes
            let r = if k1.declaration == k2.declaration {
                0
            } else if k1.declaration.is_nil() {
                1
            } else if k2.declaration.is_nil() {
                -1
            } else if k1.file != k2.file {
                k1.file_index - k2.file_index
            } else {
                k1.pos - k2.pos
            };
            if r != 0 {
                return r;
            }
        } else if k1.has_declaration {
            return -1;
        } else if k2.has_declaration {
            return 1;
        }
        // Equal ids are equal texts, which compare as 0.
        if k1.name != k2.name {
            let r = compare_strings(&k1.name, &k2.name);
            if r != 0 {
                return r;
            }
        }
        let id1 = get_symbol_id(&self.symbols, k1.symbol) as i64;
        let id2 = get_symbol_id(&self.symbols, k2.symbol) as i64;
        clamp_compare(id1 - id2)
    }

    // Go: checker/utilities.go:343 compareSymbolsWorker
    pub fn compare_symbols_worker(&self, s1: SymbolId, s2: SymbolId) -> i32 {
        if s1 == s2 {
            return 0;
        }
        if s1.is_nil() {
            return 1;
        }
        if s2.is_nil() {
            return -1;
        }
        let sym1 = self.sym(s1);
        let sym2 = self.sym(s2);
        if !sym1.declarations.is_empty() && !sym2.declarations.is_empty() {
            let r = self.compare_nodes(sym1.declarations[0], sym2.declarations[0]);
            if r != 0 {
                return r;
            }
        } else if !sym1.declarations.is_empty() {
            return -1;
        } else if !sym2.declarations.is_empty() {
            return 1;
        }
        let r = compare_strings(&sym1.name, &sym2.name);
        if r != 0 {
            return r;
        }
        // Fall back to symbol IDs. This is a last resort that should happen only when symbols have
        // no declaration and duplicate names.
        let id1 = get_symbol_id(&self.symbols, s1) as i64;
        let id2 = get_symbol_id(&self.symbols, s2) as i64;
        clamp_compare(id1 - id2)
    }

    // Go: checker/utilities.go:369 compareNodes
    pub fn compare_nodes(&self, n1: Node, n2: Node) -> i32 {
        if n1 == n2 {
            return 0;
        }
        if n1.is_nil() {
            return 1;
        }
        if n2.is_nil() {
            return -1;
        }
        let s1 = get_source_file_of_node(n1);
        let s2 = get_source_file_of_node(n2);
        if s1 != s2 {
            let f1 = self.file_index_map.get(&s1).copied().unwrap_or(0);
            let f2 = self.file_index_map.get(&s2).copied().unwrap_or(0);
            // Order by index of file in the containing program
            return f1 - f2;
        }
        // In the same file, order by source position
        n1.pos() - n2.pos()
    }
}

/// Cached `compareSymbolsWorker` inputs for `sort_symbols`.
pub(crate) struct SymbolSortKey {
    pub(crate) symbol: SymbolId,
    has_declaration: bool,
    /// First declaration, or nil.
    declaration: Node,
    /// Source file of `declaration`.
    file: Node,
    /// `file_index_map[file]`, zero when absent (Go map miss).
    file_index: i32,
    pos: i32,
    /// `(file_index, pos)` packed so that `u64` order is `(file_index, pos)`
    /// order, or `NO_SORT_ORDER` when there is no declaration or its file is
    /// not in `file_index_map`. Map indexes are unique and not negative, so
    /// two different orders mean `compare_nodes` returns nonzero with the
    /// same sign: `file_index` difference for different files, `pos`
    /// difference in one file. Equal orders (same declaration, or the same
    /// position) fall through to the full comparator.
    pub(crate) order: u64,
    /// Its text is read only when the declarations tie.
    name: Name,
}

/// `SymbolSortKey::order` when the packed order does not apply. A real
/// order is below 2^63.
pub(crate) const NO_SORT_ORDER: u64 = u64::MAX;

// PORT: Go `strings.Compare` (byte order, returns -1/0/1). It compares the
// Go bytes, so lone surrogates and the internal symbol name prefix sort as in
// Go (see `compare_go_strings`).
fn compare_strings(a: &str, b: &str) -> i32 {
    match crate::scanner_util::compare_go_strings(a, b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

// PORT: Go computes these differences as 64-bit `int`. Callers only use the
// sign, so a difference that does not fit in `i32` is clamped (sign kept).
fn clamp_compare(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

// PORT: Go `cmp.Compare` on `jsnum.Number` (a float64): NaN is less than any
// other value and equal to NaN; -0 equals +0.
fn compare_numbers(a: crate::jsnum::Number, b: crate::jsnum::Number) -> i32 {
    let (x, y) = (a.0, b.0);
    let x_nan = x.is_nan();
    let y_nan = y.is_nan();
    if x_nan {
        if y_nan {
            return 0;
        }
        return -1;
    }
    if y_nan {
        return 1;
    }
    if x < y {
        return -1;
    }
    if x > y {
        return 1;
    }
    0
}

impl Checker {
    // Go: checker/utilities.go:392 CompareTypes
    // PORT: Go panics when the types come from different checkers; one
    // checker owns all `TypeId`s here, so that check is dropped. Go calls
    // `t1.checker.compareSymbols`, which is always `compareSymbolsWorker`, so
    // this calls the worker directly and needs only `&self`.
    pub fn compare_types(&self, t1: TypeId, t2: TypeId) -> i32 {
        if t1 == t2 {
            return 0;
        }
        if t1.is_nil() {
            return -1;
        }
        if t2.is_nil() {
            return 1;
        }
        let ty1 = self.ty(t1);
        let ty2 = self.ty(t2);
        // First sort in order of increasing type flags values.
        let c = clamp_compare(get_sort_order_flags(ty1) - get_sort_order_flags(ty2));
        if c != 0 {
            return c;
        }
        // Order named types by name and, in the case of aliased types, by alias type arguments.
        // PERF: chkA. The two types are passed as read (`compare_type_names_of`).
        let c = self.compare_type_names_of(ty1, ty2);
        if c != 0 {
            return c;
        }
        // We have unnamed types or types with identical names. Now sort by data specific to the type.
        if ty1.flags.intersects(
            TypeFlags::ANY
                | TypeFlags::UNKNOWN
                | TypeFlags::STRING
                | TypeFlags::NUMBER
                | TypeFlags::BOOLEAN
                | TypeFlags::BIG_INT
                | TypeFlags::ES_SYMBOL
                | TypeFlags::VOID
                | TypeFlags::UNDEFINED
                | TypeFlags::NULL
                | TypeFlags::NEVER
                | TypeFlags::NON_PRIMITIVE,
        ) {
            // Only distinguished by type IDs, handled below.
        } else if ty1.flags.intersects(TypeFlags::OBJECT) {
            // Order instantiation expression types without relying on lazy symbol IDs.
            // Order other unnamed or identically named object types by symbol.
            if ty1
                .object_flags
                .intersects(ObjectFlags::INSTANTIATION_EXPRESSION_TYPE)
                && ty2
                    .object_flags
                    .intersects(ObjectFlags::INSTANTIATION_EXPRESSION_TYPE)
            {
                let mut declaration1 = Node::NIL;
                let mut declaration2 = Node::NIL;
                if ty1.symbol.is_some() && !self.sym(ty1.symbol).declarations.is_empty() {
                    declaration1 = self.sym(ty1.symbol).declarations[0];
                }
                if ty2.symbol.is_some() && !self.sym(ty2.symbol).declarations.is_empty() {
                    declaration2 = self.sym(ty2.symbol).declarations[0];
                }
                // A single instantiation expression can produce multiple types for union constituents,
                // so compare their source declarations before comparing the shared expression node.
                let c = self.compare_nodes(declaration1, declaration2);
                if c != 0 {
                    return c;
                }
                let c = self.compare_nodes(
                    ty1.as_instantiation_expression_type().node,
                    ty2.as_instantiation_expression_type().node,
                );
                if c != 0 {
                    return c;
                }
            } else {
                let c = self.compare_symbols_worker(ty1.symbol, ty2.symbol);
                if c != 0 {
                    return c;
                }
            }
            // When object types have the same or no symbol, order by kind. We order type references before other kinds.
            if ty1.object_flags.intersects(ObjectFlags::REFERENCE)
                && ty2.object_flags.intersects(ObjectFlags::REFERENCE)
            {
                let r1 = ty1.as_type_reference();
                let r2 = ty2.as_type_reference();
                let r1_target = r1.object.target;
                let r2_target = r2.object.target;
                if self
                    .ty(r1_target)
                    .object_flags
                    .intersects(ObjectFlags::TUPLE)
                    && self
                        .ty(r2_target)
                        .object_flags
                        .intersects(ObjectFlags::TUPLE)
                {
                    // Tuple types have no associated symbol, instead we order by tuple element information.
                    let c = compare_tuple_types(
                        self.ty(r1_target).as_tuple_type(),
                        self.ty(r2_target).as_tuple_type(),
                    );
                    if c != 0 {
                        return c;
                    }
                }
                // Here we know we have references to instantiations of the same type because we have matching targets.
                if r1.node.is_nil() && r2.node.is_nil() {
                    // Non-deferred type references with the same target are sorted by their type argument lists.
                    let c = self.compare_type_lists(
                        &r1.resolved_type_arguments,
                        &r2.resolved_type_arguments,
                    );
                    if c != 0 {
                        return c;
                    }
                } else {
                    // Deferred type references with the same target are ordered by the source location of the reference.
                    let c = self.compare_nodes(r1.node, r2.node);
                    if c != 0 {
                        return c;
                    }
                    // Instantiations of the same deferred type reference are ordered by their associated type mappers
                    // (which reflect the mapping of in-scope type parameters to type arguments).
                    let c = self.compare_type_mappers(
                        ty1.as_object_type().mapper,
                        ty2.as_object_type().mapper,
                    );
                    if c != 0 {
                        return c;
                    }
                }
            } else if ty1.object_flags.intersects(ObjectFlags::REFERENCE) {
                return -1;
            } else if ty2.object_flags.intersects(ObjectFlags::REFERENCE) {
                return 1;
            } else {
                // Order unnamed non-reference object types by kind and instantiation data.
                let k1 = i64::from((ty1.object_flags & ObjectFlags::OBJECT_TYPE_KIND_MASK).0);
                let k2 = i64::from((ty2.object_flags & ObjectFlags::OBJECT_TYPE_KIND_MASK).0);
                let c = clamp_compare(k1 - k2);
                if c != 0 {
                    return c;
                }
                if ty1.object_flags.intersects(ObjectFlags::REVERSE_MAPPED) {
                    let r1 = ty1.as_reverse_mapped_type();
                    let r2 = ty2.as_reverse_mapped_type();
                    let c = self.compare_types(r1.source, r2.source);
                    if c != 0 {
                        return c;
                    }
                    let c = self.compare_types(r1.mapped_type, r2.mapped_type);
                    if c != 0 {
                        return c;
                    }
                    let c = self.compare_types(r1.constraint_type, r2.constraint_type);
                    if c != 0 {
                        return c;
                    }
                }
                let mut m1 = ty1.as_object_type().mapper;
                let mut m2 = ty2.as_object_type().mapper;
                if ty1.object_flags.intersects(ObjectFlags::MAPPED) {
                    // instantiateAnonymousType prepends a fresh type parameter mapping.
                    // Compare the effective instantiation, not the identity of that fresh parameter.
                    // PORT: Go `m.data.(*CompositeTypeMapper).m2` panics for any other mapper.
                    let composite_m2 = |m: MapperId| match self.mapper(m) {
                        TypeMapper::Composite(d) => d.m2,
                        _ => panic!("interface conversion: not *checker.CompositeTypeMapper"),
                    };
                    if m1.is_some() {
                        m1 = composite_m2(m1);
                    }
                    if m2.is_some() {
                        m2 = composite_m2(m2);
                    }
                }
                let c = self.compare_type_mappers(m1, m2);
                if c != 0 {
                    return c;
                }
            }
        } else if ty1.flags.intersects(TypeFlags::UNION) {
            // Unions are ordered by origin and then constituent type lists.
            let o1 = ty1.as_union_type().origin;
            let o2 = ty2.as_union_type().origin;
            if o1.is_nil() && o2.is_nil() {
                let c = self.compare_type_lists(ty1.types(), ty2.types());
                if c != 0 {
                    return c;
                }
            } else if o1.is_nil() {
                return 1;
            } else if o2.is_nil() {
                return -1;
            } else {
                let c = self.compare_types(o1, o2);
                if c != 0 {
                    return c;
                }
            }
        } else if ty1.flags.intersects(TypeFlags::INTERSECTION) {
            // Intersections are ordered by their constituent type lists.
            let c = self.compare_type_lists(ty1.types(), ty2.types());
            if c != 0 {
                return c;
            }
        } else if ty1
            .flags
            .intersects(TypeFlags::ENUM | TypeFlags::ENUM_LITERAL | TypeFlags::UNIQUE_ES_SYMBOL)
        {
            // Enum members are ordered by their symbol (and thus their declaration order).
            let c = self.compare_symbols_worker(ty1.symbol, ty2.symbol);
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::STRING_LITERAL) {
            // String literal types are ordered by their values.
            let c = compare_strings(literal_string_value(ty1), literal_string_value(ty2));
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::NUMBER_LITERAL) {
            // Numeric literal types are ordered by their values.
            let c = compare_numbers(literal_number_value(ty1), literal_number_value(ty2));
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::BIG_INT_LITERAL) {
            let c = self
                .get_big_int_literal_value(t1)
                .compare(&self.get_big_int_literal_value(t2));
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::BOOLEAN_LITERAL) {
            let b1 = literal_bool_value(ty1);
            let b2 = literal_bool_value(ty2);
            if b1 != b2 {
                if b1 {
                    return 1;
                }
                return -1;
            }
        } else if ty1.flags.intersects(TypeFlags::TYPE_PARAMETER) {
            let c = self.compare_symbols_worker(ty1.symbol, ty2.symbol);
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::INDEX) {
            let c = self.compare_types(ty1.as_index_type().target, ty2.as_index_type().target);
            if c != 0 {
                return c;
            }
            let c = clamp_compare(
                i64::from(ty1.as_index_type().index_flags.0)
                    - i64::from(ty2.as_index_type().index_flags.0),
            );
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::INDEXED_ACCESS) {
            let c = self.compare_types(
                ty1.as_indexed_access_type().object_type,
                ty2.as_indexed_access_type().object_type,
            );
            if c != 0 {
                return c;
            }
            let c = self.compare_types(
                ty1.as_indexed_access_type().index_type,
                ty2.as_indexed_access_type().index_type,
            );
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::CONDITIONAL) {
            let n1 = ty1.as_conditional_type().root.borrow().node;
            let n2 = ty2.as_conditional_type().root.borrow().node;
            let c = self.compare_nodes(n1, n2);
            if c != 0 {
                return c;
            }
            let c = self.compare_type_mappers(
                ty1.as_conditional_type().mapper,
                ty2.as_conditional_type().mapper,
            );
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::SUBSTITUTION) {
            let c = self.compare_types(
                ty1.as_substitution_type().base_type,
                ty2.as_substitution_type().base_type,
            );
            if c != 0 {
                return c;
            }
            let c = self.compare_types(
                ty1.as_substitution_type().constraint,
                ty2.as_substitution_type().constraint,
            );
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::TEMPLATE_LITERAL) {
            let c = compare_string_slices(
                &ty1.as_template_literal_type().texts,
                &ty2.as_template_literal_type().texts,
            );
            if c != 0 {
                return c;
            }
            let c = self.compare_type_lists(
                &ty1.as_template_literal_type().types,
                &ty2.as_template_literal_type().types,
            );
            if c != 0 {
                return c;
            }
        } else if ty1.flags.intersects(TypeFlags::STRING_MAPPING) {
            let c = self.compare_types(
                ty1.as_string_mapping_type().target,
                ty2.as_string_mapping_type().target,
            );
            if c != 0 {
                return c;
            }
        }
        // Fall back to type IDs. This results in type creation order for built-in types.
        clamp_compare(i64::from(ty1.id.0) - i64::from(ty2.id.0))
    }
}

// PORT: Go `t.AsLiteralType().value.(string)`; panics like the Go type assertion.
fn literal_string_value(t: &Type) -> &str {
    match t.as_literal_type().value.as_ref() {
        Some(LiteralValue::String(s)) => s,
        _ => panic!("interface conversion: interface {{}} is not string"),
    }
}

// PORT: Go `t.AsLiteralType().value.(jsnum.Number)`.
fn literal_number_value(t: &Type) -> crate::jsnum::Number {
    match t.as_literal_type().value.as_ref() {
        Some(LiteralValue::Number(n)) => *n,
        _ => panic!("interface conversion: interface {{}} is not jsnum.Number"),
    }
}

// PORT: Go `t.AsLiteralType().value.(bool)`.
fn literal_bool_value(t: &Type) -> bool {
    match t.as_literal_type().value.as_ref() {
        Some(LiteralValue::Bool(b)) => *b,
        _ => panic!("interface conversion: interface {{}} is not bool"),
    }
}

// PORT: Go `slices.Compare` on `[]string`: element-wise `strings.Compare`,
// then the shorter slice is less.
fn compare_string_slices(s1: &[String], s2: &[String]) -> i32 {
    for (a, b) in s1.iter().zip(s2.iter()) {
        let c = compare_strings(a, b);
        if c != 0 {
            return c;
        }
    }
    if s1.len() < s2.len() {
        return -1;
    }
    if s1.len() > s2.len() {
        return 1;
    }
    0
}

// Go: checker/utilities.go:557 getSortOrderFlags
// PORT: takes `&Type` (pure read of one type's flags); returns Go `int` as `i64`.
pub fn get_sort_order_flags(t: &Type) -> i64 {
    // Return TypeFlagsEnum for all enum-like unit types (they'll be sorted by their symbols)
    if t.flags
        .intersects(TypeFlags::ENUM_LITERAL | TypeFlags::ENUM)
        && !t.flags.intersects(TypeFlags::UNION)
    {
        return i64::from(TypeFlags::ENUM.0);
    }
    i64::from(t.flags.0)
}

impl Checker {
    // Go: checker/utilities.go:565 compareTypeNames
    pub fn compare_type_names(&self, t1: TypeId, t2: TypeId) -> i32 {
        self.compare_type_names_of(self.ty(t1), self.ty(t2))
    }

    /// `compare_type_names` of two types already read from the arena.
    pub fn compare_type_names_of(&self, ty1: &Type, ty2: &Type) -> i32 {
        let s1 = type_name_symbol(ty1);
        let s2 = type_name_symbol(ty2);
        if s1 == s2 {
            return self.compare_type_lists(ty1.alias.type_arguments(), ty2.alias.type_arguments());
        }
        if s1.is_nil() {
            return 1;
        }
        if s2.is_nil() {
            return -1;
        }
        let (name1, name2) = (&self.sym(s1).name, &self.sym(s2).name);
        // Equal name ids are equal texts, which compare as 0.
        if name1 != name2 {
            let c = compare_strings(name1, name2);
            if c != 0 {
                return c;
            }
        }
        // Keep distinct same-named declarations together before comparing alias arguments or structure.
        // PORT: Go `t1.checker.compareSymbols` is always the worker (see `compare_types`).
        self.compare_symbols_worker(s1, s2)
    }

    // Go: checker/utilities.go:582 getTypeNameSymbol
    pub fn get_type_name_symbol(&self, t: TypeId) -> SymbolId {
        type_name_symbol(self.ty(t))
    }

    // Go: checker/utilities.go:592 getObjectTypeName
    pub fn get_object_type_name(&self, t: TypeId) -> SymbolId {
        let ty = self.ty(t);
        if ty
            .object_flags
            .intersects(ObjectFlags::CLASS_OR_INTERFACE | ObjectFlags::REFERENCE)
        {
            return ty.symbol;
        }
        SymbolId::NIL
    }
}

/// Go `getTypeNameSymbol` of a type already read from the arena.
fn type_name_symbol(ty: &Type) -> SymbolId {
    if let Some(alias) = &ty.alias {
        return alias.symbol;
    }
    if ty
        .flags
        .intersects(TypeFlags::TYPE_PARAMETER | TypeFlags::STRING_MAPPING)
        || ty
            .object_flags
            .intersects(ObjectFlags::CLASS_OR_INTERFACE | ObjectFlags::REFERENCE)
    {
        return ty.symbol;
    }
    SymbolId::NIL
}

// Go: checker/utilities.go:599 compareTupleTypes
// PORT: Go takes `*TupleType`; here `&TupleType` borrowed from the type arena.
// Go pointer equality is `std::ptr::eq`.
pub fn compare_tuple_types(t1: &TupleType, t2: &TupleType) -> i32 {
    if std::ptr::eq(t1, t2) {
        return 0;
    }
    if t1.readonly != t2.readonly {
        return if t1.readonly { 1 } else { -1 };
    }
    if t1.element_infos.len() != t2.element_infos.len() {
        return t1.element_infos.len() as i32 - t2.element_infos.len() as i32;
    }
    for i in 0..t1.element_infos.len() {
        let c = clamp_compare(
            i64::from(t1.element_infos[i].flags.0) - i64::from(t2.element_infos[i].flags.0),
        );
        if c != 0 {
            return c;
        }
    }
    for i in 0..t1.element_infos.len() {
        let c = compare_element_labels(
            t1.element_infos[i].labeled_declaration,
            t2.element_infos[i].labeled_declaration,
        );
        if c != 0 {
            return c;
        }
    }
    0
}

// Go: checker/utilities.go:621 compareElementLabels
pub fn compare_element_labels(n1: Node, n2: Node) -> i32 {
    if n1 == n2 {
        return 0;
    }
    if n1.is_nil() {
        return -1;
    }
    if n2.is_nil() {
        return 1;
    }
    compare_strings(n1.name().text(), n2.name().text())
}

impl Checker {
    // Go: checker/utilities.go:634 compareTypeLists
    pub fn compare_type_lists(&self, s1: &[TypeId], s2: &[TypeId]) -> i32 {
        if s1.len() != s2.len() {
            return s1.len() as i32 - s2.len() as i32;
        }
        for (i, &t1) in s1.iter().enumerate() {
            let c = self.compare_types(t1, s2[i]);
            if c != 0 {
                return c;
            }
        }
        0
    }

    // Go: checker/utilities.go:648 compareTypeMappers
    pub fn compare_type_mappers(&self, m1: MapperId, m2: MapperId) -> i32 {
        if m1 == m2 {
            return 0;
        }
        if m1.is_nil() {
            return 1;
        }
        if m2.is_nil() {
            return -1;
        }
        let kind1 = self.mapper(m1).kind();
        let kind2 = self.mapper(m2).kind();
        if kind1 != kind2 {
            return kind1 as i32 - kind2 as i32;
        }
        match (self.mapper(m1), self.mapper(m2)) {
            (TypeMapper::Simple(m1), TypeMapper::Simple(m2)) => {
                let c = self.compare_types(m1.source, m2.source);
                if c != 0 {
                    return c;
                }
                self.compare_types(m1.target, m2.target)
            }
            (TypeMapper::Array(m1), TypeMapper::Array(m2)) => {
                let c = self.compare_type_lists(&m1.sources, &m2.sources);
                if c != 0 {
                    return c;
                }
                self.compare_type_lists(&m1.targets, &m2.targets)
            }
            (TypeMapper::Merged(m1), TypeMapper::Merged(m2)) => {
                let (a1, a2, b1, b2) = (m1.m1, m1.m2, m2.m1, m2.m2);
                let c = self.compare_type_mappers(a1, b1);
                if c != 0 {
                    return c;
                }
                self.compare_type_mappers(a2, b2)
            }
            // PORT: Go switches on `kind1`; kinds other than Simple, Array and
            // Merged (all `TypeMapperKindUnknown`) fall through to `return 0`.
            _ => 0,
        }
    }

    // Go: checker/utilities.go:680 getDeclarationModifierFlagsFromSymbol
    pub fn get_declaration_modifier_flags_from_symbol(&self, s: SymbolId) -> ModifierFlags {
        self.get_declaration_modifier_flags_from_symbol_ex(s, false /*isWrite*/)
    }

    // Go: checker/utilities.go:684 getDeclarationModifierFlagsFromSymbolEx
    pub fn get_declaration_modifier_flags_from_symbol_ex(
        &self,
        s: SymbolId,
        is_write: bool,
    ) -> ModifierFlags {
        let sym = self.sym(s);
        if sym.check_flags.intersects(CheckFlags::SYNTHETIC) {
            let check_flags = sym.check_flags;
            let mut access_modifier = ModifierFlags::NONE;
            if !is_write && check_flags.intersects(CheckFlags::CONTAINS_PUBLIC)
                || is_write && check_flags.intersects(CheckFlags::CONTAINS_WRITE_PUBLIC)
            {
                access_modifier = ModifierFlags::PUBLIC;
            } else if !is_write && check_flags.intersects(CheckFlags::CONTAINS_PROTECTED)
                || is_write && check_flags.intersects(CheckFlags::CONTAINS_WRITE_PROTECTED)
            {
                access_modifier = ModifierFlags::PROTECTED;
            } else if !is_write && check_flags.intersects(CheckFlags::CONTAINS_PRIVATE)
                || is_write && check_flags.intersects(CheckFlags::CONTAINS_WRITE_PRIVATE)
            {
                access_modifier = ModifierFlags::PRIVATE;
            }
            if check_flags.intersects(CheckFlags::CONTAINS_STATIC) {
                return access_modifier | ModifierFlags::STATIC;
            }
            return access_modifier;
        }
        if sym.value_declaration.is_some() {
            let mut declaration = Node::NIL;
            if is_write {
                declaration = sym
                    .declarations
                    .iter()
                    .copied()
                    .find(|&d| is_set_accessor_declaration(d))
                    .unwrap_or(Node::NIL);
            }
            if declaration.is_nil() && sym.flags.intersects(SymbolFlags::GET_ACCESSOR) {
                declaration = sym
                    .declarations
                    .iter()
                    .copied()
                    .find(|&d| is_get_accessor_declaration(d))
                    .unwrap_or(Node::NIL);
            }
            if declaration.is_nil() {
                declaration = sym.value_declaration;
            }
            let flags = get_combined_modifier_flags(declaration);
            if sym.parent.is_some() && self.sym(sym.parent).flags.intersects(SymbolFlags::CLASS) {
                return flags;
            }
            return flags.without(ModifierFlags::ACCESSIBILITY_MODIFIER);
        }
        if sym.flags.intersects(SymbolFlags::PROTOTYPE) {
            return ModifierFlags::PUBLIC | ModifierFlags::STATIC;
        }
        ModifierFlags::NONE
    }
}

// PORT: the operator predicates below are renamed with a `checker_` prefix.
// `ast/fields.rs` already exports `is_exponentiation_operator`, ...,
// `is_binary_operator` (Go `ast.IsXxxOperator`, generated from the same kind
// sets), so the unprefixed names would be ambiguous through the prelude.

// Go: checker/utilities.go:1923 quotedAndCommaSeparated
pub fn quoted_and_comma_separated(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("'{item}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

// Go: checker/utilities.go:730 isExponentiationOperator
pub fn checker_is_exponentiation_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::AsteriskAsteriskToken
}

// Go: checker/utilities.go:734 isMultiplicativeOperator
pub fn checker_is_multiplicative_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::AsteriskToken
        || kind == SyntaxKind::SlashToken
        || kind == SyntaxKind::PercentToken
}

// Go: checker/utilities.go:738 isMultiplicativeOperatorOrHigher
pub fn checker_is_multiplicative_operator_or_higher(kind: SyntaxKind) -> bool {
    checker_is_exponentiation_operator(kind) || checker_is_multiplicative_operator(kind)
}

// Go: checker/utilities.go:742 isAdditiveOperator
pub fn checker_is_additive_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::PlusToken || kind == SyntaxKind::MinusToken
}

// Go: checker/utilities.go:746 isAdditiveOperatorOrHigher
pub fn checker_is_additive_operator_or_higher(kind: SyntaxKind) -> bool {
    checker_is_additive_operator(kind) || checker_is_multiplicative_operator_or_higher(kind)
}

// Go: checker/utilities.go:750 isShiftOperator
pub fn checker_is_shift_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::LessThanLessThanToken
        || kind == SyntaxKind::GreaterThanGreaterThanToken
        || kind == SyntaxKind::GreaterThanGreaterThanGreaterThanToken
}

// Go: checker/utilities.go:755 isShiftOperatorOrHigher
pub fn checker_is_shift_operator_or_higher(kind: SyntaxKind) -> bool {
    checker_is_shift_operator(kind) || checker_is_additive_operator_or_higher(kind)
}

// Go: checker/utilities.go:759 isRelationalOperator
pub fn checker_is_relational_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::LessThanToken
        || kind == SyntaxKind::LessThanEqualsToken
        || kind == SyntaxKind::GreaterThanToken
        || kind == SyntaxKind::GreaterThanEqualsToken
        || kind == SyntaxKind::InstanceOfKeyword
        || kind == SyntaxKind::InKeyword
}

// Go: checker/utilities.go:764 isRelationalOperatorOrHigher
pub fn checker_is_relational_operator_or_higher(kind: SyntaxKind) -> bool {
    checker_is_relational_operator(kind) || checker_is_shift_operator_or_higher(kind)
}

// Go: checker/utilities.go:768 isEqualityOperator
pub fn checker_is_equality_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::EqualsEqualsToken
        || kind == SyntaxKind::EqualsEqualsEqualsToken
        || kind == SyntaxKind::ExclamationEqualsToken
        || kind == SyntaxKind::ExclamationEqualsEqualsToken
}

// Go: checker/utilities.go:773 isEqualityOperatorOrHigher
pub fn checker_is_equality_operator_or_higher(kind: SyntaxKind) -> bool {
    checker_is_equality_operator(kind) || checker_is_relational_operator_or_higher(kind)
}

// Go: checker/utilities.go:777 isBitwiseOperator
pub fn checker_is_bitwise_operator(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::AmpersandToken
        || kind == SyntaxKind::BarToken
        || kind == SyntaxKind::CaretToken
}

// Go: checker/utilities.go:781 isBitwiseOperatorOrHigher
pub fn checker_is_bitwise_operator_or_higher(kind: SyntaxKind) -> bool {
    checker_is_bitwise_operator(kind) || checker_is_equality_operator_or_higher(kind)
}

// Go: checker/utilities.go:785 isLogicalOperatorOrHigher
pub fn checker_is_logical_operator_or_higher(kind: SyntaxKind) -> bool {
    is_logical_binary_operator(kind) || checker_is_bitwise_operator_or_higher(kind)
}

// Go: checker/utilities.go:789 isAssignmentOperatorOrHigher
pub fn checker_is_assignment_operator_or_higher(kind: SyntaxKind) -> bool {
    kind == SyntaxKind::QuestionQuestionToken
        || checker_is_logical_operator_or_higher(kind)
        || is_assignment_operator(kind)
}

// Go: checker/utilities.go:793 isBinaryOperator
pub fn checker_is_binary_operator(kind: SyntaxKind) -> bool {
    checker_is_assignment_operator_or_higher(kind) || kind == SyntaxKind::CommaToken
}

impl Checker {
    // Go: checker/utilities.go:797 isObjectLiteralType
    pub fn is_object_literal_type(&self, t: TypeId) -> bool {
        self.ty(t)
            .object_flags
            .intersects(ObjectFlags::OBJECT_LITERAL)
    }
}

// Go: checker/utilities.go:801 isDeclarationReadonly
pub fn is_declaration_readonly(declaration: Node) -> bool {
    get_combined_modifier_flags(declaration).intersects(ModifierFlags::READONLY)
        && !is_parameter_property_declaration(declaration, declaration.parent())
}

// orderedSetMapThreshold is the size at which an orderedSet materializes its dedup map.
// Below this, contains() scans the values slice.
// Go: checker/utilities.go:807 orderedSetMapThreshold
pub const ORDERED_SET_MAP_THRESHOLD: usize = 16;

// Go: checker/utilities.go:813 orderedSet
// PORT: Go nil map is `None`.
#[derive(Clone, Debug)]
pub struct OrderedSet<T: Eq + std::hash::Hash + Clone> {
    pub values_by_key: Option<FxHashSet<T>>,
    pub values: Vec<T>,
}

impl<T: Eq + std::hash::Hash + Clone> Default for OrderedSet<T> {
    fn default() -> Self {
        Self {
            values_by_key: None,
            values: Vec::new(),
        }
    }
}

impl<T: Eq + std::hash::Hash + Clone> OrderedSet<T> {
    // Go: checker/utilities.go:818 orderedSet.contains
    pub fn contains(&self, value: &T) -> bool {
        match &self.values_by_key {
            None => self.values.contains(value),
            Some(values_by_key) => values_by_key.contains(value),
        }
    }

    // Go: checker/utilities.go:826 orderedSet.add
    pub fn add(&mut self, value: T) {
        self.values.push(value.clone());
        // Small sets are served by a linear scan over values; only materialize the map once the set
        // grows large enough for hashing to win.
        if self.values_by_key.is_none() {
            if self.values.len() <= ORDERED_SET_MAP_THRESHOLD {
                return;
            }
            let mut values_by_key = FxHashSet::default();
            values_by_key.reserve(self.values.len());
            for v in &self.values[..self.values.len() - 1] {
                values_by_key.insert(v.clone());
            }
            self.values_by_key = Some(values_by_key);
        }
        if let Some(values_by_key) = &mut self.values_by_key {
            values_by_key.insert(value);
        }
    }
}

// Go: checker/utilities.go:843 getContainingFunctionOrClassStaticBlock
pub fn get_containing_function_or_class_static_block(node: Node) -> Node {
    find_ancestor(
        node.parent(),
        is_function_like_or_class_static_block_declaration,
    )
}

// Go: checker/utilities.go:847 isNodeDescendantOf
// PORT: renamed; `is_node_descendant_of` is `ast.IsNodeDescendantOf` (same body).
pub fn checker_is_node_descendant_of(mut node: Node, ancestor: Node) -> bool {
    while node.is_some() {
        if node == ancestor {
            return true;
        }
        node = node.parent();
    }
    false
}

impl Checker {
    // Go: checker/utilities.go:857 isTypeUsableAsPropertyName
    pub fn is_type_usable_as_property_name(&self, t: TypeId) -> bool {
        self.ty(t)
            .flags
            .intersects(TypeFlags::STRING_OR_NUMBER_LITERAL_OR_UNIQUE)
    }

    // Go: checker/utilities.go:864 getPropertyNameFromType
    /**
     * Gets the symbolic name for a member from its type.
     */
    pub fn get_property_name_from_type(&self, t: TypeId) -> String {
        let ty = self.ty(t);
        if ty.flags.intersects(TypeFlags::STRING_LITERAL) {
            return literal_string_value(ty).to_string();
        }
        if ty.flags.intersects(TypeFlags::NUMBER_LITERAL) {
            return literal_number_value(ty).to_string();
        }
        if ty.flags.intersects(TypeFlags::UNIQUE_ES_SYMBOL) {
            return ty.as_unique_es_symbol_type().name.clone();
        }
        panic!("Unhandled case in getPropertyNameFromType")
    }
}

// Go: checker/utilities.go:876 isNumericLiteralName
pub fn is_numeric_literal_name(name: &str) -> bool {
    // The intent of numeric names is that
    //     - they are names with text in a numeric form, and that
    //     - setting properties/indexing with them is always equivalent to doing so with the numeric literal 'numLit',
    //         acquired by applying the abstract 'ToNumber' operation on the name's text.
    //
    // The subtlety is in the latter portion, as we cannot reliably say that anything that looks like a numeric literal is a numeric name.
    // In fact, it is the case that the text of the name must be equal to 'ToString(numLit)' for this to hold.
    //
    // Consider the property name '"0xF00D"'. When one indexes with '0xF00D', they are actually indexing with the value of 'ToString(0xF00D)'
    // according to the ECMAScript specification, so it is actually as if the user indexed with the string '"61453"'.
    // Thus, the text of all numeric literals equivalent to '61543' such as '0xF00D', '0xf00D', '0170015', etc. are not valid numeric names
    // because their 'ToString' representation is not equal to their original text.
    // This is motivated by ECMA-262 sections 9.3.1, 9.8.1, 11.1.5, and 11.2.1.
    //
    // Here, we test whether 'ToString(ToNumber(name))' is exactly equal to 'name'.
    // The '+' prefix operator is equivalent here to applying the abstract ToNumber operation.
    // Applying the 'toString()' method on a number gives us the abstract ToString operation on a number.
    //
    // Note that this accepts the values 'Infinity', '-Infinity', and 'NaN', and that this is intentional.
    // This is desired behavior, because when indexing with them as numeric entities, you are indexing
    // with the strings '"Infinity"', '"-Infinity"', and '"NaN"' respectively.
    crate::jsnum::from_string(name).to_string() == name
}

// Go: checker/utilities.go:901 isThisProperty
pub fn is_this_property(node: Node) -> bool {
    (is_property_access_expression(node) || is_element_access_expression(node))
        && node.expression().kind() == SyntaxKind::ThisKeyword
}
