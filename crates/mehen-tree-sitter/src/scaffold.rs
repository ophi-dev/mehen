// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Shared scaffolding for per-language tree-sitter walkers.
//!
//! Every per-language walker (Go, C, Kotlin, …) shares the same shape:
//! a unit-rooted recursive cursor descent that maintains a per-space
//! `State` stack, a parallel `SpaceKind` stack, and a single-frame
//! `(nesting, depth, lambda)` cognitive context that saves/restores
//! around each subtree. The classification at each node — what counts
//! as a decision, what opens a space, which kinds are operators vs.
//! operands — is language-specific and lives in the per-language
//! crate.
//!
//! `WalkerHooks` exposes the language-specific seam. The host crate
//! implements it, then calls [`run`] to drive the walk. The
//! cognitive-context save/restore semantics, the `kinds` stack, the
//! `tree.open` / `tree.close` bookkeeping, and the unit-space
//! `set_span` / `finalize_state` / `apply_state_to` lifecycle live
//! here so they stay byte-identical across every consumer.
//!
//! Class-aware languages (Kotlin, Rust) need a hook that runs *before*
//! the space opens — see `WalkerHooks::pre_open`. Languages without
//! class-aware metrics leave it as the default no-op.

use mehen_core::{LineIndex, MetricSpace, SourceSpan, SpaceKind};
use mehen_metrics::{
    MetricEvidence, MetricTreeBuilder, State, apply_state_to, finalize_state,
    merge_child_into_parent,
};
use tree_sitter::Node;

use crate::span::node_span;

/// Per-frame cognitive context (nesting / function-depth / closure
/// counter) carried across the visit recursion. Mirrors the legacy
/// `(nesting, depth, lambda)` triple from `cognitive::Stats`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CognitiveContext {
    pub nesting: u32,
    pub depth: u32,
    pub lambda: u32,
}

/// Language-specific seam for the shared walker scaffolding.
///
/// The shared scaffolding owns the lifecycle (visit recursion,
/// cognitive context save/restore, `tree.open`/`close`, unit-space
/// finalize). Host crates implement the hooks below to plug in their
/// per-language classification.
pub trait WalkerHooks {
    /// Called once per node before any child observations run, and
    /// before `open_space` is consulted. Class-aware languages use
    /// this to classify "this node is a method/field of the
    /// *enclosing* class" — by the time `open_space` has run, the
    /// kinds stack will have the new space on top, but for those
    /// classifications the *enclosing* space's kind is what matters.
    /// Default: no-op.
    fn pre_open(&mut self, _ctx: &mut WalkerCtx<'_>, _node: &Node<'_>) {}

    /// If `node` opens a new metric space (function, closure, class,
    /// trait, …), prepare the child `State`, push the `kind` onto the
    /// `kinds` stack, and return `Some(OpenedSpace { kind })`. The
    /// shared scaffolding will record the `tree.open(...)` and push
    /// the prepared state onto the stack in lock-step.
    ///
    /// Returning `None` means this node does not open a space; the
    /// scaffolding falls through to per-node classification only.
    fn open_space(&mut self, ctx: &mut WalkerCtx<'_>, node: &Node<'_>) -> Option<OpenSpaceRequest>;

    /// Called immediately after the new space is pushed. Host
    /// updates the cognitive context (`nesting=0`, `depth+=1`,
    /// `lambda+=1`, …) per the language's Sonar-style cognitive
    /// rules. The `kind` argument is the kind that was just pushed.
    fn on_space_enter(&mut self, _ctx: &mut WalkerCtx<'_>, _kind: SpaceKind) {}

    /// Per-node classification (cyclomatic, cognitive, ABC, exit,
    /// LOC, Halstead — anything that doesn't open a space). Runs
    /// *after* the space (if any) has been opened and the kinds
    /// stack updated. Side effects on `ctx.cognitive` are scoped to
    /// the subtree (they're saved/restored around the visit
    /// recursion).
    fn classify(&mut self, ctx: &mut WalkerCtx<'_>, node: &Node<'_>);

    /// Called immediately before the closing space's state is
    /// finalized and merged into its parent. Host can update the
    /// state (e.g. WMC's `set_cyclomatic`), and route the closing
    /// state into the parent based on `closed_kind` /
    /// `parent_kind`. Default: do nothing.
    ///
    /// `parent_kind` is the kind on top of the kinds stack *after*
    /// `closed_kind` is popped — i.e. the kind that will own the
    /// merged state. `Unit` if the closing space is a top-level
    /// container.
    fn before_close(
        &mut self,
        _state: &mut State,
        _closed_kind: SpaceKind,
        _parent_kind: SpaceKind,
    ) {
    }

    /// Called *after* `merge_child_into_parent` has run. Host can
    /// route the just-closed state into a parent-side accumulator
    /// (e.g. Kotlin's `wmc.finalize_method_into(container, ...)`).
    /// `parent_state` is the parent's `&mut State`. Default: do
    /// nothing.
    fn after_close(
        &mut self,
        _state: &State,
        _closed_kind: SpaceKind,
        _parent_state: &mut State,
        _parent_kind: SpaceKind,
    ) {
    }
}

/// State passed into every `WalkerHooks` callback. Host crates use it
/// to read the source / line index, push state onto the per-space
/// stack (via `enter_space`), inspect parent kinds, or mutate the
/// cognitive context.
pub struct WalkerCtx<'a> {
    pub line_index: &'a LineIndex,
    pub source: &'a [u8],
    pub stack: &'a mut Vec<State>,
    pub kinds: &'a mut Vec<SpaceKind>,
    pub cognitive: &'a mut CognitiveContext,
    /// Contribution-evidence sink (plan §5.4). Hooks record next to the
    /// stat increment, usually through [`WalkerCtx::record_evidence`].
    pub evidence: &'a mut MetricEvidence,
}

impl<'a> WalkerCtx<'a> {
    /// The state for the currently-open space.
    #[inline]
    pub fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    /// Record contribution evidence for `node`. The span is only
    /// computed when the sink is enabled, so classify hooks can call
    /// this unconditionally next to each stat increment:
    ///
    /// ```ignore
    /// ctx.current().cyclomatic.record_decision();
    /// ctx.record_evidence(node, |e, s| e.decision(s, "if_statement"));
    /// ```
    #[inline]
    pub fn record_evidence<F>(&mut self, node: &Node<'_>, record: F)
    where
        F: FnOnce(&mut MetricEvidence, SourceSpan),
    {
        if self.evidence.is_enabled() {
            let span = node_span(node, self.line_index);
            record(self.evidence, span);
        }
    }

    /// The kind for the currently-open space.
    #[inline]
    pub fn current_kind(&self) -> SpaceKind {
        self.kinds.last().cloned().expect("kinds stack empty")
    }

    /// Iterate ancestor kinds from innermost to outermost (skipping
    /// the current frame). Used by host crates to detect whether a
    /// freshly opened function is nested inside another function.
    pub fn ancestor_kinds(&self) -> impl Iterator<Item = &SpaceKind> + '_ {
        self.kinds.iter().rev().skip(1)
    }
}

/// Description returned by [`WalkerHooks::open_space`]. Carries the
/// kind to push, the optional name, the source span, and the prepared
/// child `State` (host populates `nom` / `nargs` / `loc.set_span` /
/// `npa.record_class_like` etc. before returning).
pub struct OpenSpaceRequest {
    pub kind: SpaceKind,
    pub name: Option<String>,
    pub span: SourceSpan,
    pub state: State,
}

/// Internal driver state for the recursive walk. Bundling the
/// per-walk fields into one struct lets `visit` take `&mut self`
/// instead of an 8-argument function. `hooks` is borrowed reentrantly
/// — each callback gets a fresh `WalkerCtx` borrowed from the rest
/// of the struct, so we keep the hooks pointer separate from the
/// state pointer to keep the borrow checker happy.
struct Walker<'a> {
    line_index: &'a LineIndex,
    source: &'a [u8],
    tree: MetricTreeBuilder,
    stack: Vec<State>,
    kinds: Vec<SpaceKind>,
    cognitive: CognitiveContext,
    evidence: &'a mut MetricEvidence,
}

/// Drive the shared walker over `root`. Mirrors the per-crate
/// `walk_program` entries that previously existed in `mehen-c`,
/// `mehen-go`, and `mehen-kotlin`.
///
/// `evidence` is the caller-owned contribution sink; the host's hooks
/// reach it through [`WalkerCtx::record_evidence`], and the caller
/// finishes it after the walk to obtain the recorded contributions.
pub fn run<H: WalkerHooks>(
    hooks: &mut H,
    root: Node<'_>,
    source: &[u8],
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let unit_span = node_span(&root, line_index);

    let mut unit_state = State::new();
    unit_state.loc.set_span(
        root.start_position().row as u32,
        root.end_position().row as u32,
        true,
    );

    let mut walker = Walker {
        line_index,
        source,
        tree: MetricTreeBuilder::new(unit_span),
        stack: vec![unit_state],
        kinds: vec![SpaceKind::Unit],
        cognitive: CognitiveContext::default(),
        evidence,
    };
    walker.visit(hooks, root);

    let mut unit_state = walker.stack.pop().expect("walker stack underflow");
    finalize_state(&mut unit_state);
    apply_state_to(unit_state, walker.tree.metrics_mut());
    walker.tree.finish()
}

impl Walker<'_> {
    fn ctx(&mut self) -> WalkerCtx<'_> {
        WalkerCtx {
            line_index: self.line_index,
            source: self.source,
            stack: &mut self.stack,
            kinds: &mut self.kinds,
            cognitive: &mut self.cognitive,
            evidence: &mut *self.evidence,
        }
    }

    fn visit<H: WalkerHooks>(&mut self, hooks: &mut H, node: Node<'_>) {
        let saved_cognitive = self.cognitive;

        hooks.pre_open(&mut self.ctx(), &node);

        let opened_request = hooks.open_space(&mut self.ctx(), &node);

        let opened = if let Some(req) = opened_request {
            self.tree.open(req.kind.clone(), req.span, req.name);
            self.stack.push(req.state);
            self.kinds.push(req.kind.clone());

            hooks.on_space_enter(&mut self.ctx(), req.kind);
            true
        } else {
            false
        };

        hooks.classify(&mut self.ctx(), &node);

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                self.visit(hooks, cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        if opened {
            let closed_kind = self.kinds.pop().expect("kinds stack underflow");
            let mut state = self.stack.pop().expect("walker stack underflow");
            let parent_kind = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
            hooks.before_close(&mut state, closed_kind.clone(), parent_kind.clone());
            finalize_state(&mut state);
            apply_state_to(state.clone(), self.tree.metrics_mut());
            if let Some(parent) = self.stack.last_mut() {
                merge_child_into_parent(parent, &state);
                hooks.after_close(&state, closed_kind, parent, parent_kind);
            }
            self.tree.close();
        }

        self.cognitive = saved_cognitive;
    }
}
