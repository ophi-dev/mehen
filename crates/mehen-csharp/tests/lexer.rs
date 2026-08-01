// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Tokenization tests for the hand-written C# lexer.
//!
//! Roslyn publishes a parser-only grammar, so every terminal in
//! `mehen-csharp-parser/grammar/lexer-tokens.g4.in` is ours. A wrong token
//! *boundary* there is the most dangerous kind of bug in this crate: an
//! over-greedy rule still yields a valid token, so the parser reports no error and
//! the swallowed code silently disappears from every metric. These tests pin the
//! boundaries by measuring what survives — LLOC counts the statements a token span
//! did not eat.
//!
//! See `PROVENANCE.md`: a clean corpus run measures *parseability*, not
//! correctness.

mod common;

use common::analyze_clean;
use mehen_report::metrics_json;

/// LLOC for a snippet that must also parse cleanly.
fn lloc(source: &str) -> f64 {
    metrics_json::loc(&analyze_clean(source).root.metrics).lloc
}

#[test]
fn an_empty_string_does_not_swallow_the_statements_after_it() {
    // REGRESSION. The raw-string rule was once fenced with TWO quotes
    // (`'""' ~[\r\n]*? '""'`), but `""` is the empty string literal — so this line
    // lexed as one "raw string" spanning from the first `""` to the last, eating
    // both statements between them. It produced no diagnostic, because the result
    // was a perfectly valid token.
    //
    // class(1) + method(1) + 3 locals = 5. Anything less means a token boundary
    // is eating code.
    assert_eq!(
        lloc(
            "class C
             {
                 void M()
                 {
                     var a = \"\";
                     int x = 1;
                     var b = \"\";
                 }
             }"
        ),
        5.0
    );
}

#[test]
fn a_single_line_raw_string_is_fenced_with_three_quotes() {
    // C# 11 raw strings fence with *at least* three quotes in both the
    // single- and multi-line forms; the distinction is whether the content holds a
    // newline. class(1) + method(1) + 2 locals = 4.
    assert_eq!(
        lloc(
            "class C
             {
                 void M()
                 {
                     var a = \"\"\"x\"\"\";
                     int y = 1;
                 }
             }"
        ),
        4.0
    );
}

#[test]
fn a_multi_line_raw_string_spans_rows_without_eating_the_next_statement() {
    let source = "class C
         {
             void M()
             {
                 var a = \"\"\"
                 line
                 \"\"\";
                 int y = 1;
             }
         }";
    assert_eq!(lloc(source), 4.0);
    // Every interior row of the literal is code, not a phantom blank.
    let loc = metrics_json::loc(&analyze_clean(source).root.metrics);
    assert_eq!(loc.blank, 0.0);
}

#[test]
fn both_verbatim_interpolation_prefixes_parse() {
    // `$@"…"` and `@$"…"` are the same string in C# 11+. Roslyn's grammar spells
    // only the first, so the second needs its own lexer alternative — without it
    // `@$"a{X}"` failed to tokenize at all.
    for prefix in ["$@", "@$"] {
        let source = format!(
            "class C
             {{
                 void M()
                 {{
                     var X = 1;
                     var s = {prefix}\"a{{X}}b\";
                 }}
             }}"
        );
        assert_eq!(lloc(&source), 4.0, "prefix `{prefix}\"` must parse");
    }
}

#[test]
fn an_interpolated_raw_string_parses_its_holes() {
    // `$"""a{x}b"""` needs its own lexer mode: in the default mode the `a` between
    // holes lexes as an IDENTIFIER, which is what made this shape report three
    // diagnostics. class(1) + method(1) + 2 locals = 4.
    assert_eq!(
        lloc(
            "class C
             {
                 void M()
                 {
                     var X = 1;
                     var s = $\"\"\"a{X}b\"\"\";
                 }
             }"
        ),
        4.0
    );
}

#[test]
fn a_quote_inside_an_interpolated_raw_string_is_literal_text() {
    // Only a run of three quotes closes a raw string, so the lone `"` here is
    // content and must not end the literal early.
    assert_eq!(
        lloc(
            "class C
             {
                 void M()
                 {
                     var X = 1;
                     var s = $\"\"\"say \"hi\" {X}\"\"\";
                     int y = 1;
                 }
             }"
        ),
        5.0
    );
}

#[test]
fn a_nested_brace_inside_an_interpolation_hole_does_not_close_it() {
    // The `}` of the collection initializer is lexically identical to the one that
    // ends the hole; the grammar's brace-depth predicates are what tell them apart.
    assert_eq!(
        lloc(
            "class C
             {
                 void M()
                 {
                     var s = $\"a{ new[]{ 1, 2 }.Length }b\";
                     int y = 1;
                 }
             }"
        ),
        4.0
    );
}

#[test]
fn an_interpolation_format_specifier_is_not_code() {
    // `D4` must not lex as an identifier, and the `:` that introduces it must not
    // be read as an ordinary colon.
    assert_eq!(
        lloc(
            "class C
             {
                 void M()
                 {
                     var X = 1;
                     var s = $\"{X:D4}\";
                     int y = 1;
                 }
             }"
        ),
        5.0
    );
}
