// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-c` — C language analyzer.
//!
//! Drives a C-specific tree-sitter walker (`walker::walk_program`) that
//! mirrors every legacy `legacy::metrics::*::compute for CCode` arm
//! byte-identically. See `walker.rs` for the per-metric coverage notes.

#![forbid(unsafe_code)]

mod grammar;
mod walker;

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, ParseDiagnostic,
    Result, SourceFile, SourceSpan, byte_offset_clamped,
};
use mehen_tree_sitter::{MetricEvidence, TreeSitterParser, collect_recovered_errors, empty_space};

/// Tree-sitter `Language` accessor for `xtask tree-sitter generate`.
///
/// Exposed so the kind-enum generator reaches the grammar through this
/// crate instead of pinning `tree-sitter-c` itself, which kept xtask's
/// pin and the analyzer's pin in lockstep by hand. With this accessor,
/// the analyzer's pin is the single source of truth.
#[doc(hidden)]
pub fn __grammar_language() -> tree_sitter::Language {
    tree_sitter_c::LANGUAGE.into()
}

pub struct CAnalyzer;

impl CAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for CAnalyzer {
    fn language(&self) -> Language {
        Language::C
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::TreeSitter
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let parser = match TreeSitterParser::new(
            tree_sitter_c::LANGUAGE.into(),
            source.text.clone().into_bytes(),
        ) {
            Ok(p) => p,
            Err(e) => {
                let span = SourceSpan {
                    start_byte: 0,
                    end_byte: byte_offset_clamped(source.text.len()),
                    start_line: 1,
                    end_line: source.line_index.line_count(),
                };
                return Ok(LanguageAnalysis {
                    language: Language::C,
                    backend: AnalysisBackend::TreeSitter,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "c.parse_error",
                        format!("tree-sitter-c failed: {e}"),
                    )],
                    root: empty_space(span),
                    contributions: Vec::new(),
                });
            }
        };

        let mut evidence = MetricEvidence::new("c", config.emit_contributions);
        let root = walker::walk_program(
            parser.root(),
            parser.source(),
            &source.line_index,
            &mut evidence,
        );
        // Tree-sitter recovers from syntax errors by inserting ERROR /
        // missing nodes; surface them as `error` diagnostics so the
        // metric output can't masquerade as clean (plan §9.3).
        let diagnostics = collect_recovered_errors(parser.root(), "c.syntax_error", 16);
        // Per-space McCabe base rows (+1 per space, unit included) so
        // cyclomatic evidence sums to `cyclomatic.sum` — decisions alone
        // cannot explain the rolled-up value (an empty function moves it).
        evidence.record_cyclomatic_bases(&root);
        Ok(LanguageAnalysis {
            language: Language::C,
            backend: AnalysisBackend::TreeSitter,
            diagnostics,
            root,
            contributions: evidence.finish(),
        })
    }
}
