// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ANTLR-based Java metric walker.
//!
//! Drives a recursive descent over the ANTLR `ParseTree` (entry rule
//! `compilationUnit`) and produces a populated [`MetricSpace`]. The structure
//! mirrors the `mehen-kotlin` walker — one [`State`] per space,
//! finalize-and-merge on close, with the parent-less ANTLR tree handled by
//! threading context **top-down**.
//!
//! ## Grammar shape (vs Kotlin)
//!
//! The grammars-v4 Java grammar differs from the Kotlin spec grammar in two
//! ways that shape every classification here:
//!
//! - **Control flow is not separate named rules.** `if`/`for`/`while`/`do`/
//!   `switch`/`try`/`return`/`throw`/`break`/`continue`/`yield` are all
//!   *alternatives of the single `statement` rule*, discriminated by a leading
//!   keyword token (`IF`, `FOR`, …) that is a direct child of the `statement`
//!   context. So the walker inspects `statement` tokens rather than matching a
//!   distinct `RULE_IF_EXPRESSION`-style index.
//! - **Operators are not separate named rules.** All binary/ternary operators
//!   are alternatives of the single `expression` rule (labeled alternatives
//!   that ANTLR's Rust target flattens into one `RULE_EXPRESSION` context), so
//!   short-circuit `&&`/`||`, comparisons, and the ternary `?` are detected by
//!   scanning the operator *tokens* that are direct children of an
//!   `expression` context.
//!
//! ## Metric coverage (SonarJava-aligned)
//!
//! - **Cyclomatic**: `if`, every loop (`for`/`while`/`do`), each `case`
//!   label, the ternary `?`, and each short-circuit `&&`/`||`. `catch`,
//!   `switch` itself, and `default` are not decisions (matches SonarJava's
//!   cyclomatic; `catch` counts only in cognitive).
//! - **Cognitive**: nesting on `if`, loops, `switch` (statement *and*
//!   expression), `catch`, and the ternary; flat `+1` on `else`/`else if` and
//!   on labeled `break`/`continue`; a *parent-relative* boolean-run collapse on
//!   `&&`/`||` (a boolean node adds `+1` only when its operator differs from
//!   its enclosing boolean operator, matching SonarSource's tree-based rule).
//! - **ABC**: assignments via `=`, compound-assign, and `++`/`--` operators and
//!   via any initialized declarator (`variableDeclarator`/`fieldDeclaration`,
//!   `var x = e`, try-with-resources `T r = e`); branches via every
//!   `methodCall` and object creation (`new`); conditions via
//!   `if`/`case`/`catch`/loops/comparison & equality/`&&`/`||`/ternary/
//!   `instanceof` (bit-shifts `<<`/`>>`/`>>>` are excluded — they are not
//!   relational).
//! - **NExit**: `return` and `throw` statements.
//! - **NArgs**: `formalParameter` count for methods/constructors;
//!   `lambdaParameters` count for lambdas.
//! - **NOM**: every `methodDeclaration`, `constructorDeclaration`,
//!   `compactConstructorDeclaration`, `interfaceMethodDeclaration`,
//!   `annotationMethodRest` is a function space; every `lambdaExpression` is a
//!   closure-shaped function space.
//! - **LOC**: PLOC from per-space code-token rows during the walk, LLOC from
//!   statement/declaration-shaped rules, CLOC from a source-ordered pass over
//!   the hidden-channel comment tokens routed via `SpaceRangeTracker`.
//! - **Halstead**: per-token operator/operand classification — keywords and
//!   punctuation are operators; identifiers, literals, `this`, `super` are
//!   operands (deduped by text).
//! - **NPA / NPM / WMC**: class-vs-interface routing by the type-declaration
//!   keyword. NPA counts `fieldDeclaration` variables (and `recordComponent`s)
//!   directly under a class/interface body. NPM counts methods/constructors
//!   (including generic `<T> …` members) directly under a class/interface body.
//!   Java visibility: a class member with no access modifier is
//!   package-private (NOT public), so only an explicit `public` counts toward
//!   NPA/NPM; interface/annotation members are implicitly public.

use mehen_antlr::runtime::token::Token;
use mehen_antlr::runtime::{Node, RuleNodeView, TerminalNodeView};
use mehen_antlr::{LocToken, LocTokenKind, ctx_span};
use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricTreeBuilder, SpaceRangeTracker, State,
    apply_state_to, finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

use mehen_java_parser::java_lexer as jl;
use mehen_java_parser::java_parser as jp;

/// Drive the walk over the parsed `compilationUnit` tree and return the unit
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
            walker.visit(child, ChildHint::default());
        }
    }

    let mut unit_state = walker.stack.pop().expect("walker stack underflow");

    // CLOC pass: route each comment to the deepest enclosing space (or the
    // unit) in source order (mirrors `mehen-kotlin`/`mehen-python`).
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
/// exactly as the Kotlin walker uses it.
#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

/// Context threaded *down* into a child during the walk (ANTLR contexts have
/// no parent pointer).
#[derive(Clone, Copy, Debug, Default)]
struct ChildHint {
    /// This `statement` is the `else`-branch body of an enclosing `if`
    /// statement. An `if` reached through this hint is an `else if` and must
    /// not add cognitive nesting (only the flat `else` +1 applies).
    is_else_branch: bool,
    /// This node is a direct member position of the enclosing class/interface
    /// body, so NPA/NPM should consider it.
    in_class_member: bool,
    /// The container kind of the enclosing class-like body, so a member's
    /// counters route to class-vs-interface buckets and inherit the
    /// interface-default-public rule.
    member_container: Option<ContainerKind>,
    /// The member's resolved visibility, captured at the body-declaration
    /// wrapper (`classBodyDeclaration: modifier* memberDeclaration`) where the
    /// `modifier`s are siblings of the declaration — the declaration itself
    /// has no parent pointer and does not carry them. `None` outside a member
    /// position.
    member_is_public: Option<bool>,
    /// The immediately-enclosing short-circuit boolean operator (`&&` / `||`),
    /// threaded down through `expression` descendants (transparent parens
    /// included) so the cognitive boolean-run counter fires only at the ROOT of
    /// a logical-operator tree. A `&&`/`||` node whose `parent_bool_op` is
    /// `None` is a run root: it flattens its whole subtree in source order and
    /// counts +1 per operator-kind change (SonarSource's rule). A nested
    /// `&&`/`||` reached as a logical operand has `parent_bool_op == Some(_)`
    /// and is consumed by the root's flatten, so it does not count again.
    /// Threading resets to `None` at any non-`expression`/`primary` boundary —
    /// statement, `arguments`, `methodCall`, a comparison/ternary expression —
    /// which isolates independent boolean expressions. `None` outside one.
    parent_bool_op: Option<BoolOp>,
    /// This node is (within) a `for` statement's `forControl` header, so a
    /// `localVariableDeclaration` reached through it is the loop initializer,
    /// not a standalone statement — it must not add its own LLOC (the `for`
    /// statement already contributes the single header logical line).
    in_for_init: bool,
    /// This terminal is the token of an `identifier`/`typeIdentifier` rule.
    /// Java's contextual keywords (`record`, `var`, `yield`, `sealed`,
    /// `permits`, `module`, …) lex as dedicated token types but are
    /// identifiers in name position, so a terminal reached through this hint is
    /// a Halstead *operand* regardless of its token type (mirrors the Kotlin
    /// walker's `simpleIdentifier` handling).
    in_identifier: bool,
    /// We are inside a constant-specific enum-constant body
    /// (`enum E { A { … } }`), which opens no metric space of its own. Its
    /// members belong to `A`'s anonymous subclass, not the lexically-enclosing
    /// enum, so their `classBodyDeclaration`s must NOT seed `in_class_member`
    /// (NPA/NPM) and their methods must NOT roll into the enum's WMC. Cleared
    /// once a *real* nested class-like declaration opens its own space (its
    /// members belong to that class). Mirrors the Kotlin walker's
    /// `in_anon_body`.
    in_anon_body: bool,
    /// The enclosing record's component count, threaded down from
    /// `recordDeclaration`. A *compact* constructor (`record R(int x) { R {} }`)
    /// has no `formalParameters` node — its parameter list *is* the record's
    /// components — so its NArgs must come from this count. `None` outside a
    /// record.
    record_component_count: Option<u32>,
    /// The enclosing record's own visibility, threaded down from
    /// `recordDeclaration`. Java gives a modifier-less *compact* canonical
    /// constructor (`public record R(int x) { R {} }`) the record's access
    /// level, but the compact ctor is reached directly under the modifier-less
    /// `recordBody`, so the ambient `member_is_public` there is always `false`.
    /// A compact ctor with no explicit modifier falls back to this instead.
    /// `None` outside a record.
    enclosing_record_public: Option<bool>,
    /// The 0-based start line of the enclosing member's body-declaration
    /// wrapper (`classBodyDeclaration: modifier* memberDeclaration`), threaded
    /// down so a method/constructor space can widen its span upward to cover
    /// its own-line modifiers/annotations. In the grammars-v4 Java grammar the
    /// `modifier`s (including annotations) are SIBLINGS of the declaration on
    /// the wrapper, so the declaration's own `ctx_span` starts *after* them —
    /// leaving `@Deprecated\npublic void m() {}`'s annotation row attributed to
    /// the enclosing class. The wrapper's start line is where the declaration
    /// truly begins. Carries the wrapper's `(start_byte, start_line)` so the
    /// method space widens both its LOC span (row attribution) and its
    /// comment-routing byte range. `None` outside a member position.
    member_decl_start: Option<(u32, u32)>,
    /// This method/constructor declaration's function space was already opened
    /// by its enclosing body-declaration wrapper (so the wrapper's own-line
    /// modifiers/annotations are visited *inside* the method space, giving the
    /// method correct Halstead/PLOC/span). The declaration node must therefore
    /// NOT open a second space of its own.
    space_opened_by_wrapper: bool,
    /// We are inside an `annotation` (`@Ann(value = 1)`). Annotation values are
    /// compile-time metadata, not executable code, so ABC assignment
    /// accounting must be suppressed here: the grammar's `IsNotIdentifierAssign`
    /// predicate (which would parse `value = 1` as `identifier '=' …` rather
    /// than an assignment expression) is dropped by the Rust generator, so a
    /// named element value otherwise reaches the `RULE_EXPRESSION` assignment
    /// arm with an `=` token and inflates ABC.
    in_annotation: bool,
}

/// A short-circuit boolean operator, for parent-relative cognitive
/// boolean-run collapse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoolOp {
    And,
    Or,
}

struct Walker<'a> {
    line_index: &'a LineIndex,
    source_len: usize,
    tree: MetricTreeBuilder,
    stack: Vec<State>,
    kinds: Vec<SpaceKind>,
    /// Parallel to `stack`/`kinds`: whether the closing function space must
    /// NOT contribute its cyclomatic to the parent's WMC. Set for functions
    /// opened inside a constant-specific enum-constant body — that body opens
    /// no space of its own, so the function closes with the *enum* as parent,
    /// but it belongs to the constant's anonymous subclass, not the enum.
    suppress_parent_wmc: Vec<bool>,
    cognitive: CognitiveContext,
    loc_routing: SpaceRangeTracker,
}

impl Walker<'_> {
    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    fn visit(&mut self, node: Node<'_>, hint: ChildHint) {
        if let Some(rule) = node.as_rule() {
            self.visit_rule(rule, hint);
        } else if let Some(term) = node.as_terminal() {
            self.visit_terminal(term, hint);
        }
        // Error leaves carry no metric contribution; they are surfaced as
        // diagnostics by `mehen_antlr::collect_errors` in the analyzer.
    }

    fn visit_terminal(&mut self, term: TerminalNodeView<'_>, hint: ChildHint) {
        let tt = term.symbol().token_type();

        // Halstead operator/operand token classification. A terminal reached
        // through an `identifier`/`typeIdentifier` rule is always an operand —
        // this covers Java's contextual keywords (`record`, `var`, `yield`, …)
        // used as names, which carry dedicated token types but are identifiers
        // here.
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
                let text = term.symbol().text();
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
        // A single visible token can span multiple physical lines — a Java
        // text block (`"""…"""`, `TEXT_BLOCK`) is one token covering several
        // rows. Record *every* row it covers as code, or the interior rows sit
        // inside the enclosing span with no PLOC observation and are reported
        // as phantom blank lines (`blank = sloc - ploc - only_comment`).
        if tt >= 0 {
            let start_row = (term.symbol().line() as u32).saturating_sub(1);
            let extra_rows = term.symbol().text().bytes().filter(|&b| b == b'\n').count() as u32;
            for row in start_row..=start_row.saturating_add(extra_rows) {
                self.current().loc.observe_code_line(row);
            }
        }
    }

    fn visit_rule(&mut self, ctx: RuleNodeView<'_>, hint: ChildHint) {
        let ri = ctx.rule_index();
        let saved_cognitive = self.cognitive;

        // NPA / NPM: classify a direct member of the enclosing class/interface
        // body before opening any space for this node (so the kinds stack
        // still has the class on top).
        // A method/constructor whose space was already opened by its wrapper
        // had its NPM recorded there (into the class, before the space opened);
        // skip re-classifying here (this node now sits inside the method space,
        // so it would misroute NPM into the method).
        if hint.in_class_member
            && !hint.space_opened_by_wrapper
            && let Some(container) = hint.member_container
        {
            let public = hint.member_is_public.unwrap_or(true);
            self.classify_class_member(ctx, ri, container, public, hint);
        }

        // Capture the enclosing class-like container BEFORE opening any space
        // for this node. `member_propagation` (run inside `visit_children`,
        // after the open) resolves a member's container/visibility from the
        // stack — but a body-declaration wrapper may now open a nested type
        // space here (round 29), so `self.enclosing_container()` would then see
        // that just-opened type instead of the real enclosing scope. Passing
        // the pre-open container keeps interface/annotation member visibility
        // correct (e.g. an interface-nested record's compact ctor stays public).
        let container_before_open = self.enclosing_container();

        let opened = self.maybe_open_space(ctx, ri, hint);
        self.classify_rule(ctx, ri, hint);

        self.visit_children(ctx, ri, hint, container_before_open);

        if opened {
            self.close_space();
        }
        self.cognitive = saved_cognitive;
    }

    fn visit_children(
        &mut self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        hint: ChildHint,
        container_before_open: Option<ContainerKind>,
    ) {
        // `NodeChildren` is a cheap `Clone` slice-iterator, so it is re-walked
        // (below, and for the `else`/anon-body scans here) without allocating —
        // the hot path avoids collecting children into a `Vec` for every node.

        // For an `if` statement, the `else`-branch body is the `statement`
        // that appears after the `ELSE` token. Tag it so an `if` reached
        // through it (without an intervening `block`) is recognized as
        // `else if` and does not add nesting.
        let else_body_idx = if is_if_statement(ctx, ri) {
            else_branch_index(ctx.children())
        } else {
            None
        };
        // `is_else_branch` also flows through a *transparent* `statement`
        // wrapper so an `else if` chain is recognized even when the grammar
        // nests the inner `if` under the outer statement's `else` position
        // (e.g. a label wrapper `else lbl: if …`). It must NOT flow through:
        //   - a `block` (`else { if … }` is genuinely nested);
        //   - an actual `if` statement (`if` targets its else child precisely
        //     via `else_body_idx`; blanket propagation would wrongly stamp the
        //     flag onto the *then*-branch too — `else if (b) if (d) {}`);
        //   - any statement that introduces its OWN control-flow construct
        //     (`while`/`for`/`do`/`switch`/`try`/`synchronized`/…). Such a
        //     statement's body is a genuinely nested scope, not an else-if — so
        //     an `if` in an `else while (c) if (b) {}` loop body must keep its
        //     nesting increment.
        let propagate_else = hint.is_else_branch && statement_is_else_transparent(ctx, ri);

        // Class/interface body member positions originate at
        // `classBodyDeclaration` / `interfaceBodyDeclaration`, then flow
        // through the transparent `memberDeclaration` /
        // `interfaceMemberDeclaration` wrappers to the real member rule. The
        // visibility is resolved here (the body-declaration level) because the
        // `modifier`s are siblings of the member declaration, not its
        // children.
        let (propagate_member, member_container, member_is_public) =
            self.member_propagation(ctx, ri, hint, container_before_open);

        // Capture the member's body-declaration wrapper start line so a
        // method/constructor space can widen its span upward to cover its
        // own-line modifiers/annotations (siblings of the declaration on the
        // wrapper). Set at the wrapper that opens the member position; a
        // transparent wrapper inherits it; a nested class/function resets it
        // (its members' spans are computed from their own wrappers). Cleared
        // once a space actually opens so an inner declaration doesn't reuse an
        // outer member's start.
        let member_decl_start = if is_member_body_wrapper(ri) {
            let span = ctx_span(ctx, self.line_index, self.source_len);
            Some((span.start_byte, span.start_line))
        } else if opens_class_like(ri) || opens_function_space(ri) {
            None
        } else {
            hint.member_decl_start
        };

        // Thread the enclosing boolean operator down through `expression`
        // descendants for the parent-relative cognitive boolean-run collapse.
        //
        // - A boolean `expression` (`&&`/`||`) sets the operator for its
        //   operands.
        // - A *transparent* `expression` — one that carries NO operator token
        //   of its own (a bare operand: `a`, or a single sub-expression) — and
        //   a `primary` (`'(' expression ')'`) forward the enclosing operator,
        //   so `a && (b && c)` and `(a && b) && c` collapse into one run.
        // - Any expression that introduces its OWN operator other than
        //   `&&`/`||` — equality (`==`), comparison, ternary (`? :`), index
        //   (`[]`), unary, `instanceof`, a method call, etc. — is a distinct
        //   boolean context and RESETS the run (`None`), so a `&&` nested
        //   inside `(b && c) == d` is not wrongly collapsed with an outer `&&`.
        // - Every other node kind also resets, isolating method-call arguments
        //   and nested statements from the enclosing run.
        // The operator THIS node itself introduces (only a `&&`/`||` expression
        // does), used both to set the operands' run operator and to place a
        // predecessor for the run (its right operand follows its left).
        let this_bool_op = if ri == jp::RULE_EXPRESSION {
            if ctx.has_token(jl::AND) {
                Some(BoolOp::And)
            } else if ctx.has_token(jl::OR) {
                Some(BoolOp::Or)
            } else {
                None
            }
        } else {
            None
        };
        let child_bool_op = if ri == jp::RULE_EXPRESSION {
            if this_bool_op.is_some() {
                this_bool_op
            } else if expression_has_operator_token(ctx) {
                None
            } else {
                hint.parent_bool_op
            }
        } else if ri == jp::RULE_PRIMARY {
            hint.parent_bool_op
        } else {
            None
        };
        // A classic `for` header (`forControl → forInit → localVariableDeclaration`)
        // must not let its initializer declaration add a second LLOC. Tag ONLY
        // the direct children of `forInit` (the header declaration) — NOT the
        // whole subtree. A sticky, subtree-wide flag would also suppress a real
        // local declaration nested inside a lambda or anonymous-class body that
        // lives in the initializer, e.g.
        // `for (Supplier<Integer> s = () -> { int x = 0; return x; }; ; ) {}` —
        // the lambda body's `int x = 0;` is genuine code and must count.
        // `localVariableDeclaration` is a direct child of `forInit`, so a
        // non-sticky flag reaches exactly the header declaration and stops one
        // level down (mirrors the per-child `anon_body_child` tagging below).
        let in_for_init = ri == jp::RULE_FOR_INIT;

        // A terminal directly under `identifier`/`typeIdentifier` is a name →
        // Halstead operand (covers contextual keywords used as identifiers).
        let in_identifier = matches!(ri, jp::RULE_IDENTIFIER | jp::RULE_TYPE_IDENTIFIER);

        // Track whether we're inside an anonymous class body that opens no
        // metric space of its own — a constant-specific enum-constant body
        // (`enum E { A { … } }`) or an anonymous class expression
        // (`new Runnable() { … }`, via `classCreatorRest → classBody`). Their
        // members belong to the anonymous subclass, not the lexically-enclosing
        // class/enum, so they must not seed NPA/NPM or roll into its WMC.
        //
        // The anon body is ONLY the `classBody` child of `classCreatorRest` /
        // `enumConstant` — NOT their sibling `arguments`/`identifier`. Tagging
        // the whole node would wrongly suppress a lambda passed as a plain
        // constructor argument (`new Foo(() -> …)`), resetting its cognitive
        // depth. So the trigger is applied per-child below (only the
        // `classBody`); here we just propagate the inbound flag, CLEARING it
        // once a real nested class-like or a function space opens (the latter
        // so a lambda inside the anon body's direct method still inherits the
        // method's depth). The method's own NPA/NPM/WMC suppression is captured
        // from its inbound hint before its children are visited.
        let anon_body_child = if matches!(ri, jp::RULE_ENUM_CONSTANT | jp::RULE_CLASS_CREATOR_REST)
        {
            child_index_of_rule(ctx.children(), jp::RULE_CLASS_BODY)
        } else {
            None
        };
        let in_anon_body = if opens_class_like(ri) || opens_function_space(ri) {
            false
        } else {
            hint.in_anon_body
        };

        // Once inside an `annotation` OR an annotation element's `defaultValue`,
        // stay inside for the whole subtree so annotation metadata does not
        // record executable complexity (ABC assignments/conditions, cyclomatic
        // decisions, cognitive nesting). Two entry points: a use-site annotation
        // (`@Ann(value = 1)`, `RULE_ANNOTATION`) and an element default
        // (`@interface A { boolean v() default true && false; }`, parsed under
        // `annotationMethodRest → defaultValue → elementValue → expression`, NOT
        // under `RULE_ANNOTATION`).
        let in_annotation =
            hint.in_annotation || ri == jp::RULE_ANNOTATION || ri == jp::RULE_DEFAULT_VALUE;

        // Thread the record's component count down so a compact constructor
        // (which has no `formalParameters`) can report the components as its
        // NArgs. Set on entering `recordDeclaration`; a nested type resets it
        // (a nested class/record's members are not this record's components).
        let record_component_count = if ri == jp::RULE_RECORD_DECLARATION {
            Some(count_record_components(ctx))
        } else if opens_class_like(ri) {
            None
        } else {
            hint.record_component_count
        };

        // Thread the enclosing record's visibility down so a modifier-less
        // compact canonical constructor can inherit the record's access level
        // (Java rule). The record's visibility is what was resolved for the
        // record declaration itself as a member (`member_is_public`, set by its
        // enclosing class/type wrapper); a top-level record has no such wrapper,
        // so fall back to modifiers on the record declaration. A nested type
        // resets it (its own members are not this record's).
        let enclosing_record_public = if ri == jp::RULE_RECORD_DECLARATION {
            Some(
                hint.member_is_public.unwrap_or(false)
                    || visibility_from_modifiers(ctx) == Some(true),
            )
        } else if opens_class_like(ri) {
            None
        } else {
            hint.enclosing_record_public
        };

        // When this wrapper opened the method OR type space itself (to capture
        // own-line modifiers), tell the inner declaration to skip its own open.
        // The flag flows through the transparent `memberDeclaration` /
        // generic-method / `annotationTypeElementRest` wrappers to the
        // declaration node, which consumes it; a real space open clears it so a
        // nested declaration inside the body still opens normally.
        let opened_at_wrapper = (matches!(
            ri,
            jp::RULE_CLASS_BODY_DECLARATION
                | jp::RULE_INTERFACE_BODY_DECLARATION
                | jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION
        ) && wrapper_inner_method(ctx).is_some())
            || (is_type_wrapper(ri) && wrapper_inner_type(ctx).is_some());
        let space_opened_by_wrapper = if opens_function_space(ri) || opens_class_like(ri) {
            false
        } else {
            opened_at_wrapper || hint.space_opened_by_wrapper
        };

        for (idx, child) in ctx.children().enumerate() {
            let mut child_hint = ChildHint::default();
            if Some(idx) == else_body_idx || propagate_else {
                child_hint.is_else_branch = true;
            }
            child_hint.space_opened_by_wrapper = space_opened_by_wrapper;
            child_hint.in_class_member = propagate_member;
            child_hint.member_container = member_container;
            child_hint.member_is_public = member_is_public;
            child_hint.parent_bool_op = child_bool_op;
            child_hint.in_for_init = in_for_init;
            child_hint.in_identifier = in_identifier;
            // The anon-body `classBody` child gets the suppression; its sibling
            // `arguments`/`identifier` (a plain constructor call, an enum
            // constant's args) do not.
            let is_anon_body_child = Some(idx) == anon_body_child;
            child_hint.in_anon_body = in_anon_body || is_anon_body_child;
            child_hint.record_component_count = record_component_count;
            child_hint.enclosing_record_public = enclosing_record_public;
            child_hint.member_decl_start = member_decl_start;
            child_hint.in_annotation = in_annotation;
            // An anonymous class body (`new X() { … }`) is a fresh class scope
            // but opens no metric space, so — unlike a named class, which
            // resets via `enter_class_cognitive` in `maybe_open_space` — its
            // class-body-level code (initializer blocks, field initializers)
            // would otherwise inherit the enclosing statement's cognitive
            // nesting. Reset the cognitive context for that subtree and restore
            // it afterward (the sibling `arguments` were already visited, and
            // this scopes the reset to just the anon body).
            if is_anon_body_child {
                let saved = self.cognitive;
                self.cognitive = CognitiveContext::default();
                self.visit(child, child_hint);
                self.cognitive = saved;
            } else {
                self.visit(child, child_hint);
            }
        }
    }

    /// Compute the `(in_class_member, container, is_public)` hint for this
    /// rule's children. Members reach their declaration through transparent
    /// wrapper layers; the container comes from the enclosing space kind and
    /// the visibility is resolved from the body-declaration's `modifier`s
    /// (siblings of the member declaration).
    fn member_propagation(
        &self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        hint: ChildHint,
        container_before_open: Option<ContainerKind>,
    ) -> (bool, Option<ContainerKind>, Option<bool>) {
        // A body-declaration reached *inside* an anonymous class / enum-constant
        // body belongs to that anonymous subclass, which opens no space here —
        // so it must NOT seed a member position on the lexically-enclosing space
        // (NPA/NPM), mirroring the WMC suppression at close time. But the
        // wrapper's resolved *visibility* must still be threaded (as
        // `member_is_public`) so a NESTED type inside the anon body can inherit
        // it — e.g. `new Object(){ public record R(int x) { R {} } }` needs the
        // record's `public` for its modifier-less compact ctor. So keep
        // `propagate_member = false` and `container = None` (no attribution to
        // the anon owner) but resolve visibility from the wrapper's modifiers.
        if hint.in_anon_body {
            // Resolve the wrapper's own visibility from its modifiers; a
            // transparent inner wrapper (`memberDeclaration`, generic wrappers)
            // has no modifiers of its own, so it must INHERIT the visibility
            // already threaded down rather than reset it to `None` — otherwise
            // it clobbers the value before it reaches a nested type declaration.
            let public = if is_member_body_wrapper(ri) {
                visibility_from_modifiers(ctx)
            } else {
                hint.member_is_public
            };
            return (false, None, public);
        }
        match ri {
            // The body-declaration wrappers open a member position; the
            // container is the class-like currently on the kinds stack, and
            // the visibility is resolved from this wrapper's own `modifier`s.
            //
            // `recordBody` is included because a *compact* constructor is a
            // direct `compactConstructorDeclaration` child of `recordBody`
            // (not wrapped in `classBodyDeclaration`), so without seeding the
            // member position here it would be visited with
            // `in_class_member == false` and dropped from NPM. Its own
            // `modifier`s live on the declaration, so visibility is resolved
            // in `classify_class_member` rather than from this wrapper.
            jp::RULE_CLASS_BODY_DECLARATION
            | jp::RULE_INTERFACE_BODY_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION
            | jp::RULE_ENUM_BODY_DECLARATIONS
            | jp::RULE_RECORD_BODY => {
                // Use the container captured BEFORE this node's `maybe_open_space`
                // — a body-declaration wrapper may have just opened a nested type
                // space (round 29), so `self.enclosing_container()` here would
                // wrongly report that type. The real enclosing scope determines
                // the member's default visibility (interface members are public).
                let container = container_before_open;
                // Java visibility semantics (not Kotlin's): a class member with
                // no access modifier is *package-private*, which is NOT public,
                // so a class member's default is `false` — only an explicit
                // `public` modifier makes it count toward NPA/NPM. Interface
                // and annotation members are implicitly public, so their
                // default is `true`.
                let default_public = matches!(container, Some(ContainerKind::Interface));
                let public = visibility_from_modifiers(ctx).unwrap_or(default_public);
                (true, container, Some(public))
            }
            // A top-level / local type's access modifiers live on the
            // `typeDeclaration` / `localTypeDeclaration` wrapper (a sibling of
            // the declaration, which has no parent pointer). Thread that
            // visibility down as `member_is_public` so a record declaration can
            // inherit it for its modifier-less compact canonical constructor —
            // WITHOUT marking the type itself as a class member (a top-level
            // type is not counted in NPA/NPM), so `propagate_member` stays
            // false. A top-level type with no modifier is package-private.
            jp::RULE_TYPE_DECLARATION | jp::RULE_LOCAL_TYPE_DECLARATION => {
                let public = visibility_from_modifiers(ctx).unwrap_or(false);
                (false, None, Some(public))
            }
            // Transparent member wrappers keep the inbound member position.
            // The generic wrappers (`<T> …`) and the interface-method wrapper
            // must be transparent too: they nest the real declaration
            // (`methodDeclaration` / `constructorDeclaration` /
            // `interfaceCommonBodyDeclaration`) one level deeper, and without
            // forwarding the hint their inner declaration is visited with
            // `in_class_member = false`, so NPM/NPA silently drop generic and
            // interface members.
            jp::RULE_MEMBER_DECLARATION
            | jp::RULE_INTERFACE_MEMBER_DECLARATION
            | jp::RULE_GENERIC_METHOD_DECLARATION
            | jp::RULE_GENERIC_CONSTRUCTOR_DECLARATION
            | jp::RULE_GENERIC_INTERFACE_METHOD_DECLARATION
            | jp::RULE_INTERFACE_METHOD_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_ELEMENT_REST
            | jp::RULE_ANNOTATION_METHOD_OR_CONSTANT_REST => (
                hint.in_class_member,
                hint.member_container,
                hint.member_is_public,
            ),
            _ => (false, None, None),
        }
    }

    /// The `ContainerKind` of the class-like space currently on top of the
    /// kinds stack (for member NPA/NPM routing), or `None` if the top is not
    /// a class-like scope.
    fn enclosing_container(&self) -> Option<ContainerKind> {
        match self.kinds.last() {
            Some(SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum) => {
                Some(ContainerKind::Class)
            }
            Some(SpaceKind::Interface | SpaceKind::Trait) => Some(ContainerKind::Interface),
            _ => None,
        }
    }

    /// Open a `Function` space for a method/constructor. `span_ctx` supplies
    /// the span (the `classBodyDeclaration` wrapper when opening at the wrapper,
    /// so the span covers own-line modifiers/annotations; otherwise the
    /// declaration itself); `method_ctx` supplies the name and NArgs. When
    /// opening at the declaration node (`span_ctx == method_ctx`) the own-line
    /// modifiers of a *different* wrapper (interface/annotation) are pulled in
    /// via PLOC-range adoption from the parent; when opening at the wrapper the
    /// modifiers are walked inside this space directly, so no adoption is
    /// needed.
    fn open_method_space(
        &mut self,
        span_ctx: RuleNodeView<'_>,
        method_ctx: RuleNodeView<'_>,
        hint: ChildHint,
    ) {
        let name = method_name(method_ctx);
        // Node identity: the arena addresses every node by a `NodeId`, so
        // "different node" is an id comparison, not pointer equality.
        let opened_at_wrapper = span_ctx.node().id() != method_ctx.node().id();
        // When opening at the wrapper, NPM must be recorded into the enclosing
        // class BEFORE the method space is pushed (member classification
        // normally runs at the inner declaration, but that node now sits inside
        // this method space and would misroute NPM into the method). The inner
        // declaration skips its own classification via `space_opened_by_wrapper`.
        // A method in an anonymous-class body belongs to the anon subclass (no
        // space of its own) and must NOT count toward the enclosing container's
        // NPM — mirrors the `in_anon_body` suppression in `member_propagation`.
        if opened_at_wrapper
            && !hint.in_anon_body
            && let Some(container) = self.enclosing_container()
        {
            let default_public = matches!(container, ContainerKind::Interface);
            let public = visibility_from_modifiers(span_ctx).unwrap_or(default_public);
            self.current().npm.record_method(container, public);
        }
        // Widen the declaration-node span up to its body-declaration wrapper so
        // own-line modifiers belong to the method. Unused when opening at the
        // wrapper (the span already starts at the wrapper).
        let widened = if opened_at_wrapper {
            None
        } else {
            hint.member_decl_start
        };
        let mut state = self.new_space_state_widened(span_ctx, widened);
        // When opening at the declaration node, the modifier/annotation rows
        // were already visited (PLOC-counted) on the enclosing class before
        // this space is pushed, so adopt those rows into the method. When
        // opening at the wrapper, the modifiers are walked *inside* this space,
        // so they are counted directly (no adoption).
        if let Some((_, wrapper_start_line)) = widened {
            let method_start_line = ctx_span(span_ctx, self.line_index, self.source_len).start_line;
            if wrapper_start_line < method_start_line {
                let parent_loc = self.current().loc.clone();
                state.loc.adopt_code_lines_in_range(
                    &parent_loc,
                    wrapper_start_line.saturating_sub(1),
                    method_start_line.saturating_sub(1),
                );
            }
        }
        state.nom.record_function();
        // A compact record constructor has no `formalParameters` — its
        // parameter list *is* the record's components, so its NArgs is the
        // enclosing record's component count (threaded down via `ChildHint`).
        // Every other method shape counts its own `formalParameters`.
        let nargs = if method_ctx.rule_index() == jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION {
            hint.record_component_count.unwrap_or(0)
        } else {
            count_formal_params(method_ctx)
        };
        state.nargs.record_function_args(nargs);
        self.push_space_widened(
            SpaceKind::Function,
            name,
            span_ctx,
            state,
            hint.in_anon_body,
            widened,
        );
        self.enter_function_cognitive(hint.in_anon_body);
    }

    /// Open a class-like (`Class`/`Enum`/`Interface`) space for `type_ctx`.
    /// `span_ctx` supplies the span — the wrapper (`typeDeclaration` / member
    /// body-declaration) when opening at the wrapper, so own-line
    /// modifiers/annotations are covered; otherwise the declaration itself
    /// (with PLOC-range adoption for own-line modifiers on a different wrapper).
    /// Mirrors `open_method_space` but for class-like scopes: nested types are
    /// not member-classified into NPA/NPM, so there is no member-routing to
    /// relocate.
    fn open_type_space(&mut self, span_ctx: RuleNodeView<'_>, type_ctx: RuleNodeView<'_>) {
        let name = type_name(type_ctx);
        let ri = type_ctx.rule_index();
        // Node identity is a `NodeId` comparison in the arena model.
        let opened_at_wrapper = span_ctx.node().id() != type_ctx.node().id();
        let widened = if opened_at_wrapper {
            None
        } else {
            // (Only used by the self-open path; kept for symmetry — a class's
            // own-line modifiers are already handled by opening at the wrapper.)
            None
        };
        let mut state = self.new_space_state_widened(span_ctx, widened);
        state.npa.record_class_like();
        state.npm.record_class_like();
        let kind = if matches!(ri, jp::RULE_ENUM_DECLARATION) {
            state.wmc.record_class_like();
            SpaceKind::Enum
        } else if matches!(
            ri,
            jp::RULE_INTERFACE_DECLARATION | jp::RULE_ANNOTATION_TYPE_DECLARATION
        ) {
            // Interfaces/annotations do not carry WMC (their methods are not
            // weighted); match the original arm which omits `record_class_like`
            // on WMC.
            SpaceKind::Interface
        } else {
            state.wmc.record_class_like();
            // `record R(...)` component parameters are class attributes.
            record_record_components(type_ctx, &mut state);
            SpaceKind::Class
        };
        self.push_space_widened(kind, name, span_ctx, state, false, None);
        self.enter_class_cognitive();
    }

    /// Open a metric space for space-introducing rules. Returns whether a
    /// space was pushed.
    fn maybe_open_space(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: ChildHint) -> bool {
        match ri {
            // One function space per method shape. Interface methods reach
            // their name/params/body via `interfaceCommonBodyDeclaration`
            // (wrapped by `interfaceMethodDeclaration` /
            // `genericInterfaceMethodDeclaration`), so the space is opened
            // there — opening at the wrapper too would double-count. A function
            // opened inside an enum-constant body must not roll into the enum's
            // WMC (it belongs to the constant's anonymous subclass).
            // A `classBodyDeclaration` wrapping a plain method/constructor opens
            // the function space HERE (not at the inner declaration) so the
            // wrapper's own-line modifiers/annotations — siblings of the
            // declaration, visited before it — are walked *inside* the method
            // space and count toward its LOC/Halstead/span. The inner
            // declaration then skips its own open (`space_opened_by_wrapper`).
            // Only plain method/constructor members route this way; fields,
            // nested types, and compact/interface/annotation members keep their
            // existing open sites.
            jp::RULE_CLASS_BODY_DECLARATION
            | jp::RULE_INTERFACE_BODY_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION
                if wrapper_inner_method(ctx).is_some() =>
            {
                let method = wrapper_inner_method(ctx).expect("guarded by match arm");
                self.open_method_space(ctx, method, hint);
                true
            }
            jp::RULE_METHOD_DECLARATION
            | jp::RULE_CONSTRUCTOR_DECLARATION
            | jp::RULE_INTERFACE_COMMON_BODY_DECLARATION
            | jp::RULE_ANNOTATION_METHOD_REST
                if hint.space_opened_by_wrapper =>
            {
                // The wrapper already opened this method's space; do not open a
                // second one. (Its children are still visited into that space.)
                false
            }
            jp::RULE_METHOD_DECLARATION
            | jp::RULE_CONSTRUCTOR_DECLARATION
            | jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION
            | jp::RULE_INTERFACE_COMMON_BODY_DECLARATION
            | jp::RULE_ANNOTATION_METHOD_REST => {
                // Opened at the declaration node itself (compact ctor, interface
                // method, annotation element, or a method not reached through a
                // `classBodyDeclaration` wrapper — e.g. inside an anon body).
                // Its own-line modifiers, if any, are covered by the PLOC-range
                // adoption inside `open_method_space`.
                self.open_method_space(ctx, ctx, hint);
                true
            }
            jp::RULE_LAMBDA_EXPRESSION => {
                let mut state = self.new_space_state(ctx);
                state.nom.record_closure();
                state.nargs.record_closure_args(count_lambda_args(ctx));
                // A lambda is a `Closure`, not a `Function`: NOM/NArgs already
                // record it as a closure, and its cyclomatic must NOT roll into
                // the enclosing class's WMC (WMC weights *methods*). A lambda in
                // a field initializer would otherwise inflate the class's WMC.
                self.push_space(SpaceKind::Closure, None, ctx, state, hint.in_anon_body);
                self.enter_function_cognitive(hint.in_anon_body);
                true
            }
            // A type wrapper (`typeDeclaration`/`localTypeDeclaration` or a
            // member body-declaration) opens the class-like space HERE so its
            // own-line modifiers/annotations (`@Deprecated\npublic class C {}`,
            // `public static class Inner {}`) — siblings of the declaration,
            // visited before it — are walked *inside* the type space and count
            // toward its LOC/Halstead/span. The inner declaration then skips its
            // own open via `space_opened_by_wrapper`.
            _ if is_type_wrapper(ri) && wrapper_inner_type(ctx).is_some() => {
                let type_ctx = wrapper_inner_type(ctx).expect("guarded by match arm");
                self.open_type_space(ctx, type_ctx);
                true
            }
            jp::RULE_CLASS_DECLARATION
            | jp::RULE_RECORD_DECLARATION
            | jp::RULE_ENUM_DECLARATION
            | jp::RULE_INTERFACE_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_DECLARATION
                if hint.space_opened_by_wrapper =>
            {
                // The wrapper already opened this type's space; do not open a
                // second one. (Its children are still visited into that space.)
                false
            }
            jp::RULE_CLASS_DECLARATION
            | jp::RULE_RECORD_DECLARATION
            | jp::RULE_ENUM_DECLARATION
            | jp::RULE_INTERFACE_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_DECLARATION => {
                // Opened at the declaration node itself (a type not reached
                // through a wrapper — e.g. inside an anonymous-class body, which
                // opens no space and whose members are visited directly).
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
    /// (byte + line) upward to `widened_start`. A method/constructor uses this
    /// to cover own-line modifiers/annotations that live on its
    /// `classBodyDeclaration` wrapper (siblings of the declaration, so *before*
    /// the declaration's own `ctx_span` start).
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
    /// modifiers/annotations.
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

    /// Reset the cognitive context when opening a class-like space. A class
    /// body is a fresh scope: code that runs *directly* in it (instance/static
    /// initializer blocks, field initializers) must not inherit the enclosing
    /// method's or statement's nesting. Methods do this via
    /// `enter_function_cognitive`, but class-body-level code opens no function
    /// space, so the class-open must reset it. `visit_rule`'s `saved_cognitive`
    /// restore unwinds it when the class-like node's subtree is done.
    fn enter_class_cognitive(&mut self) {
        self.cognitive = CognitiveContext::default();
    }

    fn enter_function_cognitive(&mut self, in_anon_body: bool) {
        // Depth is inherited only from an *enclosing function/closure within
        // the same class scope* — a lambda or nested function nested directly
        // in another function's body. A class scope resets the baseline: a
        // method in a nested class is fresh, so its cognitive nesting starts at
        // 0, not inheriting the enclosing method's depth. Two kinds of class
        // scope must be honored:
        //   - a *named* local/anonymous class pushes a `Class` SpaceKind
        //     (`void outer(){ class L { void inner(){…} } }`), caught by the
        //     `take_while` boundary below;
        //   - an *anonymous* class expression (`new Runnable(){ void run(){…} }`)
        //     opens NO space — it's tracked only by `in_anon_body` — so the
        //     ancestor scan would otherwise walk past it to `outer`. When we're
        //     directly in such a body, do not inherit depth at all.
        let nested_inside_function = !in_anon_body
            && self
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
            // Roll a closing function's cyclomatic into the parent's WMC —
            // unless it's a function from an enum-constant body, which belongs
            // to that constant's anonymous subclass (no space of its own) and
            // must not inflate the enclosing enum's WMC. Java WMC is *per
            // class* — an interface's methods (`default`/`static`/abstract) are
            // NOT weighted, so only roll into a class/enum parent. A method
            // whose parent is an interface contributes nothing to WMC.
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
    fn classify_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: ChildHint) {
        if ri == jp::RULE_STATEMENT {
            self.classify_statement(ctx, hint);
        }
        if ri == jp::RULE_SWITCH_LABEL || ri == jp::RULE_SWITCH_LABELED_RULE {
            // Each `case` label is a decision (cyclomatic) and a condition
            // (ABC). `default` (no CASE token) is neither. The `switch`
            // statement/expression already opened the cognitive nesting level,
            // so a `case` adds only the flat structural cost, not another
            // nesting.
            if ctx.has_token(jl::CASE) {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
            }
        }
        // A pattern-switch guard (`case String s when expr -> …`, grammar
        // `guard: 'when' expression`) is a distinct boolean test — like an extra
        // `if` on the case — so it records one ABC condition of its own (any
        // operators *inside* the guard expression still count on top, via
        // `classify_expression`). It is its own rule, so `default` and unguarded
        // cases are unaffected. Cyclomatic/cognitive are unchanged: the `case`
        // already carries the decision and the `switch` the nesting.
        if ri == jp::RULE_GUARD {
            self.current().abc.record_condition();
        }
        // A `switch` *expression* (Java 14+) owns its `SWITCH` token in the
        // separate `switchExpression` rule — the statement-form handler in
        // `classify_statement` (keyed on `statement`'s direct `SWITCH` token)
        // can never see it. Give it the same cognitive nesting increment so a
        // switch expression and the equivalent switch statement score
        // identically, and so structures nested in its arms see the raised
        // nesting level (the `saved_cognitive` restore in `visit_rule` unwinds
        // it afterward).
        if ri == jp::RULE_SWITCH_EXPRESSION {
            let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
            self.current().cognitive.increase_nesting(eff);
            self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
        }
        // An enum constant (`enum E { A, B }`) is a public static final field
        // of the enum → a public class attribute (NPA). Constants live under
        // `enumConstants` (before the `;`), not `enumBodyDeclarations`, so they
        // never reach the `classify_class_member` member-position path; count
        // them directly here. The enclosing space must be the enum itself (not
        // a nested anonymous body, which owns no space).
        if ri == jp::RULE_ENUM_CONSTANT
            && !hint.in_anon_body
            && matches!(self.kinds.last(), Some(SpaceKind::Enum))
        {
            self.current()
                .npa
                .record_attribute(ContainerKind::Class, true);
        }
        // Annotation values (`@Ann(value = true && false)`, `@Ann(x = c ? 1 : 2)`)
        // are compile-time metadata, not executable code, so a composed constant
        // in them must NOT record cyclomatic decisions, cognitive nesting, or
        // ABC conditions/branches/assignments. `in_annotation` covers the whole
        // annotation subtree; guarding here generalizes the round-16 fix (which
        // only suppressed ABC *assignments*) to all executable-complexity
        // accounting. LOC/Halstead still count — the tokens physically exist.
        if !hint.in_annotation {
            self.classify_expression(ctx, ri, hint);
            self.classify_abc_rule(ctx, ri, hint);
        }
        self.classify_loc_rule(ctx, ri, hint);
    }

    /// Classify a `statement` context by its leading keyword token.
    fn classify_statement(&mut self, ctx: RuleNodeView<'_>, hint: ChildHint) {
        let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;

        if ctx.has_token(jl::IF) {
            // Cyclomatic + ABC always; cognitive nesting unless this is an
            // `else if` (flat +1 emitted when the ELSE token is visited).
            self.current().cyclomatic.record_decision();
            self.current().abc.record_condition();
            if !hint.is_else_branch {
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
            }
            // `else` adds a flat +1 (covers `else if`).
            if ctx.has_token(jl::ELSE) {
                self.current().cognitive.increment_by_one();
            }
        } else if ctx.has_token(jl::FOR) || ctx.has_token(jl::WHILE) || ctx.has_token(jl::DO) {
            self.current().cyclomatic.record_decision();
            self.current().abc.record_condition();
            self.current().cognitive.increase_nesting(eff);
            self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
        } else if ctx.has_token(jl::SWITCH) {
            // `switch` itself adds cognitive nesting but not cyclomatic — the
            // individual `case` labels carry the cyclomatic/ABC decisions.
            self.current().cognitive.increase_nesting(eff);
            self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
        } else if ctx.has_token(jl::RETURN) || ctx.has_token(jl::THROW) {
            self.current().nexit.record_exit();
        } else if (ctx.has_token(jl::BREAK) || ctx.has_token(jl::CONTINUE))
            && ctx.child_rule(jp::RULE_IDENTIFIER).is_some()
        {
            // A labeled break/continue is goto-like: flat +1 (cognitive).
            self.current().cognitive.increment_by_one();
        }
    }

    /// Classify operator tokens carried directly by an `expression` context:
    /// short-circuit `&&`/`||` (cyclomatic + cognitive + ABC), the ternary `?`
    /// (cyclomatic + cognitive + ABC), comparison/equality (ABC condition),
    /// and `instanceof` (ABC condition).
    fn classify_expression(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: ChildHint) {
        if ri == jp::RULE_CATCH_CLAUSE {
            // `catch` is cognitive-only (matches SonarJava): nesting increment
            // + an ABC condition, but no cyclomatic decision.
            let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
            self.current().cognitive.increase_nesting(eff);
            self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
            self.current().abc.record_condition();
            return;
        }
        if ri != jp::RULE_EXPRESSION {
            return;
        }
        // Short-circuit `&&`/`||`: a cyclomatic decision and an ABC condition
        // per operator (both independent of the cognitive run-collapse), plus
        // the cognitive boolean-sequence cost.
        let this_op = if ctx.has_token(jl::AND) {
            Some(BoolOp::And)
        } else if ctx.has_token(jl::OR) {
            Some(BoolOp::Or)
        } else {
            None
        };
        if let Some(_op) = this_op {
            self.current().cyclomatic.record_decision();
            self.current().abc.record_condition();
            // Cognitive: SonarSource counts +1 per *sequence of like logical
            // operators*, computed by flattening the boolean expression in
            // SOURCE ORDER and adding +1 whenever the operator kind changes
            // (SonarJava `CognitiveComplexityVisitor.flattenLogicalExpression`;
            // SonarKotlin `CognitiveComplexity.flattenOperators`). Parentheses
            // are transparent to the flattening, and — critically — a `!`
            // negation is NOT special: it is just an operand where flattening
            // stops, so it never breaks a run (`a && !b && c` is one `&&` run).
            //
            // Only the ROOT of a logical-operator tree does the counting: a
            // `&&`/`||` node whose enclosing boolean operator is `None`
            // (`hint.parent_bool_op` — threaded through transparent parens).
            // A nested `&&`/`||` reached as a logical operand is consumed by its
            // root's flatten and must not double-count. So `a && b || c && d`
            // (root `||`, flattened `&& || &&`) scores 3, and `a && (b || c) &&
            // d` (flattened `&& || &&` after skipping parens) also scores 3 —
            // the parenthesized `||` interrupts the `&&` sequence.
            if hint.parent_bool_op.is_none() {
                let mut ops = Vec::new();
                flatten_logical_operators(ctx, &mut ops);
                let mut prev: Option<BoolOp> = None;
                let mut increments = 0u32;
                for op in ops {
                    if prev != Some(op) {
                        increments += 1;
                    }
                    prev = Some(op);
                }
                if increments > 0 {
                    self.current().cognitive.record_increment(increments);
                }
            }
        }
        // Ternary `? :` — a decision, an ABC condition, and a cognitive nesting
        // structure (SonarJava scores it like an `if`). Bump the walker nesting
        // so a structure nested in an operand (notably a nested ternary) is
        // scored one level deeper; `visit_rule`'s `saved_cognitive` restore
        // unwinds it after the operands are walked.
        if ctx.has_token(jl::QUESTION) && ctx.has_token(jl::COLON) {
            let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
            self.current().cyclomatic.record_decision();
            self.current().cognitive.increase_nesting(eff);
            self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
            self.current().abc.record_condition();
        }
        // Comparison / equality / instanceof → ABC conditions only. A bit-shift
        // (`<<`, `>>`, `>>>`) is NOT a condition — but the grammar spells it
        // with multiple bare `LT`/`GT` terminals (there is no `<<` token), so
        // `has_token(LT)`/`has_token(GT)` can't tell a shift from a relational
        // `<`/`>`. Distinguish by count: a *relational* operator contributes
        // exactly one `LT` (or one `GT`); a shift contributes two-or-three.
        let (lt, gt) = count_angle_tokens(ctx);
        if lt == 1
            || gt == 1
            || ctx.has_token(jl::EQUAL)
            || ctx.has_token(jl::NOTEQUAL)
            || ctx.has_token(jl::LE)
            || ctx.has_token(jl::GE)
            || ctx.has_token(jl::INSTANCEOF)
        {
            self.current().abc.record_condition();
        }
    }

    fn classify_abc_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: ChildHint) {
        match ri {
            // A method/constructor call, or object creation, is a branch.
            jp::RULE_METHOD_CALL => self.current().abc.record_branch(),
            jp::RULE_CREATOR | jp::RULE_INNER_CREATOR => self.current().abc.record_branch(),
            // Calls that don't route through `methodCall`. The Java grammar
            // reaches several call forms via suffix rules; count each call
            // exactly once at the innermost call-bearing node to avoid the
            // double-counting that would arise from also counting the
            // enclosing `explicitGenericInvocation` wrapper:
            //   - `superSuffix` — a qualified super call (`I.super.m()`) and
            //     the `SUPER superSuffix` form of an explicit generic
            //     invocation (`<T>super.m()`).
            //   - `explicitGenericInvocationSuffix` in its `identifier
            //     arguments` form (a direct `arguments` child) — a qualified
            //     (`C.this.<String>m()`) or unqualified (`<String>m()`)
            //     explicit-type-argument call. Its `SUPER superSuffix` form is
            //     counted via the nested `superSuffix` above, so it's excluded
            //     here by the direct-`arguments` guard.
            // `superSuffix` is also qualified super *field* access
            // (`Outer.super.field`) — its grammar alternative
            // `'.' typeArguments? identifier arguments?` makes `arguments`
            // optional. Only count it as a branch when it actually carries an
            // `arguments` child (a call), not a bare field read.
            jp::RULE_SUPER_SUFFIX if ctx.child_rule(jp::RULE_ARGUMENTS).is_some() => {
                self.current().abc.record_branch();
            }
            jp::RULE_EXPLICIT_GENERIC_INVOCATION_SUFFIX
                if ctx.child_rule(jp::RULE_ARGUMENTS).is_some() =>
            {
                self.current().abc.record_branch();
            }
            // A generic explicit constructor invocation (`<String>this(arg)`)
            // routes through `primary: nonWildcardTypeArguments THIS arguments`
            // — not `methodCall` (which handles the plain `this(…)`/`super(…)`
            // forms). Count it when the `primary` carries a `THIS` token AND a
            // direct `arguments` child; a bare `this` / `this.field` (no
            // `arguments`) is not a call.
            jp::RULE_PRIMARY
                if ctx.has_token(jl::THIS) && ctx.child_rule(jp::RULE_ARGUMENTS).is_some() =>
            {
                self.current().abc.record_branch();
            }
            // An `expression` carrying an assignment operator is an assignment.
            // Compound assigns (`+=`, `-=`, …) and the increment/decrement
            // operators (`++`, `--`) count too (Fitzpatrick's ABC lists both
            // under A). `has_assignment_op` covers all of them. Suppressed
            // inside an annotation: a named element value (`@Ann(value = 1)`)
            // is compile-time metadata, not an executable assignment — and the
            // grammar's `IsNotIdentifierAssign` predicate that would keep it out
            // of the assignment-expression path is dropped by the Rust
            // generator.
            jp::RULE_EXPRESSION if has_assignment_op(ctx) && !hint.in_annotation => {
                self.current().abc.record_assignment();
            }
            // A local variable / field / record-component declarator with an
            // initializer (`= …`) is an assignment.
            jp::RULE_VARIABLE_DECLARATOR | jp::RULE_CONSTANT_DECLARATOR
                if ctx.has_token(jl::ASSIGN) =>
            {
                self.current().abc.record_assignment();
            }
            // A `var x = expr` local-variable declaration places its `=` as a
            // direct child of `localVariableDeclaration` (no `variableDeclarator`
            // node), and a try-with-resources `T r = expr` places its `=`
            // directly on `resource`. Both are initialized declarations → one
            // assignment. The explicit-type local (`int x = e`) routes its `=`
            // through `variableDeclarator` (handled above), so this arm's
            // `has_token(ASSIGN)` guard fires only for the `var` form and never
            // double-counts. A bare `qualifiedName` resource has no `=`.
            jp::RULE_LOCAL_VARIABLE_DECLARATION | jp::RULE_RESOURCE
                if ctx.has_token(jl::ASSIGN) =>
            {
                self.current().abc.record_assignment();
            }
            _ => {}
        }
    }

    fn classify_loc_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: ChildHint) {
        // A `for` header's initializer declaration is part of the `for`
        // statement's single logical line — not its own LLOC.
        if ri == jp::RULE_LOCAL_VARIABLE_DECLARATION && hint.in_for_init {
            return;
        }
        // An *expression-bodied* lambda (`x -> x + 1`) opens a closure space but
        // its body (`lambdaBody: expression`) contains no statement/declaration,
        // so the closure would report `lloc = 0`. Count the lambda itself as one
        // logical line to match a block-bodied lambda (whose inner statements
        // already count) and method declarations. A block body is skipped here
        // — its statements are counted individually.
        if ri == jp::RULE_LAMBDA_EXPRESSION && lambda_body_is_expression(ctx) {
            self.current().loc.observe_lloc();
            return;
        }
        // LLOC: statement- and declaration-shaped rules. Interface methods
        // (`interfaceCommonBodyDeclaration`) and annotation elements/constants
        // (`annotationMethodRest`/`annotationConstantRest`) are the
        // declaration nodes for their member kinds — a class abstract method
        // counts via `methodDeclaration`, so their interface/annotation
        // equivalents must count too, or interface/annotation APIs
        // under-report LLOC.
        if matches!(
            ri,
            jp::RULE_STATEMENT
                | jp::RULE_LOCAL_VARIABLE_DECLARATION
                | jp::RULE_FIELD_DECLARATION
                | jp::RULE_METHOD_DECLARATION
                | jp::RULE_CONSTRUCTOR_DECLARATION
                | jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION
                | jp::RULE_INTERFACE_COMMON_BODY_DECLARATION
                | jp::RULE_ANNOTATION_METHOD_REST
                | jp::RULE_ANNOTATION_CONSTANT_REST
                | jp::RULE_CONST_DECLARATION
                // An enum constant (`A`, `B` in `enum E { A, B }`) is a
                // declaration — a logical line, like a field.
                | jp::RULE_ENUM_CONSTANT
                | jp::RULE_CLASS_DECLARATION
                | jp::RULE_INTERFACE_DECLARATION
                | jp::RULE_ENUM_DECLARATION
                | jp::RULE_RECORD_DECLARATION
                | jp::RULE_ANNOTATION_TYPE_DECLARATION
                | jp::RULE_IMPORT_DECLARATION
                | jp::RULE_PACKAGE_DECLARATION
                // Java 9+ module descriptors (`module-info.java`): the module
                // declaration and each directive (`requires`/`exports`/…) are
                // logical lines, or a module file reports lloc == 0.
                | jp::RULE_MODULE_DECLARATION
                | jp::RULE_MODULE_DIRECTIVE
        ) {
            // Some `statement` shapes are pure wrappers that are not their own
            // logical line — the statement(s) they contain each count:
            //   - a bare block `{ … }` (the inner statements count),
            //   - an empty statement `;` (no work at all),
            //   - a labeled statement `label: stmt` (the label is an attribute
            //     of the inner statement, which is counted when visited).
            if ri == jp::RULE_STATEMENT
                && (ctx_is_block(ctx) || ctx_is_empty_statement(ctx) || ctx_is_label_wrapper(ctx))
            {
                return;
            }
            self.current().loc.observe_lloc();
        }
    }

    /// NPA / NPM classification for a direct member of an enclosing class or
    /// interface body. `ctx` is the member declaration rule itself; `public`
    /// is the visibility resolved from the body-declaration wrapper's
    /// `modifier`s (threaded down via [`ChildHint`]). `hint` carries the
    /// enclosing record's visibility for the compact-constructor fallback.
    fn classify_class_member(
        &mut self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        container: ContainerKind,
        public: bool,
        hint: ChildHint,
    ) {
        match ri {
            jp::RULE_FIELD_DECLARATION => {
                // A field declaration can declare several variables.
                let count = field_variable_count(ctx).max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, public);
                }
            }
            jp::RULE_CONST_DECLARATION => {
                let count = ctx.child_rules(jp::RULE_CONSTANT_DECLARATOR).count().max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, true);
                }
            }
            // Interface methods are counted at `interfaceCommonBodyDeclaration`
            // only (the single site where the space is also opened), NOT at
            // the `interfaceMethodDeclaration` wrapper — the wrapper is now a
            // transparent hint-forwarder, so counting at both would
            // double-count the non-generic interface method.
            jp::RULE_METHOD_DECLARATION
            | jp::RULE_CONSTRUCTOR_DECLARATION
            | jp::RULE_INTERFACE_COMMON_BODY_DECLARATION => {
                self.current().npm.record_method(container, public);
            }
            // A compact record constructor (`record R(int x) { public R {} }`)
            // is reached directly under `recordBody`, so the threaded `public`
            // comes from the (modifier-less) record body and is always `false`.
            // Its own `modifier`s are its children, so an *explicit* modifier is
            // resolved from `ctx`; a modifier-less compact canonical constructor
            // inherits the RECORD's access level (Java rule), threaded via
            // `enclosing_record_public` — not the record-body default.
            jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION => {
                let is_public = visibility_from_modifiers(ctx)
                    .or(hint.enclosing_record_public)
                    .unwrap_or(public);
                self.current().npm.record_method(container, is_public);
            }
            // An annotation element (`@interface A { String value(); }`) is an
            // implicitly-public interface-like method — `annotationMethodRest`
            // is the declaration reached through the annotation wrappers
            // (`annotationTypeElementRest → annotationMethodOrConstantRest`).
            jp::RULE_ANNOTATION_METHOD_REST => {
                self.current().npm.record_method(container, true);
            }
            // An annotation constant (`int X = 1;` in an `@interface`) is an
            // implicitly-public attribute; `annotationConstantRest` wraps a
            // `variableDeclarators`, so several constants can be declared at
            // once (`int X = 1, Y = 2;`).
            jp::RULE_ANNOTATION_CONSTANT_REST => {
                let count = ctx
                    .child_rule(jp::RULE_VARIABLE_DECLARATORS)
                    .map(|vds| vds.child_rules(jp::RULE_VARIABLE_DECLARATOR).count())
                    .unwrap_or(0)
                    .max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, true);
                }
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------
// Free helpers (top-down tree inspection — no parent pointers).
// --------------------------------------------------------------------

/// Index of the first direct child that is a rule with `rule_index`, if any.
/// Used to tag only the `classBody` child of `classCreatorRest`/`enumConstant`
/// as the anonymous body (its sibling `arguments` is a plain call). Takes the
/// child iterator directly so no `Vec` is allocated.
fn child_index_of_rule<'a>(
    children: impl Iterator<Item = Node<'a>>,
    rule_index: usize,
) -> Option<usize> {
    children.enumerate().find_map(|(idx, c)| {
        c.as_rule()
            .filter(|rule| rule.rule_index() == rule_index)
            .map(|_| idx)
    })
}

/// Rules that open a class-like metric space (see `maybe_open_space`). Used to
/// clear the `in_anon_body` suppression once a real nested class/interface/enum
/// owns the following body — its members belong to that class, not the enum
/// constant whose body lexically encloses it.
fn opens_class_like(ri: usize) -> bool {
    matches!(
        ri,
        jp::RULE_CLASS_DECLARATION
            | jp::RULE_RECORD_DECLARATION
            | jp::RULE_ENUM_DECLARATION
            | jp::RULE_INTERFACE_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_DECLARATION
    )
}

/// Rules that open a function/closure metric space (mirrors the function arms
/// of `maybe_open_space`). Used to clear the `in_anon_body` flag: the anon
/// body's direct method is the boundary, and a lambda nested *inside* that
/// method is enclosed by the method (a function), not the anon body.
fn opens_function_space(ri: usize) -> bool {
    matches!(
        ri,
        jp::RULE_METHOD_DECLARATION
            | jp::RULE_CONSTRUCTOR_DECLARATION
            | jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION
            | jp::RULE_INTERFACE_COMMON_BODY_DECLARATION
            | jp::RULE_ANNOTATION_METHOD_REST
            | jp::RULE_LAMBDA_EXPRESSION
    )
}

/// The body-declaration wrappers whose leading `modifier`s (including
/// annotations) are siblings of the member declaration. Their start line is
/// where the member truly begins, so a method/constructor space widens its
/// span up to it to cover own-line modifiers/annotations.
fn is_member_body_wrapper(ri: usize) -> bool {
    matches!(
        ri,
        jp::RULE_CLASS_BODY_DECLARATION
            | jp::RULE_INTERFACE_BODY_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION
    )
}

/// Flatten a logical-operator (`&&`/`||`) expression tree into its operator
/// sequence in SOURCE ORDER, mirroring SonarJava's
/// `CognitiveComplexityVisitor.flattenLogicalExpression` and SonarKotlin's
/// `CognitiveComplexity.flattenOperators`: recurse into the left operand,
/// emit this node's operator, recurse into the right operand — descending only
/// into logical-binary children and skipping transparent parentheses /
/// pass-through wrappers. A `!` negation, a comparison (`==`, `<`, …), a
/// method call, etc. are NOT logical-binary, so flattening stops there (they
/// are plain operands) — matching SonarSource, where negation never breaks a
/// boolean run. The caller counts +1 whenever the operator kind changes across
/// the resulting sequence. A depth bound guards against pathological nesting.
fn flatten_logical_operators(ctx: RuleNodeView<'_>, out: &mut Vec<BoolOp>) {
    flatten_logical_operators_inner(ctx, out, 0);
}

fn flatten_logical_operators_inner(ctx: RuleNodeView<'_>, out: &mut Vec<BoolOp>, depth: usize) {
    if depth > 64 {
        return;
    }
    // Unwrap a transparent operand — a parenthesized `primary` (`'(' expression
    // ')'`) or a pass-through `expression` with no operator token of its own —
    // to the logical-binary expression it may contain.
    let Some(logical) = unwrap_to_logical(ctx, depth) else {
        return;
    };
    let op = if logical.has_token(jl::AND) {
        BoolOp::And
    } else if logical.has_token(jl::OR) {
        BoolOp::Or
    } else {
        return;
    };
    // `expression op expression` — left operand, this operator, right operand.
    let operands: Vec<RuleNodeView<'_>> = logical.children().filter_map(|c| c.as_rule()).collect();
    if let Some(&left) = operands.first() {
        flatten_logical_operators_inner(left, out, depth + 1);
    }
    out.push(op);
    if let Some(&right) = operands.get(1) {
        flatten_logical_operators_inner(right, out, depth + 1);
    }
}

/// Resolve `ctx` to a logical-binary (`&&`/`||`) `expression`, unwrapping
/// transparent parenthesis (`primary`) and single-child pass-through
/// `expression` wrappers. Returns `None` when `ctx` is not (and does not
/// transparently wrap) a logical-binary expression — i.e. it is a plain
/// operand (identifier, comparison, negation, call, …) where flattening stops.
fn unwrap_to_logical(ctx: RuleNodeView<'_>, depth: usize) -> Option<RuleNodeView<'_>> {
    if depth > 64 {
        return None;
    }
    match ctx.rule_index() {
        jp::RULE_EXPRESSION => {
            if ctx.has_token(jl::AND) || ctx.has_token(jl::OR) {
                return Some(ctx);
            }
            // A transparent expression (a bare operand or single wrapped
            // sub-expression, no operator token of its own) — unwrap and retry.
            if !expression_has_operator_token(ctx) {
                return ctx
                    .children()
                    .filter_map(|c| c.as_rule())
                    .find_map(|rule| unwrap_to_logical(rule, depth + 1));
            }
            None
        }
        // `primary: '(' expression ')'` — unwrap the parenthesized expression.
        jp::RULE_PRIMARY => ctx
            .children()
            .filter_map(|c| c.as_rule())
            .find_map(|rule| unwrap_to_logical(rule, depth + 1)),
        _ => None,
    }
}

/// Whether an `expression` context carries its own operator — i.e. has any
/// direct terminal (token) child. A transparent operand expression (`a`, or a
/// single wrapped sub-expression) has only rule children and no tokens; an
/// operator form (`==`, `<`, ternary `? :`, index `[]`, unary, `instanceof`, a
/// method/creator call, `.` access, …) always has at least one token child.
/// Used by the cognitive boolean-run collapse: only a token-less (transparent)
/// expression forwards the enclosing `&&`/`||`; anything with its own operator
/// starts a fresh boolean context.
fn expression_has_operator_token(ctx: RuleNodeView<'_>) -> bool {
    ctx.children().any(|c| c.as_terminal().is_some())
}

/// Whether a `lambdaExpression`'s body is an expression (`lambdaBody:
/// expression`) rather than a block. An expression body is a single logical
/// line for LLOC; a block body's statements are counted individually.
fn lambda_body_is_expression(ctx: RuleNodeView<'_>) -> bool {
    ctx.child_rule(jp::RULE_LAMBDA_BODY)
        .map(|body| {
            body.child_rule(jp::RULE_EXPRESSION).is_some()
                && body.child_rule(jp::RULE_BLOCK).is_none()
        })
        .unwrap_or(false)
}

/// Whether this `statement` context is an `if` statement (has an `IF` token as
/// a direct child).
fn is_if_statement(ctx: RuleNodeView<'_>, ri: usize) -> bool {
    ri == jp::RULE_STATEMENT && ctx.has_token(jl::IF)
}

/// Whether the `is_else_branch` flag may propagate through this `statement`
/// toward a nested `if` (marking it an `else if`).
///
/// True only for a *transparent wrapper* statement — one that introduces no
/// control-flow construct of its own and isn't a block. That covers a label
/// wrapper (`lbl: stmt`) or a bare-expression statement whose subtree leads to
/// the `if`. It is FALSE for:
///   - a `block` (`else { if … }` is genuinely nested);
///   - an `if` statement itself (its else child is targeted precisely via
///     `else_branch_index`, so the flag must not blanket-tag the then-branch);
///   - any statement carrying its own control-flow keyword
///     (`for`/`while`/`do`/`switch`/`try`/`synchronized`/`return`/`throw`/
///     `break`/`continue`/`yield`/`assert`) — its body is a real nested scope,
///     not an else-if, so e.g. an `if` in `else while (c) if (b) {}` keeps its
///     nesting increment.
fn statement_is_else_transparent(ctx: RuleNodeView<'_>, ri: usize) -> bool {
    ri == jp::RULE_STATEMENT
        && !ctx_is_block(ctx)
        && !ctx.has_token(jl::IF)
        && !ctx.has_token(jl::FOR)
        && !ctx.has_token(jl::WHILE)
        && !ctx.has_token(jl::DO)
        && !ctx.has_token(jl::SWITCH)
        && !ctx.has_token(jl::TRY)
        && !ctx.has_token(jl::SYNCHRONIZED)
        && !ctx.has_token(jl::RETURN)
        && !ctx.has_token(jl::THROW)
        && !ctx.has_token(jl::BREAK)
        && !ctx.has_token(jl::CONTINUE)
        && !ctx.has_token(jl::YIELD)
        && !ctx.has_token(jl::ASSERT)
}

/// Whether this context is a bare block statement (`{ … }`) — a `statement`
/// whose only rule child is a `block`.
fn ctx_is_block(ctx: RuleNodeView<'_>) -> bool {
    let mut rules = ctx.children().filter_map(|c| c.as_rule());
    matches!((rules.next(), rules.next()), (Some(only), None)
        if only.rule_index() == jp::RULE_BLOCK)
}

/// Whether this `statement` is an empty statement (a bare `;`) — its only
/// child is the `SEMI` terminal. Distinguished from `return;`/`break;` (which
/// carry a keyword terminal too) by requiring exactly one child.
fn ctx_is_empty_statement(ctx: RuleNodeView<'_>) -> bool {
    let mut children = ctx.children();
    matches!(
        (children.next(), children.next()),
        (Some(only), None)
            if only.as_terminal().is_some_and(|t| t.symbol().token_type() == jl::SEMI)
    )
}

/// Whether this `statement` is a labeled-statement wrapper
/// (`identifierLabel = identifier ':' statement`) — its rule children are
/// exactly one `identifier` followed by one nested `statement`. The label is
/// an attribute of the inner statement (counted when visited), not its own
/// logical line.
fn ctx_is_label_wrapper(ctx: RuleNodeView<'_>) -> bool {
    let mut rules = ctx
        .children()
        .filter_map(|c| c.as_rule().map(|r| r.rule_index()));
    matches!(
        (rules.next(), rules.next(), rules.next()),
        (Some(jp::RULE_IDENTIFIER), Some(jp::RULE_STATEMENT), None)
    )
}

/// Count the direct `LT` and `GT` terminal children of an `expression`
/// context, returning `(lt_count, gt_count)`. A relational `<`/`>` contributes
/// exactly one; a bit-shift decomposes into two (`<<`, `>>`) or three (`>>>`)
/// — the grammar has no dedicated shift token — so counting distinguishes the
/// two so shifts are not miscounted as ABC conditions.
fn count_angle_tokens(ctx: RuleNodeView<'_>) -> (usize, usize) {
    let mut lt = 0;
    let mut gt = 0;
    for child in ctx.children() {
        if let Some(t) = child.as_terminal() {
            match t.symbol().token_type() {
                jl::LT => lt += 1,
                jl::GT => gt += 1,
                _ => {}
            }
        }
    }
    (lt, gt)
}

/// Index of the `else`-branch `statement` child of an `if` statement, if
/// present. The else body is the `statement` that appears *after* the `ELSE`
/// terminal among the children.
fn else_branch_index<'a>(children: impl Iterator<Item = Node<'a>>) -> Option<usize> {
    let mut seen_else = false;
    for (idx, child) in children.enumerate() {
        if let Some(t) = child.as_terminal() {
            if t.symbol().token_type() == jl::ELSE {
                seen_else = true;
            }
        } else if let Some(rule) = child.as_rule()
            && seen_else
            && rule.rule_index() == jp::RULE_STATEMENT
        {
            return Some(idx);
        }
    }
    None
}

/// The declared name of a type: its first `identifier`/`typeIdentifier`
/// child's covered text.
fn type_name(ctx: RuleNodeView<'_>) -> Option<String> {
    name_from_identifier(ctx)
}

/// The declared name of a method/constructor: its first `identifier` child's
/// covered text.
fn method_name(ctx: RuleNodeView<'_>) -> Option<String> {
    name_from_identifier(ctx)
}

/// Given a member body-declaration wrapper, find the inner method/constructor
/// declaration whose function space should be opened at the wrapper level — so
/// the wrapper's own-line modifiers/annotations (siblings of the declaration,
/// visited before the declaration node) belong to the method's
/// LOC/Halstead/span rather than the enclosing class/interface.
///
/// Handles:
/// - `classBodyDeclaration` → (generic) method/constructor declaration;
/// - `interfaceBodyDeclaration` → `interfaceMethodDeclaration` /
///   `genericInterfaceMethodDeclaration` → `interfaceCommonBodyDeclaration`;
/// - `annotationTypeElementDeclaration` → `annotationTypeElementRest` →
///   `annotationMethodOrConstantRest` → `annotationMethodRest`.
///
/// Returns the node that would otherwise open the space (the same node the
/// declaration-arm opens), or `None` when the member is not a method-shaped
/// declaration (a field, nested type, const, compact ctor — those keep their
/// existing open sites).
fn wrapper_inner_method(ctx: RuleNodeView<'_>) -> Option<RuleNodeView<'_>> {
    match ctx.rule_index() {
        jp::RULE_CLASS_BODY_DECLARATION => {
            let member = ctx.child_rule(jp::RULE_MEMBER_DECLARATION)?;
            for child in member.children() {
                if let Some(c) = child.as_rule() {
                    match c.rule_index() {
                        jp::RULE_METHOD_DECLARATION | jp::RULE_CONSTRUCTOR_DECLARATION => {
                            return Some(c);
                        }
                        jp::RULE_GENERIC_METHOD_DECLARATION => {
                            return c.child_rule(jp::RULE_METHOD_DECLARATION);
                        }
                        jp::RULE_GENERIC_CONSTRUCTOR_DECLARATION => {
                            return c.child_rule(jp::RULE_CONSTRUCTOR_DECLARATION);
                        }
                        _ => {}
                    }
                }
            }
            None
        }
        // Interface method: walk the DIRECT path interfaceBodyDeclaration →
        // interfaceMemberDeclaration → (generic)interfaceMethodDeclaration →
        // interfaceCommonBodyDeclaration. `interfaceCommonBodyDeclaration` is a
        // direct child of both method-declaration forms (`interfaceMethodModifier*
        // [typeParameters] interfaceCommonBodyDeclaration`), so use `child_rule`
        // — an unbounded search could reach a nested type's method.
        jp::RULE_INTERFACE_BODY_DECLARATION => {
            let member = ctx.child_rule(jp::RULE_INTERFACE_MEMBER_DECLARATION)?;
            let decl = member
                .child_rule(jp::RULE_INTERFACE_METHOD_DECLARATION)
                .or_else(|| member.child_rule(jp::RULE_GENERIC_INTERFACE_METHOD_DECLARATION))?;
            decl.child_rule(jp::RULE_INTERFACE_COMMON_BODY_DECLARATION)
        }
        // Annotation element: walk the DIRECT path
        // annotationTypeElementDeclaration → annotationTypeElementRest →
        // annotationMethodOrConstantRest → annotationMethodRest. A plain
        // `child_rule` chain (not an unbounded `find_descendant`) is required
        // because `annotationTypeElementRest` also has nested-type alternatives
        // (`annotationTypeDeclaration`, `classDeclaration`, …) — descending into
        // those would find a *nested* annotation's element and open a phantom
        // method for the outer type (`@interface A { @interface B { String
        // v(); } }` must have no method on `A`). `None` when the element is a
        // nested type or a constant, not a method.
        jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION => ctx
            .child_rule(jp::RULE_ANNOTATION_TYPE_ELEMENT_REST)?
            .child_rule(jp::RULE_ANNOTATION_METHOD_OR_CONSTANT_REST)?
            .child_rule(jp::RULE_ANNOTATION_METHOD_REST),
        _ => None,
    }
}

/// The wrapper rules whose own-line modifiers/annotations should be folded into
/// the type-declaration space opened beneath them (mirrors `wrapper_inner_type`
/// / the method wrappers). A top-level type wraps in `typeDeclaration`; a
/// method-local type in `localTypeDeclaration`; a member type in one of the
/// body-declaration wrappers.
fn is_type_wrapper(ri: usize) -> bool {
    matches!(
        ri,
        jp::RULE_TYPE_DECLARATION
            | jp::RULE_LOCAL_TYPE_DECLARATION
            | jp::RULE_CLASS_BODY_DECLARATION
            | jp::RULE_INTERFACE_BODY_DECLARATION
            | jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION
    )
}

/// Given a type wrapper (`typeDeclaration`/`localTypeDeclaration` or a member
/// body-declaration), find the inner class-like declaration whose space should
/// be opened at the wrapper — so the wrapper's own-line modifiers/annotations
/// (`@Deprecated\npublic class C {}`, `public static class Inner {}`) belong to
/// the type's LOC/Halstead/span rather than the enclosing space. Descends the
/// direct `child_rule` path (never an unbounded search — a body wrapper's
/// member alternatives include *other* declarations whose own nested types must
/// not be captured here). Returns `None` when the member is not a class-like
/// type (a field, method, const, etc. keep their existing sites).
fn wrapper_inner_type(ctx: RuleNodeView<'_>) -> Option<RuleNodeView<'_>> {
    let holder = match ctx.rule_index() {
        // typeDeclaration / localTypeDeclaration hold the type declaration as a
        // direct child (after `classOrInterfaceModifier*`).
        jp::RULE_TYPE_DECLARATION | jp::RULE_LOCAL_TYPE_DECLARATION => ctx,
        // A member type is `classBodyDeclaration → memberDeclaration → <type>`
        // (or the interface/annotation equivalents).
        jp::RULE_CLASS_BODY_DECLARATION => ctx.child_rule(jp::RULE_MEMBER_DECLARATION)?,
        jp::RULE_INTERFACE_BODY_DECLARATION => {
            ctx.child_rule(jp::RULE_INTERFACE_MEMBER_DECLARATION)?
        }
        jp::RULE_ANNOTATION_TYPE_ELEMENT_DECLARATION => {
            ctx.child_rule(jp::RULE_ANNOTATION_TYPE_ELEMENT_REST)?
        }
        _ => return None,
    };
    for child in holder.children() {
        if let Some(c) = child.as_rule()
            && opens_class_like(c.rule_index())
        {
            return Some(c);
        }
    }
    None
}

fn name_from_identifier(ctx: RuleNodeView<'_>) -> Option<String> {
    for child in ctx.children() {
        if let Some(c) = child.as_rule()
            && matches!(
                c.rule_index(),
                jp::RULE_IDENTIFIER | jp::RULE_TYPE_IDENTIFIER
            )
        {
            let t = c.text();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// Count the declared formal parameters of a method/constructor.
///
/// The grammar's `formalParameters` rule is asymmetric:
/// `'(' ((receiverParameter | formalParameter) (',' formalParameterList)*)? ')'`
/// — the *first* parameter is a direct `formalParameter` child of
/// `formalParameters`, and the *remaining* parameters live in a nested
/// `formalParameterList`. So the count is the direct `formalParameter`
/// children plus the `formalParameterList`'s `formalParameter`s. A leading
/// `receiverParameter` (`Foo this`) is not a value parameter and is excluded.
/// A trailing varargs (`int... rest`) is a plain `formalParameter` here.
fn count_formal_params(ctx: RuleNodeView<'_>) -> u32 {
    let Some(params) = find_descendant(ctx, jp::RULE_FORMAL_PARAMETERS) else {
        return 0;
    };
    let direct = params.child_rules(jp::RULE_FORMAL_PARAMETER).count() as u32;
    let in_list = params
        .child_rule(jp::RULE_FORMAL_PARAMETER_LIST)
        .map(|list| list.child_rules(jp::RULE_FORMAL_PARAMETER).count() as u32)
        .unwrap_or(0);
    direct + in_list
}

/// Count lambda parameters. A lambda's parameters are either a single bare
/// `identifier`, a parenthesized `formalParameterList`, or a
/// `lambdaLVTIList`/identifier list.
fn count_lambda_args(ctx: RuleNodeView<'_>) -> u32 {
    let Some(params) = ctx.child_rule(jp::RULE_LAMBDA_PARAMETERS) else {
        return 0;
    };
    // `(a, b) -> …` → formalParameterList; `(var a, var b) -> …` →
    // lambdaLVTIList; `x -> …` → a single bare identifier; `(x, y) -> …` →
    // a comma-separated identifier list.
    if let Some(list) = params.child_rule(jp::RULE_FORMAL_PARAMETER_LIST) {
        return list.child_rules(jp::RULE_FORMAL_PARAMETER).count() as u32;
    }
    if let Some(list) = params.child_rule(jp::RULE_LAMBDA_LVTI_LIST) {
        return list.child_rules(jp::RULE_LAMBDA_LVTI_PARAMETER).count() as u32;
    }

    params.child_rules(jp::RULE_IDENTIFIER).count() as u32
}

/// Count the variables declared by a `fieldDeclaration`
/// (`int a, b, c;` → 3), via `variableDeclarators → variableDeclarator`.
fn field_variable_count(ctx: RuleNodeView<'_>) -> u32 {
    ctx.child_rule(jp::RULE_VARIABLE_DECLARATORS)
        .map(|vds| vds.child_rules(jp::RULE_VARIABLE_DECLARATOR).count() as u32)
        .unwrap_or(0)
}

/// Record a record's component parameters as class attributes (NPA). Walks
/// `recordDeclaration → recordHeader → recordComponentList → recordComponent`.
fn record_record_components(ctx: RuleNodeView<'_>, state: &mut State) {
    let count = count_record_components(ctx);
    for _ in 0..count {
        state.npa.record_attribute(ContainerKind::Class, true);
    }
}

/// Count a record's declared components via
/// `recordDeclaration → recordHeader → recordComponentList → recordComponent`.
/// These are both the record's public attributes (NPA) and the parameter list
/// of its (canonical/compact) constructor (NArgs).
fn count_record_components(ctx: RuleNodeView<'_>) -> u32 {
    ctx.child_rule(jp::RULE_RECORD_HEADER)
        .and_then(|header| header.child_rule(jp::RULE_RECORD_COMPONENT_LIST))
        .map(|list| list.child_rules(jp::RULE_RECORD_COMPONENT).count() as u32)
        .unwrap_or(0)
}

/// Resolve an explicit visibility from a body-declaration wrapper's
/// `modifier`s: `Some(false)` if any `modifier` carries `private`/`protected`,
/// `Some(true)` if one carries `public`, `None` if no visibility modifier is
/// present (caller applies the container default).
///
/// `ctx` is the body-declaration wrapper (`classBodyDeclaration`,
/// `interfaceBodyDeclaration`, …); its `modifier` children are siblings of the
/// member declaration, which is where Java places visibility keywords. The
/// `modifier` rule wraps a `classOrInterfaceModifier`, so we scan two levels
/// for the visibility token.
fn visibility_from_modifiers(ctx: RuleNodeView<'_>) -> Option<bool> {
    for modifier in ctx.child_rules(jp::RULE_MODIFIER) {
        if let Some(vis) = visibility_token(modifier) {
            return Some(vis);
        }
    }
    // Interface body declarations wrap modifiers in `modifier` too, but a
    // record/enum body may present a bare `classOrInterfaceModifier`; scan
    // those directly as well.
    for m in ctx.child_rules(jp::RULE_CLASS_OR_INTERFACE_MODIFIER) {
        if let Some(vis) = visibility_from_token_holder(m) {
            return Some(vis);
        }
    }
    None
}

/// Read a visibility token from a `modifier` context, descending into its
/// `classOrInterfaceModifier` child if present.
fn visibility_token(modifier: RuleNodeView<'_>) -> Option<bool> {
    if let Some(v) = visibility_from_token_holder(modifier) {
        return Some(v);
    }
    for coi in modifier.child_rules(jp::RULE_CLASS_OR_INTERFACE_MODIFIER) {
        if let Some(v) = visibility_from_token_holder(coi) {
            return Some(v);
        }
    }
    None
}

/// Read `public`/`private`/`protected` directly from a context's token
/// children.
fn visibility_from_token_holder(ctx: RuleNodeView<'_>) -> Option<bool> {
    if ctx.has_token(jl::PUBLIC) {
        return Some(true);
    }
    if ctx.has_token(jl::PRIVATE) || ctx.has_token(jl::PROTECTED) {
        return Some(false);
    }
    None
}

/// Whether an `expression` context carries a top-level assignment operator as
/// a direct child token: `=`, a compound assign (`+=`, `-=`, …), or an
/// increment/decrement (`++`, `--`). Fitzpatrick's ABC lists `++`/`--` under
/// the assignment (A) component alongside `=`.
fn has_assignment_op(ctx: RuleNodeView<'_>) -> bool {
    ctx.has_token(jl::ASSIGN)
        || ctx.has_token(jl::ADD_ASSIGN)
        || ctx.has_token(jl::SUB_ASSIGN)
        || ctx.has_token(jl::MUL_ASSIGN)
        || ctx.has_token(jl::DIV_ASSIGN)
        || ctx.has_token(jl::AND_ASSIGN)
        || ctx.has_token(jl::OR_ASSIGN)
        || ctx.has_token(jl::XOR_ASSIGN)
        || ctx.has_token(jl::MOD_ASSIGN)
        || ctx.has_token(jl::LSHIFT_ASSIGN)
        || ctx.has_token(jl::RSHIFT_ASSIGN)
        || ctx.has_token(jl::URSHIFT_ASSIGN)
        || ctx.has_token(jl::INC)
        || ctx.has_token(jl::DEC)
}

/// Find the first descendant rule with `rule_index`, searching direct children
/// then recursing. Used for parameter lists that may sit under an intermediate
/// wrapper (e.g. `genericMethodDeclaration → methodDeclaration`).
///
/// Searches *descendants* (children-first), never `ctx` itself. The runtime's
/// [`Node::first_rule`](mehen_antlr::runtime::Node::first_rule) includes the
/// receiver in its pre-order search, so it is applied per child here to keep
/// the original "descendants only" semantics.
fn find_descendant(ctx: RuleNodeView<'_>, rule_index: usize) -> Option<RuleNodeView<'_>> {
    ctx.children()
        .find_map(|child| child.first_rule(rule_index))
        .and_then(|node| node.as_rule())
}

fn container_kind(parent_kind: SpaceKind) -> ContainerKind {
    match parent_kind {
        SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum => ContainerKind::Class,
        SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
        _ => ContainerKind::Other,
    }
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
/// Operands: identifiers, literals, `this`, `super`. Skipped: whitespace,
/// comments, EOF. Everything else (keywords, punctuation, operators) is an
/// operator.
fn halstead_class(tt: i32) -> HalsteadClass {
    if matches!(
        tt,
        jl::IDENTIFIER
            | jl::DECIMAL_LITERAL
            | jl::HEX_LITERAL
            | jl::OCT_LITERAL
            | jl::BINARY_LITERAL
            | jl::FLOAT_LITERAL
            | jl::HEX_FLOAT_LITERAL
            | jl::BOOL_LITERAL
            | jl::CHAR_LITERAL
            | jl::STRING_LITERAL
            | jl::TEXT_BLOCK
            | jl::NULL_LITERAL
            | jl::THIS
            | jl::SUPER
    ) {
        return HalsteadClass::Operand;
    }

    if matches!(tt, jl::WS | jl::COMMENT | jl::LINE_COMMENT) || tt < 0 {
        return HalsteadClass::Skip;
    }

    HalsteadClass::Operator
}

/// A stable string label for an operator token, used as its Halstead operator
/// key. The numeric token type is stable for a given generated grammar.
fn kp_token_name(tt: i32) -> String {
    format!("t{tt}")
}
