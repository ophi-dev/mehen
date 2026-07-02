// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-php` — PHP language analyzer.
//!
//! Phase 8 implementation: walks `mago_syntax`'s typed PHP AST to
//! produce `LanguageAnalysis`. Replaces the tree-sitter-php pipeline
//! per the rewrite plan §6.4.
//!
//! Mago provides a `Walker` trait whose generated `walk_in_<node>`
//! / `walk_out_<node>` callbacks let us drive the per-space `State`
//! accumulator without hand-rolling recursion. See
//! `docs/php-mago-syntax-spec.md` for design rationale and every
//! documented divergence from the legacy tree-sitter behavior.
//!
//! Mago migrations from `mago-collector`'s walk pattern: we don't
//! need its `Collector` (that's a lint-issue / pragma collector for
//! Mago's analysis pipeline). The reusable piece is the `Walker`
//! trait in `mago_syntax::walker`, which we implement directly.

#![forbid(unsafe_code)]

mod walker;

use mago_allocator::LocalArena;
use mago_database::file::FileId;

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, ParseDiagnostic,
    Result, SourceFile,
};

pub struct PhpAnalyzer;

impl PhpAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PhpAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for PhpAnalyzer {
    fn language(&self) -> Language {
        Language::Php
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::Mago
    }

    fn analyze(&self, source: &SourceFile, _config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        // mago-syntax allocates everything into a bump arena. The
        // arena lives only for this `analyze` call; everything we
        // put into `LanguageAnalysis` must be owned (no borrow
        // back into the arena), which the per-space `State`
        // accumulator pattern already guarantees. As of mago 1.42
        // the parser writes into mago's own `LocalArena` rather than
        // the external `bumpalo::Bump`.
        let arena = LocalArena::new();
        let file_id = FileId::zero();
        let program =
            mago_syntax::parser::parse_file_content(&arena, file_id, source.text.as_bytes());

        // Recovered Mago syntax errors are surfaced as `error` (not
        // `warning`) so the diagnostic contract (plan §9.3) treats the
        // analysis as incomplete: `mehen metrics` exits 1 and
        // `analyze_diff` records the file under `analysis_errors`.
        let diagnostics: Vec<ParseDiagnostic> = program
            .errors
            .iter()
            .map(|err| ParseDiagnostic::error("php.parse_error", format!("mago-syntax: {err}")))
            .collect();

        let root = walker::walk_program(program, &source.text, &source.line_index);

        Ok(LanguageAnalysis {
            language: Language::Php,
            backend: AnalysisBackend::Mago,
            diagnostics,
            root,
            contributions: Vec::new(),
        })
    }
}
