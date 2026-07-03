// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NExit (exit-point) tests for the ANTLR Java walker.
//!
//! `return` and `throw` statements count as exits; `break`/`continue` do not.

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
fn return_and_throw_count_as_exits() {
    let a = analyze(
        "class C {
             int f(int a) {
                 if (a < 0) { throw new IllegalArgumentException(); }
                 return a;
             }
         }",
    );
    let nexit = mehen_report::metrics_json::nexits(&a.root.metrics);
    insta::assert_json_snapshot!(nexit, @r#"
    {
      "sum": 2.0,
      "average": 2.0,
      "min": 0.0,
      "max": 2.0
    }
    "#);
}

#[test]
fn break_and_continue_are_not_exits() {
    let a = analyze(
        "class C {
             void f(int[] xs) {
                 for (int x : xs) {
                     if (x == 0) { continue; }
                     if (x < 0) { break; }
                 }
             }
         }",
    );
    let nexit = mehen_report::metrics_json::nexits(&a.root.metrics);
    insta::assert_json_snapshot!(nexit, @r#"
    {
      "sum": 0.0,
      "average": 0.0,
      "min": 0.0,
      "max": 0.0
    }
    "#);
}
