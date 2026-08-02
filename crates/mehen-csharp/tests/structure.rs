// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Space-structure tests for the ANTLR C# walker: which constructs open a
//! metric space, what kind it is, and what it is named.
//!
//! These pin the shape of the reported tree, which every per-space metric
//! (NOM, NArgs, WMC, per-space LOC) depends on.

mod common;

use common::{analyze, analyze_clean};
use mehen_core::{MetricSpace, SpaceKind};

/// Flatten the space tree into `(depth, kind, name)` triples in tree order.
fn shape(root: &MetricSpace) -> Vec<(usize, String, Option<String>)> {
    fn walk(s: &MetricSpace, depth: usize, out: &mut Vec<(usize, String, Option<String>)>) {
        out.push((depth, s.kind.as_str().to_string(), s.name.clone()));
        for c in &s.spaces {
            walk(c, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

#[test]
fn type_kinds_map_to_space_kinds() {
    let a = analyze_clean(
        "class K { }
         struct S { }
         interface I { }
         enum E { A }",
    );
    let kinds: Vec<_> = shape(&a.root)
        .into_iter()
        .skip(1) // the unit
        .map(|(_, kind, name)| (kind, name))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("class".to_string(), Some("K".to_string())),
            // A `struct` is a class-like container (it carries WMC/NPA/NPM the
            // same way a class does).
            ("class".to_string(), Some("S".to_string())),
            ("interface".to_string(), Some("I".to_string())),
            ("enum".to_string(), Some("E".to_string())),
        ]
    );
}

#[test]
fn every_method_shape_opens_a_named_function_space() {
    let a = analyze_clean(
        "class C {
             public C() { }
             ~C() { }
             void M() { }
             public static C operator +(C a, C b) { return a; }
         }",
    );
    let names: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(_, _, name)| name)
        .collect();
    assert_eq!(
        names,
        vec![
            Some("C".to_string()), // constructor
            Some("C".to_string()), // destructor (`~C`)
            Some("M".to_string()), // method
            Some("operator +".to_string()),
        ]
    );
}

#[test]
fn both_property_accessors_are_sibling_spaces() {
    // The grammar nests the SECOND accessor inside the first's rule, but they
    // are siblings — and each must be named after its owning property.
    let a = analyze_clean(
        "class C {
             public int Count { get; set; }
         }",
    );
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(depth, _, name)| (depth, name))
        .collect();
    assert_eq!(
        functions,
        vec![
            (2, Some("Count.get".to_string())),
            (2, Some("Count.set".to_string())),
        ],
        "accessors must be siblings at the same depth, each named after the property"
    );
}

#[test]
fn event_accessors_are_sibling_spaces() {
    let a = analyze_clean(
        "class C {
             public event System.EventHandler E { add { } remove { } }
         }",
    );
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(depth, _, name)| (depth, name))
        .collect();
    assert_eq!(
        functions,
        vec![
            (2, Some("E.add".to_string())),
            (2, Some("E.remove".to_string())),
        ]
    );
}

#[test]
fn expression_bodied_property_opens_one_accessor() {
    // `int P => 1;` has no accessor list at all — the property itself is the
    // getter, so exactly one function space opens.
    let a = analyze_clean(
        "class C {
             public int P => 1;
         }",
    );
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(_, _, name)| name)
        .collect();
    // Exactly one getter, named as the block form would be — two properties that
    // are semantically the same getter must not report different NOM / NArgs /
    // WMC just because one uses `=> …` and the other `{ get { … } }`.
    assert_eq!(functions, vec![Some("P.get".to_string())]);
}

#[test]
fn lambda_and_anonymous_method_are_closures() {
    let a = analyze_clean(
        "class C {
             void F() {
                 System.Func<int, int> a = x => x + 1;
                 System.Func<int, int> b = delegate(int x) { return x; };
             }
         }",
    );
    let closures = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "closure")
        .count();
    assert_eq!(closures, 2);
}

#[test]
fn local_function_is_a_named_nested_function() {
    // A local function's name lives on its nested `local_function_header`, so
    // this pins that the walker reaches through it.
    let a = analyze_clean(
        "class C {
             void Outer() {
                 int Inner(int x) => x * 2;
             }
         }",
    );
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(depth, _, name)| (depth, name))
        .collect();
    assert_eq!(
        functions,
        vec![
            (2, Some("Outer".to_string())),
            (3, Some("Inner".to_string())),
        ]
    );
}

#[test]
fn interface_members_open_spaces_under_the_interface() {
    let a = analyze_clean(
        "interface I {
             double Area { get; }
             void Scale(double f);
         }",
    );
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(_, _, name)| name)
        .collect();
    assert_eq!(
        functions,
        vec![Some("Area.get".to_string()), Some("Scale".to_string())]
    );
}

#[test]
fn namespace_does_not_open_a_space_but_its_types_do() {
    // A namespace is not a metric space (it carries no complexity of its own);
    // the types inside it attach directly to the unit.
    let a = analyze_clean(
        "namespace N
         {
             class C { }
         }",
    );
    assert_eq!(a.root.kind, SpaceKind::Unit);
    assert_eq!(a.root.spaces.len(), 1);
    assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
    assert_eq!(a.root.spaces[0].name.as_deref(), Some("C"));
}

#[test]
fn nested_types_nest_their_spaces() {
    let a = analyze_clean(
        "class Outer {
             class Inner {
                 void M() { }
             }
         }",
    );
    assert_eq!(
        shape(&a.root),
        vec![
            (0, "unit".to_string(), None),
            (1, "class".to_string(), Some("Outer".to_string())),
            (2, "class".to_string(), Some("Inner".to_string())),
            (3, "function".to_string(), Some("M".to_string())),
        ]
    );
}

#[test]
fn an_extension_block_is_its_own_container() {
    // A C# 14 `extension(T x) { … }` block holds `member_declaration*` exactly as a
    // class body does, so it must open its own space — otherwise its members attach
    // to the enclosing static class and report as that class's own methods.
    // Anonymous, since the block declares no name.
    let a = analyze_clean(
        "static class E {
             extension(string s) {
                 public int Length => s.Length;
             }
         }",
    );
    assert_eq!(
        shape(&a.root),
        vec![
            (0, "unit".to_string(), None),
            (1, "class".to_string(), Some("E".to_string())),
            (2, "class".to_string(), None),
            (3, "function".to_string(), Some("Length.get".to_string())),
        ]
    );
}

#[test]
fn an_extension_block_holding_a_method_is_not_a_constructor() {
    // REGRESSION, and the third instance of the contextual-keyword ordering hazard
    // (`record`, `union`, now `extension`) — but the worst of them, because the
    // collision is with `constructor_declaration`:
    //
    //     constructor_declaration
    //       : attribute_list* modifier* identifier_token parameter_list … block
    //
    // is exactly the shape of `extension(string s) { … }`, and `member_declaration`
    // lists `base_method_declaration` before `base_type_declaration`. So the block
    // parsed as a *constructor named `extension`* holding its members as local
    // functions — with zero diagnostics and metrics identical to the `E(string s)`
    // constructor spelling.
    //
    // `an_extension_block_is_its_own_container` above did not catch it: it uses a
    // *property* member, and a property is not a legal statement, so the constructor
    // path dies there and ANTLR falls back to the type path. A method member IS a
    // legal statement (`local_function_statement` takes `modifier*`), so the
    // constructor path stayed viable end to end. Hence the assertion here is against
    // the `class` control rather than a literal shape — an extension container must
    // be indistinguishable from any other type container.
    let extension = analyze_clean(
        "static class E {
             extension(string s) {
                 public int L() { return s.Length; }
             }
         }",
    );
    let control = analyze_clean(
        "static class E {
             class Inner {
                 public int L() { return 1; }
             }
         }",
    );
    // Only the container's own name differs — an extension block declares none.
    let anonymize = |root: &MetricSpace| {
        shape(root)
            .into_iter()
            .map(|(depth, kind, name)| (depth, kind, if depth == 2 { None } else { name }))
            .collect::<Vec<_>>()
    };
    assert_eq!(anonymize(&extension.root), anonymize(&control.root));
    // Spelled out, so a shape change in both at once cannot pass silently.
    assert_eq!(
        anonymize(&extension.root),
        vec![
            (0, "unit".to_string(), None),
            (1, "class".to_string(), Some("E".to_string())),
            (2, "class".to_string(), None),
            (3, "function".to_string(), Some("L".to_string())),
        ]
    );
}

#[test]
fn extension_is_still_a_legal_identifier() {
    // Hoisting `type_declaration` must not make the word reserved — `extension` is a
    // contextual keyword, so it stays usable as an ordinary name.
    let a = analyze_clean("class C { void M() { int extension = 1; var x = extension; } }");
    assert_eq!(a.root.spaces[0].spaces.len(), 1);
}

#[test]
fn a_positional_record_is_a_type_not_a_method() {
    // REGRESSION, and the most consequential silent misparse yet: `record R(int X);`
    // — the single most common record spelling — parsed as a *method* named `R`
    // returning a type called `record`, because `record` is a contextual keyword and
    // `member_declaration` tries `base_method_declaration` before the type forms.
    // The record was reported as a function space with no NPA/NPM/WMC container, with
    // zero diagnostics. Only `record class R { }` parsed correctly, which is how it
    // survived: the explicit-kind form cannot match `method_declaration`.
    for source in [
        "record R(int X);",
        "record R(int X) { }",
        "record class R { }",
        "record struct R(int X);",
    ] {
        let a = analyze_clean(source);
        assert_eq!(
            a.root.spaces[0].kind,
            SpaceKind::Class,
            "`{source}` must open a type space"
        );
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("R"));
    }
}

#[test]
fn record_is_still_a_legal_identifier() {
    // Minting a real `KW_RECORD` token must not make `record` reserved: it is
    // contextual, so it is widened back into `identifier_token` and remains usable as
    // an ordinary name.
    let a = analyze_clean(
        "class C {
             void M() { int record = 1; var x = record; }
         }",
    );
    assert_eq!(a.root.spaces[0].spaces.len(), 1);
}

#[test]
fn a_property_with_both_accessors_is_not_a_record() {
    // The `record` fix needed a real token precisely because reordering alone put the
    // record path on the *committed* path here: `T P { get => …; set { … } }` predicted
    // `record_keyword` = `T`, and a predicate cannot prune a committed path — it
    // surfaced as a hard error on 29 corpus files. Pinned as a parse-clean assertion
    // plus the accessor shape.
    let a = analyze_clean("struct S { public T P { readonly get => 1; set { } } }");
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(_, _, name)| name)
        .collect();
    assert_eq!(
        functions,
        vec![Some("P.get".to_string()), Some("P.set".to_string())]
    );
}

#[test]
fn a_primary_constructor_is_a_function_space() {
    // A primary constructor's parameters live on the *type* declaration and no
    // `constructor_declaration` node exists anywhere, so `class C(int x)` reported
    // NOM 0 / NArgs 0 where the identical explicit form reported 1 / 1. Pinned
    // against the explicit form, since the point is the equivalence.
    let primary = analyze_clean("class C(int x) { }");
    let explicit = analyze_clean("class C { public C(int x) { } }");
    let names = |a: &mehen_core::LanguageAnalysis| {
        shape(&a.root)
            .into_iter()
            .filter(|(_, kind, _)| kind == "function")
            .map(|(_, _, name)| name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&primary), vec![Some("C".to_string())]);
    assert_eq!(names(&primary), names(&explicit));
}

#[test]
fn an_extension_receiver_is_not_a_primary_constructor() {
    // `extension(string s)` carries the same `parameter_list?` a primary constructor
    // does, but it is the extension *receiver*: nothing is constructed, and the block
    // has no name. Only the member inside it is a function.
    let a = analyze_clean(
        "static class E {
             extension(string s) { public int Length => s.Length; }
         }",
    );
    let functions: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(_, _, name)| name)
        .collect();
    assert_eq!(functions, vec![Some("Length.get".to_string())]);
}

#[test]
fn conversion_operators_are_named_by_their_target_type() {
    // REGRESSION. A conversion operator's name is its target type, which is a rule
    // child rather than a token — the code returned a bare `"operator"` while the
    // comment above it said otherwise. A type declaring several conversions reported
    // them all identically, indistinguishable in per-function output.
    let a = analyze_clean(
        "class C {
             public static implicit operator int(C c) => 0;
             public static explicit operator string(C c) => null;
         }",
    );
    let names: Vec<_> = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .map(|(_, _, name)| name)
        .collect();
    assert_eq!(
        names,
        vec![
            Some("operator int".to_string()),
            Some("operator string".to_string()),
        ]
    );
}

#[test]
fn a_union_declaration_opens_a_type_space() {
    // REGRESSION, and the same shape as the `record` misparse: `union` is only a
    // contextual keyword, so it is widened back into `identifier_token` and
    // `union Result { }` matched `method_declaration` with `union` as the return type.
    // It differs from `record` in one detail — a union *with members* forces the type
    // path, because a member body cannot follow a method signature — which is why only
    // the empty form mis-parsed and why this survived longer.
    let empty = analyze_clean("union Result { }");
    assert_eq!(empty.root.spaces[0].kind, SpaceKind::Class);
    assert_eq!(empty.root.spaces[0].name.as_deref(), Some("Result"));

    // With members it must match the `struct` control exactly.
    let union = analyze_clean("union Result { public int A; public void M() { } }");
    let structure = analyze_clean("struct Result { public int A; public void M() { } }");
    assert_eq!(shape(&union.root), shape(&structure.root));
}

#[test]
fn union_is_still_a_legal_identifier() {
    // Hoisting `union_declaration` must not make the word reserved.
    let a = analyze_clean("class C { void M() { int union = 1; var x = union; } }");
    assert_eq!(a.root.spaces[0].spaces.len(), 1);
}

#[test]
fn a_delegate_does_not_get_a_synthetic_constructor() {
    // REGRESSION. `delegate int D(int x);` carries a `parameter_list` because that IS
    // its signature, but the primary-constructor path matched on "has a parameter list"
    // and fabricated a function named `D` — inflating NOM/NArgs and rolling a phantom
    // method into the delegate's WMC, which is meant to be a childless space.
    let a = analyze_clean("delegate int D(int x);");
    assert_eq!(
        shape(&a.root),
        vec![
            (0, "unit".to_string(), None),
            (1, "class".to_string(), Some("D".to_string())),
        ],
        "a delegate opens a childless type space"
    );
}

#[test]
fn every_primary_constructor_form_still_opens_one() {
    // The allowlist must not drop a form that genuinely supports a primary constructor.
    for source in [
        "class C(int x) { }",
        "struct C(int x) { }",
        "record C(int X);",
    ] {
        let a = analyze_clean(source);
        let functions: Vec<_> = shape(&a.root)
            .into_iter()
            .filter(|(_, kind, _)| kind == "function")
            .map(|(_, _, name)| name)
            .collect();
        assert_eq!(
            functions,
            vec![Some("C".to_string())],
            "`{source}` must open one constructor space"
        );
    }
}

#[test]
fn an_interface_does_not_take_a_primary_constructor() {
    // REGRESSION. `interface I(int x) { }` is not valid C# — primary constructors are
    // for `class`, `struct`, and `record` — but Roslyn's permissive grammar accepts the
    // optional parameter list without a diagnostic, so listing `interface` in the
    // allowlist minted a constructor space for invalid source.
    let a = analyze("interface I(int x) { }");
    let functions = shape(&a.root)
        .into_iter()
        .filter(|(_, kind, _)| kind == "function")
        .count();
    assert_eq!(functions, 0, "no constructor for an invalid declaration");
}
