// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Reusable walker scaffolding for tree-sitter-backed analyzers.
//!
//! Each language analyzer crate plugs its decision/operator/operand rules
//! into this walker; the shared bookkeeping (`MetricTreeBuilder` updates,
//! per-space accumulator stack, LOC line classification at unit and span
//! level, Halstead event emission, MI/Halstead/LOC publishing into the
//! `MetricSet`) lives here so language crates focus on their own syntax.
//!
//! The walker is intentionally minimal: it does not implement cognitive
//! nesting state machines or context-sensitive Halstead classification.
//! Those live in the owning language crate and are wired in via the
//! [`LanguageRules`] trait.

use mehen_core::{LineIndex, MetricSpace, SourceSpan, SpaceId, SpaceKind};
use mehen_metrics::{
    MetricEvidence, MetricTreeBuilder, State, apply_state_to, finalize_state,
    merge_child_into_parent,
};
use tree_sitter::Node;

use crate::span::node_span;

/// What a language reports about an AST node.
///
/// This is the "language interpretation" surface from the rewrite plan
/// §5.2: each language crate decides which constructs are decisions,
/// operators, operands, exits, etc. The shared walker accumulates them.
#[derive(Default, Clone, Debug)]
pub struct NodeFacts {
    /// Counts toward cyclomatic complexity (`if`, `for`, `&&`, …).
    pub cyclomatic_decision: bool,
    /// Cognitive-complexity contribution for this node, per Sonar's
    /// whitepaper. See [`CognitiveFact`] for the variants.
    pub cognitive: CognitiveFact,
    /// Halstead operator with the node kind as its key.
    pub halstead_operator: bool,
    /// Halstead operand with the node text as its dedup key.
    pub halstead_operand: bool,
    /// Counts toward NExit (`return`, `throw`, `raise`, …).
    pub nexit: bool,
    /// Counts toward ABC's `B` (branches: function calls, `goto`, …).
    pub abc_branch: bool,
    /// Counts toward ABC's `C` (conditionals: comparisons, boolean ops).
    pub abc_condition: bool,
    /// Counts toward ABC's `A` (assignments).
    pub abc_assignment: bool,
    /// LOC classification of this node. See [`LocFact`].
    pub loc: LocFact,
}

/// Cognitive-complexity classification of an AST node, mirroring the
/// pre-1.0 per-language `Cognitive::compute` arms.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum CognitiveFact {
    /// No cognitive contribution.
    #[default]
    None,
    /// Nesting-increasing construct: adds `nesting + 1` to the
    /// structural count and bumps the nesting depth for the descendant
    /// nodes (e.g. `if`, `for`, `while`, `switch`, `catch`, ternary).
    IncreaseNesting,
    /// Same-level conditional clause: adds `1` without bumping nesting
    /// (e.g. `else`, `elseif`, `finally`, `trap`). Also resets the
    /// boolean-sequence tracker.
    NonNestingPlusOne,
    /// Boolean operator leaf — feed the BoolSequence collapser. The
    /// payload is a stable identifier (e.g. `"-and"`, `"-or"`,
    /// `"&&"`).
    BooleanOperator(smol_str::SmolStr),
    /// Unary negation operator — set the BoolSequence's last_op
    /// without bumping structural so a leading `!`/`-not` doesn't
    /// trick the collapser.
    NotOperator(smol_str::SmolStr),
    /// Statement boundary — reset the BoolSequence so chained
    /// operators don't bleed across statements.
    StatementBoundary,
    /// Statement boundary that also feeds a list of boolean operators
    /// found among the node's direct children (e.g. PowerShell's
    /// `pipeline` may carry `pipeline_chain_tail` children with
    /// `&&` / `||`). Each operator runs through the BoolSequence
    /// collapser in order.
    StatementBoundaryWithBooleans(Vec<smol_str::SmolStr>),
    /// Container that feeds a list of boolean operators to the
    /// BoolSequence collapser (e.g. PowerShell's `logical_expression`
    /// holding `-and` / `-or` / `-xor` leaves). Does NOT reset the
    /// sequence first — the wrapper node itself is not a statement
    /// boundary.
    BooleanContainer(Vec<smol_str::SmolStr>),
    /// Function-depth marker — reset the structural-nesting context
    /// (the function's own body restarts at nesting=0). Used by
    /// `function_statement` / `class_method_definition` etc.
    FunctionEntry,
    /// Closure / lambda marker — bump the lambda counter (which feeds
    /// `nesting` for descendants).
    LambdaEntry,
}

/// LOC family classification: how this node contributes to the LOC
/// suite (PLOC / LLOC / CLOC). Mirrors the pre-1.0 per-language
/// `Loc::compute` match arms (`src/metrics/loc.rs`).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocFact {
    /// Node is a container (statement list, block, parameter list,
    /// string interior, …) and must NOT contribute to PLOC. The walker
    /// ignores this node for LOC purposes but still recurses into it.
    Container,
    /// Node is a statement-shaped construct that bumps LLOC by one.
    /// Statement classification is per-language (e.g. `pipeline`,
    /// `if_statement`, `function_statement` for PowerShell;
    /// `expression_statement`, `function_definition` for PHP).
    Lloc,
    /// Node is a comment. The walker uses `node.start_row()` /
    /// `node.end_row()` to update CLOC (distinguishing comment-on-code
    /// vs. independent-line comments per the legacy algorithm).
    Comment,
    /// Default: any other node — its `start_row()` is added to the
    /// PLOC line set.
    #[default]
    Code,
}

/// What kind of scope a node opens, if any. `None` means the node is not
/// itself a space boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeOpen {
    /// Open a space with the given kind. `name` may be `None` if the
    /// language can't extract one cheaply.
    Open {
        kind: SpaceKind,
        name: Option<String>,
    },
}

/// Trait implemented by each language analyzer to plug into the shared
/// walker.
pub trait LanguageRules {
    /// Returns the node kind names that mark a space boundary, alongside
    /// the kind to open. The walker consults this on the way down.
    fn scope_for(&self, node: &Node<'_>, source: &[u8]) -> Option<ScopeOpen>;

    /// Classify a single AST node for shared metrics.
    fn classify(&self, node: &Node<'_>) -> NodeFacts;

    /// Count the number of arguments declared by the function or
    /// closure rooted at `node`. Default: zero. Each language overrides
    /// to count its own parameter-list shape (PowerShell:
    /// `parameter_list > script_parameter`; PHP / TS: `formal_parameters
    /// > parameter`; Python: `parameters > identifier`; etc.).
    fn count_args(&self, _node: &Node<'_>, _source: &[u8]) -> u32 {
        0
    }

    /// Classify a node as a class attribute (NPA) inside a class-like
    /// container, and decide its visibility. Default: not an attribute.
    /// Languages override to recognize their property/field syntax.
    fn classify_attribute(&self, _node: &Node<'_>, _source: &[u8]) -> Option<MemberClassification> {
        None
    }

    /// Classify a node as a class method (NPM) inside a class-like
    /// container, and decide its visibility. Default: not a method.
    /// Languages override to recognize their method declaration syntax.
    fn classify_method(&self, _node: &Node<'_>, _source: &[u8]) -> Option<MemberClassification> {
        None
    }
}

/// Classification of a class-or-interface member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberClassification {
    pub container: mehen_metrics::ContainerKind,
    pub is_public: bool,
}

/// The result of running [`walk`] on a tree.
pub struct WalkResult {
    pub root: MetricSpace,
}

/// Walk `root_node` over `source_text` using `rules` and produce a
/// populated `MetricSpace` tree.
///
/// Generic over the rules so monomorphization gives each language its own
/// fast path. The walker:
/// 1. opens a Unit space at the root,
/// 2. on every node, asks `rules.scope_for(...)`; if it opens a scope it
///    pushes a fresh `State`,
/// 3. on every node, asks `rules.classify(...)` and accumulates the
///    facts into the current `State`,
/// 4. classifies every physical line of the source for the unit-level
///    `loc`, and every line covered by an opened scope for that scope's
///    `loc`,
/// 5. on close, publishes the per-space metric set via the shared
///    [`apply_state_to`] helper, then folds it back into the parent.
///
/// Because every fact flows through this one function, contribution
/// evidence (plan §5.4) is recorded centrally: the caller passes a
/// [`MetricEvidence`] sink and each applied fact records a span +
/// reason derived from the node kind. The caller finishes the sink to
/// obtain the contributions.
pub fn walk<R: LanguageRules>(
    root_node: Node<'_>,
    source_text: &[u8],
    line_index: &LineIndex,
    rules: &R,
    evidence: &mut MetricEvidence,
) -> WalkResult {
    let unit_span = node_span(&root_node, line_index);
    let mut walker = Walker {
        tree: MetricTreeBuilder::new(unit_span),
        source_text,
        line_index,
        stack: vec![State::new()],
        kinds: vec![SpaceKind::Unit],
        rules,
        evidence,
    };
    // The unit space's LOC span covers the full source.
    walker.stack[0].loc.set_span(
        root_node.start_position().row as u32,
        root_node.end_position().row as u32,
        true,
    );
    walker.visit(root_node);
    let mut unit_state = walker.stack.pop().expect("walker stack underflow");
    finalize_state(&mut unit_state);
    apply_state_to(unit_state, walker.tree.metrics_mut());
    WalkResult {
        root: walker.tree.finish(),
    }
}

struct Walker<'a, R: LanguageRules> {
    tree: MetricTreeBuilder,
    source_text: &'a [u8],
    line_index: &'a LineIndex,
    stack: Vec<State>,
    /// Per-stack-frame `SpaceKind` so child code can ask "what's my
    /// enclosing container?" without re-walking the parser tree. Same
    /// length as `stack`; index 0 is the unit.
    kinds: Vec<SpaceKind>,
    rules: &'a R,
    evidence: &'a mut MetricEvidence,
}

/// Per-node cognitive-complexity context threaded through the walker
/// recursion. Mirrors the pre-1.0 `(nesting, depth, lambda)` triple
/// stored in `nesting_map[NodeId]`. `nesting + depth + lambda` is the
/// effective nesting level when an `IncreaseNesting` node is observed.
#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

impl<'a, R: LanguageRules> Walker<'a, R> {
    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    fn visit(&mut self, node: Node<'_>) {
        self.visit_with_ctx(node, CognitiveContext::default());
    }

    fn visit_with_ctx(&mut self, node: Node<'_>, mut ctx: CognitiveContext) {
        let opened_kind = match self.rules.scope_for(&node, self.source_text) {
            Some(ScopeOpen::Open { kind, name }) => {
                let span = node_span(&node, self.line_index);
                let mut child_state = State::new();
                // The child space's LOC span is the AST node's row range.
                // Non-unit spaces use the `+1` convention (counts the
                // function-signature line as part of the space).
                child_state.loc.set_span(
                    node.start_position().row as u32,
                    node.end_position().row as u32,
                    false,
                );
                // Per pre-1.0 `Nom::compute`: when the walker enters a
                // function/closure space, the *child* state owns the
                // increment — its own `functions`/`closures` count
                // includes itself. The unit space and class spaces
                // intentionally do not self-count.
                match kind {
                    SpaceKind::Function => {
                        child_state.nom.record_function();
                        let count = self.rules.count_args(&node, self.source_text);
                        child_state.nargs.record_function_args(count);
                        if self.evidence.is_enabled() {
                            self.evidence.function(span, node.kind());
                            self.evidence.function_args(span, count, node.kind());
                        }
                    }
                    SpaceKind::Closure => {
                        child_state.nom.record_closure();
                        let count = self.rules.count_args(&node, self.source_text);
                        child_state.nargs.record_closure_args(count);
                        if self.evidence.is_enabled() {
                            self.evidence.closure(span, node.kind());
                            self.evidence.closure_args(span, count, node.kind());
                        }
                    }
                    SpaceKind::Class | SpaceKind::Impl => {
                        // Mark NPA / NPM / WMC as having seen a class-like
                        // space so they're emitted (vs. omitted) at the
                        // unit level.
                        child_state.npa.record_class_like();
                        child_state.npm.record_class_like();
                        child_state.wmc.record_class_like();
                    }
                    SpaceKind::Interface | SpaceKind::Trait => {
                        child_state.npa.record_class_like();
                        child_state.npm.record_class_like();
                    }
                    _ => {}
                }
                self.tree.open(kind.clone(), span, name);
                self.stack.push(child_state);
                self.kinds.push(kind.clone());
                Some(kind)
            }
            None => None,
        };
        let opened_space = opened_kind.is_some();

        let facts = self.rules.classify(&node);
        // Contribution evidence (plan §5.4): one span per node, computed
        // only when the sink is enabled and a fact that can move a
        // metric actually fired. Detail codes reuse the tree-sitter
        // node kind, so reasons read `powershell.cyclomatic.if_statement`.
        let ev_span = if self.evidence.is_enabled()
            && (facts.cyclomatic_decision
                || facts.nexit
                || facts.abc_branch
                || facts.abc_condition
                || facts.abc_assignment
                || !matches!(facts.cognitive, CognitiveFact::None))
        {
            Some(node_span(&node, self.line_index))
        } else {
            None
        };
        if facts.cyclomatic_decision {
            self.current().cyclomatic.record_decision();
            if let Some(span) = ev_span {
                self.evidence.decision(span, node.kind());
            }
        }
        // Cognitive — drive the per-node state machine. The walker
        // tracks `(nesting, depth, lambda)` via `ctx`, threaded through
        // the recursion. See [`CognitiveFact`] for variant semantics.
        // Evidence amounts are the structural delta each variant
        // actually applied — boolean-run bookkeeping that doesn't move
        // the metric records nothing.
        match &facts.cognitive {
            CognitiveFact::None => {}
            CognitiveFact::IncreaseNesting => {
                let effective_nesting = ctx.nesting + ctx.depth + ctx.lambda;
                self.current().cognitive.increase_nesting(effective_nesting);
                ctx.nesting += 1;
                if let Some(span) = ev_span {
                    self.evidence
                        .cognitive(span, effective_nesting.saturating_add(1), node.kind());
                }
            }
            CognitiveFact::NonNestingPlusOne => {
                self.current().cognitive.increment_by_one();
                self.current().cognitive.boolean_seq.reset();
                if let Some(span) = ev_span {
                    self.evidence.cognitive(span, 1, node.kind());
                }
            }
            CognitiveFact::BooleanOperator(op) => {
                let before = self.current().cognitive.structural;
                self.current().cognitive.observe_boolean(op.as_str());
                let delta = self.current().cognitive.structural.saturating_sub(before);
                if let Some(span) = ev_span {
                    self.evidence.cognitive(span, delta, node.kind());
                }
            }
            CognitiveFact::NotOperator(op) => {
                self.current()
                    .cognitive
                    .boolean_seq
                    .not_operator(op.as_str());
            }
            CognitiveFact::StatementBoundary => {
                self.current().cognitive.boolean_seq.reset();
            }
            CognitiveFact::StatementBoundaryWithBooleans(ops) => {
                self.current().cognitive.boolean_seq.reset();
                let before = self.current().cognitive.structural;
                for op in ops {
                    self.current().cognitive.observe_boolean(op.as_str());
                }
                let delta = self.current().cognitive.structural.saturating_sub(before);
                if let Some(span) = ev_span {
                    self.evidence.cognitive(span, delta, node.kind());
                }
            }
            CognitiveFact::BooleanContainer(ops) => {
                let before = self.current().cognitive.structural;
                for op in ops {
                    self.current().cognitive.observe_boolean(op.as_str());
                }
                let delta = self.current().cognitive.structural.saturating_sub(before);
                if let Some(span) = ev_span {
                    self.evidence.cognitive(span, delta, node.kind());
                }
            }
            CognitiveFact::FunctionEntry => {
                // Mirrors the pre-1.0 `increment_function_depth_any`:
                // depth bumps only when the entered function is nested
                // inside *another* function/method. Detect this by
                // counting the enclosing `Function` spaces in `kinds`
                // (excluding the just-pushed self).
                let nested_inside_function = self
                    .kinds
                    .iter()
                    .rev()
                    .skip(1) // skip the just-opened self
                    .any(|k| matches!(k, SpaceKind::Function));
                ctx.nesting = 0;
                ctx.lambda = 0;
                if nested_inside_function {
                    ctx.depth = ctx.depth.saturating_add(1);
                }
            }
            CognitiveFact::LambdaEntry => {
                ctx.lambda = ctx.lambda.saturating_add(1);
            }
        }
        if facts.halstead_operator {
            let kind = node.kind();
            self.current()
                .halstead
                .observe_operator(mehen_metrics::HalsteadOperator {
                    kind: kind.into(),
                    text: None,
                });
        }
        if facts.halstead_operand {
            let kind = node.kind();
            let text = crate::span::text_of(&node, self.source_text);
            self.current()
                .halstead
                .observe_operand(mehen_metrics::HalsteadOperand {
                    kind: kind.into(),
                    text: Some(text.into()),
                });
        }
        if facts.nexit {
            self.current().nexit.record_exit();
            if let Some(span) = ev_span {
                self.evidence.exit(span, node.kind());
            }
        }
        if facts.abc_branch {
            self.current().abc.record_branch();
            if let Some(span) = ev_span {
                self.evidence.abc_branch(span, node.kind());
            }
        }
        if facts.abc_condition {
            self.current().abc.record_condition();
            if let Some(span) = ev_span {
                self.evidence.abc_condition(span, node.kind());
            }
        }
        if facts.abc_assignment {
            self.current().abc.record_assignment();
            if let Some(span) = ev_span {
                self.evidence.abc_assignment(span, node.kind());
            }
        }
        // NPA / NPM — language-classified attribute / method
        // declarations. The enclosing class-like state owns the
        // increment; recorded *after* a possible scope-open above
        // pushed a new (method) space, so we walk back to the parent's
        // kind to decide whether we're inside a class.
        let enclosing_class_kind = if opened_space {
            // The current space is the just-opened one (e.g. a method);
            // its parent is the previous frame.
            self.kinds
                .iter()
                .rev()
                .nth(1)
                .cloned()
                .unwrap_or(SpaceKind::Unit)
        } else {
            self.kinds.last().cloned().unwrap_or(SpaceKind::Unit)
        };
        let in_class_like = matches!(
            enclosing_class_kind,
            SpaceKind::Class | SpaceKind::Impl | SpaceKind::Interface | SpaceKind::Trait
        );
        if in_class_like {
            if let Some(cls) = self.rules.classify_attribute(&node, self.source_text) {
                let parent_idx = if opened_space {
                    self.stack.len().saturating_sub(2)
                } else {
                    self.stack.len().saturating_sub(1)
                };
                if let Some(parent) = self.stack.get_mut(parent_idx) {
                    parent.npa.record_attribute(cls.container, cls.is_public);
                    if cls.is_public && self.evidence.is_enabled() {
                        let span = node_span(&node, self.line_index);
                        self.evidence.public_attribute(span, node.kind());
                    }
                }
            }
            if let Some(cls) = self.rules.classify_method(&node, self.source_text) {
                let parent_idx = if opened_space {
                    self.stack.len().saturating_sub(2)
                } else {
                    self.stack.len().saturating_sub(1)
                };
                if let Some(parent) = self.stack.get_mut(parent_idx) {
                    parent.npm.record_method(cls.container, cls.is_public);
                    if cls.is_public && self.evidence.is_enabled() {
                        let span = node_span(&node, self.line_index);
                        self.evidence.public_method(span, node.kind());
                    }
                }
            }
        }
        // LOC: each AST node contributes per-language to PLOC / LLOC /
        // CLOC. The walker stays language-agnostic — it forwards the
        // language's `LocFact` decision into the per-space accumulator.
        let start_row = node.start_position().row as u32;
        let end_row = node.end_position().row as u32;
        match facts.loc {
            LocFact::Container => {}
            LocFact::Lloc => {
                self.current().loc.observe_lloc();
            }
            LocFact::Comment => {
                self.current().loc.observe_comment(start_row, end_row);
            }
            LocFact::Code => {
                self.current().loc.observe_code_line(start_row);
            }
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                self.visit_with_ctx(cursor.node(), ctx);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        if opened_space {
            let closed_kind = self.kinds.pop().expect("kinds underflow on close");
            let mut state = self.stack.pop().expect("walker stack underflow on close");
            // Per pre-1.0 `Wmc::compute`: a function/method space
            // contributes its cyclomatic value into the enclosing
            // class-like's WMC sum. The walker snapshots the cyclomatic
            // value here from the closing function space.
            if matches!(closed_kind, SpaceKind::Function) {
                state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
            }
            finalize_state(&mut state);
            apply_state_to_for_close(&state, self.tree.metrics_mut());
            // Fold this space's rolled-up bounds into the parent so the
            // unit's final stats reflect every nested space. WMC also
            // folds the closing method's per-space `wmc` into the
            // parent's class/interface bucket when the parent is the
            // class-like container.
            if let Some(parent) = self.stack.last_mut() {
                let parent_kind = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
                merge_child_into_parent(parent, &state);
                if matches!(closed_kind, SpaceKind::Function) {
                    let container = match parent_kind {
                        SpaceKind::Class | SpaceKind::Impl => mehen_metrics::ContainerKind::Class,
                        SpaceKind::Interface | SpaceKind::Trait => {
                            mehen_metrics::ContainerKind::Interface
                        }
                        _ => mehen_metrics::ContainerKind::Other,
                    };
                    state.wmc.finalize_method_into(container, &mut parent.wmc);
                }
            }
            self.tree.close();
        }
    }
}

/// Variant of [`apply_state_to`] that takes a borrow — used at space
/// close where the state must also be merged into the parent. The
/// freestanding `apply_state_to(state, target)` continues to consume
/// its argument so external callers (the walker's unit close path)
/// don't pay for an extra clone.
fn apply_state_to_for_close(state: &State, target: &mut mehen_core::MetricSet) {
    apply_state_to(state.clone(), target);
}

/// Convenience: build an "empty" space (used by analyzers when the parser
/// fails before any walk can happen).
pub fn empty_space(span: SourceSpan) -> MetricSpace {
    MetricSpace::new(SpaceId(0), SpaceKind::Unit, span)
}
