// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NOM / NArgs / NExit / NPA / NPM / WMC tests for the ANTLR C# walker.
//!
//! C# visibility rules these pin: a class/struct member with no access
//! modifier is `private` (so it does NOT count toward the public API),
//! `internal` is assembly-scoped and likewise not public, interface members are
//! implicitly public, and `enum` members are implicitly public constants.

mod common;

use common::analyze_clean;
use mehen_report::metrics_json;

#[test]
fn nom_counts_functions_and_closures_separately() {
    let a = analyze_clean(
        "class C {
             void M() { }
             void N() {
                 System.Func<int, int> f = x => x;
             }
         }",
    );
    let nom = metrics_json::nom(&a.root.metrics);
    assert_eq!(nom.functions, 2.0, "M and N");
    assert_eq!(nom.closures, 1.0, "the lambda");
}

#[test]
fn nargs_counts_parameters_per_shape() {
    let a = analyze_clean(
        "class C {
             void Zero() { }
             void Two(int a, int b) { }
             void Params(params int[] rest) { }
         }",
    );
    let nargs = metrics_json::nargs(&a.root.metrics);
    // 0 + 2 + 1 (a `params` array is one parameter) = 3
    assert_eq!(nargs.total_functions, 3.0);
}

#[test]
fn nargs_counts_lambda_parameters_as_closure_args() {
    let a = analyze_clean(
        "class C {
             void F() {
                 System.Func<int, int, int> f = (a, b) => a + b;
             }
         }",
    );
    let nargs = metrics_json::nargs(&a.root.metrics);
    assert_eq!(nargs.total_closures, 2.0);
}

#[test]
fn nargs_counts_operator_parameters() {
    let a = analyze_clean(
        "class C {
             public static C operator +(C a, C b) { return a; }
         }",
    );
    let nargs = metrics_json::nargs(&a.root.metrics);
    assert_eq!(nargs.total_functions, 2.0);
}

#[test]
fn nexit_counts_returns_throws_and_yields() {
    let a = analyze_clean(
        "class C {
             int F(int v) {
                 if (v < 0) { throw new System.ArgumentException(); }
                 return v;
             }
             System.Collections.Generic.IEnumerable<int> G() {
                 yield return 1;
                 yield break;
             }
         }",
    );
    let nexit = metrics_json::nexits(&a.root.metrics);
    // throw + return + yield return + yield break = 4
    assert_eq!(nexit.sum, 4.0);
}

#[test]
fn throw_expression_counts_as_an_exit() {
    // A `throw` *expression* (C# 7) never reaches the statement form.
    let a = analyze_clean(
        "class C {
             string F(string s) { return s ?? throw new System.ArgumentNullException(); }
         }",
    );
    let nexit = metrics_json::nexits(&a.root.metrics);
    // the `return` + the `throw` expression = 2
    assert_eq!(nexit.sum, 2.0);
}

// Key note for the NPA assertions below: in the published family,
// `npa.classes` is the *public* class-attribute count while
// `npa.class_attributes` is the count of ALL class attributes (the CDA
// denominator). So a visibility assertion checks `classes`, and a
// "did we see every declarator" assertion checks `class_attributes`.

#[test]
fn npa_counts_only_public_fields() {
    // `private`, implicit (private), and `internal` fields are all NOT public.
    let a = analyze_clean(
        "class C {
             public int Pub;
             private int Priv;
             int Implicit;
             internal int Internal;
         }",
    );
    let npa = metrics_json::npa(&a.root.metrics);
    assert_eq!(npa.classes, 1.0, "only `public int Pub` is public");
    assert_eq!(
        npa.class_attributes, 4.0,
        "but all four are attributes (the CDA denominator)"
    );
}

#[test]
fn npa_counts_each_declarator_of_a_multi_field() {
    let a = analyze_clean("class C { public int a, b, c; }");
    let npa = metrics_json::npa(&a.root.metrics);
    assert_eq!(npa.classes, 3.0);
    assert_eq!(npa.class_attributes, 3.0);
}

#[test]
fn npa_counts_constants_and_enum_members() {
    let a = analyze_clean(
        "class C { public const int Max = 1; }
         enum E { A, B, C }",
    );
    let npa = metrics_json::npa(&a.root.metrics);
    // the public const + 3 implicitly-public enum members = 4
    assert_eq!(npa.classes, 4.0);
}

#[test]
fn npm_counts_only_public_methods_of_a_class() {
    let a = analyze_clean(
        "class C {
             public void Pub() { }
             private void Priv() { }
             void Implicit() { }
         }",
    );
    let npm = metrics_json::npm(&a.root.metrics);
    // `npm.classes` is the PUBLIC class-method count; `npm.class_methods` is
    // all of them (the CDA denominator) — same convention as NPA above.
    assert_eq!(npm.classes, 1.0, "only `public void Pub` is public");
    assert_eq!(npm.class_methods, 3.0, "but all three are methods");
}

#[test]
fn npm_treats_interface_members_as_implicitly_public() {
    let a = analyze_clean(
        "interface I {
             double Area { get; }
             void Scale(double f);
         }",
    );
    let npm = metrics_json::npm(&a.root.metrics);
    assert_eq!(npm.interface_methods, 2.0);
    assert_eq!(npm.class_methods, 0.0);
}

#[test]
fn npm_counts_properties_as_api_members() {
    let a = analyze_clean(
        "class C {
             public int Count { get; set; }
         }",
    );
    let npm = metrics_json::npm(&a.root.metrics);
    assert_eq!(npm.class_methods, 1.0, "the property is one API member");
}

#[test]
fn wmc_sums_method_complexity_per_class() {
    // F: 1 + if = 2; G: 1. WMC = 3.
    let a = analyze_clean(
        "class C {
             void F(int v) { if (v > 0) { } }
             void G() { }
         }",
    );
    let wmc = metrics_json::wmc(&a.root.metrics);
    assert_eq!(wmc.classes, 3.0);
}

#[test]
fn wmc_excludes_interface_members() {
    // An interface's members are not weighted (matches `mehen-java`).
    let a = analyze_clean("interface I { void M(); }");
    let wmc = metrics_json::wmc(&a.root.metrics);
    assert_eq!(wmc.interfaces, 0.0);
    assert_eq!(wmc.classes, 0.0);
}

#[test]
fn wmc_excludes_lambdas_and_local_functions() {
    // Their complexity belongs to the enclosing method, which already counts
    // it — so neither may inflate the class's WMC beyond the method's own.
    let a = analyze_clean(
        "class C {
             void F() {
                 System.Func<int, int> f = x => x > 0 ? 1 : 0;
                 int Local(int y) { if (y > 0) { return 1; } return 0; }
             }
         }",
    );
    let wmc = metrics_json::wmc(&a.root.metrics);
    // Only `F` is weighted: its own McCabe is 1 (the ternary and the `if` live
    // in the lambda / local function, which carry their own spaces).
    assert_eq!(wmc.classes, 1.0);
}

#[test]
fn struct_members_route_to_the_class_buckets() {
    let a = analyze_clean(
        "struct S {
             public int X;
             public void M() { }
         }",
    );
    let npa = metrics_json::npa(&a.root.metrics);
    let npm = metrics_json::npm(&a.root.metrics);
    assert_eq!(npa.class_attributes, 1.0);
    assert_eq!(npm.class_methods, 1.0);
    assert_eq!(npa.interface_attributes, 0.0);
}

#[test]
fn nargs_counts_a_simple_lambda_parameter() {
    // REGRESSION. `x => …` puts its single parameter in a bare `identifier_token`, not
    // a `parameter` child, so the count came back 0 — while the equivalent `(x) => …`
    // (a `parenthesized_lambda_expression`, which does have a `parameter_list`)
    // returned 1. Same arity, two different numbers depending on whether the author
    // wrote the parentheses.
    let bare = analyze_clean(
        "class C {
             void F() { System.Func<int, int> f = x => x + 1; }
         }",
    );
    let parenthesized = analyze_clean(
        "class C {
             void F() { System.Func<int, int> f = (x) => x + 1; }
         }",
    );
    assert_eq!(
        metrics_json::nargs(&bare.root.metrics).total_closures,
        1.0,
        "`x => …` takes one argument"
    );
    assert_eq!(
        metrics_json::nargs(&bare.root.metrics).total_closures,
        metrics_json::nargs(&parenthesized.root.metrics).total_closures,
        "the parentheses must not change the arity"
    );
}

#[test]
fn nargs_of_an_indexer_accessor_does_not_depend_on_body_syntax() {
    // REGRESSION. An accessor's space opens at `accessor_declaration`, which carries no
    // parameter list — the indexer's `bracketed_parameter_list` is on the *owner*. So
    // the block-bodied form reported 0 while the expression-bodied form, whose space
    // opens at `indexer_declaration`, reported 1. The count is now threaded down.
    let block = analyze_clean("class C { int this[int i] { get { return i; } } }");
    let expression = analyze_clean("class C { int this[int i] => i; }");
    assert_eq!(
        metrics_json::nargs(&block.root.metrics).total_functions,
        1.0,
        "an indexer's getter takes the indexer's one argument"
    );
    assert_eq!(
        metrics_json::nargs(&block.root.metrics).total_functions,
        metrics_json::nargs(&expression.root.metrics).total_functions,
    );
}

#[test]
fn nargs_of_a_property_accessor_is_zero() {
    // A property's accessors take nothing — `set`'s `value` is implicit, not declared.
    // Pinned alongside the indexer case: the same threading must not leak a count into
    // a property.
    let a = analyze_clean("class C { int P { get => 1; set { } } }");
    assert_eq!(metrics_json::nargs(&a.root.metrics).total_functions, 0.0);
}

#[test]
fn nom_and_nargs_count_a_primary_constructor() {
    // REGRESSION. A primary constructor's parameters live on the type declaration and
    // no `constructor_declaration` node exists, so `class C(int x)` was absent from NOM
    // and NArgs entirely. Pinned against the explicit form.
    let primary = analyze_clean("class C(int x) { }");
    let explicit = analyze_clean("class C { public C(int x) { } }");
    for a in [&primary, &explicit] {
        assert_eq!(metrics_json::nom(&a.root.metrics).functions, 1.0);
        assert_eq!(metrics_json::nargs(&a.root.metrics).total_functions, 1.0);
    }
}
