// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NOM (number of methods) tests for the ANTLR Java walker.
//!
//! Every method/constructor is a function space; every lambda is a closure.
//! Interface methods (abstract and `default`) count exactly once (reached via
//! `interfaceCommonBodyDeclaration`).

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile, SpaceKind};
use mehen_java::JavaAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = JavaAnalyzer::new();
    let file = SourceFile::new("Foo.java".into(), Language::Java, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn counts_methods_and_constructor() {
    let a = analyze(
        "class C {
             C() {}
             int a() { return 1; }
             void b() {}
         }",
    );
    let nom = mehen_report::metrics_json::nom(&a.root.metrics);
    insta::assert_json_snapshot!(nom, @r#"
    {
      "functions": 3.0,
      "closures": 0.0,
      "functions_average": 0.6,
      "closures_average": 0.0,
      "total": 3.0,
      "average": 0.6,
      "functions_min": 0.0,
      "functions_max": 1.0,
      "closures_min": 0.0,
      "closures_max": 0.0
    }
    "#);
}

#[test]
fn interface_methods_count_once() {
    // Regression: interface methods reach the walker through
    // `interfaceMethodDeclaration → interfaceCommonBodyDeclaration`; the space
    // is opened only at the common-body rule so each method counts once.
    let a = analyze(
        "interface I {
             void m();
             default int d() { return 2; }
         }",
    );
    let nom = mehen_report::metrics_json::nom(&a.root.metrics);
    insta::assert_json_snapshot!(nom, @r#"
    {
      "functions": 2.0,
      "closures": 0.0,
      "functions_average": 0.5,
      "closures_average": 0.0,
      "total": 2.0,
      "average": 0.5,
      "functions_min": 0.0,
      "functions_max": 1.0,
      "closures_min": 0.0,
      "closures_max": 0.0
    }
    "#);
}

#[test]
fn lambda_is_a_closure() {
    let a = analyze(
        "class C {
             Runnable r = () -> System.out.println(\"hi\");
         }",
    );
    let nom = mehen_report::metrics_json::nom(&a.root.metrics);
    insta::assert_json_snapshot!(nom, @r#"
    {
      "functions": 0.0,
      "closures": 1.0,
      "functions_average": 0.0,
      "closures_average": 0.3333333333333333,
      "total": 1.0,
      "average": 0.3333333333333333,
      "functions_min": 0.0,
      "functions_max": 0.0,
      "closures_min": 0.0,
      "closures_max": 1.0
    }
    "#);
    // The lambda opens a closure-shaped function space under the class.
    assert_eq!(a.root.spaces.len(), 1);
    assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
}
