// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-tree-sitter` — shared support for tree-sitter-backed analyzer
//! crates.
//!
//! Per the rewrite plan §4.5 this crate is *not* the owner of Rust, Go, C,
//! Kotlin, or PowerShell semantics — it only helps language analyzer crates
//! use tree-sitter cleanly. Anything that interprets a node kind belongs in
//! the owning language crate.
//!
//! The 1.0 first phase exposes:
//! - `TreeSitterParser`: a small wrapper around `tree_sitter::Parser` that
//!   handles error states, retains source for span/text queries, and
//!   provides byte-offset traversal helpers,
//! - `node_span`: a helper that lifts a tree-sitter node into mehen's
//!   `SourceSpan`,
//! - `text_of`: a helper that fetches the source text covered by a node.
//!
//! The generator and generated kind-enum utilities will land in this crate
//! when phase 7's `cargo xtask tree-sitter generate <language>` is wired up.
//! For Phase 1 only the runtime support layer exists.

#![forbid(unsafe_code)]

mod parser;
mod scaffold;
mod span;
mod walker;

pub use mehen_metrics::{MetricEvidence, State, apply_state_to};
pub use parser::{TreeSitterError, TreeSitterParser, collect_recovered_errors};
pub use scaffold::{CognitiveContext, OpenSpaceRequest, WalkerCtx, WalkerHooks, run};
pub use span::{node_span, text_of};
pub use walker::{
    CognitiveFact, LanguageRules, LocFact, MemberClassification, NodeFacts, ScopeOpen, WalkResult,
    empty_space, walk,
};
