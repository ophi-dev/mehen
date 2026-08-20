// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Tree-sitter-c walker producing per-space metric output that matches
//! the pre-1.0 `legacy::metrics::*::compute for CCode` arms exactly.
//!
//! The walker plugs language-specific classification into the shared
//! [`mehen_tree_sitter::WalkerHooks`] scaffold so the visit recursion,
//! cognitive context save/restore, kinds-stack bookkeeping, and unit
//! finalize stay byte-identical with the Go and Kotlin walkers.
//!
//! Metric coverage:
//! - **Cyclomatic** (legacy `cyclomatic.rs:112-135`): one decision per
//!   `if_statement | case_statement | for_statement | while_statement |
//!   do_statement | conditional_expression | && | ||`. `switch` itself
//!   is not a decision (`case` arms are); `default` is fallthrough.
//! - **Cognitive** (legacy `cognitive.rs:234-294`):
//!   * Increase nesting on `if_statement` (skipping the inner `if` of an
//!     `else if` whose parent is `else_clause`), `for_statement |
//!     while_statement | do_statement | switch_statement |
//!     conditional_expression`. `function_definition` /
//!     `function_definition2` reset nesting and bump function depth.
//!   * `else_clause`: flat `+1` plus `boolean_seq.reset()`.
//!   * `expression_statement | expression_statement2 | return_statement
//!     | declaration`: `boolean_seq.reset()`.
//!   * `binary_expression | binary_expression2`: drive the
//!     `BoolSequence` collapser per `&&`/`||` operator child.
//!   * No closures or lambdas in C.
//! - **ABC** (legacy `abc.rs:233-281`):
//!   * Assignments: `assignment_expression`, `init_declarator` with an
//!     `=` direct child, `update_expression`.
//!   * Branches: every `call_expression | call_expression2`.
//!   * Conditions: `if_statement | else_clause | case_statement |
//!     for_statement | while_statement | do_statement |
//!     conditional_expression | == | != | < | <= | > | >= | && | || |
//!     !`.
//! - **NExit** (legacy `exit.rs:117-128`): `return_statement` only.
//!   `break`/`continue`/`goto` are intra-function flow.
//! - **NArgs** (legacy `nargs.rs:230-275`, `compute_c_args`): walk
//!   `child_by_field_name("declarator")` inward through
//!   `function_declarator` chains until a `parameter_list` is found,
//!   filter `parameter_declaration` children, exclude `variadic_parameter`
//!   (`...`), and apply the `(void)` rule (lone parameter whose source
//!   text equals `void` → 0 args).
//! - **NOM** (legacy `nom.rs:180-189`): every `function_definition` /
//!   `function_definition2` opens a function space. C has no closures.
//! - **LOC** (legacy `loc.rs:567-613`): PLOC default arm,
//!   LLOC for the 36-variant statement / preprocessor-container set,
//!   CLOC for `comment` nodes via `observe_comment`.
//! - **Halstead** (legacy `getter.rs::get_op_type for CCode`): operators
//!   for ~70 keyword/punctuation kinds, operands for the 14 identifier-
//!   shaped + literal kinds. Operands dedup by text only (kind =
//!   `"Operand"`); operators dedup by kind. Same convention as
//!   `mehen-go/src/walker.rs`.
//! - **NPA / NPM / WMC** (legacy `npa.rs:202-205`, `npm.rs:203-206`,
//!   `wmc.rs:142-145`): C is excluded from class-aware metrics; all
//!   three are intentionally no-ops here.
//! - **MI**: derived in `mehen_metrics::state::apply_state_to` from
//!   loc/cyclomatic/halstead — no C-specific logic.

use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{HalsteadOperand, HalsteadOperator, MetricEvidence, State};
use mehen_tree_sitter::{OpenSpaceRequest, WalkerCtx, WalkerHooks, node_span, run, text_of};
use smol_str::SmolStr;
use tree_sitter::Node;

use crate::grammar::C;

/// Drive the walker over the parsed C tree and return the populated
/// `MetricSpace`. Plugs C classification into the shared
/// [`mehen_tree_sitter::run`] scaffold. Contribution evidence is
/// recorded into the caller-owned `evidence` sink (plan §5.4).
pub(crate) fn walk_program(
    root: Node<'_>,
    source: &[u8],
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let mut hooks = CHooks;
    run(&mut hooks, root, source, line_index, evidence)
}

struct CHooks;

impl WalkerHooks for CHooks {
    fn open_space(&mut self, ctx: &mut WalkerCtx<'_>, node: &Node<'_>) -> Option<OpenSpaceRequest> {
        match C::from(node.kind_id()) {
            C::FunctionDefinition | C::FunctionDefinition2 => {
                let name = function_name(node, ctx.source).map(|s| s.to_string());
                let span = node_span(node, ctx.line_index);
                let mut state = State::new();
                state.loc.set_span(
                    node.start_position().row as u32,
                    node.end_position().row as u32,
                    false,
                );
                state.nom.record_function();
                let argc = count_c_args(node, ctx.source);
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
            _ => None,
        }
    }

    fn on_space_enter(&mut self, ctx: &mut WalkerCtx<'_>, kind: SpaceKind) {
        if matches!(kind, SpaceKind::Function) {
            // Legacy `Cognitive for CCode`'s `FunctionDefinition |
            // FunctionDefinition2` arm: reset nesting; bump function
            // depth when nested. C nested-function syntax is GCC-only
            // and rare, but the depth bump is preserved for parity.
            let nested_inside_function = ctx
                .ancestor_kinds()
                .any(|k| matches!(k, SpaceKind::Function));
            ctx.cognitive.nesting = 0;
            if nested_inside_function {
                ctx.cognitive.depth = ctx.cognitive.depth.saturating_add(1);
            }
        }
    }

    fn before_close(&mut self, state: &mut State, closed_kind: SpaceKind, _parent: SpaceKind) {
        if matches!(closed_kind, SpaceKind::Function) {
            // Mirrors the legacy `wmc::Stats` close path. WMC is
            // class-aware; C has no classes, so this value is never
            // published — but the bookkeeping is kept so per-space
            // walker shape stays uniform with Go/Kotlin.
            state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
        }
    }

    fn classify(&mut self, ctx: &mut WalkerCtx<'_>, node: &Node<'_>) {
        let kind = C::from(node.kind_id());

        // Cyclomatic — legacy `Cyclomatic for CCode`.
        if matches!(
            kind,
            C::IfStatement
                | C::CaseStatement
                | C::ForStatement
                | C::WhileStatement
                | C::DoStatement
                | C::ConditionalExpression
                | C::AMPAMP
                | C::PIPEPIPE
        ) {
            ctx.current().cyclomatic.record_decision();
            ctx.record_evidence(node, |e, s| e.decision(s, node.kind()));
        }

        classify_cognitive(ctx, node, kind);
        classify_abc(ctx, node, kind);

        // NExit — legacy `Exit for CCode`. Only `return_statement`;
        // break/continue/goto are intra-function flow.
        if matches!(kind, C::ReturnStatement) {
            ctx.current().nexit.record_exit();
            ctx.record_evidence(node, |e, s| e.exit(s, node.kind()));
        }

        classify_loc(ctx, node, kind);
        classify_halstead(ctx, node, kind);
    }
}

fn classify_cognitive(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: C) {
    match kind {
        // Outer `if`. `is_else_if` checks parent == ElseClause: when
        // true, the structural +1 is paid by the surrounding `else
        // clause` arm and only the boolean-seq reset stays here
        // (defense-in-depth duplicate of the ElseClause reset).
        C::IfStatement if !is_else_if(node) => {
            let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.increase_nesting(effective);
            ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(node, |e, s| e.cognitive(s, delta, node.kind()));
        }
        C::IfStatement => {
            ctx.current().cognitive.boolean_seq.reset();
        }
        C::ForStatement
        | C::WhileStatement
        | C::DoStatement
        | C::SwitchStatement
        | C::ConditionalExpression => {
            let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.increase_nesting(effective);
            ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(node, |e, s| e.cognitive(s, delta, node.kind()));
        }
        C::ElseClause => {
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.increment_by_one();
            ctx.current().cognitive.boolean_seq.reset();
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(node, |e, s| e.cognitive(s, delta, node.kind()));
        }
        C::ExpressionStatement | C::ExpressionStatement2 | C::ReturnStatement | C::Declaration => {
            ctx.current().cognitive.boolean_seq.reset();
        }
        C::BinaryExpression | C::BinaryExpression2 => {
            // Legacy `compute_booleans::<C>(node, &AMPAMP, &PIPEPIPE)`:
            // walk the children and feed each `&&`/`||` operator into
            // the sequence collapser. Evidence spans point at the
            // operator token; same-operator repeats apply no delta and
            // record nothing.
            for child in iter_children(node) {
                let op = match C::from(child.kind_id()) {
                    C::AMPAMP => "&&",
                    C::PIPEPIPE => "||",
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

fn classify_abc(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: C) {
    match kind {
        C::AssignmentExpression | C::UpdateExpression => {
            ctx.current().abc.record_assignment();
            ctx.record_evidence(node, |e, s| e.abc_assignment(s, node.kind()));
        }
        C::InitDeclarator if has_child_kind(node, C::EQ) => {
            ctx.current().abc.record_assignment();
            ctx.record_evidence(node, |e, s| e.abc_assignment(s, node.kind()));
        }
        C::CallExpression | C::CallExpression2 => {
            ctx.current().abc.record_branch();
            ctx.record_evidence(node, |e, s| e.abc_branch(s, node.kind()));
        }
        C::IfStatement
        | C::ElseClause
        | C::CaseStatement
        | C::ForStatement
        | C::WhileStatement
        | C::DoStatement
        | C::ConditionalExpression
        | C::EQEQ
        | C::BANGEQ
        | C::LT
        | C::LTEQ
        | C::GT
        | C::GTEQ
        | C::AMPAMP
        | C::PIPEPIPE
        | C::BANG => {
            ctx.current().abc.record_condition();
            ctx.record_evidence(node, |e, s| e.abc_condition(s, node.kind()));
        }
        _ => {}
    }
}

fn classify_loc(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: C) {
    match kind {
        // Containers and string internals must not contribute their
        // own physical line. Mirrors the legacy `Loc for CCode`'s
        // explicit no-op arm.
        C::TranslationUnit
        | C::StringLiteral
        | C::ConcatenatedString
        | C::CharLiteral
        | C::CompoundStatement
        | C::StringContent
        | C::EscapeSequence => {}
        C::Comment => {
            let start = node.start_position().row as u32;
            let end = node.end_position().row as u32;
            ctx.current().loc.observe_comment(start, end);
        }
        // LLOC kind set: 36 statement-shaped + preprocessor-container
        // variants (legacy `loc.rs:583-616`). Each occurrence
        // contributes one logical line.
        C::Declaration
        | C::TypeDefinition
        | C::ExpressionStatement
        | C::ExpressionStatement2
        | C::IfStatement
        | C::SwitchStatement
        | C::CaseStatement
        | C::WhileStatement
        | C::DoStatement
        | C::ForStatement
        | C::ReturnStatement
        | C::BreakStatement
        | C::ContinueStatement
        | C::GotoStatement
        | C::LabeledStatement
        | C::SehTryStatement
        | C::SehLeaveStatement
        | C::FunctionDefinition
        | C::FunctionDefinition2
        | C::PreprocInclude
        | C::PreprocDef
        | C::PreprocFunctionDef
        | C::PreprocCall
        | C::PreprocIf
        | C::PreprocIf2
        | C::PreprocIf3
        | C::PreprocIf4
        | C::PreprocIfdef
        | C::PreprocIfdef2
        | C::PreprocIfdef3
        | C::PreprocIfdef4
        | C::PreprocElse
        | C::PreprocElse2
        | C::PreprocElse3
        | C::PreprocElse4
        | C::PreprocElif
        | C::PreprocElif2
        | C::PreprocElif3
        | C::PreprocElif4
        | C::PreprocElifdef
        | C::PreprocElifdef2
        | C::PreprocElifdef3
        | C::PreprocElifdef4 => {
            ctx.current().loc.observe_lloc();
        }
        _ => {
            let start = node.start_position().row as u32;
            ctx.current().loc.observe_code_line(start);
        }
    }
}

fn classify_halstead(ctx: &mut WalkerCtx<'_>, node: &Node<'_>, kind: C) {
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
// Halstead classification (legacy `Getter::get_op_type for CCode`).
// --------------------------------------------------------------------

enum HalsteadType {
    Operator,
    Operand,
    Unknown,
}

fn halstead_op_type(kind: C) -> HalsteadType {
    match kind {
        // Keywords and control flow.
        C::If
        | C::Else
        | C::Switch
        | C::Case
        | C::Default
        | C::While
        | C::Do
        | C::For
        | C::Return
        | C::Break
        | C::Continue
        | C::Goto
        | C::Sizeof
        | C::Alignof
        | C::Alignof2
        | C::Alignof3
        | C::Alignof4
        | C::Alignof5
        | C::Offsetof
        | C::Typedef
        | C::Extern
        | C::Static
        | C::Auto
        | C::Register
        | C::Inline
        | C::Inline2
        | C::Inline3
        | C::Forceinline
        | C::ThreadLocal
        | C::Thread
        | C::Const
        | C::Constexpr
        | C::Volatile
        | C::Volatile2
        | C::Restrict
        | C::Restrict2
        | C::Atomic
        | C::Noreturn
        | C::Noreturn2
        | C::Nonnull
        | C::Alignas
        | C::Alignas2
        | C::Signed
        | C::Unsigned
        | C::Long
        | C::Short
        | C::Enum
        | C::Struct
        | C::Union
        // Punctuation.
        | C::LPAREN
        | C::LPAREN2
        | C::RPAREN
        | C::LBRACE
        | C::RBRACE
        | C::LBRACK
        | C::RBRACK
        | C::COMMA
        | C::SEMI
        | C::COLON
        | C::QMARK
        | C::DOT
        | C::DASHGT
        // Arithmetic / bitwise / logical / comparison operators.
        | C::PLUS
        | C::DASH
        | C::STAR
        | C::SLASH
        | C::PERCENT
        | C::AMP
        | C::PIPE
        | C::CARET
        | C::TILDE
        | C::BANG
        | C::LTLT
        | C::GTGT
        | C::AMPAMP
        | C::PIPEPIPE
        | C::EQ
        | C::EQEQ
        | C::BANGEQ
        | C::LT
        | C::LTEQ
        | C::GT
        | C::GTEQ
        | C::PLUSEQ
        | C::DASHEQ
        | C::STAREQ
        | C::SLASHEQ
        | C::PERCENTEQ
        | C::AMPEQ
        | C::PIPEEQ
        | C::CARETEQ
        | C::LTLTEQ
        | C::GTGTEQ
        | C::PLUSPLUS
        | C::DASHDASH
        // Preprocessor directives count as operators.
        | C::HASHinclude
        | C::HASHdefine
        | C::HASHif
        | C::HASHifdef
        | C::HASHifndef
        | C::HASHelse
        | C::HASHelif
        | C::HASHelifdef
        | C::HASHelifndef
        | C::HASHendif => HalsteadType::Operator,

        // Operands: identifiers, type identifiers, and literals.
        C::Identifier
        | C::FieldIdentifier
        | C::TypeIdentifier
        | C::StatementIdentifier
        | C::PrimitiveType
        | C::NumberLiteral
        | C::CharLiteral
        | C::StringLiteral
        | C::ConcatenatedString
        | C::True
        | C::False
        | C::NULL
        | C::Nullptr
        | C::SystemLibString => HalsteadType::Operand,

        _ => HalsteadType::Unknown,
    }
}

// --------------------------------------------------------------------
// Function-name and NArgs helpers — direct ports of legacy
// `getter.rs::get_func_space_name for CCode` and `compute_c_args`.
// --------------------------------------------------------------------

/// Walk `node.declarator` inward through `function_declarator` /
/// `pointer_declarator` / `parenthesized_declarator` chains until an
/// identifier is found. Mirrors legacy `getter.rs::get_func_space_name`.
fn function_name<'src>(node: &Node<'_>, source: &'src [u8]) -> Option<&'src str> {
    let mut cur = node.child_by_field_name("declarator");
    while let Some(current) = cur {
        match C::from(current.kind_id()) {
            C::Identifier | C::FieldIdentifier | C::TypeIdentifier => {
                let bytes = &source[current.start_byte()..current.end_byte()];
                return core::str::from_utf8(bytes).ok();
            }
            _ => {
                cur = current.child_by_field_name("declarator");
            }
        }
    }
    None
}

#[inline(always)]
fn is_c_function_declarator(kind: u16) -> bool {
    matches!(
        C::from(kind),
        C::FunctionDeclarator
            | C::FunctionDeclarator2
            | C::FunctionDeclarator3
            | C::FunctionDeclarator4
            | C::FunctionDeclarator5
    )
}

#[inline(always)]
fn is_c_parameter_list(kind: u16) -> bool {
    matches!(C::from(kind), C::ParameterList | C::ParameterList2)
}

/// Walk the `declarator` field inward until the innermost
/// `function_declarator` is found; that node's direct `parameter_list`
/// child holds the parameters. Mirrors legacy `compute_c_args`.
fn count_c_args(node: &Node<'_>, source: &[u8]) -> u32 {
    let mut cur = node.child_by_field_name("declarator");
    while let Some(current) = cur {
        if is_c_function_declarator(current.kind_id()) {
            let mut cursor = current.walk();
            let Some(param_list) = current
                .children(&mut cursor)
                .find(|c| is_c_parameter_list(c.kind_id()))
            else {
                return 0;
            };
            let mut list_cursor = param_list.walk();
            let params: Vec<_> = param_list
                .children(&mut list_cursor)
                .filter(|p| C::from(p.kind_id()) == C::ParameterDeclaration)
                .collect();
            // `(void)` is C's spelling for "no parameters" and must not
            // be counted. Detect it precisely by checking that the
            // sole parameter's text literally matches `void`.
            // `variadic_parameter` (`...`) is filtered out above.
            let is_void_only = params.len() == 1
                && source
                    .get(params[0].start_byte()..params[0].end_byte())
                    .is_some_and(|bytes| bytes == b"void");
            return if is_void_only { 0 } else { params.len() as u32 };
        }
        cur = current.child_by_field_name("declarator");
    }
    0
}

// --------------------------------------------------------------------
// Tree-sitter helpers
// --------------------------------------------------------------------

fn parent_kind(node: &Node<'_>) -> Option<C> {
    node.parent().map(|p| C::from(p.kind_id()))
}

/// `is_else_if`: an `if_statement` whose direct parent is the
/// `else_clause` wrapper. tree-sitter-c parses `else if (...)` as
/// `if_statement { else_clause { if_statement } }`, so the *inner*
/// if matches this predicate. Mirrors legacy
/// `checker.rs::is_else_if for CCode`.
fn is_else_if(node: &Node<'_>) -> bool {
    if C::from(node.kind_id()) != C::IfStatement {
        return false;
    }
    parent_kind(node) == Some(C::ElseClause)
}

fn has_child_kind(node: &Node<'_>, kind: C) -> bool {
    iter_children(node).any(|c| C::from(c.kind_id()) == kind)
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
