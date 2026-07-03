// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Cognitive complexity tests for the ANTLR Java walker (SonarSource rules).
//!
//! Nesting increments (`+1` plus the current nesting level): `if`, loops,
//! `switch`, `catch`, and the ternary. Flat `+1`: `else`/`else if`, labeled
//! `break`/`continue`. Sequences of like boolean operators collapse. `else if`
//! does not add a nesting level.

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
fn nested_structures_accumulate_nesting() {
    // for(+1) → if(+2) → while(+3) = 6.
    let a = analyze(
        "class C {
             void f(int[] xs) {
                 for (int x : xs) {
                     if (x > 0) {
                         while (x > 0) { x--; }
                     }
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 6.0,
      "average": 6.0,
      "min": 0.0,
      "max": 6.0
    }
    "###);
}

#[test]
fn boolean_sequence_collapses_like_operators() {
    // `if`(+1) then `a && b || c`: one `&&` run (+1) and one `||` run (+1) = 3.
    let a = analyze(
        "class C {
             boolean check(boolean a, boolean b, boolean c) {
                 if (a && b || c) { return true; }
                 return false;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn mixed_boolean_operators_count_each_run() {
    // `if`(+1) then `a && b || c && d`: three like-operator runs — the first
    // `&&` (+1), the `||` (+1), and the second `&&` (+1) — because switching
    // operator ends a run and switching back starts a new one. Total = 4.
    let a = analyze(
        "class C {
             boolean check(boolean a, boolean b, boolean c, boolean d) {
                 if (a && b || c && d) { return true; }
                 return false;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 4.0,
      "average": 4.0,
      "min": 0.0,
      "max": 4.0
    }
    "###);
}

#[test]
fn else_if_does_not_add_nesting() {
    // if(+1), else if → flat else(+1) + the if is an else-branch so no
    // nesting, else(+1) = 3 total.
    let a = analyze(
        "class C {
             int f(int x) {
                 if (x > 2) { return 2; }
                 else if (x > 1) { return 1; }
                 else { return 0; }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn switch_expression_scores_like_switch_statement() {
    // Regression (audit): a switch *expression* (Java 14+) must get the same
    // cognitive nesting as the statement form. Here: switch expr(+1) then a
    // nested `if` in an arm at nesting 1 (+2) = 3.
    let expr = analyze(
        "class C {
             int f(int x) {
                 int y = switch (x) {
                     case 1 -> { if (x > 0) { yield 1; } yield 2; }
                     default -> 0;
                 };
                 return y;
             }
         }",
    );
    let stmt = analyze(
        "class C {
             int f(int x) {
                 switch (x) {
                     case 1: if (x > 0) { return 1; } return 2;
                     default: return 0;
                 }
             }
         }",
    );
    let e = mehen_report::metrics_json::cognitive(&expr.root.metrics);
    let s = mehen_report::metrics_json::cognitive(&stmt.root.metrics);
    let ej = serde_json::to_value(&e).unwrap();
    let sj = serde_json::to_value(&s).unwrap();
    assert_eq!(
        ej, sj,
        "switch expression and switch statement must score identically"
    );
    insta::assert_json_snapshot!(e, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn nested_ternary_deepens_nesting() {
    // Regression (audit): a ternary nested in another ternary's operand is one
    // level deeper. `a>0 ? (b>0 ? 1 : 2) : 3`: outer ternary(+1 at level 0),
    // inner ternary(+2 at level 1) = 3.
    let a = analyze(
        "class C {
             int f(int a, int b) {
                 return a > 0 ? (b > 0 ? 1 : 2) : 3;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn catch_adds_nesting_increment() {
    // `catch`(+1). `try` itself adds nothing.
    let a = analyze(
        "class C {
             void f() {
                 try { risky(); } catch (Exception e) { }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 1.0,
      "average": 1.0,
      "min": 0.0,
      "max": 1.0
    }
    "###);
}
