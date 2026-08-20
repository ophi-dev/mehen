// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-go` — Go language analyzer.
//!
//! Phase-3 reorganization complete: the analyzer owns its tree-sitter
//! cursor walk locally (`walker.rs`) and the language-specific kind
//! enum (`grammar.rs`) instead of relying on the shared
//! `mehen-tree-sitter::walker::LanguageRules` plug-in. This mirrors the
//! per-language crate shape used by `mehen-ruby`, `mehen-python`,
//! `mehen-typescript`, and `mehen-php`. Per the rewrite plan §6.1 Go
//! stays on tree-sitter for 1.0 — only the *interpretation* moves.

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
/// crate instead of pinning `tree-sitter-go` itself.
#[doc(hidden)]
pub fn __grammar_language() -> tree_sitter::Language {
    tree_sitter_go::LANGUAGE.into()
}

pub struct GoAnalyzer;

impl GoAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GoAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for GoAnalyzer {
    fn language(&self) -> Language {
        Language::Go
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::TreeSitter
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let parser = match TreeSitterParser::new(
            tree_sitter_go::LANGUAGE.into(),
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
                    language: Language::Go,
                    backend: AnalysisBackend::TreeSitter,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "go.parse_error",
                        format!("tree-sitter-go failed: {e}"),
                    )],
                    root: empty_space(span),
                    contributions: Vec::new(),
                });
            }
        };

        let mut evidence = MetricEvidence::new("go", config.emit_contributions);
        let root = walker::walk_program(
            parser.root(),
            parser.source(),
            &source.line_index,
            &mut evidence,
        );
        // Tree-sitter recovers from syntax errors by inserting ERROR /
        // missing nodes; surface them as `error` diagnostics so the
        // metric output can't masquerade as clean (plan §9.3).
        let diagnostics = collect_recovered_errors(parser.root(), "go.syntax_error", 16);
        Ok(LanguageAnalysis {
            language: Language::Go,
            backend: AnalysisBackend::TreeSitter,
            diagnostics,
            root,
            contributions: evidence.finish(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, MetricKey, SourceFile, SpaceKind};
    use mehen_metrics::keys;

    fn analyze(source: &str) -> LanguageAnalysis {
        GoAnalyzer::new()
            .analyze(
                &SourceFile::new("a.go".into(), Language::Go, source.to_string()),
                &AnalysisConfig::default(),
            )
            .unwrap()
    }

    #[test]
    fn func_creates_function_space() {
        let a = analyze("package main\nfunc Foo() int { return 1 }\n");
        assert!(a.root.spaces.iter().any(|s| s.kind == SpaceKind::Function));
    }

    #[test]
    fn cyclomatic_counts_branches() {
        let a = analyze(
            "package main\nfunc f(x int) int { if x > 0 && x < 10 { return 1 }; return 2 }\n",
        );
        let func = a
            .root
            .spaces
            .iter()
            .find(|s| s.kind == SpaceKind::Function)
            .unwrap();
        let cy = func
            .metrics
            .get(&MetricKey::new(keys::CYCLOMATIC))
            .unwrap()
            .as_f64();
        assert!(cy >= 3.0, "expected >= 3, got {cy}");
    }
}
