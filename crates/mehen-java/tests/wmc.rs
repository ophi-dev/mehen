// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! WMC (weighted methods per class) tests for the ANTLR Java walker.
//!
//! WMC sums the cyclomatic complexity of a class's methods. Interfaces are
//! excluded from WMC.

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
fn class_sums_method_cyclomatics() {
    // `simple` McCabe 1; `branchy` McCabe 1 + if(1) + &&(1) = 3. WMC = 4.
    let a = analyze(
        "class C {
             int simple() { return 1; }
             int branchy(int a, int b) {
                 if (a > 0 && b > 0) { return a; }
                 return b;
             }
         }",
    );
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    insta::assert_json_snapshot!(wmc, @r#"
    {
      "classes": 4.0,
      "interfaces": 0.0,
      "total": 4.0
    }
    "#);
}

#[test]
fn enum_constant_body_method_does_not_inflate_enum_wmc() {
    // Regression (PR #160 review): a method inside a constant-specific enum
    // body (`A { void m() {…} }`) belongs to `A`'s anonymous subclass, not the
    // enum, so it must NOT roll into the enum's WMC. The enum here declares no
    // methods of its own, so WMC stays 0.
    let a = analyze(
        "enum E {
             A {
                 public void m() { if (true) {} }
             };
         }",
    );
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    insta::assert_json_snapshot!(wmc, @r#"
    {
      "classes": 0.0,
      "interfaces": 0.0,
      "total": 0.0
    }
    "#);
}

#[test]
fn interface_methods_are_excluded_from_wmc() {
    // Regression (PR #160 review): Java WMC is per class — an interface's
    // methods (including `default`) must not accumulate WMC, even in a file
    // that also contains a class. Here the class has no methods, so total WMC
    // is 0 despite the interface's `default` method containing an `if`.
    let a = analyze(
        "class C {}
         interface I {
             default int m() { if (flag) {} return 1; }
         }",
    );
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    insta::assert_json_snapshot!(wmc, @r#"
    {
      "classes": 0.0,
      "interfaces": 0.0,
      "total": 0.0
    }
    "#);
}

#[test]
fn anonymous_class_body_method_does_not_inflate_enclosing_wmc() {
    // Regression (PR #160 review): a method in an anonymous class body
    // (`new Runnable() { void run() {…} }`, reached via
    // `classCreatorRest → classBody`) belongs to the anonymous subclass, not
    // the enclosing class C, so it must NOT roll into C's WMC. C declares no
    // methods of its own.
    let a = analyze("class C { Runnable r = new Runnable() { public void run() { if (x) {} } }; }");
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    insta::assert_json_snapshot!(wmc, @r#"
    {
      "classes": 0.0,
      "interfaces": 0.0,
      "total": 0.0
    }
    "#);
}

#[test]
fn lambda_in_field_initializer_does_not_inflate_class_wmc() {
    // Regression (PR #160 review): a lambda is a Closure, not a method — its
    // cyclomatic must NOT roll into the class's WMC (WMC weights methods). The
    // class declares no methods, so WMC stays 0 even though the lambda body
    // contains an `if`.
    let a = analyze("class C { Runnable r = () -> { if (flag) {} }; }");
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    insta::assert_json_snapshot!(wmc, @r#"
    {
      "classes": 0.0,
      "interfaces": 0.0,
      "total": 0.0
    }
    "#);
}
