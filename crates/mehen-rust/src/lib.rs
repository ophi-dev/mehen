// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-rust` — Rust language analyzer.
//!
//! Phase 9 implementation: parses via `ra_ap_syntax` (rust-analyzer's
//! published syntax/parser stack) instead of tree-sitter-rust. The walker
//! lives in [`walker`] and follows the same per-space `State` accumulator
//! pattern used by `mehen-python` and `mehen-typescript`.
//!
//! See `docs/rust-ra-ap-syntax-spec.md` for the per-metric design rules
//! and the documented divergences from the legacy tree-sitter walker.

#![forbid(unsafe_code)]

mod walker;

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, LineIndex,
    ParseDiagnostic, Result, SourceFile,
};
use mehen_metrics::MetricEvidence;
use ra_ap_syntax::{Edition, SourceFile as RustSourceFile};

pub struct RustAnalyzer;

impl RustAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RustAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for RustAnalyzer {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::RaApSyntax
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        // ra_ap_syntax always returns a tree, even on parse errors. Errors
        // are surfaced through `parse.errors()`; we don't fail the
        // analysis on recoverable errors — the legacy tree-sitter
        // pipeline also produced metrics from partial trees. Recovered
        // errors are surfaced as `error` (not `warning`) so the
        // diagnostic contract (plan §9.3) treats the analysis as
        // incomplete: `mehen metrics` exits 1 and `analyze_diff`
        // records the file under `analysis_errors`.
        let parse = RustSourceFile::parse(&source.text, Edition::CURRENT);
        let file = parse.tree();
        let line_index = LineIndex::new(&source.text);
        let mut evidence = MetricEvidence::new("rust", config.emit_contributions);
        let root = walker::walk_source_file(&file, &source.text, &line_index, &mut evidence);
        let diagnostics: Vec<ParseDiagnostic> = parse
            .errors()
            .iter()
            .take(16)
            .map(|e| ParseDiagnostic::error("rust.syntax_error", e.to_string()))
            .collect();
        Ok(LanguageAnalysis {
            language: Language::Rust,
            backend: AnalysisBackend::RaApSyntax,
            diagnostics,
            root,
            contributions: evidence.finish(),
        })
    }
}
