// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Cyclomatic complexity tests for the ANTLR C# walker.
//!
//! Decisions (SonarC#-aligned): `if`, every loop (`while`/`do`/`for`/
//! `foreach`), each `case` label, the ternary `?:`, and each short-circuit
//! `&&`/`||`. `switch` itself, `catch`, `else`, `try`, and `default:` are not
//! decisions. Every function space contributes a base McCabe `+1`, as does the
//! enclosing type space — so the unit `sum` folds in the type(1) and each
//! member's McCabe value.

mod common;

use common::analyze_clean;

fn sum(a: &mehen_core::LanguageAnalysis) -> f64 {
    mehen_report::metrics_json::cyclomatic(&a.root.metrics).sum
}

#[test]
fn simple_if_is_one_decision() {
    // unit(1) + class(1) + method(1 + 1 if) = 4
    let a = analyze_clean(
        "class C {
             int F(int a, int b) {
                 if (a > b) { return a; }
                 return b;
             }
         }",
    );
    assert_eq!(sum(&a), 4.0);
}

#[test]
fn else_is_not_a_decision() {
    // Only the `if` counts; `else` adds nothing to cyclomatic.
    let a = analyze_clean(
        "class C {
             int F(int a) {
                 if (a > 0) { return 1; } else { return 2; }
             }
         }",
    );
    assert_eq!(sum(&a), 4.0);
}

#[test]
fn every_loop_form_is_one_decision() {
    // unit(1) + class(1) + method(1 + while + do + for + foreach) = 7
    let a = analyze_clean(
        "class C {
             void F(int[] xs) {
                 while (true) { break; }
                 do { break; } while (true);
                 for (int i = 0; i < 1; i++) { }
                 foreach (var x in xs) { }
             }
         }",
    );
    assert_eq!(sum(&a), 7.0);
}

#[test]
fn switch_itself_is_not_a_decision_but_cases_are() {
    // unit(1) + class(1) + method(1 + case 1 + case 2) = 5.
    // `switch` and `default:` add nothing.
    let a = analyze_clean(
        "class C {
             int F(int v) {
                 switch (v) {
                     case 1: return 1;
                     case 2: return 2;
                     default: return 0;
                 }
             }
         }",
    );
    assert_eq!(sum(&a), 5.0);
}

#[test]
fn ternary_is_a_decision() {
    let a = analyze_clean(
        "class C {
             int F(int a) { return a > 0 ? 1 : 2; }
         }",
    );
    assert_eq!(sum(&a), 4.0);
}

#[test]
fn each_short_circuit_operator_is_a_decision() {
    // unit(1) + class(1) + method(1 + if + && + ||) = 6
    let a = analyze_clean(
        "class C {
             bool F(bool a, bool b, bool c) {
                 if (a && b || c) { return true; }
                 return false;
             }
         }",
    );
    assert_eq!(sum(&a), 6.0);
}

#[test]
fn catch_and_try_are_not_decisions() {
    // unit(1) + class(1) + method(1) = 3 — neither `try` nor `catch` counts.
    let a = analyze_clean(
        "class C {
             void F() {
                 try { G(); } catch (System.Exception) { }
             }
             void G() { }
         }",
    );
    // Two methods, so unit(1) + class(1) + F(1) + G(1) = 4.
    assert_eq!(sum(&a), 4.0);
}

#[test]
fn null_coalescing_is_not_a_decision() {
    // `??` is an ABC condition but not a McCabe decision (it is not a
    // short-circuit *boolean* operator in SonarSource's decision list).
    let a = analyze_clean(
        "class C {
             string F(string s) { return s ?? \"d\"; }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn a_switch_expression_scores_like_a_switch_statement() {
    // REGRESSION. A switch *expression* scored no decisions at all, so rewriting a
    // switch statement into the expression form — which is the idiomatic modern C#
    // spelling of exactly the same branching — silently lowered the score. Pinned
    // against the statement form rather than an absolute number, since the point is
    // the equivalence.
    let expression = analyze_clean(
        "class C {
             int F(int v) {
                 return v switch { 1 => 1, 2 => 2, _ => 0 };
             }
         }",
    );
    let statement = analyze_clean(
        "class C {
             int F(int v) {
                 switch (v) {
                     case 1: return 1;
                     case 2: return 2;
                     default: return 0;
                 }
             }
         }",
    );
    assert_eq!(sum(&expression), sum(&statement));
    // unit(1) + class(1) + method(1 + 2 arms) = 5.
    assert_eq!(sum(&expression), 5.0);
}

#[test]
fn pattern_combinators_are_decisions() {
    // REGRESSION. `and`/`or` are C# 9's spelling of `&&`/`||` in pattern position;
    // they must count the same. `not` mirrors `!` and is not itself a decision.
    // unit(1) + class(1) + method(1 + 1 `and`) = 4.
    let a = analyze_clean(
        "class C {
             bool F(object o) { return o is int and long; }
         }",
    );
    assert_eq!(sum(&a), 4.0);
}

#[test]
fn a_negated_pattern_is_not_a_decision() {
    // `is not null` is one type test, no combinator decision — same as `!x`.
    let a = analyze_clean(
        "class C {
             bool F(object o) { return o is not null; }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn generic_delimiters_are_not_decisions() {
    // The `<`/`>` of a generic type are delimiters, not comparisons — and a
    // comparison is not a McCabe decision anyway, so this pins that the delimiter
    // handling did not accidentally start recording one.
    let a = analyze_clean(
        "class C {
             System.Collections.Generic.List<int> F() { return null; }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}
