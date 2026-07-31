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
//! ## Grammar shape
//!
//! The parser is derived from Roslyn's own published grammar (see
//! `mehen-csharp-parser/grammar/PROVENANCE.md`), which names rules after the
//! compiler's *syntax nodes*. Three consequences shape every classification here:
//!
//! - **Each statement form is its own rule.** `if_statement`, `while_statement`,
//!   `switch_statement`, `catch_clause`, `else_clause`, … so control flow is
//!   dispatched on `rule_index()`. That is also more precise than the keyword
//!   probing the previous grammars-v4 grammar required: a `has_token(IF)` test on
//!   a shared `simple_embedded_statement` fires for an `if` anywhere in the node,
//!   whereas a rule match cannot.
//! - **Each declaration carries its own `attribute_list* modifier*`.** So a
//!   member's span already covers its attributes, and its visibility is readable
//!   on the declaration itself — no wrapper rule to open the space at, no span
//!   widening, and no threading of resolved visibility down a wrapper chain.
//! - **The expression cycle is inlined.** Roslyn's `expression` participates in a
//!   mutual left-recursion cycle, and the generator's hub inlining (upstream
//!   #221) folds 16 of 17 satellites into it — so `invocation_expression`,
//!   `assignment_expression`, `binary_expression`, and `conditional_expression`
//!   have no rule index of their own. They are classified by *shape* through the
//!   typed `ExpressionContext`: one `expression` child plus an `argument_list` is
//!   an invocation, two children a binary/assignment, three a ternary.
//!
//! ## Typed contexts
//!
//! Navigation uses the generated typed contexts, whose accessors reach only a
//! rule's *declared direct children* — the property the untyped `child_rule` /
//! `has_token` probes could assert only by comment. Dispatch stays on
//! `rule_index()`, since a metric walker is fundamentally one match over rule
//! kinds. This mirrors `mehen-java` and `mehen-kotlin`.
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
//! - **ABC**: assignments via the assignment-shaped `expression` (all
//!   `=`/compound/`??=` forms), `++`/`--`, and any initialized declarator;
//!   branches via every invocation-shaped `expression`, object creation, and
//!   `constructor_initializer` (NOT member access — that is qualification, not a
//!   call, so a qualified call still scores exactly one branch); conditions via
//!   `if`/`case`/`catch`/`when`/loops/comparison & equality/`&&`/`||`/ternary/
//!   `??`/`is`/`as`. Bit-shifts are excluded, and the prep makes that reliable:
//!   `>>` is spelled as adjacent `>` tokens rejoined in the parser, so a `GT`
//!   token is always a comparison and never half a shift.
//! - **NExit**: `return_statement`, `throw_statement`, `throw_expression`, and
//!   `yield_statement` (both `yield return` and `yield break`).
//! - **NArgs**: the `parameter` count of a `parameter_list` /
//!   `bracketed_parameter_list`. Roslyn uses one `parameter` rule for every
//!   position — methods, operators, lambdas, anonymous methods alike — with
//!   `params` as a modifier rather than a distinct rule.
//! - **NOM**: every `method_declaration`, `constructor_declaration`,
//!   `destructor_declaration`, `operator_declaration`,
//!   `conversion_operator_declaration`, `local_function_statement`, and
//!   `accessor_declaration` (one rule covers get/set/init/add/remove) is a
//!   function space; `simple_lambda_expression`,
//!   `parenthesized_lambda_expression`, and `anonymous_method_expression` are
//!   closure-shaped function spaces.
//! - **LOC**: PLOC from per-space code-token rows during the walk, LLOC from
//!   statement/declaration-shaped rules, CLOC from a source-ordered pass over
//!   the hidden-channel comment tokens routed via `SpaceRangeTracker`.
//! - **Halstead**: per-token operator/operand classification — keywords and
//!   punctuation are operators; identifiers, literals, `this`, `base` are
//!   operands (deduped by text). A terminal reached through `identifier_token` is
//!   always an operand, which is what makes C#'s contextual keywords come out
//!   right: the prep widens that rule to accept all 42 of them.
//! - **NPA / NPM / WMC**: class-vs-interface routing by the declaration rule
//!   (`struct` and `record` count as class-like containers; `interface` as an
//!   interface). NPA counts `field_declaration` / `event_field_declaration`
//!   declarators, named `event_declaration`s, and `enum_member_declaration`s
//!   directly under a type body. NPM counts methods/constructors/operators/
//!   properties/indexers directly under a type body. C# visibility: a type
//!   member with no access modifier is `private` (NOT public), so only an
//!   explicit `public` counts toward NPA/NPM; interface members are implicitly
//!   public. `enum` members are implicitly public.

use mehen_antlr::runtime::token::Token;
use mehen_antlr::runtime::{FromRuleNode, Node, RuleNodeView, TerminalNodeView};
use mehen_antlr::{LocToken, LocTokenKind, ctx_span};
use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricTreeBuilder, SpaceRangeTracker, State,
    apply_state_to, finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

use mehen_csharp_parser::c_sharp_lexer as cl;
use mehen_csharp_parser::c_sharp_parser as cp;
// Typed contexts, used for *navigation* — their accessors reach only a rule's
// declared direct children, which is the property the untyped `child_rule` /
// `has_token` probes could only assert by comment. Dispatch stays on
// `rule_index()`: a metric walker is fundamentally one match over rule kinds.
use mehen_csharp_parser::c_sharp_parser::PrefixUnaryExpressionContext;

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
    /// This node is inside an `accessor_declaration`'s body. An accessor opens a
    /// metric space but is not itself a logical line, so an expression-bodied
    /// accessor (`get => _x;`) has nothing else to make its space non-empty —
    /// [`Walker::classify_loc_rule`] counts the `arrow_expression_clause` for it.
    /// Every other expression body hangs off a declaration that is already
    /// counted, where counting the clause too would double.
    in_accessor_body: bool,
    /// The declared name of the enclosing property / indexer / event, threaded
    /// down so a `get`/`set` accessor space can be named `Prop.get` rather than
    /// anonymous.
    ///
    /// (No `member_decl_start` or `space_opened_by_wrapper` here: Roslyn puts
    /// `attribute_list* modifier*` directly on each declaration, so a member's
    /// own span already covers its attributes and there is no wrapper to open the
    /// space at. Both fields existed only for the grammars-v4 shape.)
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
                cl::KW_ELSE => self.current().cognitive.increment_by_one(),
                cl::AMP_AMP => self.current().cognitive.observe_boolean("&&"),
                cl::PIPE_PIPE => self.current().cognitive.observe_boolean("||"),
                _ => {}
            }

            // Cyclomatic: each short-circuit boolean operator token is a
            // decision (independent of the cognitive run collapse).
            if matches!(tt, cl::AMP_AMP | cl::PIPE_PIPE) {
                self.current().cyclomatic.record_decision();
            }

            // ABC conditions: comparison / equality / boolean / null-coalescing
            // operator tokens.
            //
            // Unlike the grammars-v4 grammar, relational `<`/`>` can be counted
            // from the token stream directly: the prep spells `>>` as adjacent
            // `>` tokens rejoined in the parser behind `token_index_adjacent`, so
            // a `GT` token here is always a comparison and never half a shift.
            if is_abc_condition_token(tt) {
                self.current().abc.record_condition();
            }

            // ABC assignments: `++`/`--` (Fitzpatrick lists both under A).
            // The `=`/compound forms are handled at the assignment expression.
            if matches!(tt, cl::PLUS_PLUS | cl::MINUS_MINUS) {
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
        if hint.in_type_member && let Some(container) = hint.member_container {
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

        // No accessor-sibling hoisting needed: Roslyn's `accessor_list` holds
        // every accessor as a flat sibling, so `{ get; set; }` walks as two peer
        // `accessor_declaration` children. (grammars-v4 nested the second
        // accessor inside the first accessor's rule, which the walker had to
        // undo by re-visiting it after the first space closed.)
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
        // below without allocating.

        // An `else if` must not add cognitive nesting — only the flat `else` +1
        // applies. Roslyn spells the else branch as its own `else_clause`, so the
        // flag is set there and only when its body is a bare `if_statement`;
        // `else { if … }` is genuinely nested and gets no flag. That replaces the
        // old index-scan for the `if_body` following an `ELSE` token, plus the
        // transparency chain that carried the flag down to the nested `if`.
        let propagate_else = ri == cp::RULE_ELSE_CLAUSE && else_clause_is_else_if(ctx);

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
        // A `for`/`foreach`/`using`/`fixed` header declaration must not let its
        // declaration add a second LLOC — the statement already contributes the
        // single header logical line. Tag ONLY the direct children of the
        // header-bearing rules (a non-sticky flag), so a real local declaration
        // nested inside a lambda in the initializer still counts.
        //
        // Roslyn inlines the header declaration into the statement itself
        // (`for_statement : … LPAREN (variable_declaration? | …)`), so the tag
        // goes on the statement rather than on a separate
        // `for_initializer`/`resource_acquisition` rule.
        let in_for_init = matches!(
            ri,
            cp::RULE_FOR_STATEMENT | cp::RULE_USING_STATEMENT | cp::RULE_FIXED_STATEMENT
        );

        // A terminal directly under `identifier_token` is a name → Halstead
        // operand. This is what makes C#'s contextual keywords come out right:
        // the prep widens `identifier_token` to accept all 42 of them, so a
        // `KW_VAR` reached here is an operand rather than an operator.
        let in_identifier = ri == cp::RULE_IDENTIFIER_TOKEN;

        // Once inside an attribute, stay inside for the whole subtree so
        // attribute metadata records no executable complexity.
        let in_attributes = hint.in_attributes || ri == cp::RULE_ATTRIBUTE_LIST;

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
        // Inside an accessor's body from the accessor declaration downward, so
        // an expression-bodied `get => _x;` can count its own logical line. A
        // nested function or type resets it — their bodies are counted normally.
        let in_accessor_body = if ri == cp::RULE_ACCESSOR_DECLARATION {
            true
        } else if opens_type_like(ri) || opens_function_space(ri) {
            false
        } else {
            hint.in_accessor_body
        };

        for child in ctx.children() {
            let child_hint = ChildHint {
                is_else_branch: propagate_else,
                in_type_member: propagate_member,
                member_container,
                member_is_public,
                in_for_init,
                in_identifier,
                in_attributes,
                in_accessor_body,
                accessor_owner: accessor_owner.clone(),
            };
            self.visit(child, &child_hint);
        }
    }

    /// Compute the `(in_type_member, container, is_public)` hint for this rule's
    /// children.
    ///
    /// `member_declaration` marks a member position; the `base_*` rules beneath
    /// it are pure dispatch alternations (Roslyn's syntax model has abstract
    /// bases like `BaseMethodDeclarationSyntax`, so the generator emits one
    /// alternation rule per abstraction) and simply pass the position through.
    ///
    /// Visibility is read on the real declaration, because Roslyn puts
    /// `modifier*` there rather than on a wrapper — so unlike the grammars-v4
    /// shape there is nothing to resolve early and thread down.
    fn member_propagation(
        &self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        hint: &ChildHint,
        container_before_open: Option<ContainerKind>,
    ) -> (bool, Option<ContainerKind>, Option<bool>) {
        match ri {
            // A member position opens here. The container is captured BEFORE this
            // node's `maybe_open_space`, since a member that is itself a nested
            // type has already pushed that type's space.
            cp::RULE_MEMBER_DECLARATION => (true, container_before_open, None),
            // Pure dispatch layers: keep the inbound member position so the hint
            // reaches the real declaration one level deeper.
            cp::RULE_BASE_FIELD_DECLARATION
            | cp::RULE_BASE_METHOD_DECLARATION
            | cp::RULE_BASE_PROPERTY_DECLARATION
            | cp::RULE_BASE_TYPE_DECLARATION
            | cp::RULE_TYPE_DECLARATION => (
                hint.in_type_member,
                hint.member_container,
                hint.member_is_public,
            ),
            // The real declarations, which carry their own `modifier*`.
            //
            // C# visibility semantics: a class/struct member with no access
            // modifier is *private*, which is NOT public, so the default is
            // `false` — only an explicit `public` counts toward NPA/NPM.
            // Interface members are implicitly public.
            _ if hint.in_type_member && declares_member(ri) => {
                let container = hint.member_container;
                let default_public = matches!(container, Some(ContainerKind::Interface));
                let public = visibility_from_modifiers(ctx).unwrap_or(default_public);
                (true, container, Some(public))
            }
            // An `enum` member is implicitly public and carries no modifiers.
            cp::RULE_ENUM_MEMBER_DECLARATION => (true, hint.member_container, Some(true)),
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
        let mut state = self.new_space_state(span_ctx);
        // No row adoption needed: Roslyn puts `attribute_list* modifier*` on the
        // declaration itself, so the space's own span already starts at the
        // member's first attribute row rather than after it.
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
        let suppress_wmc = is_closure || fn_ctx.rule_index() == cp::RULE_LOCAL_FUNCTION_STATEMENT;
        self.push_space(kind, name, span_ctx, state, suppress_wmc);
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
            cp::RULE_ENUM_DECLARATION => {
                state.wmc.record_class_like();
                SpaceKind::Enum
            }
            // An `interface` carries no WMC (its members are not weighted),
            // matching the Java walker's interface handling.
            cp::RULE_INTERFACE_DECLARATION => SpaceKind::Interface,
            // `class`, `struct`, `record`, and `delegate` are class-like. A
            // `delegate` is a type declaration with a signature but no body; it
            // opens a (childless) class space so its own LOC/NArgs are
            // attributed.
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
    /// Open a metric space when `ctx` is a declaration that owns one.
    ///
    /// Roslyn's grammar puts `attribute_list* modifier*` directly on every
    /// declaration, so a member's span already starts at its attributes and its
    /// visibility is readable on the declaration itself. The grammars-v4 shape
    /// needed a wrapper rule (`class_member_declaration` → `all_member_modifiers`
    /// + `common_member_declaration`) to factor that prefix out of an LL
    /// decision, and the walker had to open the space at the wrapper and widen
    /// the span back. None of that applies here — hence no wrapper handling, no
    /// span widening, and no `space_opened_by_wrapper` suppression.
    fn maybe_open_space(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) -> bool {
        match ri {
            cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_LOCAL_FUNCTION_STATEMENT => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // Property / indexer / event accessors are each their own function
            // space (SonarC# counts them as methods): `get`/`set` bodies carry
            // real complexity and are the C# analogue of Kotlin's
            // `getter`/`setter`.
            //
            // One rule covers all of get/set/init/add/remove, and `accessor_list`
            // holds them as flat siblings — so unlike grammars-v4 there is no
            // asymmetry where the second accessor nests inside the first, and no
            // sibling-hoisting is needed.
            cp::RULE_ACCESSOR_DECLARATION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // Closures: a lambda (`x => …`, `(a, b) => …`) or an anonymous
            // method (`delegate(int x) { … }`). NOM/NArgs record them as
            // closures, and their cyclomatic must NOT roll into the enclosing
            // type's WMC (WMC weights *methods*). Roslyn splits lambdas by
            // parameter shape, and all three are real rules rather than labeled
            // alternatives — so no `is_anonymous_method` probe.
            cp::RULE_SIMPLE_LAMBDA_EXPRESSION
            | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
            | cp::RULE_ANONYMOUS_METHOD_EXPRESSION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Closure);
                true
            }
            _ if opens_type_like(ri) => {
                self.open_type_space(ctx, ctx);
                true
            }
            _ => false,
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

        // Roslyn gives each statement form its own rule, so these are rule-index
        // matches rather than keyword probes on a shared
        // `simple_embedded_statement`. That is also more precise: a
        // `has_token(IF)` probe fires for an `if` anywhere inside the node,
        // whereas a rule match cannot.
        match ri {
            // Cyclomatic + ABC always; cognitive nesting unless this is an
            // `else if`, whose flat +1 is emitted at the `else_clause`.
            cp::RULE_IF_STATEMENT => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                if !hint.is_else_branch {
                    self.current().cognitive.increase_nesting(eff);
                    self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                }
                self.current().cognitive.boolean_seq.reset();
            }
            cp::RULE_WHILE_STATEMENT
            | cp::RULE_DO_STATEMENT
            | cp::RULE_FOR_STATEMENT
            | cp::RULE_FOR_EACH_STATEMENT
            | cp::RULE_FOR_EACH_VARIABLE_STATEMENT => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().cognitive.boolean_seq.reset();
            }
            // `switch` itself adds cognitive nesting but not cyclomatic — the
            // `case` labels carry the decisions.
            cp::RULE_SWITCH_STATEMENT => {
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().cognitive.boolean_seq.reset();
            }
            // NOTE: `try` and `lock` deliberately score NOTHING. SonarSource's
            // cognitive-complexity spec increments on `catch` (a handler is the
            // structure a reader must follow), not on the `try` block itself, and
            // `lock`/`synchronized` is not in the increment list at all. The
            // `catch` nesting is applied at `catch_clause` below, matching
            // `mehen-java`.
            cp::RULE_RETURN_STATEMENT | cp::RULE_THROW_STATEMENT => {
                self.current().nexit.record_exit();
                self.current().cognitive.boolean_seq.reset();
            }
            // `yield return` / `yield break` both leave the iterator.
            cp::RULE_YIELD_STATEMENT => {
                self.current().nexit.record_exit();
                self.current().cognitive.boolean_seq.reset();
            }
            // `goto` (including `goto case` / `goto default`) is goto-like: a
            // flat +1, no nesting.
            cp::RULE_GOTO_STATEMENT => {
                self.current().cognitive.increment_by_one();
                self.current().cognitive.boolean_seq.reset();
            }
            // A `case` label is a decision (cyclomatic) and a condition (ABC) in
            // both its constant and pattern forms; `default:` is its own rule and
            // is neither. The `switch` already opened the cognitive nesting
            // level, so a `case` adds no further nesting.
            cp::RULE_CASE_SWITCH_LABEL | cp::RULE_CASE_PATTERN_SWITCH_LABEL => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
            }
            // A `when` guard is a distinct boolean test — on a `case` label
            // (`case int i when i > 0:`) or a switch-expression arm — so it
            // records one ABC condition of its own. `catch (E e) when (…)` is a
            // separate rule with the same meaning.
            cp::RULE_WHEN_CLAUSE | cp::RULE_CATCH_FILTER_CLAUSE => {
                self.current().abc.record_condition();
            }
            // `catch` is cognitive-only (matches SonarC#/SonarJava): a nesting
            // increment plus an ABC condition, but no cyclomatic decision. One
            // rule now covers both the typed and bare forms.
            cp::RULE_CATCH_CLAUSE => {
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().abc.record_condition();
            }
            // A `throw` *expression* (`x ?? throw new E()`, C# 7) is an exit that
            // the statement form above never sees.
            cp::RULE_THROW_EXPRESSION => self.current().nexit.record_exit(),
            // Statement-shaped positions that are not one of the forms above
            // still start a fresh boolean sequence, so operators never collapse
            // across a boundary — `F(a && b); G(c && d)` is +2, not +1.
            cp::RULE_EXPRESSION_STATEMENT
            | cp::RULE_LOCAL_DECLARATION_STATEMENT
            | cp::RULE_ARROW_EXPRESSION_CLAUSE
            | cp::RULE_BLOCK => {
                self.current().cognitive.boolean_seq.reset();
            }
            // The prefix `!` records a not-operator so a following same-kind
            // boolean operator is not collapsed with the one before the negation
            // (`a && !b && c` is one run in SonarSource's model, but the run
            // tracker needs the marker to keep parity with Kotlin).
            cp::RULE_PREFIX_UNARY_EXPRESSION => {
                if PrefixUnaryExpressionContext::from_rule_node(ctx)
                    .is_some_and(|expr| expr.bang_token().is_some())
                {
                    self.current().cognitive.boolean_seq.not_operator("!");
                }
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
        if matches!(
            ri,
            cp::RULE_SIMPLE_LAMBDA_EXPRESSION | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
        ) && !lambda_body_is_block(ctx)
        {
            self.current().loc.observe_lloc();
            return;
        }
        // An expression-bodied accessor (`get => _x;`) opens a space whose only
        // content is that expression — no statement — so the space would report
        // `lloc = 0` without counting the body itself as one logical line.
        //
        // Roslyn spells every expression body as `arrow_expression_clause`, so
        // this is one rule rather than the old `body`/`accessor_body`/
        // `local_function_body` trio.
        //
        // It must fire only where the enclosing declaration is not itself counted
        // below, or `int F() => 1;` would count twice. `in_accessor_body` marks
        // the one case that needs it: an `accessor_declaration` opens a space but
        // is not a logical line of its own, so an expression-bodied accessor has
        // nothing else to count.
        if ri == cp::RULE_ARROW_EXPRESSION_CLAUSE && hint.in_accessor_body {
            self.current().loc.observe_lloc();
            return;
        }

        if matches!(
            ri,
            // Statement-shaped rules. Roslyn gives each statement form its own
            // rule, so this replaces the single `simple_embedded_statement`.
            cp::RULE_EXPRESSION_STATEMENT
                | cp::RULE_LOCAL_DECLARATION_STATEMENT
                | cp::RULE_LOCAL_FUNCTION_STATEMENT
                | cp::RULE_IF_STATEMENT
                | cp::RULE_WHILE_STATEMENT
                | cp::RULE_DO_STATEMENT
                | cp::RULE_FOR_STATEMENT
                | cp::RULE_FOR_EACH_STATEMENT
                | cp::RULE_FOR_EACH_VARIABLE_STATEMENT
                | cp::RULE_SWITCH_STATEMENT
                | cp::RULE_TRY_STATEMENT
                | cp::RULE_USING_STATEMENT
                | cp::RULE_LOCK_STATEMENT
                | cp::RULE_FIXED_STATEMENT
                | cp::RULE_CHECKED_STATEMENT
                | cp::RULE_UNSAFE_STATEMENT
                | cp::RULE_RETURN_STATEMENT
                | cp::RULE_THROW_STATEMENT
                | cp::RULE_YIELD_STATEMENT
                | cp::RULE_BREAK_STATEMENT
                | cp::RULE_CONTINUE_STATEMENT
                | cp::RULE_GOTO_STATEMENT
                | cp::RULE_LABELED_STATEMENT
                // Declaration-shaped rules. `empty_statement` is deliberately
                // absent: a bare `;` is not a logical line. So is `block`, which
                // is a wrapper whose inner statements each count.
                | cp::RULE_FIELD_DECLARATION
                | cp::RULE_EVENT_FIELD_DECLARATION
                | cp::RULE_EVENT_DECLARATION
                | cp::RULE_METHOD_DECLARATION
                | cp::RULE_CONSTRUCTOR_DECLARATION
                | cp::RULE_DESTRUCTOR_DECLARATION
                | cp::RULE_OPERATOR_DECLARATION
                | cp::RULE_CONVERSION_OPERATOR_DECLARATION
                | cp::RULE_PROPERTY_DECLARATION
                | cp::RULE_INDEXER_DECLARATION
                | cp::RULE_ENUM_MEMBER_DECLARATION
                | cp::RULE_CLASS_DECLARATION
                | cp::RULE_STRUCT_DECLARATION
                | cp::RULE_INTERFACE_DECLARATION
                | cp::RULE_ENUM_DECLARATION
                | cp::RULE_RECORD_DECLARATION
                | cp::RULE_DELEGATE_DECLARATION
                | cp::RULE_NAMESPACE_DECLARATION
                | cp::RULE_FILE_SCOPED_NAMESPACE_DECLARATION
                | cp::RULE_USING_DIRECTIVE
                | cp::RULE_EXTERN_ALIAS_DIRECTIVE
        ) {
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
            // A field or event-field declaration can declare several variables
            // (`int a, b, c;` / `event E a, b;`). `const` is a modifier here
            // rather than a separate rule, so there is no `constant_declaration`
            // arm — a `const int a, b;` reaches this same path.
            cp::RULE_FIELD_DECLARATION | cp::RULE_EVENT_FIELD_DECLARATION => {
                let count = declarator_count(ctx).max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, public);
                }
            }
            // A named `event` with accessors declares exactly one member.
            cp::RULE_EVENT_DECLARATION => {
                self.current().npa.record_attribute(container, public);
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
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_PROPERTY_DECLARATION
            | cp::RULE_INDEXER_DECLARATION => {
                self.current().npm.record_method(container, public);
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
        cp::RULE_CLASS_DECLARATION
            | cp::RULE_STRUCT_DECLARATION
            | cp::RULE_INTERFACE_DECLARATION
            | cp::RULE_ENUM_DECLARATION
            | cp::RULE_DELEGATE_DECLARATION
            // Roslyn models `record` / `record struct` as their own node rather
            // than a modifier on a class, so it is a peer here.
            | cp::RULE_RECORD_DECLARATION
    )
}

/// Rules that open a function/closure metric space (mirrors the function arms
/// of `maybe_open_space`).
///
/// Roslyn declares one rule per accessor *list* entry rather than one per
/// accessor keyword, so a single `accessor_declaration` covers get/set/init/
/// add/remove — the keyword is a child token, not part of the rule name.
fn opens_function_space(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_LOCAL_FUNCTION_STATEMENT
            | cp::RULE_ACCESSOR_DECLARATION
            // Lambdas are split by parameter shape (`x => …` vs `(x, y) => …`),
            // and `delegate { … }` is a third node.
            | cp::RULE_SIMPLE_LAMBDA_EXPRESSION
            | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
            | cp::RULE_ANONYMOUS_METHOD_EXPRESSION
    )
}

/// Whether a lambda's body is a block (`… => { … }`) rather than an expression.
/// A block body's statements are counted individually for LLOC; an expression
/// body makes the lambda itself one logical line.
///
/// Roslyn writes the body as `(block | expression)` directly on the lambda rule,
/// so there is no `anonymous_function_body` wrapper to descend through.
fn lambda_body_is_block(ctx: RuleNodeView<'_>) -> bool {
    ctx.child_rule(cp::RULE_BLOCK).is_some()
}

/// Whether an `else_clause`'s body is a bare `if_statement` — i.e. this is an
/// `else if` chain rather than a nested `if` inside an `else` block.
///
/// Roslyn spells the else branch as its own `else_clause : KW_ELSE statement`
/// rule, so this is one direct-child check. (grammars-v4 wrote
/// `if_statement : IF (…) if_body (ELSE if_body)?`, which forced the walker to
/// find the `if_body` appearing *after* the `ELSE` terminal by index, then track
/// transparency through an `if_body`/`embedded_statement`/`statement` chain.)
///
/// A block stops the chain: `else { if … }` is genuinely nested, and so is a
/// statement with its own control-flow keyword.
fn else_clause_is_else_if(ctx: RuleNodeView<'_>) -> bool {
    ctx.child_rule(cp::RULE_STATEMENT)
        .and_then(|stmt| stmt.child_rule(cp::RULE_IF_STATEMENT))
        .is_some()
}

/// The declared name of a member/type: its first `identifier` child's covered
/// text. Falls back to the `member_name`/`method_member_name` wrapper's text
/// for the members that spell their name through one.
fn name_from_identifier(ctx: RuleNodeView<'_>) -> Option<String> {
    for child in ctx.children() {
        let Some(c) = child.as_rule() else { continue };
        // Roslyn spells every declared name as `identifier_token`; there is no
        // `member_name` / `method_member_name` indirection.
        if c.rule_index() == cp::RULE_IDENTIFIER_TOKEN {
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
    // One `accessor_declaration` covers every accessor kind, with the keyword as
    // a direct child token — so the kind is read rather than inferred from which
    // of five rules matched. That also picks up `init` (C# 9), which the
    // grammars-v4 shape had no rule for.
    if ctx.rule_index() == cp::RULE_ACCESSOR_DECLARATION {
        let suffix = if ctx.has_token(cl::KW_GET) {
            "get"
        } else if ctx.has_token(cl::KW_SET) {
            "set"
        } else if ctx.has_token(cl::KW_INIT) {
            "init"
        } else if ctx.has_token(cl::KW_ADD) {
            "add"
        } else if ctx.has_token(cl::KW_REMOVE) {
            "remove"
        } else {
            // The grammar also allows a bare `identifier_token` here, for
            // Roslyn's error-recovery shapes.
            "accessor"
        };
        return Some(match &hint.accessor_owner {
            Some(owner) => format!("{owner}.{suffix}"),
            None => suffix.to_string(),
        });
    }
    // An operator's name is `operator <op>`. Roslyn spells the operator as a
    // direct token choice on the declaration rather than a separate
    // `overloadable_operator` rule, so name it from the declaration's text up to
    // the parameter list.
    if ctx.rule_index() == cp::RULE_OPERATOR_DECLARATION {
        return Some("operator".to_string());
    }
    if ctx.rule_index() == cp::RULE_CONVERSION_OPERATOR_DECLARATION {
        return Some("operator".to_string());
    }
    // A lambda / anonymous method is anonymous.
    if matches!(
        ctx.rule_index(),
        cp::RULE_SIMPLE_LAMBDA_EXPRESSION
            | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
            | cp::RULE_ANONYMOUS_METHOD_EXPRESSION
    ) {
        return None;
    }
    // Every other function-shaped declaration — including a local function —
    // carries its own `identifier_token` directly, so there is no header rule to
    // descend into.
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
    // Roslyn spells every parameter position with the same `parameter` rule, so
    // one lookup covers methods, constructors, operators, local functions,
    // lambdas, and anonymous methods alike. (grammars-v4 needed five distinct
    // shapes — `fixed_parameter`, `parameter_array`, `arg_declaration`,
    // `explicit_anonymous_function_parameter`, and a bare identifier — because LL
    // parsing forced a separate rule per position.) A `params` array is a
    // `KW_PARAMS` modifier on an ordinary parameter, so it counts as one.
    //
    // `simple_lambda_expression` (`x => …`) holds its single `parameter`
    // directly, with no enclosing list.
    if ctx.rule_index() == cp::RULE_SIMPLE_LAMBDA_EXPRESSION {
        return ctx.child_rules(cp::RULE_PARAMETER).count() as u32;
    }
    // An indexer's parameters are bracketed (`this[int i]`).
    let list = ctx
        .child_rule(cp::RULE_PARAMETER_LIST)
        .or_else(|| ctx.child_rule(cp::RULE_BRACKETED_PARAMETER_LIST));
    list.map(|list| list.child_rules(cp::RULE_PARAMETER).count() as u32)
        .unwrap_or(0)
}

/// Count the declarators of a field / event declaration (`int a, b, c;`).
///
/// Roslyn has one `variable_declaration : type variable_declarator (','
/// variable_declarator)*` for all of them — no separate `constant_declarators`
/// list, since `const` is a modifier rather than a distinct declaration rule.
fn declarator_count(ctx: RuleNodeView<'_>) -> u32 {
    ctx.child_rule(cp::RULE_VARIABLE_DECLARATION)
        .map(|decl| decl.child_rules(cp::RULE_VARIABLE_DECLARATOR).count() as u32)
        .unwrap_or(0)
}

/// Resolve an explicit visibility from a member/type wrapper's
/// `all_member_modifiers`: `Some(true)` if it carries `public`, `Some(false)`
/// if it carries `private`/`protected`/`internal`, `None` if no access modifier
/// is present (caller applies the container default).
///
/// `internal` is *not* public: it is assembly-scoped, so it does not
/// contribute to the type's public API surface (NPA/NPM).
/// The rules that are a *real* member declaration — the ones carrying their own
/// `attribute_list* modifier*`, as opposed to the `base_*` dispatch alternations
/// above them.
fn declares_member(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_FIELD_DECLARATION
            | cp::RULE_EVENT_FIELD_DECLARATION
            | cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_PROPERTY_DECLARATION
            | cp::RULE_INDEXER_DECLARATION
            | cp::RULE_EVENT_DECLARATION
            | cp::RULE_DELEGATE_DECLARATION
            | cp::RULE_CLASS_DECLARATION
            | cp::RULE_STRUCT_DECLARATION
            | cp::RULE_INTERFACE_DECLARATION
            | cp::RULE_ENUM_DECLARATION
            | cp::RULE_RECORD_DECLARATION
    )
}

/// Resolve a declaration's access from its own `modifier*` children.
///
/// `Some(true)` for an explicit `public`, `Some(false)` when another access
/// modifier is present, `None` when the declaration states none — the caller
/// supplies the container's default, since an unmarked class member is private
/// while an unmarked interface member is public.
///
/// Roslyn puts `modifier*` directly on each declaration, so this reads the real
/// node rather than a wrapper. `modifier_children()` is typed and reaches only
/// direct children, so a modifier on a *nested* declaration cannot leak in.
fn visibility_from_modifiers(ctx: RuleNodeView<'_>) -> Option<bool> {
    let mut saw_non_public = false;
    for modifier in ctx.child_rules(cp::RULE_MODIFIER) {
        if modifier.has_token(cl::KW_PUBLIC) {
            return Some(true);
        }
        if modifier.has_token(cl::KW_PRIVATE)
            || modifier.has_token(cl::KW_PROTECTED)
            || modifier.has_token(cl::KW_INTERNAL)
            || modifier.has_token(cl::KW_FILE)
        {
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
        cl::EQ_EQ
            | cl::NE
            | cl::AMP_AMP
            | cl::PIPE_PIPE
            | cl::QUESTION_QUESTION
            // Relational comparisons. Safe to count from the token stream here
            // because the prep splits `>>` into adjacent `>` tokens rejoined in
            // the parser, so a `GT` is never half a shift operator.
            | cl::LT
            | cl::GT
            | cl::LE
            | cl::GE
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
            | cl::DEC_INT_LIT
            | cl::HEX_INT_LIT
            | cl::BIN_INT_LIT
            | cl::REAL_LIT
            | cl::CHAR_LIT
            | cl::STRING_LIT
            | cl::VERBATIM_STRING_LIT
            | cl::ML_RAW_STRING_LIT
            | cl::SL_RAW_STRING_LIT
            | cl::KW_TRUE
            | cl::KW_FALSE
            | cl::KW_NULL
            | cl::KW_THIS
            | cl::KW_BASE
            // Every interpolated-string content piece — literal text, escapes,
            // doubled braces, and the format specifier — arrives as this one
            // token: the hand-written lexer's mode rules all `type(…)` to it.
            | cl::INTERPOLATED_TEXT
            | cl::XML_TEXT_LIT
    ) {
        return HalsteadClass::Operand;
    }

    if matches!(
        tt,
        cl::WHITESPACES
            | cl::BYTE_ORDER_MARK
            | cl::SINGLE_LINE_COMMENT
            | cl::DELIMITED_COMMENT
            | cl::SINGLE_LINE_DOC_COMMENT
            | cl::DELIMITED_DOC_COMMENT
            // mehen routes preprocessor directives to their own channel rather
            // than evaluating them, so a directive line is neither operator nor
            // operand. (The grammars-v4 lexer instead emitted a
            // `SKIPPED_SECTION` for the inactive branch of an `#if`.)
            | cl::DIRECTIVE_LINE
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
