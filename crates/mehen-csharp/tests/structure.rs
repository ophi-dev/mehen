// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Space-structure tests for the ANTLR C# walker: which constructs open a
//! metric space, what kind it is, and what it is named.
//!
//! These pin the shape of the reported tree, which every per-space metric
//! (NOM, NArgs, WMC, per-space LOC) depends on.

mod common;

use common::analyze_clean;
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
