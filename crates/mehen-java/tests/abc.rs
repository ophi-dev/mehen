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
fn generic_explicit_this_constructor_call_is_a_branch() {
    // Regression (PR #160 review): a generic explicit constructor invocation
    // (`<String>this(arg)`) routes through `primary: nonWildcardTypeArguments
    // THIS arguments`, not `methodCall`, so it must be counted as an ABC
    // branch too. The plain `this(…)`/`super(…)` forms go through `methodCall`.
    let a = analyze("class C { <T> C(T t) {} C() { <String>this(null); } }");
    let abc = serde_json::to_value(mehen_report::metrics_json::abc(&a.root.metrics)).unwrap();
    assert_eq!(
        abc["branches"],
        serde_json::json!(1.0),
        "a generic explicit this-constructor call is a branch"
    );
    // Guard: a bare `this` / `this.field` access (no `arguments`) is not a call.
    let field = analyze("class C { int x; int m() { return this.x; } }");
    let fabc = serde_json::to_value(mehen_report::metrics_json::abc(&field.root.metrics)).unwrap();
    assert_eq!(
        fabc["branches"],
        serde_json::json!(0.0),
        "a bare `this` field access is not a branch"
    );
}

#[test]
fn qualified_super_field_access_is_not_a_branch() {
    // Regression (PR #160 review): `superSuffix` also represents qualified
    // super *field* access (`Outer.super.field`), where the grammar's
    // `arguments` child is optional. A bare field read is NOT a call, so it
    // must not count as an ABC branch — only a `superSuffix` with an
    // `arguments` child (a real call) does.
    let a = analyze(
        "class Outer {
             int field;
             class Inner extends Outer {
                 int f() { return Outer.super.field; }
             }
         }",
    );
    let abc = serde_json::to_value(mehen_report::metrics_json::abc(&a.root.metrics)).unwrap();
    assert_eq!(
        abc["branches"],
        serde_json::json!(0.0),
        "a super field read is not a branch"
    );
}

#[test]
fn annotation_named_element_is_not_an_assignment() {
    // Regression (PR #160 review): the vendored grammar's `IsNotIdentifierAssign`
    // predicate is dropped by the Rust generator, so `@Ann(value = 1)`'s named
    // element value parses through the assignment-expression path with an `=`.
    // Annotation metadata is not executable code, so it must NOT count as an
    // ABC assignment.
    let a = analyze("class C { @Ann(value = 1) void m() {} }");
    let abc = serde_json::to_value(mehen_report::metrics_json::abc(&a.root.metrics)).unwrap();
    assert_eq!(
        abc["assignments"],
        serde_json::json!(0.0),
        "an annotation named-element value is not an assignment"
    );
    // Guard: a real assignment in the body of an annotated method still counts
    // (the `in_annotation` flag must not leak past the annotation subtree).
    let with_body = analyze("class C { @Ann(value = 1) void m() { int x = 5; } }");
    let wb =
        serde_json::to_value(mehen_report::metrics_json::abc(&with_body.root.metrics)).unwrap();
    assert_eq!(
        wb["assignments"],
        serde_json::json!(1.0),
        "a real assignment in an annotated method's body still counts"
    );
}

#[test]
fn switch_guard_is_a_condition() {
    // Regression (PR #160 review): a Java pattern-switch guard
    // (`case String s when ready -> …`, grammar `guard: 'when' expression`) is
    // a distinct boolean test — like an extra `if` on the case — so it must
    // record an ABC condition. Using a bare boolean operand (`ready`) isolates
    // the guard: without the fix no expression operator fires, so the guard's
    // test would be uncounted. Here: C = the `case` (1) + the guard (1) = 2.
    let a = analyze(
        "class C {
             boolean ready;
             int f(Object o) {
                 return switch (o) {
                     case String s when ready -> 1;
                     default -> 0;
                 };
             }
         }",
    );
    let abc = serde_json::to_value(mehen_report::metrics_json::abc(&a.root.metrics)).unwrap();
    assert_eq!(
        abc["conditions"],
        serde_json::json!(2.0),
        "a guarded pattern case counts the case AND the guard as conditions"
    );
    // Guard: operators inside the guard still count on top of the guard test.
    // `case String s when a > b` → case (1) + guard (1) + `>` (1) = 3.
    let with_op = analyze(
        "class C {
             int f(Object o, int a, int b) {
                 return switch (o) {
                     case String s when a > b -> 1;
                     default -> 0;
                 };
             }
         }",
    );
    let wabc =
        serde_json::to_value(mehen_report::metrics_json::abc(&with_op.root.metrics)).unwrap();
    assert_eq!(
        wabc["conditions"],
        serde_json::json!(3.0),
        "an operator inside the guard adds a condition on top of the guard test"
    );
    // Guard: an unguarded pattern case (no `when`) counts only the case.
    let unguarded = analyze(
        "class C {
             int f(Object o) {
                 return switch (o) {
                     case String s -> 1;
                     default -> 0;
                 };
             }
         }",
    );
    let uabc =
        serde_json::to_value(mehen_report::metrics_json::abc(&unguarded.root.metrics)).unwrap();
    assert_eq!(
        uabc["conditions"],
        serde_json::json!(1.0),
        "an unguarded pattern case counts only the case, not a phantom guard"
    );
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
