// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NArgs (declared parameter count) tests for the ANTLR Java walker.

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
fn counts_method_parameters() {
    let a = analyze(
        "class C {
             int add(int a, int b, int c) { return a + b + c; }
         }",
    );
    let nargs = mehen_report::metrics_json::nargs(&a.root.metrics);
    insta::assert_json_snapshot!(nargs, @r#"
    {
      "total_functions": 3.0,
      "total_closures": 0.0,
      "average_functions": 3.0,
      "average_closures": 0.0,
      "total": 3.0,
      "average": 3.0,
      "functions_min": 3.0,
      "functions_max": 3.0,
      "closures_min": 0.0,
      "closures_max": 0.0
    }
    "#);
}

#[test]
fn counts_lambda_parameters() {
    let a = analyze(
        "class C {
             java.util.function.BiFunction<Integer,Integer,Integer> add = (a, b) -> a + b;
         }",
    );
    let nargs = mehen_report::metrics_json::nargs(&a.root.metrics);
    insta::assert_json_snapshot!(nargs, @r#"
    {
      "total_functions": 0.0,
      "total_closures": 2.0,
      "average_functions": 0.0,
      "average_closures": 2.0,
      "total": 2.0,
      "average": 2.0,
      "functions_min": 0.0,
      "functions_max": 0.0,
      "closures_min": 2.0,
      "closures_max": 2.0
    }
    "#);
}

#[test]
fn varargs_parameter_counts() {
    let a = analyze(
        "class C {
             int sum(int first, int... rest) { return first; }
         }",
    );
    let nargs = mehen_report::metrics_json::nargs(&a.root.metrics);
    insta::assert_json_snapshot!(nargs, @r#"
    {
      "total_functions": 2.0,
      "total_closures": 0.0,
      "average_functions": 2.0,
      "average_closures": 0.0,
      "total": 2.0,
      "average": 2.0,
      "functions_min": 2.0,
      "functions_max": 2.0,
      "closures_min": 0.0,
      "closures_max": 0.0
    }
    "#);
}
