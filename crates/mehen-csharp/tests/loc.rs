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

#[test]
fn a_trailing_comment_on_a_directive_still_counts_as_cloc() {
    // REGRESSION. `DIRECTIVE_LINE` was `'#' ~[\r\n]*`, which swallowed the whole row —
    // so `#if DEBUG // explain why` recorded no CLOC at all. The negated set now
    // excludes `/` so the token stops before a comment.
    let a = loc("class C {
         #if DEBUG // explain why
             void M() { }
         #endif
         }");
    assert_eq!(a.cloc, 1.0);
}

#[test]
fn a_directive_without_a_comment_records_no_cloc() {
    let a = loc("class C {
         #if DEBUG
             void M() { }
         #endif
         }");
    assert_eq!(a.cloc, 0.0);
}

#[test]
fn a_slash_inside_a_directive_does_not_split_it() {
    // The second alternative requires the char after `/` not to start a comment, so a
    // path-like `#line` directive stays one token while `#pragma … // note` splits.
    let path = loc("class C {
         #line 1 \"a/b.cs\"
             void M() { }
         }");
    assert_eq!(path.cloc, 0.0, "a `/` in a path is not a comment");
    let pragma = loc("class C {
         #pragma warning disable CA1024 // note
             void M() { }
         }");
    assert_eq!(pragma.cloc, 1.0);
}

#[test]
fn a_label_is_not_its_own_logical_line() {
    // REGRESSION. `labeled_statement` is a wrapper: it recorded a logical line and the
    // nested `return_statement` recorded another, so adding a label turned one statement
    // into two even on the same source row. A label is an attribute of the statement it
    // labels — `mehen-java` omits the equivalent wrapper for the same reason.
    let labeled = loc("class C
         {
             static void M() { start: return; }
         }");
    let plain = loc("class C
         {
             static void M() { return; }
         }");
    assert_eq!(labeled.lloc, plain.lloc);
    // class(1) + method(1) + return(1) = 3.
    assert_eq!(labeled.lloc, 3.0);
}

#[test]
fn a_block_comment_counts_every_row_whatever_the_terminator() {
    // REGRESSION from this PR's own terminator work: the lexer accepts all five C# line
    // terminators, but `loc_tokens` counted only `\n` when finding a delimited
    // comment's end row — so `/* a<U+2028>b */` reported one CLOC row instead of two.
    for terminator in ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'] {
        let source = format!("/* a{terminator}b */\nclass C {{ }}");
        let a = loc(&source);
        assert_eq!(
            a.cloc, 2.0,
            "U+{:04X} must split the comment across two CLOC rows",
            terminator as u32
        );
    }
}

#[test]
fn crlf_does_not_double_count_a_comment_row() {
    // CRLF is one break, so `/* a\r\nb */` is two rows, not three — matching
    // `LineIndex`, which skips the `\n` after a `\r`.
    let a = loc("/* a\r\nb */\nclass C { }");
    assert_eq!(a.cloc, 2.0);
}

#[test]
fn a_comment_after_a_unicode_terminator_lands_on_its_own_row() {
    // REGRESSION. `loc_tokens` took each token's start row from `tok.line()`, which the
    // *runtime's* lexer advances on `\n` alone — so after any other terminator a comment
    // was routed onto the preceding code row, and its real row fell out as a phantom
    // blank. The row now comes from the shared `LineIndex`.
    for terminator in ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'] {
        let source = format!("class C {{ }}{terminator}// note");
        let a = loc(&source);
        assert_eq!(a.cloc, 1.0, "U+{:04X}: one comment row", terminator as u32);
        assert_eq!(
            a.blank, 0.0,
            "U+{:04X}: the comment row must not read as blank",
            terminator as u32
        );
    }
}

#[test]
fn a_directive_payload_may_end_with_a_slash() {
    // REGRESSION introduced by the trailing-comment fix: excluding `/` from the negated
    // set meant neither repetition alternative could take a *final* slash, so
    // `#region generated/` stopped the token short and the slash surfaced as a visible
    // SLASH token — a syntax error on valid source.
    //
    // The fix requires a line terminator or EOF after that slash. A bare `'/'?` was the
    // first attempt and broke the case above: it matched the first `/` of a trailing
    // `//` comment and cost the row its CLOC.
    let a = loc("#region generated/
         class C { }
         #endregion");
    assert_eq!(a.cloc, 0.0, "a directive is not a comment");
    let b = loc("class C
         {
         #warning path/
         }");
    assert_eq!(b.cloc, 0.0);
}

#[test]
fn a_comment_marker_inside_a_directive_string_is_string_content() {
    // REGRESSION. Roslyn's `line_directive_trivia` / `load_directive_trivia` both accept
    // a `string_literal_token`, so `//` and `/*` between those quotes are string
    // *content* — but the trailing-comment split above is character-level and stopped at
    // the first `/`. `//` therefore left the rest of the row to SINGLE_LINE_COMMENT and
    // invented a comment on a row that has none.
    for source in [
        "#line 1 \"https://host/a.cs\"\nclass C { }",
        "#load \"https://host/a.csx\"\nclass C { }",
    ] {
        assert_eq!(
            loc(source).cloc,
            0.0,
            "a `//` inside the directive's string is not a comment: {source}"
        );
    }

    // `/*` was worse than a miscount: DELIMITED_COMMENT ran to the next `*/`, so the
    // tail of the path came back as *visible* tokens. `loc` asserts a clean parse, so
    // reaching the assertion at all is the substance of this case.
    assert_eq!(loc("#line 1 \"c:/a/*b*/c.cs\"\nclass C { }").cloc, 0.0);
}

#[test]
fn an_unpaired_quote_in_a_directive_still_ends_at_the_row() {
    // The quote-aware alternative overlaps the plain single-character one, so an
    // unclosed quote cannot complete the atom and falls through — consuming the rest of
    // the row and no more. ANTLR maximizes the match for the rule as a whole rather than
    // committing to the first viable alternative, which is what lets one rule serve both
    // cases without a predicate.
    //
    // Two rows of code after the directive: if the atom had swallowed the newline, the
    // class and its method would be inside the directive token and LLOC would drop.
    assert_eq!(
        loc("#error say \"hi
             class C
             {
                 void M() { }
             }")
        .lloc,
        2.0
    );
}

#[test]
fn a_trailing_comment_after_a_directive_string_still_counts() {
    // The counterpart to the two cases above: making the scan quote-aware must not cost
    // a *real* trailing comment its CLOC, and a `"` inside that comment must not start
    // an atom that eats the row's end.
    assert_eq!(loc("#line 1 \"a.cs\" // note\nclass C { }").cloc, 1.0);
    assert_eq!(loc("#if A // say \"hi\nclass C { }\n#endif").cloc, 1.0);
}

#[test]
fn an_extension_block_contributes_a_logical_line() {
    // REGRESSION. An `extension(T x) { … }` block opens a class-like space, so it must
    // record the logical line every other type-like declaration does — it was missing
    // from the LLOC allowlist, so an extension holding one method reported 1 where the
    // analogous `class Inner { … }` container reports 2.
    //
    // Asserted against the class control rather than an absolute, since the two
    // spellings must be indistinguishable here.
    let extension = loc("static class E
         {
             extension(string s)
             {
                 public int L() { return s.Length; }
             }
         }");
    let control = loc("static class E
         {
             class Inner
             {
                 public int L() { return 1; }
             }
         }");
    assert_eq!(extension.lloc, control.lloc);
    // outer class + container + method + return = 4.
    assert_eq!(extension.lloc, 4.0);
}

#[test]
fn a_generic_local_declaration_is_one_logical_line() {
    // REGRESSION (#218). While `List<int> l = new();` parsed as a chained
    // comparison expression (see the same-named fix in `abc.rs`), its logical-line
    // count could drift from the equivalent declarations'. LLOC must not depend on
    // which of the three spellings declares the local: class(1) + method(1) +
    // declaration(1) = 3 for each.
    for source in [
        "class C { static void F() { System.Collections.Generic.List<int> l = new(); } }",
        "class C { static void F() { var l = new System.Collections.Generic.List<int>(); } }",
        "class C { static void F() { int l = 1; } }",
        // And without an initializer — the misparse was independent of the `= …`.
        "class C { static void F() { System.Collections.Generic.List<int> l; } }",
    ] {
        assert_eq!(loc(source).lloc, 3.0, "one declaration line: {source}");
    }
}
