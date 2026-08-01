// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC-family tests for the ANTLR C# walker.
//!
//! - `sloc`: every physical line in the span.
//! - `ploc`: lines carrying a code token.
//! - `lloc`: statement- and declaration-shaped rules.
//! - `cloc`: comment lines, recovered from the hidden channel (comments never
//!   appear in the parse tree).
//! - `blank`: `sloc - ploc - comment-only`.

mod common;

use common::analyze_clean;
use mehen_report::metrics_json;

fn loc(source: &str) -> metrics_json::Loc {
    metrics_json::loc(&analyze_clean(source).root.metrics)
}

#[test]
fn counts_physical_code_and_blank_lines() {
    let a = loc("class C
         {
             void F()
             {
                 int x = 1;

                 x = 2;
             }
         }");
    assert_eq!(a.sloc, 9.0);
    assert_eq!(a.ploc, 8.0);
    assert_eq!(a.blank, 1.0);
}

#[test]
fn line_comments_are_cloc_not_ploc() {
    // Comments are hidden-channel: they reach LOC only through the
    // post-walk comment pass.
    let a = loc("// leading
         class C
         {
             // inside
             void F() { }
         }");
    assert_eq!(a.cloc, 2.0);
}

#[test]
fn xml_doc_comments_count_as_comments() {
    // `///` is a distinct token type (`SINGLE_LINE_DOC_COMMENT`) from `//`, so
    // this pins that the analyzer routes all five C# comment token types.
    let a = loc("/// <summary>Doc.</summary>
         class C { }");
    assert_eq!(a.cloc, 1.0);
}

#[test]
fn block_and_delimited_doc_comments_count_every_row() {
    let a = loc("/** doc
          * continues
          */
         class C { }");
    assert_eq!(a.cloc, 3.0);
}

#[test]
fn a_trailing_comment_shares_its_line_with_code() {
    // The line is both code and comment: it counts in ploc AND cloc, and is
    // NOT blank.
    let a = loc("class C { void F() { } } // trailing");
    assert_eq!(a.sloc, 1.0);
    assert_eq!(a.ploc, 1.0);
    assert_eq!(a.cloc, 1.0);
    assert_eq!(a.blank, 0.0);
}

#[test]
fn lloc_counts_statements_and_declarations() {
    // class(1) + method(1) + 3 statements = 5
    let a = loc("class C
         {
             void F()
             {
                 int x = 1;
                 x = 2;
                 return;
             }
         }");
    assert_eq!(a.lloc, 5.0);
}

#[test]
fn a_bare_block_is_not_its_own_logical_line() {
    // The `{ }` wrapper adds nothing; only the inner statement counts:
    // class(1) + method(1) + inner statement(1) = 3.
    let a = loc("class C
         {
             void F()
             {
                 { int x = 1; }
             }
         }");
    assert_eq!(a.lloc, 3.0);
}

#[test]
fn an_empty_statement_is_not_a_logical_line() {
    // class(1) + method(1) = 2; the bare `;` adds nothing.
    let a = loc("class C
         {
             void F() { ; }
         }");
    assert_eq!(a.lloc, 2.0);
}

#[test]
fn a_for_header_declaration_is_one_logical_line() {
    // The initializer declaration is part of the `for` statement's single
    // header line, not a second logical line:
    // class(1) + method(1) + for(1) + body statement(1) = 4.
    let a = loc("class C
         {
             void F()
             {
                 for (int i = 0; i < 3; i++) { int x = i; }
             }
         }");
    assert_eq!(a.lloc, 4.0);
}

#[test]
fn an_expression_bodied_method_is_one_logical_line() {
    // `int F() => 1;` is a single declaration — the `=>` body must NOT add a
    // second logical line on top of the declaration itself.
    // class(1) + method(1) = 2.
    let a = loc("class C
         {
             int F() => 1;
         }");
    assert_eq!(a.lloc, 2.0);
}

#[test]
fn an_expression_bodied_accessor_counts_its_body() {
    // An accessor opens its own space; `get => _x;` has no statement in it, so
    // the body counts as that space's one logical line.
    // class(1) + field(1) + property(1) + accessor body(1) = 4.
    let a = loc("class C
         {
             private int _x;
             public int X { get => _x; }
         }");
    assert_eq!(a.lloc, 4.0);
}

#[test]
fn an_expression_bodied_lambda_counts_one_line() {
    // A lambda opens a closure space; `x => x + 1` has no statement, so the
    // lambda itself is that space's one logical line.
    // class(1) + method(1) + local declaration(1) + lambda(1) = 4.
    let a = loc("class C
         {
             void F()
             {
                 System.Func<int, int> f = x => x + 1;
             }
         }");
    assert_eq!(a.lloc, 4.0);
}

#[test]
fn usings_and_namespace_are_logical_lines() {
    // using(1) + namespace(1) + class(1) = 3
    let a = loc("using System;
         namespace N
         {
             class C { }
         }");
    assert_eq!(a.lloc, 3.0);
}

#[test]
fn a_verbatim_string_spanning_lines_marks_every_row_as_code() {
    // A multi-line verbatim string is ONE token covering several rows; every
    // row must be code, or the interior rows are reported as phantom blanks.
    let a = loc("class C
         {
             string S = @\"one
         two
         three\";
         }");
    assert_eq!(a.sloc, 6.0);
    assert_eq!(a.blank, 0.0, "interior string rows must not read as blank");
}

#[test]
fn preprocessor_directives_are_not_comments_and_not_logical_lines() {
    // mehen does **not** evaluate `#if`: directives are routed to their own
    // channel, so they are neither code nor comment for LOC, and an inactive
    // region is still parsed as ordinary code.
    //
    // That is a deliberate trade. Evaluating `#if` means choosing a symbol set,
    // and metrics for one configuration's subset of the file is less useful than
    // approximate metrics for all of it — a member excluded in *this* build is
    // still code someone maintains. (The previous grammars-v4 lexer evaluated
    // directives via a stateful hook and handed the inactive branch over as a
    // single hidden `SKIPPED_SECTION` token; this grammar has no such hook.)
    let a = loc("class C
         {
         #if NEVER
             void Excluded() { }
         #endif
             void Kept() { }
         }");
    assert_eq!(a.cloc, 0.0, "a directive is not a comment");
    // class(1) + Excluded(1) + Kept(1) = 3 — the `#if`/`#endif` rows themselves
    // are not logical lines.
    assert_eq!(
        a.lloc, 3.0,
        "an inactive region is parsed as code, so its member counts"
    );
    // A directive row IS a physical code line, though. It carries source text, so
    // it must not fall through to `blank = sloc - ploc - only_comment`.
    assert_eq!(a.sloc, 7.0);
    assert_eq!(
        a.ploc, 7.0,
        "every row carries a token, directives included"
    );
    assert_eq!(a.blank, 0.0, "a directive row is not blank");
}

#[test]
fn a_directive_row_is_code_not_blank() {
    // REGRESSION. PLOC is recorded during the tree walk, which cannot see a
    // directive — it goes to its own channel, so it never reaches the parser as a
    // terminal. The row therefore carried no PLOC observation and was reported as a
    // *blank line*, which it plainly is not. Directives are now routed through the
    // same post-walk pass that handles comments.
    let a = loc("class C
         {
         #define FOO
             void M() { }
         }");
    assert_eq!(a.sloc, 5.0);
    assert_eq!(a.ploc, 5.0);
    assert_eq!(a.blank, 0.0);
    assert_eq!(a.cloc, 0.0);
}

#[test]
fn a_real_blank_line_is_still_blank() {
    // The counterpart: routing directives into PLOC must not make every row code.
    let a = loc("class C
         {

             void M() { }
         }");
    assert_eq!(a.sloc, 5.0);
    assert_eq!(a.ploc, 4.0);
    assert_eq!(a.blank, 1.0);
}
