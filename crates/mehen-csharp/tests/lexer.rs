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

#[test]
fn a_unicode_escape_is_a_legal_identifier_character() {
    // REGRESSION. `int a = 1;` declares `a` — Roslyn lists
    // `unicode_escape_sequence` in both `identifier_start_character` and
    // `identifier_part_character`, so tokenizing those rules has to carry it over. The
    // backslash could not be consumed and the declaration reported two lexer errors.
    assert_eq!(
        lloc("class C { static void M() { int \\u0061 = 1; } }"),
        3.0
    );
}

#[test]
fn a_directive_ends_at_every_csharp_line_terminator() {
    // REGRESSION, and the same oversight as the comment rules one commit earlier:
    // `DIRECTIVE_LINE` still stopped only at CR/LF, so a directive ended by NEL /
    // U+2028 / U+2029 consumed the separator *and everything after it* onto the
    // directive channel — a whole file reported as zero declarations with zero
    // diagnostics.
    for terminator in ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'] {
        let source = format!("#if X{terminator}class C {{ void M() {{ }} }}\n#endif\n");
        let a = analyze_clean(&source);
        let nom = mehen_report::metrics_json::nom(&a.root.metrics);
        assert_eq!(
            nom.functions, 1.0,
            "U+{:04X} must end the directive so the class survives",
            terminator as u32
        );
    }
}

#[test]
fn every_enumerated_raw_string_fence_width_parses() {
    // The fence-length rule ("close on a run at least as long as the opening one") needs
    // state to express in general — a member holding the opening width and a predicate
    // comparing each candidate closer. Every alternative here is stateless, so the widths
    // are enumerated three through eight, each embedding one fewer quote than its fence.
    //
    // Six through eight were added after a review found that five was the ceiling: a
    // six-quote fence (which exists to embed `"""""`) reported two diagnostics on valid
    // C#. Past eight the three-quote arm matches and terminates early, costing the
    // literal's tail — an acceptable floor, since the deepest fence in the 322-file
    // corpus is three.
    for width in 3..=8 {
        let fence = "\"".repeat(width);
        let inner = "\"".repeat(width - 1);
        let source = format!("class C {{ static string M() => {fence}a{inner}b{fence}; }}");
        assert_eq!(
            lloc(&source),
            2.0,
            "a {width}-quote fence must embed a {}-quote run",
            width - 1
        );
    }
}

#[test]
fn a_longer_raw_string_fence_can_embed_three_quotes() {
    // REGRESSION. `""""a"""b""""` is valid C# — a four-quote fence exists precisely so
    // the content can contain `"""` — but a single non-greedy rule stopped at the
    // *first* three-quote run, terminating the literal early and leaving `b""""` as
    // stray tokens. The fence-length rule ("close on a run at least as long as the
    // opening one") needs one alternative per length, longest first.
    assert_eq!(
        lloc("class C { static string M() => \"\"\"\"a\"\"\"b\"\"\"\"; }"),
        2.0
    );
    // And one length further, for a literal embedding four quotes.
    assert_eq!(
        lloc("class C { static string M() => \"\"\"\"\"a\"\"\"\"b\"\"\"\"\"; }"),
        2.0
    );
}

#[test]
fn a_longer_fence_works_multi_line_too() {
    // The multi-line form needs the same per-length alternatives, and the single-line
    // rule must still win the tie for a one-liner (ANTLR breaks equal-length matches by
    // order, and the parser reaches the two through different rules).
    assert_eq!(
        lloc(
            "class C
             {
                 static string M() => \"\"\"\"
             a\"\"\"b
             \"\"\"\";
             }"
        ),
        2.0
    );
}

#[test]
fn u8_is_a_legal_identifier() {
    // REGRESSION. `u8`/`U8` are *contextual* — a suffix only directly after a string
    // literal — so `class C { int u8; }` is valid C# and reported four diagnostics.
    // They had been withheld from the `identifier_token` widening, which is the same
    // mistake the comment beside that set already warned about for `_`.
    assert_eq!(lloc("class C { int u8; }"), 2.0);
    assert_eq!(lloc("class C { int U8; }"), 2.0);
}

#[test]
fn the_utf8_suffix_still_works_after_a_literal() {
    // The counterpart: widening must not break the suffix, which is positional —
    // `utf8_string_literal_token : string_literal_token (KW_U8 | KW_U8_LOWER)` requires
    // the preceding literal, so a bare `u8` cannot be mistaken for it.
    assert_eq!(
        lloc("class C { static System.ReadOnlySpan<byte> M() => \"x\"u8; }"),
        2.0
    );
}

#[test]
fn a_two_dollar_raw_string_opens_its_hole_on_a_doubled_brace() {
    // REGRESSION. The dollar count sets the brace width: with `$$"""…"""` a hole opens on
    // `{{` and a lone `{` is literal text (C# 11 chose this so brace-heavy text like JSON
    // needs no escaping). The lexer had ONE raw mode, whose `{{` rule read a doubled brace
    // as escaped text — so `$$"""{{a && b}}"""` swallowed the hole's expression whole. Zero
    // diagnostics, and `a && b`'s operators and complexity vanished.
    //
    // `dotnet/runtime` uses this shape for embedded JSON (`$$"""{"k":{{v}}}"""`), which is
    // exactly why it matters, and why it was missed: those files are under `tests/`, not in
    // the `src/` corpus.
    //
    // Asserted through cognitive complexity, which is 0 if the hole is text and 1 if the
    // `&&` really reached the parser — pinned against both narrower spellings.
    let cognitive = |source: &str| {
        mehen_report::metrics_json::cognitive(&analyze_clean(source).root.metrics).sum
    };
    let two_dollar =
        cognitive("class C { static string F(bool a, bool b) => $$\"\"\"{{a && b}}\"\"\"; }");
    let one_dollar =
        cognitive("class C { static string F(bool a, bool b) => $\"\"\"{a && b}\"\"\"; }");
    let plain = cognitive("class C { static string F(bool a, bool b) => $\"{a && b}\"; }");
    assert_eq!(two_dollar, one_dollar, "the hole must reach the parser");
    assert_eq!(two_dollar, plain);
    assert_eq!(two_dollar, 1.0);
}

#[test]
fn a_lone_brace_in_a_two_dollar_raw_string_is_text() {
    // The counterpart, and the reason the width-2 text rule is split into single-character
    // brace rules: a LONE brace at this width is literal text, so brace-heavy content must
    // survive intact. ANTLR takes the longest match and breaks only ties by order, so a
    // text rule able to consume `{{a}}` would beat the two-character hole rule no matter
    // which came first — that was the first attempt and it swallowed the hole again.
    //
    // Two statements after the literal: if a brace rule over-matched, the token would eat
    // them and LLOC would drop. class(1) + method(1) + 2 locals = 4.
    assert_eq!(
        lloc(
            "class C
             {
                 static void M()
                 {
                     var json = $$\"\"\"{\"k\": 1}\"\"\";
                     int x = 1;
                 }
             }"
        ),
        4.0
    );
}

#[test]
fn an_interpolated_raw_string_closes_on_its_own_fence_width() {
    // REGRESSION. A four-quote opening fence exists so that an embedded `"""` is content:
    // `$""""a"""b""""`. The interpolation modes closed on any three-or-more run, so the
    // string ended at the embedded triple and the tail became stray code — 8 diagnostics,
    // and the metrics around it wrong too (LLOC 3 against 2).
    //
    // Only the CLOSE rule is fence-width-sensitive, so each width carries its own mode.
    // Four is the documented floor: a wider fence is needed only when the *content* holds a
    // run of three or more quotes, and `dotnet/runtime` has no interpolated raw string with
    // even a four-quote fence.
    //
    // `analyze_clean` asserts no diagnostics, so reaching the assertion is the substance;
    // LLOC then pins that the token did not eat the statements after it.
    for source in [
        "class C { static string F() => $\"\"\"ab\"\"\"; }",
        "class C { static string F() => $\"\"\"\"a\"\"\"b\"\"\"\"; }",
        "class C { static string F(int v) => $$\"\"\"{{v}}\"\"\"; }",
        "class C { static string F(int v) => $$\"\"\"\"{{v}}\"\"\"a\"\"\"b\"\"\"\"; }",
    ] {
        // class(1) + method(1) = 2. More means the literal's tail leaked out as code.
        assert_eq!(lloc(source), 2.0, "fence width must be respected: {source}");
    }
}

#[test]
fn an_unknown_escape_sequence_is_an_error() {
    // REGRESSION. `Escape` ended in `| .`, so any character after a backslash was accepted
    // and `'\q'` — not valid C# — lexed as an ordinary character literal. The analyzer
    // reported a clean, complete analysis of invalid source, which is the wrong direction
    // for a tool whose contract is that a clean parse means something.
    //
    // `analyze` rather than `analyze_clean`: the point here is that diagnostics DO appear.
    for source in [
        "class C { static char F() => '\\q'; }",
        "class C { static string F() => \"a\\qb\"; }",
        "class C { static string F() => \"a\\eb\"; }",
    ] {
        assert!(
            !common::analyze(source).diagnostics.is_empty(),
            "an unknown escape must be reported: {source}"
        );
    }
}

#[test]
fn every_legal_escape_sequence_still_lexes() {
    // The guard on the enumeration: narrowing `Escape` must not reject anything ECMA-334
    // §6.4.5.6 allows. The simple set is `\' \" \\ \0 \a \b \f \n \r \t \v`, plus hex
    // (one to four digits) and the two unicode widths.
    for escape in [
        "\\'",
        "\\\"",
        "\\\\",
        "\\0",
        "\\a",
        "\\b",
        "\\f",
        "\\n",
        "\\r",
        "\\t",
        "\\v",
        "\\x4",
        "\\x41",
        "\\x0041",
        "\\u0041",
        "\\U00000041",
    ] {
        // Both literal kinds: a char literal has no closure to absorb a mis-sized escape,
        // so it is the stricter of the two.
        assert_eq!(
            lloc(&format!(
                "class C {{ static string F() => \"a{escape}b\"; }}"
            )),
            2.0,
            "`{escape}` must lex in a string"
        );
        assert_eq!(
            lloc(&format!("class C {{ static char F() => '{escape}'; }}")),
            2.0,
            "`{escape}` must lex in a char literal"
        );
    }
}

#[test]
fn an_interpolated_string_validates_its_escapes_too() {
    // REGRESSION beyond the ordinary-literal escape fix: the interpolation mode had its own
    // `'\\' .` rule, so `$"\q"` still lexed clean while `"\q"` was rejected — the two
    // spellings of the same invalid source disagreed. It now reuses the enumerated `Escape`
    // fragment.
    for source in [
        "class C { static string F() => $\"\\q\"; }",
        "class C { static string F() => $\"a\\eb\"; }",
    ] {
        assert!(
            !common::analyze(source).diagnostics.is_empty(),
            "an unknown escape in an interpolated string must be reported: {source}"
        );
    }
    // And a legal one still lexes, in the interpolated spelling as well.
    assert_eq!(lloc("class C { static string F() => $\"a\\nb\"; }"), 2.0);
}

#[test]
fn a_hex_escape_takes_at_most_four_digits() {
    // REGRESSION. `'x' [0-9a-fA-F]+` consumed an unbounded run, so `'\x12345'` — five
    // digits, not valid C# — lexed as one clean character literal. ECMA-334 allows one to
    // four.
    assert!(
        !common::analyze("class C { static char F() => '\\x12345'; }")
            .diagnostics
            .is_empty(),
        "five hex digits must be reported"
    );
    // All four legal widths still lex.
    for escape in ["\\x1", "\\x12", "\\x123", "\\x1234"] {
        assert_eq!(
            lloc(&format!("class C {{ static char F() => '{escape}'; }}")),
            2.0,
            "`{escape}` must lex"
        );
    }
}

#[test]
fn an_integer_suffix_takes_at_most_one_marker_of_each_kind() {
    // REGRESSION. Two independent `[uUlL]?` slots accepted combinations C# rejects — `1uu`,
    // `1LL`, `1uU` all lexed as ordinary integer literals, so invalid source reported a
    // clean parse. `IntSuffix` now enumerates the legal pairs: at most one unsigned marker
    // and one long marker, in either order.
    for bad in ["1uu", "1UU", "1ll", "1LL", "1uU", "1Ll"] {
        assert!(
            !common::analyze(&format!("class C {{ static object F() => {bad}; }}"))
                .diagnostics
                .is_empty(),
            "`{bad}` is not a legal suffix combination"
        );
    }
    // Every legal combination, across all three integer bases.
    for good in [
        "1u", "1U", "1l", "1L", "1ul", "1uL", "1Ul", "1UL", "1lu", "1lU", "1Lu", "1LU", "0x1u",
        "0x1UL", "0b1ul",
    ] {
        assert_eq!(
            lloc(&format!("class C {{ static object F() => {good}; }}")),
            2.0,
            "`{good}` must lex"
        );
    }
}

#[test]
fn a_width_two_hole_close_consumes_both_braces() {
    // REGRESSION. The hole close lives in the *default* mode, shared by every interpolation
    // flavour, and matched one `}`. A hole opened with `{{` therefore left its second brace
    // to be re-lexed in the width-two mode, which called it literal text — one phantom
    // Halstead operand that the equivalent one-dollar spelling does not have.
    //
    // A `wideStack` parallel to `holeStack` records each open hole's brace width, so the
    // doubled close is gated on it. That gate is load-bearing: a first attempt matched `}}`
    // whenever a hole was open, which broke `$"{v}}}"` — a width-one close followed by an
    // escaped brace — by taking both braces as the close.
    let vocab = |source: &str| {
        let a = analyze_clean(source);
        let h = mehen_report::metrics_json::halstead(&a.root.metrics);
        h.n1 + h.n2
    };
    assert_eq!(
        vocab("class C { static string F(int v) => $$\"\"\"{{v}}\"\"\"; }"),
        vocab("class C { static string F(int v) => $\"\"\"{v}\"\"\"; }"),
        "the two hole widths must cost the same"
    );

    // The cases the gate protects, all of which must stay clean:
    for source in [
        // width-one close plus an escaped brace
        "class C { static string F(int v) => $\"{v}}}\"; }",
        // a genuinely literal brace at width two
        "class C { static string F() => $$\"\"\"a}b\"\"\"; }",
        // nesting: the inner hole's width must not clobber the outer one's
        "class C { static string F(int v) => $$\"\"\"{{ $\"{v}\" }}\"\"\"; }",
        // and a format clause, whose close pops the same stacks
        "class C { static string F(int v) => $$\"\"\"{{v:D4}}\"\"\"; }",
    ] {
        assert_eq!(lloc(source), 2.0, "{source}");
    }
}
