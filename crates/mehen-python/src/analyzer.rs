// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, ParseDiagnostic,
    Result, SourceFile, SourceSpan, byte_offset_clamped,
};
use mehen_metrics::{MetricEvidence, MetricTreeBuilder};
use ruff_python_parser::parse_module;

use crate::walker::walk_module;

/// Ruff-backed Python analyzer (Phase 6, see `docs/python-ruff-spec.md`).
///
/// Replaces the tree-sitter-python analyzer with `ruff_python_parser` +
/// `ruff_python_ast`. The Ruff AST is richer than the tree-sitter CST in
/// ways that change a small number of metrics — every drift is justified
/// from the metric definition rather than from a desire to mirror the
/// legacy walker. See the spec doc for the full ledger.
pub struct PythonAnalyzer;

impl PythonAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PythonAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for PythonAnalyzer {
    fn language(&self) -> Language {
        Language::Python
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::PythonRuff
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let parsed = match parse_module(source.text.as_str()) {
            Ok(p) => p,
            Err(err) => {
                let span = SourceSpan {
                    start_byte: 0,
                    end_byte: byte_offset_clamped(source.text.len()),
                    start_line: 1,
                    end_line: source.line_index.line_count(),
                };
                let mut tree = MetricTreeBuilder::new(span);
                let _ = tree.metrics_mut();
                return Ok(LanguageAnalysis {
                    language: Language::Python,
                    backend: AnalysisBackend::PythonRuff,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "python.parse_error",
                        format!("ruff_python_parser failed: {err}"),
                    )],
                    root: tree.finish(),
                    contributions: Vec::new(),
                });
            }
        };

        let mut evidence = MetricEvidence::new("python", config.emit_contributions);
        let root = walk_module(&parsed, &source.text, &source.line_index, &mut evidence);
        // Recovered Ruff syntax errors are surfaced as `error` (not
        // `warning`) so the diagnostic contract (plan §9.3) treats the
        // analysis as incomplete: `mehen metrics` exits 1 and
        // `analyze_diff` records the file under `analysis_errors`.
        let diagnostics = parsed
            .errors()
            .iter()
            .map(|e| ParseDiagnostic::error("python.syntax_error", format!("{}", e)))
            .collect();
        // Per-space McCabe base rows (+1 per space, unit included) so
        // cyclomatic evidence sums to `cyclomatic.sum` — decisions alone
        // cannot explain the rolled-up value (an empty function moves it).
        evidence.record_cyclomatic_bases(&root);
        Ok(LanguageAnalysis {
            language: Language::Python,
            backend: AnalysisBackend::PythonRuff,
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
        let analyzer = PythonAnalyzer::new();
        let file = SourceFile::new("test.py".into(), Language::Python, source.to_string());
        analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
    }

    #[test]
    fn empty_file_yields_root_unit() {
        let a = analyze("");
        assert_eq!(a.root.kind, SpaceKind::Unit);
        assert!(a.root.spaces.is_empty());
    }

    #[test]
    fn def_creates_function_space() {
        let a = analyze("def foo():\n    pass\n");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Function);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("foo"));
    }

    #[test]
    fn class_creates_class_space_with_method() {
        let a = analyze("class C:\n    def m(self):\n        pass\n");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("C"));
        assert_eq!(a.root.spaces[0].spaces.len(), 1);
        assert_eq!(a.root.spaces[0].spaces[0].kind, SpaceKind::Function);
    }

    #[test]
    fn cyclomatic_counts_decision_points() {
        // Function: 1 (base) + if + or + elif = 4
        let a =
            analyze("def f(x):\n    if x or x:\n        return 1\n    elif x:\n        return 2\n");
        let func = &a.root.spaces[0];
        let cyclomatic = func
            .metrics
            .get(&MetricKey::new(keys::CYCLOMATIC))
            .unwrap()
            .as_f64();
        assert!(cyclomatic >= 4.0, "expected >= 4, got {cyclomatic}");
    }
}
