// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-ruby` — Ruby language analyzer.
//!
//! Phase 9 implementation: ruby-prism-backed walker. Replaces the
//! Phase-3 tree-sitter-ruby analyzer. Per `docs/ruby-prism-spec.md`,
//! every metric is computed from the Prism AST. Ruby-specific
//! behaviour (modifier forms, `rescue` modifier, `case…in` pattern
//! matching, blocks vs lambdas, numbered/`it` block parameters,
//! singleton classes, ivar conventions) is documented in that spec.

#![forbid(unsafe_code)]

mod walker;

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, ParseDiagnostic,
    Result, SourceFile,
};
use mehen_metrics::MetricEvidence;

/// Ruby Prism analyzer (Phase 9, see `docs/ruby-prism-spec.md`).
pub struct RubyAnalyzer;

impl RubyAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RubyAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for RubyAnalyzer {
    fn language(&self) -> Language {
        Language::Ruby
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::Prism
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let parse = ruby_prism::parse(source.text.as_bytes());
        let mut evidence = MetricEvidence::new("ruby", config.emit_contributions);
        let root = walker::walk_program(&parse, &source.text, &source.line_index, &mut evidence);
        // Recovered Prism syntax errors are surfaced as `error` (not
        // `warning`) so the diagnostic contract (plan §9.3) treats the
        // analysis as incomplete: `mehen metrics` exits 1 and
        // `analyze_diff` records the file under `analysis_errors`.
        let diagnostics: Vec<ParseDiagnostic> = parse
            .errors()
            .map(|e| ParseDiagnostic::error("ruby.syntax_error", e.message().to_string()))
            .collect();
        Ok(LanguageAnalysis {
            language: Language::Ruby,
            backend: AnalysisBackend::Prism,
            diagnostics,
            root,
            contributions: evidence.finish(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, SourceFile, SpaceKind};

    fn analyze(source: &str, path: &str) -> LanguageAnalysis {
        let analyzer = RubyAnalyzer::new();
        let file = SourceFile::new(path.into(), Language::Ruby, source.to_string());
        analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
    }

    #[test]
    fn empty_file_yields_root_unit() {
        let a = analyze("", "test.rb");
        assert_eq!(a.root.kind, SpaceKind::Unit);
        assert!(a.root.spaces.is_empty());
    }

    #[test]
    fn def_creates_function_space() {
        let a = analyze("def foo\n  1\nend\n", "test.rb");
        assert!(a.root.spaces.iter().any(|s| s.kind == SpaceKind::Function));
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("foo"));
    }

    #[test]
    fn class_creates_class_space_with_method() {
        let a = analyze("class C\n  def m; end\nend\n", "test.rb");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("C"));
        assert_eq!(a.root.spaces[0].spaces.len(), 1);
        assert_eq!(a.root.spaces[0].spaces[0].kind, SpaceKind::Function);
    }
}
