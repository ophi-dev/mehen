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
use mehen_core::MetricSpace;
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

#[test]
fn a_primary_constructor_owns_its_whole_header() {
    // REGRESSION (#219). The synthetic primary-constructor space got NOM and NArgs —
    // computed at the open — but none of the Halstead, LLOC, or ABC contributions of
    // the syntax it owns, because the space was closed before the walk reached its
    // subtree: those landed on the enclosing type instead. `class C(int x) : B(C.F(x))`
    // is the primary spelling of `class C : B { public C(int x) : base(C.F(x)) { } }`,
    // so the two forms must *split* their per-space numbers identically — in
    // particular the base-constructor call's two ABC branches (the call itself plus
    // the nested `C.F(x)` invocation) belong to the constructor, not the class.
    //
    // Compared over the whole space tree rather than just the constructor, so a
    // partial fix cannot pass: widening the synthetic span so the post-walk byte
    // routing moves LOC/Halstead would still leave the walk-time ABC branches on the
    // type, and the class row here would show them.
    let primary = analyze_clean(
        "class C(int x) : B(C.F(x))
         {
             static int F(int x) { return x; }
         }",
    );
    let explicit = analyze_clean(
        "class C : B
         {
             public C(int x) : base(C.F(x)) { }
             static int F(int x) { return x; }
         }",
    );

    /// Flatten the tree into `(depth, kind, name, ABC branches, LLOC)` rows.
    fn rows(root: &MetricSpace) -> Vec<(usize, String, Option<String>, f64, f64)> {
        fn walk(
            s: &MetricSpace,
            depth: usize,
            out: &mut Vec<(usize, String, Option<String>, f64, f64)>,
        ) {
            out.push((
                depth,
                s.kind.as_str().to_string(),
                s.name.clone(),
                metrics_json::abc(&s.metrics).branches,
                metrics_json::loc(&s.metrics).lloc,
            ));
            for c in &s.spaces {
                walk(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(root, 0, &mut out);
        out
    }
    assert_eq!(rows(&primary.root), rows(&explicit.root));

    // The constructor's Halstead must cover its whole header. The parameter list
    // alone is `( int x )` — 3 operator occurrences, 1 operand — so anything
    // beyond that is the base list: `: B(C.F(x))` adds `:`, `.`, and two `(`/`)`
    // pairs (operators) plus `B`, `C`, `F`, `x` (operands).
    let ctor = &primary.root.spaces[0].spaces[0];
    assert_eq!(ctor.name.as_deref(), Some("C"));
    let h = metrics_json::halstead(&ctor.metrics);
    assert_eq!(h.big_n1, 9.0, "operators: ( int ) : . and 2 more ()-pairs");
    assert_eq!(h.big_n2, 5.0, "operands: x B C F x");
}

#[test]
fn a_primary_constructor_does_not_own_implemented_interfaces() {
    // The guard on the whole-header fix above: only the base-constructor *call* is
    // constructor syntax. An implemented interface in the same base list —
    // `class C(int x) : B(x), IFoo` — belongs to the type, exactly as the explicit
    // `class C : B, IFoo { public C(int x) : base(x) { } }` spelling attributes it.
    // Closing the synthetic space at the end of the full `base_list` swept `IFoo`
    // (and the `,`) into the constructor's Halstead.
    let a = analyze_clean("class C(int x) : B(x), IFoo { }");
    let ctor = &a.root.spaces[0].spaces[0];
    assert_eq!(ctor.name.as_deref(), Some("C"));
    // `( int x ) : B ( x )` and nothing after the call: were `, IFoo` included, N1
    // would gain the `,` and N2 the `IFoo`.
    let h = metrics_json::halstead(&ctor.metrics);
    assert_eq!(h.big_n1, 6.0, "operators: ( int ) : ( ) — no `,`");
    assert_eq!(h.big_n2, 3.0, "operands: x B x — no IFoo");
    // The base call is still the constructor's ABC branch.
    assert_eq!(metrics_json::abc(&ctor.metrics).branches, 1.0);

    // With no base call at all, the base list is purely the type's: the
    // constructor is just its parameter list, as for `struct S : IFoo` with an
    // explicit constructor.
    let a = analyze_clean("struct S(int x) : IFoo { }");
    let ctor = &a.root.spaces[0].spaces[0];
    assert_eq!(ctor.name.as_deref(), Some("S"));
    let h = metrics_json::halstead(&ctor.metrics);
    assert_eq!(h.big_n1, 3.0, "operators: ( int )");
    assert_eq!(h.big_n2, 1.0, "operands: x — no IFoo, no `:`");
    assert_eq!(metrics_json::abc(&ctor.metrics).branches, 0.0);
}

#[test]
fn an_expression_bodied_return_is_an_exit() {
    // REGRESSION. `int F() => 1;` has no `return_statement` node, so NExit stayed 0
    // while the equivalent `int F() { return 1; }` reported 1 — and NExit's own
    // documentation includes value-returning expressions. Pinned against the block form.
    let arrow = analyze_clean("class C { static int F() => 1; }");
    let block = analyze_clean("class C { static int F() { return 1; } }");
    assert_eq!(metrics_json::nexits(&arrow.root.metrics).sum, 1.0);
    assert_eq!(
        metrics_json::nexits(&arrow.root.metrics).sum,
        metrics_json::nexits(&block.root.metrics).sum,
    );
}

#[test]
fn a_void_expression_body_is_not_an_exit() {
    // The guard: an expression body is a return only when the member returns a value.
    // A `void` member, a constructor, and a `set` accessor return nothing.
    let void_member = analyze_clean("class C { static void G() { } static void M() => G(); }");
    assert_eq!(metrics_json::nexits(&void_member.root.metrics).sum, 0.0);

    let constructor = analyze_clean("class C { static void G() { } public C() => G(); }");
    assert_eq!(metrics_json::nexits(&constructor.root.metrics).sum, 0.0);
}

#[test]
fn a_getter_expression_body_is_an_exit_but_a_setter_is_not() {
    // A `get` yields a value; `set` does not. Read per space, since the unit sums both.
    let a = analyze_clean("class C { int _x; public int P { get => _x; set => _x = value; } }");
    let accessors: Vec<_> = a.root.spaces[0]
        .spaces
        .iter()
        .map(|s| (s.name.clone(), metrics_json::nexits(&s.metrics).sum))
        .collect();
    assert_eq!(
        accessors,
        vec![
            (Some("P.get".to_string()), 1.0),
            (Some("P.set".to_string()), 0.0),
        ]
    );
}

#[test]
fn a_throw_only_expression_body_is_one_exit() {
    // REGRESSION introduced by the expression-body exit fix: `int F() => throw new E();`
    // recorded the clause's implicit return AND the descendant `throw`, reporting NExit 2
    // where the block-bodied form reports 1. The clause is the return only when it
    // actually returns a value.
    let arrow = analyze_clean(
        "class C { class E : System.Exception { } static int F() => throw new E(); }",
    );
    let block = analyze_clean(
        "class C { class E : System.Exception { } static int F() { throw new E(); } }",
    );
    assert_eq!(metrics_json::nexits(&arrow.root.metrics).sum, 1.0);
    assert_eq!(
        metrics_json::nexits(&arrow.root.metrics).sum,
        metrics_json::nexits(&block.root.metrics).sum,
    );
}

#[test]
fn a_throw_inside_a_larger_expression_body_is_a_second_exit() {
    // The guard is one level deep, not a subtree scan: in `=> x ?? throw new E();` the
    // clause's return is real (it returns `x` when non-null) and the `throw` is another
    // exit, so both count — matching the block-bodied `return x ?? throw new E();`.
    let arrow = analyze_clean(
        "class C {
             class E : System.Exception { }
             static string F(string x) => x ?? throw new E();
         }",
    );
    let block = analyze_clean(
        "class C {
             class E : System.Exception { }
             static string F(string x) { return x ?? throw new E(); }
         }",
    );
    assert_eq!(metrics_json::nexits(&arrow.root.metrics).sum, 2.0);
    assert_eq!(
        metrics_json::nexits(&arrow.root.metrics).sum,
        metrics_json::nexits(&block.root.metrics).sum,
    );
}

#[test]
fn a_primary_constructor_is_a_public_method_of_its_type() {
    // REGRESSION. A primary constructor records NOM, NArgs, and WMC, but had no
    // `member_declaration` to route through — so unlike the explicit spelling, which
    // reaches `classify_type_member`, it recorded no NPM. The type's public API therefore
    // depended on which constructor spelling the author chose.
    //
    // Always public: a primary constructor's accessibility cannot be narrowed (there are
    // no modifiers to put on it), and its parameters ARE the construction surface.
    let primary = analyze_clean("class C(int x) { }");
    let explicit = analyze_clean("class C { public C(int x) { } }");
    // `npm.classes` is the PUBLIC class-method count; `npm.class_methods` is all of them.
    let npm = |a: &mehen_core::LanguageAnalysis| {
        let m = metrics_json::npm(&a.root.metrics);
        (m.classes, m.class_methods)
    };
    assert_eq!(npm(&primary), npm(&explicit));
    assert_eq!(npm(&primary), (1.0, 1.0));

    // `struct` and `record` are class-like for NPM, as they are for the rest of the
    // family, so all three declaration kinds that admit a primary constructor agree.
    for source in ["struct S(int x) { }", "record R(int X);"] {
        assert_eq!(npm(&analyze_clean(source)), (1.0, 1.0), "{source}");
    }
}

#[test]
fn a_void_like_async_expression_body_is_not_an_exit() {
    // REGRESSION. `returns_value` tested the declared type against the text `"void"`, so
    // `async Task M() => await Work();` looked like it returned — but it produces no
    // result, and its block-bodied twin records no exit. NExit therefore depended on body
    // syntax for one of the most common shapes in modern C#.
    let nexit = |source: &str| metrics_json::nexits(&analyze_clean(source).root.metrics).sum;

    // The non-generic awaitables are void-like: arrow and block forms must agree.
    for ty in ["Task", "ValueTask", "System.Threading.Tasks.Task"] {
        let arrow = nexit(&format!(
            "using System.Threading.Tasks;
             class C {{ static async {ty} M() => await Task.Delay(1); }}"
        ));
        let block = nexit(&format!(
            "using System.Threading.Tasks;
             class C {{ static async {ty} M() {{ await Task.Delay(1); }} }}"
        ));
        assert_eq!(arrow, block, "`{ty}` is void-like");
        assert_eq!(arrow, 0.0, "`{ty}` yields no value");
    }

    // The generic forms DO return, so they must keep their exit — the fix must not mute
    // every task-returning method.
    for ty in ["Task<int>", "ValueTask<int>"] {
        let arrow = nexit(&format!(
            "using System.Threading.Tasks;
             class C {{ static async {ty} M() => await Task.FromResult(1); }}"
        ));
        assert_eq!(arrow, 1.0, "`{ty}` returns a value");
    }

    // And an ordinary value-returning arrow body still counts, so the void-like set did
    // not widen into everything.
    assert_eq!(nexit("class C { static int M() => 1; }"), 1.0);
    assert_eq!(
        nexit("class C { static void W() { } static void M() => W(); }"),
        0.0
    );
}

#[test]
fn a_non_async_task_method_still_returns() {
    // REGRESSION introduced by the void-like fix above: treating a bare `Task` as
    // void-like unconditionally suppressed the exit for a *non-async* task-returning
    // method, which must literally `return` a task object — so
    // `Task M() => Task.CompletedTask;` reported 0 while
    // `Task M() { return Task.CompletedTask; }` reported 1. The same
    // body-syntax-dependent NExit, moved rather than fixed.
    //
    // "Void-like" is therefore a property of the type AND the `async` modifier, not of
    // the type alone: `async` makes the compiler wrap the body's completion in the task.
    let nexit = |source: &str| metrics_json::nexits(&analyze_clean(source).root.metrics).sum;

    for ty in ["Task", "ValueTask"] {
        let arrow = nexit(&format!(
            "using System.Threading.Tasks;
             class C {{ static {ty} M() => default; }}"
        ));
        let block = nexit(&format!(
            "using System.Threading.Tasks;
             class C {{ static {ty} M() {{ return default; }} }}"
        ));
        assert_eq!(arrow, block, "non-async `{ty}` returns a value");
        assert_eq!(arrow, 1.0, "non-async `{ty}` is not void-like");
    }

    // `void` stays void-like unconditionally — it has no async/non-async distinction.
    assert_eq!(
        nexit("class C { static void W() { } static void M() => W(); }"),
        0.0
    );
}

#[test]
fn an_expression_bodied_lambda_records_its_exit() {
    // REGRESSION. A lambda has no `arrow_expression_clause` for the member arm to match —
    // Roslyn spells the body as a bare `(block | expression)` directly on the lambda — so
    // `x => x + 1` reported NExit 0 while `x => { return x + 1; }` reported 1.
    let closure_nexit = |source: &str| {
        let a = analyze_clean(source);
        fn walk(s: &mehen_core::MetricSpace, out: &mut Vec<f64>) {
            if s.kind == mehen_core::SpaceKind::Closure {
                out.push(metrics_json::nexits(&s.metrics).sum);
            }
            for c in &s.spaces {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(&a.root, &mut out);
        out
    };

    let arrow = closure_nexit(
        "using System;
         class C { static void F() { Func<int,int> f = x => x + 1; f(1); } }",
    );
    let block = closure_nexit(
        "using System;
         class C { static void F() { Func<int,int> f = x => { return x + 1; }; f(1); } }",
    );
    assert_eq!(arrow, block, "the two lambda body spellings must agree");
    assert_eq!(arrow, vec![1.0]);

    // The explicitly-typed C# 10 form is the same node with a `type?` child, so it agrees.
    assert_eq!(
        closure_nexit(
            "using System;
             class C { static void F() { Func<int,int> f = int (int x) => x + 1; f(1); } }"
        ),
        vec![1.0]
    );

    // A `throw` body is excluded, for the same reason the member arm excludes it:
    // `RULE_THROW_EXPRESSION` records that exit itself, so counting here would double it.
    assert_eq!(
        closure_nexit(
            "using System;
             class C { static void F() { Func<int,int> f = x => throw new Exception(); f(1); } }"
        ),
        vec![1.0],
        "a throwing lambda has exactly one exit, not two"
    );
}

#[test]
fn a_primary_constructor_owns_its_signature_tokens() {
    // REGRESSION. The synthetic space was pushed and popped *before* the type's children
    // were visited, so it received none of the tokens inside its own signature: Halstead
    // vocabulary 0. It now opens when the walk reaches the `parameter_list`, which is what
    // the constructor consists of — Roslyn synthesizes no `constructor_declaration` node.
    fn ctor_space(a: &mehen_core::LanguageAnalysis) -> mehen_core::MetricSpace {
        fn walk(s: &mehen_core::MetricSpace, out: &mut Vec<mehen_core::MetricSpace>) {
            if s.kind == mehen_core::SpaceKind::Function {
                out.push(s.clone());
            }
            for c in &s.spaces {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(&a.root, &mut out);
        out.remove(0)
    }

    let primary = ctor_space(&analyze_clean("class C(int x) { }"));
    let vocab =
        metrics_json::halstead(&primary.metrics).n1 + metrics_json::halstead(&primary.metrics).n2;
    assert!(
        vocab > 0.0,
        "the constructor must own the tokens of `(int x)`, got vocabulary {vocab}"
    );

    // NOT compared against the explicit spelling's vocabulary, deliberately: the two
    // occupy different source text. A primary constructor is `(int x)` — four tokens —
    // while `public C(int x) { }` is eight, because it repeats the type name and adds a
    // modifier and braces. Halstead measures the text, so it must differ; NOM, NArgs, NPM,
    // and WMC are the metrics that must agree, and they are pinned above.
    assert_eq!(metrics_json::nargs(&primary.metrics).total, 1.0);
}

#[test]
fn each_anonymous_object_member_is_its_own_boolean_context() {
    // REGRESSION, and the fourth instance of this shape (argument, interpolation hole,
    // initializer element, now anonymous-object member): each member of
    // `new { A = a && b, B = c && d }` is an independent expression, so there are two `&&`
    // runs. Its members are real `anonymous_object_member_declarator` rules, so unlike
    // initializer elements each isolates on its own rather than needing a per-child reset.
    let cognitive = |source: &str| metrics_json::cognitive(&analyze_clean(source).root.metrics).sum;
    let anon = cognitive(
        "class C {
             static object F(bool a, bool b, bool c, bool d) => new { A = a && b, B = c && d };
         }",
    );
    let locals = cognitive(
        "class C {
             static object F(bool a, bool b, bool c, bool d)
             {
                 var x = a && b;
                 var y = c && d;
                 return new { A = x, B = y };
             }
         }",
    );
    assert_eq!(anon, locals);
    assert_eq!(anon, 2.0, "two independent runs");

    // The guard: one member is still one run.
    assert_eq!(
        cognitive("class C { static object F(bool a, bool b) => new { A = a && b }; }"),
        1.0
    );
}

#[test]
fn a_primary_constructor_records_its_logical_line() {
    // REGRESSION, and a correction to my own earlier reasoning: opening the synthetic space
    // at the `parameter_list` gave it tokens but no logical line, because a parameter list
    // is not a declaration rule. `class C(int x) { }` reported LLOC 0 for its constructor
    // where `class C { C(int x) { } }` reports 1.
    //
    // This does NOT double-count the `class C(int x)` row: that row belongs to the *class*
    // space, recorded by `class_declaration`. This is the *constructor* space's own line —
    // exactly the precedent an expression-bodied lambda sets, which records one so it
    // matches its block-bodied twin.
    fn first_function(a: &mehen_core::LanguageAnalysis) -> mehen_core::MetricSpace {
        fn walk(s: &mehen_core::MetricSpace, out: &mut Vec<mehen_core::MetricSpace>) {
            if s.kind == mehen_core::SpaceKind::Function {
                out.push(s.clone());
            }
            for c in &s.spaces {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(&a.root, &mut out);
        out.remove(0)
    }

    let primary = first_function(&analyze_clean("class C(int x) { }"));
    let explicit = first_function(&analyze_clean("class C { public C(int x) { } }"));
    assert_eq!(
        metrics_json::loc(&primary.metrics).lloc,
        metrics_json::loc(&explicit.metrics).lloc
    );
    assert_eq!(metrics_json::loc(&primary.metrics).lloc, 1.0);

    for source in ["struct S(int x) { }", "record R(int X);"] {
        assert_eq!(
            metrics_json::loc(&first_function(&analyze_clean(source)).metrics).lloc,
            1.0,
            "{source}"
        );
    }
}

#[test]
fn an_explicitly_void_lambda_is_not_an_exit() {
    // REGRESSION introduced by the lambda-exit fix: that arm recorded an exit for *every*
    // non-block lambda body, but C# 10's `void () => Console.WriteLine()` declares a return
    // type and declares no value — so it disagreed with its own block-bodied twin.
    let closure_nexits = |source: &str| {
        let a = analyze_clean(source);
        fn walk(s: &mehen_core::MetricSpace, out: &mut Vec<f64>) {
            if s.kind == mehen_core::SpaceKind::Closure {
                out.push(metrics_json::nexits(&s.metrics).sum);
            }
            for c in &s.spaces {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(&a.root, &mut out);
        out
    };

    let arrow = closure_nexits(
        "using System;
         class C { static void F() { Action a = void () => Console.WriteLine(); a(); } }",
    );
    let block = closure_nexits(
        "using System;
         class C { static void F() { Action a = void () => { Console.WriteLine(); }; a(); } }",
    );
    assert_eq!(arrow, block, "the two void-lambda spellings must agree");
    assert_eq!(arrow, vec![0.0]);

    // The guard: an explicitly-typed *value*-returning lambda keeps its exit, so the check
    // reads the declared type rather than muting every typed lambda.
    assert_eq!(
        closure_nexits(
            "using System;
             class C { static void F() { Func<int,int> f = int (int x) => x + 1; f(1); } }"
        ),
        vec![1.0]
    );
    // And an untyped one, which has no `type?` slot at all.
    assert_eq!(
        closure_nexits(
            "using System;
             class C { static void F() { Func<int,int> f = x => x + 1; f(1); } }"
        ),
        vec![1.0]
    );
}
