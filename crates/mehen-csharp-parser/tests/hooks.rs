// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Behavioral tests for the derived grammar's semantic surfaces — the
//! interpolated-string state in `@lexer::members` and the pattern-lowered
//! parser predicates.
//!
//! Each test pins one observable consequence so a regression in the grammar — or
//! a regenerate that stops routing a helper — fails here rather than silently
//! skewing downstream metrics. The interpolation cases are the interesting ones:
//! the `}` closing a hole is lexically identical to the one closing a nested
//! block, so telling them apart needs a brace depth per open hole and a
//! *conditional* mode pop. SemIR has no conditional action, so the grammar
//! encodes it as two predicate-gated rules over the same character, ordered so
//! rule selection supplies the conjunction (see `grammar/lexer-tokens.g4.in`).

use antlr4_runtime::{CommonTokenStream, InputStream, Parser};
use mehen_csharp_parser::c_sharp_lexer::CSharpLexer;
use mehen_csharp_parser::c_sharp_parser::CSharpParser;

/// Parse a compilation unit, returning the recovered syntax-error count.
///
/// Plain `CSharpLexer::new`: the lexer needs no hooks at all. Its state lives in
/// `@lexer::members` and every action and predicate lowers to pure SemIR through
/// the derived `patterns.toml`, so there is no hand-written Rust to install.
fn syntax_errors(source: &str) -> usize {
    let lexer = CSharpLexer::new(InputStream::new(source));
    let tokens = CommonTokenStream::new(lexer);
    let mut parser = CSharpParser::new(tokens);
    parser.remove_error_listeners();
    let _ = parser
        .compilation_unit()
        .expect("entry rule must not hard-fail");
    parser.number_of_syntax_errors()
}

#[test]
fn plain_class_parses_cleanly() {
    assert_eq!(syntax_errors("class C { void M() { } }"), 0);
}

#[test]
fn interpolated_string_parses_cleanly() {
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $"a{X}b"; } }"#),
        0
    );
}

#[test]
fn interpolation_hole_tracks_nested_braces() {
    // The `}` of the collection initializer must NOT end the hole; only the
    // outer one does. This is the case a single unconditional mode command
    // cannot express, and the reason the rules are split and ordered.
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $"a{ new[]{1,2}.Length }b"; } }"#),
        0
    );
}

#[test]
fn escaped_braces_are_literal_text() {
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $"{{literal}}"; } }"#),
        0
    );
}

#[test]
fn interpolation_format_clause_is_not_code() {
    // `D4` after the `:` is format text, not an identifier, so it needs its own
    // lexer mode — entered by the `:` rule gated on brace depth 0.
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $"{v:D4}"; } }"#),
        0
    );
}

#[test]
fn nested_interpolated_strings_parse_cleanly() {
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $"outer {$"inner {x}"} end"; } }"#),
        0
    );
}

#[test]
fn verbatim_interpolated_string_keeps_backslash_literal() {
    // In `$@"…"` a backslash is an ordinary character, so it must not start an
    // escape. Needs a text mode distinct from the regular-string one.
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $@"a{X}\b"; } }"#),
        0
    );
}

#[test]
fn verbatim_interpolated_string_doubles_quotes() {
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $@"a""b{X}"; } }"#),
        0
    );
}

#[test]
fn nested_generics_close_with_adjacent_angle_brackets() {
    // `>>` is emitted as two `>` tokens and rejoined in the parser behind
    // `token_index_adjacent`, so a generic closer never lexes as a shift.
    assert_eq!(syntax_errors("class C { List<List<int>> F; }"), 0);
}

#[test]
fn right_shift_still_parses_as_an_operator() {
    // The other side of the same predicate: adjacent `>` `>` in expression
    // position is a shift.
    assert_eq!(syntax_errors("class C { int M(int a, int b) => a >> b; }"), 0);
}

#[test]
fn record_is_a_contextual_keyword() {
    // Roslyn declares the keyword as `<ContextualKind>`, which its grammar
    // generator drops; the prep restores it as a text predicate. It must remain
    // usable as an ordinary name.
    assert_eq!(syntax_errors("record R(int X);"), 0);
    assert_eq!(syntax_errors("class C { void M() { int record = 1; } }"), 0);
}

#[test]
fn var_is_a_contextual_keyword() {
    // The widened `identifier_token` must accept `var` as a name while
    // `var_pattern` still recognizes it positionally.
    assert_eq!(syntax_errors("class C { void M() { var x = 1; } }"), 0);
    assert_eq!(syntax_errors("class C { void M() { int var = 1; } }"), 0);
}

#[test]
fn discard_designation_parses() {
    // `out _` was the one gap whose error recovery grew the runtime's
    // diagnostic arena without bound; pinned so it cannot regress silently.
    assert_eq!(
        syntax_errors("class C { bool M(string p, out int n) => G(p, out n, out _); }"),
        0
    );
}

#[test]
fn deeply_nested_holes_restore_the_enclosing_depth() {
    // `holeStack` saves the enclosing hole's brace depth so an inner hole cannot
    // clobber it. Without that, the outer `}` would be misread once the inner
    // string had incremented the shared counter.
    assert_eq!(
        syntax_errors(r#"class C { void M() { var s = $"a{ $"b{ new[]{1}.Length }c" }d"; } }"#),
        0
    );
}
