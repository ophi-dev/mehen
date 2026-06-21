// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Cognitive complexity tests for the tree-sitter-kotlin walker.
//!
//! Ports the legacy `kotlin_*` cognitive tests from
//! `crates/mehen-engine/src/legacy/metrics/cognitive.rs` byte-identical.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_kotlin::KotlinAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = KotlinAnalyzer::new();
    let file = SourceFile::new("foo.kt".into(), Language::Kotlin, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn kotlin_nested_if_increments_nesting() {
    let a = analyze(
        "fun f(a: Boolean, b: Boolean) {
             if (a) {      // +1
                 if (b) {  // +2 (nesting = 1)
                     println(\"hi\")
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(
        cog,
        @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }"###
    );
}

#[test]
fn kotlin_try_catch_nesting() {
    // SonarKotlin's `CognitiveComplexity` increments and bumps nesting on
    // `KtCatchClause`, not on the enclosing `try`. An `if` inside the
    // catch block therefore sees nesting=1 at the +1 structural cost.
    let a = analyze(
        "fun f() {
             try {
                 if (a) {       // +1 (try itself contributes 0)
                     println(\"a\")
                 }
             } catch (e: Exception) { // +1 catch
                 if (b) {               // +2 (nesting = 1 from catch)
                     println(\"b\")
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(
        cog,
        @r###"
    {
      "sum": 4.0,
      "average": 4.0,
      "min": 0.0,
      "max": 4.0
    }"###
    );
}

#[test]
fn kotlin_labeled_break_and_continue() {
    // Label-qualified `break@label` / `continue@label` flip the linear
    // flow and earn +1 each per the Sonar whitepaper. Unlabelled forms
    // don't.
    let a = analyze(
        "fun f() {
             outer@ for (i in 0..10) {        // +1 for
                 for (j in 0..10) {           // +2 (nesting=1)
                     if (i == j) {            // +3 (nesting=2)
                         continue@outer       // +1 labelled continue
                     }
                     if (j > 5) {             // +3 (nesting=2)
                         break@outer          // +1 labelled break
                     }
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(
        cog,
        @r###"
    {
      "sum": 11.0,
      "average": 11.0,
      "min": 0.0,
      "max": 11.0
    }"###
    );
}

#[test]
fn kotlin_else_if_counts_as_one() {
    // `else if` in Kotlin parses as an `if_expression` whose parent is
    // another `if_expression`. It should NOT increase nesting; only the
    // `else` keyword adds +1, matching other C-style languages.
    let a = analyze(
        "fun f(a: Int) {
             if (a > 0) {          // +1
                 println(\"pos\")
             } else if (a < 0) {   // +1
                 println(\"neg\")
             } else {              // +1
                 println(\"zero\")
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(
        cog,
        @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }"###
    );
}

#[test]
fn kotlin_nested_if_in_then_branch_is_not_else_if() {
    // Regression: an unbraced nested `if` in the *then* branch of an
    // outer `if` parses as `if_expression > control_structure_body >
    // if_expression`. The grammar also uses `control_structure_body`
    // for the `else` branch, so `is_else_if` must specifically check
    // that the body it lives in is the outer if's `alternative`, not
    // its `consequence`. Otherwise this nested-if is misclassified as
    // `else if` and cognitive complexity undercounts by 2 (no +1
    // structural cost and no +1 nesting).
    let a = analyze(
        "fun f(a: Boolean, b: Boolean) {
             if (a)            // +1
                 if (b)        // +2 (nesting = 1)
                     println(\"hi\")
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(
        cog,
        @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }"###
    );
}

#[test]
fn kotlin_nested_if_inside_else_if_chain_counts() {
    // Mixed shape: a nested `if` inside both the then-branch of the
    // outer `if` AND the body of an `else if`. The outer `if` counts
    // +1, the nested `if` in the then-branch counts +2 (nesting=1),
    // the `else if` counts +1 (flattened, no nesting), and its nested
    // `if` counts +2 (nesting=1) for a total of 6. This locks in that
    // the fix only flattens the else-branch, not the then-branch.
    let a = analyze(
        "fun f(a: Int, b: Int) {
             if (a > 0) {            // +1
                 if (b > 0) {        // +2 (nesting = 1)
                     println(\"x\")
                 }
             } else if (a < 0) {     // +1 (flattened else-if)
                 if (b > 0) {        // +2 (nesting = 1)
                     println(\"y\")
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(
        cog,
        @r###"
    {
      "sum": 6.0,
      "average": 6.0,
      "min": 0.0,
      "max": 6.0
    }"###
    );
}

#[test]
fn kotlin_nesting_preserved_after_nested_lambda() {
    // Regression: a lambda resets the cognitive context on entry. Sibling
    // code after the lambda (the second `if`) must still see the enclosing
    // `if`'s nesting. If the outer context isn't snapshotted *before* the
    // lambda's function-entry reset, the inner `if` under-counts (sum 2).
    let a = analyze(
        "fun f(a: Boolean, xs: List<Int>) {
             if (a) {                       // +1
                 xs.forEach { println(it) } // lambda resets context
                 if (a) {                   // +2 (nesting = 1, preserved)
                     println(\"x\")
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    assert_eq!(
        cog.sum, 3.0,
        "inner if must retain outer nesting after lambda"
    );
}

#[test]
fn kotlin_negation_breaks_boolean_sequence() {
    // `a && !b && c`: the prefix `!` separates the two `&&` runs, so the
    // second `&&` is not collapsed with the first → +2, not +1.
    let a = analyze(
        "fun g(a: Boolean, b: Boolean, c: Boolean): Boolean {
             return a && !b && c
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    assert_eq!(cog.sum, 2.0);
}

#[test]
fn kotlin_boolean_sequence_resets_between_call_statements() {
    // Two standalone calls each carrying `&&`. The boolean sequence must
    // reset at the statement boundary, so the second `&&` adds +1 instead
    // of collapsing with the first → +2, not +1.
    let a = analyze(
        "fun h(a: Boolean, b: Boolean, c: Boolean, d: Boolean) {
             foo(a && b)
             bar(c && d)
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    assert_eq!(cog.sum, 2.0);
}

/// Regression: the postfix `!!` not-null assertion shares the `EXCL_*`
/// tokens with the prefix `!` logical-not, but it must NOT break a boolean
/// run. `a && b!! && c` collapses both `&&` into one run → +1 (contrast
/// `a && !b && c` which is +2, see `kotlin_negation_breaks_boolean_sequence`).
#[test]
fn kotlin_not_null_assertion_does_not_break_boolean_sequence() {
    let a = analyze(
        "fun h(a: Boolean, b: Boolean?, c: Boolean): Boolean {
             return a && b!! && c
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    assert_eq!(cog.sum, 1.0);
}
