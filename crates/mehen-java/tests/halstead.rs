// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Halstead tests for the ANTLR Java walker.
//!
//! Operators = keyword and punctuation/operator tokens; operands =
//! identifiers, literals, `this`, `super` (deduped by text). Whitespace,
//! comments, and EOF are skipped.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_java::JavaAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = JavaAnalyzer::new();
    let file = SourceFile::new("Foo.java".into(), Language::Java, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn operators_and_operands_are_counted() {
    let a = analyze(
        "class C {
             int add(int a, int b) { return a + b; }
         }",
    );
    let h = mehen_report::metrics_json::halstead(&a.root.metrics);
    // Non-zero vocabulary/volume proves operator+operand classification runs.
    assert!(h.n1 > 0.0, "distinct operators must be counted");
    assert!(h.n2 > 0.0, "distinct operands must be counted");
    assert!(h.volume > 0.0, "volume must be positive");
}

#[test]
fn contextual_keyword_used_as_identifier_is_an_operand() {
    // Regression (audit): `record`/`var`/`yield`/… used as a *name* lex as
    // dedicated tokens but are identifiers → Halstead operands, not operators.
    // A field named `record` should classify like a field named `plain`: same
    // operand count, same operator count.
    let named_kw = analyze("class C { int record = 5; }");
    let named_plain = analyze("class C { int plain = 5; }");
    let hk = mehen_report::metrics_json::halstead(&named_kw.root.metrics);
    let hp = mehen_report::metrics_json::halstead(&named_plain.root.metrics);
    assert_eq!(
        hk.n1, hp.n1,
        "a contextual keyword used as a name must not add a distinct operator"
    );
    assert_eq!(
        hk.n2, hp.n2,
        "a contextual keyword used as a name must be an operand like any identifier"
    );
}
