// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Behavioral tests for the `CSharpLexerBase` hook port (`src/hooks.rs`) and
//! the pattern-lowered `CSharpParserBase` predicates.
//!
//! Each test pins one observable consequence of a helper so a regression in
//! the port (or a regenerate that stops routing a helper) fails here rather
//! than silently skewing downstream metrics.

use antlr4_runtime::{CommonTokenStream, InputStream, Parser};
use mehen_csharp_parser::c_sharp_lexer::CSharpLexer;
use mehen_csharp_parser::c_sharp_parser::CSharpParser;
use mehen_csharp_parser::hooks::CSharpLexerBase;

/// Parse a compilation unit with the lexer hooks installed, returning the
/// recovered syntax-error count.
fn syntax_errors(source: &str) -> usize {
    let lexer = CSharpLexer::with_typed_hooks(InputStream::new(source), CSharpLexerBase::default());
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
    assert_eq!(syntax_errors("class C { void M() {} }\n"), 0);
}

#[test]
fn interpolated_string_parses_cleanly() {
    // Exercises OnInterpolatedRegularStringStart/OnOpenBrace/OnCloseBrace:
    // the `{x + 1}` hole re-enters expression lexing, `:D` is a format
    // clause (OnColon), and lexing must pop back out at the closing quote.
    let src = "class C { string M(int x) { return $\"a{x + 1:D}b\"; } }\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn nested_interpolated_strings_parse_cleanly() {
    // A hole containing another interpolated string exercises the
    // level/verbatium stacks.
    let src = "class C { string M(int x) { return $\"a{$\"inner{x}\"}b\"; } }\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn verbatim_interpolated_string_parses_cleanly() {
    // `$@\"…\"` flips the verbatium flag (IsVerbatiumDoubleQuoteInside);
    // doubled quotes inside are escapes, and `\` is a literal backslash.
    let src = "class C { string M(int x) { return $@\"c:\\dir\\{x}\"\"q\"\"\"; } }\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn preprocessor_inactive_section_is_skipped() {
    // The `#if FOO` block is inactive (FOO undefined): its body — which is
    // NOT valid C# — must be consumed by the directive state machine
    // (skip_false_block) rather than tokenized.
    let src = "#if FOO\nthis is ] not [ C# at all ;;\n#endif\nclass C {}\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn preprocessor_define_activates_branch() {
    // `#define FOO` then `#if FOO` keeps the branch active, `#else` inactive.
    let src = "#define FOO\n#if FOO\nclass A {}\n#else\nnot C# ]][[\n#endif\nclass B {}\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn preprocessor_condition_operators_evaluate() {
    // `!`, `&&`, `==` in `#if` expressions flow through the Expression
    // evaluator in the hooks port.
    let src = "#define A\n#if A && !B\nclass Kept {}\n#elif A == false\nnot C# ((\n#endif\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn nested_generics_close_with_right_shift() {
    // `List<List<int>>` ends in `>>` lexed as two `>` tokens; the
    // IsRightShift/token-adjacency pattern must let the type close while
    // still treating spaced `> >` in expressions as comparisons.
    let src =
        "class C { System.Collections.Generic.List<System.Collections.Generic.List<int>> f; }\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn var_declaration_predicate_holds() {
    // IsLocalVariableDeclaration (ctx_rule_text != "var"): both a `var`
    // inferred local and an explicitly-typed local must parse cleanly.
    let src = "class C { void M() { var x = 1; int y = 2; } }\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn hookless_lexer_fails_loud_on_hooked_coordinate() {
    // The generated modules carry `--sem-unknown error`: a lexer built
    // without hooks must fail (not mis-lex) when input reaches a hooked
    // action — here an interpolated string.
    let lexer = CSharpLexer::new(InputStream::new(
        "class C { string M(int x) { return $\"a{x}\"; } }\n",
    ));
    let tokens = CommonTokenStream::new(lexer);
    let mut parser = CSharpParser::new(tokens);
    parser.remove_error_listeners();
    let hard_fail = parser.compilation_unit().is_err();
    let recovered = parser.number_of_syntax_errors() > 0;
    assert!(
        hard_fail || recovered,
        "hook-less lex of an interpolated string must fail loud, not mis-lex"
    );
}
