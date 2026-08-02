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

#[test]
fn a_diagnostic_span_covers_exactly_the_offending_token() {
    // REGRESSION. The error-node span added 1 to the runtime's `stop_byte`, but that
    // offset is already **exclusive** — `Token::byte_span` is
    // `start_byte()..stop_byte()`. Every diagnostic span was therefore one byte too
    // long, so a `(` was reported as `"( "`, swallowing the following character. This
    // is also the only place in `mehen-antlr` that did so: `span.rs` and `comments.rs`
    // already used the offset directly.
    let source = "class C { void M( }\n";
    let a = common::analyze(source);
    assert!(!a.diagnostics.is_empty(), "this input must not parse");
    for diagnostic in &a.diagnostics {
        let Some(span) = diagnostic.span else {
            continue;
        };
        let text = &source[span.start_byte as usize..span.end_byte as usize];
        assert!(
            !text.ends_with(' '),
            "span {}..{} = {text:?} runs past its token",
            span.start_byte,
            span.end_byte
        );
    }
}

#[test]
fn an_interpolation_format_clause_may_contain_a_quoted_literal() {
    // REGRESSION. A custom numeric format can carry a quoted literal —
    // `$"{n:0\"kg\"}"` is valid C# for "the number, then kg" — but the
    // INTERPOLATION_FORMAT mode had no rule for `"` at all, so the backslash lexed as
    // ordinary text and the following quote could not be consumed.
    //
    // The fix has to keep the whole clause as ONE token: the parser rule is
    // `interpolation_format_clause : ':' interpolated_string_text_token`, so emitting
    // the escape separately made the clause unparsable (the first attempt did exactly
    // that and traded three lexer errors for two parser errors).
    assert_eq!(
        lloc("class C { static string M(int n) => $\"{n:0\\\"kg\\\"}\"; }"),
        2.0
    );
}

#[test]
fn a_verbatim_format_clause_may_contain_a_doubled_quote() {
    // The verbatim spelling of the same thing. Both escapes are accepted in this mode
    // because the enclosing string decides which is legal, and the clause's extent is
    // all any metric reads.
    assert_eq!(
        lloc("class C { static string M(int n) => $@\"{n:0\"\"kg\"\"}\"; }"),
        2.0
    );
}

#[test]
fn a_backslash_in_a_format_clause_is_ordinary_text() {
    // A backslash that is not part of an escaped quote must still lex — the first fix
    // attempt broke this by giving `\` its own rule after the escape rule.
    assert_eq!(
        lloc("class C { static string M(int n) => $\"{n:0\\\\0}\"; }"),
        2.0
    );
}

#[test]
fn an_alignment_and_format_clause_together_still_parse() {
    // `{n,5:D4}` — the alignment clause is a separate rule reached before the format
    // one, so this pins that widening the format text did not disturb it.
    assert_eq!(
        lloc("class C { static string M(int n) => $\"{n,5:D4}\"; }"),
        2.0
    );
}

#[test]
fn a_parenthesized_ternary_in_an_interpolation_hole_parses() {
    // REGRESSION. `nestDepth` counted only braces, so the ternary's `:` sat at depth 0
    // and INTERP_FORMAT_COLON claimed it as a format delimiter — `2)` became
    // interpolation text and `$"{(flag ? 1 : 2)}"` reported four diagnostics. The
    // counter now tracks every bracketing construct inside a hole, since the only
    // question it answers is whether a `:` is still part of the expression.
    assert_eq!(
        lloc("class C { static string M(bool flag) => $\"{(flag ? 1 : 2)}\"; }"),
        2.0
    );
}

#[test]
fn brackets_in_an_interpolation_hole_parse() {
    // The `[`/`]` half of the same counter — an indexer or a dictionary initializer
    // inside a hole has the same shape as the parenthesized ternary.
    assert_eq!(
        lloc("class C { static string M(int[] a) => $\"{a[0]}\"; }"),
        2.0
    );
}

#[test]
fn a_format_clause_does_not_leak_hole_state() {
    // REGRESSION, and the worse of the two: INTERP_FORMAT_END popped both lexer modes
    // but left its `holeStack` entry behind, so `holeStack.Count > 0` stayed true after
    // the string ended. The next `:` anywhere in the file then matched
    // INTERP_FORMAT_COLON and pushed INTERPOLATION_FORMAT again, swallowing the rest of
    // the line as format text — here, a ternary three tokens later.
    assert_eq!(
        lloc(
            "class C {
             static int M(int n, bool flag) {
                 var s = $\"{n:D4}\";
                 return flag ? 1 : 2;
             }
         }"
        ),
        4.0
    );
}

#[test]
fn a_line_comment_ends_at_every_csharp_line_terminator() {
    // REGRESSION. ECMA-334 §6.3.1 lists five line terminators — CR, LF, NEL (U+0085),
    // LS (U+2028), PS (U+2029) — but the comment rules excluded only CR and LF, so a
    // comment ended by one of the other three swallowed the rest of the file. The type
    // grammar's optional braces then allowed recovery, so the analysis *completed* with
    // the swallowed members silently missing: a clean parse over half a file.
    for terminator in ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'] {
        let source = format!("class C {{ // note{terminator}    void M() {{ }} }}");
        let a = analyze_clean(&source);
        let nom = mehen_report::metrics_json::nom(&a.root.metrics);
        assert_eq!(
            nom.functions, 1.0,
            "U+{:04X} must end the comment so `M` survives",
            terminator as u32
        );
    }
}
