// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Recovered-error collection for ANTLR parse trees.
//!
//! Like tree-sitter, the ANTLR runtime recovers from syntax errors rather
//! than aborting: bad input surfaces as `ParseTree::Error` leaves embedded
//! in an otherwise-complete tree. Per mehen's diagnostic contract (rewrite
//! plan §9.3) these must be reported as `error`-severity diagnostics so
//! that `mehen metrics` exits 1 and `mehen diff` records the file under
//! `analysis_errors` — metric output from a partially-recovered tree must
//! never masquerade as a clean parse.
//!
//! Hard parser failures (the entry-rule call itself returning `Err`) are a
//! separate, `fatal` path handled by the analyzer crate; this module only
//! covers recovered `Error` nodes within a returned tree.

use antlr4_runtime::Node;
use antlr4_runtime::token::Token;
use mehen_core::ParseDiagnostic;

/// Walk `tree` and emit one `error`-severity [`ParseDiagnostic`] per recovered
/// error leaf ([`NodeKind::Error`](antlr4_runtime::NodeKind::Error)), capped at
/// `max_diagnostics` to bound noise on heavily corrupted input.
///
/// `code` is the language-namespaced diagnostic code, e.g.
/// `"kotlin.syntax_error"`. Returns an empty `Vec` for a clean parse.
///
/// Since the 0.11 runtime rewrite the tree is a flat arena traversed through
/// borrowing [`Node`] views. [`Node::descendants`] yields a pre-order iterator
/// over the whole subtree, so error leaves are collected by filtering it with
/// [`Node::as_error`] — no hand-rolled recursion.
pub fn collect_errors(tree: Node<'_>, code: &str, max_diagnostics: usize) -> Vec<ParseDiagnostic> {
    tree.descendants()
        .filter_map(Node::as_error)
        .take(max_diagnostics)
        .map(|err| {
            let line = err.symbol().line();
            ParseDiagnostic::error(code.to_string(), format!("ANTLR error node at line {line}"))
        })
        .collect()
}
