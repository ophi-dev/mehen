// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-engine` — pipeline orchestration.
//!
//! This crate owns:
//! - language analyzer registry,
//! - language detection by extension and content,
//! - the public engine APIs (`analyze_metrics`, `analyze_diff`,
//!   `rank_top_offenders`),
//! - per-file concurrency (per the rewrite plan §4.6: per-file analysis is
//!   the parallelism unit; analyzers are constructed per worker; parser
//!   arenas live for one analyze call),
//! - the only `LanguageDispatcher` implementation in 1.0, exposed to
//!   `mehen-markdown` for the embedded-code path.
//!
//! Phase 1 wired the registry and the dispatcher; Phase 5 added the
//! `analyze_diff` and `rank_top_offenders` orchestrators. The per-file
//! parallelism unit and recursion/depth limits land in follow-up
//! commits; this implementation keeps each operation single-threaded
//! and predictable.

#![deny(unsafe_code)]

pub mod ci;
mod concurrent_files;
mod detection;
mod diff;
mod dispatcher;
mod git_attributes;
mod metric_selector;
mod registry;
mod top_offenders;

pub use diff::{DiffOpts, run_diff};
pub use top_offenders::{TopOffendersOpts, run_top_offenders};

/// Register the embedded-code dispatch callback the moved
/// [`mehen_markdown::analyze_markdown`] uses to fold fenced source
/// snippets into Markdown metrics. Idempotent — backed by a
/// `OnceLock` inside `mehen-markdown`, so repeat calls are silent
/// no-ops.
///
/// Every supported fence language is now backed by a per-language
/// analyzer crate, so this dispatch path goes straight through the
/// new `AnalyzerRegistry`.
pub fn init_markdown() {
    mehen_markdown::set_embedded_dispatch(markdown_dispatch::dispatch);
}

mod markdown_dispatch {
    use mehen_markdown::{EmbeddedFenceMetrics, FenceLanguage};

    use crate::AnalyzerRegistry;

    /// Run the AnalyzerRegistry against a fence body. Every supported
    /// fence language now has a per-language analyzer crate, so this
    /// is the only dispatch path Markdown needs.
    ///
    /// The registry is shared across calls via a process-wide
    /// `OnceLock`: `dispatch` is the `mehen_markdown::DispatchFn`
    /// callback (a bare `fn` pointer that can't capture state), and
    /// every fenced code block in a Markdown document drives this
    /// function once. Without the cache each fence rebuilt the
    /// per-language factory `Vec` from scratch — measurable overhead
    /// on documents with hundreds of fences.
    pub(super) fn dispatch(lang: FenceLanguage, body: String) -> Option<EmbeddedFenceMetrics> {
        use std::sync::OnceLock;

        use mehen_core::{AnalysisConfig, MetricKey, SourceFile, keys};

        static REGISTRY: OnceLock<AnalyzerRegistry> = OnceLock::new();
        let language = language_for(lang);
        let registry = REGISTRY.get_or_init(AnalyzerRegistry::default_set);
        let analyzer = registry.analyzer_for(language)?;
        let path = camino::Utf8PathBuf::try_from(synthetic_path(lang)).ok()?;
        let source = SourceFile::new(path, language, body);
        let analysis = analyzer.analyze(&source, &AnalysisConfig::default()).ok()?;
        // Migrated analyzers can return `Ok(...)` with a partial tree
        // alongside an `Error`/`Fatal` diagnostic when the fence body
        // doesn't parse cleanly. Per §9.3 those analyses are
        // incomplete; folding their numeric metrics back into Markdown
        // would silently skew embedded scores.
        if crate::diff::has_blocking_diagnostic(&analysis.diagnostics) {
            return None;
        }
        let read = |key: &str| {
            analysis
                .root
                .metrics
                .get(&MetricKey::new(key))
                .map(|v| v.as_f64())
                .unwrap_or(0.0)
        };
        Some(EmbeddedFenceMetrics {
            volume: read(keys::HALSTEAD_VOLUME),
            cognitive_sum: read("cognitive.sum"),
            sloc: read(keys::LOC_SLOC),
        })
    }

    fn synthetic_path(lang: FenceLanguage) -> std::path::PathBuf {
        let name = match lang {
            FenceLanguage::Rust => "fence.rs",
            FenceLanguage::Python => "fence.py",
            FenceLanguage::Typescript => "fence.ts",
            FenceLanguage::Tsx => "fence.tsx",
            FenceLanguage::Go => "fence.go",
            FenceLanguage::Ruby => "fence.rb",
            // A fenced Kotlin snippet is script-like — it commonly contains
            // top-level statements (`println(...)`), which the `.kt`
            // compilation-unit grammar rejects. Use `.kts` so the Kotlin
            // analyzer selects the `script` entry rule (a superset that also
            // accepts top-level declarations), avoiding a cascade of recovered
            // syntax errors that would otherwise drop the fence's metrics.
            FenceLanguage::Kotlin => "fence.kts",
            FenceLanguage::Java => "fence.java",
            FenceLanguage::Powershell => "fence.ps1",
            // `.csx` (script) rather than `.cs`: a fence is usually a snippet of
            // top-level statements, which Roslyn's `compilation_unit` accepts via
            // `global_statement`, and both extensions route to the same analyzer.
            FenceLanguage::CSharp => "fence.csx",
            FenceLanguage::C => "fence.c",
            FenceLanguage::Php => "fence.php",
        };
        std::path::PathBuf::from(name)
    }

    fn language_for(lang: FenceLanguage) -> mehen_core::Language {
        use mehen_core::Language;
        match lang {
            FenceLanguage::Rust => Language::Rust,
            FenceLanguage::Python => Language::Python,
            FenceLanguage::Typescript => Language::TypeScript,
            FenceLanguage::Tsx => Language::Tsx,
            FenceLanguage::Go => Language::Go,
            FenceLanguage::Ruby => Language::Ruby,
            FenceLanguage::Kotlin => Language::Kotlin,
            FenceLanguage::Java => Language::Java,
            FenceLanguage::Powershell => Language::PowerShell,
            FenceLanguage::CSharp => Language::CSharp,
            FenceLanguage::C => Language::C,
            FenceLanguage::Php => Language::Php,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A fence body whose Python code has a hard syntax error.
        /// Ruff returns `Ok(LanguageAnalysis)` with a partial tree
        /// plus an `Error`-severity diagnostic, so the legacy
        /// pre-fix dispatcher would have folded its (mostly-zero
        /// but nonzero `loc.sloc`) numbers into the Markdown
        /// embedded score.
        #[test]
        fn registry_dispatch_drops_blocking_diagnostic_python() {
            let bad = "def f(:\n    return 1\n".to_string();
            assert!(dispatch(FenceLanguage::Python, bad).is_none());
        }

        #[test]
        fn registry_dispatch_keeps_clean_python() {
            let good = "def f():\n    return 1\n".to_string();
            assert!(dispatch(FenceLanguage::Python, good).is_some());
        }

        /// A C# fence must reach the analyzer. Registering `Language::CSharp` in
        /// the registry is not enough on its own: the fence tag has to map to a
        /// `FenceLanguage` variant, and that variant needs both a synthetic path
        /// and a `Language` mapping here, or the fence is silently skipped and
        /// its volume/complexity/SLOC never reach the Markdown score.
        #[test]
        fn registry_dispatch_analyzes_csharp_fence() {
            let good = "class C { void M() { } }\n".to_string();
            assert!(dispatch(FenceLanguage::CSharp, good).is_some());
        }

        /// A fence is usually a snippet of top-level statements rather than a
        /// full compilation unit, which the Roslyn-derived grammar accepts via
        /// `global_statement` (C# 9). Pinned because the synthetic path is
        /// `fence.csx` for exactly this reason.
        #[test]
        fn registry_dispatch_analyzes_csharp_top_level_statements() {
            let script = "var total = 1 + 2;\n".to_string();
            assert!(dispatch(FenceLanguage::CSharp, script).is_some());
        }
    }
}

pub use detection::detect_language;
pub use diff::analyze_diff;
pub use dispatcher::EngineDispatcher;
pub use mehen_core::{
    AnalysisErrorRecord, AnalyzeMetricsInput, DiffFile, DiffInput, DiffReport, DiffSide,
    MetricsReport, TopOffenderEntry, TopOffendersInput, TopOffendersReport,
};
pub use registry::{AnalyzerRegistry, RegistryError};
pub use top_offenders::rank_top_offenders;

use mehen_core::{AnalysisError, Result};

/// Run a single-file analysis using the default registry.
///
/// The returned report has its `path` populated from the input, so callers
/// don't need to set it manually after the conversion from
/// `LanguageAnalysis` (`LanguageAnalysis` itself does not carry the path).
///
/// Phase 1 implementation; Phase 5 expands this to the full `mehen metrics`
/// orchestration (output formatting, diagnostics → exit codes, …).
pub fn analyze_metrics(input: AnalyzeMetricsInput) -> Result<MetricsReport> {
    let registry = AnalyzerRegistry::default_set();
    let path = input.source.path.clone();
    let analyzer = registry
        .analyzer_for(input.source.language)
        .ok_or(AnalysisError::AnalyzerUnavailable(input.source.language))?;
    let analysis = analyzer.analyze(&input.source, &input.config)?;
    let mut report = MetricsReport::from(analysis);
    report.path = path;
    Ok(report)
}
