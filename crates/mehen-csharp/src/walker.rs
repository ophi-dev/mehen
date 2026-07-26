// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ANTLR-based C# metric walker.
//!
//! Drives a recursive descent over the ANTLR `ParseTree` (entry rule
//! `compilation_unit`) and produces a populated [`MetricSpace`]. The structure
//! mirrors the `mehen-java` / `mehen-kotlin` walkers — one [`State`] per space,
//! finalize-and-merge on close, with the parent-less ANTLR tree handled by
//! threading context **top-down**.
//!
//! ## Grammar shape (vs Java)
//!
//! The grammars-v4 C# grammar differs from the Java one in two ways that shape
//! every classification here:
//!
//! - **Control flow lives in labeled alternatives of one rule.** `if`, `switch`,
//!   `while`, `do`, `for`, `foreach`, `try`, `lock`, `using`, `return`, `throw`,
//!   `break`, `continue`, `goto`, `yield` are all alternatives of
//!   `simple_embedded_statement`, each discriminated by a leading keyword token
//!   that is a direct child. So the walker inspects that rule's tokens rather
//!   than matching distinct rule indices (same approach as the Java walker's
//!   `statement` handling). Which of those keywords actually *score* is a
//!   separate question — see the per-metric list below.
//! - **Operators are a precedence *cascade* of named rules.** Unlike Java's one
//!   flat `expression` rule, C# spells each precedence level as its own rule
//!   (`conditional_and_expression`, `conditional_or_expression`,
//!   `equality_expression`, `relational_expression`, …). That makes operator
//!   classification direct: a `conditional_and_expression` carrying `&&`
//!   tokens *is* the short-circuit node, and a level with no operator token is
//!   a transparent pass-through. The boolean-run collapse therefore uses the
//!   `mehen-metrics` `observe_boolean` sequence accumulator (like the Kotlin
//!   walker) rather than Java's tree-flattening, with explicit resets at
//!   statement and call-argument boundaries.
//!
//! ## Metric coverage (SonarC#-aligned)
//!
//! - **Cyclomatic**: `if`, every loop (`while`/`do`/`for`/`foreach`), each
//!   `case` label, the ternary `?:`, and each short-circuit `&&`/`||`. `catch`,
//!   `switch` itself, and `default:` are not decisions (matches SonarC#, which
//!   follows the same rule as SonarJava; `catch` counts only in cognitive).
//! - **Cognitive**: nesting on `if`, loops, `switch`, `catch`, and the ternary;
//!   flat `+1` on `else`/`else if` and on `goto`; a sequence-collapsing boolean
//!   run on `&&`/`||` (+1 per operator-kind change, reset per statement and per
//!   call argument, matching SonarSource). `try` and `lock` add nothing — the
//!   spec increments on the *handler* (`catch`), not the guarded block, and
//!   `lock` is not an increment at all.
//! - **ABC**: assignments via `assignment` (all `=`/compound/`??=` forms),
//!   `++`/`--`, and any initialized declarator
//!   (`local_variable_declarator`/`variable_declarator`/`constant_declarator`/
//!   `enum_member_declaration`/`arg_declaration` default); branches via every
//!   `method_invocation`, `object_creation_expression`, and
//!   `constructor_initializer` (NOT `member_access` — that is qualification,
//!   not a call, so a qualified call still scores exactly one branch);
//!   conditions via
//!   `if`/`case`/`catch`/`when`-filter/loops/comparison & equality/`&&`/`||`/
//!   ternary/`??`/`is`/`as` (bit-shifts `<<`/`>>` are excluded — not
//!   relational).
//! - **NExit**: `return`, `throw` (statement and expression forms), and
//!   `yield return`/`yield break`.
//! - **NArgs**: `formal_parameter_list` count for methods/constructors/
//!   local functions/indexers; the two `arg_declaration`s of an operator;
//!   `anonymous_function_signature` count for lambdas and anonymous methods.
//! - **NOM**: every `method_declaration`, `constructor_declaration`,
//!   `destructor_definition`, `operator_declaration`,
//!   `conversion_operator_declarator`, `local_function_declaration`, property/
//!   indexer/event accessor (`get`/`set`/`add`/`remove`) is a function space;
//!   every `lambda_expression` and `anonymousMethodExpression` is a
//!   closure-shaped function space.
//! - **LOC**: PLOC from per-space code-token rows during the walk, LLOC from
//!   statement/declaration-shaped rules, CLOC from a source-ordered pass over
//!   the hidden-channel comment tokens routed via `SpaceRangeTracker`.
//! - **Halstead**: per-token operator/operand classification — keywords and
//!   punctuation are operators; identifiers, literals, `this`, `base` are
//!   operands (deduped by text).
//! - **NPA / NPM / WMC**: class-vs-interface routing by the type-definition
//!   keyword (`struct` counts as a class-like container; `interface` as an
//!   interface). NPA counts `field_declaration` variables, `constant_declaration`
//!   declarators, `event_declaration` variables, and `enum_member_declaration`s
//!   directly under a type body. NPM counts methods/constructors/operators/
//!   properties/indexers directly under a type body. C# visibility: a type
//!   member with no access modifier is `private` (NOT public), so only an
//!   explicit `public` counts toward NPA/NPM; interface members are implicitly
//!   public. `enum` members are implicitly public.

use mehen_antlr::runtime::token::Token;
use mehen_antlr::runtime::{Node, RuleNodeView, TerminalNodeView};
use mehen_antlr::{LocToken, LocTokenKind, ctx_span};
use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricTreeBuilder, SpaceRangeTracker, State,
    apply_state_to, finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

use mehen_csharp_parser::c_sharp_lexer as cl;
use mehen_csharp_parser::c_sharp_parser as cp;

/// Drive the walk over the parsed `compilation_unit` tree and return the unit
/// `MetricSpace`. LOC is computed from `loc_tokens` in a single ordered pass
/// *after* the tree walk has opened and closed every space.
pub(crate) fn walk(
    tree: Node<'_>,
    line_index: &LineIndex,
    source_len: usize,
    loc_tokens: &[LocToken],
) -> MetricSpace {
    let unit_span = match tree.as_rule() {
        Some(rule) => ctx_span(rule, line_index, source_len),
        None => mehen_core::SourceSpan::empty(),
    };

    let mut unit_state = State::new();
    unit_state
        .loc
        .set_span(0, line_index.line_count().saturating_sub(1), true);

    let mut walker = Walker {
        line_index,
        source_len,
        tree: MetricTreeBuilder::new(unit_span),
        stack: vec![unit_state],
        kinds: vec![SpaceKind::Unit],
        suppress_parent_wmc: vec![false],
        cognitive: CognitiveContext::default(),
        loc_routing: SpaceRangeTracker::new(),
    };

    if let Some(rule) = tree.as_rule() {
        for child in rule.children() {
            walker.visit(child, &ChildHint::default());
        }
    }

    let mut unit_state = walker.stack.pop().expect("walker stack underflow");

    // CLOC pass: route each comment to the deepest enclosing space (or the
    // unit) in source order (mirrors `mehen-java`/`mehen-kotlin`).
    for t in loc_tokens {
        if t.kind == LocTokenKind::Comment {
            walker.loc_routing.observe_comment(
                t.start_byte,
                t.end_byte,
                &mut unit_state.loc,
                t.start_row,
                t.end_row,
            );
        }
    }

    finalize_state(&mut unit_state);

    let mut root = walker.tree.finish();
    let mut unit_halstead = std::mem::take(&mut unit_state.halstead);
    let mut unit_loc = std::mem::take(&mut unit_state.loc);
    walker
        .loc_routing
        .finalize_into_tree(&mut root, &mut unit_halstead, &mut unit_loc);
    unit_state.halstead = unit_halstead;
    unit_state.loc = unit_loc;
    apply_state_to(unit_state, &mut root.metrics);
    root
}

/// Per-frame cognitive context — the `(nesting, depth, lambda)` triple used
/// exactly as the Java/Kotlin walkers use it.
#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

/// Context threaded *down* into a child during the walk (ANTLR contexts have
/// no parent pointer).
///
/// `Clone` (not `Copy`): `accessor_owner` carries an owned name so a property's
/// accessors can be named after it. Cloning a `SmolStr` is a cheap refcount
/// bump / inline copy, so threading the hint per child stays allocation-free in
/// the common case.
#[derive(Clone, Debug, Default)]
struct ChildHint {
    /// This `if_body`/`embedded_statement` is the `else`-branch body of an
    /// enclosing `if`. An `if` reached through this hint is an `else if` and
    /// must not add cognitive nesting (only the flat `else` +1 applies).
    is_else_branch: bool,
    /// This node is a direct member position of the enclosing type body, so
    /// NPA/NPM should consider it.
    in_type_member: bool,
    /// The container kind of the enclosing type body, so a member's counters
    /// route to class-vs-interface buckets and inherit the
    /// interface-default-public rule.
    member_container: Option<ContainerKind>,
    /// The member's resolved visibility, captured at the member-declaration
    /// wrapper (`class_member_declaration: attributes? all_member_modifiers?
    /// (…)`) where the modifiers are siblings of the inner declaration — the
    /// declaration itself has no parent pointer and does not carry them.
    /// `None` outside a member position.
    member_is_public: Option<bool>,
    /// This node is (within) a `for` statement's initializer, so a
    /// `local_variable_declaration` reached through it is the loop initializer,
    /// not a standalone statement — it must not add its own LLOC (the `for`
    /// statement already contributes the single header logical line). Also set
    /// for a `using`/`fixed` resource acquisition and a `foreach` header.
    in_for_init: bool,
    /// This terminal is the token of an `identifier` rule. C#'s contextual
    /// keywords (`var`, `async`, `await`, `get`, `set`, `value`, `when`,
    /// `where`, `from`, `select`, `nameof`, …) lex as dedicated token types but
    /// are identifiers in name position, so a terminal reached through this
    /// hint is a Halstead *operand* regardless of its token type (mirrors the
    /// Java walker's `identifier` handling).
    in_identifier: bool,
    /// We are inside an `attributes` list (`[Obsolete("x")]`). Attribute
    /// arguments are compile-time metadata, not executable code, so
    /// cyclomatic/cognitive/ABC accounting is suppressed for the whole subtree
    /// (LOC/Halstead still count — the tokens physically exist).
    in_attributes: bool,
    /// The 0-based start line + byte of the enclosing member's declaration
    /// wrapper (`class_member_declaration`/`struct_member_declaration`/
    /// `type_declaration`), threaded down so a method/type space can widen its
    /// span upward to cover its own-line attributes and modifiers. In this
    /// grammar `attributes` and `all_member_modifiers` are SIBLINGS of the
    /// inner declaration, so the declaration's own `ctx_span` starts *after*
    /// them — leaving `[Obsolete]\npublic void M() {}`'s attribute row
    /// attributed to the enclosing type. `None` outside a member position.
    member_decl_start: Option<(u32, u32)>,
    /// This declaration's space was already opened by its enclosing member
    /// wrapper (so the wrapper's own-line attributes/modifiers are visited
    /// *inside* the space, giving the member correct Halstead/PLOC/span). The
    /// declaration node must therefore NOT open a second space of its own.
    space_opened_by_wrapper: bool,
    /// The declared name of the enclosing `typed_member_declaration` /
    /// property-bearing member, threaded down so a `get`/`set` accessor space
    /// can be named `Prop.get` rather than anonymous.
    accessor_owner: Option<SmolStr>,
}

struct Walker<'a> {
    line_index: &'a LineIndex,
    source_len: usize,
    tree: MetricTreeBuilder,
    stack: Vec<State>,
    kinds: Vec<SpaceKind>,
    /// Parallel to `stack`/`kinds`: whether the closing function space must NOT
    /// contribute its cyclomatic to the parent's WMC. Set for local functions
    /// and lambdas, whose complexity belongs to the enclosing method (already
    /// counted there), not as a separate weighted method of the class.
    suppress_parent_wmc: Vec<bool>,
    cognitive: CognitiveContext,
    loc_routing: SpaceRangeTracker,
}

impl Walker<'_> {
    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    fn visit(&mut self, node: Node<'_>, hint: &ChildHint) {
        if let Some(rule) = node.as_rule() {
            self.visit_rule(rule, hint);
        } else if let Some(term) = node.as_terminal() {
            self.visit_terminal(term, hint);
        }
        // Error leaves carry no metric contribution; they are surfaced as
        // diagnostics by `mehen_antlr::collect_errors` in the analyzer.
    }

    fn visit_terminal(&mut self, term: TerminalNodeView<'_>, hint: &ChildHint) {
        let tt = term.symbol().token_type();

        // Cognitive: `else` adds a flat +1 (covers `else if`). The boolean
        // operator tokens feed the sequence collapser; unlike Java's flat
        // `expression` rule, C#'s precedence cascade gives each `&&`/`||` its
        // own node, so observing the tokens in source order is exactly
        // SonarSource's flattened sequence.
        if !hint.in_attributes {
            match tt {
                cl::ELSE => self.current().cognitive.increment_by_one(),
                cl::OP_AND => self.current().cognitive.observe_boolean("&&"),
                cl::OP_OR => self.current().cognitive.observe_boolean("||"),
                _ => {}
            }

            // Cyclomatic: each short-circuit boolean operator token is a
            // decision (independent of the cognitive run collapse).
            if matches!(tt, cl::OP_AND | cl::OP_OR) {
                self.current().cyclomatic.record_decision();
            }

            // ABC conditions: comparison / equality / boolean / null-coalescing
            // operator tokens. Relational `<`/`>` are handled at the
            // `relational_expression` rule (the grammar spells `<<`/`>>` with
            // bare `LT`/`GT` runs, so a bare token probe cannot tell a shift
            // from a comparison).
            if is_abc_condition_token(tt) {
                self.current().abc.record_condition();
            }

            // ABC assignments: `++`/`--` (Fitzpatrick lists both under A).
            // The `=`/compound forms are handled at the `assignment` rule.
            if matches!(tt, cl::OP_INC | cl::OP_DEC) {
                self.current().abc.record_assignment();
            }
        }

        // Halstead operator/operand token classification. A terminal reached
        // through an `identifier` rule is always an operand — this covers C#'s
        // contextual keywords (`var`, `async`, `get`, `where`, …) used as names,
        // which carry dedicated token types but are identifiers here.
        let class = if hint.in_identifier {
            HalsteadClass::Operand
        } else {
            halstead_class(tt)
        };
        match class {
            HalsteadClass::Operator => {
                self.current().halstead.observe_operator(HalsteadOperator {
                    kind: SmolStr::new(kp_token_name(tt)),
                    text: None,
                });
            }
            HalsteadClass::Operand => {
                let text = term.symbol().text_or_empty();
                self.current().halstead.observe_operand(HalsteadOperand {
                    kind: SmolStr::new("Operand"),
                    text: Some(SmolStr::new(text)),
                });
            }
            HalsteadClass::Skip => {}
        }

        // PLOC: a visible code token's start row is a code line, recorded into
        // the current space during the AST walk. Comments are hidden-channel
        // (routed after the walk), and EOF (`tt < 0`) is not code.
        //
        // A single visible token can span multiple physical lines — a verbatim
        // string (`@"…"`, `VERBATIUM_STRING`) or an interpolated-string content
        // token is one token covering several rows. Record *every* row it
        // covers as code, or the interior rows sit inside the enclosing span
        // with no PLOC observation and are reported as phantom blank lines
        // (`blank = sloc - ploc - only_comment`).
        if tt >= 0 {
            let start_row = (term.symbol().line() as u32).saturating_sub(1);
            let extra_rows = term
                .symbol()
                .text_or_empty()
                .bytes()
                .filter(|&b| b == b'\n')
                .count() as u32;
            for row in start_row..=start_row.saturating_add(extra_rows) {
                self.current().loc.observe_code_line(row);
            }
        }
    }

    fn visit_rule(&mut self, ctx: RuleNodeView<'_>, hint: &ChildHint) {
        let ri = ctx.rule_index();
        // Snapshot the cognitive context of the *enclosing* construct before
        // anything in this subtree mutates it. Must happen before
        // `maybe_open_space`, which resets nesting/lambda and bumps depth for
        // function spaces.
        let saved_cognitive = self.cognitive;

        // NPA / NPM: classify a direct member of the enclosing type body
        // before opening any space for this node (so the kinds stack still has
        // the type on top). A member whose space was already opened by its
        // wrapper had its NPM recorded there; skip re-classifying here (this
        // node now sits inside the member space, so it would misroute NPM).
        if hint.in_type_member
            && !hint.space_opened_by_wrapper
            && let Some(container) = hint.member_container
        {
            let public = hint.member_is_public.unwrap_or(true);
            self.classify_type_member(ctx, ri, container, public);
        }

        // Capture the enclosing type container BEFORE opening any space for
        // this node: a member wrapper may open a nested type space here, and
        // `member_propagation` (run after the open) would then see that
        // just-opened type instead of the real enclosing scope.
        let container_before_open = self.enclosing_container();

        let opened = self.maybe_open_space(ctx, ri, hint);
        self.classify_rule(ctx, ri, hint);

        // A call argument (`G(a && b)`) is an independent boolean context: its
        // inner short-circuit run must not collapse with a same-kind operator
        // outside the call, and vice-versa. Save the enclosing run's `last_op`,
        // start the argument fresh, then restore it so the *outer* run
        // continues across the call as if it were a single operand. Same for a
        // parenthesized sub-expression's interior? No: parentheses ARE
        // transparent to SonarSource's flattening, so only arguments isolate.
        let saved_bool = if ri == cp::RULE_ARGUMENT {
            Some(self.current().cognitive.boolean_seq.last_op.take())
        } else {
            None
        };

        self.visit_children(ctx, ri, hint, container_before_open);

        if let Some(prev) = saved_bool {
            self.current().cognitive.boolean_seq.last_op = prev;
        }

        if opened {
            self.close_space();
        }

        // The second accessor of a property/indexer/event is a *nested* child
        // of the first accessor's rule in this grammar, but the two are
        // siblings semantically. `visit_children` skipped it; visit it now that
        // the first accessor's space has closed, so it becomes a sibling space
        // of the first and still sees the owner name from the inbound hint.
        if let Some(sibling) = accessor_sibling(ctx, ri) {
            self.visit_rule(sibling, hint);
        }
        self.cognitive = saved_cognitive;
    }

    fn visit_children(
        &mut self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        hint: &ChildHint,
        container_before_open: Option<ContainerKind>,
    ) {
        // `NodeChildren` is a cheap `Clone` slice-iterator, so it is re-walked
        // (below, and for the `else` scan here) without allocating.

        // For an `if` statement, the `else`-branch body is the `if_body` that
        // appears after the `ELSE` token. Tag it so an `if` reached through it
        // (without an intervening `block`) is recognized as `else if` and does
        // not add nesting.
        let else_body_idx = if is_if_statement(ctx, ri) {
            else_branch_index(ctx.children())
        } else {
            None
        };
        // `is_else_branch` also flows through the transparent body wrappers
        // (`if_body`, `embedded_statement`, `statement`) so an `else if` chain
        // is recognized. It must NOT flow through a `block` (`else { if … }` is
        // genuinely nested) nor through a statement that introduces its own
        // control-flow construct.
        let propagate_else = hint.is_else_branch && is_else_transparent(ctx, ri);

        // Type body member positions originate at the member-declaration
        // wrappers, then flow through the transparent `common_member_declaration`
        // / `typed_member_declaration` wrappers to the real member rule. The
        // visibility is resolved at the wrapper because `all_member_modifiers`
        // is a sibling of the inner declaration, not its child.
        let (propagate_member, member_container, member_is_public) =
            self.member_propagation(ctx, ri, hint, container_before_open);

        // Capture the member wrapper's start so a member space can widen its
        // span upward over its own-line attributes/modifiers. A nested type or
        // function resets it (their members compute from their own wrappers).
        let member_decl_start = if is_member_wrapper(ri) {
            let span = ctx_span(ctx, self.line_index, self.source_len);
            Some((span.start_byte, span.start_line))
        } else if opens_type_like(ri) || opens_function_space(ri) {
            None
        } else {
            hint.member_decl_start
        };

        // A `for`/`foreach`/`using`/`fixed` header declaration must not let its
        // declaration add a second LLOC — the statement already contributes the
        // single header logical line. Tag ONLY the direct children of the
        // header-bearing rules (a non-sticky flag), so a real local declaration
        // nested inside a lambda in the initializer still counts.
        let in_for_init = matches!(
            ri,
            cp::RULE_FOR_INITIALIZER | cp::RULE_FOR_ITERATOR | cp::RULE_RESOURCE_ACQUISITION
        );

        // A terminal directly under `identifier` is a name → Halstead operand
        // (covers C# contextual keywords used as identifiers).
        let in_identifier = ri == cp::RULE_IDENTIFIER;

        // Once inside `attributes`, stay inside for the whole subtree so
        // attribute metadata records no executable complexity.
        let in_attributes = hint.in_attributes
            || matches!(ri, cp::RULE_ATTRIBUTES | cp::RULE_GLOBAL_ATTRIBUTE_SECTION);

        // Thread the property/indexer/event name down so its accessors can be
        // named `Prop.get` / `Prop.set`. Set at the member-bearing declaration;
        // a nested function/type resets it.
        let accessor_owner = if let Some(name) = accessor_owner_name(ctx, ri) {
            Some(name)
        } else if opens_type_like(ri) || opens_function_space(ri) {
            None
        } else {
            hint.accessor_owner.clone()
        };

        // When this wrapper opened the member OR type space itself (to capture
        // own-line attributes/modifiers), tell the inner declaration to skip
        // its own open. The flag flows through the transparent
        // `common_member_declaration`/`typed_member_declaration` wrappers to
        // the declaration node, which consumes it; a real space open clears it
        // so a nested declaration inside the body still opens normally.
        let opened_at_wrapper = is_member_wrapper(ri)
            && (wrapper_inner_function(ctx).is_some() || wrapper_inner_type(ctx).is_some());
        let space_opened_by_wrapper = if opens_function_space(ri) || opens_type_like(ri) {
            false
        } else {
            opened_at_wrapper || hint.space_opened_by_wrapper
        };

        // The second accessor is nested in the first accessor's rule but is
        // semantically its sibling; `visit_rule` visits it after this space
        // closes, so skip it here (see `accessor_sibling`).
        let sibling_accessor_id = accessor_sibling(ctx, ri).map(|s| s.node().id());

        for (idx, child) in ctx.children().enumerate() {
            if let Some(skip_id) = sibling_accessor_id
                && child.as_rule().is_some_and(|r| r.node().id() == skip_id)
            {
                continue;
            }
            let child_hint = ChildHint {
                is_else_branch: Some(idx) == else_body_idx || propagate_else,
                in_type_member: propagate_member,
                member_container,
                member_is_public,
                in_for_init,
                in_identifier,
                in_attributes,
                member_decl_start,
                space_opened_by_wrapper,
                accessor_owner: accessor_owner.clone(),
            };
            self.visit(child, &child_hint);
        }
    }

    /// Compute the `(in_type_member, container, is_public)` hint for this
    /// rule's children. Members reach their declaration through transparent
    /// wrapper layers; the container comes from the enclosing space kind and
    /// the visibility is resolved from the wrapper's `all_member_modifiers`
    /// (siblings of the member declaration).
    fn member_propagation(
        &self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        hint: &ChildHint,
        container_before_open: Option<ContainerKind>,
    ) -> (bool, Option<ContainerKind>, Option<bool>) {
        match ri {
            // The member-declaration wrappers open a member position; the
            // container is the type-like currently on the kinds stack, and the
            // visibility is resolved from this wrapper's `all_member_modifiers`.
            cp::RULE_CLASS_MEMBER_DECLARATION | cp::RULE_STRUCT_MEMBER_DECLARATION => {
                // Use the container captured BEFORE this node's
                // `maybe_open_space` — a member wrapper may have just opened a
                // nested type space, so `self.enclosing_container()` here would
                // wrongly report that type.
                let container = container_before_open;
                // C# visibility semantics: a class/struct member with no access
                // modifier is *private*, which is NOT public, so the default is
                // `false` — only an explicit `public` counts toward NPA/NPM.
                // Interface members are implicitly public.
                let default_public = matches!(container, Some(ContainerKind::Interface));
                let public = visibility_from_modifiers(ctx).unwrap_or(default_public);
                (true, container, Some(public))
            }
            // An interface member carries no access modifiers at all (they are
            // implicitly public) and holds its signature inline rather than in
            // a nested declaration rule.
            cp::RULE_INTERFACE_MEMBER_DECLARATION => (true, container_before_open, Some(true)),
            // An `enum` member is implicitly public; it is a direct child of
            // `enum_body` with no modifier wrapper.
            cp::RULE_ENUM_BODY => (true, container_before_open, Some(true)),
            // A top-level / namespace-level type's modifiers live on the
            // `type_declaration` wrapper. Thread the visibility down WITHOUT
            // marking the type itself as a member (a top-level type is not
            // counted in NPA/NPM), so `propagate_member` stays false.
            cp::RULE_TYPE_DECLARATION => {
                let public = visibility_from_modifiers(ctx).unwrap_or(false);
                (false, None, Some(public))
            }
            // Transparent member wrappers keep the inbound member position, so
            // the hint reaches the real declaration one or two levels deeper.
            cp::RULE_COMMON_MEMBER_DECLARATION | cp::RULE_TYPED_MEMBER_DECLARATION => (
                hint.in_type_member,
                hint.member_container,
                hint.member_is_public,
            ),
            _ => (false, None, None),
        }
    }

    /// The `ContainerKind` of the type-like space currently on top of the kinds
    /// stack (for member NPA/NPM routing), or `None` if the top is not a
    /// type-like scope.
    fn enclosing_container(&self) -> Option<ContainerKind> {
        match self.kinds.last() {
            Some(SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum) => {
                Some(ContainerKind::Class)
            }
            Some(SpaceKind::Interface | SpaceKind::Trait) => Some(ContainerKind::Interface),
            _ => None,
        }
    }

    /// Open a `Function` space for a method-shaped member. `span_ctx` supplies
    /// the span (the member wrapper when opening at the wrapper, so the span
    /// covers own-line attributes/modifiers; otherwise the declaration itself);
    /// `fn_ctx` supplies the name and NArgs.
    fn open_function_space(
        &mut self,
        span_ctx: RuleNodeView<'_>,
        fn_ctx: RuleNodeView<'_>,
        hint: &ChildHint,
        kind: SpaceKind,
    ) {
        let name = function_name(fn_ctx, hint);
        // Node identity: the arena addresses every node by a `NodeId`, so
        // "different node" is an id comparison, not pointer equality.
        let opened_at_wrapper = span_ctx.node().id() != fn_ctx.node().id();
        // When opening at the wrapper, NPM must be recorded into the enclosing
        // type BEFORE the member space is pushed (member classification
        // normally runs at the inner declaration, but that node now sits inside
        // this space and would misroute NPM into the member).
        if opened_at_wrapper && let Some(container) = self.enclosing_container() {
            let default_public = matches!(container, ContainerKind::Interface);
            let public = visibility_from_modifiers(span_ctx).unwrap_or(default_public);
            self.current().npm.record_method(container, public);
        }
        // Widen the declaration-node span up to its member wrapper so own-line
        // attributes/modifiers belong to the member. Unused when opening at the
        // wrapper (the span already starts there).
        let widened = if opened_at_wrapper {
            None
        } else {
            hint.member_decl_start
        };
        let mut state = self.new_space_state_widened(span_ctx, widened);
        // When opening at the declaration node, the attribute/modifier rows
        // were already visited (PLOC-counted) on the enclosing type before this
        // space is pushed, so adopt those rows into the member.
        if let Some((_, wrapper_start_line)) = widened {
            let own_start_line = ctx_span(span_ctx, self.line_index, self.source_len).start_line;
            if wrapper_start_line < own_start_line {
                let parent_loc = self.current().loc.clone();
                state.loc.adopt_code_lines_in_range(
                    &parent_loc,
                    wrapper_start_line.saturating_sub(1),
                    own_start_line.saturating_sub(1),
                );
            }
        }
        let is_closure = matches!(kind, SpaceKind::Closure);
        if is_closure {
            state.nom.record_closure();
            state.nargs.record_closure_args(count_args(fn_ctx));
        } else {
            state.nom.record_function();
            state.nargs.record_function_args(count_args(fn_ctx));
        }
        // A local function's and a lambda's complexity belongs to the enclosing
        // method (already counted there), so neither rolls into the type's WMC.
        let suppress_wmc = is_closure || fn_ctx.rule_index() == cp::RULE_LOCAL_FUNCTION_DECLARATION;
        self.push_space_widened(kind, name, span_ctx, state, suppress_wmc, widened);
        self.enter_function_cognitive(is_closure);
    }

    /// Open a type-like (`Class`/`Enum`/`Interface`) space for `type_ctx`.
    /// `span_ctx` supplies the span — the wrapper when opening there (so
    /// own-line attributes/modifiers are covered), otherwise the definition.
    fn open_type_space(&mut self, span_ctx: RuleNodeView<'_>, type_ctx: RuleNodeView<'_>) {
        let name = name_from_identifier(type_ctx);
        let ri = type_ctx.rule_index();
        let mut state = self.new_space_state(span_ctx);
        state.npa.record_class_like();
        state.npm.record_class_like();
        let kind = match ri {
            cp::RULE_ENUM_DEFINITION => {
                state.wmc.record_class_like();
                SpaceKind::Enum
            }
            // An `interface` carries no WMC (its members are not weighted),
            // matching the Java walker's interface handling.
            cp::RULE_INTERFACE_DEFINITION => SpaceKind::Interface,
            // `class`, `struct`, and `delegate` are class-like. A `delegate` is
            // a type declaration with a signature but no body; it opens a
            // (childless) class space so its own LOC/NArgs are attributed.
            _ => {
                state.wmc.record_class_like();
                SpaceKind::Class
            }
        };
        self.push_space(kind, name, span_ctx, state, false);
        self.enter_class_cognitive();
    }

    /// Open a metric space for space-introducing rules. Returns whether a space
    /// was pushed.
    fn maybe_open_space(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) -> bool {
        // A member wrapper opens the member's space HERE (not at the inner
        // declaration) so the wrapper's own-line attributes/modifiers —
        // siblings of the declaration, visited before it — are walked *inside*
        // the space and count toward its LOC/Halstead/span.
        if is_member_wrapper(ri) {
            if let Some((inner, kind)) = wrapper_inner_function(ctx) {
                self.open_function_space(ctx, inner, hint, kind);
                return true;
            }
            if let Some(inner) = wrapper_inner_type(ctx) {
                self.open_type_space(ctx, inner);
                return true;
            }
        }

        // The wrapper already opened this declaration's space; do not open a
        // second one. (Its children are still visited into that space.)
        if hint.space_opened_by_wrapper
            && (opens_function_space(ri) || opens_type_like(ri))
            && !matches!(ri, cp::RULE_LAMBDA_EXPRESSION)
        {
            return false;
        }

        match ri {
            // Method-shaped members reached WITHOUT a wrapper (e.g. an
            // interface member's inline signature, or a local function inside a
            // method body).
            cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DEFINITION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATOR
            | cp::RULE_LOCAL_FUNCTION_DECLARATION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // Property / indexer / event accessors are each their own function
            // space (SonarC# counts them as methods): `get`/`set` bodies carry
            // real complexity and are the C# analogue of Kotlin's
            // `getter`/`setter`.
            //
            // `accessor_declarations` is asymmetric: the FIRST accessor's
            // `GET`/`SET` token and `accessor_body` are inline on this rule,
            // and the SECOND one is a *nested*
            // `get_accessor_declaration`/`set_accessor_declaration` child. The
            // same shape applies to `event_accessor_declarations` (`ADD block
            // remove_accessor_declaration | …`).
            //
            // Opening a space at each of those rules would (a) drop the first
            // accessor entirely if only the nested rules were matched, and (b)
            // nest the second accessor *inside* the first — but the two are
            // siblings, not parent and child. So the container rule opens the
            // FIRST accessor's space only, and the nested rule is walked as a
            // sibling: `visit_children` hoists it out (see
            // `accessor_sibling_index`) rather than descending into it here.
            cp::RULE_ACCESSOR_DECLARATIONS | cp::RULE_EVENT_ACCESSOR_DECLARATIONS => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            cp::RULE_GET_ACCESSOR_DECLARATION
            | cp::RULE_SET_ACCESSOR_DECLARATION
            | cp::RULE_ADD_ACCESSOR_DECLARATION
            | cp::RULE_REMOVE_ACCESSOR_DECLARATION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // A lambda (`x => x + 1`, `(a, b) => { … }`) and an anonymous
            // method (`delegate(int x) { … }`) are closures: NOM/NArgs record
            // them as closures and their cyclomatic must NOT roll into the
            // enclosing type's WMC (WMC weights *methods*).
            cp::RULE_LAMBDA_EXPRESSION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Closure);
                true
            }
            _ => {
                // An anonymous method is a labeled alternative of
                // `primary_expression_start`, not its own rule.
                if is_anonymous_method(ctx, ri) {
                    self.open_function_space(ctx, ctx, hint, SpaceKind::Closure);
                    return true;
                }
                // A type definition reached WITHOUT a wrapper (e.g. a type
                // nested directly under `common_member_declaration` in a
                // context whose wrapper did not open it).
                if opens_type_like(ri) {
                    self.open_type_space(ctx, ctx);
                    return true;
                }
                false
            }
        }
    }

    fn new_space_state(&self, ctx: RuleNodeView<'_>) -> State {
        self.new_space_state_widened(ctx, None)
    }

    /// Build a space's initial `State`, optionally widening the span's start
    /// (byte + line) upward to `widened_start`.
    fn new_space_state_widened(
        &self,
        ctx: RuleNodeView<'_>,
        widened_start: Option<(u32, u32)>,
    ) -> State {
        let mut state = State::new();
        let span = ctx_span(ctx, self.line_index, self.source_len);
        let start_line = match widened_start {
            Some((_, line)) if line < span.start_line => line,
            _ => span.start_line,
        };
        state.loc.set_span(
            start_line.saturating_sub(1),
            span.end_line.saturating_sub(1),
            false,
        );
        state
    }

    fn push_space(
        &mut self,
        kind: SpaceKind,
        name: Option<String>,
        ctx: RuleNodeView<'_>,
        state: State,
        suppress_parent_wmc: bool,
    ) {
        self.push_space_widened(kind, name, ctx, state, suppress_parent_wmc, None);
    }

    /// Like [`push_space`], but widens the recorded span's start (byte + line)
    /// upward to `widened_start` when it precedes the context's own start — so
    /// comment routing and the tree span cover the member's own-line
    /// attributes/modifiers.
    fn push_space_widened(
        &mut self,
        kind: SpaceKind,
        name: Option<String>,
        ctx: RuleNodeView<'_>,
        state: State,
        suppress_parent_wmc: bool,
        widened_start: Option<(u32, u32)>,
    ) {
        let mut span = ctx_span(ctx, self.line_index, self.source_len);
        if let Some((start_byte, start_line)) = widened_start
            && start_byte < span.start_byte
        {
            span.start_byte = start_byte;
            span.start_line = start_line;
        }
        let space_id = self.tree.open(kind.clone(), span, name);
        self.loc_routing
            .record_open(space_id, span.start_byte, span.end_byte);
        self.stack.push(state);
        self.kinds.push(kind);
        self.suppress_parent_wmc.push(suppress_parent_wmc);
    }

    /// Reset the cognitive context when opening a type-like space. A type body
    /// is a fresh scope: code that runs *directly* in it (field initializers)
    /// must not inherit the enclosing statement's nesting.
    fn enter_class_cognitive(&mut self) {
        self.cognitive = CognitiveContext::default();
    }

    fn enter_function_cognitive(&mut self, is_closure: bool) {
        // Depth is inherited only from an *enclosing function/closure within
        // the same type scope* — a lambda or local function nested directly in
        // another function's body. A type scope resets the baseline: a method in
        // a nested type is fresh, so its cognitive nesting starts at 0.
        let nested_inside_function = self
            .kinds
            .iter()
            .rev()
            .skip(1)
            .take_while(|k| {
                !matches!(
                    k,
                    SpaceKind::Class
                        | SpaceKind::Interface
                        | SpaceKind::Trait
                        | SpaceKind::Impl
                        | SpaceKind::Enum
                )
            })
            .any(|k| matches!(k, SpaceKind::Function | SpaceKind::Closure));
        self.cognitive.nesting = 0;
        self.cognitive.lambda = 0;
        if nested_inside_function {
            self.cognitive.depth = self.cognitive.depth.saturating_add(1);
        }
        let _ = is_closure;
    }

    fn close_space(&mut self) {
        let closed_kind = self.kinds.pop().expect("kinds underflow");
        let suppress_wmc = self.suppress_parent_wmc.pop().unwrap_or(false);
        let mut state = self.stack.pop().expect("stack underflow");
        // A function OR closure space carries its own McCabe value (base + 1),
        // used for its per-space cyclomatic and (for methods) the WMC rollup.
        if matches!(closed_kind, SpaceKind::Function | SpaceKind::Closure) {
            state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
        }
        finalize_state(&mut state);
        if let Some(space_id) = self.tree.current_id() {
            self.loc_routing
                .record_close(space_id, &state.loc, &state.cyclomatic);
        }
        apply_state_to(state.clone(), self.tree.metrics_mut());
        if let Some(parent) = self.stack.last_mut() {
            let parent_kind = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
            merge_child_into_parent(parent, &state);
            // Roll a closing method's cyclomatic into the parent's WMC. C# WMC
            // is *per class* — an interface's members are not weighted, so only
            // roll into a class/struct/enum parent. Local functions and lambdas
            // are suppressed (their complexity is the enclosing method's).
            if matches!(closed_kind, SpaceKind::Function)
                && !suppress_wmc
                && matches!(
                    parent_kind,
                    SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum
                )
            {
                let container = container_kind(parent_kind);
                state.wmc.finalize_method_into(container, &mut parent.wmc);
            }
        }
        self.tree.close();
    }

    /// Per-rule cyclomatic / cognitive / ABC / exit / LOC classification.
    fn classify_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) {
        // Attribute arguments are compile-time metadata, not executable code,
        // so they record no executable complexity. LOC/Halstead still count —
        // the tokens physically exist.
        if !hint.in_attributes {
            self.classify_control_flow(ctx, ri, hint);
            self.classify_expression(ctx, ri);
            self.classify_abc_rule(ctx, ri);
        }
        self.classify_loc_rule(ctx, ri, hint);
    }

    /// Classify the control-flow constructs. C# spells them as labeled
    /// alternatives of `simple_embedded_statement`, each with a leading keyword
    /// token as a direct child, so they are discriminated by token probe.
    fn classify_control_flow(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) {
        let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;

        match ri {
            cp::RULE_SIMPLE_EMBEDDED_STATEMENT => {
                if ctx.has_token(cl::IF) {
                    // Cyclomatic + ABC always; cognitive nesting unless this is
                    // an `else if` (the flat +1 is emitted at the ELSE token).
                    self.current().cyclomatic.record_decision();
                    self.current().abc.record_condition();
                    if !hint.is_else_branch {
                        self.current().cognitive.increase_nesting(eff);
                        self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                    }
                } else if ctx.has_token(cl::WHILE)
                    || ctx.has_token(cl::DO)
                    || ctx.has_token(cl::FOR)
                    || ctx.has_token(cl::FOREACH)
                {
                    self.current().cyclomatic.record_decision();
                    self.current().abc.record_condition();
                    self.current().cognitive.increase_nesting(eff);
                    self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                } else if ctx.has_token(cl::SWITCH) {
                    // `switch` itself adds cognitive nesting but not
                    // cyclomatic — the `case` labels carry the decisions.
                    self.current().cognitive.increase_nesting(eff);
                    self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                }
                // NOTE: `try` and `lock` deliberately add NOTHING here.
                // SonarSource's cognitive-complexity spec increments on `catch`
                // (a handler is the structure a reader must follow), not on the
                // `try` block itself, and `lock`/`synchronized` is not in the
                // increment list at all. The `catch` nesting is applied at the
                // catch-clause rules below, matching `mehen-java` (whose
                // walker likewise never increments on `try`/`synchronized`).
                else if ctx.has_token(cl::RETURN) || ctx.has_token(cl::THROW) {
                    self.current().nexit.record_exit();
                } else if ctx.has_token(cl::YIELD) {
                    // `yield return` / `yield break` both leave the iterator.
                    self.current().nexit.record_exit();
                } else if ctx.has_token(cl::GOTO) {
                    // `goto` (including `goto case`/`goto default`) is
                    // goto-like: a flat +1 (cognitive).
                    self.current().cognitive.increment_by_one();
                }
                // Each statement starts a fresh boolean sequence so operators
                // never collapse across statement boundaries — e.g.
                // `F(a && b); G(c && d)` is +2, not +1.
                self.current().cognitive.boolean_seq.reset();
            }
            // A `case` label is a decision (cyclomatic) and a condition (ABC);
            // `default:` (no CASE token) is neither. The `switch` already opened
            // the cognitive nesting level, so a `case` adds no further nesting.
            cp::RULE_SWITCH_LABEL if ctx.has_token(cl::CASE) => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
            }
            // A pattern-switch guard (`case int i when i > 0:`) is a distinct
            // boolean test — like an extra `if` on the case — so it records one
            // ABC condition of its own.
            cp::RULE_CASE_GUARD => self.current().abc.record_condition(),
            // `catch` is cognitive-only (matches SonarC#/SonarJava): a nesting
            // increment plus an ABC condition, but no cyclomatic decision.
            cp::RULE_SPECIFIC_CATCH_CLAUSE | cp::RULE_GENERAL_CATCH_CLAUSE => {
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().abc.record_condition();
            }
            // An exception filter (`catch (E e) when (cond)`) is an extra
            // boolean test on the handler.
            cp::RULE_EXCEPTION_FILTER => self.current().abc.record_condition(),
            // A `throw` *expression* (`x ?? throw new E()`, C# 7) is an exit
            // that never reaches the statement-form probe above.
            cp::RULE_THROW_EXPRESSION => self.current().nexit.record_exit(),
            // A declaration statement and an accessor/expression body start a
            // fresh boolean sequence too — they are statement-shaped positions
            // that are not wrapped in `simple_embedded_statement`.
            cp::RULE_DECLARATION_STATEMENT | cp::RULE_ACCESSOR_BODY | cp::RULE_BODY => {
                self.current().cognitive.boolean_seq.reset();
            }
            // The prefix `!` records a not-operator so a following same-kind
            // boolean operator is not collapsed with the one before the
            // negation (`a && !b && c` is one run in SonarSource's model, but
            // the run tracker needs the marker to keep parity with Kotlin).
            cp::RULE_UNARY_EXPRESSION if ctx.has_token(cl::BANG) => {
                self.current().cognitive.boolean_seq.not_operator("!");
            }
            _ => {}
        }
    }

    /// Classify operator-bearing expression rules: the ternary (cyclomatic +
    /// cognitive + ABC), null-coalescing (ABC), and the relational level (ABC,
    /// distinguishing a comparison from a shift).
    fn classify_expression(&mut self, ctx: RuleNodeView<'_>, ri: usize) {
        match ri {
            // Ternary `? :` — a decision, an ABC condition, and a cognitive
            // nesting structure (SonarSource scores it like an `if`). The
            // grammar makes the `?`/`:` optional on `conditional_expression`, so
            // a bare pass-through node must not score.
            cp::RULE_CONDITIONAL_EXPRESSION if ctx.has_token(cl::INTERR) => {
                let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cyclomatic.record_decision();
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().abc.record_condition();
            }
            // `??` null-coalescing is a condition (an implicit null test). Its
            // token is `OP_COALESCING`, already caught by the token-level ABC
            // scan, so nothing to add here — kept as documentation of intent.
            cp::RULE_NULL_COALESCING_EXPRESSION => {}
            // The relational level spells `<`/`>` with bare `LT`/`GT` tokens
            // that the shift level ALSO uses (`<<` is `LT LT`, `>>` is the
            // `right_shift` rule). Count a condition only for the genuine
            // comparison operators on this rule: `<`, `>`, `<=`, `>=`, plus the
            // type tests `is` and `as`. Equality (`==`/`!=`) is caught by the
            // token-level scan on `OP_EQ`/`OP_NE`.
            cp::RULE_RELATIONAL_EXPRESSION => {
                let comparisons = ctx.child_tokens(cl::LT).count()
                    + ctx.child_tokens(cl::GT).count()
                    + ctx.child_tokens(cl::OP_LE).count()
                    + ctx.child_tokens(cl::OP_GE).count()
                    + ctx.child_tokens(cl::IS).count()
                    + ctx.child_tokens(cl::AS).count();
                for _ in 0..comparisons {
                    self.current().abc.record_condition();
                }
            }
            _ => {}
        }
    }

    fn classify_abc_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize) {
        match ri {
            // Every assignment form (`=`, compound, `??=`) is one A.
            cp::RULE_ASSIGNMENT => self.current().abc.record_assignment(),
            // A call or object creation is a branch (ABC's B counts function
            // calls, method calls, and message sends).
            //
            // `member_access` (`a.B`) is deliberately NOT counted: it is the
            // qualification `.B`, not a call. Counting it would (a) score a
            // plain field/property *read* as a branch, which ABC does not, and
            // (b) score a qualified call twice — the grammar spells
            // `o.Helper()` as `member_access` + `method_invocation`, and
            // `System.Console.WriteLine(x)` as two `member_access` plus one
            // `method_invocation`. Counting only the invocation keeps one
            // branch per call regardless of qualification depth, matching
            // `mehen-java` (which counts `methodCall`/`creator`, never field
            // access).
            cp::RULE_METHOD_INVOCATION
            | cp::RULE_OBJECT_CREATION_EXPRESSION
            | cp::RULE_CONSTRUCTOR_INITIALIZER => self.current().abc.record_branch(),
            // An initialized declarator is an assignment. Each declarator rule
            // carries its own `=` token when initialized.
            cp::RULE_LOCAL_VARIABLE_DECLARATOR
            | cp::RULE_VARIABLE_DECLARATOR
            | cp::RULE_CONSTANT_DECLARATOR
            | cp::RULE_ENUM_MEMBER_DECLARATION
            | cp::RULE_ARG_DECLARATION
                if ctx.has_token(cl::ASSIGNMENT) =>
            {
                self.current().abc.record_assignment();
            }
            _ => {}
        }
    }

    fn classify_loc_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) {
        // A `for`/`foreach`/`using` header's declaration is part of the
        // statement's single logical line — not its own LLOC.
        if matches!(
            ri,
            cp::RULE_LOCAL_VARIABLE_DECLARATION | cp::RULE_LOCAL_CONSTANT_DECLARATION
        ) && hint.in_for_init
        {
            return;
        }
        // An *expression-bodied* lambda (`x => x + 1`) opens a closure space
        // but its body contains no statement/declaration, so the closure would
        // report `lloc = 0`. Count the lambda itself as one logical line to
        // match a block-bodied lambda (whose inner statements already count).
        if ri == cp::RULE_LAMBDA_EXPRESSION && !lambda_body_is_block(ctx) {
            self.current().loc.observe_lloc();
            return;
        }
        // An expression-bodied **accessor** (`get => _x;`) or expression-bodied
        // `body`/`local_function_body` opens a space whose only content is that
        // expression — no statement — so the space would report `lloc = 0`
        // without counting the body itself as one logical line.
        //
        // This deliberately does NOT list the member declaration rules
        // (`method_declaration`, `property_declaration`, …). An expression body
        // on those is spelled with the `right_arrow` as a DIRECT child of the
        // declaration (`… (method_body | right_arrow throwable_expression
        // ';')`), and the declaration itself is already a logical line via the
        // declaration list below — so `int F() => 1;` correctly counts once,
        // not twice.
        if matches!(
            ri,
            cp::RULE_BODY | cp::RULE_ACCESSOR_BODY | cp::RULE_LOCAL_FUNCTION_BODY
        ) && ctx.child_rule(cp::RULE_BLOCK).is_none()
            && ctx.child_rule(cp::RULE_RIGHT_ARROW).is_some()
        {
            self.current().loc.observe_lloc();
            return;
        }

        if matches!(
            ri,
            // Statement- and declaration-shaped rules.
            cp::RULE_SIMPLE_EMBEDDED_STATEMENT
                | cp::RULE_LOCAL_VARIABLE_DECLARATION
                | cp::RULE_LOCAL_CONSTANT_DECLARATION
                | cp::RULE_LOCAL_FUNCTION_DECLARATION
                | cp::RULE_FIELD_DECLARATION
                | cp::RULE_CONSTANT_DECLARATION
                | cp::RULE_EVENT_DECLARATION
                | cp::RULE_METHOD_DECLARATION
                | cp::RULE_CONSTRUCTOR_DECLARATION
                | cp::RULE_DESTRUCTOR_DEFINITION
                | cp::RULE_OPERATOR_DECLARATION
                | cp::RULE_CONVERSION_OPERATOR_DECLARATOR
                | cp::RULE_PROPERTY_DECLARATION
                | cp::RULE_INDEXER_DECLARATION
                | cp::RULE_ENUM_MEMBER_DECLARATION
                | cp::RULE_CLASS_DEFINITION
                | cp::RULE_STRUCT_DEFINITION
                | cp::RULE_INTERFACE_DEFINITION
                | cp::RULE_ENUM_DEFINITION
                | cp::RULE_DELEGATE_DEFINITION
                | cp::RULE_INTERFACE_MEMBER_DECLARATION
                | cp::RULE_NAMESPACE_DECLARATION
                | cp::RULE_USING_DIRECTIVE
                | cp::RULE_EXTERN_ALIAS_DIRECTIVE
        ) {
            // An empty statement (`;`) is not a logical line, and a bare block
            // `{ … }` is a wrapper whose inner statements each count.
            if ri == cp::RULE_SIMPLE_EMBEDDED_STATEMENT && ctx_is_empty_statement(ctx) {
                return;
            }
            self.current().loc.observe_lloc();
        }
    }

    /// NPA / NPM classification for a direct member of an enclosing type body.
    /// `ctx` is the member declaration rule itself; `public` is the visibility
    /// resolved from the member wrapper's modifiers (threaded via [`ChildHint`]).
    fn classify_type_member(
        &mut self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        container: ContainerKind,
        public: bool,
    ) {
        match ri {
            // A field declaration can declare several variables
            // (`int a, b, c;`).
            cp::RULE_FIELD_DECLARATION => {
                let count = declarator_count(ctx, cp::RULE_VARIABLE_DECLARATORS).max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, public);
                }
            }
            cp::RULE_CONSTANT_DECLARATION => {
                let count = declarator_count(ctx, cp::RULE_CONSTANT_DECLARATORS).max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, public);
                }
            }
            // An `event` can declare several variables too (`event E a, b;`),
            // or a single named event with accessors.
            cp::RULE_EVENT_DECLARATION => {
                let count = declarator_count(ctx, cp::RULE_VARIABLE_DECLARATORS).max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, public);
                }
            }
            // An `enum` member (`enum E { A, B }`) is a public constant field
            // of the enum → a public class attribute.
            cp::RULE_ENUM_MEMBER_DECLARATION => {
                self.current()
                    .npa
                    .record_attribute(ContainerKind::Class, true);
            }
            // Methods, constructors, operators, and the property/indexer forms
            // are all methods for NPM purposes (SonarC# counts a property as a
            // member of the type's public API).
            cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DEFINITION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATOR
            | cp::RULE_PROPERTY_DECLARATION
            | cp::RULE_INDEXER_DECLARATION => {
                self.current().npm.record_method(container, public);
            }
            // An interface member's signature is inline on the member rule
            // (there is no nested declaration), so classify it here. Every
            // interface member is implicitly public.
            cp::RULE_INTERFACE_MEMBER_DECLARATION => {
                if ctx.child_rule(cp::RULE_FORMAL_PARAMETER_LIST).is_some()
                    || ctx.has_token(cl::OPEN_PARENS)
                {
                    self.current().npm.record_method(container, true);
                } else if ctx.has_token(cl::EVENT) {
                    self.current().npa.record_attribute(container, true);
                } else {
                    // A property/indexer signature (`int P { get; set; }`).
                    self.current().npm.record_method(container, true);
                }
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------
// Free helpers (top-down tree inspection — no parent pointers).
// --------------------------------------------------------------------

/// Rules that open a type-like metric space (see `maybe_open_space`).
fn opens_type_like(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_CLASS_DEFINITION
            | cp::RULE_STRUCT_DEFINITION
            | cp::RULE_INTERFACE_DEFINITION
            | cp::RULE_ENUM_DEFINITION
            | cp::RULE_DELEGATE_DEFINITION
    )
}

/// Rules that open a function/closure metric space (mirrors the function arms
/// of `maybe_open_space`).
fn opens_function_space(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DEFINITION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATOR
            | cp::RULE_LOCAL_FUNCTION_DECLARATION
            | cp::RULE_ACCESSOR_DECLARATIONS
            | cp::RULE_EVENT_ACCESSOR_DECLARATIONS
            | cp::RULE_GET_ACCESSOR_DECLARATION
            | cp::RULE_SET_ACCESSOR_DECLARATION
            | cp::RULE_ADD_ACCESSOR_DECLARATION
            | cp::RULE_REMOVE_ACCESSOR_DECLARATION
            | cp::RULE_LAMBDA_EXPRESSION
    )
}

/// The member-declaration wrappers whose leading `attributes` and
/// `all_member_modifiers` are siblings of the inner declaration. Their start
/// line is where the member truly begins, so a member space widens its span up
/// to it, and the visibility is resolved there.
fn is_member_wrapper(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_CLASS_MEMBER_DECLARATION
            | cp::RULE_STRUCT_MEMBER_DECLARATION
            | cp::RULE_TYPE_DECLARATION
    )
}

/// Given a member wrapper, find the inner method-shaped declaration whose
/// function space should be opened at the wrapper level — so the wrapper's
/// own-line attributes/modifiers belong to the member's LOC/Halstead/span
/// rather than the enclosing type.
///
/// Walks the DIRECT child path only (`class_member_declaration →
/// common_member_declaration → …`), never an unbounded search, so a nested
/// type's method can never be captured here. Returns `None` when the member is
/// not method-shaped (a field, nested type, const, property — those keep their
/// own open sites).
fn wrapper_inner_function(ctx: RuleNodeView<'_>) -> Option<(RuleNodeView<'_>, SpaceKind)> {
    let common = match ctx.rule_index() {
        cp::RULE_CLASS_MEMBER_DECLARATION | cp::RULE_STRUCT_MEMBER_DECLARATION => {
            ctx.child_rule(cp::RULE_COMMON_MEMBER_DECLARATION)
        }
        _ => None,
    };
    // A destructor is a direct child of the member wrapper, not of
    // `common_member_declaration`.
    if let Some(dtor) = ctx.child_rule(cp::RULE_DESTRUCTOR_DEFINITION) {
        return Some((dtor, SpaceKind::Function));
    }
    let common = common?;
    for candidate in [
        cp::RULE_CONSTRUCTOR_DECLARATION,
        cp::RULE_METHOD_DECLARATION,
        cp::RULE_CONVERSION_OPERATOR_DECLARATOR,
    ] {
        if let Some(inner) = common.child_rule(candidate) {
            return Some((inner, SpaceKind::Function));
        }
    }
    // `typed_member_declaration` wraps the type-prefixed member forms; a
    // method or operator there is method-shaped.
    let typed = common.child_rule(cp::RULE_TYPED_MEMBER_DECLARATION)?;
    for candidate in [cp::RULE_METHOD_DECLARATION, cp::RULE_OPERATOR_DECLARATION] {
        if let Some(inner) = typed.child_rule(candidate) {
            return Some((inner, SpaceKind::Function));
        }
    }
    None
}

/// Given a member or type wrapper, find the inner type definition whose space
/// should be opened at the wrapper — so the wrapper's own-line
/// attributes/modifiers belong to the type's LOC/Halstead/span.
///
/// Walks the DIRECT `child_rule` path only.
fn wrapper_inner_type(ctx: RuleNodeView<'_>) -> Option<RuleNodeView<'_>> {
    let holder = match ctx.rule_index() {
        // `type_declaration` holds the definition as a direct child (after
        // `attributes? all_member_modifiers?`).
        cp::RULE_TYPE_DECLARATION => ctx,
        // A member type is `class_member_declaration → common_member_declaration
        // → <definition>`.
        cp::RULE_CLASS_MEMBER_DECLARATION | cp::RULE_STRUCT_MEMBER_DECLARATION => {
            ctx.child_rule(cp::RULE_COMMON_MEMBER_DECLARATION)?
        }
        _ => return None,
    };
    holder
        .children()
        .filter_map(|c| c.as_rule())
        .find(|c| opens_type_like(c.rule_index()))
}

/// The *second* accessor of a property/indexer/event, which this grammar nests
/// inside the first accessor's rule (`accessor_declarations: … GET
/// accessor_body set_accessor_declaration?`) even though the two are siblings.
///
/// Returned so `visit_rule` can visit it *after* the first accessor's space
/// closes — making it a sibling space rather than a child — while
/// `visit_children` skips it during the first accessor's own descent.
fn accessor_sibling(ctx: RuleNodeView<'_>, ri: usize) -> Option<RuleNodeView<'_>> {
    if !matches!(
        ri,
        cp::RULE_ACCESSOR_DECLARATIONS | cp::RULE_EVENT_ACCESSOR_DECLARATIONS
    ) {
        return None;
    }
    [
        cp::RULE_GET_ACCESSOR_DECLARATION,
        cp::RULE_SET_ACCESSOR_DECLARATION,
        cp::RULE_ADD_ACCESSOR_DECLARATION,
        cp::RULE_REMOVE_ACCESSOR_DECLARATION,
    ]
    .into_iter()
    .find_map(|rule| ctx.child_rule(rule))
}

/// Whether this `primary_expression_start` is the anonymous-method alternative
/// (`delegate(int x) { … }`) — it carries a `DELEGATE` token and a `block`.
fn is_anonymous_method(ctx: RuleNodeView<'_>, ri: usize) -> bool {
    ri == cp::RULE_PRIMARY_EXPRESSION_START
        && ctx.has_token(cl::DELEGATE)
        && ctx.child_rule(cp::RULE_BLOCK).is_some()
}

/// Whether a `lambda_expression`'s body is a block (`… => { … }`) rather than
/// an expression. A block body's statements are counted individually for LLOC;
/// an expression body makes the lambda itself one logical line.
fn lambda_body_is_block(ctx: RuleNodeView<'_>) -> bool {
    ctx.child_rule(cp::RULE_ANONYMOUS_FUNCTION_BODY)
        .map(|body| body.child_rule(cp::RULE_BLOCK).is_some())
        .unwrap_or(false)
}

/// Whether this `simple_embedded_statement` is an `if` statement.
fn is_if_statement(ctx: RuleNodeView<'_>, ri: usize) -> bool {
    ri == cp::RULE_SIMPLE_EMBEDDED_STATEMENT && ctx.has_token(cl::IF)
}

/// Index of the `else`-branch `if_body` child of an `if` statement, if present.
/// The else body is the `if_body` that appears *after* the `ELSE` terminal.
fn else_branch_index<'a>(children: impl Iterator<Item = Node<'a>>) -> Option<usize> {
    let mut seen_else = false;
    for (idx, child) in children.enumerate() {
        if let Some(t) = child.as_terminal() {
            if t.symbol().token_type() == cl::ELSE {
                seen_else = true;
            }
        } else if let Some(rule) = child.as_rule()
            && seen_else
            && rule.rule_index() == cp::RULE_IF_BODY
        {
            return Some(idx);
        }
    }
    None
}

/// Whether the `is_else_branch` flag may propagate through this rule toward a
/// nested `if` (marking it an `else if`).
///
/// True only for the *transparent body wrappers* — `if_body` and
/// `embedded_statement`/`statement` — and only when they do not introduce a
/// block or a control-flow construct of their own. A `block` stops the flow
/// (`else { if … }` is genuinely nested), and a statement carrying its own
/// control-flow keyword does too (`else while (c) if (b) {}` keeps its nesting).
fn is_else_transparent(ctx: RuleNodeView<'_>, ri: usize) -> bool {
    match ri {
        cp::RULE_IF_BODY | cp::RULE_EMBEDDED_STATEMENT | cp::RULE_STATEMENT => {
            ctx.child_rule(cp::RULE_BLOCK).is_none()
        }
        cp::RULE_SIMPLE_EMBEDDED_STATEMENT => {
            // Only a *nested* `if` may inherit the flag; a statement with its
            // own loop/switch/try keyword is a real nested scope.
            !ctx.has_token(cl::WHILE)
                && !ctx.has_token(cl::DO)
                && !ctx.has_token(cl::FOR)
                && !ctx.has_token(cl::FOREACH)
                && !ctx.has_token(cl::SWITCH)
                && !ctx.has_token(cl::TRY)
                && !ctx.has_token(cl::LOCK)
                && !ctx.has_token(cl::USING)
                && !ctx.has_token(cl::IF)
        }
        _ => false,
    }
}

/// Whether this `simple_embedded_statement` is an empty statement (a bare `;`) —
/// its only child is the `SEMICOLON` terminal.
fn ctx_is_empty_statement(ctx: RuleNodeView<'_>) -> bool {
    let mut children = ctx.children();
    matches!(
        (children.next(), children.next()),
        (Some(only), None)
            if only.as_terminal().is_some_and(|t| t.symbol().token_type() == cl::SEMICOLON)
    )
}

/// The declared name of a member/type: its first `identifier` child's covered
/// text. Falls back to the `member_name`/`method_member_name` wrapper's text
/// for the members that spell their name through one.
fn name_from_identifier(ctx: RuleNodeView<'_>) -> Option<String> {
    for child in ctx.children() {
        let Some(c) = child.as_rule() else { continue };
        if matches!(
            c.rule_index(),
            cp::RULE_IDENTIFIER | cp::RULE_MEMBER_NAME | cp::RULE_METHOD_MEMBER_NAME
        ) {
            let t = c.text();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// The space name for a function-shaped node. Accessors have no name of their
/// own, so they are named `<owner>.get` / `.set` / `.add` / `.remove` from the
/// property/indexer/event name threaded down through [`ChildHint`].
fn function_name(ctx: RuleNodeView<'_>, hint: &ChildHint) -> Option<String> {
    let accessor_suffix = match ctx.rule_index() {
        cp::RULE_GET_ACCESSOR_DECLARATION => Some("get"),
        cp::RULE_SET_ACCESSOR_DECLARATION => Some("set"),
        cp::RULE_ADD_ACCESSOR_DECLARATION => Some("add"),
        cp::RULE_REMOVE_ACCESSOR_DECLARATION => Some("remove"),
        // The FIRST accessor is inline on `accessor_declarations` /
        // `event_accessor_declarations` (see `maybe_open_space`), so its kind
        // is read from whichever keyword token this rule carries directly.
        // `has_token` inspects only DIRECT children, so the second accessor's
        // keyword — which lives inside the nested `*_accessor_declaration` —
        // cannot leak in here.
        cp::RULE_ACCESSOR_DECLARATIONS => {
            if ctx.has_token(cl::GET) {
                Some("get")
            } else {
                Some("set")
            }
        }
        cp::RULE_EVENT_ACCESSOR_DECLARATIONS => {
            if ctx.has_token(cl::ADD) {
                Some("add")
            } else {
                Some("remove")
            }
        }
        _ => None,
    };
    if let Some(suffix) = accessor_suffix {
        return Some(match &hint.accessor_owner {
            Some(owner) => format!("{owner}.{suffix}"),
            None => suffix.to_string(),
        });
    }
    // A destructor's name is `~Name`; an operator's is `operator <op>`. Both
    // are readable enough from the leading identifier / operator token.
    if ctx.rule_index() == cp::RULE_OPERATOR_DECLARATION {
        return ctx
            .child_rule(cp::RULE_OVERLOADABLE_OPERATOR)
            .map(|op| format!("operator {}", op.text()));
    }
    if ctx.rule_index() == cp::RULE_CONVERSION_OPERATOR_DECLARATOR {
        return Some("operator".to_string());
    }
    // A lambda / anonymous method is anonymous.
    if matches!(
        ctx.rule_index(),
        cp::RULE_LAMBDA_EXPRESSION | cp::RULE_PRIMARY_EXPRESSION_START
    ) {
        return None;
    }
    // A local function keeps its name on the nested `local_function_header`
    // (`local_function_declaration: local_function_header local_function_body`),
    // so the direct-children scan below would find nothing.
    if ctx.rule_index() == cp::RULE_LOCAL_FUNCTION_DECLARATION {
        return ctx
            .child_rule(cp::RULE_LOCAL_FUNCTION_HEADER)
            .and_then(name_from_identifier);
    }
    name_from_identifier(ctx)
}

/// The property/indexer/event name to thread down to its accessors, if `ctx` is
/// one of those member forms.
fn accessor_owner_name(ctx: RuleNodeView<'_>, ri: usize) -> Option<SmolStr> {
    match ri {
        cp::RULE_PROPERTY_DECLARATION | cp::RULE_EVENT_DECLARATION => {
            name_from_identifier(ctx).map(SmolStr::new)
        }
        cp::RULE_INDEXER_DECLARATION => Some(SmolStr::new("this[]")),
        _ => None,
    }
}

/// Count the declared parameters of a function-shaped node.
///
/// Methods/constructors/local functions/indexers spell their parameters in a
/// `formal_parameter_list` (`fixed_parameters (',' parameter_array)?` or a bare
/// `parameter_array`); an operator declares its one-or-two `arg_declaration`s
/// inline; a lambda/anonymous method uses an `anonymous_function_signature`
/// (explicit or implicit parameter list, or a single bare identifier).
fn count_args(ctx: RuleNodeView<'_>) -> u32 {
    // Lambda / anonymous method.
    if let Some(sig) = ctx.child_rule(cp::RULE_ANONYMOUS_FUNCTION_SIGNATURE) {
        return count_anonymous_signature_args(sig);
    }
    if ctx.rule_index() == cp::RULE_PRIMARY_EXPRESSION_START {
        // An anonymous method's parameters are an inline
        // `explicit_anonymous_function_parameter_list`.
        return ctx
            .child_rule(cp::RULE_EXPLICIT_ANONYMOUS_FUNCTION_PARAMETER_LIST)
            .map(|list| {
                list.child_rules(cp::RULE_EXPLICIT_ANONYMOUS_FUNCTION_PARAMETER)
                    .count() as u32
            })
            .unwrap_or(0);
    }
    // An operator's parameters are direct `arg_declaration` children.
    if matches!(
        ctx.rule_index(),
        cp::RULE_OPERATOR_DECLARATION | cp::RULE_CONVERSION_OPERATOR_DECLARATOR
    ) {
        return ctx.child_rules(cp::RULE_ARG_DECLARATION).count() as u32;
    }
    // A local function keeps its signature on the `local_function_header`.
    let holder = if ctx.rule_index() == cp::RULE_LOCAL_FUNCTION_DECLARATION {
        ctx.child_rule(cp::RULE_LOCAL_FUNCTION_HEADER)
            .unwrap_or(ctx)
    } else {
        ctx
    };
    holder
        .child_rule(cp::RULE_FORMAL_PARAMETER_LIST)
        .map(count_formal_parameter_list)
        .unwrap_or(0)
}

/// Count a `formal_parameter_list`: `parameter_array | fixed_parameters (','
/// parameter_array)?`. A `params` array counts as one parameter.
fn count_formal_parameter_list(list: RuleNodeView<'_>) -> u32 {
    let fixed = list
        .child_rule(cp::RULE_FIXED_PARAMETERS)
        .map(|f| f.child_rules(cp::RULE_FIXED_PARAMETER).count() as u32)
        .unwrap_or(0);
    let array = list.child_rules(cp::RULE_PARAMETER_ARRAY).count() as u32;
    fixed + array
}

/// Count an `anonymous_function_signature`: `() | (explicit list) |
/// (implicit list) | identifier`.
fn count_anonymous_signature_args(sig: RuleNodeView<'_>) -> u32 {
    if let Some(list) = sig.child_rule(cp::RULE_EXPLICIT_ANONYMOUS_FUNCTION_PARAMETER_LIST) {
        return list
            .child_rules(cp::RULE_EXPLICIT_ANONYMOUS_FUNCTION_PARAMETER)
            .count() as u32;
    }
    if let Some(list) = sig.child_rule(cp::RULE_IMPLICIT_ANONYMOUS_FUNCTION_PARAMETER_LIST) {
        return list.child_rules(cp::RULE_IDENTIFIER).count() as u32;
    }
    // `x => …` — a single bare identifier.
    sig.child_rules(cp::RULE_IDENTIFIER).count() as u32
}

/// Count the declarators of a field/const/event declaration via its
/// `variable_declarators` / `constant_declarators` child.
fn declarator_count(ctx: RuleNodeView<'_>, list_rule: usize) -> u32 {
    let declarator_rule = if list_rule == cp::RULE_CONSTANT_DECLARATORS {
        cp::RULE_CONSTANT_DECLARATOR
    } else {
        cp::RULE_VARIABLE_DECLARATOR
    };
    ctx.child_rule(list_rule)
        .map(|list| list.child_rules(declarator_rule).count() as u32)
        .unwrap_or(0)
}

/// Resolve an explicit visibility from a member/type wrapper's
/// `all_member_modifiers`: `Some(true)` if it carries `public`, `Some(false)`
/// if it carries `private`/`protected`/`internal`, `None` if no access modifier
/// is present (caller applies the container default).
///
/// `internal` is *not* public: it is assembly-scoped, so it does not
/// contribute to the type's public API surface (NPA/NPM).
fn visibility_from_modifiers(ctx: RuleNodeView<'_>) -> Option<bool> {
    let modifiers = ctx.child_rule(cp::RULE_ALL_MEMBER_MODIFIERS)?;
    let mut saw_non_public = false;
    for m in modifiers.child_rules(cp::RULE_ALL_MEMBER_MODIFIER) {
        if m.has_token(cl::PUBLIC) {
            return Some(true);
        }
        if m.has_token(cl::PRIVATE) || m.has_token(cl::PROTECTED) || m.has_token(cl::INTERNAL) {
            saw_non_public = true;
        }
    }
    saw_non_public.then_some(false)
}

fn container_kind(parent_kind: SpaceKind) -> ContainerKind {
    match parent_kind {
        SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum => ContainerKind::Class,
        SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
        _ => ContainerKind::Other,
    }
}

/// Equality / boolean / null-coalescing operator tokens that count as an ABC
/// "condition".
///
/// Every operator the `relational_expression` rule owns — `<`, `>`, `<=`, `>=`,
/// `is`, `as` — is deliberately EXCLUDED here and counted at that rule instead
/// (`classify_expression`). The rule-level scan is required for `<`/`>`, whose
/// bare `LT`/`GT` tokens this grammar reuses for shifts (`<<` is `LT LT`), and
/// `<=`/`>=` must follow the same path or they would be counted twice — once
/// here and once at the rule. Equality (`==`/`!=`) has dedicated tokens that
/// appear nowhere else, so it stays on this cheap token scan.
fn is_abc_condition_token(tt: i32) -> bool {
    matches!(
        tt,
        cl::OP_EQ | cl::OP_NE | cl::OP_AND | cl::OP_OR | cl::OP_COALESCING
    )
}

// --------------------------------------------------------------------
// Halstead token classification.
// --------------------------------------------------------------------

enum HalsteadClass {
    Operator,
    Operand,
    Skip,
}

/// Classify a token type as a Halstead operator, operand, or skipped.
///
/// Operands: identifiers, literals (including every interpolated-string content
/// token), `this`, `base`. Skipped: whitespace, comments, the BOM, the
/// inactive-`#if` `SKIPPED_SECTION`, and EOF. Everything else (keywords,
/// punctuation, operators) is an operator.
fn halstead_class(tt: i32) -> HalsteadClass {
    if matches!(
        tt,
        cl::IDENTIFIER
            | cl::LITERAL_ACCESS
            | cl::INTEGER_LITERAL
            | cl::HEX_INTEGER_LITERAL
            | cl::BIN_INTEGER_LITERAL
            | cl::REAL_LITERAL
            | cl::CHARACTER_LITERAL
            | cl::REGULAR_STRING
            | cl::VERBATIUM_STRING
            | cl::TRUE
            | cl::FALSE
            | cl::NULL
            | cl::THIS
            | cl::BASE
            // Interpolated-string content pieces are literal text.
            | cl::REGULAR_CHAR_INSIDE
            | cl::REGULAR_STRING_INSIDE
            | cl::VERBATIUM_DOUBLE_QUOTE_INSIDE
            | cl::VERBATIUM_INSIDE_STRING
            | cl::DOUBLE_CURLY_INSIDE
            | cl::DOUBLE_CURLY_CLOSE_INSIDE
            | cl::FORMAT_STRING
            | cl::TEXT
            | cl::CONDITIONAL_SYMBOL
            | cl::DIGITS
    ) {
        return HalsteadClass::Operand;
    }

    if matches!(
        tt,
        cl::WHITESPACES
            | cl::DIRECTIVE_WHITESPACES
            | cl::BYTE_ORDER_MARK
            | cl::SKIPPED_SECTION
            | cl::SINGLE_LINE_COMMENT
            | cl::DELIMITED_COMMENT
            | cl::SINGLE_LINE_DOC_COMMENT
            | cl::DELIMITED_DOC_COMMENT
            | cl::EMPTY_DELIMITED_DOC_COMMENT
    ) || tt < 0
    {
        return HalsteadClass::Skip;
    }

    HalsteadClass::Operator
}

/// A stable string label for an operator token, used as its Halstead operator
/// key. The numeric token type is stable for a given generated grammar.
fn kp_token_name(tt: i32) -> String {
    format!("t{tt}")
}
