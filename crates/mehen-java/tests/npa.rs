// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NPA (number of public attributes) tests for the ANTLR Java walker.
//!
//! Java visibility: a class field with no access modifier is package-private
//! (NOT public); only an explicit `public` field counts toward NPA. Interface
//! fields are implicitly public. Record components count as public attributes.

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
fn only_public_class_fields_count() {
    // `x` private, `y` public, `z` package-private → public NPA = 1, total = 3.
    let a = analyze(
        "class C {
             private int x;
             public int y;
             int z;
         }",
    );
    let npa = mehen_report::metrics_json::npa(&a.root.metrics);
    insta::assert_json_snapshot!(npa, @r#"
    {
      "classes": 1.0,
      "interfaces": 0.0,
      "class_attributes": 3.0,
      "interface_attributes": 0.0,
      "classes_average": 0.3333333333333333,
      "interfaces_average": null,
      "total": 1.0,
      "total_attributes": 3.0,
      "average": 0.3333333333333333
    }
    "#);
}

#[test]
fn multiple_declarators_each_count() {
    // `public int a, b, c;` declares three public attributes.
    let a = analyze(
        "class C {
             public int a, b, c;
         }",
    );
    let npa = mehen_report::metrics_json::npa(&a.root.metrics);
    insta::assert_json_snapshot!(npa, @r#"
    {
      "classes": 3.0,
      "interfaces": 0.0,
      "class_attributes": 3.0,
      "interface_attributes": 0.0,
      "classes_average": 1.0,
      "interfaces_average": null,
      "total": 3.0,
      "total_attributes": 3.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn interface_fields_are_public() {
    let a = analyze(
        "interface I {
             int A = 1;
             int B = 2;
         }",
    );
    let npa = mehen_report::metrics_json::npa(&a.root.metrics);
    insta::assert_json_snapshot!(npa, @r#"
    {
      "classes": 0.0,
      "interfaces": 2.0,
      "class_attributes": 0.0,
      "interface_attributes": 2.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_attributes": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn record_components_are_public_attributes() {
    let a = analyze("record Point(int x, int y) {}");
    let npa = mehen_report::metrics_json::npa(&a.root.metrics);
    insta::assert_json_snapshot!(npa, @r#"
    {
      "classes": 2.0,
      "interfaces": 0.0,
      "class_attributes": 2.0,
      "interface_attributes": 0.0,
      "classes_average": 1.0,
      "interfaces_average": null,
      "total": 2.0,
      "total_attributes": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn annotation_constants_are_public_attributes() {
    // Regression (PR #160 review): annotation constants (`int X = 1;` in an
    // `@interface`) reach the walker via annotationConstantRest and are
    // implicitly-public interface attributes. `int Y = 2, Z = 3;` declares two.
    let a = analyze("@interface Ann { int X = 1; int Y = 2, Z = 3; }");
    let npa = mehen_report::metrics_json::npa(&a.root.metrics);
    insta::assert_json_snapshot!(npa, @r#"
    {
      "classes": 0.0,
      "interfaces": 3.0,
      "class_attributes": 0.0,
      "interface_attributes": 3.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 3.0,
      "total_attributes": 3.0,
      "average": 1.0
    }
    "#);
}
