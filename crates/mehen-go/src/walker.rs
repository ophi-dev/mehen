// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Tree-sitter-go walker producing per-space metric output that matches
//! the pre-1.0 `legacy::metrics::*::compute for GoCode` arms exactly.
//!
//! The walker drives its own tree-sitter cursor recursion (rather than
//! the generic `mehen-tree-sitter::walker::LanguageRules` plug-in) so it
//! can do parent-aware classification — Go's `is_else_if` and
//! `default_case in select` predicates both inspect the parent node,
//! which the generic walker cannot express.
//!
//! Metric coverage:
//! - **Cyclomatic**: every `if_statement`, `for_statement`,
//!   `expression_case`, `type_case`, `communication_case`, every `&&`/`||`
//!   token, plus a `default_case` whose parent is `select_statement`
//!   (legacy: `Cyclomatic for GoCode`).
//! - **Cognitive**: nesting on `if_statement` (skipping the inner `if`
//!   of an `else if`), `for_statement`,
//!   `expression_switch_statement`, `type_switch_statement`,
//!   `select_statement`; flat `+1` on every `else` keyword;
//!   boolean-sequence reset on every statement-shape node;
//!   `not_operator("!")` for unary `!` operators; per-`&&`/`||`
//!   sequence collapse via the shared `BoolSequence` (legacy:
//!   `Cognitive for GoCode`).
//! - **ABC**: assignments via `assignment_statement` /
//!   `short_var_declaration` (target count from the `left` field) and
//!   `inc_statement` / `dec_statement` (one each), `receive_statement`
//!   / `range_clause` only when the `left` field is present, `var_spec`
//!   with an `=` token, and `const_spec`. Branches: every
//!   `call_expression`. Conditions: `if`, `for`, every `case` arm,
//!   plus the comparison + boolean operator tokens. (Legacy:
//!   `Abc for GoCode`.)
//! - **NExit**: `return_statement` (legacy: `Exit for GoCode`).
//! - **NArgs**: per-`parameter_declaration` /
//!   `variadic_parameter_declaration` count = `max(1, identifier_count)`
//!   (legacy: `compute_go_args`).
//! - **NOM**: every `function_declaration` / `method_declaration` →
//!   function space; every `func_literal` → closure space.
//! - **LOC**: PLOC (every node default arm), LLOC (the legacy 26-kind
//!   set), CLOC (every `comment` node). Legacy: `Loc for GoCode`.
//! - **Halstead**: per-node operator/operand emission using the
//!   legacy `Getter::get_op_type for GoCode` table. Operands dedup by
//!   text only (kind = `"Operand"`) so semantically-equal text from
//!   different identifier-shaped nodes (e.g. `Identifier`,
//!   `Identifier2`, `Identifier3`, `BlankIdentifier`,
//!   `FieldIdentifier`, `LabelName`, `PackageIdentifier`,
//!   `TypeIdentifier`) merges into a single bucket — matching the
//!   legacy raw-byte-slice key.
//! - **NPA / NPM / WMC**: Go has no class-like constructs; all three
//!   are intentionally no-ops, matching the legacy
//!   `impl X for GoCode` empty bodies.

use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{HalsteadOperand, HalsteadOperator, MetricEvidence, State};
use mehen_tree_sitter::{OpenSpaceRequest, WalkerCtx, WalkerHooks, node_span, run, text_of};
use smol_str::SmolStr;
use tree_sitter::Node;

use crate::grammar::Go;

/// Drive the walker over the parsed Go tree and return the populated
/// `MetricSpace`. Plugs Go classification into the shared
/// [`mehen_tree_sitter::run`] scaffold. Contribution evidence is
/// recorded into the caller-owned `evidence` sink (plan §5.4).
pub(crate) fn walk_program(
    root: Node<'_>,
    source: &[u8],
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let mut hooks = GoHooks;
    run(&mut hooks, root, source, line_index, evidence)
}

struct GoHooks;

impl WalkerHooks for GoHooks {
    fn open_space(&mut self, ctx: &mut WalkerCtx<'_>, node: &Node<'_>) -> Option<OpenSpaceRequest> {
        match Go::from(node.kind_id()) {
            Go::FunctionDeclaration | Go::MethodDeclaration => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| text_of(&n, ctx.source).to_string());
                let span = node_span(node, ctx.line_index);
                let mut state = State::new();
                state.loc.set_span(
                    node.start_position().row as u32,
                    node.end_position().row as u32,
                    false,
                );
                state.nom.record_function();
                let argc = count_go_args(node);
                state.nargs.record_function_args(argc);
                if ctx.evidence.is_enabled() {
                    ctx.evidence.function(span, node.kind());
                    ctx.evidence.function_args(span, argc, node.kind());
                }
                Some(OpenSpaceRequest {
                    kind: SpaceKind::Function,
                    name,
                    span,
                    state,
                })
            }
            Go::FuncLiteral => {
                let span = node_span(node, ctx.line_index);
                let mut state = State::new();
                state.loc.set_span(
                    node.start_position().row as u32,
                    node.end_position().row as u32,
                    false,
                );
                state.nom.record_closure();
                let argc = count_go_args(node);
                state.nargs.record_closure_args(argc);
                if ctx.evidence.is_enabled() {
                    ctx.evidence.closure(span, node.kind());
                    ctx.evidence.closure_args(span, argc, node.kind());
                }
                Some(OpenSpaceRequest {
                    kind: SpaceKind::Closure,
                    name: None,
                    span,
                    state,
                })
            }
            _ => None,
        }
    }

    fn on_space_enter(&mut self, ctx: &mut WalkerCtx<'_>, kind: SpaceKind) {
        match kind {
            SpaceKind::Function => {
                // Legacy `Cognitive for GoCode`'s `FunctionDeclaration | MethodDeclaration`
                // arm: reset nesting, bump function-depth when nested.
                let nested_inside_function = ctx
                    .ancestor_kinds()
                    .any(|k| matches!(k, SpaceKind::Function));
                ctx.cognitive.nesting = 0;
                if nested_inside_function {
                    ctx.cognitive.depth = ctx.cognitive.depth.saturating_add(1);
                }
            }
            SpaceKind::Closure => {
                // Legacy `FuncLiteral` arm: bump lambda counter only;
                // nesting/depth pass through unchanged.
                ctx.cognitive.lambda = ctx.cognitive.lambda.saturating_add(1);
            }
            _ => {}
        }
    }

    fn before_close(&mut self, state: &mut State, closed_kind: SpaceKind, _parent: SpaceKind) {
        if matches!(closed_kind, SpaceKind::Function) {
            state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
        }
    }

    fn classify(&mut self, ctx: &mut WalkerCtx<'_>, node: &Node<'_>) {
        let kind = Go::from(node.kind_id());

        // Cyclomatic — legacy `Cyclomatic for GoCode`. `default_case`
        // inside a `select` is a real communication branch; inside a
        // `switch` it's fallthrough and does not count.
        let is_decision = matches!(
            kind,
            Go::IfStatement
                | Go::ForStatement
                | Go::ExpressionCase
                | Go::TypeCase
                | Go::CommunicationCase
                | Go::AMPAMP
                | Go::PIPEPIPE
        ) || (matches!(kind, Go::DefaultCase)
            && parent_kind(node) == Some(Go::SelectStatement));
        if is_decision {
            ctx.current().cyclomatic.record_decision();
            ctx.record_evidence(node, |e, s| e.decision(s, node.kind()));
        }

        classify_cognitive(ctx, node, kind);
        classify_abc(ctx, node, kind);

        // NExit — legacy `Exit for GoCode`.
        if matches!(kind, Go::ReturnStatement) {
            ctx.current().nexit.record_exit();
            ctx.record_evidence(node, |e, s| e.exit(s, node.kind()));
        }

        classify_loc(ctx, node, kind);
        classify_halstead(ctx, node, kind);
    }
}

fn classify_cognitive(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: Go) {
    match kind {
        // The else-if form (`IfStatement` whose direct parent is
        // also an `IfStatement`) is a no-op in legacy — the outer
        // `if` already opened a nesting level and the connecting
        // `else` keyword adds the flat `+1`.
        Go::IfStatement if !is_else_if(node) => {
            let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.increase_nesting(effective);
            ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(node, |e, s| e.cognitive(s, delta, node.kind()));
        }
        Go::IfStatement => {}
        Go::ForStatement
        | Go::ExpressionSwitchStatement
        | Go::TypeSwitchStatement
        | Go::SelectStatement => {
            let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.increase_nesting(effective);
            ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(node, |e, s| e.cognitive(s, delta, node.kind()));
        }
        Go::Else => {
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.increment_by_one();
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(node, |e, s| e.cognitive(s, delta, node.kind()));
        }
        Go::ExpressionStatement
        | Go::SendStatement
        | Go::ReceiveStatement
        | Go::IncStatement
        | Go::DecStatement
        | Go::AssignmentStatement
        | Go::ShortVarDeclaration
        | Go::VarSpec
        | Go::ConstSpec
        | Go::ReturnStatement => {
            ctx.current().cognitive.boolean_seq.reset();
        }
        // Legacy passes the literal node kind_id (the top-level
        // `unary_expression` kind, not the operator child) into
        // `boolean_seq.not_operator`. We forward a stable `"!"`
        // marker — same effect because `eval_based_on_prev` only
        // cares whether the recorded last_op equals the new
        // boolean operator string.
        Go::UnaryExpression if has_child_kind(node, Go::BANG) => {
            ctx.current().cognitive.boolean_seq.not_operator("!");
        }
        Go::BinaryExpression => {
            // Legacy `compute_booleans::<Go>`: walk the children;
            // for each `&&` / `||` operator child, feed the
            // sequence collapser. The actual punctuation is one of
            // the binary expression's children. Evidence spans point
            // at the operator token; same-operator repeats apply no
            // delta and record nothing.
            for child in iter_children(node) {
                let op = match Go::from(child.kind_id()) {
                    Go::AMPAMP => "&&",
                    Go::PIPEPIPE => "||",
                    _ => continue,
                };
                let before = ctx.current().cognitive.structural;
                ctx.current().cognitive.observe_boolean(op);
                let delta = ctx.current().cognitive.structural.saturating_sub(before);
                ctx.record_evidence(&child, |e, s| e.cognitive(s, delta, op));
            }
        }
        _ => {}
    }
}

fn classify_abc(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: Go) {
    match kind {
        Go::AssignmentStatement | Go::ShortVarDeclaration => {
            let count = go_assignment_target_count(node);
            ctx.current().abc.assignments = ctx.current().abc.assignments.saturating_add(count);
            ctx.record_evidence(node, |e, s| e.abc_assignments_n(s, count, node.kind()));
        }
        Go::ReceiveStatement | Go::RangeClause if node.child_by_field_name("left").is_some() => {
            let count = go_assignment_target_count(node);
            ctx.current().abc.assignments = ctx.current().abc.assignments.saturating_add(count);
            ctx.record_evidence(node, |e, s| e.abc_assignments_n(s, count, node.kind()));
        }
        Go::IncStatement | Go::DecStatement => {
            ctx.current().abc.record_assignment();
            ctx.record_evidence(node, |e, s| e.abc_assignment(s, node.kind()));
        }
        Go::ConstSpec => {
            let count = go_spec_name_count(node);
            ctx.current().abc.assignments = ctx.current().abc.assignments.saturating_add(count);
            ctx.record_evidence(node, |e, s| e.abc_assignments_n(s, count, node.kind()));
        }
        Go::VarSpec if has_child_kind(node, Go::EQ) => {
            let count = go_spec_name_count(node);
            ctx.current().abc.assignments = ctx.current().abc.assignments.saturating_add(count);
            ctx.record_evidence(node, |e, s| e.abc_assignments_n(s, count, node.kind()));
        }
        Go::CallExpression => {
            ctx.current().abc.record_branch();
            ctx.record_evidence(node, |e, s| e.abc_branch(s, node.kind()));
        }
        Go::IfStatement
        | Go::ForStatement
        | Go::ExpressionCase
        | Go::DefaultCase
        | Go::TypeCase
        | Go::CommunicationCase
        | Go::EQEQ
        | Go::BANGEQ
        | Go::LT
        | Go::LTEQ
        | Go::GT
        | Go::GTEQ
        | Go::AMPAMP
        | Go::PIPEPIPE
        | Go::BANG => {
            ctx.current().abc.record_condition();
            ctx.record_evidence(node, |e, s| e.abc_condition(s, node.kind()));
        }
        _ => {}
    }
}

fn classify_loc(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: Go) {
    match kind {
        Go::SourceFile => {}
        Go::Comment => {
            let start = node.start_position().row as u32;
            let end = node.end_position().row as u32;
            ctx.current().loc.observe_comment(start, end);
        }
        Go::ExpressionStatement
        | Go::SendStatement
        | Go::ReceiveStatement
        | Go::IncStatement
        | Go::DecStatement
        | Go::AssignmentStatement
        | Go::ShortVarDeclaration
        | Go::ImportSpec
        | Go::VarSpec
        | Go::ConstSpec
        | Go::TypeSpec
        | Go::EmptyStatement
        | Go::LabeledStatement
        | Go::LabeledStatement2
        | Go::GoStatement
        | Go::DeferStatement
        | Go::ReturnStatement
        | Go::BreakStatement
        | Go::ContinueStatement
        | Go::GotoStatement
        | Go::FallthroughStatement
        | Go::IfStatement
        | Go::ExpressionSwitchStatement
        | Go::TypeSwitchStatement
        | Go::SelectStatement
        | Go::ForStatement => {
            ctx.current().loc.observe_lloc();
        }
        _ => {
            let start = node.start_position().row as u32;
            ctx.current().loc.observe_code_line(start);
        }
    }
}

fn classify_halstead(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: Go) {
    // Halstead routes to the *current* (innermost) space so nested
    // function bodies carry their own counts; the close path's
    // `merge_child_into_parent` rolls these up into the enclosing
    // scope and the unit (set-union for `n1`/`n2`, sum for
    // `N1`/`N2`).
    match halstead_op_type(kind) {
        HalsteadType::Operator => {
            let kind_label: &'static str = kind.into();
            ctx.current().halstead.observe_operator(HalsteadOperator {
                kind: SmolStr::new(kind_label),
                text: None,
            });
        }
        HalsteadType::Operand => {
            let text = text_of(node, ctx.source);
            ctx.current().halstead.observe_operand(HalsteadOperand {
                kind: SmolStr::new("Operand"),
                text: Some(SmolStr::new(text)),
            });
        }
        HalsteadType::Unknown => {}
    }
}

// --------------------------------------------------------------------
// Halstead classification (legacy `Getter::get_op_type for GoCode`).
// --------------------------------------------------------------------

enum HalsteadType {
    Operator,
    Operand,
    Unknown,
}

fn halstead_op_type(kind: Go) -> HalsteadType {
    match kind {
        // Operators: keywords and control-flow tokens.
        // Note: `Go::Go` is the `go` keyword (goroutine launch), not the
        // language identifier.
        Go::Func
        | Go::Go
        | Go::Defer
        | Go::Return
        | Go::If
        | Go::Else
        | Go::For
        | Go::Range
        | Go::Switch
        | Go::Select
        | Go::Case
        | Go::Default
        | Go::Break
        | Go::Continue
        | Go::Goto
        | Go::Fallthrough
        | Go::Chan
        | Go::Map
        | Go::Struct
        | Go::Interface
        | Go::Type
        | Go::Var
        | Go::Const
        | Go::Package
        | Go::Import
        // Punctuation operators.
        | Go::DOT
        | Go::COMMA
        | Go::SEMI
        | Go::COLON
        | Go::COLONEQ
        | Go::EQ
        | Go::PLUSEQ
        | Go::DASHEQ
        | Go::STAREQ
        | Go::SLASHEQ
        | Go::PERCENTEQ
        | Go::AMPEQ
        | Go::PIPEEQ
        | Go::CARETEQ
        | Go::LTLTEQ
        | Go::GTGTEQ
        | Go::AMPCARETEQ
        // Arithmetic / logic operators.
        | Go::PLUS
        | Go::DASH
        | Go::STAR
        | Go::SLASH
        | Go::PERCENT
        | Go::AMP
        | Go::PIPE
        | Go::CARET
        | Go::LTLT
        | Go::GTGT
        | Go::AMPAMP
        | Go::PIPEPIPE
        | Go::AMPCARET
        | Go::PLUSPLUS
        | Go::DASHDASH
        | Go::LTDASH
        | Go::TILDE
        | Go::EQEQ
        | Go::BANGEQ
        | Go::LT
        | Go::LTEQ
        | Go::GT
        | Go::GTEQ
        | Go::BANG
        | Go::LPAREN
        | Go::LBRACK
        | Go::LBRACE
        | Go::DOTDOTDOT => HalsteadType::Operator,

        // Operands: identifiers, type identifiers, and literals.
        Go::Identifier
        | Go::Identifier2
        | Go::Identifier3
        | Go::BlankIdentifier
        | Go::FieldIdentifier
        | Go::LabelName
        | Go::PackageIdentifier
        | Go::TypeIdentifier
        | Go::IntLiteral
        | Go::FloatLiteral
        | Go::ImaginaryLiteral
        | Go::RuneLiteral
        | Go::RawStringLiteral
        | Go::InterpretedStringLiteral
        | Go::True
        | Go::False
        | Go::Nil
        | Go::Iota => HalsteadType::Operand,

        _ => HalsteadType::Unknown,
    }
}

// --------------------------------------------------------------------
// ABC helpers — direct ports of legacy `go_*` helper fns.
// --------------------------------------------------------------------

fn go_expression_list_len(node: &Node<'_>) -> u32 {
    iter_children(node)
        .filter(|child| !matches!(Go::from(child.kind_id()), Go::COMMA | Go::Comment))
        .count() as u32
}

fn go_assignment_target_count(node: &Node<'_>) -> u32 {
    node.child_by_field_name("left")
        .map(|child| go_expression_list_len(&child))
        .unwrap_or(1)
}

fn go_spec_name_count(node: &Node<'_>) -> u32 {
    let mut count: u32 = 0;
    for child in iter_children(node) {
        match Go::from(child.kind_id()) {
            Go::EQ => break,
            Go::Identifier | Go::Identifier2 | Go::Identifier3 | Go::BlankIdentifier => {
                count = count.saturating_add(1)
            }
            _ => {}
        }
    }
    count.max(1)
}

// --------------------------------------------------------------------
// NArgs helper — direct port of legacy `compute_go_args`.
// --------------------------------------------------------------------

fn count_go_args(node: &Node<'_>) -> u32 {
    let Some(params) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut total: u32 = 0;
    for child in iter_children(&params) {
        match Go::from(child.kind_id()) {
            Go::ParameterDeclaration | Go::VariadicParameterDeclaration => {
                let mut names: u32 = 0;
                for inner in iter_children(&child) {
                    if matches!(
                        Go::from(inner.kind_id()),
                        Go::Identifier | Go::Identifier2 | Go::Identifier3 | Go::BlankIdentifier
                    ) {
                        names = names.saturating_add(1);
                    }
                }
                total = total.saturating_add(names.max(1));
            }
            _ => {}
        }
    }
    total
}

// --------------------------------------------------------------------
// Tree-sitter helpers
// --------------------------------------------------------------------

fn parent_kind(node: &Node<'_>) -> Option<Go> {
    node.parent().map(|p| Go::from(p.kind_id()))
}

fn is_else_if(node: &Node<'_>) -> bool {
    if Go::from(node.kind_id()) != Go::IfStatement {
        return false;
    }
    parent_kind(node) == Some(Go::IfStatement)
}

fn has_child_kind(node: &Node<'_>, kind: Go) -> bool {
    iter_children(node).any(|c| Go::from(c.kind_id()) == kind)
}

fn iter_children<'tree>(node: &Node<'tree>) -> impl Iterator<Item = Node<'tree>> {
    let mut cursor = node.walk();
    let mut nodes = Vec::new();
    if cursor.goto_first_child() {
        loop {
            nodes.push(cursor.node());
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    nodes.into_iter()
}
