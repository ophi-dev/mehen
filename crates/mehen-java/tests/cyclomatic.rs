// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Cyclomatic complexity tests for the ANTLR Java walker.
//!
//! Decisions (SonarJava-aligned): `if`, every loop (`for`/`while`/`do`), each
//! `case` label, the ternary `?`, and each short-circuit `&&`/`||`. `switch`
//! itself, `catch`, `else`, and `try` are not decisions. Every method space
//! contributes a base McCabe `+1`, as does the enclosing class space — so the
//! unit `sum` folds in the class(1) and each method's McCabe value.

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
fn simple_if() {
    // unit(1) + class(1) + method(1 + 1 if) = 4
    let a = analyze(
        "class C {
             int f(int a, int b) {
                 if (a > b) { return a; }
                 return b;
             }
         }",
    );
    let cy = mehen_report::metrics_json::cyclomatic(&a.root.metrics);
    insta::assert_json_snapshot!(cy, @r###"
    {
      "sum": 4.0,
      "average": 1.3333333333333333,
      "min": 1.0,
      "max": 2.0
    }
    "###);
}

#[test]
fn logical_operators() {
    // method McCabe = 1 + if(1) + &&(1) + ||(1) = 4; unit sum = 1 + class(1) + 4
    let a = analyze(
        "class C {
             boolean check(boolean a, boolean b, boolean c) {
                 if (a && b || c) { return true; }
                 return false;
             }
         }",
    );
    let cy = mehen_report::metrics_json::cyclomatic(&a.root.metrics);
    insta::assert_json_snapshot!(cy, @r###"
    {
      "sum": 6.0,
      "average": 2.0,
      "min": 1.0,
      "max": 4.0
    }
    "###);
}

#[test]
fn switch_cases_count_not_switch_or_default() {
    // method McCabe = 1 + case(1) + case(1) = 3 (`default` does not count).
    let a = analyze(
        "class C {
             int g(int x) {
                 switch (x) {
                     case 1: return 1;
                     case 2: return 2;
                     default: return 0;
                 }
             }
         }",
    );
    let cy = mehen_report::metrics_json::cyclomatic(&a.root.metrics);
    insta::assert_json_snapshot!(cy, @r###"
    {
      "sum": 5.0,
      "average": 1.6666666666666667,
      "min": 1.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn try_catch_is_not_a_decision() {
    // `try`/`catch` add no cyclomatic decision (matches SonarJava): method
    // McCabe stays 1. unit sum = 1 + class(1) + 1.
    let a = analyze(
        "class C {
             void f() {
                 try { risky(); } catch (Exception e) { }
             }
         }",
    );
    let cy = mehen_report::metrics_json::cyclomatic(&a.root.metrics);
    insta::assert_json_snapshot!(cy, @r###"
    {
      "sum": 3.0,
      "average": 1.0,
      "min": 1.0,
      "max": 1.0
    }
    "###);
}

#[test]
fn ternary_counts_as_decision() {
    // method McCabe = 1 + ternary(1) = 2.
    let a = analyze(
        "class C {
             int f(int a) {
                 return a > 0 ? a : -a;
             }
         }",
    );
    let cy = mehen_report::metrics_json::cyclomatic(&a.root.metrics);
    insta::assert_json_snapshot!(cy, @r###"
    {
      "sum": 4.0,
      "average": 1.3333333333333333,
      "min": 1.0,
      "max": 2.0
    }
    "###);
}

#[test]
fn annotation_value_expressions_do_not_add_complexity() {
    // Regression (PR #160 review): annotation values are compile-time metadata,
    // not executable code. A composed constant in an annotation value
    // (`@Ann(value = true && false)`, `@Ann(x = c ? 1 : 2)`) must NOT record
    // cyclomatic decisions (nor cognitive/ABC) — it would otherwise inflate the
    // annotated method/class complexity. The annotated method scores the same
    // as the un-annotated one.
    let plain = analyze("class C { void m() {} }");
    let p =
        serde_json::to_value(mehen_report::metrics_json::cyclomatic(&plain.root.metrics)).unwrap();
    for src in [
        "class C { @Ann(value = true && false) void m() {} }",
        "class C { @Ann(x = cond ? 1 : 2) void m() {} }",
        "class C { @Ann(flags = A || B || C) void m() {} }",
    ] {
        let a = analyze(src);
        let av =
            serde_json::to_value(mehen_report::metrics_json::cyclomatic(&a.root.metrics)).unwrap();
        assert_eq!(
            av["sum"], p["sum"],
            "annotation-value expressions must not add cyclomatic complexity: {src}"
        );
    }
    // Guard: a real `&&` in the method *body* still counts.
    let with_body = analyze(
        "class C { @Ann(value = true) boolean m(boolean a, boolean b) { return a && b; } }",
    );
    let wb = serde_json::to_value(mehen_report::metrics_json::cyclomatic(
        &with_body.root.metrics,
    ))
    .unwrap();
    assert!(
        wb["sum"].as_f64().unwrap() > p["sum"].as_f64().unwrap(),
        "a real decision in an annotated method's body still counts"
    );
}

#[test]
fn annotation_element_default_expressions_do_not_add_complexity() {
    // Regression (PR #160 review): an annotation element's DEFAULT value
    // (`@interface A { boolean v() default true && false; }`) is metadata too,
    // parsed under `annotationMethodRest → defaultValue → elementValue →
    // expression` — NOT under `RULE_ANNOTATION`. The `in_annotation` guard must
    // also trigger on `defaultValue`, or a composed constant in a default
    // inflates the annotation method's cyclomatic/cognitive/ABC.
    let plain = analyze("@interface A { boolean v(); }");
    let p =
        serde_json::to_value(mehen_report::metrics_json::cyclomatic(&plain.root.metrics)).unwrap();
    for src in [
        "@interface A { boolean v() default true && false; }",
        "@interface A { int v() default cond ? 1 : 2; }",
    ] {
        let a = analyze(src);
        let av =
            serde_json::to_value(mehen_report::metrics_json::cyclomatic(&a.root.metrics)).unwrap();
        assert_eq!(
            av["sum"], p["sum"],
            "an annotation element default expression must not add complexity: {src}"
        );
    }
}
