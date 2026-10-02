//! Port of Go `checker/checker.go` lines 8445-9354: call, new, tagged
//! template, decorator and instanceof resolution, overload selection
//! (`resolveCall`, `chooseOverload`), arity checks and signature
//! applicability.

use crate::prelude::*;
use smallvec::SmallVec;

// PORT: Go `*Relation` is shared and mutated across calls, so it is
// `Rc<RefCell<Relation>>`; a `*Relation` param is `&Rc<RefCell<Relation>>`.
// PORT: Go `candidatesOutArray *[]*Signature` is `Option<&mut Vec<SignatureId>>`.
// PORT: Go `diagnosticOutput *[]*ast.Diagnostic` is `Option<&mut Vec<Diagnostic>>`.
// PORT: a nullable Go `*diagnostics.Message` is `Option<&'static crate::diagnostics::Message>`.

// Go: checker/checker.go:8804 CallState
// PERF: callcopy1. `type_arguments` and `args` hold most lists inline, so a
// call copies each list once and allocates only for a long one. Go shares
// `node.TypeArguments()` and `node.Arguments()`; the port needs a `[Node]`
// for the callees that take `&[Node]`.
#[derive(Clone, Debug, Default)]
pub struct CallState {
    pub node: Node,
    pub type_arguments: SmallVec<[Node; 4]>,
    pub args: SmallVec<[Node; 8]>,
    // PERF: the candidate lists live on the stack up to 4 signatures.
    pub candidates: SmallVec<[SignatureId; 4]>,
    pub arg_check_mode: CheckMode,
    pub is_single_non_generic_candidate: bool,
    pub signature_help_trailing_comma: bool,
    pub recursive_resolution: bool,
    pub candidates_for_argument_error: SmallVec<[SignatureId; 4]>,
    pub candidate_for_argument_arity_error: SignatureId,
    pub candidate_for_type_argument_error: SignatureId,
}

// Go: checker/checker.go:8822 constructorAccessibilityError
#[derive(Clone, Copy, Debug)]
pub struct ConstructorAccessibilityError {
    pub kind: ModifierFlags,
    pub declaring_class: TypeId,
}

/// Go `*candidatesOutArray = s.candidates` in `resolveCall`, at a return:
/// moves the final candidates to the caller's array.
fn set_candidates_out(out: Option<&mut Vec<SignatureId>>, s: &mut CallState) {
    if let Some(out) = out {
        *out = std::mem::take(&mut s.candidates).into_vec();
    }
}

impl Checker {
    // Go: checker/checker.go:8445 resolveCallExpression
    pub fn resolve_call_expression(
        &mut self,
        node: Node,
        candidates_out_array: Option<&mut Vec<SignatureId>>,
        check_mode: CheckMode,
    ) -> SignatureId {
        if node.expression().kind() == SyntaxKind::SuperKeyword {
            let super_type = self.check_super_expression(node.expression());
            if self.is_type_any(super_type) {
                for arg in node.arguments() {
                    // Still visit arguments so they get marked for visibility, etc
                    self.check_expression(arg);
                }
                return self.any_signature;
            }
            if !self.is_error_type(super_type) {
                // In super call, the candidate signatures are the matching arity signatures of the base constructor function instantiated
                // with the type arguments specified in the extends clause.
                let base_type_node = get_class_extends_heritage_element(get_containing_class(node));
                if base_type_node.is_some() {
                    let base_constructors = self.get_instantiated_constructors_for_type_arguments(
                        super_type,
                        &base_type_node.type_arguments().to_vec(),
                        base_type_node,
                    );
                    return self.resolve_call(
                        node,
                        &base_constructors,
                        candidates_out_array,
                        check_mode,
                        SignatureFlags::NONE,
                        None,
                    );
                }
            }
            return self.resolve_untyped_call(node);
        }
        if is_import_call(node) {
            return self.resolve_untyped_call(node);
        }
        let call_chain_flags: SignatureFlags;
        let mut func_type = self.check_expression(node.expression());
        if is_call_chain(node) {
            let non_optional_type = self.get_optional_expression_type(func_type, node.expression());
            if non_optional_type == func_type {
                call_chain_flags = SignatureFlags::NONE;
            } else if is_outermost_optional_chain(node) {
                call_chain_flags = SignatureFlags::IS_OUTER_CALL_CHAIN;
            } else {
                call_chain_flags = SignatureFlags::IS_INNER_CALL_CHAIN;
            }
            func_type = non_optional_type;
        } else {
            call_chain_flags = SignatureFlags::NONE;
        }
        func_type = self.check_non_null_type_with_reporter(
            func_type,
            node.expression(),
            &mut |c: &mut Checker, n: Node, facts: TypeFacts| {
                c.report_cannot_invoke_possibly_null_or_undefined_error(n, facts)
            },
        );
        if func_type == self.silent_never_type {
            return self.silent_never_signature;
        }
        let apparent_type = self.get_apparent_type(func_type);
        if self.is_error_type(apparent_type) {
            // Another error has already been reported
            return self.resolve_error_call(node);
        }
        // Technically, this signatures list may be incomplete. We are taking the apparent type,
        // but we are not including call signatures that may have been added to the Object or
        // Function interface, since they have none by default. This is a bit of a leap of faith
        // that the user will not add any.
        let call_signatures = self.get_signatures_of_type(apparent_type, SignatureKind::CALL);
        let num_construct_signatures = self
            .get_signatures_of_type(apparent_type, SignatureKind::CONSTRUCT)
            .len() as i32;
        // TS 1.0 Spec: 4.12
        // In an untyped function call no TypeArgs are permitted, Args can be any argument list, no contextual
        // types are provided for the argument expressions, and the result is always of type Any.
        if self.is_untyped_function_call(
            func_type,
            apparent_type,
            call_signatures.len() as i32,
            num_construct_signatures,
        ) {
            // The unknownType indicates that an error already occurred (and was reported).  No
            // need to report another error in this case.
            // PORT: Go `node.TypeArguments() != nil` is nil only when the list is nil.
            if !self.is_error_type(func_type) && node.type_argument_list().is_some() {
                self.error(
                    node,
                    diag::Untyped_function_calls_may_not_accept_type_arguments,
                    args![],
                );
            }
            return self.resolve_untyped_call(node);
        }
        // If FuncExpr's apparent type(section 3.8.1) is a function type, the call is a typed function call.
        // TypeScript employs overload resolution in typed function calls in order to support functions
        // with multiple call signatures.
        if call_signatures.is_empty() {
            if num_construct_signatures != 0 {
                let type_str = self.type_to_string(func_type);
                self.error(
                    node,
                    diag::Value_of_type_0_is_not_callable_Did_you_mean_to_include_new,
                    args![type_str],
                );
            } else {
                let mut related_information: Option<Diagnostic> = None;
                if node.arguments().len() == 1 {
                    let text = source_file_text(get_source_file_of_node(node));
                    let options = SkipTriviaOptions {
                        stop_after_line_break: true,
                        ..Default::default()
                    };
                    let pos = skip_trivia_ex(&text, node.expression().end(), Some(&options));
                    if is_line_break(text.as_bytes()[(pos - 1) as usize] as char) {
                        related_information = Some(create_diagnostic_for_node(
                            node.expression(),
                            diag::Are_you_missing_a_semicolon,
                            args![],
                        ));
                    }
                }
                self.invocation_error(
                    node.expression(),
                    apparent_type,
                    SignatureKind::CALL,
                    related_information,
                );
            }
            return self.resolve_error_call(node);
        }
        // When a call to a generic function is an argument to an outer call to a generic function for which
        // inference is in process, we have a choice to make. If the inner call relies on inferences made from
        // its contextual type to its return type, deferring the inner call processing allows the best possible
        // contextual type to accumulate. But if the outer call relies on inferences made from the return type of
        // the inner call, the inner call should be processed early. There's no sure way to know which choice is
        // right (only a full unification algorithm can determine that), so we resort to the following heuristic:
        // If no type arguments are specified in the inner call and at least one call signature is generic and
        // returns a function type, we choose to defer processing. This narrowly permits function composition
        // operators to flow inferences through return types, but otherwise processes calls right away. We
        // use the resolvingSignature singleton to indicate that we deferred processing. This result will be
        // propagated out and eventually turned into silentNeverType (a type that is assignable to anything and
        // from which we never make inferences).
        if check_mode.intersects(CheckMode::SKIP_GENERIC_FUNCTIONS)
            && node.type_arguments().is_empty()
        {
            let mut some = false;
            for &sig in &call_signatures {
                if self.is_generic_function_returning_function(sig) {
                    some = true;
                    break;
                }
            }
            if some {
                self.skipped_generic_function(node, check_mode);
                return self.resolving_signature;
            }
        }
        self.resolve_call(
            node,
            &call_signatures,
            candidates_out_array,
            check_mode,
            call_chain_flags,
            None,
        )
    }

    // Go: checker/checker.go:8549 resolveNewExpression
    pub fn resolve_new_expression(
        &mut self,
        node: Node,
        candidates_out_array: Option<&mut Vec<SignatureId>>,
        check_mode: CheckMode,
    ) -> SignatureId {
        let mut expression_type = self.check_non_null_expression(node.expression());
        if expression_type == self.silent_never_type {
            return self.silent_never_signature;
        }
        // If expressionType's apparent type(section 3.8.1) is an object type with one or
        // more construct signatures, the expression is processed in the same manner as a
        // function call, but using the construct signatures as the initial set of candidate
        // signatures for overload resolution. The result type of the function call becomes
        // the result type of the operation.
        expression_type = self.get_apparent_type(expression_type);
        if self.is_error_type(expression_type) {
            // Another error has already been reported
            return self.resolve_error_call(node);
        }
        // TS 1.0 spec: 4.11
        // If expressionType is of type Any, Args can be any argument
        // list and the result of the operation is of type Any.
        if self.is_type_any(expression_type) {
            if !node.type_arguments().is_empty() {
                self.error(
                    node,
                    diag::Untyped_function_calls_may_not_accept_type_arguments,
                    args![],
                );
            }
            return self.resolve_untyped_call(node);
        }
        // Technically, this signatures list may be incomplete. We are taking the apparent type,
        // but we are not including construct signatures that may have been added to the Object or
        // Function interface, since they have none by default. This is a bit of a leap of faith
        // that the user will not add any.
        let construct_signatures =
            self.get_signatures_of_type(expression_type, SignatureKind::CONSTRUCT);
        if !construct_signatures.is_empty() {
            let accessibility_error = self.get_constructor_accessibility_error(
                node,
                &construct_signatures,
                ModifierFlags::NON_PUBLIC_ACCESSIBILITY_MODIFIER,
            );
            if let Some(accessibility_error) = accessibility_error {
                if accessibility_error.kind.intersects(ModifierFlags::PRIVATE) {
                    let class_str = self.type_to_string(accessibility_error.declaring_class);
                    self.error(
                        node,
                        diag::Constructor_of_class_0_is_private_and_only_accessible_within_the_class_declaration,
                        args![class_str],
                    );
                }
                if accessibility_error
                    .kind
                    .intersects(ModifierFlags::PROTECTED)
                {
                    let class_str = self.type_to_string(accessibility_error.declaring_class);
                    self.error(
                        node,
                        diag::Constructor_of_class_0_is_protected_and_only_accessible_within_the_class_declaration,
                        args![class_str],
                    );
                }
                return self.resolve_error_call(node);
            }
            // If the expression is a class of abstract type, or an abstract construct signature,
            // then it cannot be instantiated.
            // In the case of a merged class-module or class-interface declaration,
            // only the class declaration node will have the Abstract flag set.
            if self.some_signature(
                &construct_signatures,
                &mut |c: &mut Checker, sig: SignatureId| {
                    c.sig(sig).flags.intersects(SignatureFlags::ABSTRACT)
                },
            ) {
                self.error(
                    node,
                    diag::Cannot_create_an_instance_of_an_abstract_class,
                    args![],
                );
                return self.resolve_error_call(node);
            }
            let expression_symbol = self.ty(expression_type).symbol;
            if expression_symbol.is_some() {
                let value_decl =
                    get_class_like_declaration_of_symbol(&self.symbols, expression_symbol);
                if value_decl.is_some() && has_modifier(value_decl, ModifierFlags::ABSTRACT) {
                    self.error(
                        node,
                        diag::Cannot_create_an_instance_of_an_abstract_class,
                        args![],
                    );
                    return self.resolve_error_call(node);
                }
            }
            return self.resolve_call(
                node,
                &construct_signatures,
                candidates_out_array,
                check_mode,
                SignatureFlags::NONE,
                None,
            );
        }
        // If expressionType's apparent type is an object type with no construct signatures but
        // one or more call signatures, the expression is processed as a function call. A compile-time
        // error occurs if the result of the function call is not Void. The type of the result of the
        // operation is Any. It is an error to have a Void this type.
        let call_signatures = self.get_signatures_of_type(expression_type, SignatureKind::CALL);
        if !call_signatures.is_empty() {
            let signature = self.resolve_call(
                node,
                &call_signatures,
                candidates_out_array,
                check_mode,
                SignatureFlags::NONE,
                None,
            );
            if !self.no_implicit_any {
                if self.sig(signature).declaration.is_some()
                    && self.get_return_type_of_signature(signature) != self.void_type
                {
                    self.error(
                        node,
                        diag::Only_a_void_function_can_be_called_with_the_new_keyword,
                        args![],
                    );
                }
                if self.get_this_type_of_signature(signature) == self.void_type {
                    self.error(
                        node,
                        diag::A_function_that_is_called_with_the_new_keyword_cannot_have_a_this_type_that_is_void,
                        args![],
                    );
                }
            }
            return signature;
        }
        self.invocation_error(
            node.expression(),
            expression_type,
            SignatureKind::CONSTRUCT,
            None,
        );
        self.resolve_error_call(node)
    }

    // Go: checker/checker.go:8827 getConstructorAccessibilityError
    // PORT: Go returns a nil `*constructorAccessibilityError` for no error; here `None`.
    pub fn get_constructor_accessibility_error(
        &mut self,
        node: Node,
        signatures: &[SignatureId],
        modifiers_mask: ModifierFlags,
    ) -> Option<ConstructorAccessibilityError> {
        for &signature in signatures {
            if self.sig(signature).declaration.is_nil() {
                continue;
            }
            let declaration = self.sig(signature).declaration;
            let modifiers = get_selected_modifier_flags(declaration, modifiers_mask);
            // (1) Public constructors and (2) constructor functions are always accessible.
            if modifiers.0 == 0 || !is_constructor_declaration(declaration) {
                continue;
            }
            let declaring_class_declaration =
                get_class_like_declaration_of_symbol(&self.symbols, declaration.parent().symbol());
            // A private or protected constructor can only be instantiated within its own class (or a subclass, for protected)
            if !self.is_node_within_class(node, declaring_class_declaration) {
                let containing_class = get_containing_class(node);
                if containing_class.is_some() && modifiers.intersects(ModifierFlags::PROTECTED) {
                    let containing_type = self.get_type_of_node(containing_class);
                    if self.type_has_protected_accessible_base(
                        declaration.parent().symbol(),
                        containing_type,
                    ) {
                        continue;
                    }
                }
                let declaring_class =
                    self.get_declared_type_of_symbol(declaration.parent().symbol());
                return Some(ConstructorAccessibilityError {
                    kind: modifiers,
                    declaring_class,
                });
            }
        }
        None
    }

    // Go: checker/checker.go:8654 typeHasProtectedAccessibleBase
    pub fn type_has_protected_accessible_base(&mut self, target: SymbolId, t: TypeId) -> bool {
        let target_type = self.get_target_type(t);
        let base_types = self.get_base_types(target_type);
        if base_types.is_empty() {
            return false;
        }
        let first_base = base_types[0];
        if self
            .ty(first_base)
            .flags
            .intersects(TypeFlags::INTERSECTION)
        {
            let types = self
                .ty(first_base)
                .as_intersection_type()
                .union_or_intersection
                .types
                .clone();
            let (mixin_flags, _) = self.find_mixins(&types);
            let members = self.ty(first_base).types_list();
            for (i, intersection_member) in members.into_iter().enumerate() {
                // We want to ignore mixin ctors
                if !mixin_flags[i] {
                    if self
                        .ty(intersection_member)
                        .object_flags
                        .intersects(ObjectFlags::CLASS | ObjectFlags::INTERFACE)
                    {
                        if self.ty(intersection_member).symbol == target {
                            return true;
                        }
                        if self.type_has_protected_accessible_base(target, intersection_member) {
                            return true;
                        }
                    }
                }
            }
            return false;
        }
        if self.ty(first_base).symbol == target {
            return true;
        }
        self.type_has_protected_accessible_base(target, first_base)
    }

    // Go: checker/checker.go:8684 someSignature
    // PORT: Go package-level func; it reads signature data, so it is a Checker method.
    pub fn some_signature(
        &mut self,
        signatures: &[SignatureId],
        f: &mut dyn FnMut(&mut Checker, SignatureId) -> bool,
    ) -> bool {
        for &sig in signatures {
            let composite = self.sig(sig).composite.clone();
            match composite {
                Some(composite) => {
                    if composite.is_union {
                        for &s in &composite.signatures {
                            if f(self, s) {
                                return true;
                            }
                        }
                    }
                }
                None => {
                    if f(self, sig) {
                        return true;
                    }
                }
            }
        }
        false
    }

    // Go: checker/checker.go:8693 resolveTaggedTemplateExpression
    pub fn resolve_tagged_template_expression(
        &mut self,
        node: Node,
        candidates_out_array: Option<&mut Vec<SignatureId>>,
        check_mode: CheckMode,
    ) -> SignatureId {
        let tag = node.tag();
        let tag_type = self.check_expression(tag);
        let apparent_type = self.get_apparent_type(tag_type);
        if self.is_error_type(apparent_type) {
            // Another error has already been reported
            return self.resolve_error_call(node);
        }
        let call_signatures = self.get_signatures_of_type(apparent_type, SignatureKind::CALL);
        let num_construct_signatures = self
            .get_signatures_of_type(apparent_type, SignatureKind::CONSTRUCT)
            .len() as i32;
        if self.is_untyped_function_call(
            tag_type,
            apparent_type,
            call_signatures.len() as i32,
            num_construct_signatures,
        ) {
            return self.resolve_untyped_call(node);
        }
        if call_signatures.is_empty() {
            if is_array_literal_expression(node.parent()) {
                self.error(
                    tag,
                    diag::It_is_likely_that_you_are_missing_a_comma_to_separate_these_two_template_expressions_They_form_a_tagged_template_expression_which_cannot_be_invoked,
                    args![],
                );
                return self.resolve_error_call(node);
            }
            self.invocation_error(tag, apparent_type, SignatureKind::CALL, None);
            return self.resolve_error_call(node);
        }
        self.resolve_call(
            node,
            &call_signatures,
            candidates_out_array,
            check_mode,
            SignatureFlags::NONE,
            None,
        )
    }

    // Go: checker/checker.go:8717 resolveDecorator
    pub fn resolve_decorator(
        &mut self,
        node: Node,
        candidates_out_array: Option<&mut Vec<SignatureId>>,
        check_mode: CheckMode,
    ) -> SignatureId {
        if !can_have_decorators(node.parent()) {
            return self.resolve_error_call(node);
        }
        let func_type = self.check_expression(node.expression());
        let apparent_type = self.get_apparent_type(func_type);
        if self.is_error_type(apparent_type) {
            return self.resolve_error_call(node);
        }
        let call_signatures = self.get_signatures_of_type(apparent_type, SignatureKind::CALL);
        let num_construct_signatures = self
            .get_signatures_of_type(apparent_type, SignatureKind::CONSTRUCT)
            .len() as i32;
        if self.is_untyped_function_call(
            func_type,
            apparent_type,
            call_signatures.len() as i32,
            num_construct_signatures,
        ) {
            return self.resolve_untyped_call(node);
        }
        if self.is_potentially_uncalled_decorator(node, &call_signatures)
            && !is_parenthesized_expression(node.expression())
        {
            let node_str = get_text_of_node(node.expression());
            self.error(
                node,
                diag::X_0_accepts_too_few_arguments_to_be_used_as_a_decorator_here_Did_you_mean_to_call_it_first_and_write_0,
                args![node_str],
            );
            return self.resolve_error_call(node);
        }
        let head_message = self.get_diagnostic_head_message_for_decorator_resolution(node);
        if call_signatures.is_empty() {
            let details = self.invocation_error_details(
                node.expression(),
                apparent_type,
                SignatureKind::CALL,
            );
            let diag = new_diagnostic_chain(Some(details), head_message, args![]);
            // Go (#4825): the recovery adds related info to the stored
            // diagnostic (see `invocation_error_recovery` for the order).
            let recovery = self.invocation_error_recovery(apparent_type, SignatureKind::CALL);
            if let Some(diag) = self.add_diagnostic(diag) {
                diag.add_related_info(recovery);
            }
            return self.resolve_error_call(node);
        }
        let decorator_signature = self.get_decorator_call_signature(node);
        if decorator_signature.is_nil() {
            return self.resolve_error_call(node);
        }
        self.resolve_call(
            node,
            &call_signatures,
            candidates_out_array,
            check_mode,
            SignatureFlags::NONE,
            Some(head_message),
        )
    }

    // Sometimes, we have a decorator that could accept zero arguments,
    // but is receiving too many arguments as part of the decorator invocation.
    // In those cases, a user may have meant to *call* the expression before using it as a decorator.
    // Go: checker/checker.go:8753 isPotentiallyUncalledDecorator
    pub fn is_potentially_uncalled_decorator(
        &mut self,
        decorator: Node,
        signatures: &[SignatureId],
    ) -> bool {
        if signatures.is_empty() {
            return false;
        }
        for &sig in signatures {
            let ok = self.sig(sig).min_argument_count == 0
                && !self.signature_has_rest_parameter(sig)
                && (self.sig(sig).parameters.len() as i32)
                    < self.get_decorator_argument_count(decorator, sig);
            if !ok {
                return false;
            }
        }
        true
    }

    // Gets the localized diagnostic head message to use for errors when resolving a decorator as a call expression.
    // Go: checker/checker.go:8760 getDiagnosticHeadMessageForDecoratorResolution
    pub fn get_diagnostic_head_message_for_decorator_resolution(
        &mut self,
        node: Node,
    ) -> &'static crate::diagnostics::Message {
        match node.parent().kind() {
            SyntaxKind::ClassDeclaration | SyntaxKind::ClassExpression => {
                diag::Unable_to_resolve_signature_of_class_decorator_when_called_as_an_expression
            }
            SyntaxKind::Parameter => {
                diag::Unable_to_resolve_signature_of_parameter_decorator_when_called_as_an_expression
            }
            SyntaxKind::PropertyDeclaration => {
                diag::Unable_to_resolve_signature_of_property_decorator_when_called_as_an_expression
            }
            SyntaxKind::MethodDeclaration | SyntaxKind::GetAccessor | SyntaxKind::SetAccessor => {
                diag::Unable_to_resolve_signature_of_method_decorator_when_called_as_an_expression
            }
            _ => panic!("Unhandled case in getDiagnosticHeadMessageForDecoratorResolution"),
        }
    }

    // Go: checker/checker.go:8774 resolveInstanceofExpression
    pub fn resolve_instanceof_expression(
        &mut self,
        node: Node,
        candidates_out_array: Option<&mut Vec<SignatureId>>,
        check_mode: CheckMode,
    ) -> SignatureId {
        // if rightType is an object type with a custom `[Symbol.hasInstance]` method, then it is potentially
        // valid on the right-hand side of the `instanceof` operator. This allows normal `object` types to
        // participate in `instanceof`, as per Step 2 of https://tc39.es/ecma262/#sec-instanceofoperator.
        let right = node.right();
        let right_type = self.check_expression(right);
        if !self.is_type_any(right_type) {
            let has_instance_method_type =
                self.get_symbol_has_instance_method_of_object_type(right_type);
            if has_instance_method_type.is_some() {
                let apparent_type = self.get_apparent_type(has_instance_method_type);
                if self.is_error_type(apparent_type) {
                    return self.resolve_error_call(node);
                }
                let call_signatures =
                    self.get_signatures_of_type(apparent_type, SignatureKind::CALL);
                let construct_signatures =
                    self.get_signatures_of_type(apparent_type, SignatureKind::CONSTRUCT);
                if self.is_untyped_function_call(
                    has_instance_method_type,
                    apparent_type,
                    call_signatures.len() as i32,
                    construct_signatures.len() as i32,
                ) {
                    return self.resolve_untyped_call(node);
                }
                if !call_signatures.is_empty() {
                    return self.resolve_call(
                        node,
                        &call_signatures,
                        candidates_out_array,
                        check_mode,
                        SignatureFlags::NONE,
                        None,
                    );
                }
            } else if !(self.type_has_call_or_construct_signatures(right_type) || {
                let global_function_type = self.global_function_type;
                self.is_type_subtype_of(right_type, global_function_type)
            }) {
                self.error(
                    right,
                    diag::The_right_hand_side_of_an_instanceof_expression_must_be_either_of_type_any_a_class_function_or_other_type_assignable_to_the_Function_interface_type_or_an_object_type_with_a_Symbol_hasInstance_method,
                    args![],
                );
                return self.resolve_error_call(node);
            }
        }
        // fall back to a default signature
        self.any_signature
    }

    // Go: checker/checker.go:8817 resolveCall
    pub fn resolve_call(
        &mut self,
        node: Node,
        signatures: &[SignatureId],
        candidates_out_array: Option<&mut Vec<SignatureId>>,
        check_mode: CheckMode,
        call_chain_flags: SignatureFlags,
        mut head_message: Option<&'static crate::diagnostics::Message>,
    ) -> SignatureId {
        let is_tagged_template = node.kind() == SyntaxKind::TaggedTemplateExpression;
        let is_decorator = node.kind() == SyntaxKind::Decorator;
        let is_jsx_opening_or_self_closing_element = is_jsx_opening_like_element(node);
        let is_instanceof = node.kind() == SyntaxKind::BinaryExpression;
        let has_candidates_out_array = candidates_out_array.is_some();
        let report_errors = !self.is_inference_partially_blocked && !has_candidates_out_array;
        let mut s = CallState::default();
        s.node = node;
        // PORT: Go calls the checker package's `isSuperCall`, which has the same
        // body as `ast.IsSuperCall`.
        if !is_decorator
            && !is_instanceof
            && !crate::ast::is_super_call(node)
            && !is_jsx_opening_fragment(node)
        {
            s.type_arguments = node.type_arguments().iter().collect();
            // We already perform checking on the type arguments on the class declaration itself.
            if is_tagged_template
                || is_jsx_opening_or_self_closing_element
                || node.expression().kind() != SyntaxKind::SuperKeyword
            {
                self.check_source_elements(node.type_arguments());
            }
        }
        s.candidates = self.reorder_candidates(signatures, call_chain_flags);
        // PORT: Go stores the same backing array in `*candidatesOutArray`, so
        // later writes to `s.candidates[i]` are visible to the caller. The
        // caller reads the array only after this returns, so the port moves
        // `s.candidates` there at each return (`set_candidates_out`).

        if s.candidates.is_empty() {
            // In Strada we would error here, but no known repro doesn't have at least
            // one other error in this codepath. Just return instead. See #54442
            set_candidates_out(candidates_out_array, &mut s);
            return self.unknown_signature;
        }

        s.args = match self.get_effective_call_arguments(node) {
            EffectiveArgs::Slice(args) => args.iter().collect(),
            EffectiveArgs::Owned(args) => SmallVec::from_vec(args),
        };
        // The excludeArgument array contains true for each context sensitive argument (an argument
        // is context sensitive it is susceptible to a one-time permanent contextual typing).
        //
        // The idea is that we will perform type argument inference & assignability checking once
        // without using the susceptible parameters that are functions, and once more for those
        // parameters, contextually typing each as we go along.
        //
        // For a tagged template, then the first argument be 'undefined' if necessary because it
        // represents a TemplateStringsArray.
        //
        // For a decorator, no arguments are susceptible to contextual typing due to the fact
        // decorators are applied to a declaration by the emitter, and not to an expression.
        s.is_single_non_generic_candidate =
            s.candidates.len() == 1 && self.sig(s.candidates[0]).type_parameters.is_empty();
        let mut any_context_sensitive = false;
        if !is_decorator && !s.is_single_non_generic_candidate {
            for &arg in &s.args {
                if self.is_context_sensitive(arg) {
                    any_context_sensitive = true;
                    break;
                }
            }
        }
        if any_context_sensitive {
            s.arg_check_mode = CheckMode::SKIP_CONTEXT_SENSITIVE;
        } else {
            s.arg_check_mode = CheckMode::NORMAL;
        }
        // The following variables are captured and modified by calls to chooseOverload.
        // If overload resolution or type argument inference fails, we want to report the
        // best error possible. The best error is one which says that an argument was not
        // assignable to a parameter. This implies that everything else about the overload
        // was fine. So if there is any overload that is only incorrect because of an
        // argument, we will report an error on that one.
        //
        //     function foo(s: string): void;
        //     function foo(n: number): void; // Report argument error on this overload
        //     function foo(): void;
        //     foo(true);
        //
        // If none of the overloads even made it that far, there are two possibilities.
        // There was a problem with type arguments for some overload, in which case
        // report an error on that. Or none of the overloads even had correct arity,
        // in which case give an arity error.
        //
        //     function foo<T extends string>(x: T): void; // Report type argument error
        //     function foo(): void;
        //     foo<number>(0);
        //
        // If we are in signature help, a trailing comma indicates that we intend to provide another argument,
        // so we will only accept overloads with arity at least 1 higher than the current number of provided arguments.
        s.signature_help_trailing_comma = check_mode.intersects(CheckMode::IS_FOR_SIGNATURE_HELP)
            && is_call_expression(node)
            && node.argument_list().has_trailing_comma();
        // Section 4.12.1:
        // if the candidate list contains one or more signatures for which the type of each argument
        // expression is a subtype of each corresponding parameter type, the return type of the first
        // of those signatures becomes the return type of the function call.
        // Otherwise, the return type of the first signature in the candidate list becomes the return
        // type of the function call.
        //
        // Whether the call is an error is determined by assignability of the arguments. The subtype pass
        // is just important for choosing the best signature. So in the case where there is only one
        // signature, the subtype pass is useless. So skipping it is an optimization.
        let mut result = SignatureId::NIL;
        s.recursive_resolution = self.call_resolution_stack.contains(&s.node);
        self.call_resolution_stack.push(s.node);
        if s.candidates.len() > 1 {
            let relation = self.subtype_relation.clone();
            result = self.choose_overload(&mut s, &relation);
        }
        if result.is_nil() {
            let relation = self.assignable_relation.clone();
            result = self.choose_overload(&mut s, &relation);
        }
        self.call_resolution_stack.pop();
        if result.is_some() {
            set_candidates_out(candidates_out_array, &mut s);
            return result;
        }
        // PORT: Go passes `s.candidates` by slice, so the callee's in-place edits are seen by
        // `s.candidates` and by `*candidatesOutArray` (same backing array).
        result = self.get_candidate_for_overload_failure(
            s.node,
            &mut s.candidates,
            &s.args,
            has_candidates_out_array,
            check_mode,
        );
        // With an out array `report_errors` is false, so nothing below reads
        // `s.candidates`.
        set_candidates_out(candidates_out_array, &mut s);
        // Preemptively cache the result; getResolvedSignature will do this after we return, but
        // we need to ensure that the result is present for the error checks below so that if
        // this signature is encountered again, we handle the circularity (rather than producing a
        // different result which may produce no errors and assert). Callers of getResolvedSignature
        // don't hit this issue because they only observe this result after it's had a chance to
        // be cached, but the error reporting code below executes before getResolvedSignature sets
        // resolvedSignature.
        self.signature_links.get(node).resolved_signature = result;
        // No signatures were applicable. Now report errors based on the last applicable signature with
        // no arguments excluded from assignability checks.
        // If candidate is undefined, it means that no candidates had a suitable arity. In that case,
        // skip the checkApplicableSignature check.
        if report_errors {
            // If the call expression is a synthetic call to a `[Symbol.hasInstance]` method then we will produce a head
            // message when reporting diagnostics that explains how we got to `right[Symbol.hasInstance](left)` from
            // `left instanceof right`, as it pertains to "Argument" related messages reported for the call.
            if head_message.is_none() && is_instanceof {
                head_message = Some(
                    diag::The_left_hand_side_of_an_instanceof_expression_must_be_assignable_to_the_first_argument_of_the_right_hand_side_s_Symbol_hasInstance_method,
                );
            }
            self.report_call_resolution_errors(node, &mut s, signatures, head_message);
        }
        result
    }

    // Go: checker/checker.go:8931 reorderCandidates
    pub fn reorder_candidates(
        &mut self,
        signatures: &[SignatureId],
        call_chain_flags: SignatureFlags,
    ) -> SmallVec<[SignatureId; 4]> {
        let mut last_parent = Node::NIL;
        let mut last_symbol = SymbolId::NIL;
        let mut index: i32 = 0;
        let mut cutoff_index: i32 = 0;
        let mut splice_index: i32;
        let mut specialized_index: i32 = -1;
        let mut result: SmallVec<[SignatureId; 4]> = SmallVec::with_capacity(signatures.len());
        for &signature in signatures {
            let mut signature = signature;
            let mut symbol = SymbolId::NIL;
            let mut parent = Node::NIL;
            let declaration = self.sig(signature).declaration;
            if declaration.is_some() {
                symbol = self.get_symbol_of_declaration(declaration);
                parent = declaration.parent();
            }
            if last_symbol.is_nil() || symbol == last_symbol {
                if last_parent.is_some() && parent == last_parent {
                    index += 1;
                } else {
                    last_parent = parent;
                    index = cutoff_index;
                }
            } else {
                // current declaration belongs to a different symbol
                // set cutoffIndex so re-orderings in the future won't change result set from 0 to cutoffIndex
                index = result.len() as i32;
                cutoff_index = result.len() as i32;
                last_parent = parent;
            }
            last_symbol = symbol;
            // specialized signatures always need to be placed before non-specialized signatures regardless
            // of the cutoff position; see GH#1133
            if self.signature_has_literal_types(signature) {
                specialized_index += 1;
                splice_index = specialized_index;
                // The cutoff index always needs to be greater than or equal to the specialized signature index
                // in order to prevent non-specialized signatures from being added before a specialized
                // signature.
                cutoff_index += 1;
            } else {
                splice_index = index;
            }
            if call_chain_flags.0 != 0 {
                signature = self.get_optional_call_signature(signature, call_chain_flags);
            }
            result.insert(splice_index as usize, signature);
        }
        result
    }

    // Go: checker/checker.go:8981 signatureHasLiteralTypes
    // PORT: Go package-level func; it reads signature data, so it is a Checker method.
    pub fn signature_has_literal_types(&self, s: SignatureId) -> bool {
        self.sig(s)
            .flags
            .intersects(SignatureFlags::HAS_LITERAL_TYPES)
    }

    // Go: checker/checker.go:8985 getOptionalCallSignature
    pub fn get_optional_call_signature(
        &mut self,
        signature: SignatureId,
        call_chain_flags: SignatureFlags,
    ) -> SignatureId {
        if (self.sig(signature).flags & SignatureFlags::CALL_CHAIN_FLAGS) == call_chain_flags {
            return signature;
        }
        let key = CachedSignatureKey {
            sig: signature,
            key: if call_chain_flags == SignatureFlags::IS_INNER_CALL_CHAIN {
                *SIGNATURE_KEY_INNER
            } else {
                *SIGNATURE_KEY_OUTER
            },
        };
        if let Some(&cached) = self.cached_signatures.get(&key) {
            if cached.is_some() {
                return cached;
            }
        }
        let result = self.clone_signature(signature);
        self.sig_mut(result).flags |= call_chain_flags;
        self.cached_signatures.insert(key, result);
        result
    }

    // Go: checker/checker.go:8999 chooseOverload
    pub fn choose_overload(
        &mut self,
        s: &mut CallState,
        relation: &Rc<RefCell<Relation>>,
    ) -> SignatureId {
        s.candidates_for_argument_error.clear();
        s.candidate_for_argument_arity_error = SignatureId::NIL;
        s.candidate_for_type_argument_error = SignatureId::NIL;
        let args = &s.args;
        if s.is_single_non_generic_candidate {
            let candidate = s.candidates[0];
            if !s.type_arguments.is_empty()
                || !self.has_correct_arity(s.node, args, candidate, s.signature_help_trailing_comma)
            {
                return SignatureId::NIL;
            }
            if !self.is_signature_applicable(
                s.node,
                args,
                candidate,
                relation,
                CheckMode::NORMAL,
                false, /*reportErrors*/
                None,  /*diagnosticOutput*/
            ) {
                s.candidates_for_argument_error.push(candidate);
                return SignatureId::NIL;
            }
            return candidate;
        }
        // Nothing in the loop changes `s.candidates` before the return.
        for candidate_index in 0..s.candidates.len() {
            let candidate = s.candidates[candidate_index];
            let type_arguments = &s.type_arguments;
            if !self.has_correct_type_argument_arity(candidate, type_arguments)
                || !self.has_correct_arity(s.node, args, candidate, s.signature_help_trailing_comma)
            {
                continue;
            }
            let mut check_candidate: SignatureId;
            let mut inference_context = InferenceContextId::NIL;
            // PERF: the type parameters are read from the candidate in place
            // (`new_inference_context_of_signature`), not copied.
            if !self.sig(candidate).type_parameters.is_empty() {
                let type_argument_types: SmallVec<[TypeId; 4]>;
                if !s.type_arguments.is_empty() {
                    match self.check_type_arguments(
                        candidate,
                        type_arguments,
                        false, /*reportErrors*/
                        None,
                    ) {
                        Some(types) => type_argument_types = SmallVec::from_vec(types),
                        None => {
                            s.candidate_for_type_argument_error = candidate;
                            continue;
                        }
                    }
                } else {
                    // When we are recursively resolving a call with a single candidate, we skip constraints checks during
                    // type inference to avoid circularity errors. For example, see #64192.
                    let mut flags = InferenceFlags::NONE;
                    if s.recursive_resolution && s.candidates.len() == 1 {
                        flags |= InferenceFlags::NO_CONSTRAINT_CHECKS;
                    }
                    if is_in_js_file(s.node) {
                        flags |= InferenceFlags::ANY_DEFAULT;
                    }
                    inference_context = self.new_inference_context_of_signature(candidate, flags);
                    type_argument_types = self.infer_type_arguments(
                        s.node,
                        candidate,
                        args,
                        s.arg_check_mode | CheckMode::SKIP_GENERIC_FUNCTIONS,
                        inference_context,
                    );
                    if self
                        .inference_context(inference_context)
                        .flags
                        .intersects(InferenceFlags::SKIPPED_GENERIC_FUNCTION)
                    {
                        s.arg_check_mode |= CheckMode::SKIP_GENERIC_FUNCTIONS;
                    }
                }
                let mut inferred_type_parameters: Vec<TypeId> = Vec::new();
                let mut inferred_type_parameters_origin = 0;
                if inference_context.is_some() {
                    let context = self.inference_context(inference_context);
                    inferred_type_parameters = context.inferred_type_parameters.clone();
                    inferred_type_parameters_origin = context.inferred_type_parameters_origin;
                }
                let candidate_declaration = self.sig(candidate).declaration;
                check_candidate = self.get_signature_instantiation(
                    candidate,
                    &type_argument_types,
                    is_in_js_file(candidate_declaration),
                    &inferred_type_parameters,
                    inferred_type_parameters_origin,
                );
                // If the original signature has a generic rest type, instantiation may produce a
                // signature with different arity and we need to perform another arity check.
                if self.get_non_array_rest_type(candidate).is_some()
                    && !self.has_correct_arity(
                        s.node,
                        args,
                        check_candidate,
                        s.signature_help_trailing_comma,
                    )
                {
                    s.candidate_for_argument_arity_error = check_candidate;
                    continue;
                }
            } else {
                check_candidate = candidate;
            }
            if !self.is_signature_applicable(
                s.node,
                args,
                check_candidate,
                relation,
                s.arg_check_mode,
                false, /*reportErrors*/
                None,  /*diagnosticOutput*/
            ) {
                // Give preference to error candidates that have no rest parameters (as they are more specific)
                s.candidates_for_argument_error.push(check_candidate);
                continue;
            }
            if s.arg_check_mode.0 != 0 {
                // If one or more context sensitive arguments were excluded, we start including
                // them now (and keeping do so for any subsequent candidates) and perform a second
                // round of type inference and applicability checking for this particular candidate.
                s.arg_check_mode = CheckMode::NORMAL;
                if inference_context.is_some() {
                    let type_argument_types = self.infer_type_arguments(
                        s.node,
                        candidate,
                        args,
                        s.arg_check_mode,
                        inference_context,
                    );
                    let context = self.inference_context(inference_context);
                    let inferred_type_parameters = context.inferred_type_parameters.clone();
                    let inferred_type_parameters_origin = context.inferred_type_parameters_origin;
                    let candidate_declaration = self.sig(candidate).declaration;
                    check_candidate = self.get_signature_instantiation(
                        candidate,
                        &type_argument_types,
                        is_in_js_file(candidate_declaration),
                        &inferred_type_parameters,
                        inferred_type_parameters_origin,
                    );
                    // If the original signature has a generic rest type, instantiation may produce a
                    // signature with different arity and we need to perform another arity check.
                    if self.get_non_array_rest_type(candidate).is_some()
                        && !self.has_correct_arity(
                            s.node,
                            args,
                            check_candidate,
                            s.signature_help_trailing_comma,
                        )
                    {
                        s.candidate_for_argument_arity_error = check_candidate;
                        continue;
                    }
                }
                if !self.is_signature_applicable(
                    s.node,
                    args,
                    check_candidate,
                    relation,
                    s.arg_check_mode,
                    false, /*reportErrors*/
                    None,  /*diagnosticOutput*/
                ) {
                    // Give preference to error candidates that have no rest parameters (as they are more specific)
                    s.candidates_for_argument_error.push(check_candidate);
                    continue;
                }
            }
            s.candidates[candidate_index] = check_candidate;
            return check_candidate;
        }
        SignatureId::NIL
    }

    // Go: checker/checker.go:9081 hasCorrectArity
    pub fn has_correct_arity(
        &mut self,
        node: Node,
        args: &[Node],
        signature: SignatureId,
        signature_help_trailing_comma: bool,
    ) -> bool {
        if is_jsx_opening_fragment(node) {
            return true;
        }
        let arg_count: i32;
        let mut call_is_incomplete = false;
        // In incomplete call we want to be lenient when we have too few arguments
        let mut effective_parameter_count = self.get_parameter_count(signature);
        let mut effective_minimum_arguments = self.get_min_argument_count(signature);
        if is_tagged_template_expression(node) {
            arg_count = args.len() as i32;
            let template = node.template();
            if is_template_expression(template) {
                // If a tagged template expression lacks a tail literal, the call is incomplete.
                // Specifically, a template only can end in a TemplateTail or a Missing literal.
                let spans = template.template_spans().nodes();
                let last_span = if spans.is_empty() {
                    Node::NIL
                } else {
                    spans.get(spans.len() - 1)
                };
                // we should always have at least one span.
                call_is_incomplete = node_is_missing(last_span.literal())
                    || is_unterminated_literal(last_span.literal());
            } else {
                // If the template didn't end in a backtick, or its beginning occurred right prior to EOF,
                // then this might actually turn out to be a TemplateHead in the future;
                // so we consider the call to be incomplete.
                call_is_incomplete = is_unterminated_literal(template);
            }
        } else if is_decorator(node) {
            arg_count = self.get_decorator_argument_count(node, signature);
        } else if is_binary_expression(node) {
            arg_count = 1;
        } else if is_jsx_opening_like_element(node) {
            call_is_incomplete = node.attributes().end() == node.end();
            if call_is_incomplete {
                return true;
            }
            arg_count = if effective_minimum_arguments == 0 {
                args.len() as i32
            } else {
                1
            };
            effective_parameter_count = if args.is_empty() {
                effective_parameter_count
            } else {
                1
            }; // class may have argumentless ctor functions - still resolve ctor and compare vs props member type
            effective_minimum_arguments = effective_minimum_arguments.min(1); // sfc may specify context argument - handled by framework and not typechecked
        } else if is_new_expression(node) && node.argument_list().is_nil() {
            // This only happens when we have something of the form: 'new C'
            return self.get_min_argument_count(signature) == 0;
        } else {
            if signature_help_trailing_comma {
                arg_count = args.len() as i32 + 1;
            } else {
                arg_count = args.len() as i32;
            }
            // If we are missing the close parenthesis, the call is incomplete.
            call_is_incomplete = node.argument_list().end() == node.end();
            // If a spread argument is present, check that it corresponds to a rest parameter or at least that it's in the valid range.
            let spread_arg_index = self.get_spread_argument_index(args);
            if spread_arg_index >= 0 {
                return spread_arg_index >= self.get_min_argument_count(signature)
                    && (self.has_effective_rest_parameter(signature)
                        || spread_arg_index < self.get_parameter_count(signature));
            }
        }
        // Too many arguments implies incorrect arity.
        if !self.has_effective_rest_parameter(signature) && arg_count > effective_parameter_count {
            return false;
        }
        // If the call is incomplete, we should skip the lower bound check.
        // JSX signatures can have extra parameters provided by the library which we don't check
        if call_is_incomplete || arg_count >= effective_minimum_arguments {
            return true;
        }
        for i in arg_count..effective_minimum_arguments {
            let t = self.get_type_at_position(signature, i);
            let filtered = self.filter_type(t, &mut |c: &mut Checker, t: TypeId| c.accepts_void(t));
            if self.ty(filtered).flags.intersects(TypeFlags::NEVER) {
                return false;
            }
        }
        true
    }

    // Go: checker/checker.go:9153 acceptsVoid
    // PORT: Go package-level func; it reads type data, so it is a Checker method.
    pub fn accepts_void(&self, t: TypeId) -> bool {
        self.ty(t).flags.intersects(TypeFlags::VOID)
    }

    // Go: checker/checker.go:9157 getDecoratorArgumentCount
    pub fn get_decorator_argument_count(&mut self, node: Node, signature: SignatureId) -> i32 {
        if self.compiler_options.experimental_decorators.is_true() {
            return self.get_legacy_decorator_argument_count(node, signature);
        }
        self.get_parameter_count(signature).max(1).min(2)
    }

    /**
     * Returns the argument count for a decorator node that works like a function invocation.
     */
    // Go: checker/checker.go:9167 getLegacyDecoratorArgumentCount
    pub fn get_legacy_decorator_argument_count(
        &mut self,
        node: Node,
        signature: SignatureId,
    ) -> i32 {
        match node.parent().kind() {
            SyntaxKind::ClassDeclaration | SyntaxKind::ClassExpression => 1,
            SyntaxKind::PropertyDeclaration => {
                if has_accessor_modifier(node.parent()) {
                    return 3;
                }
                2
            }
            SyntaxKind::MethodDeclaration | SyntaxKind::GetAccessor | SyntaxKind::SetAccessor => {
                // For decorators with only two parameters we supply only two arguments
                if self.get_parameter_count(signature) <= 2 {
                    return 2;
                }
                3
            }
            SyntaxKind::Parameter => 3,
            _ => panic!("Unhandled case in getLegacyDecoratorArgumentCount"),
        }
    }

    // Go: checker/checker.go:9188 hasCorrectTypeArgumentArity
    pub fn has_correct_type_argument_arity(
        &mut self,
        signature: SignatureId,
        type_arguments: &[Node],
    ) -> bool {
        // If the user supplied type arguments, but the number of type arguments does not match
        // the declared number of type parameters, the call has an incorrect arity.
        let len = type_arguments.len() as i32;
        // PERF: callcopy1. Go reads the minimum count first; it only reads
        // declarations, so a call with no type arguments skips it and the
        // list copy.
        if len == 0 {
            return true;
        }
        let type_parameters = self.sig(signature).type_parameters.clone();
        let num_type_parameters = type_parameters.len() as i32;
        let min_type_argument_count = self.get_min_type_argument_count(&type_parameters);
        len >= min_type_argument_count && len <= num_type_parameters
    }

    // Go: checker/checker.go:9196 checkTypeArguments
    // PORT: Go returns a nil slice on failure; the port returns `None`.
    pub fn check_type_arguments(
        &mut self,
        signature: SignatureId,
        type_argument_nodes: &[Node],
        report_errors: bool,
        head_message: Option<&'static crate::diagnostics::Message>,
    ) -> Option<Vec<TypeId>> {
        let is_java_script = is_in_js_file(self.sig(signature).declaration);
        let type_parameters = self.sig(signature).type_parameters.clone();
        let mut mapped: Vec<TypeId> = Vec::with_capacity(type_argument_nodes.len());
        for &n in type_argument_nodes {
            mapped.push(self.get_type_from_type_node(n));
        }
        let min_type_argument_count = self.get_min_type_argument_count(&type_parameters);
        let type_argument_types = self.fill_missing_type_arguments(
            &mapped,
            &type_parameters,
            min_type_argument_count,
            is_java_script,
        );
        let mut mapper = MapperId::NIL;
        for i in 0..type_argument_nodes.len() {
            debug_assert!(
                i < type_parameters.len() && type_parameters[i].is_some(),
                "Should not call checkTypeArguments with too many type arguments"
            );
            let constraint = self.get_constraint_of_type_parameter(type_parameters[i]);
            if constraint.is_some() {
                let type_argument_head_message =
                    head_message.unwrap_or(diag::Type_0_does_not_satisfy_the_constraint_1);
                if mapper.is_nil() {
                    mapper = self.new_type_mapper(&type_parameters, &type_argument_types);
                }
                let type_argument = type_argument_types[i];
                let mut error_node = Node::NIL;
                if report_errors {
                    error_node = type_argument_nodes[i];
                }
                let mut diags: Vec<Diagnostic> = Vec::new();
                let instantiated = self.instantiate_type(constraint, mapper);
                let target = self.get_type_with_this_argument(instantiated, type_argument, false);
                if !self.check_type_assignable_to_ex(
                    type_argument,
                    target,
                    error_node,
                    Some(type_argument_head_message),
                    Some(&mut diags),
                ) {
                    if !diags.is_empty() {
                        let mut diagnostic = diags.swap_remove(0);
                        if head_message.is_some() {
                            diagnostic = new_diagnostic_chain(
                                Some(diagnostic),
                                diag::Type_0_does_not_satisfy_the_constraint_1,
                                args![],
                            );
                        }
                        self.add_diagnostic(diagnostic);
                    }
                    return None;
                }
            }
        }
        Some(type_argument_types)
    }

    // Go: checker/checker.go:9230 isSignatureApplicable
    pub fn is_signature_applicable(
        &mut self,
        node: Node,
        args: &[Node],
        signature: SignatureId,
        relation: &Rc<RefCell<Relation>>,
        check_mode: CheckMode,
        report_errors: bool,
        mut diagnostic_output: Option<&mut Vec<Diagnostic>>,
    ) -> bool {
        if is_jsx_call_like(node) {
            return self.check_applicable_signature_for_jsx_call_like_element(
                node,
                signature,
                relation,
                check_mode,
                report_errors,
                diagnostic_output,
            );
        }
        let this_type = self.get_this_type_of_signature(signature);
        if this_type.is_some()
            && this_type != self.void_type
            && !(is_new_expression(node)
                || is_call_expression(node) && is_super_property(node.expression()))
        {
            // If the called expression is not of the form `x.f` or `x["f"]`, then sourceType = voidType
            // If the signature's 'this' type is voidType, then the check is skipped -- anything is compatible.
            // If the expression is a new expression or super call expression, then the check is skipped.
            let this_argument_node = self.get_this_argument_of_call(node);
            let this_argument_type = self.get_this_argument_type(this_argument_node);
            let mut error_node = Node::NIL;
            if report_errors {
                error_node = this_argument_node;
                if error_node.is_nil() {
                    error_node = node;
                }
            }
            let head_message =
                diag::The_this_context_of_type_0_is_not_assignable_to_method_s_this_of_type_1;
            if !self.check_type_related_to_ex(
                this_argument_type,
                this_type,
                relation,
                error_node,
                Some(head_message),
                diagnostic_output.as_deref_mut(),
            ) {
                return false;
            }
        }
        let head_message = diag::Argument_of_type_0_is_not_assignable_to_parameter_of_type_1;
        let rest_type = self.get_non_array_rest_type(signature);
        let arg_count: i32;
        if rest_type.is_some() {
            arg_count = (self.get_parameter_count(signature) - 1).min(args.len() as i32);
        } else {
            arg_count = args.len() as i32;
        }
        for i in 0..arg_count {
            let arg = args[i as usize];
            if !is_omitted_expression(arg) {
                let param_type = self.get_type_at_position(signature, i);
                let arg_type = self.check_expression_with_contextual_type(
                    arg,
                    param_type,
                    InferenceContextId::NIL, /*inferenceContext*/
                    check_mode,
                );
                // If one or more arguments are still excluded (as indicated by CheckMode.SkipContextSensitive),
                // we obtain the regular type of any object literal arguments because we may not have inferred complete
                // parameter types yet and therefore excess property checks may yield false positives (see #17041).
                let check_arg_type: TypeId;
                if check_mode.intersects(CheckMode::SKIP_CONTEXT_SENSITIVE) {
                    check_arg_type = self.get_regular_type_of_object_literal(arg_type);
                } else {
                    check_arg_type = arg_type;
                }
                let effective_check_argument_node = self.get_effective_check_node(arg);
                if !self.check_type_related_to_and_optionally_elaborate(
                    check_arg_type,
                    param_type,
                    relation,
                    if report_errors {
                        effective_check_argument_node
                    } else {
                        Node::NIL
                    },
                    effective_check_argument_node,
                    Some(head_message),
                    diagnostic_output.as_deref_mut(),
                ) {
                    self.maybe_add_missing_await_info(
                        arg,
                        check_arg_type,
                        param_type,
                        relation,
                        report_errors,
                        diagnostic_output.as_deref_mut(),
                    );
                    return false;
                }
            }
        }
        if rest_type.is_some() {
            let spread_type = self.get_spread_argument_type(
                args,
                arg_count,
                args.len() as i32,
                rest_type,
                InferenceContextId::NIL, /*context*/
                check_mode,
            );
            let rest_arg_count = args.len() as i32 - arg_count;
            let mut error_node = Node::NIL;
            if report_errors {
                match rest_arg_count {
                    0 => error_node = node,
                    1 => error_node = self.get_effective_check_node(args[arg_count as usize]),
                    _ => {
                        error_node =
                            self.create_synthetic_expression(node, spread_type, false, Node::NIL);
                        set_node_loc(
                            error_node,
                            TextRange::new(
                                args[arg_count as usize].pos(),
                                args[args.len() - 1].end(),
                            ),
                        );
                    }
                }
            }
            if !self.check_type_related_to_ex(
                spread_type,
                rest_type,
                relation,
                error_node,
                Some(head_message),
                diagnostic_output.as_deref_mut(),
            ) {
                self.maybe_add_missing_await_info(
                    error_node,
                    spread_type,
                    rest_type,
                    relation,
                    report_errors,
                    diagnostic_output.as_deref_mut(),
                );
                return false;
            }
        }
        true
    }

    // Go: checker/checker.go:9305 maybeAddMissingAwaitInfo
    pub fn maybe_add_missing_await_info(
        &mut self,
        error_node: Node,
        source: TypeId,
        target: TypeId,
        relation: &Rc<RefCell<Relation>>,
        report_errors: bool,
        diagnostic_output: Option<&mut Vec<Diagnostic>>,
    ) {
        if let Some(diagnostic_output) = diagnostic_output {
            if error_node.is_some() && report_errors && !diagnostic_output.is_empty() {
                // Bail if target is Promise-like---something else is wrong
                if self.get_awaited_type_of_promise(target).is_some() {
                    return;
                }
                let awaited_type_of_source = self.get_awaited_type_of_promise(source);
                if awaited_type_of_source.is_some()
                    && self.is_type_related_to(awaited_type_of_source, target, relation)
                {
                    diagnostic_output[0].add_related_info(Some(new_diagnostic_for_node(
                        error_node,
                        diag::Did_you_forget_to_use_await,
                        args![],
                    )));
                }
            }
        }
    }

    // Returns the `this` argument node in calls like `x.f(...)` and `x[f](...)`. `nil` otherwise.
    // Go: checker/checker.go:9319 getThisArgumentOfCall
    pub fn get_this_argument_of_call(&self, node: Node) -> Node {
        if is_binary_expression(node) {
            return node.right();
        }
        let mut expression = Node::NIL;
        if is_call_expression(node) {
            expression = node.expression();
        } else if is_tagged_template_expression(node) {
            expression = node.tag();
        } else if is_decorator(node) && !self.legacy_decorators {
            expression = node.expression();
        }
        if expression.is_some() {
            let callee = skip_outer_expressions(expression, OuterExpressionKinds::OEK_ALL);
            if is_access_expression(callee) {
                return callee.expression();
            }
        }
        Node::NIL
    }

    // Go: checker/checker.go:9341 getThisArgumentType
    pub fn get_this_argument_type(&mut self, node: Node) -> TypeId {
        if node.is_nil() {
            return self.void_type;
        }
        let this_argument_type = self.check_expression(node);
        if is_optional_chain_root(node.parent()) {
            return self.get_non_nullable_type(this_argument_type);
        } else if is_optional_chain(node.parent()) {
            return self.remove_optional_type_marker(this_argument_type);
        }
        this_argument_type
    }
}
