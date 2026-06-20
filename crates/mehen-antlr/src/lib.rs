// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-antlr` — shared support for ANTLR-backed analyzer crates.
//!
//! This is the ANTLR peer of [`mehen-tree-sitter`]. It does **not** own any
//! language's semantics — anything that interprets a rule index or token
//! type belongs in the owning `mehen-<lang>` crate. It provides the plumbing
//! every ANTLR-backed analyzer needs:
//!
//! - **runtime re-export** ([`runtime`]) so analyzer crates depend on the
//!   ANTLR runtime through this crate and never pin its version themselves,
//! - **span conversion** ([`span`]) bridging ANTLR's char-index positions to
//!   mehen's byte-offset [`SourceSpan`](mehen_core::SourceSpan), correct for
//!   non-ASCII source,
//! - **diagnostics** ([`diagnostics`]) turning recovered `ParseTree::Error`
//!   leaves into mehen [`ParseDiagnostic`](mehen_core::ParseDiagnostic)s,
//! - **comment LOC** ([`comments`]) recovering CLOC from the hidden-channel
//!   token stream (comments are absent from the parse tree).
//!
//! Each ANTLR-backed analyzer owns its own recursive walk over the
//! [`ParseTree`](runtime::ParseTree) — matching the per-language `Visitor`
//! pattern that `mehen-rust` and `mehen-ruby` use — because metric
//! interpretation (which rule opens a space, how cognitive nesting is
//! threaded) is language-specific and ANTLR's parent-less tree means
//! parent context has to be threaded top-down by the owning walker. This
//! crate deliberately does *not* impose a generic walker; a shared walker
//! can be extracted once a second ANTLR grammar shows what is truly common.
//!
//! ## Why ANTLR is a first-class backend
//!
//! mehen already runs analyzers on non-tree-sitter parsers (`ra_ap_syntax`
//! for Rust, Prism for Ruby, Ruff for Python, Oxc for TS). The
//! [`LanguageAnalyzer`](mehen_core::LanguageAnalyzer) trait is parser-neutral
//! and [`AnalysisBackend`](mehen_core::AnalysisBackend) is an open enum.
//! ANTLR slots in as a peer backend: an analyzer parses with a generated
//! ANTLR parser, walks the resulting [`ParseTree`](runtime::ParseTree), and
//! returns an owned `LanguageAnalysis`. The generated parser/lexer modules
//! are produced offline by `cargo xtask antlr generate <lang>` from a
//! vendored `.g4` grammar — the same generate-and-check-in workflow used
//! for tree-sitter kind enums.

#![forbid(unsafe_code)]

mod comments;
mod diagnostics;
mod span;

use mehen_core::{MetricSpace, SourceSpan, SpaceId, SpaceKind};

/// Re-export of the ANTLR v4 Rust runtime (`antlr4_runtime`, the library of
/// the `antlr-rust-runtime` package). Generated parser/lexer modules and
/// analyzer crates reach the runtime through this path so the version is
/// pinned in exactly one place ([`mehen-antlr`'s `Cargo.toml`]).
pub use antlr4_runtime as runtime;

pub use comments::{CommentRows, comment_rows};
pub use diagnostics::{child_rule, collect_errors, has_child_token};
pub use span::{CharByteMap, ctx_span, span_from_char_range};

/// Build an "empty" unit space — used by analyzers when the parser fails
/// before any walk can happen.
pub fn empty_space(span: SourceSpan) -> MetricSpace {
    MetricSpace::new(SpaceId(0), SpaceKind::Unit, span)
}
