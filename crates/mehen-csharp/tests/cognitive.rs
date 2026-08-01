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

#[test]
fn a_switch_expression_nests_like_a_switch_statement() {
    // REGRESSION. A switch *expression* added no cognitive nesting, so the modern
    // spelling of the same branching scored 0 where the statement form scored 1.
    let expression = analyze_clean(
        "class C {
             int F(int v) {
                 return v switch { 1 => 1, _ => 0 };
             }
         }",
    );
    let statement = analyze_clean(
        "class C {
             int F(int v) {
                 switch (v) { case 1: return 1; default: return 0; }
             }
         }",
    );
    assert_eq!(sum(&expression), sum(&statement));
    assert_eq!(sum(&expression), 1.0);
}

#[test]
fn a_switch_expression_nested_in_an_if_costs_its_depth() {
    // The nesting increment must be a real level, not a flat +1: the inner switch
    // expression sits one level deep, so it costs 2 on top of the `if`'s 1.
    let a = analyze_clean(
        "class C {
             int F(int v, bool flag) {
                 if (flag) {
                     return v switch { 1 => 1, _ => 0 };
                 }
                 return 0;
             }
         }",
    );
    assert_eq!(sum(&a), 3.0);
}

#[test]
fn pattern_combinators_score_as_boolean_operators() {
    // `and`/`or` are C# 9's pattern-position spelling of `&&`/`||`, so they feed the
    // same run-collapsing tracker. A run of the SAME combinator is one increment.
    let one_run = analyze_clean(
        "class C {
             bool F(object o) { return o is int and long and short; }
         }",
    );
    assert_eq!(sum(&one_run), 1.0, "a same-operator run collapses to +1");

    // Mixing them breaks the run, exactly as `a && b || c` does.
    let mixed = analyze_clean(
        "class C {
             bool F(object o) { return o is (int and long) or string; }
         }",
    );
    assert_eq!(sum(&mixed), 2.0);
}

#[test]
fn sibling_field_initializers_are_independent_boolean_contexts() {
    // REGRESSION. Two field initializers share the enclosing *type* space rather
    // than a statement, so neither hit any of the statement-shaped rules that reset
    // the boolean-run tracker — the two `&&` runs collapsed into one and the pair
    // scored 1. Pinned against the equivalent locals, which always scored 2.
    let fields = analyze_clean(
        "class C {
             bool A = X() && Y();
             bool B = U() && V();
             static bool X() => true;
             static bool Y() => true;
             static bool U() => true;
             static bool V() => true;
         }",
    );
    let locals = analyze_clean(
        "class C {
             void F() {
                 bool a = X() && Y();
                 bool b = U() && V();
             }
             static bool X() => true;
             static bool Y() => true;
             static bool U() => true;
             static bool V() => true;
         }",
    );
    assert_eq!(sum(&fields), 2.0, "two independent `&&` runs");
    assert_eq!(sum(&fields), sum(&locals));
}

#[test]
fn negation_does_not_break_a_boolean_run() {
    // REGRESSION, and a cross-language inconsistency: C# scored `a && !b && c` as 2
    // while `mehen-java` scored the identical logic as 1.
    //
    // Java is right. Both SonarJava (`CognitiveComplexityVisitor
    // .flattenLogicalExpression`) and SonarKotlin (`CognitiveComplexity
    // .flattenOperators`) flatten only the `&&`/`||` operators and treat a negated
    // operand as a plain operand where flattening stops — the `!` is invisible to the
    // run. See `mehen-java/tests/cognitive.rs::negation_does_not_break_boolean_run`,
    // which cites both.
    let negated = analyze_clean(
        "class C {
             static bool F(bool a, bool b, bool c) { return a && !b && c; }
         }",
    );
    let plain = analyze_clean(
        "class C {
             static bool F(bool a, bool b, bool c) { return a && b && c; }
         }",
    );
    assert_eq!(sum(&negated), 1.0, "the `!` must not split the `&&` run");
    assert_eq!(sum(&negated), sum(&plain));

    // Multiple negations in one run are equally invisible.
    let many = analyze_clean(
        "class C {
             static bool F(bool a, bool b, bool c) { return !a && !b && c; }
         }",
    );
    assert_eq!(sum(&many), 1.0);
}

#[test]
fn mixing_boolean_operators_still_costs_two() {
    // The counterpart to the negation fix: ignoring `!` must not also collapse a
    // genuine operator *change*. `a && b || c` is two runs.
    let a = analyze_clean(
        "class C {
             static bool F(bool a, bool b, bool c) { return a && b || c; }
         }",
    );
    assert_eq!(sum(&a), 2.0);
}
