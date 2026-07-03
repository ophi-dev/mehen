// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NPM (number of public methods) tests for the ANTLR Java walker.
//!
//! Java visibility: a class method with no access modifier is package-private
//! (NOT public); only an explicit `public` method counts toward NPM.
//! `protected`/`private` are non-public. Interface methods are implicitly
//! public.

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
fn only_explicitly_public_class_methods_count() {
    // `pub` public (1), `prot` protected, `priv` private, `pkg` package-private
    // → public NPM = 1, total methods = 4.
    let a = analyze(
        "class C {
             public void pub() {}
             protected void prot() {}
             private void priv() {}
             void pkg() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 1.0,
      "interfaces": 0.0,
      "class_methods": 4.0,
      "interface_methods": 0.0,
      "classes_average": 0.25,
      "interfaces_average": null,
      "total": 1.0,
      "total_methods": 4.0,
      "average": 0.25
    }
    "#);
}

#[test]
fn generic_methods_and_constructors_count() {
    // Regression (audit): generic methods/constructors reach the walker
    // through genericMethodDeclaration/genericConstructorDeclaration wrappers.
    // Both public members must count toward NPM.
    let a = analyze(
        "class C {
             public <T> T identity(T x) { return x; }
             public <T> C(T seed) {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 2.0,
      "interfaces": 0.0,
      "class_methods": 2.0,
      "interface_methods": 0.0,
      "classes_average": 1.0,
      "interfaces_average": null,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn generic_interface_methods_count() {
    // Regression (audit): a generic interface method reaches the walker via
    // genericInterfaceMethodDeclaration → interfaceCommonBodyDeclaration and
    // must count exactly once toward interface NPM.
    let a = analyze(
        "interface I {
             int plain();
             <T> T generic(T x);
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 2.0,
      "class_methods": 0.0,
      "interface_methods": 2.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn interface_methods_are_public() {
    let a = analyze(
        "interface I {
             void m();
             default int d() { return 2; }
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 2.0,
      "class_methods": 0.0,
      "interface_methods": 2.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}
