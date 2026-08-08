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
use mehen_csharp_parser::c_sharp_parser::{self as c_sharp_parser, CSharpParser};

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
fn every_split_shift_operator_still_parses() {
    // The other side of the same predicate: adjacent `>` `>` in expression position
    // is a shift. All four spellings are split by the prep and rejoined behind
    // `token_index_adjacent`, so each junction's predicate is exercised — `>>>=`
    // carries two.
    for expression in ["a >> b", "a >>> b", "a >>= b", "a >>>= b"] {
        assert_eq!(
            syntax_errors(&format!(
                "class C {{ int M(int a, int b) {{ return {expression}; }} }}"
            )),
            0,
            "`{expression}` must parse"
        );
    }
}

#[test]
fn record_is_a_contextual_keyword() {
    // Roslyn declares the keyword as `<ContextualKind>`, which its grammar generator
    // drops; the prep restores it by minting a real `KW_RECORD` token and widening
    // `identifier_token` with it, so `record` stays usable as an ordinary name.
    assert_eq!(syntax_errors("record R(int X);"), 0);
    assert_eq!(syntax_errors("class C { void M() { int record = 1; } }"), 0);
    // An error count is NOT enough here: `record R(int X);` parsed with zero errors
    // for a long time while producing a *method* named `R`. The tree shape is pinned
    // in `mehen-csharp/tests/structure.rs`, which is where the space kinds are
    // visible; this file can only assert parseability.
}

#[test]
fn a_property_with_both_accessors_is_not_a_record() {
    // The counterpart to the record fix: `record_declaration` sits ahead of
    // `base_method_declaration` among `member_declaration`'s alternatives, so the
    // record path must not be viable for an ordinary property. It is not, because
    // `record_keyword` is a real token — with the earlier predicate form this shape
    // was a hard error on 29 corpus files.
    assert_eq!(
        syntax_errors("struct S { public T P { readonly get => 1; set { } } }"),
        0
    );
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

/// Whether any node in the tree is a `declaration_expression`.
///
/// `F(x)` fits both `declaration_expression : type variable_designation` and
/// `invocation_expression : expression argument_list`, and the published grammar
/// lists the declaration form first — so every method call parsed as a
/// declaration, with zero reported errors. Error counts cannot catch that, hence
/// a shape assertion.
fn has_declaration_expression(source: &str) -> bool {
    use antlr4_runtime::Node;
    let lexer = CSharpLexer::new(InputStream::new(source));
    let mut parser = CSharpParser::new(CommonTokenStream::new(lexer));
    parser.remove_error_listeners();
    let tree = parser
        .compilation_unit()
        .expect("entry rule must not hard-fail");
    let parsed = parser.into_parsed_file(tree);
    fn walk(node: Node<'_>) -> bool {
        if node
            .as_rule()
            .is_some_and(|rule| rule.rule_index() == c_sharp_parser::RULE_DECLARATION_EXPRESSION)
        {
            return true;
        }
        node.children().any(walk)
    }
    walk(parsed.tree())
}

#[test]
fn invocations_are_not_declaration_expressions() {
    for source in [
        "class C { void M() { F(x); } }",
        "class C { void M() { a.B(); } }",
        "class C { void M() { a.B(x).C(y); } }",
        "class C { int M() { return F(x); } }",
    ] {
        assert!(
            !has_declaration_expression(source),
            "method call mis-parsed as a declaration expression: {source}"
        );
    }
}

#[test]
fn out_declarations_are_still_declaration_expressions() {
    // The other side of the same reorder: where nothing else fits, the
    // declaration form must still win.
    assert!(has_declaration_expression(
        "class C { void M() { F(out int x); } }"
    ));
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

#[test]
fn the_entry_rule_consumes_the_whole_file() {
    // REGRESSION. Roslyn's `compilation_unit` does not end in `EOF`, so the parser
    // stopped at the first token it could not continue with and reported success on
    // whatever it had consumed: `class C { } } } }` parsed with ZERO diagnostics and
    // the stray braces were never looked at. For a metrics tool "parsed cleanly" has
    // to mean the whole file was accounted for, so the prep anchors the entry rule.
    assert!(
        syntax_errors("class C { }\n} } }\n") > 0,
        "an unconsumed tail must be a syntax error"
    );
    assert!(
        syntax_errors("class C { }\nelse { }\n") > 0,
        "a dangling `else` is not a legal top-level member"
    );
    // The tail has to be *lexable* and *not a legal member*. `syntax_errors` reports
    // the PARSER's count, so unrecognizable characters (`@@@`) are dropped by the
    // lexer before the parser sees them; and C# 9 top-level statements mean
    // `return 1;` IS a legal `global_statement`, so it is no test of the anchor.
}

#[test]
fn anchoring_does_not_reject_valid_files() {
    // The counterpart: every legal top-level shape must still reach EOF cleanly,
    // including the two that are not a plain sequence of type declarations.
    for source in [
        "class C { void M() { } }\n",
        "namespace N;\nclass C { }\n",
        "using System;\nnamespace N { class C { } }\n",
        "var x = 1;\n",
    ] {
        assert_eq!(syntax_errors(source), 0, "must parse: {source:?}");
    }
}

#[test]
fn an_incomplete_member_is_a_syntax_error() {
    // REGRESSION. `incomplete_member : attribute_list* modifier* type` is Roslyn's
    // error-*recovery* node: it exists so the compiler can build a tree for source
    // being typed, where `public int` is a member the author has not finished. Roslyn
    // emits a diagnostic beside it; the published grammar carries only the node. So a
    // syntax-only parser accepted `class C { int }` as a complete, error-free unit —
    // which contradicts mehen's contract, where a clean parse is what tells
    // `mehen metrics` to exit 0. The prep drops the alternative.
    assert!(syntax_errors("class C { int }\n") > 0);
    assert!(syntax_errors("class C { public int }\n") > 0);
}

#[test]
fn dropping_incomplete_member_keeps_every_real_member_form() {
    // Nothing legal may be lost: each real member form has its own rule, and the
    // dropped alternative matched only a type with no declarator after it.
    for source in [
        "class C { int x; }\n",
        "abstract class C { public abstract void M(); }\n",
        "interface I { void M(); }\n",
        "class C { public int P { get; set; } }\n",
        "class C { public int P { get; set; } = 5; }\n",
        "class C { [System.Obsolete] public static readonly int X = 1; }\n",
        "unsafe struct S { public fixed int data[4]; }\n",
        "class C { public event System.EventHandler E; }\n",
    ] {
        assert_eq!(syntax_errors(source), 0, "must parse: {source:?}");
    }
}

#[test]
fn a_switch_statement_requires_its_parentheses() {
    // REGRESSION. Roslyn writes `switch_statement`'s parens as independently optional
    // (`'switch' '('? expression ')'? '{' … '}'`), so `switch value { … }` — which is
    // not valid C# — parsed without recovery and was reported as a clean analysis. The
    // paren-free spelling belongs to the switch *expression*, a separate rule.
    assert!(syntax_errors("class C { void M(int v) { switch v { default: break; } } }\n") > 0);
}

#[test]
fn requiring_switch_parens_keeps_both_valid_forms() {
    // The statement with parens, and the paren-free switch *expression*.
    assert_eq!(
        syntax_errors("class C { void M(int v) { switch (v) { default: break; } } }\n"),
        0
    );
    assert_eq!(
        syntax_errors("class C { int M(int v) => v switch { _ => 0 }; }\n"),
        0
    );
}

/// Which of `statement`'s two overlapping alternatives claimed the method's
/// first statement: `Some(true)` for `local_declaration_statement`,
/// `Some(false)` for `expression_statement`, `None` for neither.
fn first_statement_is_declaration(source: &str) -> Option<bool> {
    use antlr4_runtime::Node;
    let lexer = CSharpLexer::new(InputStream::new(source));
    let mut parser = CSharpParser::new(CommonTokenStream::new(lexer));
    parser.remove_error_listeners();
    let tree = parser
        .compilation_unit()
        .expect("entry rule must not hard-fail");
    assert_eq!(parser.number_of_syntax_errors(), 0, "must parse: {source}");
    let parsed = parser.into_parsed_file(tree);
    fn walk(node: Node<'_>) -> Option<bool> {
        if let Some(rule) = node.as_rule() {
            match rule.rule_index() {
                c_sharp_parser::RULE_LOCAL_DECLARATION_STATEMENT => return Some(true),
                c_sharp_parser::RULE_EXPRESSION_STATEMENT => return Some(false),
                _ => {}
            }
        }
        node.children().find_map(walk)
    }
    walk(parsed.tree())
}

#[test]
fn a_generic_local_declaration_is_not_an_expression_statement() {
    // REGRESSION (#218). `statement`'s alternatives are alphabetical upstream, so
    // `expression_statement` preceded `local_declaration_statement` — and
    // `List<int> l;` is viable as the chained comparison `(List < int) > l`, so
    // ANTLR took the expression path with ZERO errors. Same ordering hazard as
    // `declaration_expression` and the `member_declaration` hoists; the prep now
    // hoists the declaration alternative. An error count cannot catch this, hence
    // the shape assertion. The metric consequences are pinned in
    // `mehen-csharp/tests/{abc,loc}.rs`.
    for source in [
        "class C { void M() { List<int> l; } }",
        "class C { void M() { List<int> l = new(); } }",
        "class C { void M() { System.Span<int> s = stackalloc int[4]; } }",
        "class C { void M() { Dictionary<string, List<int>> map = new(); } }",
        "class C { void M() { string? s = null; } }",
    ] {
        assert_eq!(
            first_statement_is_declaration(source),
            Some(true),
            "must be a local_declaration_statement: {source}"
        );
    }
}

#[test]
fn statement_expressions_still_win_where_no_declaration_fits() {
    // The other side of the hoist: every legal statement-expression shape
    // (invocation, creation, assignment, increment/decrement, qualified await)
    // is not viable as `type declarator ;`, so the declaration path must die in
    // prediction and leave each an expression statement. The indexed await is
    // here because locals get a bracket-less declarator: without that split,
    // `tasks[i]` would match the fixed-size-buffer declarator shape and the
    // statement would read as a declaration of `tasks[i]` with type `await`.
    for source in [
        "class C { void M() { F(x); } }",
        "class C { void M(int i) { i = 1; } }",
        "class C { void M(int i) { i++; } }",
        "class C { void M() { new C(); } }",
        "class C { void M() { obj.Method<int>(1); } }",
        "class C { async System.Threading.Tasks.Task M() { await x.RunAsync(); } }",
        "class C { async System.Threading.Tasks.Task M() { await tasks[i]; } }",
    ] {
        assert_eq!(
            first_statement_is_declaration(source),
            Some(false),
            "must be an expression_statement: {source}"
        );
    }
}

#[test]
fn the_hoists_residual_trade_is_a_bare_await_operand() {
    // The DELIBERATE residual of the hoist, pinned so it stays a decision rather
    // than drifting: `await t;` with a bare identifier operand is genuinely
    // ambiguous — outside an async method it is a valid declaration of a local
    // `t` whose type is named `await`, and Roslyn picks the await expression only
    // by asking whether the enclosing method is async, which a syntax-only
    // grammar cannot. The hoist resolves it to the declaration, exactly as it
    // would for any `T t;`. A declarator cannot be qualified, called, or indexed
    // (locals get the bracket-less declarator), so every other await operand
    // shape stays an expression — asserted above.
    assert_eq!(
        first_statement_is_declaration(
            "class C { async System.Threading.Tasks.Task M() { await t; } }"
        ),
        Some(true),
        "the documented trade: a bare await operand reads as a declaration"
    );
}

#[test]
fn a_local_declarator_has_no_fixed_size_buffer_form() {
    // The split that keeps `await tasks[i];` an expression must not reach field
    // position: `fixed int data[4];` is the fixed-size-buffer declarator, legal
    // only as a struct field, and it keeps the bracketed `variable_declarator`.
    assert_eq!(
        syntax_errors("unsafe struct S { public fixed int data[4]; }"),
        0
    );
    // In statement position the bracketed form is not valid C# (CS0650), and the
    // declaration path can no longer take it. It still parses silently — the
    // permissive expression hub reads `int buf` as a declaration_expression and
    // `[4]` as an element access, and tightening that is not worth carving up
    // the inlined expression cycle for input no compiler accepts — but the
    // assertion pins the mechanism: statement-position declarators are
    // bracket-less.
    assert_eq!(
        first_statement_is_declaration("class C { void M() { int buf[4]; } }"),
        Some(false),
        "a bracketed declarator must not reach the local declaration path"
    );
}
