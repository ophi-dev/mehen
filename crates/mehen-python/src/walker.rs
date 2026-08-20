// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Ruff AST + token-stream walker that produces a populated `MetricSpace`.
//!
//! Recursion is driven by ruff's
//! [`SourceOrderVisitor`](ruff_python_ast::visitor::source_order::SourceOrderVisitor):
//! we override the per-shape hooks where a metric side effect or a
//! lifecycle boundary (open/close space, push/pop cognitive context) is
//! required, and let the default `walk_*` helpers handle the rest of the
//! descent. The walker follows the same per-space `State` accumulator
//! pattern used by `mehen-typescript` and `mehen-php`: one `State` per
//! opened space, finalize on close, fold child stats into parent.
//!
//! Python-specific design decisions are documented in
//! `docs/python-ruff-spec.md`. The short version:
//!
//! - **Type annotations and default-value expressions**: in Python, type
//!   annotations are runtime-accessible objects (Pydantic, dataclasses,
//!   `typing.get_type_hints`, etc.). Tokens inside an annotation
//!   subtree are treated like ordinary tokens for Halstead — they ARE
//!   operands and operators in the running program. This is a deliberate
//!   semantic difference from `mehen-typescript`, where TS-only type
//!   metadata is excluded because TS types are erased at runtime.
//!
//! - **Docstrings**: a string literal that is the *first statement of a
//!   module / class / function body* is a docstring per PEP 257 — a
//!   structural language feature, not arbitrary code. We do not emit
//!   Halstead operators/operands for docstring tokens, but the LOC
//!   accounting still counts those lines as `cloc` (comment-like) per
//!   the legacy convention.
//!
//! - **`match`/`case`**: every `case` clause is a cyclomatic decision
//!   point and a cognitive nesting bump. The Python `match` statement
//!   is a structural pattern match, so each case is a real branch.
//!
//! - **Exception groups (`try*`/`except*`)**: an `except*` handler still
//!   counts the same as a regular `except` — both add a decision and a
//!   nesting level; the underlying `is_star: bool` flag on `StmtTry` is
//!   noted for evidence but does not change the metric output.

use mehen_core::{LineIndex, MetricSpace, SourceSpan, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricEvidence, MetricTreeBuilder,
    SpaceRangeTracker, State, apply_state_to, close_space, finalize_state,
};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    self as ast, BoolOp, Comprehension, ElifElseClause, ExceptHandler, Expr, MatchCase, ModModule,
    Stmt, UnaryOp,
};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};
use smol_str::SmolStr;

/// Drive the walker over a parsed Python module. Crate-internal entry
/// point — only `mehen_python::PythonAnalyzer::analyze` calls this; not
/// part of any cross-crate API. Contribution evidence is recorded into
/// the caller-owned `evidence` sink (plan §5.4).
pub(crate) fn walk_module(
    parsed: &Parsed<ModModule>,
    source: &str,
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let module = parsed.syntax();
    let unit_span = SourceSpan {
        start_byte: module.range.start().to_u32(),
        end_byte: module.range.end().to_u32(),
        start_line: line_index.line_at(module.range.start().to_u32()),
        end_line: line_index.line_at(module.range.end().to_u32()),
    };

    let mut visitor = Visitor::new(source, line_index, unit_span, evidence);
    visitor.record_module_docstring(&module.body);
    visitor.visit_body(&module.body);

    visitor.emit_halstead_from_tokens(parsed.tokens());

    visitor.finish()
}

struct Visitor<'a> {
    source: &'a str,
    line_index: &'a LineIndex,
    tree: MetricTreeBuilder,
    /// Per-space accumulator stack — index 0 is the unit.
    stack: Vec<State>,
    /// Parallel to `stack`: the SpaceKind of each frame so we can tell
    /// "what's the enclosing class-like" without re-walking.
    kinds: Vec<SpaceKind>,
    /// Cognitive context inherited down the recursion. Mirrors the
    /// legacy `(nesting, depth, lambda)` triple from
    /// `mehen-engine/src/legacy/metrics/cognitive.rs::python` —
    /// Python increments `lambda` only inside an `ExprLambda` and the
    /// boolean-sequence reset rules apply to expression statements.
    cognitive: CognitiveContext,
    /// Byte ranges of nodes whose tokens should NOT contribute to the
    /// Halstead token sweep. Currently this is only docstring spans
    /// (per PEP 257). Type annotation spans are NOT added here because
    /// Python types are runtime-accessible — see crate docs.
    docstring_ranges: Vec<TextRange>,
    /// Routes Halstead tokens emitted by the post-AST sweep to the
    /// deepest enclosing function/class/lambda space. Without this,
    /// nested-scope Halstead numbers are zero in the per-space JSON
    /// even though the unit rollup is correct (PR #95
    /// discussion_r3265658502).
    halstead_routing: SpaceRangeTracker,
    /// Contribution-evidence sink (plan §5.4). Record methods are
    /// no-ops when disabled; call sites go through
    /// [`Visitor::record_evidence`] so spans are only computed when
    /// the sink is enabled.
    evidence: &'a mut MetricEvidence,
}

#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
    /// Depth of nested `BoolOp` expressions. Used by the BoolOp handler
    /// to detect the *outermost* boolean operator inside a statement —
    /// only that one gets the legacy "lambda ancestor" bonus
    /// (mehen-engine cognitive.rs:281 `count_specific_ancestors` to
    /// detect outermost-up-to-Lambda-boundary).
    bool_op_depth: u32,
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
            tree: MetricTreeBuilder::new(unit_span),
            stack: vec![state],
            kinds: vec![SpaceKind::Unit],
            cognitive: CognitiveContext::default(),
            docstring_ranges: Vec::new(),
            halstead_routing: SpaceRangeTracker::new(),
            evidence,
        }
    }

    /// Record contribution evidence for `range`. The span is only
    /// computed when the sink is enabled, so call sites can invoke
    /// this unconditionally next to each stat increment (mirrors
    /// `WalkerCtx::record_evidence` in `mehen-tree-sitter`).
    #[inline]
    fn record_evidence<F>(&mut self, range: TextRange, record: F)
    where
        F: FnOnce(&mut MetricEvidence, SourceSpan),
    {
        if self.evidence.is_enabled() {
            let span = text_range_to_source_span(range, self.line_index);
            record(self.evidence, span);
        }
    }

    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    fn parent_kind(&self) -> SpaceKind {
        self.kinds.last().cloned().unwrap_or(SpaceKind::Unit)
    }

    fn finish(mut self) -> MetricSpace {
        let mut unit_state = self.stack.pop().expect("walker stack underflow");
        finalize_state(&mut unit_state);
        // Route post-AST tokens (Halstead operator/operand events,
        // PLOC code-lines, comment lines) to nested spaces. The unit
        // builder + LocStats are taken out of `unit_state` so the
        // tracker can accumulate fall-through events into them; the
        // routing pass also propagates each child's counts up the
        // parent chain so the unit ends up with the file-wide rollup.
        let mut unit_halstead = std::mem::take(&mut unit_state.halstead);
        let mut unit_loc = std::mem::take(&mut unit_state.loc);
        let mut tree = self.tree.finish();
        self.halstead_routing
            .finalize_into_tree(&mut tree, &mut unit_halstead, &mut unit_loc);
        unit_state.halstead = unit_halstead;
        unit_state.loc = unit_loc;
        // Re-run the unit publish so its Halstead, LOC, and MI keys
        // reflect the rolled-up values that include token-driven
        // events routed to nested scopes.
        apply_state_to(unit_state, &mut tree.metrics);
        tree
    }

    fn record_module_docstring(&mut self, body: &[Stmt]) {
        if let Some(span) = leading_docstring_range(body) {
            self.docstring_ranges.push(span);
        }
    }

    fn open_space(&mut self, kind: SpaceKind, range: TextRange, name: Option<String>) {
        let mut child = State::for_opened_space(kind.clone());
        let start_row = self
            .line_index
            .line_at(range.start().to_u32())
            .saturating_sub(1);
        let end_row = self
            .line_index
            .line_at(range.end().to_u32())
            .saturating_sub(1);
        child.loc.set_span(start_row, end_row, false);

        let span = text_range_to_source_span(range, self.line_index);
        let space_id = self.tree.open(kind.clone(), span, name);
        // Record the byte range so the post-AST Halstead token sweep
        // can route tokens to this scope.
        self.halstead_routing
            .record_open(space_id, range.start().to_u32(), range.end().to_u32());
        self.stack.push(child);
        self.kinds.push(kind);
    }

    fn close_space(&mut self) {
        close_space(
            &mut self.stack,
            &mut self.kinds,
            &mut self.tree,
            &mut self.halstead_routing,
        );
    }

    fn enter_function(&mut self, func: &'a ast::StmtFunctionDef) {
        // Python decorators: each `@decorator` is in itself an extra
        // expression that runs at definition time. The legacy walker
        // records them as part of the enclosing space's metric stream
        // (decorators land in the *enclosing* class/unit). We follow
        // that by visiting decorators *before* opening the function
        // space.
        for decorator in &func.decorator_list {
            self.visit_expr(&decorator.expression);
        }

        self.open_space(
            SpaceKind::Function,
            func.range,
            Some(func.name.id.as_str().to_string()),
        );
        let argc = func.parameters.len() as u32;
        self.current().nargs.record_function_args(argc);
        // NOM is recorded inside `State::for_opened_space(Function)`
        // (called by `open_space`); the evidence for both families is
        // attached here, at the site that opened the space.
        self.record_evidence(func.range, |e, s| e.function(s, "stmt_function_def"));
        self.record_evidence(func.range, |e, s| {
            e.function_args(s, argc, "stmt_function_def")
        });

        // Cognitive — function entry resets nesting/lambda and bumps
        // depth when nested inside another function.
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

        // Walk parameters (defaults / annotations contribute Halstead /
        // ABC against the function's own state — Python evaluates these
        // at definition time but they belong to the function's
        // signature). The default `visit_parameters` walks defaults +
        // annotations through `visit_expr` — exactly what we want.
        self.visit_parameters(&func.parameters);

        // Capture the leading docstring so it does not contribute to
        // Halstead via the token sweep.
        if let Some(span) = leading_docstring_range(&func.body) {
            self.docstring_ranges.push(span);
        }

        self.visit_body(&func.body);

        self.cognitive = saved;
        self.close_space();
    }

    fn enter_class(&mut self, class: &'a ast::StmtClassDef) {
        for decorator in &class.decorator_list {
            self.visit_expr(&decorator.expression);
        }
        // Class arguments (base classes, metaclass=...) live in the
        // enclosing scope, not in the class body — they execute at
        // definition time.
        if let Some(args) = class.arguments.as_deref() {
            self.visit_arguments(args);
        }

        self.open_space(
            SpaceKind::Class,
            class.range,
            Some(class.name.id.as_str().to_string()),
        );

        if let Some(span) = leading_docstring_range(&class.body) {
            self.docstring_ranges.push(span);
        }

        for stmt in &class.body {
            // Class-body assignments — `name: T = value` (StmtAnnAssign)
            // and `name = value` (StmtAssign with bare-identifier
            // target) — count as class attributes (NPA). Method-style
            // `def f(self):` inside a class body counts as a method
            // (NPM). We classify here because the AnnAssign / Assign
            // context (top-level of class body) matters.
            self.classify_class_body_member(stmt);
            self.visit_stmt(stmt);
        }

        self.close_space();
    }

    fn classify_class_body_member(&mut self, stmt: &Stmt) {
        let parent = self.parent_kind();
        if !matches!(
            parent,
            SpaceKind::Class | SpaceKind::Impl | SpaceKind::Interface | SpaceKind::Trait
        ) {
            return;
        }
        match stmt {
            Stmt::AnnAssign(ast::StmtAnnAssign { target, .. }) => {
                if let Expr::Name(name) = target.as_ref() {
                    let is_public = python_attribute_is_public(name.id.as_str());
                    self.current()
                        .npa
                        .record_attribute(ContainerKind::Class, is_public);
                    // NPA headline counts public members only — evidence
                    // follows suit and skips non-public names.
                    if is_public {
                        self.record_evidence(name.range, |e, s| {
                            e.public_attribute(s, "stmt_ann_assign");
                        });
                    }
                }
            }
            Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                for tgt in targets {
                    if let Expr::Name(name) = tgt {
                        let is_public = python_attribute_is_public(name.id.as_str());
                        self.current()
                            .npa
                            .record_attribute(ContainerKind::Class, is_public);
                        if is_public {
                            self.record_evidence(name.range, |e, s| {
                                e.public_attribute(s, "stmt_assign");
                            });
                        }
                    }
                }
            }
            Stmt::FunctionDef(ast::StmtFunctionDef { name, .. }) => {
                let is_public = python_method_is_public(name.id.as_str());
                self.current()
                    .npm
                    .record_method(ContainerKind::Class, is_public);
                if is_public {
                    self.record_evidence(stmt.range(), |e, s| {
                        e.public_method(s, "stmt_function_def");
                    });
                }
            }
            _ => {}
        }
    }

    fn enter_lambda(&mut self, lam: &'a ast::ExprLambda) {
        self.open_space(SpaceKind::Closure, lam.range, None);
        let argc = lam
            .parameters
            .as_deref()
            .map(|p| p.len() as u32)
            .unwrap_or(0);
        self.current().nargs.record_closure_args(argc);
        // NOM is recorded inside `State::for_opened_space(Closure)`
        // (called by `open_space`); evidence attaches here.
        self.record_evidence(lam.range, |e, s| e.closure(s, "expr_lambda"));
        self.record_evidence(lam.range, |e, s| e.closure_args(s, argc, "expr_lambda"));

        let mut ctx = self.cognitive;
        ctx.lambda = ctx.lambda.saturating_add(1);
        let saved = self.cognitive;
        self.cognitive = ctx;

        if let Some(params) = lam.parameters.as_deref() {
            self.visit_parameters(params);
        }
        self.visit_expr(&lam.body);

        self.cognitive = saved;
        self.close_space();
    }

    fn observe_loc_for_stmt(&mut self, stmt: &Stmt) {
        let range = stmt.range();
        let start_row = self
            .line_index
            .line_at(range.start().to_u32())
            .saturating_sub(1);
        let cur = self.current();
        // LLOC: only "actionable" statements. Container statements
        // (`def`, `class`) are not LLOC per the legacy
        // `legacy/metrics/loc.rs::PythonCode::compute` enumeration —
        // their bodies contain the lloc-bumping nodes. Match expr and
        // bare keywords (`pass`/`break`/etc.) are still bumped via the
        // generic `Stmt::*` arm.
        let is_lloc = !matches!(
            stmt,
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) | Stmt::TypeAlias(_)
        );
        if is_lloc {
            cur.loc.observe_lloc();
        }
        cur.loc.observe_code_line(start_row);
    }

    /// Token-stream Halstead emission — runs after the AST walk.
    ///
    /// Each Ruff token is mapped to one of `Operator(kind)`,
    /// `Operand(kind)`, or `Skip`. Tokens whose span falls inside a
    /// recorded docstring are skipped entirely. Type annotations and
    /// default values are NOT skipped — Python types are runtime
    /// objects, not erased metadata.
    fn emit_halstead_from_tokens(&mut self, tokens: &ruff_python_ast::token::Tokens) {
        // Sort docstring ranges so a binary scan is cheap.
        self.docstring_ranges.sort_by_key(|r| r.start());

        for tok in tokens.iter() {
            let span = tok.range();

            // LOC: comment tokens contribute to `cloc`. Routed to the
            // deepest enclosing scope so per-space `loc.cloc` reflects
            // comments inside that scope's body; lines outside every
            // recorded scope go into the unit. The legacy walker
            // (`legacy/metrics/loc.rs::PythonCode::compute`) matched
            // the `Comment` node and called `add_cloc_lines`; the
            // equivalent here is `observe_comment` (Python comments
            // are always single-line).
            if matches!(tok.kind(), TokenKind::Comment) {
                let start_row = self
                    .line_index
                    .line_at(span.start().to_u32())
                    .saturating_sub(1);
                let end_row = self
                    .line_index
                    .line_at(span.end().to_u32())
                    .saturating_sub(1);
                self.halstead_routing.observe_comment(
                    span.start().to_u32(),
                    span.end().to_u32(),
                    &mut self.stack[0].loc,
                    start_row,
                    end_row,
                );
            }

            // Module-level docstrings are PEP 257 documentation, so
            // their tokens are excluded from Halstead. The legacy
            // walker also folded triple-quoted module/class/function
            // docstrings into `cloc` via the `String` arm — apply the
            // same here so cloc covers both `# …` line comments and
            // top-of-body docstrings. Routed by span so a function's
            // docstring lands on its space's `loc.cloc`.
            if self.is_inside_docstring(span)
                && matches!(
                    tok.kind(),
                    TokenKind::String | TokenKind::FStringStart | TokenKind::FStringEnd
                )
            {
                let start_row = self
                    .line_index
                    .line_at(span.start().to_u32())
                    .saturating_sub(1);
                let end_row = self
                    .line_index
                    .line_at(span.end().to_u32())
                    .saturating_sub(1);
                self.halstead_routing.observe_comment(
                    span.start().to_u32(),
                    span.end().to_u32(),
                    &mut self.stack[0].loc,
                    start_row,
                    end_row,
                );
            }

            if self.is_inside_docstring(span) {
                continue;
            }
            // Route Halstead events to the deepest enclosing
            // function/class/lambda space so per-space JSON entries
            // are non-zero (and the rolled-up unit values match).
            // `route_through_tracker` falls back to the unit
            // `HalsteadBuilder` when no recorded entry covers the
            // token.
            let s = span.start().to_u32();
            let e = span.end().to_u32();
            match classify_token(tok.kind()) {
                TokenClass::Operator(kind) => {
                    self.halstead_routing.observe_operator(
                        s,
                        e,
                        &mut self.stack[0].halstead,
                        HalsteadOperator {
                            kind: SmolStr::new(kind),
                            text: None,
                        },
                    );
                }
                TokenClass::Operand(kind) => {
                    let text = self.source.get(s as usize..e as usize).unwrap_or("");
                    self.halstead_routing.observe_operand(
                        s,
                        e,
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

    fn is_inside_docstring(&self, span: TextRange) -> bool {
        self.docstring_ranges
            .iter()
            .any(|r| span.start() >= r.start() && span.end() <= r.end())
    }
}

impl<'a> SourceOrderVisitor<'a> for Visitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        // LOC accounting — every statement bumps lloc, and its starting
        // line is a code line.
        self.observe_loc_for_stmt(stmt);

        match stmt {
            Stmt::FunctionDef(func) => {
                self.enter_function(func);
            }
            Stmt::ClassDef(class) => {
                self.enter_class(class);
            }
            Stmt::If(ast::StmtIf {
                test,
                body,
                elif_else_clauses,
                ..
            }) => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.record_evidence(stmt.range(), |e, s| e.decision(s, "stmt_if"));
                self.record_evidence(stmt.range(), |e, s| e.abc_condition(s, "stmt_if"));
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(stmt.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "stmt_if");
                });
                // Match legacy `increase_nesting` (mehen-engine cognitive.rs:239):
                // a new control-flow scope resets the boolean sequence so two
                // sibling `if a and b: ...` blocks each contribute +1 for their
                // own `and`, instead of collapsing into a single same-op run.
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                self.visit_expr(test);
                self.visit_body(body);
                // Elif/else clauses inherit the nesting bump from their
                // owning `if` (legacy walks them as children of `if_statement`,
                // so they see the parent's nesting via the nesting map).
                // Keep `cognitive.nesting` raised while walking them.
                for clause in elif_else_clauses {
                    self.visit_elif_else_clause(clause);
                }
                self.cognitive.nesting -= 1;
            }
            Stmt::For(ast::StmtFor {
                target,
                iter,
                body,
                orelse,
                ..
            }) => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.record_evidence(stmt.range(), |e, s| e.decision(s, "stmt_for"));
                self.record_evidence(stmt.range(), |e, s| e.abc_condition(s, "stmt_for"));
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(stmt.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "stmt_for");
                });
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                self.visit_expr(target);
                self.visit_expr(iter);
                self.visit_body(body);
                self.cognitive.nesting -= 1;
                if !orelse.is_empty() {
                    // `for ... else` — the else-branch runs only if
                    // the loop completed without `break`. Legacy treats
                    // the else-clause as +1 cyclomatic (a real branch
                    // that depends on `break` not firing).
                    self.current().cyclomatic.record_decision();
                    self.current().cognitive.increment_by_one();
                    self.current().abc.record_condition();
                    if let Some(range) = body_range(orelse) {
                        self.record_evidence(range, |e, s| e.decision(s, "for_else"));
                        self.record_evidence(range, |e, s| e.cognitive(s, 1, "for_else"));
                        self.record_evidence(range, |e, s| e.abc_condition(s, "for_else"));
                    }
                    self.visit_body(orelse);
                }
            }
            Stmt::While(ast::StmtWhile {
                test, body, orelse, ..
            }) => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.record_evidence(stmt.range(), |e, s| e.decision(s, "stmt_while"));
                self.record_evidence(stmt.range(), |e, s| e.abc_condition(s, "stmt_while"));
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(stmt.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "stmt_while");
                });
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                self.visit_expr(test);
                self.visit_body(body);
                self.cognitive.nesting -= 1;
                if !orelse.is_empty() {
                    self.current().cyclomatic.record_decision();
                    self.current().cognitive.increment_by_one();
                    self.current().abc.record_condition();
                    if let Some(range) = body_range(orelse) {
                        self.record_evidence(range, |e, s| e.decision(s, "while_else"));
                        self.record_evidence(range, |e, s| e.cognitive(s, 1, "while_else"));
                        self.record_evidence(range, |e, s| e.abc_condition(s, "while_else"));
                    }
                    self.visit_body(orelse);
                }
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                // `try` itself is a +1 nesting bump (cognitive only —
                // legacy does not count the bare `try` for cyclomatic
                // because the decision is in the handler). The `try`
                // raises the nesting level for its body AND its
                // except / else / finally branches (siblings in the
                // Ruff AST, but children of the `try_statement` in
                // tree-sitter — both should see the same nesting).
                self.current().abc.record_condition();
                self.record_evidence(stmt.range(), |e, s| e.abc_condition(s, "stmt_try"));
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(stmt.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "stmt_try");
                });
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                self.visit_body(body);
                for handler in handlers {
                    self.visit_except_handler(handler);
                }
                if !orelse.is_empty() {
                    self.current().cognitive.increment_by_one();
                    if let Some(range) = body_range(orelse) {
                        self.record_evidence(range, |e, s| e.cognitive(s, 1, "try_else"));
                    }
                    self.visit_body(orelse);
                }
                if !finalbody.is_empty() {
                    self.current().cognitive.increment_by_one();
                    if let Some(range) = body_range(finalbody) {
                        self.record_evidence(range, |e, s| e.cognitive(s, 1, "try_finally"));
                    }
                    self.visit_body(finalbody);
                }
                self.cognitive.nesting -= 1;
            }
            Stmt::Match(ast::StmtMatch { subject, cases, .. }) => {
                // `match` itself does not increment cyclomatic — each
                // `case` does. ABC records `match` as a condition once
                // (the match itself is a structural branch).
                self.current().abc.record_condition();
                self.record_evidence(stmt.range(), |e, s| e.abc_condition(s, "stmt_match"));
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(stmt.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "stmt_match");
                });
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                self.visit_expr(subject);
                for case in cases {
                    self.visit_match_case(case);
                }
                self.cognitive.nesting -= 1;
            }
            Stmt::With(ast::StmtWith { items, body, .. }) => {
                // `with` is not a cyclomatic decision (no branching),
                // but it does add cognitive nesting (a structural
                // scope) and one ABC condition equivalent.
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(stmt.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "stmt_with");
                });
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                for item in items {
                    self.visit_expr(&item.context_expr);
                    if let Some(opt_vars) = &item.optional_vars {
                        self.visit_expr(opt_vars);
                    }
                }
                self.visit_body(body);
                self.cognitive.nesting -= 1;
            }
            Stmt::Return(ast::StmtReturn { value, .. }) => {
                self.current().nexit.record_exit();
                self.record_evidence(stmt.range(), |e, s| e.exit(s, "stmt_return"));
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            Stmt::Raise(ast::StmtRaise { exc, cause, .. }) => {
                self.current().nexit.record_exit();
                self.record_evidence(stmt.range(), |e, s| e.exit(s, "stmt_raise"));
                if let Some(e) = exc {
                    self.visit_expr(e);
                }
                if let Some(c) = cause {
                    self.visit_expr(c);
                }
            }
            Stmt::Assign(_) => {
                self.current().abc.record_assignment();
                self.record_evidence(stmt.range(), |e, s| e.abc_assignment(s, "stmt_assign"));
                walk_stmt(self, stmt);
            }
            Stmt::AugAssign(_) => {
                self.current().abc.record_assignment();
                self.record_evidence(stmt.range(), |e, s| e.abc_assignment(s, "stmt_aug_assign"));
                walk_stmt(self, stmt);
            }
            Stmt::AnnAssign(ast::StmtAnnAssign { value, .. }) => {
                if value.is_some() {
                    self.current().abc.record_assignment();
                    self.record_evidence(stmt.range(), |e, s| {
                        e.abc_assignment(s, "stmt_ann_assign");
                    });
                }
                walk_stmt(self, stmt);
            }
            Stmt::Expr(_) => {
                // ExpressionStatement resets the boolean sequence (for
                // cognitive complexity boolean-chain folding).
                self.current().cognitive.boolean_seq.reset();
                walk_stmt(self, stmt);
            }
            Stmt::TypeAlias(_) => {
                // `type X = Y` (PEP 695 type alias). The target is an
                // assignment — count it once.
                self.current().abc.record_assignment();
                self.record_evidence(stmt.range(), |e, s| e.abc_assignment(s, "stmt_type_alias"));
                walk_stmt(self, stmt);
            }
            // Plain descent — defaults handle the children we'd visit
            // anyway. Statements with no decision/assignment side
            // effect: bare keywords (`pass`/`break`/`continue`),
            // imports, name declarations (`global`/`nonlocal`), `del`,
            // `assert`, IPython escape commands.
            Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Pass(_)
            | Stmt::Global(_)
            | Stmt::Nonlocal(_)
            | Stmt::Import(_)
            | Stmt::ImportFrom(_)
            | Stmt::Delete(_)
            | Stmt::Assert(_)
            | Stmt::IpyEscapeCommand(_) => {
                walk_stmt(self, stmt);
            }
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                let label = match op {
                    BoolOp::And => "and",
                    BoolOp::Or => "or",
                };
                // Boolean `and` / `or` — each operand beyond the first
                // is one decision point per legacy. Evidence spans point
                // at the extra operand (Ruff's AST has no operator-token
                // node to anchor to).
                for value in values.iter().skip(1) {
                    self.current().cyclomatic.record_decision();
                    self.current().abc.record_condition();
                    self.record_evidence(value.range(), |e, s| e.decision(s, label));
                    self.record_evidence(value.range(), |e, s| e.abc_condition(s, label));
                }
                // Lambda-ancestor bonus (legacy cognitive.rs:281): the
                // *outermost* BoolOp inside a statement adds one structural
                // unit per enclosing lambda. Inside `bar = lambda a:
                // lambda b: b or True or True`, the `or` sequence sits
                // inside two lambdas — legacy adds +2 to the per-space
                // structural before the boolean-sequence collapser.
                let lambda_bonus = if self.cognitive.bool_op_depth == 0 {
                    self.cognitive.lambda
                } else {
                    0
                };
                if lambda_bonus > 0 {
                    self.current().cognitive.record_increment(lambda_bonus);
                    self.record_evidence(expr.range(), |e, s| {
                        e.cognitive(s, lambda_bonus, "bool_op_lambda_bonus");
                    });
                }
                let before = self.current().cognitive.structural;
                self.current().cognitive.observe_boolean(label);
                let delta = self.current().cognitive.structural.saturating_sub(before);
                self.record_evidence(expr.range(), |e, s| e.cognitive(s, delta, label));
                self.cognitive.bool_op_depth = self.cognitive.bool_op_depth.saturating_add(1);
                for v in values {
                    self.visit_expr(v);
                }
                self.cognitive.bool_op_depth = self.cognitive.bool_op_depth.saturating_sub(1);
            }
            Expr::Named(_) => {
                self.current().abc.record_assignment();
                self.record_evidence(expr.range(), |e, s| e.abc_assignment(s, "expr_named"));
                walk_expr(self, expr);
            }
            Expr::UnaryOp(ast::ExprUnaryOp { op, .. }) => {
                if matches!(op, UnaryOp::Not) {
                    self.current().cognitive.boolean_seq.not_operator("not");
                }
                walk_expr(self, expr);
            }
            Expr::Lambda(lam) => {
                self.enter_lambda(lam);
            }
            Expr::If(ast::ExprIf {
                test, body, orelse, ..
            }) => {
                // Conditional expression `a if b else c` — one decision.
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.record_evidence(expr.range(), |e, s| e.decision(s, "expr_if"));
                self.record_evidence(expr.range(), |e, s| e.abc_condition(s, "expr_if"));
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(expr.range(), |e, s| {
                    e.cognitive(s, effective.saturating_add(1), "expr_if");
                });
                self.current().cognitive.boolean_seq.reset();
                self.cognitive.nesting += 1;
                self.visit_expr(test);
                self.visit_expr(body);
                self.visit_expr(orelse);
                self.cognitive.nesting -= 1;
            }
            Expr::Compare(ast::ExprCompare { comparators, .. }) => {
                // Comparison ops (`==`, `<`, ...) — each pair counts as
                // one ABC condition. Evidence spans point at the
                // right-hand comparator of each pair.
                for comparator in comparators.iter() {
                    self.current().abc.record_condition();
                    self.record_evidence(comparator.range(), |e, s| {
                        e.abc_condition(s, "expr_compare");
                    });
                }
                walk_expr(self, expr);
            }
            Expr::Call(_) => {
                self.current().abc.record_branch();
                self.record_evidence(expr.range(), |e, s| e.abc_branch(s, "expr_call"));
                walk_expr(self, expr);
            }
            // Halstead-wise, `a.b` is two operand tokens (`a` and `b`)
            // plus one operator (`.`) — exactly what the lexer emits.
            // We do NOT emit an extra "attribute" operand for the
            // joined chain text; doing so would triple-count the same
            // syntactic structure (legacy Python tree-sitter walker
            // also did not emit such an entry, see
            // `crates/mehen-engine/src/legacy/getter.rs` — `Attribute`
            // is not in the operand match arms). The default
            // `walk_expr` descends into `value` and visits the `attr`
            // identifier as a no-op, which is exactly what we want.
            //
            // Everything else (BinOp, Subscript/Starred, Tuple/List/
            // Set/Slice/Dict, comprehensions, Await/Yield, FString/
            // TString, atomic literals, Name) is structural-only —
            // defaults give us the same recursion we used to do
            // manually.
            _ => walk_expr(self, expr),
        }
    }

    fn visit_elif_else_clause(&mut self, clause: &'a ElifElseClause) {
        // Elif: +1 cyclomatic (the chained condition is a real branch),
        // +1 cognitive (no nesting bump — its cost is paid by the outer
        // `if`), reset the boolean sequence.
        // Else: +1 cognitive only — no cyclomatic increment because the
        // else branch isn't a separate decision (the if already picked
        // a branch).
        if clause.test.is_some() {
            self.current().cyclomatic.record_decision();
            self.current().cognitive.increment_by_one();
            self.current().cognitive.boolean_seq.reset();
            self.current().abc.record_condition();
            self.record_evidence(clause.range, |e, s| e.decision(s, "elif_clause"));
            self.record_evidence(clause.range, |e, s| e.cognitive(s, 1, "elif_clause"));
            self.record_evidence(clause.range, |e, s| e.abc_condition(s, "elif_clause"));
        } else {
            self.current().cognitive.increment_by_one();
            self.current().abc.record_condition();
            self.record_evidence(clause.range, |e, s| e.cognitive(s, 1, "else_clause"));
            self.record_evidence(clause.range, |e, s| e.abc_condition(s, "else_clause"));
        }
        if let Some(test) = &clause.test {
            self.visit_expr(test);
        }
        self.visit_body(&clause.body);
    }

    fn visit_except_handler(&mut self, handler: &'a ExceptHandler) {
        let ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler { type_, body, .. }) =
            handler;
        self.current().cyclomatic.record_decision();
        self.current().abc.record_condition();
        self.record_evidence(handler.range(), |e, s| e.decision(s, "except_handler"));
        self.record_evidence(handler.range(), |e, s| e.abc_condition(s, "except_handler"));
        let effective = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
        self.current().cognitive.increase_nesting(effective);
        self.record_evidence(handler.range(), |e, s| {
            e.cognitive(s, effective.saturating_add(1), "except_handler");
        });
        self.current().cognitive.boolean_seq.reset();
        self.cognitive.nesting += 1;
        if let Some(t) = type_ {
            self.visit_expr(t);
        }
        self.visit_body(body);
        self.cognitive.nesting -= 1;
    }

    fn visit_match_case(&mut self, case: &'a MatchCase) {
        self.current().cyclomatic.record_decision();
        self.current().abc.record_condition();
        self.record_evidence(case.range, |e, s| e.decision(s, "match_case"));
        self.record_evidence(case.range, |e, s| e.abc_condition(s, "match_case"));
        let effective = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
        self.current().cognitive.increase_nesting(effective);
        self.record_evidence(case.range, |e, s| {
            e.cognitive(s, effective.saturating_add(1), "match_case");
        });
        self.current().cognitive.boolean_seq.reset();
        self.cognitive.nesting += 1;
        // We deliberately do NOT call `self.visit_pattern(&case.pattern)` —
        // pattern bindings have no metric impact (legacy didn't count them
        // either). Guards and bodies do.
        if let Some(g) = &case.guard {
            self.visit_expr(g);
        }
        self.visit_body(&case.body);
        self.cognitive.nesting -= 1;
    }

    fn visit_comprehension(&mut self, comp: &'a Comprehension) {
        // A comprehension's first generator is +1 cyclomatic (the
        // implicit `for`); each `if` filter is also +1.
        self.current().cyclomatic.record_decision();
        self.current().abc.record_condition();
        self.record_evidence(comp.range, |e, s| e.decision(s, "comprehension"));
        self.record_evidence(comp.range, |e, s| e.abc_condition(s, "comprehension"));
        for f in &comp.ifs {
            self.current().cyclomatic.record_decision();
            self.current().abc.record_condition();
            self.record_evidence(f.range(), |e, s| e.decision(s, "comprehension_if"));
            self.record_evidence(f.range(), |e, s| e.abc_condition(s, "comprehension_if"));
        }
        self.visit_expr(&comp.target);
        self.visit_expr(&comp.iter);
        for f in &comp.ifs {
            self.visit_expr(f);
        }
    }
}

fn text_range_to_source_span(range: TextRange, line_index: &LineIndex) -> SourceSpan {
    SourceSpan {
        start_byte: range.start().to_u32(),
        end_byte: range.end().to_u32(),
        start_line: line_index.line_at(range.start().to_u32()),
        end_line: line_index.line_at(range.end().to_u32()),
    }
}

/// Byte range covering a non-empty statement body (first statement's
/// start to last statement's end). Used to anchor evidence for
/// clause-shaped increments that have no node of their own in the Ruff
/// AST (`for`/`while`/`try` else-branches, `finally` bodies).
fn body_range(body: &[Stmt]) -> Option<TextRange> {
    let first = body.first()?;
    let last = body.last()?;
    Some(TextRange::new(first.range().start(), last.range().end()))
}

fn leading_docstring_range(body: &[Stmt]) -> Option<TextRange> {
    let first = body.first()?;
    if let Stmt::Expr(ast::StmtExpr { value, .. }) = first
        && let Expr::StringLiteral(s) = value.as_ref()
    {
        return Some(s.range);
    }
    None
}

fn python_method_is_public(name: &str) -> bool {
    if name.starts_with("__") && name.ends_with("__") {
        return true;
    }
    !name.starts_with('_')
}

fn python_attribute_is_public(name: &str) -> bool {
    if name.starts_with("__") && name.ends_with("__") {
        return true;
    }
    !name.starts_with('_')
}

enum TokenClass {
    Operator(&'static str),
    Operand(&'static str),
    Skip,
}

fn classify_token(kind: TokenKind) -> TokenClass {
    use TokenClass::*;
    use TokenKind::*;
    match kind {
        // Operators — punctuation that reads as `do something`.
        Lpar => Operator("("),
        Lsqb => Operator("["),
        Lbrace => Operator("{"),
        Comma => Operator(","),
        Colon => Operator(":"),
        Semi => Operator(";"),
        Dot => Operator("."),
        At => Operator("@"),
        Plus => Operator("+"),
        Minus => Operator("-"),
        Star => Operator("*"),
        Slash => Operator("/"),
        Percent => Operator("%"),
        Vbar => Operator("|"),
        Amper => Operator("&"),
        CircumFlex => Operator("^"),
        Tilde => Operator("~"),
        DoubleStar => Operator("**"),
        DoubleSlash => Operator("//"),
        LeftShift => Operator("<<"),
        RightShift => Operator(">>"),
        Less => Operator("<"),
        Greater => Operator(">"),
        Equal => Operator("="),
        EqEqual => Operator("=="),
        NotEqual => Operator("!="),
        LessEqual => Operator("<="),
        GreaterEqual => Operator(">="),
        PlusEqual => Operator("+="),
        MinusEqual => Operator("-="),
        StarEqual => Operator("*="),
        SlashEqual => Operator("/="),
        PercentEqual => Operator("%="),
        AmperEqual => Operator("&="),
        VbarEqual => Operator("|="),
        CircumflexEqual => Operator("^="),
        DoubleStarEqual => Operator("**="),
        DoubleSlashEqual => Operator("//="),
        LeftShiftEqual => Operator("<<="),
        RightShiftEqual => Operator(">>="),
        ColonEqual => Operator(":="),
        AtEqual => Operator("@="),
        Rarrow => Operator("->"),
        // Keywords — these are Halstead operators.
        And => Operator("and"),
        Or => Operator("or"),
        Not => Operator("not"),
        If => Operator("if"),
        Elif => Operator("elif"),
        Else => Operator("else"),
        For => Operator("for"),
        While => Operator("while"),
        Try => Operator("try"),
        Except => Operator("except"),
        Finally => Operator("finally"),
        With => Operator("with"),
        Return => Operator("return"),
        Raise => Operator("raise"),
        Yield => Operator("yield"),
        Assert => Operator("assert"),
        Import => Operator("import"),
        From => Operator("from"),
        As => Operator("as"),
        Pass => Operator("pass"),
        Break => Operator("break"),
        Continue => Operator("continue"),
        Def => Operator("def"),
        Class => Operator("class"),
        Lambda => Operator("lambda"),
        In => Operator("in"),
        Is => Operator("is"),
        Async => Operator("async"),
        Await => Operator("await"),
        Global => Operator("global"),
        Nonlocal => Operator("nonlocal"),
        Del => Operator("del"),
        // Soft keywords (`match`, `case`, `type`, `_`-as-pattern) read
        // as operators when they head a statement.
        Match => Operator("match"),
        Case => Operator("case"),
        Type => Operator("type"),
        // Operands — leaves that name or contain a value.
        Name => Operand("Identifier"),
        Int | Float | Complex => Operand("Number"),
        String | FStringStart | FStringMiddle | FStringEnd | TStringStart | TStringMiddle
        | TStringEnd => Operand("String"),
        True => Operand("True"),
        False => Operand("False"),
        None => Operand("None"),
        Ellipsis => Operand("Ellipsis"),
        // Closing punctuation, newlines, indents, and comments are not
        // counted (closing `)` etc. would double-count alongside their
        // opening counterpart — Halstead's classical formula counts
        // brackets *as a pair*, with the open bracket as the operator
        // and the close as a no-op).
        Rpar | Rsqb | Rbrace => Skip,
        Newline | NonLogicalNewline | Indent | Dedent | EndOfFile | Comment | Question
        | Exclamation | Lazy | Unknown | IpyEscapeCommand => Skip,
    }
}
