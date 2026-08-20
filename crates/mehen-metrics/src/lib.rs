// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-metrics` — shared metric contracts, formulas, accumulators, and
//! aggregation helpers.
//!
//! This crate must not become a central all-language calculator. Per the
//! rewrite plan §4.3:
//! - It owns the metric *math* (Halstead volume, MI formulas, min/max/avg
//!   finalization, set-based n1/n2 dedup, …).
//! - It does not own language interpretation (which Python AST nodes are
//!   decisions, whether a Ruby rescue modifier counts toward cognitive,
//!   etc.).
//!
//! Phase 1 ships the typed accumulator surface — `LocStats`,
//! `CyclomaticStats`, `CognitiveStats`, `HalsteadStats`, `AbcStats`,
//! `NargsStats`, `NomStats`, `NexitStats`, `MiStats`, `WmcStats`,
//! `NpaStats`, `NpmStats` — plus the `HalsteadBuilder` event sink and the
//! `MetricTreeBuilder` helper that language crates use to assemble a
//! `MetricSpace` tree without each crate re-implementing id allocation.

#![forbid(unsafe_code)]

mod abc;
mod cognitive;
mod counters;
mod cyclomatic;
mod evidence;
mod halstead;
mod halstead_builder;
mod halstead_routing;
mod loc;
mod mi;
mod state;
mod tree_builder;

pub use abc::AbcStats;
pub use cognitive::CognitiveStats;
pub use counters::{ContainerKind, NargsStats, NexitStats, NomStats, NpaStats, NpmStats, WmcStats};
pub use cyclomatic::CyclomaticStats;
pub use evidence::MetricEvidence;
pub use halstead::HalsteadStats;
pub use halstead_builder::{HalsteadBuilder, HalsteadCounts, HalsteadOperand, HalsteadOperator};
pub use halstead_routing::SpaceRangeTracker;
pub use loc::{LineClass, LocStats};
pub use mi::MiStats;
pub use state::{State, apply_state_to, close_space, finalize_state, merge_child_into_parent};
pub use tree_builder::MetricTreeBuilder;

// Re-export the metric key namespace and the selector/threshold contract
// surface from `mehen-core` so existing `mehen_metrics::*` consumers
// keep compiling. Per the plan §4.2 these are contract types that
// belong to `mehen-core`; per §8.2 the selector catalogue may live in
// either crate.
pub use mehen_core::{
    MetricKey, MetricSelector, Polarity, SelectorAggregator, SelectorParseError, Threshold,
    ThresholdEvaluation, ThresholdViolation, keys,
};
