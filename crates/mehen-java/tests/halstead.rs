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

#[test]
fn method_own_line_modifiers_belong_to_the_method() {
    // Regression (PR #160 review): a method's own-line modifiers/annotations
    // (`@Deprecated\npublic void m() {}`) are siblings of the declaration on
    // the `classBodyDeclaration` wrapper. The method space is opened at the
    // wrapper so those tokens are walked *inside* the method — counting toward
    // its Halstead (and PLOC), not only the enclosing class. An annotated
    // method must have strictly more Halstead length than the same method with
    // the annotation removed.
    fn method_len(sp: &mehen_core::MetricSpace) -> Option<f64> {
        if sp.kind == mehen_core::SpaceKind::Function {
            return Some(
                sp.metrics
                    .get(&mehen_core::MetricKey::new("halstead.N1"))
                    .map(|m| m.as_f64())
                    .unwrap_or(0.0),
            );
        }
        sp.spaces.iter().find_map(method_len)
    }
    let plain = analyze("class C {\n  public void m() {\n    x();\n  }\n}");
    let annotated = analyze("class C {\n  @Deprecated\n  public void m() {\n    x();\n  }\n}");
    let p = method_len(&plain.root).expect("method space");
    let a = method_len(&annotated.root).expect("method space");
    assert!(
        a > p,
        "an annotated method must count the annotation tokens in its Halstead: \
         annotated N1={a} vs plain N1={p}"
    );

    // The method's PLOC must include the annotation line too (consistent with
    // Halstead — both derive from the same tokens now visited inside the space).
    fn method_ploc(sp: &mehen_core::MetricSpace) -> Option<f64> {
        if sp.kind == mehen_core::SpaceKind::Function {
            return Some(
                sp.metrics
                    .get(&mehen_core::MetricKey::new("loc.ploc"))
                    .map(|m| m.as_f64())
                    .unwrap_or(0.0),
            );
        }
        sp.spaces.iter().find_map(method_ploc)
    }
    assert_eq!(
        method_ploc(&annotated.root),
        Some(4.0),
        "the annotation row is part of the method's PLOC"
    );
    assert_eq!(method_ploc(&plain.root), Some(3.0));
}
