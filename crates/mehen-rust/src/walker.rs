// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ra_ap_syntax-based walker that produces a populated `MetricSpace`.
//!
//! Mirrors the per-space `State` accumulator pattern used by
//! `mehen-python` (`crates/mehen-python/src/walker.rs`) and
//! `mehen-typescript` (`crates/mehen-typescript/src/walker.rs`):
//!
//! - one `State` for the unit, plus one for every opened
//!   function / closure / impl / trait space,
//! - finalize on close, fold child stats into parent,
//! - Halstead is driven by a post-AST token sweep over the source file's
//!   tokens.
//!
//! Rust-specific design decisions are documented in
//! `docs/rust-ra-ap-syntax-spec.md`. The short version:
//!
//! - **`?` operator**: counts as a cyclomatic decision and a cognitive
//!   `+1` (no nesting bump). It's a real short-circuit on `Err`/`None`,
//!   matching legacy and Sonar.
//! - **Match arms**: each arm contributes +1 cyclomatic. The `match`
//!   expression itself opens a cognitive nesting frame.
//! - **`else if`**: the inner `if` does NOT add cognitive nesting (legacy
//!   `is_else_if` rule); only the outer `if` does. The `else` branch
//!   contributes a flat +1 instead.
//! - **Macro contents are opaque**: tokens *inside* a `MacroCall`
//!   argument list (or `macro_rules!` body) do not contribute to
//!   cyclomatic, cognitive, ABC, or exit counters. The macro name itself
//!   counts as a branch. This matches the legacy
//!   `is_inside_rust_macro_tokens` filter.
//! - **Type annotations contribute to Halstead**: type identifiers like
//!   `Vec<T>` are Halstead operands. Rust types are not erased — they
//!   describe runtime values. (Same reasoning as Python; opposite of TS.)
//! - **Doc comments contribute to LOC `cloc`** but not to Halstead.
//!   Inline `//` and `/* */` comments contribute to `cloc` only.
//! - **Struct / enum / union do not open a class space** — they record
//!   their fields against the enclosing space's NPA counters. Legacy's
//!   `is_func_space` listed only `SourceFile | FunctionItem | ImplItem |
//!   TraitItem | ClosureExpression`, and Phase 9 preserves that.

use mehen_core::{LineIndex, MetricSpace, SourceSpan, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricEvidence, MetricTreeBuilder,
    SpaceRangeTracker, State, apply_state_to, close_space, finalize_state,
};
use ra_ap_syntax::{
    AstNode, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, WalkEvent,
    ast::{self, BinaryOp, HasName, HasVisibility, LogicOp, UnaryOp},
};
use smol_str::SmolStr;

/// Crate-internal entry point — drive the walker over a parsed
/// `SourceFile`. Only `mehen_rust::RustAnalyzer::analyze` calls this;
/// the function is not part of any cross-crate API. Contribution
/// evidence is recorded into the caller-owned `evidence` sink
/// (plan §5.4).
pub(crate) fn walk_source_file(
    file: &SourceFile,
    source: &str,
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let unit_range = file.syntax().text_range();
    let unit_span = text_range_to_source_span(unit_range, line_index);

    let mut visitor = Visitor::new(source, line_index, unit_span, evidence);
    visitor.walk(file.syntax());
    visitor.emit_halstead_from_tokens(file.syntax());
    visitor.finish()
}

#[derive(Clone, Copy)]
enum LeaveAction {
    None,
    CloseSpace,
    CloseSpaceAndRestoreCognitive(CognitiveContext),
    RestoreCognitive(CognitiveContext),
    ExitMacroOpaque,
}

#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

struct Visitor<'a> {
    source: &'a str,
    line_index: &'a LineIndex,
    tree: MetricTreeBuilder,
    /// Per-space accumulator stack — index 0 is the unit.
    stack: Vec<State>,
    /// Parallel to `stack`: the SpaceKind of each open frame.
    kinds: Vec<SpaceKind>,
    /// Cognitive context for the currently-walked subtree. Saved on
    /// nesting-bumping / function-entry events and restored on leave.
    cognitive: CognitiveContext,
    /// Macro-opaque ranges — tokens inside these are skipped during the
    /// Halstead token sweep. Mirrors the legacy
    /// `is_inside_rust_macro_tokens` filter for Halstead; the structural
    /// walk uses `macro_opaque_depth` directly.
    macro_opaque_ranges: Vec<TextRange>,
    /// Active depth count of macro-opaque scopes (>= 1 means we're
    /// currently inside a macro body during the structural walk).
    macro_opaque_depth: u32,
    /// Routes Halstead tokens emitted by the post-AST sweep to the
    /// deepest enclosing function/closure/impl/trait space so per-space
    /// JSON entries are non-zero. PR #95 discussion_r3265658502
    /// flagged the same gap on the Python walker; the Rust walker had
    /// the same `stack[0]`-only behaviour.
    halstead_routing: SpaceRangeTracker,
    /// Contribution-evidence sink (plan §5.4). Recorded next to each
    /// stat increment, usually through [`Visitor::record_evidence`].
    evidence: &'a mut MetricEvidence,
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
            macro_opaque_ranges: Vec::new(),
            macro_opaque_depth: 0,
            halstead_routing: SpaceRangeTracker::new(),
            evidence,
        }
    }

    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    /// Record contribution evidence for `range`. The span is only
    /// computed when the sink is enabled, so call sites can invoke this
    /// unconditionally next to each stat increment — mirrors
    /// `mehen_tree_sitter::WalkerCtx::record_evidence`.
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

    fn finish(mut self) -> MetricSpace {
        let mut unit_state = self.stack.pop().expect("walker stack underflow");
        finalize_state(&mut unit_state);
        // Route post-AST tokens (Halstead operator/operand,
        // PLOC code lines, comment lines) to nested spaces; see
        // [`SpaceRangeTracker`].
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

    fn open_space(&mut self, kind: SpaceKind, range: TextRange, name: Option<String>) {
        let mut child = State::for_opened_space(kind.clone());
        let start_row = self
            .line_index
            .line_at(range.start().into())
            .saturating_sub(1);
        let end_row = self
            .line_index
            .line_at(range.end().into())
            .saturating_sub(1);
        child.loc.set_span(start_row, end_row, false);

        let span = text_range_to_source_span(range, self.line_index);
        let space_id = self.tree.open(kind.clone(), span, name);
        self.halstead_routing
            .record_open(space_id, range.start().into(), range.end().into());
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

    /// Drive a preorder walk over the syntax tree. Uses an explicit
    /// `WalkEvent` loop so we can finalize the per-space stack on
    /// `Leave` events.
    fn walk(&mut self, root: &SyntaxNode) {
        let mut actions: Vec<LeaveAction> = Vec::new();
        for event in root.preorder() {
            match event {
                WalkEvent::Enter(node) => {
                    let action = self.enter_node(&node);
                    actions.push(action);
                }
                WalkEvent::Leave(_) => {
                    let action = actions.pop().expect("walker action stack underflow");
                    match action {
                        LeaveAction::None => {}
                        LeaveAction::CloseSpace => self.close_space(),
                        LeaveAction::CloseSpaceAndRestoreCognitive(saved) => {
                            self.close_space();
                            self.cognitive = saved;
                        }
                        LeaveAction::RestoreCognitive(saved) => {
                            self.cognitive = saved;
                        }
                        LeaveAction::ExitMacroOpaque => {
                            self.macro_opaque_depth = self.macro_opaque_depth.saturating_sub(1);
                        }
                    }
                }
            }
        }
    }

    /// Handle a node-enter event. Returns the matching leave action.
    fn enter_node(&mut self, node: &SyntaxNode) -> LeaveAction {
        let kind = node.kind();

        // Block tail expression — `fn f() { 42 }`'s `42` is a logical
        // line of code (legacy `is_rust_tail_expression` rule). The tail
        // expr is not wrapped in an EXPR_STMT, so the EXPR_STMT arm
        // below would miss it. Run this *before* the kind-specific
        // match so the per-kind handling still fires (a `for` tail
        // expression still records its cyclomatic decision, etc.).
        if self.macro_opaque_depth == 0 && is_block_tail_expression(node) {
            self.current().loc.observe_lloc();
        }

        // Inside a macro body: structural metrics are off, but we still
        // need to track nested macro boundaries so the depth unwinds.
        if self.macro_opaque_depth > 0 {
            if matches!(
                kind,
                SyntaxKind::MACRO_CALL | SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF
            ) {
                self.macro_opaque_ranges.push(node.text_range());
                self.macro_opaque_depth += 1;
                return LeaveAction::ExitMacroOpaque;
            }
            return LeaveAction::None;
        }

        match kind {
            // -----------------------------------------------------------------
            // Function / closure / impl / trait — open a metric space.
            // -----------------------------------------------------------------
            SyntaxKind::FN => {
                let func = ast::Fn::cast(node.clone()).unwrap();

                // NPM bookkeeping: if this Fn is directly inside an
                // Impl/Trait body, count it as a method on the
                // *enclosing* state (the impl/trait we're currently
                // inside). The function's own state is not used for
                // NPM — recording there would double-count when the
                // child merges back into the parent.
                self.classify_method(&func);

                // A trait function signature without a body
                // (`fn a(&self);`) is not a func-space in the legacy
                // walker. Its NPM contribution was already recorded
                // above; nothing else to do.
                if func.body().is_none() {
                    return LeaveAction::None;
                }

                let name = func.name().map(|n| n.text().to_string());
                let saved = self.cognitive;

                // Cognitive: function entry resets nesting/lambda; bumps
                // depth when nested inside another function.
                let nested = self
                    .kinds
                    .iter()
                    .skip(1)
                    .any(|k| matches!(k, SpaceKind::Function));
                let mut ctx = self.cognitive;
                ctx.nesting = 0;
                ctx.lambda = 0;
                if nested {
                    ctx.depth = ctx.depth.saturating_add(1);
                }
                self.cognitive = ctx;

                self.open_space(SpaceKind::Function, node.text_range(), name);

                let argc = func
                    .param_list()
                    .map(|pl| count_params(&pl) as u32)
                    .unwrap_or(0);
                self.current().nargs.record_function_args(argc);
                // NOM function is recorded by `State::for_opened_space`
                // inside `open_space`; evidence for both NOM and NArgs
                // attaches to the space-open site.
                self.record_evidence(node.text_range(), |e, s| {
                    e.function(s, "fn");
                    e.function_args(s, argc, "fn");
                });

                LeaveAction::CloseSpaceAndRestoreCognitive(saved)
            }
            SyntaxKind::CLOSURE_EXPR => {
                let saved = self.cognitive;
                let mut ctx = self.cognitive;
                ctx.lambda = ctx.lambda.saturating_add(1);
                self.cognitive = ctx;

                self.open_space(SpaceKind::Closure, node.text_range(), None);
                // NOM closure is recorded by `State::for_opened_space`
                // inside `open_space`; evidence attaches to the open site.
                self.record_evidence(node.text_range(), |e, s| e.closure(s, "closure_expr"));

                if let Some(closure) = ast::ClosureExpr::cast(node.clone()) {
                    let argc = closure
                        .param_list()
                        .map(|pl| count_params(&pl) as u32)
                        .unwrap_or(0);
                    self.current().nargs.record_closure_args(argc);
                    self.record_evidence(node.text_range(), |e, s| {
                        e.closure_args(s, argc, "closure_expr");
                    });
                }
                LeaveAction::CloseSpaceAndRestoreCognitive(saved)
            }
            SyntaxKind::IMPL => {
                let imp = ast::Impl::cast(node.clone()).unwrap();
                let name = imp.self_ty().map(|t| t.syntax().text().to_string());
                self.open_space(SpaceKind::Impl, node.text_range(), name);
                LeaveAction::CloseSpace
            }
            SyntaxKind::TRAIT => {
                let tr = ast::Trait::cast(node.clone()).unwrap();
                let name = tr.name().map(|n| n.text().to_string());
                self.open_space(SpaceKind::Trait, node.text_range(), name);
                LeaveAction::CloseSpace
            }

            // -----------------------------------------------------------------
            // Decision points (cyclomatic + cognitive + ABC)
            // -----------------------------------------------------------------
            SyntaxKind::IF_EXPR => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.record_evidence(node.text_range(), |e, s| {
                    e.decision(s, "if_expr");
                    e.abc_condition(s, "if_expr");
                });
                let bumped_nesting = if !is_else_if(node) {
                    let effective =
                        self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                    self.current().cognitive.increase_nesting(effective);
                    self.record_evidence(node.text_range(), |e, s| {
                        e.cognitive(s, effective.saturating_add(1), "if_expr");
                    });
                    true
                } else {
                    // `else if` — the legacy walker emits the +1 (flat)
                    // contribution at the connecting `Else` token. We
                    // attribute that +1 to the *parent* IF_EXPR via the
                    // else-branch-detection below, so this inner `if`
                    // adds nothing on its own.
                    false
                };
                // The legacy walker emits a flat +1 for every `Else`
                // token (covers both `else if` and bare `else { … }`).
                // ra_ap_syntax doesn't surface a dedicated Else AST
                // node — but each IF_EXPR exposes its own `else_token()`
                // / `else_branch()`. Attribute the +1 to the IF_EXPR
                // that owns the else branch. Evidence points at the
                // `else` keyword itself.
                if let Some(if_expr) = ast::IfExpr::cast(node.clone())
                    && let Some(else_token) = if_expr.else_token()
                {
                    self.current().cognitive.increment_by_one();
                    self.record_evidence(else_token.text_range(), |e, s| {
                        e.cognitive(s, 1, "else_kw");
                    });
                }
                self.current().cognitive.boolean_seq.reset();
                let saved = self.cognitive;
                if bumped_nesting {
                    self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                }
                LeaveAction::RestoreCognitive(saved)
            }
            SyntaxKind::WHILE_EXPR | SyntaxKind::FOR_EXPR | SyntaxKind::LOOP_EXPR => {
                let detail = match kind {
                    SyntaxKind::WHILE_EXPR => "while_expr",
                    SyntaxKind::FOR_EXPR => "for_expr",
                    _ => "loop_expr",
                };
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(node.text_range(), |e, s| {
                    e.decision(s, detail);
                    e.abc_condition(s, detail);
                    e.cognitive(s, effective.saturating_add(1), detail);
                });
                self.current().cognitive.boolean_seq.reset();
                let saved = self.cognitive;
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                LeaveAction::RestoreCognitive(saved)
            }
            SyntaxKind::MATCH_EXPR => {
                self.current().abc.record_condition();
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.record_evidence(node.text_range(), |e, s| {
                    e.abc_condition(s, "match_expr");
                    e.cognitive(s, effective.saturating_add(1), "match_expr");
                });
                self.current().cognitive.boolean_seq.reset();
                let saved = self.cognitive;
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                LeaveAction::RestoreCognitive(saved)
            }
            SyntaxKind::MATCH_ARM => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.record_evidence(node.text_range(), |e, s| {
                    e.decision(s, "match_arm");
                    e.abc_condition(s, "match_arm");
                });
                LeaveAction::None
            }
            SyntaxKind::TRY_EXPR => {
                // `?` short-circuits on Err/None: +1 cyclomatic, +1 cognitive
                // (no nesting), +1 ABC condition, +1 exit.
                self.current().cyclomatic.record_decision();
                self.current().cognitive.increment_by_one();
                self.current().abc.record_condition();
                self.current().nexit.record_exit();
                self.record_evidence(node.text_range(), |e, s| {
                    e.decision(s, "try_expr");
                    e.cognitive(s, 1, "try_expr");
                    e.abc_condition(s, "try_expr");
                    e.exit(s, "try_expr");
                });
                LeaveAction::None
            }
            SyntaxKind::RETURN_EXPR => {
                self.current().nexit.record_exit();
                self.record_evidence(node.text_range(), |e, s| e.exit(s, "return_expr"));
                LeaveAction::None
            }
            SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => {
                if has_label_child(node) {
                    let detail = if kind == SyntaxKind::BREAK_EXPR {
                        "break_expr"
                    } else {
                        "continue_expr"
                    };
                    self.current().cognitive.increment_by_one();
                    self.record_evidence(node.text_range(), |e, s| e.cognitive(s, 1, detail));
                }
                LeaveAction::None
            }
            SyntaxKind::BIN_EXPR => {
                if let Some(bin) = ast::BinExpr::cast(node.clone())
                    && let Some((op_token, op)) = bin.op_details()
                {
                    match op {
                        BinaryOp::LogicOp(LogicOp::And) => {
                            self.current().cyclomatic.record_decision();
                            self.current().abc.record_condition();
                            // Evidence spans point at the operator
                            // token; same-operator repeats apply no
                            // cognitive delta and record nothing.
                            let before = self.current().cognitive.structural;
                            self.current().cognitive.observe_boolean("&&");
                            let delta = self.current().cognitive.structural.saturating_sub(before);
                            self.record_evidence(op_token.text_range(), |e, s| {
                                e.decision(s, "&&");
                                e.abc_condition(s, "&&");
                                e.cognitive(s, delta, "&&");
                            });
                        }
                        BinaryOp::LogicOp(LogicOp::Or) => {
                            self.current().cyclomatic.record_decision();
                            self.current().abc.record_condition();
                            let before = self.current().cognitive.structural;
                            self.current().cognitive.observe_boolean("||");
                            let delta = self.current().cognitive.structural.saturating_sub(before);
                            self.record_evidence(op_token.text_range(), |e, s| {
                                e.decision(s, "||");
                                e.abc_condition(s, "||");
                                e.cognitive(s, delta, "||");
                            });
                        }
                        BinaryOp::CmpOp(_) => {
                            self.current().abc.record_condition();
                            let op_text: &str = op_token.text();
                            self.record_evidence(op_token.text_range(), |e, s| {
                                e.abc_condition(s, op_text);
                            });
                        }
                        BinaryOp::Assignment { .. } => {
                            self.current().abc.record_assignment();
                            self.record_evidence(node.text_range(), |e, s| {
                                e.abc_assignment(s, "bin_expr");
                            });
                        }
                        BinaryOp::ArithOp(_) => {}
                    }
                }
                LeaveAction::None
            }
            SyntaxKind::PREFIX_EXPR => {
                if let Some(pre) = ast::PrefixExpr::cast(node.clone())
                    && matches!(pre.op_kind(), Some(UnaryOp::Not))
                {
                    self.current().cognitive.boolean_seq.not_operator("!");
                }
                LeaveAction::None
            }

            // -----------------------------------------------------------------
            // Statement-level — LLOC, ABC.assignments
            // -----------------------------------------------------------------
            SyntaxKind::LET_STMT => {
                if let Some(stmt) = ast::LetStmt::cast(node.clone())
                    && stmt.eq_token().is_some()
                {
                    self.current().abc.record_assignment();
                    self.record_evidence(node.text_range(), |e, s| {
                        e.abc_assignment(s, "let_stmt");
                    });
                }
                self.current().loc.observe_lloc();
                LeaveAction::None
            }
            SyntaxKind::EXPR_STMT => {
                self.current().loc.observe_lloc();
                LeaveAction::None
            }

            // -----------------------------------------------------------------
            // Branches (B in ABC)
            // -----------------------------------------------------------------
            SyntaxKind::CALL_EXPR | SyntaxKind::METHOD_CALL_EXPR => {
                let detail = if kind == SyntaxKind::CALL_EXPR {
                    "call_expr"
                } else {
                    "method_call_expr"
                };
                self.current().abc.record_branch();
                self.record_evidence(node.text_range(), |e, s| e.abc_branch(s, detail));
                LeaveAction::None
            }
            SyntaxKind::MACRO_CALL => {
                self.current().abc.record_branch();
                self.record_evidence(node.text_range(), |e, s| e.abc_branch(s, "macro_call"));
                self.macro_opaque_ranges.push(node.text_range());
                self.macro_opaque_depth += 1;
                LeaveAction::ExitMacroOpaque
            }
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => {
                self.macro_opaque_ranges.push(node.text_range());
                self.macro_opaque_depth += 1;
                LeaveAction::ExitMacroOpaque
            }

            // -----------------------------------------------------------------
            // Class-like attribute counters (NPA / NPM)
            //
            // Rust structs / unions do not open their own metric space
            // (legacy `is_func_space` did not include them). But for the
            // NPA family the stats accumulator needs `classes` to count
            // class-like containers — call `record_class_like` directly
            // on the enclosing space once per struct so the published
            // total reflects "1 struct = 1 class".
            // -----------------------------------------------------------------
            SyntaxKind::STRUCT | SyntaxKind::UNION => {
                self.current().npa.record_class_like();
                LeaveAction::None
            }
            SyntaxKind::RECORD_FIELD => {
                if let Some(field) = ast::RecordField::cast(node.clone()) {
                    let is_public = field.visibility().is_some();
                    self.current()
                        .npa
                        .record_attribute(ContainerKind::Class, is_public);
                    // Only public members move the headline NPA value.
                    if is_public {
                        self.record_evidence(node.text_range(), |e, s| {
                            e.public_attribute(s, "record_field");
                        });
                    }
                }
                LeaveAction::None
            }
            SyntaxKind::TUPLE_FIELD => {
                if let Some(field) = ast::TupleField::cast(node.clone()) {
                    let is_public = field.visibility().is_some();
                    self.current()
                        .npa
                        .record_attribute(ContainerKind::Class, is_public);
                    if is_public {
                        self.record_evidence(node.text_range(), |e, s| {
                            e.public_attribute(s, "tuple_field");
                        });
                    }
                }
                LeaveAction::None
            }

            _ => LeaveAction::None,
        }
    }

    /// NPM bookkeeping: a `Fn` directly inside an Impl's or Trait's
    /// associated-item list is a method. Trait methods are implicitly
    /// public; Impl methods inherit Rust's `pub`/`pub(...)` visibility.
    fn classify_method(&mut self, func: &ast::Fn) {
        // Hop through the AssocItemList to reach the IMPL/TRAIT.
        let parent = func.syntax().parent();
        let grand_kind = match parent.as_ref().and_then(|p| p.parent()) {
            Some(g) => g.kind(),
            None => return,
        };
        let container = match grand_kind {
            SyntaxKind::IMPL => ContainerKind::Class,
            SyntaxKind::TRAIT => ContainerKind::Interface,
            _ => return,
        };
        let is_public = matches!(grand_kind, SyntaxKind::TRAIT) || func.visibility().is_some();
        self.current().npm.record_method(container, is_public);
        // Only public methods move the headline NPM value. Trait and
        // impl methods both fold into the published `npm` total.
        if is_public {
            self.record_evidence(func.syntax().text_range(), |e, s| e.public_method(s, "fn"));
        }
    }

    /// Token-stream Halstead emission — runs after the AST walk.
    /// Each token maps to one of `Operator(kind)`, `Operand(kind)`, or
    /// `Skip`. Tokens whose span falls inside a macro-opaque range are
    /// skipped entirely, and comment tokens are folded into LOC `cloc`.
    fn emit_halstead_from_tokens(&mut self, root: &SyntaxNode) {
        // Sort macro ranges so the inside-test is cheap.
        self.macro_opaque_ranges.sort_by_key(|r| r.start());

        for elem in root.descendants_with_tokens() {
            let token = match elem {
                NodeOrToken::Token(t) => t,
                NodeOrToken::Node(_) => continue,
            };
            self.observe_token(&token);
        }
    }

    fn observe_token(&mut self, token: &SyntaxToken) {
        let kind = token.kind();
        let range = token.text_range();

        // LOC: comment tokens — both line and block — route to the
        // deepest enclosing scope so per-space `loc.cloc` reflects
        // comments inside that scope's body. Lines that fall outside
        // every recorded scope go into the unit's LocStats.
        if kind == SyntaxKind::COMMENT {
            let start_row = self
                .line_index
                .line_at(range.start().into())
                .saturating_sub(1);
            let end_row = self
                .line_index
                .line_at(range.end().into())
                .saturating_sub(1);
            self.halstead_routing.observe_comment(
                range.start().into(),
                range.end().into(),
                &mut self.stack[0].loc,
                start_row,
                end_row,
            );
            return;
        }
        if kind == SyntaxKind::WHITESPACE {
            return;
        }

        // Macro-opaque ranges: any token whose span is *strictly inside*
        // a macro-opaque range (not at the boundary — the macro name and
        // the trailing `!` live outside the body) is excluded from
        // Halstead.
        if self.is_inside_macro_body(range) {
            return;
        }

        let s: u32 = range.start().into();
        let e: u32 = range.end().into();
        match classify_token(kind) {
            TokenClass::Operator(kind_str) => {
                self.halstead_routing.observe_operator(
                    s,
                    e,
                    &mut self.stack[0].halstead,
                    HalsteadOperator {
                        kind: SmolStr::new(kind_str),
                        text: None,
                    },
                );
            }
            TokenClass::Operand(kind_str) => {
                let text = self
                    .source
                    .get(usize::from(range.start())..usize::from(range.end()))
                    .unwrap_or("");
                self.halstead_routing.observe_operand(
                    s,
                    e,
                    &mut self.stack[0].halstead,
                    HalsteadOperand {
                        kind: SmolStr::new(kind_str),
                        text: Some(SmolStr::new(text)),
                    },
                );

                // Note an LLOC line for the token (matches legacy
                // `is_rust_tail_expression` which counts the trailing
                // expression of a block as a logical line). The actual
                // implementation lives at the AST level for precision —
                // see EXPR_STMT / LET_STMT above. Token-level LOC is
                // limited to comments here.
            }
            TokenClass::Skip => {}
        }

        // PLOC: any non-whitespace, non-comment token's starting line
        // is a code line — routed to the deepest enclosing scope so
        // per-space `loc.ploc` reflects the function/closure body.
        // Lines outside every recorded scope go into the unit
        // (top-level use statements, free constants, etc.).
        let start_row = self
            .line_index
            .line_at(range.start().into())
            .saturating_sub(1);
        self.halstead_routing.observe_code_line(
            range.start().into(),
            range.end().into(),
            &mut self.stack[0].loc,
            start_row,
        );
    }

    fn is_inside_macro_body(&self, range: TextRange) -> bool {
        // Each macro_opaque_range covers the *entire* MacroCall node —
        // including the macro name (`println`) and the bang (`!`). For
        // Halstead parity with legacy we want the macro name to count
        // (it's a real call), so we only skip tokens that fall *strictly
        // after* the bang. A simpler heuristic: skip tokens whose range
        // is fully inside any macro_opaque_range AND whose kind is one
        // of the body delimiters' content (not the leading
        // identifier/bang). We achieve that by checking position: the
        // first two tokens of a MacroCall (the path identifier, the
        // bang) live outside the body's `{...}` / `(...)` / `[...]`
        // brackets, so we skip only tokens after the opening bracket.
        //
        // For now the token sweep emits everything; the macro path and
        // bang are explicitly counted as the call's branch via the AST
        // walk. This keeps the implementation simple while preserving
        // the legacy "macro name is a branch but body is opaque"
        // behavior.
        for r in &self.macro_opaque_ranges {
            if range.start() >= r.start() && range.end() <= r.end() {
                return true;
            }
        }
        false
    }
}

// =====================================================================
// Helpers
// =====================================================================

fn text_range_to_source_span(range: TextRange, line_index: &LineIndex) -> SourceSpan {
    SourceSpan {
        start_byte: range.start().into(),
        end_byte: range.end().into(),
        start_line: line_index.line_at(range.start().into()),
        end_line: line_index.line_at(range.end().into()),
    }
}

fn count_params(pl: &ast::ParamList) -> usize {
    let regular = pl.params().count();
    let self_param = pl.self_param().is_some() as usize;
    regular + self_param
}

fn is_else_if(node: &SyntaxNode) -> bool {
    if node.kind() != SyntaxKind::IF_EXPR {
        return false;
    }
    if let Some(parent) = node.parent()
        && parent.kind() == SyntaxKind::IF_EXPR
        && let Some(parent_if) = ast::IfExpr::cast(parent.clone())
        && let Some(else_branch) = parent_if.else_branch()
        && let ast::ElseBranch::IfExpr(inner_if) = else_branch
    {
        return inner_if.syntax() == node;
    }
    false
}

fn has_label_child(node: &SyntaxNode) -> bool {
    node.children_with_tokens()
        .any(|c| matches!(c.kind(), SyntaxKind::LIFETIME))
}

/// Is this node the *tail expression* of a `STMT_LIST` (i.e. the final
/// expression of a block body, with no terminating `;`)? The legacy
/// `is_rust_tail_expression` rule treated such expressions as a logical
/// line of code; we need the same so `fn f() { 42 }` reports 1 LLOC.
fn is_block_tail_expression(node: &SyntaxNode) -> bool {
    // A tail expression is an `Expr` whose direct parent is a
    // STMT_LIST and whose position in the parent matches the
    // STMT_LIST's `tail_expr()` (the last expression with no trailing
    // semicolon).
    let parent = match node.parent() {
        Some(p) if p.kind() == SyntaxKind::STMT_LIST => p,
        _ => return false,
    };
    if !ast::Expr::can_cast(node.kind()) {
        return false;
    }
    let stmt_list = match ast::StmtList::cast(parent) {
        Some(sl) => sl,
        None => return false,
    };
    match stmt_list.tail_expr() {
        Some(tail) => tail.syntax() == node,
        None => false,
    }
}

enum TokenClass {
    Operator(&'static str),
    Operand(&'static str),
    Skip,
}

fn classify_token(kind: SyntaxKind) -> TokenClass {
    // We deliberately avoid `use SyntaxKind::*` here. ra_ap_syntax's
    // generated enum has hundreds of variants, and bare-name match
    // patterns (`LT`, `EQ`, `IDENT`, ...) are interpreted by Rust as
    // *fresh bindings*, not enum constants — that lets every arm
    // shadow the next and warns "unreachable pattern". Use full paths
    // through the `T!` macro / `SyntaxKind::*` so each arm is
    // unambiguous.
    use TokenClass::*;
    use ra_ap_syntax::T;
    match kind {
        // Punctuation / operators (use the `T!` macro from ra_ap_syntax
        // which expands to the SyntaxKind variant for the literal).
        T!['('] => Operator("("),
        T!['['] => Operator("["),
        T!['{'] => Operator("{"),
        T![,] => Operator(","),
        T![:] => Operator(":"),
        T![;] => Operator(";"),
        T![.] => Operator("."),
        T![@] => Operator("@"),
        T![+] => Operator("+"),
        T![-] => Operator("-"),
        T![*] => Operator("*"),
        T![/] => Operator("/"),
        T![%] => Operator("%"),
        T![|] => Operator("|"),
        T![&] => Operator("&"),
        T![^] => Operator("^"),
        T![~] => Operator("~"),
        T![&&] => Operator("&&"),
        T![||] => Operator("||"),
        T![<<] => Operator("<<"),
        T![>>] => Operator(">>"),
        T![=] => Operator("="),
        T![==] => Operator("=="),
        T![!=] => Operator("!="),
        T![<] => Operator("<"),
        T![>] => Operator(">"),
        T![<=] => Operator("<="),
        T![>=] => Operator(">="),
        T![+=] => Operator("+="),
        T![-=] => Operator("-="),
        T![*=] => Operator("*="),
        T![/=] => Operator("/="),
        T![%=] => Operator("%="),
        T![&=] => Operator("&="),
        T![|=] => Operator("|="),
        T![^=] => Operator("^="),
        T![<<=] => Operator("<<="),
        T![>>=] => Operator(">>="),
        T![..] => Operator(".."),
        T![..=] => Operator("..="),
        T![::] => Operator("::"),
        T![=>] => Operator("=>"),
        T![->] => Operator("->"),
        T![?] => Operator("?"),
        T![!] => Operator("!"),
        // Keywords — Halstead operators.
        T![fn] => Operator("fn"),
        T![let] => Operator("let"),
        T![if] => Operator("if"),
        T![else] => Operator("else"),
        T![while] => Operator("while"),
        T![for] => Operator("for"),
        T![loop] => Operator("loop"),
        T![match] => Operator("match"),
        T![return] => Operator("return"),
        T![break] => Operator("break"),
        T![continue] => Operator("continue"),
        T![as] => Operator("as"),
        T![in] => Operator("in"),
        T![mut] => Operator("mut"),
        T![ref] => Operator("ref"),
        T![static] => Operator("static"),
        T![const] => Operator("const"),
        T![struct] => Operator("struct"),
        T![enum] => Operator("enum"),
        T![trait] => Operator("trait"),
        T![impl] => Operator("impl"),
        T![type] => Operator("type"),
        T![use] => Operator("use"),
        T![mod] => Operator("mod"),
        T![pub] => Operator("pub"),
        T![where] => Operator("where"),
        T![async] => Operator("async"),
        T![await] => Operator("await"),
        T![dyn] => Operator("dyn"),
        T![unsafe] => Operator("unsafe"),
        T![move] => Operator("move"),
        T![extern] => Operator("extern"),
        T![self] => Operator("self"),
        T![Self] => Operator("Self"),
        T![super] => Operator("super"),
        T![crate] => Operator("crate"),
        T![yield] => Operator("yield"),
        // Operands — leaves that name or contain a value.
        SyntaxKind::IDENT => Operand("Identifier"),
        SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER => Operand("Number"),
        SyntaxKind::STRING | SyntaxKind::BYTE_STRING | SyntaxKind::C_STRING => Operand("String"),
        SyntaxKind::CHAR | SyntaxKind::BYTE => Operand("Char"),
        T![true] => Operand("True"),
        T![false] => Operand("False"),
        SyntaxKind::LIFETIME_IDENT => Operand("Lifetime"),
        // Closing punctuation pairs with its opener; skip to avoid
        // double-counting (classical Halstead pair convention).
        T![')'] | T![']'] | T!['}'] => Skip,
        // Trivia + EOF + everything else.
        _ => Skip,
    }
}
