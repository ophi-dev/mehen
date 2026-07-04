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
use mehen_antlr::runtime::{ParseTree, ParserRuleContext, TerminalNode};
use mehen_antlr::{LocToken, LocTokenKind, ctx_span};
use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricTreeBuilder, SpaceRangeTracker, State,
    apply_state_to, finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

use crate::generated::java_lexer as jl;
use crate::generated::java_parser as jp;

/// Drive the walk over the parsed `compilationUnit` tree and return the unit
/// `MetricSpace`. LOC is computed from `loc_tokens` in a single ordered pass
/// *after* the tree walk has opened and closed every space.
pub(crate) fn walk(
    tree: &ParseTree,
    line_index: &LineIndex,
    source_len: usize,
    loc_tokens: &[LocToken],
) -> MetricSpace {
    let unit_span = match tree {
        ParseTree::Rule(rule) => ctx_span(rule.context(), line_index, source_len),
        _ => mehen_core::SourceSpan::empty(),
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

    if let ParseTree::Rule(rule) = tree {
        for child in rule.context().children() {
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
    /// threaded down through `expression` descendants so the cognitive
    /// boolean-run collapse can be *parent-relative* (SonarJava's rule): a
    /// `&&`/`||` adds a flat +1 only when its operator differs from its
    /// parent's. Threading stops (resets to `None`) at any non-`expression`
    /// boundary — statement, `arguments`, `methodCall` — which isolates
    /// independent boolean expressions (e.g. a method-call argument) from the
    /// enclosing run. `None` outside a boolean expression.
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

    fn visit(&mut self, node: &ParseTree, hint: ChildHint) {
        match node {
            ParseTree::Rule(rule) => self.visit_rule(rule.context(), hint),
            ParseTree::Terminal(term) => self.visit_terminal(term, hint),
            ParseTree::Error(_) => {}
        }
    }

    fn visit_terminal(&mut self, term: &TerminalNode, hint: ChildHint) {
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
                let text = term.symbol().text().unwrap_or("");
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
            let extra_rows = term
                .symbol()
                .text()
                .map(|t| t.bytes().filter(|&b| b == b'\n').count() as u32)
                .unwrap_or(0);
            for row in start_row..=start_row.saturating_add(extra_rows) {
                self.current().loc.observe_code_line(row);
            }
        }
    }

    fn visit_rule(&mut self, ctx: &ParserRuleContext, hint: ChildHint) {
        let ri = ctx.rule_index();
        let saved_cognitive = self.cognitive;

        // NPA / NPM: classify a direct member of the enclosing class/interface
        // body before opening any space for this node (so the kinds stack
        // still has the class on top).
        if hint.in_class_member
            && let Some(container) = hint.member_container
        {
            let public = hint.member_is_public.unwrap_or(true);
            self.classify_class_member(ctx, ri, container, public);
        }

        let opened = self.maybe_open_space(ctx, ri, hint);
        self.classify_rule(ctx, ri, hint);

        self.visit_children(ctx, ri, hint);

        if opened {
            self.close_space();
        }
        self.cognitive = saved_cognitive;
    }

    fn visit_children(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
        let children = ctx.children();

        // For an `if` statement, the `else`-branch body is the `statement`
        // that appears after the `ELSE` token. Tag it so an `if` reached
        // through it (without an intervening `block`) is recognized as
        // `else if` and does not add nesting.
        let else_body_idx = if is_if_statement(ctx, ri) {
            else_branch_index(children)
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
            self.member_propagation(ctx, ri, hint);

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
        let child_bool_op = if ri == jp::RULE_EXPRESSION {
            if ctx.has_token(jl::AND) {
                Some(BoolOp::And)
            } else if ctx.has_token(jl::OR) {
                Some(BoolOp::Or)
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
        // must not let its initializer declaration add a second LLOC. Mark the
        // header subtree so `classify_loc_rule` suppresses the forInit
        // declaration's own LLOC.
        let in_for_init =
            hint.in_for_init || matches!(ri, jp::RULE_FOR_CONTROL | jp::RULE_FOR_INIT);

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
            child_index_of_rule(children, jp::RULE_CLASS_BODY)
        } else {
            None
        };
        let in_anon_body = if opens_class_like(ri) || opens_function_space(ri) {
            false
        } else {
            hint.in_anon_body
        };

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

        for (idx, child) in children.iter().enumerate() {
            let mut child_hint = ChildHint::default();
            if Some(idx) == else_body_idx || propagate_else {
                child_hint.is_else_branch = true;
            }
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
        ctx: &ParserRuleContext,
        ri: usize,
        hint: ChildHint,
    ) -> (bool, Option<ContainerKind>, Option<bool>) {
        // A `classBodyDeclaration` reached *inside* a constant-specific
        // enum-constant body belongs to that constant's anonymous subclass,
        // which opens no space here — so it must NOT seed a member position on
        // the lexically-enclosing enum (NPA/NPM), mirroring the WMC
        // suppression at close time.
        if hint.in_anon_body {
            return (false, None, None);
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
                let container = self.enclosing_container();
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

    /// Open a metric space for space-introducing rules. Returns whether a
    /// space was pushed.
    fn maybe_open_space(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) -> bool {
        match ri {
            // One function space per method shape. Interface methods reach
            // their name/params/body via `interfaceCommonBodyDeclaration`
            // (wrapped by `interfaceMethodDeclaration` /
            // `genericInterfaceMethodDeclaration`), so the space is opened
            // there — opening at the wrapper too would double-count. A function
            // opened inside an enum-constant body must not roll into the enum's
            // WMC (it belongs to the constant's anonymous subclass).
            jp::RULE_METHOD_DECLARATION
            | jp::RULE_CONSTRUCTOR_DECLARATION
            | jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION
            | jp::RULE_INTERFACE_COMMON_BODY_DECLARATION
            | jp::RULE_ANNOTATION_METHOD_REST => {
                let name = method_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.nom.record_function();
                // A compact record constructor has no `formalParameters` — its
                // parameter list *is* the record's components, so its NArgs is
                // the enclosing record's component count (threaded down via
                // `ChildHint`). Every other method shape counts its own
                // `formalParameters`.
                let nargs = if ri == jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION {
                    hint.record_component_count.unwrap_or(0)
                } else {
                    count_formal_params(ctx)
                };
                state.nargs.record_function_args(nargs);
                self.push_space(SpaceKind::Function, name, ctx, state, hint.in_anon_body);
                self.enter_function_cognitive(hint.in_anon_body);
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
            jp::RULE_CLASS_DECLARATION | jp::RULE_RECORD_DECLARATION => {
                let name = type_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.npa.record_class_like();
                state.npm.record_class_like();
                state.wmc.record_class_like();
                // Record component parameters (`record R(int x, int y)`) as
                // class attributes.
                record_record_components(ctx, &mut state);
                // A class-like space owns its own WMC, so it never suppresses a
                // parent contribution (and it opens its own space, clearing any
                // enclosing enum-constant-body suppression for its members).
                self.push_space(SpaceKind::Class, name, ctx, state, false);
                self.enter_class_cognitive();
                true
            }
            jp::RULE_ENUM_DECLARATION => {
                let name = type_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.npa.record_class_like();
                state.npm.record_class_like();
                state.wmc.record_class_like();
                self.push_space(SpaceKind::Enum, name, ctx, state, false);
                self.enter_class_cognitive();
                true
            }
            jp::RULE_INTERFACE_DECLARATION | jp::RULE_ANNOTATION_TYPE_DECLARATION => {
                let name = type_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.npa.record_class_like();
                state.npm.record_class_like();
                self.push_space(SpaceKind::Interface, name, ctx, state, false);
                self.enter_class_cognitive();
                true
            }
            _ => false,
        }
    }

    fn new_space_state(&self, ctx: &ParserRuleContext) -> State {
        let mut state = State::new();
        let span = ctx_span(ctx, self.line_index, self.source_len);
        state.loc.set_span(
            span.start_line.saturating_sub(1),
            span.end_line.saturating_sub(1),
            false,
        );
        state
    }

    fn push_space(
        &mut self,
        kind: SpaceKind,
        name: Option<String>,
        ctx: &ParserRuleContext,
        state: State,
        suppress_parent_wmc: bool,
    ) {
        let span = ctx_span(ctx, self.line_index, self.source_len);
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
    fn classify_rule(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
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
        self.classify_expression(ctx, ri, hint);
        self.classify_abc_rule(ctx, ri);
        self.classify_loc_rule(ctx, ri, hint);
    }

    /// Classify a `statement` context by its leading keyword token.
    fn classify_statement(&mut self, ctx: &ParserRuleContext, hint: ChildHint) {
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
    fn classify_expression(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
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
        // per operator, plus a *parent-relative* cognitive increment. Per
        // SonarSource's cognitive-complexity rule, a run of like operators
        // costs +1 once; a boolean node adds +1 only when its operator differs
        // from the enclosing boolean operator (`hint.parent_bool_op`). This is
        // tree-structural, not traversal-order — so `a && b || c && d` scores 3
        // (the `||` differs from the enclosing `if`, and each `&&` differs from
        // the `||`), where an order-sensitive `last_op` would collapse the two
        // `&&` runs to 2.
        let this_op = if ctx.has_token(jl::AND) {
            Some(BoolOp::And)
        } else if ctx.has_token(jl::OR) {
            Some(BoolOp::Or)
        } else {
            None
        };
        if let Some(op) = this_op {
            self.current().cyclomatic.record_decision();
            self.current().abc.record_condition();
            if hint.parent_bool_op != Some(op) {
                self.current().cognitive.increment_by_one();
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

    fn classify_abc_rule(&mut self, ctx: &ParserRuleContext, ri: usize) {
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
            // under A). `has_assignment_op` covers all of them.
            jp::RULE_EXPRESSION if has_assignment_op(ctx) => {
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

    fn classify_loc_rule(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
        // A `for` header's initializer declaration is part of the `for`
        // statement's single logical line — not its own LLOC.
        if ri == jp::RULE_LOCAL_VARIABLE_DECLARATION && hint.in_for_init {
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
    /// `modifier`s (threaded down via [`ChildHint`]).
    fn classify_class_member(
        &mut self,
        ctx: &ParserRuleContext,
        ri: usize,
        container: ContainerKind,
        public: bool,
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
            // comes from the (modifier-less) record body. Its `modifier`s are
            // its own children, so resolve visibility from `ctx` itself.
            jp::RULE_COMPACT_CONSTRUCTOR_DECLARATION => {
                let is_public = visibility_from_modifiers(ctx).unwrap_or(public);
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
/// as the anonymous body (its sibling `arguments` is a plain call).
fn child_index_of_rule(children: &[ParseTree], rule_index: usize) -> Option<usize> {
    children.iter().position(
        |c| matches!(c, ParseTree::Rule(rule) if rule.context().rule_index() == rule_index),
    )
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

/// Whether an `expression` context carries its own operator — i.e. has any
/// direct terminal (token) child. A transparent operand expression (`a`, or a
/// single wrapped sub-expression) has only rule children and no tokens; an
/// operator form (`==`, `<`, ternary `? :`, index `[]`, unary, `instanceof`, a
/// method/creator call, `.` access, …) always has at least one token child.
/// Used by the cognitive boolean-run collapse: only a token-less (transparent)
/// expression forwards the enclosing `&&`/`||`; anything with its own operator
/// starts a fresh boolean context.
fn expression_has_operator_token(ctx: &ParserRuleContext) -> bool {
    ctx.children()
        .iter()
        .any(|c| matches!(c, ParseTree::Terminal(_)))
}

/// Whether this `statement` context is an `if` statement (has an `IF` token as
/// a direct child).
fn is_if_statement(ctx: &ParserRuleContext, ri: usize) -> bool {
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
fn statement_is_else_transparent(ctx: &ParserRuleContext, ri: usize) -> bool {
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
fn ctx_is_block(ctx: &ParserRuleContext) -> bool {
    let mut rules = ctx.children().iter().filter_map(|c| match c {
        ParseTree::Rule(rule) => Some(rule.context()),
        _ => None,
    });
    matches!((rules.next(), rules.next()), (Some(only), None)
        if only.rule_index() == jp::RULE_BLOCK)
}

/// Whether this `statement` is an empty statement (a bare `;`) — its only
/// child is the `SEMI` terminal. Distinguished from `return;`/`break;` (which
/// carry a keyword terminal too) by requiring exactly one child.
fn ctx_is_empty_statement(ctx: &ParserRuleContext) -> bool {
    let children = ctx.children();
    children.len() == 1
        && matches!(&children[0], ParseTree::Terminal(t) if t.symbol().token_type() == jl::SEMI)
}

/// Whether this `statement` is a labeled-statement wrapper
/// (`identifierLabel = identifier ':' statement`) — its rule children are
/// exactly one `identifier` followed by one nested `statement`. The label is
/// an attribute of the inner statement (counted when visited), not its own
/// logical line.
fn ctx_is_label_wrapper(ctx: &ParserRuleContext) -> bool {
    let mut rules = ctx.children().iter().filter_map(|c| match c {
        ParseTree::Rule(rule) => Some(rule.context().rule_index()),
        _ => None,
    });
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
fn count_angle_tokens(ctx: &ParserRuleContext) -> (usize, usize) {
    let mut lt = 0;
    let mut gt = 0;
    for child in ctx.children() {
        if let ParseTree::Terminal(t) = child {
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
fn else_branch_index(children: &[ParseTree]) -> Option<usize> {
    let mut seen_else = false;
    for (idx, child) in children.iter().enumerate() {
        match child {
            ParseTree::Terminal(t) if t.symbol().token_type() == jl::ELSE => seen_else = true,
            ParseTree::Rule(rule)
                if seen_else && rule.context().rule_index() == jp::RULE_STATEMENT =>
            {
                return Some(idx);
            }
            _ => {}
        }
    }
    None
}

/// The declared name of a type: its first `identifier`/`typeIdentifier`
/// child's covered text.
fn type_name(ctx: &ParserRuleContext) -> Option<String> {
    name_from_identifier(ctx)
}

/// The declared name of a method/constructor: its first `identifier` child's
/// covered text.
fn method_name(ctx: &ParserRuleContext) -> Option<String> {
    name_from_identifier(ctx)
}

fn name_from_identifier(ctx: &ParserRuleContext) -> Option<String> {
    for child in ctx.children() {
        if let ParseTree::Rule(rule) = child {
            let c = rule.context();
            if matches!(
                c.rule_index(),
                jp::RULE_IDENTIFIER | jp::RULE_TYPE_IDENTIFIER
            ) {
                let t = c.text();
                if !t.is_empty() {
                    return Some(t);
                }
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
fn count_formal_params(ctx: &ParserRuleContext) -> u32 {
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
fn count_lambda_args(ctx: &ParserRuleContext) -> u32 {
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
fn field_variable_count(ctx: &ParserRuleContext) -> u32 {
    ctx.child_rule(jp::RULE_VARIABLE_DECLARATORS)
        .map(|vds| vds.child_rules(jp::RULE_VARIABLE_DECLARATOR).count() as u32)
        .unwrap_or(0)
}

/// Record a record's component parameters as class attributes (NPA). Walks
/// `recordDeclaration → recordHeader → recordComponentList → recordComponent`.
fn record_record_components(ctx: &ParserRuleContext, state: &mut State) {
    let count = count_record_components(ctx);
    for _ in 0..count {
        state.npa.record_attribute(ContainerKind::Class, true);
    }
}

/// Count a record's declared components via
/// `recordDeclaration → recordHeader → recordComponentList → recordComponent`.
/// These are both the record's public attributes (NPA) and the parameter list
/// of its (canonical/compact) constructor (NArgs).
fn count_record_components(ctx: &ParserRuleContext) -> u32 {
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
fn visibility_from_modifiers(ctx: &ParserRuleContext) -> Option<bool> {
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
fn visibility_token(modifier: &ParserRuleContext) -> Option<bool> {
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
fn visibility_from_token_holder(ctx: &ParserRuleContext) -> Option<bool> {
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
fn has_assignment_op(ctx: &ParserRuleContext) -> bool {
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
fn find_descendant(ctx: &ParserRuleContext, rule_index: usize) -> Option<&ParserRuleContext> {
    if let Some(direct) = ctx.child_rule(rule_index) {
        return Some(direct);
    }
    for child in ctx.children() {
        if let ParseTree::Rule(rule) = child
            && let Some(found) = find_descendant(rule.context(), rule_index)
        {
            return Some(found);
        }
    }
    None
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
