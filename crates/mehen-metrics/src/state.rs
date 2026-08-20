// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Per-space accumulator state and metric publishing helpers.
//!
//! This module owns the *generic* metric bookkeeping that every language
//! analyzer needs:
//!
//! - the [`State`] struct holding one of every accumulator,
//! - [`finalize_state`] / [`merge_child_into_parent`] for the rolled-up
//!   sum/min/max/avg lifecycle,
//! - [`apply_state_to`] which materializes a finalized [`State`] into a
//!   [`MetricSet`] using the keys from `mehen_metrics::keys`.
//!
//! Per the rewrite plan §4.3 these helpers belong here (in `mehen-metrics`)
//! rather than in any one language adapter — they describe the *output
//! contract* of every per-space metric set and are reused by every
//! language crate. The shared tree-sitter walker
//! ([`mehen_tree_sitter::walk`]) and the Oxc-backed `mehen-typescript`
//! analyzer both publish through this module.
//!
//! What does *not* live here:
//! - language-specific syntax interpretation (which AST kinds count as
//!   decisions, operators, exits, ...). Those live in the owning
//!   language crate.
//! - the parser-side walking strategy (tree-sitter cursor vs Oxc visitor).

use mehen_core::{MetricKey, MetricSet, SpaceKind};

use crate::{
    AbcStats, CognitiveStats, ContainerKind, CyclomaticStats, HalsteadBuilder, HalsteadStats,
    LocStats, MetricTreeBuilder, MiStats, NargsStats, NexitStats, NomStats, NpaStats, NpmStats,
    SpaceRangeTracker, WmcStats, keys,
};

/// Per-space accumulator state. Analyzers push one of these for the
/// `Unit` root and for every space they open via their walker's
/// scope-open hook.
#[derive(Default, Clone)]
pub struct State {
    pub loc: LocStats,
    pub cyclomatic: CyclomaticStats,
    pub cognitive: CognitiveStats,
    pub halstead: HalsteadBuilder,
    pub abc: AbcStats,
    pub nargs: NargsStats,
    pub nom: NomStats,
    pub nexit: NexitStats,
    pub npa: NpaStats,
    pub npm: NpmStats,
    pub wmc: WmcStats,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize a fresh `State` for an opened space, applying the
    /// kind-specific bookkeeping every walker performs:
    ///
    /// - `Function` records a function in `nom`.
    /// - `Closure` records a closure in `nom`.
    /// - `Class` / `Impl` record a class-like in `npa`/`npm`/`wmc`.
    /// - `Interface` / `Trait` record a class-like in `npa`/`npm` only.
    /// - Other kinds (`Unit`, `Enum`, `Custom`) do nothing.
    ///
    /// Callers still set their own LOC span — each walker has its own
    /// `LineIndex` access pattern (Ruff `TextRange`, Oxc `Span`,
    /// tree-sitter byte offsets) so we keep span resolution at the
    /// call site.
    pub fn for_opened_space(kind: SpaceKind) -> Self {
        let mut child = Self::new();
        match kind {
            SpaceKind::Function => child.nom.record_function(),
            SpaceKind::Closure => child.nom.record_closure(),
            SpaceKind::Class | SpaceKind::Impl => {
                child.npa.record_class_like();
                child.npm.record_class_like();
                child.wmc.record_class_like();
            }
            SpaceKind::Interface | SpaceKind::Trait => {
                child.npa.record_class_like();
                child.npm.record_class_like();
            }
            _ => {}
        }
        child
    }
}

/// Close a space: pop its `State` and `SpaceKind` from the walker's
/// stacks, finalize, stash the AST-side LOC + cyclomatic snapshots
/// for the post-AST Halstead overlay, publish the per-space `MetricSet`,
/// merge the rolled-up bounds into the parent, and close the
/// `MetricTreeBuilder`.
///
/// This is byte-identical across the Python, TypeScript, Ruby, Rust,
/// PHP, Go, C, and Kotlin walkers — they all carry the same four state
/// buckets (`stack`, `kinds`, `tree`, `halstead_routing`) and run the
/// same finalize → record_close → apply → merge → close sequence.
/// CPD flagged a 30-line / 130-token cluster across them; pulling the
/// shared logic here means a fix or feature lands once instead of
/// eight times.
///
/// Panics on stack underflow (unbalanced open/close — the same way the
/// per-walker copies did).
pub fn close_space(
    stack: &mut Vec<State>,
    kinds: &mut Vec<SpaceKind>,
    tree: &mut MetricTreeBuilder,
    halstead_routing: &mut SpaceRangeTracker,
) {
    let closed_kind = kinds.pop().expect("kinds underflow");
    let mut state = stack.pop().expect("stack underflow");
    if matches!(closed_kind, SpaceKind::Function) {
        state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
    }
    finalize_state(&mut state);
    if let Some(space_id) = tree.current_id() {
        halstead_routing.record_close(space_id, &state.loc, &state.cyclomatic);
    }
    apply_state_to(state.clone(), tree.metrics_mut());
    if let Some(parent) = stack.last_mut() {
        let parent_kind = kinds.last().cloned().unwrap_or(SpaceKind::Unit);
        merge_child_into_parent(parent, &state);
        if matches!(closed_kind, SpaceKind::Function) {
            let container = match parent_kind {
                SpaceKind::Class | SpaceKind::Impl => ContainerKind::Class,
                SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
                _ => ContainerKind::Other,
            };
            state.wmc.finalize_method_into(container, &mut parent.wmc);
        }
    }
    tree.close();
}

/// Snapshot the per-space "current" values into rolled-up
/// sum/min/max/avg fields. Called on every space close before the
/// per-space MetricSet is published or merged into the parent.
pub fn finalize_state(state: &mut State) {
    state.cyclomatic.finalize_minmax();
    state.cyclomatic.finalize_average();
    state.loc.finalize_minmax();
    state.nom.finalize_minmax();
    state.nargs.finalize_minmax();
    state.nexit.finalize_minmax();
    state.nexit.finalize_average(state.nom.total());
    state
        .nargs
        .finalize_average(state.nom.functions_sum, state.nom.closures_sum);
    state.abc.finalize_minmax();
    state.npa.finalize_minmax();
    state.npm.finalize_minmax();
    state.cognitive.finalize_minmax();
    state.cognitive.finalize(state.nom.total());
}

/// Fold a finalized child state's rolled-up totals (sum/min/max/n)
/// into the parent state. The parent's per-space "current" values are
/// not affected — children contribute only via the bounds.
pub fn merge_child_into_parent(parent: &mut State, child: &State) {
    parent.cyclomatic.merge(&child.cyclomatic);
    parent.cyclomatic.finalize_average();
    parent.loc.merge(&child.loc);
    parent.nom.merge(&child.nom);
    parent.nargs.merge(&child.nargs);
    parent.nexit.merge(&child.nexit);
    parent.nexit.finalize_average(parent.nom.total());
    parent
        .nargs
        .finalize_average(parent.nom.functions_sum, parent.nom.closures_sum);
    parent.abc.merge(&child.abc);
    parent.halstead.merge(&child.halstead);
    parent.npa.merge(&child.npa);
    parent.npm.merge(&child.npm);
    parent.wmc.merge(&child.wmc);
    parent.cognitive.merge(&child.cognitive);
    parent.cognitive.finalize(parent.nom.total());
}

/// Publish a finalized `State` into a `MetricSet` using the shared key
/// names. Per the rewrite plan §5.1 each metric publishes the rolled-up
/// `{ sum, min, max, average }` set under aggregator-suffixed selectors
/// (`cyclomatic.sum`, `cyclomatic.min`, …) plus the bare per-space
/// value at the metric's root key.
pub fn apply_state_to(state: State, target: &mut MetricSet) {
    publish_cyclomatic(&state.cyclomatic, target);
    publish_loc(&state.loc, target);
    publish_nom(&state.nom, target);
    publish_nargs(&state.nargs, &state.nom, target);
    publish_nexit(&state.nexit, target);
    publish_cognitive(&state.cognitive, target);

    let halstead = HalsteadStats::from_counts(state.halstead.counts());
    publish_halstead(&halstead, target);

    let mi = MiStats::compute(&state.loc, &state.cyclomatic, &halstead);
    target.insert(MetricKey::new(keys::MI_VS), mi.mi_visual_studio);
    target.insert(MetricKey::new(keys::MI_ORIGINAL), mi.mi_original);
    target.insert(MetricKey::new(keys::MI_SEI), mi.mi_sei);

    publish_abc(&state.abc, target);
    publish_npa(&state.npa, target);
    publish_npm(&state.npm, target);
    publish_wmc(&state.wmc, target);
}

fn publish_npa(stats: &NpaStats, target: &mut MetricSet) {
    if stats.is_disabled() {
        return;
    }
    target.insert(MetricKey::new(keys::NPA), stats.total_npa() as i64);
    target.insert(
        MetricKey::new(format!("{}.classes", keys::NPA)),
        stats.class_npa_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.interfaces", keys::NPA)),
        stats.interface_npa_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.class_attributes", keys::NPA)),
        stats.class_na_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.interface_attributes", keys::NPA)),
        stats.interface_na_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.classes_average", keys::NPA)),
        stats.class_cda(),
    );
    target.insert(
        MetricKey::new(format!("{}.interfaces_average", keys::NPA)),
        stats.interface_cda(),
    );
    target.insert(
        MetricKey::new(format!("{}.total_attributes", keys::NPA)),
        stats.total_na() as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.average", keys::NPA)),
        stats.total_cda(),
    );
}

fn publish_npm(stats: &NpmStats, target: &mut MetricSet) {
    if stats.is_disabled() {
        return;
    }
    target.insert(MetricKey::new(keys::NPM), stats.total_npm() as i64);
    target.insert(
        MetricKey::new(format!("{}.classes", keys::NPM)),
        stats.class_npm_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.interfaces", keys::NPM)),
        stats.interface_npm_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.class_methods", keys::NPM)),
        stats.class_nm_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.interface_methods", keys::NPM)),
        stats.interface_nm_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.classes_average", keys::NPM)),
        stats.class_avg(),
    );
    target.insert(
        MetricKey::new(format!("{}.interfaces_average", keys::NPM)),
        stats.interface_avg(),
    );
    target.insert(
        MetricKey::new(format!("{}.total_methods", keys::NPM)),
        stats.total_nm() as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.average", keys::NPM)),
        stats.total_avg(),
    );
}

fn publish_wmc(stats: &WmcStats, target: &mut MetricSet) {
    if stats.is_disabled() {
        return;
    }
    target.insert(MetricKey::new(keys::WMC), stats.total() as i64);
    target.insert(
        MetricKey::new(format!("{}.classes", keys::WMC)),
        stats.class_wmc_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.interfaces", keys::WMC)),
        stats.interface_wmc_sum as i64,
    );
}

fn publish_cognitive(stats: &CognitiveStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::COGNITIVE), stats.cognitive_sum as i64);
    target.insert(
        MetricKey::new(format!("{}.sum", keys::COGNITIVE)),
        stats.cognitive_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.average", keys::COGNITIVE)),
        stats.cognitive_average,
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::COGNITIVE)),
        stats.min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::COGNITIVE)),
        stats.max as i64,
    );
}

/// Publish the full Halstead key set (`halstead.volume`,
/// `halstead.difficulty`, `halstead.effort`, `halstead.{n1,N1,n2,N2}`,
/// `halstead.{length,vocabulary,level,time,bugs,…}`) onto a
/// `MetricSet`.
///
/// Visibility is `pub(crate)` because the post-AST token-routing
/// overlay in `crate::halstead_routing` needs to rewrite the same
/// keys when Pattern B walkers (Python, TypeScript, Rust, PHP) emit
/// Halstead in a separate pass after `apply_state_to` has already
/// run. Both call sites must publish identical keys with identical
/// formulas; otherwise the per-space JSON Halstead numbers drift
/// from the unit-level rollup. Funneling them through the same
/// helper makes that drift impossible.
pub(crate) fn publish_halstead(stats: &HalsteadStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::HALSTEAD_VOLUME), stats.volume());
    target.insert(
        MetricKey::new(keys::HALSTEAD_DIFFICULTY),
        stats.difficulty(),
    );
    target.insert(MetricKey::new(keys::HALSTEAD_EFFORT), stats.effort());
    target.insert(
        MetricKey::new(keys::HALSTEAD_VOCABULARY),
        stats.vocabulary(),
    );
    target.insert(MetricKey::new(keys::HALSTEAD_LENGTH), stats.length());
    target.insert(
        MetricKey::new(format!("{}.n1", keys::HALSTEAD)),
        stats.u_operators as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.N1", keys::HALSTEAD)),
        stats.operators as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.n2", keys::HALSTEAD)),
        stats.u_operands as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.N2", keys::HALSTEAD)),
        stats.operands as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.length", keys::HALSTEAD)),
        stats.length(),
    );
    target.insert(
        MetricKey::new(format!("{}.estimated_program_length", keys::HALSTEAD)),
        stats.estimated_program_length(),
    );
    target.insert(
        MetricKey::new(format!("{}.purity_ratio", keys::HALSTEAD)),
        stats.purity_ratio(),
    );
    target.insert(
        MetricKey::new(format!("{}.vocabulary", keys::HALSTEAD)),
        stats.vocabulary(),
    );
    target.insert(
        MetricKey::new(format!("{}.level", keys::HALSTEAD)),
        stats.level(),
    );
    target.insert(
        MetricKey::new(format!("{}.time", keys::HALSTEAD)),
        stats.time(),
    );
    target.insert(
        MetricKey::new(format!("{}.bugs", keys::HALSTEAD)),
        stats.bugs(),
    );
}

fn publish_abc(stats: &AbcStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::ABC), stats.magnitude());
    target.insert(
        MetricKey::new(keys::ABC_ASSIGNMENTS),
        stats.assignments_sum as i64,
    );
    target.insert(
        MetricKey::new(keys::ABC_BRANCHES),
        stats.branches_sum as i64,
    );
    target.insert(
        MetricKey::new(keys::ABC_CONDITIONS),
        stats.conditions_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.assignments_average", keys::ABC)),
        stats.assignments_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.branches_average", keys::ABC)),
        stats.branches_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.conditions_average", keys::ABC)),
        stats.conditions_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.assignments_min", keys::ABC)),
        stats.assignments_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.assignments_max", keys::ABC)),
        stats.assignments_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.branches_min", keys::ABC)),
        stats.branches_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.branches_max", keys::ABC)),
        stats.branches_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.conditions_min", keys::ABC)),
        stats.conditions_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.conditions_max", keys::ABC)),
        stats.conditions_max as i64,
    );
}

fn publish_nargs(stats: &NargsStats, nom: &NomStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::NARGS), stats.total() as i64);
    target.insert(
        MetricKey::new(format!("{}.total_functions", keys::NARGS)),
        stats.fn_nargs_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.total_closures", keys::NARGS)),
        stats.closure_nargs_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.average_functions", keys::NARGS)),
        stats.fn_nargs_average,
    );
    target.insert(
        MetricKey::new(format!("{}.average_closures", keys::NARGS)),
        stats.closure_nargs_average,
    );
    target.insert(
        MetricKey::new(format!("{}.average", keys::NARGS)),
        stats.nargs_average(nom.functions_sum, nom.closures_sum),
    );
    target.insert(
        MetricKey::new(format!("{}.functions_min", keys::NARGS)),
        stats.fn_nargs_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.functions_max", keys::NARGS)),
        stats.fn_nargs_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.closures_min", keys::NARGS)),
        stats.closure_nargs_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.closures_max", keys::NARGS)),
        stats.closure_nargs_max as i64,
    );
}

fn publish_nom(stats: &NomStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::NOM), stats.total() as i64);
    target.insert(
        MetricKey::new(keys::NOM_FUNCTIONS),
        stats.functions_sum as i64,
    );
    target.insert(
        MetricKey::new(keys::NOM_CLOSURES),
        stats.closures_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.functions_average", keys::NOM)),
        stats.functions_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.closures_average", keys::NOM)),
        stats.closures_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.average", keys::NOM)),
        stats.average(),
    );
    target.insert(
        MetricKey::new(format!("{}.functions_min", keys::NOM)),
        stats.functions_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.functions_max", keys::NOM)),
        stats.functions_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.closures_min", keys::NOM)),
        stats.closures_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.closures_max", keys::NOM)),
        stats.closures_max as i64,
    );
}

fn publish_nexit(stats: &NexitStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::NEXIT), stats.exits as i64);
    target.insert(
        MetricKey::new(format!("{}.sum", keys::NEXIT)),
        stats.sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.average", keys::NEXIT)),
        stats.average,
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::NEXIT)),
        stats.min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::NEXIT)),
        stats.max as i64,
    );
}

fn publish_cyclomatic(stats: &CyclomaticStats, target: &mut MetricSet) {
    let mccabe = stats.cyclomatic.saturating_add(1) as i64;
    target.insert(MetricKey::new(keys::CYCLOMATIC), mccabe);
    target.insert(
        MetricKey::new(format!("{}.sum", keys::CYCLOMATIC)),
        stats.cyclomatic_sum as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::CYCLOMATIC)),
        stats.min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::CYCLOMATIC)),
        stats.max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.avg", keys::CYCLOMATIC)),
        stats.cyclomatic_average,
    );
}

fn publish_loc(stats: &LocStats, target: &mut MetricSet) {
    target.insert(MetricKey::new(keys::LOC_LLOC), stats.lloc() as i64);
    target.insert(MetricKey::new(keys::LOC_SLOC), stats.sloc() as i64);
    target.insert(MetricKey::new(keys::LOC_PLOC), stats.ploc() as i64);
    target.insert(MetricKey::new(keys::LOC_CLOC), stats.cloc() as i64);
    target.insert(MetricKey::new(keys::LOC_BLANK), stats.blank() as i64);
    target.insert(MetricKey::new(keys::LOC), stats.sloc() as i64);

    target.insert(
        MetricKey::new(format!("{}.min", keys::LOC_SLOC)),
        stats.sloc_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::LOC_SLOC)),
        stats.sloc_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.avg", keys::LOC_SLOC)),
        stats.sloc_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::LOC_PLOC)),
        stats.ploc_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::LOC_PLOC)),
        stats.ploc_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.avg", keys::LOC_PLOC)),
        stats.ploc_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::LOC_LLOC)),
        stats.lloc_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::LOC_LLOC)),
        stats.lloc_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.avg", keys::LOC_LLOC)),
        stats.lloc_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::LOC_CLOC)),
        stats.cloc_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::LOC_CLOC)),
        stats.cloc_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.avg", keys::LOC_CLOC)),
        stats.cloc_average(),
    );
    target.insert(
        MetricKey::new(format!("{}.min", keys::LOC_BLANK)),
        stats.blank_min as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.max", keys::LOC_BLANK)),
        stats.blank_max as i64,
    );
    target.insert(
        MetricKey::new(format!("{}.avg", keys::LOC_BLANK)),
        stats.blank_average(),
    );
}
