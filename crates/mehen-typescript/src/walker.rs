// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Oxc Visit-based walker that produces a populated `MetricSpace`.
//!
//! The walker mirrors the algorithm in
//! `mehen_tree_sitter::walker::walk` (per-space `State` stack, finalize
//! on close, fold child stats into parent) but works against Oxc's AST
//! and lexer token stream instead of tree-sitter nodes. Per the rewrite
//! plan §4.3 the *publishing* logic (`apply_state_to`,
//! `finalize_state`, `merge_child_into_parent`) lives in
//! `mehen-metrics`; this file owns the language-specific *interpretation*
//! plus the AST traversal.
//!
//! Halstead operator/operand classification reads the lexer token
//! stream — see [`emit_halstead_from_tokens`]. Every other metric is
//! driven by [`Visitor::enter_node`] over `AstKind`.
//!
//! Reference for the operator / operand / decision-point sets:
//! `crates/mehen-engine/src/legacy/getter.rs` (TS `get_op_type`),
//! `crates/mehen-engine/src/legacy/checker.rs` (TS `is_func` / closure /
//! non-arg / func-space), and `crates/mehen-engine/src/legacy/metrics/*`.
//! The Oxc rewrite intentionally preserves the legacy classification;
//! any deviation must be documented as deliberate parity work.

use mehen_core::{LineIndex, MetricSpace, SourceSpan, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricEvidence, MetricTreeBuilder,
    SpaceRangeTracker, State, apply_state_to, close_space, finalize_state,
};
use oxc_allocator::Vec as ArenaVec;
use oxc_ast::AstKind;
use oxc_ast::ast::{
    AssignmentTarget, Class, Function, FunctionType, Program, PropertyKey, TSAccessibility,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Kind;
use oxc_parser::Token;
use oxc_span::Span;
use oxc_syntax::scope::ScopeFlags;
use smol_str::SmolStr;

/// Crate-internal entry point — drive the walker over a parsed
/// program. Only the `*Analyzer::analyze` impls in this crate call
/// this; not part of any cross-crate API. Contribution evidence is
/// recorded into the caller-owned `evidence` sink (plan §5.4).
pub(crate) fn walk_program<'a>(
    program: &Program<'a>,
    tokens: &ArenaVec<'a, Token>,
    source: &str,
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let unit_span = program_span(program, line_index);
    let mut visitor = Visitor::new(source, line_index, unit_span, evidence);
    visitor.visit_program(program);

    // Halstead is driven by the token stream. Each token is emitted into
    // the *innermost open space* by mapping the token's source span back
    // to the visitor's per-space stack — but the token stream does not
    // carry AST-context, so we scan it once after the AST walk and use
    // the recorded scope spans to assign each token. See
    // `emit_halstead_from_tokens` for the assignment algorithm.
    visitor.emit_halstead_from_tokens(tokens, source);

    visitor.finish()
}

fn program_span(program: &Program<'_>, line_index: &LineIndex) -> SourceSpan {
    SourceSpan {
        start_byte: program.span.start,
        end_byte: program.span.end,
        start_line: line_index.line_at(program.span.start),
        end_line: line_index.line_at(program.span.end),
    }
}

fn span_to_source_span(span: Span, line_index: &LineIndex) -> SourceSpan {
    SourceSpan {
        start_byte: span.start,
        end_byte: span.end,
        start_line: line_index.line_at(span.start),
        end_line: line_index.line_at(span.end),
    }
}

struct Visitor<'a> {
    source: &'a str,
    line_index: &'a LineIndex,
    /// Contribution-evidence sink (plan §5.4). Record methods are
    /// no-ops when disabled, so visits call them unconditionally via
    /// `record_evidence` next to each stat increment.
    evidence: &'a mut MetricEvidence,
    tree: MetricTreeBuilder,
    /// Per-space accumulator stack — index 0 is the unit.
    stack: Vec<State>,
    /// Parallel to `stack`: the SpaceKind of each open frame so the
    /// walker can answer "what's my enclosing class-like" without
    /// re-walking the AST. Index 0 is the unit.
    kinds: Vec<SpaceKind>,
    /// Cognitive context inherited down the recursion. The walker
    /// reuses the legacy `(nesting, depth, lambda)` triple.
    cognitive: CognitiveContext,
    /// Byte ranges of TypeScript-only AST nodes (type annotations,
    /// `implements` clauses, interface bodies, class names, …). The
    /// post-walk token sweep skips tokens whose span falls inside one
    /// of these ranges so TS-only identifiers don't inflate `n2 / N2`.
    /// See `docs/typescript-halstead-spec.md`.
    type_only_ranges: Vec<Span>,
    /// Routes Halstead tokens emitted by the post-AST sweep to the
    /// deepest enclosing function/class/closure space so per-space
    /// JSON entries are non-zero. PR #95 discussion_r3265658502
    /// flagged the same gap on the Python walker; the TS walker had
    /// the same `stack[0]`-only behaviour and now shares the routing
    /// helper.
    halstead_routing: SpaceRangeTracker,
}

#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

impl<'a> Visitor<'a> {
    fn new(
        source: &'a str,
        line_index: &'a LineIndex,
        unit_span: SourceSpan,
        evidence: &'a mut MetricEvidence,
    ) -> Self {
        let mut state = State::new();
        state.loc.set_span(
            unit_span.start_line.saturating_sub(1),
            unit_span.end_line.saturating_sub(1),
            true,
        );
        Self {
            source,
            line_index,
            evidence,
            tree: MetricTreeBuilder::new(unit_span),
            stack: vec![state],
            kinds: vec![SpaceKind::Unit],
            cognitive: CognitiveContext::default(),
            type_only_ranges: Vec::new(),
            halstead_routing: SpaceRangeTracker::new(),
        }
    }

    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    /// Record contribution evidence for an Oxc span. The
    /// `SourceSpan` is only computed when the sink is enabled, so
    /// visits can call this unconditionally next to each stat
    /// increment:
    ///
    /// ```ignore
    /// self.current().cyclomatic.record_decision();
    /// self.record_evidence(span, |e, s| e.decision(s, "if_statement"));
    /// ```
    #[inline]
    fn record_evidence<F>(&mut self, span: Span, record: F)
    where
        F: FnOnce(&mut MetricEvidence, SourceSpan),
    {
        if self.evidence.is_enabled() {
            let source_span = span_to_source_span(span, self.line_index);
            record(self.evidence, source_span);
        }
    }

    fn finish(mut self) -> MetricSpace {
        // Close the unit.
        let mut unit_state = self.stack.pop().expect("walker stack underflow");
        finalize_state(&mut unit_state);
        // Route post-AST tokens (Halstead operator/operand, PLOC code
        // lines, comment lines) to nested spaces. The tracker
        // accumulated per-space events during the token sweep; we now
        // propagate child counts up the parent chain and overlay
        // each entry's Halstead/LOC/MI keys onto the matching
        // `MetricSpace`. The unit's own state is re-published below
        // with the rolled-up unit builders so its keys reflect the
        // file-wide values.
        let mut unit_halstead = std::mem::take(&mut unit_state.halstead);
        let mut unit_loc = std::mem::take(&mut unit_state.loc);
        let mut tree = self.tree.finish();
        self.halstead_routing
            .finalize_into_tree(&mut tree, &mut unit_halstead, &mut unit_loc);
        unit_state.halstead = unit_halstead;
        unit_state.loc = unit_loc;
        apply_state_to(unit_state, &mut tree.metrics);
        tree
    }

    /// Push a fresh `State` for an opened space. The kind-specific
    /// `nom` / `npa` / `npm` / `wmc` bookkeeping is shared with every
    /// other walker via [`State::for_opened_space`].
    fn open_space(&mut self, kind: SpaceKind, span: Span, name: Option<String>) {
        let mut child = State::for_opened_space(kind.clone());
        // LOC span uses 0-based row counts (legacy convention). Convert
        // 1-based line numbers from `line_index` to 0-based.
        let start_row = self.line_index.line_at(span.start).saturating_sub(1);
        let end_row = self.line_index.line_at(span.end).saturating_sub(1);
        child.loc.set_span(start_row, end_row, false);

        let source_span = span_to_source_span(span, self.line_index);
        let space_id = self.tree.open(kind.clone(), source_span, name);
        // Record the byte range so the post-AST Halstead token sweep
        // can route tokens to this scope.
        self.halstead_routing
            .record_open(space_id, span.start, span.end);
        self.stack.push(child);
        self.kinds.push(kind);
    }

    /// Pop the open space, finalize it, and merge into parent. Shared
    /// with every other walker via [`close_space`].
    fn close_space(&mut self) {
        close_space(
            &mut self.stack,
            &mut self.kinds,
            &mut self.tree,
            &mut self.halstead_routing,
        );
    }

    /// Token-stream Halstead emission — runs after the AST walk.
    ///
    /// Each Oxc lexer token is mapped to a `HalsteadOperator` or
    /// `HalsteadOperand` event using a kind translation that mirrors
    /// the legacy `Getter::get_op_type for TypescriptCode` (see
    /// `crates/mehen-engine/src/legacy/getter.rs:119-137`). Each event
    /// is routed to the deepest enclosing function/class/closure space
    /// via [`SpaceRangeTracker`] — tokens that fall outside every
    /// recorded scope go into the unit `HalsteadBuilder`. The tracker's
    /// `finalize_into_tree` (called from [`Self::finish`]) then
    /// propagates each space's set up its parent chain (set-union for
    /// `n1`/`n2`, sum for `N1`/`N2`).
    fn emit_halstead_from_tokens(&mut self, tokens: &ArenaVec<'a, Token>, source: &str) {
        // Sort `type_only_ranges` once so we can answer "is this span
        // inside any range" with a binary search.
        self.type_only_ranges.sort_by_key(|s| s.start);
        for tok in tokens.iter() {
            let tspan = tok.span();
            // Per Halstead's spec, operators *do things* and operands
            // *are things* — pure TypeScript type metadata
            // (annotations, interface bodies, `implements` clauses,
            // type parameters, predefined-type keywords) participates
            // in neither because it carries zero runtime semantics.
            // Tokens inside a TS-only AST subtree are skipped entirely
            // here. This is a deliberate improvement over the pre-1.0
            // tree-sitter-typescript classification which counted the
            // `:` from `: number` as an operator while skipping the
            // `number` keyword (an inconsistent split).
            if self.is_inside_type_only(tspan) {
                continue;
            }
            match classify_token(tok.kind()) {
                TokenClass::Operator(kind) => {
                    self.halstead_routing.observe_operator(
                        tspan.start,
                        tspan.end,
                        &mut self.stack[0].halstead,
                        HalsteadOperator {
                            kind: SmolStr::new(kind),
                            text: None,
                        },
                    );
                }
                TokenClass::Operand(kind) => {
                    let text = source
                        .get(tspan.start as usize..tspan.end as usize)
                        .unwrap_or("");
                    self.halstead_routing.observe_operand(
                        tspan.start,
                        tspan.end,
                        &mut self.stack[0].halstead,
                        HalsteadOperand {
                            kind: SmolStr::new(kind),
                            text: Some(SmolStr::new(text)),
                        },
                    );
                }
                TokenClass::Skip => {}
            }
        }
    }

    fn is_inside_type_only(&self, span: Span) -> bool {
        // Linear scan — `type_only_ranges` is small in practice (a
        // handful of nodes per TS file). The ranges are already sorted
        // by start so we could binary search, but a linear scan is
        // fine for typical fence sizes.
        self.type_only_ranges
            .iter()
            .any(|r| span.start >= r.start && span.end <= r.end)
    }
}

enum TokenClass {
    Operator(&'static str),
    Operand(&'static str),
    Skip,
}

/// Map an Oxc lexer `Kind` to one of legacy TS's operator/operand kinds.
///
/// Reference: `crates/mehen-engine/src/legacy/getter.rs:119-137`. Any
/// `Kind` not enumerated below is intentionally `Skip` (e.g. `Skip`
/// itself for whitespace/comment tokens; reserved keywords that don't
/// appear in legacy's set; punctuation that wasn't operator-classified).
fn classify_token(kind: Kind) -> TokenClass {
    use TokenClass::*;
    match kind {
        // Operators — keywords (legacy `Export, Import, Extends, From, As,
        // Return, Delete, Throw, Break, Continue, If, Else, Switch, Case,
        // Default, Async, For, In, Of, While, Try, Catch, Finally, With,
        // Yield, Await, New, Let, Var, Const, Function`).
        Kind::Export => Operator("export"),
        Kind::Import => Operator("import"),
        Kind::Extends => Operator("extends"),
        Kind::From => Operator("from"),
        Kind::As => Operator("as"),
        Kind::Return => Operator("return"),
        Kind::Delete => Operator("delete"),
        Kind::Throw => Operator("throw"),
        Kind::Break => Operator("break"),
        Kind::Continue => Operator("continue"),
        Kind::If => Operator("if"),
        Kind::Else => Operator("else"),
        Kind::Switch => Operator("switch"),
        Kind::Case => Operator("case"),
        Kind::Default => Operator("default"),
        Kind::Async => Operator("async"),
        Kind::For => Operator("for"),
        Kind::In => Operator("in"),
        Kind::Of => Operator("of"),
        Kind::While => Operator("while"),
        Kind::Try => Operator("try"),
        Kind::Catch => Operator("catch"),
        Kind::Finally => Operator("finally"),
        Kind::With => Operator("with"),
        Kind::Yield => Operator("yield"),
        Kind::Await => Operator("await"),
        Kind::New => Operator("new"),
        Kind::Let => Operator("let"),
        Kind::Var => Operator("var"),
        Kind::Const => Operator("const"),
        Kind::Function => Operator("function"),
        // Punctuators — legacy `LPAREN, COMMA, LBRACK, LBRACE, SEMI, DOT,
        // STAR, COLON, EQ, AT, AMPAMP, PIPEPIPE, PLUS, DASH, DASHDASH,
        // PLUSPLUS, SLASH, PERCENT, STARSTAR, PIPE, AMP, LTLT, TILDE,
        // LT, LTEQ, EQEQ, BANGEQ, GTEQ, GT, GTGT, GTGTGT, PLUSEQ, BANG,
        // BANGEQEQ, EQEQEQ, DASHEQ, STAREQ, SLASHEQ, PERCENTEQ,
        // STARSTAREQ, GTGTEQ, GTGTGTEQ, LTLTEQ, AMPEQ, CARET, CARETEQ,
        // PIPEEQ, QMARK, QMARKQMARK`.
        Kind::LParen => Operator("("),
        Kind::Comma => Operator(","),
        Kind::LBrack => Operator("["),
        Kind::LCurly => Operator("{"),
        Kind::Semicolon => Operator(";"),
        Kind::Dot => Operator("."),
        Kind::Star => Operator("*"),
        Kind::Colon => Operator(":"),
        Kind::Eq => Operator("="),
        Kind::At => Operator("@"),
        Kind::Amp2 => Operator("&&"),
        Kind::Pipe2 => Operator("||"),
        Kind::Plus => Operator("+"),
        Kind::Minus => Operator("-"),
        Kind::Minus2 => Operator("--"),
        Kind::Plus2 => Operator("++"),
        Kind::Slash => Operator("/"),
        Kind::Percent => Operator("%"),
        Kind::Star2 => Operator("**"),
        Kind::Pipe => Operator("|"),
        Kind::Amp => Operator("&"),
        Kind::ShiftLeft => Operator("<<"),
        Kind::Tilde => Operator("~"),
        Kind::LAngle => Operator("<"),
        Kind::LtEq => Operator("<="),
        Kind::Eq2 => Operator("=="),
        Kind::Neq => Operator("!="),
        Kind::GtEq => Operator(">="),
        Kind::RAngle => Operator(">"),
        Kind::ShiftRight => Operator(">>"),
        Kind::ShiftRight3 => Operator(">>>"),
        Kind::PlusEq => Operator("+="),
        Kind::Bang => Operator("!"),
        Kind::Neq2 => Operator("!=="),
        Kind::Eq3 => Operator("==="),
        Kind::MinusEq => Operator("-="),
        Kind::StarEq => Operator("*="),
        Kind::SlashEq => Operator("/="),
        Kind::PercentEq => Operator("%="),
        Kind::Star2Eq => Operator("**="),
        Kind::ShiftRightEq => Operator(">>="),
        Kind::ShiftRight3Eq => Operator(">>>="),
        Kind::ShiftLeftEq => Operator("<<="),
        Kind::AmpEq => Operator("&="),
        Kind::Caret => Operator("^"),
        Kind::CaretEq => Operator("^="),
        Kind::PipeEq => Operator("|="),
        Kind::Question => Operator("?"),
        Kind::Question2 => Operator("??"),
        // Operands — legacy `Identifier, NestedIdentifier,
        // MemberExpression, PropertyIdentifier, String, Number, True,
        // False, Null, Void, This, Super, Undefined, Set, Get, Typeof,
        // Instanceof`. The legacy walker visited *both* the leaf
        // `Identifier` token AND the wrapping `MemberExpression` /
        // `NestedIdentifier` named CST node — three operands per
        // `console.log` (one for each component plus the join). The
        // token stream gives us only the leaves; the walker emits the
        // wrappers from AST visits in `enter_node`.
        Kind::Ident => Operand("Identifier"),
        Kind::Decimal
        | Kind::Float
        | Kind::Binary
        | Kind::Octal
        | Kind::Hex
        | Kind::PositiveExponential
        | Kind::NegativeExponential
        | Kind::DecimalBigInt
        | Kind::BinaryBigInt
        | Kind::OctalBigInt
        | Kind::HexBigInt => Operand("Number"),
        Kind::Str => Operand("String"),
        Kind::NoSubstitutionTemplate
        | Kind::TemplateHead
        | Kind::TemplateMiddle
        | Kind::TemplateTail => Operand("TemplateString"),
        Kind::True => Operand("True"),
        Kind::False => Operand("False"),
        Kind::Null => Operand("Null"),
        Kind::Void => Operand("Void"),
        Kind::This => Operand("This"),
        Kind::Super => Operand("Super"),
        // `Kind::Undefined` is the TypeScript contextual keyword — it
        // appears in *type* positions (e.g. `x: undefined`). The
        // pre-1.0 tree-sitter-typescript path treats type-position
        // tokens as Halstead-skipped (they're not in `Getter::get_op_type`).
        // The JavaScript expression `undefined` is an `Ident`, which
        // already lands in `Kind::Ident => Operand("Identifier")`.
        // Keeping `Undefined` here would inflate operand counts in any
        // TypeScript file that uses `undefined` as a type literal — see
        // the `embedded_code_large.md` fence parity work.
        Kind::Set => Operand("Set"),
        Kind::Get => Operand("Get"),
        Kind::Typeof => Operand("Typeof"),
        Kind::Instanceof => Operand("Instanceof"),
        // `constructor` is a contextual keyword token (it is also a
        // `MethodDefinitionKind::Constructor` slot for the AST). The
        // pre-1.0 tree-sitter-typescript path emits
        // `property_identifier "constructor"` which IS in the legacy
        // operand list — so we count it here for parity.
        Kind::Constructor => Operand("Identifier"),
        Kind::PrivateIdentifier => Operand("PrivateIdentifier"),
        Kind::JSXText => Operand("JSXText"),
        Kind::RegExp => Operand("RegExp"),
        // Closing punctuators (`)`, `}`, `]`) and arrows (`=>`) and
        // colons inside type annotations are not in legacy's operator
        // set — match legacy by skipping.
        _ => Skip,
    }
}

impl<'a> Visit<'a> for Visitor<'a> {
    // ---------- Scope-opening visits ----------

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let kind = function_space_kind(it);
        let name = it.id.as_ref().map(|id| id.name.as_str().to_string());
        self.open_space(kind.clone(), it.span, name);

        // NArgs — `record_function_args` / `record_closure_args` is
        // owned by the just-opened child state. Recursing immediately
        // populates it. NOM itself is recorded by
        // `State::for_opened_space` inside `open_space`; evidence for
        // both is recorded here at the open site.
        let argc = it.params.items.len() as u32;
        match kind {
            SpaceKind::Function => {
                self.current().nargs.record_function_args(argc);
                self.record_evidence(it.span, |e, s| {
                    e.function(s, "function");
                    e.function_args(s, argc, "function");
                });
            }
            SpaceKind::Closure => {
                self.current().nargs.record_closure_args(argc);
                self.record_evidence(it.span, |e, s| {
                    e.closure(s, "function_expression");
                    e.closure_args(s, argc, "function_expression");
                });
            }
            _ => {}
        }

        // Cognitive — function entry resets nesting/lambda and bumps
        // depth when nested inside another function (legacy
        // `increment_function_depth_any`). Closures bump lambda only.
        let mut ctx = self.cognitive;
        match kind {
            SpaceKind::Function => {
                let nested = self
                    .kinds
                    .iter()
                    .rev()
                    .skip(1) // skip self
                    .any(|k| matches!(k, SpaceKind::Function));
                ctx.nesting = 0;
                ctx.lambda = 0;
                if nested {
                    ctx.depth = ctx.depth.saturating_add(1);
                }
            }
            SpaceKind::Closure => {
                ctx.lambda = ctx.lambda.saturating_add(1);
            }
            _ => {}
        }
        let saved = self.cognitive;
        self.cognitive = ctx;

        walk::walk_function(self, it, flags);

        self.cognitive = saved;
        self.close_space();
    }

    fn visit_arrow_function_expression(&mut self, it: &oxc_ast::ast::ArrowFunctionExpression<'a>) {
        // Arrow functions classify per legacy `is_js_func` /
        // `is_js_closure`: an arrow whose ancestor sequence terminates
        // in `VariableDeclarator | AssignmentExpression | LabeledStatement`
        // is a Function; otherwise a Closure. Since Visit doesn't
        // expose ancestors, we approximate with the AST shape of the
        // *current parent frame* — which we track via the kinds stack.
        //
        // For now, treat all arrow functions as Closures (matches
        // legacy's behavior for the common inline arrow case). The
        // var-bound case (`const f = () => {}`) is detected in
        // `visit_variable_declarator` by inspecting the init shape.
        let kind = SpaceKind::Closure;
        self.open_space(kind.clone(), it.span, None);
        let argc = it.params.items.len() as u32;
        self.current().nargs.record_closure_args(argc);
        self.record_evidence(it.span, |e, s| {
            e.closure(s, "arrow_function_expression");
            e.closure_args(s, argc, "arrow_function_expression");
        });

        let mut ctx = self.cognitive;
        ctx.lambda = ctx.lambda.saturating_add(1);
        let saved = self.cognitive;
        self.cognitive = ctx;

        walk::walk_arrow_function_expression(self, it);

        self.cognitive = saved;
        self.close_space();
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        let name = it.id.as_ref().map(|id| id.name.as_str().to_string());
        self.open_space(SpaceKind::Class, it.span, name);
        walk::walk_class(self, it);
        self.close_space();
    }

    fn visit_ts_interface_declaration(&mut self, it: &oxc_ast::ast::TSInterfaceDeclaration<'a>) {
        let name = it.id.name.as_str().to_string();
        self.open_space(SpaceKind::Interface, it.span, Some(name));
        walk::walk_ts_interface_declaration(self, it);
        self.close_space();
    }

    fn visit_method_definition(&mut self, it: &oxc_ast::ast::MethodDefinition<'a>) {
        // Methods open a Function space wrapping the inner Function's
        // body. The default walker would descend into `it.value`
        // (which is a Function) and visit_function would push another
        // space — instead, we open the space here and skip the inner
        // visit_function by walking only the relevant subtrees.
        let name = method_name(&it.key);
        self.open_space(SpaceKind::Function, it.span, name);

        let argc = it.value.params.items.len() as u32;
        self.current().nargs.record_function_args(argc);
        self.record_evidence(it.span, |e, s| {
            e.function(s, "method_definition");
            e.function_args(s, argc, "method_definition");
        });

        let mut ctx = self.cognitive;
        let nested = self
            .kinds
            .iter()
            .rev()
            .skip(1)
            .any(|k| matches!(k, SpaceKind::Function));
        ctx.nesting = 0;
        ctx.lambda = 0;
        if nested {
            ctx.depth = ctx.depth.saturating_add(1);
        }
        let saved = self.cognitive;
        self.cognitive = ctx;

        // Visit the method's key and decorators (so e.g. computed
        // expressions count for cyclomatic), then descend into the
        // function's body but bypass `visit_function` so we don't
        // open a duplicate space. Reach into the body directly.
        for d in &it.decorators {
            self.visit_decorator(d);
        }
        self.visit_property_key(&it.key);
        // Visit params + body without re-pushing a Function frame.
        self.visit_formal_parameters(&it.value.params);
        if let Some(body) = &it.value.body {
            self.visit_function_body(body);
        }

        self.cognitive = saved;
        self.close_space();

        // NPM record (after close) — counted on the parent class's
        // state. Because we already finished `close_space`, the parent
        // is now `current`.
        if matches!(self.parent_kind_top(), SpaceKind::Class | SpaceKind::Impl) {
            let is_public = method_is_public(it);
            self.current()
                .npm
                .record_method(ContainerKind::Class, is_public);
            if is_public {
                self.record_evidence(it.span, |e, s| e.public_method(s, "method_definition"));
            }
        }
    }

    // ---------- Per-node classification ----------

    fn enter_node(&mut self, kind: AstKind<'a>) {
        // Cyclomatic decision points — `IfStatement, ForStatement,
        // ForInStatement, ForOfStatement, WhileStatement, DoStatement,
        // SwitchCase, CatchClause, ConditionalExpression`, plus `&&` /
        // `||` from `LogicalExpression`. Reference:
        // `crates/mehen-engine/src/legacy/metrics/cyclomatic.rs:136-159`.
        // The match yields the evidence detail so the decision set and
        // its reason codes cannot drift apart.
        let decision_detail = match kind {
            AstKind::IfStatement(_) => Some("if_statement"),
            AstKind::ForStatement(_) => Some("for_statement"),
            AstKind::ForInStatement(_) => Some("for_in_statement"),
            AstKind::ForOfStatement(_) => Some("for_of_statement"),
            AstKind::WhileStatement(_) => Some("while_statement"),
            AstKind::DoWhileStatement(_) => Some("do_while_statement"),
            AstKind::SwitchCase(_) => Some("switch_case"),
            AstKind::CatchClause(_) => Some("catch_clause"),
            AstKind::ConditionalExpression(_) => Some("conditional_expression"),
            _ => None,
        };
        if let Some(detail) = decision_detail {
            self.current().cyclomatic.record_decision();
            self.record_evidence(ast_kind_span(kind), |e, s| e.decision(s, detail));
        }
        if let AstKind::LogicalExpression(le) = kind {
            use oxc_syntax::operator::LogicalOperator::*;
            if matches!(le.operator, And | Or) {
                self.current().cyclomatic.record_decision();
                let detail = if matches!(le.operator, And) {
                    "&&"
                } else {
                    "||"
                };
                self.record_evidence(le.span, |e, s| e.decision(s, detail));
            }
        }

        // NExit — `ReturnStatement`, `ThrowStatement`. Legacy:
        // `crates/mehen-engine/src/legacy/metrics/exit.rs:132-152`.
        let exit_detail = match kind {
            AstKind::ReturnStatement(_) => Some("return_statement"),
            AstKind::ThrowStatement(_) => Some("throw_statement"),
            _ => None,
        };
        if let Some(detail) = exit_detail {
            self.current().nexit.record_exit();
            self.record_evidence(ast_kind_span(kind), |e, s| e.exit(s, detail));
        }

        // ABC. Legacy:
        // `crates/mehen-engine/src/legacy/metrics/abc.rs:410-447`.
        match kind {
            AstKind::AssignmentExpression(_) => {
                self.current().abc.record_assignment();
                self.record_evidence(ast_kind_span(kind), |e, s| {
                    e.abc_assignment(s, "assignment_expression");
                });
            }
            AstKind::UpdateExpression(_) => {
                self.current().abc.record_assignment();
                self.record_evidence(ast_kind_span(kind), |e, s| {
                    e.abc_assignment(s, "update_expression");
                });
            }
            AstKind::VariableDeclarator(decl) if decl.init.is_some() => {
                self.current().abc.record_assignment();
                self.record_evidence(decl.span, |e, s| {
                    e.abc_assignment(s, "variable_declarator");
                });
            }
            AstKind::CallExpression(_) => {
                self.current().abc.record_branch();
                self.record_evidence(ast_kind_span(kind), |e, s| {
                    e.abc_branch(s, "call_expression");
                });
            }
            AstKind::NewExpression(_) => {
                self.current().abc.record_branch();
                self.record_evidence(ast_kind_span(kind), |e, s| {
                    e.abc_branch(s, "new_expression");
                });
            }
            AstKind::IfStatement(_)
            | AstKind::ForStatement(_)
            | AstKind::ForInStatement(_)
            | AstKind::WhileStatement(_)
            | AstKind::DoWhileStatement(_)
            | AstKind::ConditionalExpression(_)
            | AstKind::SwitchCase(_)
            | AstKind::CatchClause(_) => {
                self.current().abc.record_condition();
                // `decision_detail` above covers the same construct
                // names except ForOfStatement (not an ABC condition);
                // recompute the label from the same table.
                let detail = decision_detail.unwrap_or("condition");
                self.record_evidence(ast_kind_span(kind), |e, s| e.abc_condition(s, detail));
            }
            // ABC condition counts the comparison binary operators
            // (`==`, `===`, `!=`, `!==`, `<`, `<=`, `>`, `>=`) plus
            // `&&` / `||` (legacy treats those tokens directly). Map
            // both `BinaryExpression` and `LogicalExpression` here.
            AstKind::BinaryExpression(be) => {
                use oxc_syntax::operator::BinaryOperator::*;
                if matches!(
                    be.operator,
                    Equality
                        | Inequality
                        | StrictEquality
                        | StrictInequality
                        | LessThan
                        | LessEqualThan
                        | GreaterThan
                        | GreaterEqualThan
                ) {
                    self.current().abc.record_condition();
                    let detail = comparison_op_str(be.operator);
                    self.record_evidence(be.span, |e, s| e.abc_condition(s, detail));
                }
            }
            AstKind::LogicalExpression(le) => {
                use oxc_syntax::operator::LogicalOperator::*;
                if matches!(le.operator, And | Or) {
                    self.current().abc.record_condition();
                    let detail = if matches!(le.operator, And) {
                        "&&"
                    } else {
                        "||"
                    };
                    self.record_evidence(le.span, |e, s| e.abc_condition(s, detail));
                }
            }
            _ => {}
        }

        // ABC also counts `else` clauses as conditions (legacy
        // `ElseClause`). Oxc has no `ElseClause` AST node; instead an
        // `IfStatement` with an `alternate: Some(_)` represents one.
        // To match legacy we'd have to inspect `IfStatement.alternate`
        // — but legacy counts EVERY `else` (including the `else if`
        // chain, where each inner `IfStatement` is the alternate).
        // Since Oxc collapses nested `else if`s into a chain of
        // `IfStatement` nodes (each one's alternate is the next), each
        // IfStatement we visit represents the parent's else branch.
        // The legacy CST has a separate `ElseClause` node *only* for
        // `else { ... }` blocks; for `else if`, the `IfStatement`
        // itself acts as the else.
        //
        // For parity here: count `+1 condition` whenever an
        // `IfStatement.alternate` is `Some(...)` and is NOT another
        // `IfStatement`. This counts plain `else { ... }` blocks but
        // skips `else if` (which already counts as a separate
        // IfStatement decision).
        if let AstKind::IfStatement(if_stmt) = kind
            && let Some(alt) = &if_stmt.alternate
        {
            use oxc_ast::ast::Statement;
            if !matches!(alt, Statement::IfStatement(_)) {
                self.current().abc.record_condition();
                use oxc_span::GetSpan;
                self.record_evidence(alt.span(), |e, s| e.abc_condition(s, "else_clause"));
            }
        }

        // LOC — every node's start line is a potential PLOC line.
        // Legacy: `crates/mehen-engine/src/legacy/metrics/loc.rs:622-645`.
        // Containers (`Program`, `String`, `DQUOTE`) skip; statement
        // shapes bump LLOC; comments are handled separately via
        // `program.comments` in a pre-pass.
        let span = ast_kind_span(kind);
        let start_row = self.line_index.line_at(span.start).saturating_sub(1);
        match kind {
            AstKind::Program(_) | AstKind::StringLiteral(_) => {
                // Containers — skip per legacy.
            }
            AstKind::ExpressionStatement(_)
            | AstKind::ImportDeclaration(_)
            | AstKind::ExportNamedDeclaration(_)
            | AstKind::ExportDefaultDeclaration(_)
            | AstKind::ExportAllDeclaration(_)
            | AstKind::BlockStatement(_)
            | AstKind::IfStatement(_)
            | AstKind::SwitchStatement(_)
            | AstKind::ForStatement(_)
            | AstKind::ForInStatement(_)
            | AstKind::ForOfStatement(_)
            | AstKind::WhileStatement(_)
            | AstKind::DoWhileStatement(_)
            | AstKind::TryStatement(_)
            | AstKind::WithStatement(_)
            | AstKind::BreakStatement(_)
            | AstKind::ContinueStatement(_)
            | AstKind::DebuggerStatement(_)
            | AstKind::ReturnStatement(_)
            | AstKind::ThrowStatement(_)
            | AstKind::EmptyStatement(_) => {
                self.current().loc.observe_lloc();
                self.current().loc.observe_code_line(start_row);
            }
            _ => {
                self.current().loc.observe_code_line(start_row);
            }
        }

        // Cognitive — drive the per-node state machine. Reference:
        // `crates/mehen-engine/src/legacy/metrics/cognitive.rs:391-433`
        // (the `js_cognitive!` macro).
        match kind {
            AstKind::IfStatement(if_stmt) => {
                // Legacy: nesting-increase only if not else-if. Oxc
                // reports an `else if` as `IfStatement.alternate ->
                // IfStatement`, so the *inner* IfStatement is an
                // else-if. Detect by checking if our parent AST node
                // (which we don't directly track) is an
                // `IfStatement`'s alternate — approximate via a stable
                // property: if the inner's alternate is
                // `Some(IfStatement)`, leave it untouched here; what
                // matters is the *current* node. We don't know if this
                // IfStatement is some parent's alternate — we'd need a
                // parent-stack. For Phase 7 parity, treat every
                // IfStatement as a fresh nesting bump; any visible
                // drift falls into snapshot review.
                let _ = if_stmt;
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.cognitive.nesting += 1;
                self.record_evidence(ast_kind_span(kind), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "if_statement");
                });
            }
            AstKind::ForStatement(_)
            | AstKind::ForInStatement(_)
            | AstKind::ForOfStatement(_)
            | AstKind::WhileStatement(_)
            | AstKind::DoWhileStatement(_)
            | AstKind::SwitchStatement(_)
            | AstKind::TryStatement(_)
            | AstKind::CatchClause(_)
            | AstKind::ConditionalExpression(_) => {
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.cognitive.nesting += 1;
                let detail = match kind {
                    AstKind::ForStatement(_) => "for_statement",
                    AstKind::ForInStatement(_) => "for_in_statement",
                    AstKind::ForOfStatement(_) => "for_of_statement",
                    AstKind::WhileStatement(_) => "while_statement",
                    AstKind::DoWhileStatement(_) => "do_while_statement",
                    AstKind::SwitchStatement(_) => "switch_statement",
                    AstKind::TryStatement(_) => "try_statement",
                    AstKind::CatchClause(_) => "catch_clause",
                    _ => "conditional_expression",
                };
                self.record_evidence(ast_kind_span(kind), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), detail);
                });
            }
            AstKind::ExpressionStatement(_) => {
                self.current().cognitive.boolean_seq.reset();
            }
            AstKind::UnaryExpression(ue) => {
                use oxc_syntax::operator::UnaryOperator::*;
                if matches!(ue.operator, LogicalNot) {
                    self.current().cognitive.boolean_seq.not_operator("!");
                }
            }
            AstKind::LogicalExpression(le) => {
                use oxc_syntax::operator::LogicalOperator::*;
                let op = match le.operator {
                    And => Some("&&"),
                    Or => Some("||"),
                    Coalesce => None,
                };
                if let Some(op) = op {
                    // Evidence amount is the boolean-run delta the
                    // collapser actually applied — a same-operator
                    // repeat moves nothing and records nothing.
                    let before = self.current().cognitive.structural;
                    self.current().cognitive.observe_boolean(op);
                    let delta = self.current().cognitive.structural.saturating_sub(before);
                    self.record_evidence(le.span, |e, s| e.cognitive(s, delta, op));
                }
            }
            _ => {}
        }

        // NPA / NPM — only matter when in a class-like.
        if matches!(
            self.parent_kind_top(),
            SpaceKind::Class | SpaceKind::Impl | SpaceKind::Interface | SpaceKind::Trait
        ) {
            match kind {
                AstKind::PropertyDefinition(pd) => {
                    let is_public = ts_field_visibility(
                        pd.accessibility,
                        matches!(pd.key, PropertyKey::PrivateIdentifier(_)),
                    );
                    let container = match self.parent_kind_top() {
                        SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
                        _ => ContainerKind::Class,
                    };
                    self.current().npa.record_attribute(container, is_public);
                    if is_public {
                        self.record_evidence(pd.span, |e, s| {
                            e.public_attribute(s, "property_definition");
                        });
                    }
                }
                AstKind::AccessorProperty(ap) => {
                    let is_public = ts_field_visibility(
                        ap.accessibility,
                        matches!(ap.key, PropertyKey::PrivateIdentifier(_)),
                    );
                    self.current()
                        .npa
                        .record_attribute(ContainerKind::Class, is_public);
                    if is_public {
                        self.record_evidence(ap.span, |e, s| {
                            e.public_attribute(s, "accessor_property");
                        });
                    }
                }
                AstKind::TSPropertySignature(ts) => {
                    // Interface field — always public in TS.
                    self.current()
                        .npa
                        .record_attribute(ContainerKind::Interface, true);
                    self.record_evidence(ts.span, |e, s| {
                        e.public_attribute(s, "ts_property_signature");
                    });
                }
                AstKind::TSMethodSignature(ts) => {
                    self.current()
                        .npm
                        .record_method(ContainerKind::Interface, true);
                    self.record_evidence(ts.span, |e, s| {
                        e.public_method(s, "ts_method_signature");
                    });
                }
                _ => {}
            }
        }

        // Halstead — emit AST-level operands for `MemberExpression`,
        // `IdentifierReference`, `IdentifierName`, and
        // `PrivateIdentifier`. The token stream emits the *leaf*
        // identifier tokens; the AST visit emits the wrapper-text
        // operand so `console.log` produces three operand entries
        // (`console`, `log`, `console.log`) per legacy. This ensures
        // n2 / N2 match the legacy snapshot.
        match kind {
            AstKind::StaticMemberExpression(_) | AstKind::ComputedMemberExpression(_) => {
                let span = ast_kind_span(kind);
                let text = self
                    .source
                    .get(span.start as usize..span.end as usize)
                    .unwrap_or("");
                // Route to the deepest enclosing scope so per-space
                // JSON entries pick up the wrapper-text operand. The
                // tracker falls back to the unit `HalsteadBuilder` for
                // top-level member expressions.
                self.halstead_routing.observe_operand(
                    span.start,
                    span.end,
                    &mut self.stack[0].halstead,
                    HalsteadOperand {
                        kind: SmolStr::new("MemberExpression"),
                        text: Some(SmolStr::new(text)),
                    },
                );
            }
            _ => {}
        }

        // Track TypeScript-only AST subtrees so the post-walk token
        // sweep can skip tokens inside them. The pre-1.0 tree-sitter-
        // typescript classification doesn't include `type_identifier`,
        // `predefined_type`, etc. as operands; matching that means
        // suppressing tokens that fall inside type positions.
        //
        // Reference legacy operand list:
        // `crates/mehen-engine/src/legacy/getter.rs:132-134`
        // (`Identifier | NestedIdentifier | MemberExpression |
        // PropertyIdentifier | String | Number | …`). Notably absent:
        // `TypeIdentifier`, `PredefinedType`.
        match kind {
            // Type annotations — `: number`, `: Shape`, `Shape[]`, etc.
            AstKind::TSTypeAnnotation(_)
            // Type parameters — `<T>` and `T extends ...`.
            | AstKind::TSTypeParameterDeclaration(_)
            | AstKind::TSTypeParameterInstantiation(_)
            | AstKind::TSTypeParameter(_)
            // Heritage clauses — `class C implements Shape, Other`.
            | AstKind::TSClassImplements(_)
            // Type references — `Shape`, `Array<T>`, etc.
            | AstKind::TSTypeReference(_)
            // Specific type forms.
            | AstKind::TSUnionType(_)
            | AstKind::TSIntersectionType(_)
            | AstKind::TSArrayType(_)
            | AstKind::TSTupleType(_)
            | AstKind::TSConditionalType(_)
            | AstKind::TSIndexedAccessType(_)
            | AstKind::TSLiteralType(_)
            | AstKind::TSTypeLiteral(_)
            | AstKind::TSTypeOperator(_)
            | AstKind::TSParenthesizedType(_)
            | AstKind::TSFunctionType(_)
            | AstKind::TSConstructorType(_)
            | AstKind::TSTypePredicate(_)
            | AstKind::TSTypeQuery(_)
            | AstKind::TSImportType(_)
            | AstKind::TSMappedType(_)
            | AstKind::TSInferType(_)
            | AstKind::TSThisType(_)
            | AstKind::TSAnyKeyword(_)
            | AstKind::TSStringKeyword(_)
            | AstKind::TSNumberKeyword(_)
            | AstKind::TSBigIntKeyword(_)
            | AstKind::TSBooleanKeyword(_)
            | AstKind::TSSymbolKeyword(_)
            | AstKind::TSNullKeyword(_)
            | AstKind::TSUndefinedKeyword(_)
            | AstKind::TSObjectKeyword(_)
            | AstKind::TSVoidKeyword(_)
            | AstKind::TSIntrinsicKeyword(_)
            | AstKind::TSNeverKeyword(_)
            | AstKind::TSUnknownKeyword(_)
            | AstKind::TSTemplateLiteralType(_)
            | AstKind::TSQualifiedName(_)
            // Interface bodies and members live in TS type-only space.
            | AstKind::TSInterfaceBody(_)
            | AstKind::TSPropertySignature(_)
            | AstKind::TSMethodSignature(_)
            | AstKind::TSCallSignatureDeclaration(_)
            | AstKind::TSConstructSignatureDeclaration(_)
            | AstKind::TSIndexSignature(_)
            | AstKind::TSIndexSignatureName(_)
            // The interface declaration's `id` (the interface name) and
            // its `extends` clause are also TS-only.
            | AstKind::TSInterfaceHeritage(_) => {
                self.type_only_ranges.push(ast_kind_span(kind));
            }
            // Class name (`Class.id`) is a runtime binding — it IS
            // an operand. The interface name belongs to the type-only
            // declaration; the whole `TSInterfaceDeclaration` span is
            // already tracked above as a type-only range.
            AstKind::TSInterfaceDeclaration(itf) => {
                self.type_only_ranges.push(itf.span);
            }
            _ => {}
        }
    }

    fn leave_node(&mut self, kind: AstKind<'a>) {
        match kind {
            AstKind::IfStatement(_)
            | AstKind::ForStatement(_)
            | AstKind::ForInStatement(_)
            | AstKind::ForOfStatement(_)
            | AstKind::WhileStatement(_)
            | AstKind::DoWhileStatement(_)
            | AstKind::SwitchStatement(_)
            | AstKind::TryStatement(_)
            | AstKind::CatchClause(_)
            | AstKind::ConditionalExpression(_) => {
                self.cognitive.nesting = self.cognitive.nesting.saturating_sub(1);
            }
            _ => {}
        }
    }
}

impl<'a> Visitor<'a> {
    /// The *current* enclosing space kind — i.e. the top of the kinds
    /// stack (the just-opened space is the top; this returns the
    /// parent of any node about to be visited next).
    fn parent_kind_top(&self) -> SpaceKind {
        self.kinds.last().cloned().unwrap_or(SpaceKind::Unit)
    }
}

fn function_space_kind(f: &Function<'_>) -> SpaceKind {
    match f.r#type {
        FunctionType::FunctionDeclaration | FunctionType::FunctionExpression => {
            // Legacy `is_js_func` / `is_js_closure`: a FunctionExpression
            // is a Function when assigned-to-a-name (`var f = function(){}`,
            // `obj = { foo: function() {} }`, `let f = function(){}`)
            // and a Closure otherwise. We can't see ancestors here, so
            // approximate: `FunctionDeclaration` is always a Function;
            // a `FunctionExpression` with an `id` is also a Function;
            // anonymous `FunctionExpression` is a Closure. The
            // var-bound case (`const f = function() {}`) is detected
            // upstream and adjusted via `record_*` patching in
            // `visit_variable_declarator`.
            if matches!(f.r#type, FunctionType::FunctionDeclaration) || f.id.is_some() {
                SpaceKind::Function
            } else {
                SpaceKind::Closure
            }
        }
        FunctionType::TSDeclareFunction | FunctionType::TSEmptyBodyFunctionExpression => {
            SpaceKind::Function
        }
    }
}

fn method_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str().to_string()),
        PropertyKey::PrivateIdentifier(id) => Some(format!("#{}", id.name.as_str())),
        _ => None,
    }
}

fn method_is_public(method: &oxc_ast::ast::MethodDefinition<'_>) -> bool {
    if matches!(method.key, PropertyKey::PrivateIdentifier(_)) {
        return false;
    }
    !matches!(
        method.accessibility,
        Some(TSAccessibility::Private) | Some(TSAccessibility::Protected)
    )
}

fn ts_field_visibility(
    accessibility: Option<TSAccessibility>,
    is_private_identifier: bool,
) -> bool {
    if is_private_identifier {
        return false;
    }
    !matches!(
        accessibility,
        Some(TSAccessibility::Private) | Some(TSAccessibility::Protected)
    )
}

fn ast_kind_span(kind: AstKind<'_>) -> Span {
    use oxc_span::GetSpan;
    kind.span()
}

/// Spelling of a comparison operator for evidence reason codes
/// (`typescript.abc.condition.===`).
fn comparison_op_str(op: oxc_syntax::operator::BinaryOperator) -> &'static str {
    use oxc_syntax::operator::BinaryOperator::*;
    match op {
        Equality => "==",
        Inequality => "!=",
        StrictEquality => "===",
        StrictInequality => "!==",
        LessThan => "<",
        LessEqualThan => "<=",
        GreaterThan => ">",
        GreaterEqualThan => ">=",
        _ => "comparison",
    }
}

#[allow(dead_code)]
fn assignment_target_is_simple(_t: &AssignmentTarget<'_>) -> bool {
    true
}
