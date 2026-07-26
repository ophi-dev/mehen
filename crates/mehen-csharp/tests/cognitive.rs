// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Cognitive complexity tests for the ANTLR C# walker.
//!
//! Follows SonarSource's cognitive-complexity specification: nesting
//! increments on `if`, loops, `switch`, `catch`, and the ternary (each costing
//! `1 + current nesting`); flat `+1` on `else`/`else if` and `goto`; and a
//! collapsing boolean run on `&&`/`||` that adds `+1` per operator-kind change.
//!
//! Notably `try` and `lock` add NOTHING — the spec increments on the handler
//! (`catch`), not the guarded block.

mod common;

use common::analyze_clean;

fn sum(a: &mehen_core::LanguageAnalysis) -> f64 {
    mehen_report::metrics_json::cognitive(&a.root.metrics).sum
}

#[test]
fn flat_if_costs_one() {
    let a = analyze_clean(
        "class C {
             void F(int a) { if (a > 0) { } }
         }",
    );
    assert_eq!(sum(&a), 1.0);
}

#[test]
fn nested_if_costs_one_plus_nesting() {
    // outer if(1) + inner if(1 + 1 nesting) = 3
    let a = analyze_clean(
        "class C {
             void F(int a, int b) {
                 if (a > 0) {
                     if (b > 0) { }
                 }
             }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn else_adds_a_flat_increment() {
    // if(1) + else(1) = 2
    let a = analyze_clean(
        "class C {
             void F(int a) { if (a > 0) { } else { } }
         }",
    );
    assert_eq!(sum(&a), 2.0);
}

#[test]
fn else_if_does_not_add_nesting() {
    // Per SonarSource's spec an `if`/`else if` chain costs +1 per branch
    // keyword and adds NO nesting: if(1) + else-if(1) = 2. Scoring the inner
    // `if` as a nested `if` would give 3 — this pins that it does not, and
    // matches `mehen-java` on the same shape.
    let a = analyze_clean(
        "class C {
             void F(int a) {
                 if (a > 0) { } else if (a < 0) { }
             }
         }",
    );
    assert_eq!(sum(&a), 2.0);
}

#[test]
fn full_else_if_else_chain_costs_one_per_branch() {
    // if(1) + else-if(1) + else(1) = 3, all flat.
    let a = analyze_clean(
        "class C {
             void F(int a) {
                 if (a > 0) { } else if (a < 0) { } else { }
             }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn else_with_a_braced_nested_if_does_nest() {
    // `else { if … }` is a genuinely nested `if`, unlike `else if`:
    // if(1) + else(1) + nested if(1 + 1 nesting) = 4.
    let a = analyze_clean(
        "class C {
             void F(int a) {
                 if (a > 0) { } else { if (a < 0) { } }
             }
         }",
    );
    assert_eq!(sum(&a), 4.0);
}

#[test]
fn try_adds_nothing_but_catch_nests() {
    // `try` scores 0; `catch` scores 1 (flat, at nesting 0).
    let a = analyze_clean(
        "class C {
             void F() {
                 try { } catch (System.Exception) { }
             }
         }",
    );
    assert_eq!(sum(&a), 1.0);
}

#[test]
fn catch_inside_a_loop_pays_the_loop_nesting() {
    // foreach(1) + catch(1 + 1 nesting) = 3. This is the shape that caught a
    // real bug: scoring `try` too would make it 4+.
    let a = analyze_clean(
        "class C {
             void F(int[] xs) {
                 foreach (var x in xs) {
                     try { } catch (System.Exception) { }
                 }
             }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn lock_adds_nothing() {
    let a = analyze_clean(
        "class C {
             private readonly object _g = new object();
             void F() { lock (_g) { } }
         }",
    );
    assert_eq!(sum(&a), 0.0);
}

#[test]
fn switch_nests_once_regardless_of_case_count() {
    // `switch` costs 1; the individual `case` labels add no cognitive cost.
    let a = analyze_clean(
        "class C {
             void F(int v) {
                 switch (v) {
                     case 1: break;
                     case 2: break;
                     default: break;
                 }
             }
         }",
    );
    assert_eq!(sum(&a), 1.0);
}

#[test]
fn same_boolean_operator_run_collapses() {
    // `a && b && c` is ONE run → if(1) + run(1) = 2.
    let a = analyze_clean(
        "class C {
             void F(bool a, bool b, bool c) { if (a && b && c) { } }
         }",
    );
    assert_eq!(sum(&a), 2.0);
}

#[test]
fn mixed_boolean_operators_count_each_change() {
    // `a && b || c` changes operator once → if(1) + 2 = 3.
    let a = analyze_clean(
        "class C {
             void F(bool a, bool b, bool c) { if (a && b || c) { } }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn boolean_runs_do_not_collapse_across_statements() {
    // Two separate statements, each one `&&` run → 2 (not 1).
    let a = analyze_clean(
        "class C {
             bool G(bool x) { return x; }
             void F(bool a, bool b) {
                 G(a && b);
                 G(a && b);
             }
         }",
    );
    assert_eq!(sum(&a), 2.0);
}

#[test]
fn ternary_nests_like_an_if() {
    let a = analyze_clean(
        "class C {
             int F(bool a) { return a ? 1 : 2; }
         }",
    );
    assert_eq!(sum(&a), 1.0);
}

#[test]
fn goto_adds_a_flat_increment() {
    let a = analyze_clean(
        "class C {
             void F() {
                 start:
                 goto start;
             }
         }",
    );
    assert_eq!(sum(&a), 1.0);
}

#[test]
fn a_method_in_a_nested_type_starts_fresh() {
    // The inner type's method must not inherit the outer method's nesting:
    // outer if(1) + inner if(1) = 2, not 3.
    let a = analyze_clean(
        "class Outer {
             void F(int a) {
                 if (a > 0) { }
             }
             class Inner {
                 void G(int b) {
                     if (b > 0) { }
                 }
             }
         }",
    );
    assert_eq!(sum(&a), 2.0);
}
