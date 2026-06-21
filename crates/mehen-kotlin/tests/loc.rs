// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC tests for the tree-sitter-kotlin walker.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_kotlin::KotlinAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    // Match the legacy `check_metrics` test harness: trim whitespace,
    // append a single trailing newline. The line-count helpers in
    // `LineIndex` count `\n` boundaries so the trailing newline pushes
    // SLOC up by one row, matching the legacy snapshots.
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = KotlinAnalyzer::new();
    let file = SourceFile::new("foo.kt".into(), Language::Kotlin, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn kotlin_simple_loc() {
    let a = analyze(
        "// header
         fun greet(name: String) {
             println(\"hi, \" + name)
         }",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    insta::assert_json_snapshot!(loc);
}

#[test]
fn kotlin_nested_calls_do_not_add_extra_lloc() {
    let a = analyze(
        "fun f() {
             val x = foo(bar())
             foo(bar())
         }",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    insta::assert_json_snapshot!(
        loc,
        @r###"
    {
      "sloc": 4.0,
      "ploc": 4.0,
      "lloc": 3.0,
      "cloc": 0.0,
      "blank": 0.0,
      "sloc_average": 2.0,
      "ploc_average": 2.0,
      "lloc_average": 1.5,
      "cloc_average": 0.0,
      "blank_average": 0.0,
      "sloc_min": 4.0,
      "sloc_max": 4.0,
      "cloc_min": 0.0,
      "cloc_max": 0.0,
      "ploc_min": 4.0,
      "ploc_max": 4.0,
      "lloc_min": 3.0,
      "lloc_max": 3.0,
      "blank_min": 0.0,
      "blank_max": 0.0
    }"###
    );
}

#[test]
fn kotlin_counts_companion_and_accessors_as_lloc() {
    let a = analyze(
        "class C {
             companion object {
                 fun make() = C()
             }

             var x: Int = 0
                 get() = field
                 set(value) { field = value }
         }",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    assert_eq!(loc.lloc, 7.0);
}

/// Regression: a control-flow expression used as a statement (`if`,
/// `return`, …) is counted as LLOC by its own rule arm. The
/// `statement → expression` arm must NOT count it again, or every bare
/// `if`/`when`/`try`/`return`/`throw` statement records two LLOC.
#[test]
fn kotlin_control_flow_statements_count_lloc_once() {
    let a = analyze(
        "fun f(a: Int): Int {
             if (a > 0) { foo() }
             bar()
             return a
         }",
    );
    // f (1) + if (1) + foo() (1) + bar() (1) + return (1) = 5.
    // (Pre-fix: the `if` and `return` would each count twice → 7.)
    assert_eq!(mehen_report::metrics_json::loc(&a.root.metrics).lloc, 5.0);
}

/// Regression: a multiline block comment that starts after code on the same
/// line (`val x = /* … */ 1`) is classified as a code-comment, not
/// comment-only — comments are now routed in source order after the AST walk
/// (which seeds each space's known code lines), so the "comment shares a line
/// with code" check sees the code. The file totals stay correct.
#[test]
fn kotlin_inline_block_comment_after_code() {
    let a = analyze(
        "fun f(): Int {
             val x = /* trailing */ 1
             return x
         }",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    // The inline block comment shares the `val x = … 1` line, so it is a
    // code-comment: cloc counts it but it adds no comment-only/blank line.
    assert_eq!(loc.cloc, 1.0);
    assert_eq!(loc.blank, 0.0);
}

/// Regression: Kotlin folds optional trivia into certain operator tokens
/// (`NOT_IS: '!is' (Hidden|NL)`), so a comment glued to the operator
/// (`x !is/* note */ Int`) lives inside the operator token's text rather than
/// a standalone comment token. The LOC pass scans these trivia-bearing
/// operators for embedded comments so CLOC isn't undercounted.
#[test]
fn kotlin_comment_embedded_in_operator_token_counts_as_cloc() {
    let a = analyze(
        "fun f(x: Any): Boolean {
             return x !is/* note */ Int
         }",
    );
    assert_eq!(mehen_report::metrics_json::loc(&a.root.metrics).cloc, 1.0);
}

/// Regression: `AT_PRE_WS`/`AT_BOTH_WS` annotation tokens fold the *leading*
/// newline into the token text (`"\n@"`), so `line()` points at the blank
/// line before the annotation. The PLOC observation advances past leading
/// newlines, so a blank line before an annotated declaration is not counted
/// as code.
#[test]
fn kotlin_blank_line_before_annotation_is_not_ploc() {
    // 4 source lines: `fun a()`, blank, `@Deprecated`, `fun b()`.
    let a = analyze("fun a() {}\n\n@Deprecated\nfun b() {}\n");
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    assert_eq!(loc.ploc, 3.0, "the blank line must not count as code");
    assert_eq!(loc.blank, 1.0);
}
