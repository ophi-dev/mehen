// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC tests for the ANTLR Java walker.
//!
//! PLOC = physical code lines; LLOC = logical (statement/declaration) lines;
//! CLOC = comment lines (block `COMMENT` + `LINE_COMMENT`, routed to the
//! deepest enclosing space); SLOC = source lines.

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
fn simple_loc() {
    let a = analyze(
        "package demo;\n\
         // a line comment\n\
         class C {\n\
         \x20   /* a block comment */\n\
         \x20   int f() {\n\
         \x20       return 1;\n\
         \x20   }\n\
         }\n",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    insta::assert_json_snapshot!(loc, @r#"
    {
      "sloc": 8.0,
      "ploc": 6.0,
      "lloc": 4.0,
      "cloc": 2.0,
      "blank": 0.0,
      "sloc_average": 2.6666666666666665,
      "ploc_average": 2.0,
      "lloc_average": 1.3333333333333333,
      "cloc_average": 0.6666666666666666,
      "blank_average": 0.0,
      "sloc_min": 3.0,
      "sloc_max": 3.0,
      "cloc_min": 0.0,
      "cloc_max": 0.0,
      "ploc_min": 3.0,
      "ploc_max": 3.0,
      "lloc_min": 2.0,
      "lloc_max": 2.0,
      "blank_min": 0.0,
      "blank_max": 0.0
    }
    "#);
}

#[test]
fn classic_for_header_is_one_lloc_like_enhanced_for() {
    // Regression (audit): a classic `for (int i=…; …)` must not double-count
    // its initializer declaration as a second LLOC — it should match the
    // equivalent enhanced-for's LLOC.
    let classic = analyze(
        "class C {
             void f() {
                 for (int i = 0; i < 10; i++) { g(i); }
             }
         }",
    );
    let enhanced = analyze(
        "class C {
             void f(int[] xs) {
                 for (int x : xs) { g(x); }
             }
         }",
    );
    let c = serde_json::to_value(mehen_report::metrics_json::loc(&classic.root.metrics)).unwrap();
    let e = serde_json::to_value(mehen_report::metrics_json::loc(&enhanced.root.metrics)).unwrap();
    assert_eq!(
        c["lloc"], e["lloc"],
        "classic-for and enhanced-for should report the same LLOC"
    );
}

#[test]
fn empty_and_labeled_statements_are_not_their_own_lloc() {
    // Regression (audit): a bare `;` (empty statement) and a label wrapper
    // (`lbl: stmt`) are not their own logical lines. Here the only LLOC-bearing
    // statements are the two `g(...)` calls inside the labeled loop and the
    // plain call, matching an equivalent body without `;`/labels.
    let with_noise = analyze(
        "class C {
             void f() {
                 ;
                 lbl: for (int i = 0; i < 2; i++) { g(i); }
                 ;
             }
         }",
    );
    let without = analyze(
        "class C {
             void f() {
                 for (int i = 0; i < 2; i++) { g(i); }
             }
         }",
    );
    let a =
        serde_json::to_value(mehen_report::metrics_json::loc(&with_noise.root.metrics)).unwrap();
    let b = serde_json::to_value(mehen_report::metrics_json::loc(&without.root.metrics)).unwrap();
    assert_eq!(
        a["lloc"], b["lloc"],
        "empty statements and labels must not inflate LLOC"
    );
}

#[test]
fn interface_and_annotation_members_count_as_lloc() {
    // Regression (PR #160 review): an interface method
    // (`interfaceCommonBodyDeclaration`) and an annotation element
    // (`annotationMethodRest`) are declaration nodes and must count as LLOC,
    // just like a class abstract method — an interface API should not
    // under-report logical LOC vs the equivalent abstract class.
    let iface = analyze("interface I { void m(); }");
    let cls = analyze("abstract class C { abstract void m(); }");
    let i = serde_json::to_value(mehen_report::metrics_json::loc(&iface.root.metrics)).unwrap();
    let c = serde_json::to_value(mehen_report::metrics_json::loc(&cls.root.metrics)).unwrap();
    assert_eq!(
        i["lloc"], c["lloc"],
        "interface method LLOC should match the equivalent abstract class method"
    );
    // Annotation element + constant each count: type decl + method + constant.
    let anno = analyze("@interface An { String v(); int X = 1; }");
    let av = serde_json::to_value(mehen_report::metrics_json::loc(&anno.root.metrics)).unwrap();
    assert_eq!(av["lloc"], serde_json::json!(3.0));
}

#[test]
fn block_only_statement_is_not_its_own_lloc() {
    // A bare `{ … }` block statement is not a logical line; the inner
    // statements each count.
    let a = analyze(
        "class C {
             void f() {
                 {
                     int x = 1;
                 }
             }
         }",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    insta::assert_json_snapshot!(loc, @r#"
    {
      "sloc": 7.0,
      "ploc": 7.0,
      "lloc": 3.0,
      "cloc": 0.0,
      "blank": 0.0,
      "sloc_average": 2.3333333333333335,
      "ploc_average": 2.3333333333333335,
      "lloc_average": 1.0,
      "cloc_average": 0.0,
      "blank_average": 0.0,
      "sloc_min": 5.0,
      "sloc_max": 5.0,
      "cloc_min": 0.0,
      "cloc_max": 0.0,
      "ploc_min": 5.0,
      "ploc_max": 5.0,
      "lloc_min": 2.0,
      "lloc_max": 2.0,
      "blank_min": 0.0,
      "blank_max": 0.0
    }
    "#);
}
