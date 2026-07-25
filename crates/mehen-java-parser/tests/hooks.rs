// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Behavioral tests for the `JavaParserBase` hook port (`src/hooks.rs`).
//!
//! Each test pins one observable consequence of a predicate so a regression
//! in the port (or a regenerate that stops routing the predicate to hooks)
//! fails here rather than silently skewing downstream metrics.

use antlr4_runtime::{CommonTokenStream, InputStream, Parser};
use mehen_java_parser::hooks::JavaParserBase;
use mehen_java_parser::java_lexer::JavaLexer;
use mehen_java_parser::java_parser::JavaParser;

/// Parse a compilation unit with the hooks installed, returning the
/// recovered syntax-error count.
fn syntax_errors(source: &str) -> usize {
    let lexer = JavaLexer::new(InputStream::new(source));
    let tokens = CommonTokenStream::new(lexer);
    let mut parser = JavaParser::with_typed_hooks(tokens, JavaParserBase);
    parser.remove_error_listeners();
    let _ = parser
        .compilation_unit()
        .expect("entry rule must not hard-fail");
    parser.number_of_syntax_errors()
}

#[test]
fn named_annotation_arguments_parse_cleanly() {
    // `IsNotIdentifierAssign` steers `key = value` pairs to the explicit
    // `identifier '=' annotationValue` alternative.
    let src = "@interface Foo { int bar() default 0; }\n@Foo(bar = 1)\nclass C {}\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn contextual_keyword_annotation_argument_parses_cleanly() {
    // The identifier-like set includes contextual keywords (`module`, `yield`,
    // `record`, …): `@Foo(record = 1)` must take the named-argument alternative
    // exactly like a plain identifier.
    let src = "@interface Foo { int record() default 0; }\n@Foo(record = 1)\nclass C {}\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn positional_annotation_argument_parses_cleanly() {
    // Lookahead that is not `<identifier-like> =` keeps the predicate true, so
    // a single positional value still parses via `annotationValue`.
    let src = "@interface Foo { int value() default 0; }\n@Foo(41 + 1)\nclass C {}\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn trailing_varargs_record_component_parses_cleanly() {
    // `DoLastRecordComponent` accepts `...` on the LAST component.
    let src = "record R(int x, int... ys) {}\n";
    assert_eq!(syntax_errors(src), 0);
}

#[test]
fn non_trailing_varargs_record_component_is_rejected() {
    // …and rejects `...` on a non-last component, as `javac` does. Without
    // the hook (old assume-true builds) this parsed cleanly.
    let src = "record R(int... xs, int y) {}\n";
    assert!(
        syntax_errors(src) > 0,
        "varargs before the last record component must be a syntax error"
    );
}

#[test]
fn hookless_parser_fails_loud_on_hooked_predicate() {
    // The generated modules carry `--sem-unknown error`: a parser built
    // without hooks must surface `AntlrError::Unsupported` when an input
    // reaches a hooked predicate — never silently assume it true.
    let lexer = JavaLexer::new(InputStream::new("@Foo(bar = 1) class C {}\n"));
    let tokens = CommonTokenStream::new(lexer);
    let mut parser = JavaParser::new(tokens);
    parser.remove_error_listeners();
    assert!(
        parser.compilation_unit().is_err(),
        "hook-less parse must fail loud, not mis-parse"
    );
}
