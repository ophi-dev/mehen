// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ABC (Assignments / Branches / Conditions) tests for the ANTLR Java walker.
//!
//! A = assignments (`=`, compound assigns, declarators with an initializer);
//! B = branches (method/constructor calls, `new`); C = conditions (comparison
//! & equality operators, `&&`/`||`, ternary, `instanceof`, and each
//! `if`/`case`/`catch`/loop test). `magnitude = sqrt(A² + B² + C²)`.

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
fn assignments_branches_conditions() {
    // A: `int r = a + b;` initializer (1) + `r += bump();` compound (1) = 2
    // B: `bump()` call (1)
    // C: `if (a > b)` test (1) + `>` operator (1) = 2
    let a = analyze(
        "class C {
             int f(int a, int b) {
                 int r = a + b;
                 if (a > b) { r += bump(); }
                 return r;
             }
         }",
    );
    let abc = mehen_report::metrics_json::abc(&a.root.metrics);
    insta::assert_json_snapshot!(abc, @r#"
    {
      "assignments": 2.0,
      "branches": 1.0,
      "conditions": 2.0,
      "magnitude": 3.0,
      "assignments_average": 0.6666666666666666,
      "branches_average": 0.3333333333333333,
      "conditions_average": 0.6666666666666666,
      "assignments_min": 0.0,
      "assignments_max": 2.0,
      "branches_min": 0.0,
      "branches_max": 1.0,
      "conditions_min": 0.0,
      "conditions_max": 2.0
    }
    "#);
}

#[test]
fn increment_decrement_count_as_assignments() {
    // Regression (audit): `++`/`--` are assignments (A) per Fitzpatrick's ABC.
    // `i++;` and `--j;` → A=2. No conditions/branches.
    let a = analyze(
        "class C {
             void f() {
                 int i = 0, j = 0;
                 i++;
                 --j;
             }
         }",
    );
    let abc = mehen_report::metrics_json::abc(&a.root.metrics);
    // A: two declarators with initializers (2) + i++ (1) + --j (1) = 4.
    insta::assert_json_snapshot!(abc, @r#"
    {
      "assignments": 4.0,
      "branches": 0.0,
      "conditions": 0.0,
      "magnitude": 4.0,
      "assignments_average": 1.3333333333333333,
      "branches_average": 0.0,
      "conditions_average": 0.0,
      "assignments_min": 0.0,
      "assignments_max": 4.0,
      "branches_min": 0.0,
      "branches_max": 0.0,
      "conditions_min": 0.0,
      "conditions_max": 0.0
    }
    "#);
}

#[test]
fn bit_shifts_are_not_conditions() {
    // Regression (audit): `<<`/`>>`/`>>>` must NOT count as ABC conditions
    // (they decompose into bare LT/GT tokens). A relational `<` still does.
    let shift = analyze(
        "class C {
             int f(int a, int b) { return (a << b) + (a >> b) + (a >>> b); }
         }",
    );
    let abc_shift = mehen_report::metrics_json::abc(&shift.root.metrics);
    insta::assert_json_snapshot!(abc_shift, @r#"
    {
      "assignments": 0.0,
      "branches": 0.0,
      "conditions": 0.0,
      "magnitude": 0.0,
      "assignments_average": 0.0,
      "branches_average": 0.0,
      "conditions_average": 0.0,
      "assignments_min": 0.0,
      "assignments_max": 0.0,
      "branches_min": 0.0,
      "branches_max": 0.0,
      "conditions_min": 0.0,
      "conditions_max": 0.0
    }
    "#);
}

#[test]
fn var_and_resource_initializers_count_as_assignments() {
    // Regression (audit): `var x = e` and try-with-resources `T r = e` are
    // initialized declarations → assignments, like `int x = e`.
    let a = analyze(
        "class C {
             void f() {
                 var x = compute();
                 try (AutoCloseable r = open()) { use(r); } catch (Exception e) {}
             }
         }",
    );
    let abc = mehen_report::metrics_json::abc(&a.root.metrics);
    insta::assert_json_snapshot!(abc, @r#"
    {
      "assignments": 2.0,
      "branches": 3.0,
      "conditions": 1.0,
      "magnitude": 3.7416573867739413,
      "assignments_average": 0.6666666666666666,
      "branches_average": 1.0,
      "conditions_average": 0.3333333333333333,
      "assignments_min": 0.0,
      "assignments_max": 2.0,
      "branches_min": 0.0,
      "branches_max": 3.0,
      "conditions_min": 0.0,
      "conditions_max": 1.0
    }
    "#);
}

#[test]
fn explicit_generic_invocation_is_a_branch() {
    // Regression (PR #160 review): `this.<String>m()` routes through
    // `explicitGenericInvocation`, not `methodCall`, so it must be counted as
    // an ABC branch too. Here: B=1 (the generic call).
    let a = analyze(
        "class C {
             <T> T m() { return null; }
             void f() { this.<String>m(); }
         }",
    );
    let abc = mehen_report::metrics_json::abc(&a.root.metrics);
    insta::assert_json_snapshot!(abc, @r#"
    {
      "assignments": 0.0,
      "branches": 1.0,
      "conditions": 0.0,
      "magnitude": 1.0,
      "assignments_average": 0.0,
      "branches_average": 0.25,
      "conditions_average": 0.0,
      "assignments_min": 0.0,
      "assignments_max": 0.0,
      "branches_min": 0.0,
      "branches_max": 1.0,
      "conditions_min": 0.0,
      "conditions_max": 0.0
    }
    "#);
}

#[test]
fn suffix_routed_calls_are_branches() {
    // Regression (PR #160 review): calls that don't route through `methodCall`
    // must still count as ABC branches, exactly once each:
    //   - `I.super.d()`   → superSuffix
    //   - `<String>m()`   → explicitGenericInvocationSuffix (unqualified)
    //   - `this.<String>m()` → explicitGenericInvocation(Suffix) (qualified)
    for (src, label) in [
        (
            "interface I { default void d() {} } class C implements I { void f() { I.super.d(); } }",
            "I.super.d()",
        ),
        (
            "class C { <T> T m() { return null; } void f() { <String>m(); } }",
            "<String>m()",
        ),
        (
            "class C { <T> T m() { return null; } void f() { this.<String>m(); } }",
            "this.<String>m()",
        ),
    ] {
        let a = analyze(src);
        let abc = serde_json::to_value(mehen_report::metrics_json::abc(&a.root.metrics)).unwrap();
        assert_eq!(
            abc["branches"],
            serde_json::json!(1.0),
            "exactly one branch for: {label}"
        );
    }
}

#[test]
fn object_creation_is_a_branch() {
    // B: `new Object()` (1) + no other calls. A: `Object o = …` initializer (1).
    let a = analyze(
        "class C {
             void f() {
                 Object o = new Object();
             }
         }",
    );
    let abc = mehen_report::metrics_json::abc(&a.root.metrics);
    insta::assert_json_snapshot!(abc, @r#"
    {
      "assignments": 1.0,
      "branches": 1.0,
      "conditions": 0.0,
      "magnitude": 1.4142135623730951,
      "assignments_average": 0.3333333333333333,
      "branches_average": 0.3333333333333333,
      "conditions_average": 0.0,
      "assignments_min": 0.0,
      "assignments_max": 1.0,
      "branches_min": 0.0,
      "branches_max": 1.0,
      "conditions_min": 0.0,
      "conditions_max": 0.0
    }
    "#);
}
