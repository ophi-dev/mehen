// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC tests for the ANTLR Java walker.
//!
//! PLOC = physical code lines; LLOC = logical (statement/declaration) lines;
//! CLOC = comment lines (block `COMMENT` + `LINE_COMMENT`, routed to the
//! deepest enclosing space); SLOC = source lines.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_java::JavaAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = JavaAnalyzer::new();
    let file = SourceFile::new("Foo.java".into(), Language::Java, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn simple_loc() {
    let a = analyze(
        "package demo;\n\
         // a line comment\n\
         class C {\n\
         \x20   /* a block comment */\n\
         \x20   int f() {\n\
         \x20       return 1;\n\
         \x20   }\n\
         }\n",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    insta::assert_json_snapshot!(loc, @r#"
    {
      "sloc": 8.0,
      "ploc": 6.0,
      "lloc": 4.0,
      "cloc": 2.0,
      "blank": 0.0,
      "sloc_average": 2.6666666666666665,
      "ploc_average": 2.0,
      "lloc_average": 1.3333333333333333,
      "cloc_average": 0.6666666666666666,
      "blank_average": 0.0,
      "sloc_min": 3.0,
      "sloc_max": 3.0,
      "cloc_min": 0.0,
      "cloc_max": 0.0,
      "ploc_min": 3.0,
      "ploc_max": 3.0,
      "lloc_min": 2.0,
      "lloc_max": 2.0,
      "blank_min": 0.0,
      "blank_max": 0.0
    }
    "#);
}

#[test]
fn classic_for_header_is_one_lloc_like_enhanced_for() {
    // Regression (audit): a classic `for (int i=…; …)` must not double-count
    // its initializer declaration as a second LLOC — it should match the
    // equivalent enhanced-for's LLOC.
    let classic = analyze(
        "class C {
             void f() {
                 for (int i = 0; i < 10; i++) { g(i); }
             }
         }",
    );
    let enhanced = analyze(
        "class C {
             void f(int[] xs) {
                 for (int x : xs) { g(x); }
             }
         }",
    );
    let c = serde_json::to_value(mehen_report::metrics_json::loc(&classic.root.metrics)).unwrap();
    let e = serde_json::to_value(mehen_report::metrics_json::loc(&enhanced.root.metrics)).unwrap();
    assert_eq!(
        c["lloc"], e["lloc"],
        "classic-for and enhanced-for should report the same LLOC"
    );
}

#[test]
fn empty_and_labeled_statements_are_not_their_own_lloc() {
    // Regression (audit): a bare `;` (empty statement) and a label wrapper
    // (`lbl: stmt`) are not their own logical lines. Here the only LLOC-bearing
    // statements are the two `g(...)` calls inside the labeled loop and the
    // plain call, matching an equivalent body without `;`/labels.
    let with_noise = analyze(
        "class C {
             void f() {
                 ;
                 lbl: for (int i = 0; i < 2; i++) { g(i); }
                 ;
             }
         }",
    );
    let without = analyze(
        "class C {
             void f() {
                 for (int i = 0; i < 2; i++) { g(i); }
             }
         }",
    );
    let a =
        serde_json::to_value(mehen_report::metrics_json::loc(&with_noise.root.metrics)).unwrap();
    let b = serde_json::to_value(mehen_report::metrics_json::loc(&without.root.metrics)).unwrap();
    assert_eq!(
        a["lloc"], b["lloc"],
        "empty statements and labels must not inflate LLOC"
    );
}

#[test]
fn text_block_interior_rows_count_as_ploc() {
    // Regression (PR #160 review): a Java text block (`"""…"""`) is a single
    // `TEXT_BLOCK` token spanning multiple physical lines. Every row it covers
    // is code (PLOC), not blank — otherwise the interior rows are reported as
    // phantom blank lines.
    let a = analyze(
        "class C {\n\
         \x20   String s = \"\"\"\n\
         \x20       line one\n\
         \x20       line two\n\
         \x20       \"\"\";\n\
         }\n",
    );
    let loc = serde_json::to_value(mehen_report::metrics_json::loc(&a.root.metrics)).unwrap();
    // 6 physical lines, all code, none blank.
    assert_eq!(
        loc["ploc"],
        serde_json::json!(6.0),
        "all text-block rows are code"
    );
    assert_eq!(
        loc["blank"],
        serde_json::json!(0.0),
        "no phantom blank lines"
    );
}

#[test]
fn enum_constants_count_as_lloc() {
    // Regression (PR #160 review): each enum constant is a declaration → a
    // logical line. `enum E { A, B, C }` = enum decl (1) + 3 constants = 4.
    let a = analyze("enum E { A, B, C }");
    let loc = serde_json::to_value(mehen_report::metrics_json::loc(&a.root.metrics)).unwrap();
    assert_eq!(loc["lloc"], serde_json::json!(4.0));
}

#[test]
fn expression_bodied_lambda_counts_one_lloc() {
    // Regression (PR #160 review): an expression-bodied lambda (`x -> x + 1`)
    // opens a closure space whose body is an `expression`, not a statement, so
    // its own `loc.lloc` would be 0. It must count as one logical line, like a
    // block-bodied lambda (whose inner statements count) and method decls. Find
    // the closure space and assert its LLOC.
    fn closure_lloc(sp: &mehen_core::MetricSpace) -> Option<f64> {
        if sp.kind == mehen_core::SpaceKind::Closure {
            return Some(
                sp.metrics
                    .get(&mehen_core::MetricKey::new("loc.lloc"))
                    .map(|m| m.as_f64())
                    .unwrap_or(0.0),
            );
        }
        sp.spaces.iter().find_map(closure_lloc)
    }
    let expr = analyze("class C { java.util.function.Function<Integer, Integer> f = x -> x + 1; }");
    assert_eq!(
        closure_lloc(&expr.root),
        Some(1.0),
        "an expression-bodied lambda is one logical line"
    );
    let block = analyze("class C { Runnable r = () -> { g(); }; }");
    assert_eq!(
        closure_lloc(&block.root),
        Some(1.0),
        "a block-bodied lambda counts its inner statement (no double-count)"
    );
}

#[test]
fn module_descriptor_directives_count_as_lloc() {
    // Regression (PR #160 review): a `module-info.java` descriptor parses via
    // `modularCompilationUnit → moduleDeclaration` with `moduleDirective`
    // children; these must count as LLOC or a module file reports lloc == 0.
    // Here: module declaration (1) + requires (1) + exports (1) = 3.
    let a = analyze("module com.example { requires java.base; exports com.example.api; }");
    let loc = serde_json::to_value(mehen_report::metrics_json::loc(&a.root.metrics)).unwrap();
    assert_eq!(
        loc["lloc"],
        serde_json::json!(3.0),
        "module declaration + 2 directives should be 3 logical lines"
    );
    assert!(
        a.diagnostics.is_empty(),
        "module descriptor should parse cleanly"
    );
}

#[test]
fn interface_and_annotation_members_count_as_lloc() {
    // Regression (PR #160 review): an interface method
    // (`interfaceCommonBodyDeclaration`) and an annotation element
    // (`annotationMethodRest`) are declaration nodes and must count as LLOC,
    // just like a class abstract method — an interface API should not
    // under-report logical LOC vs the equivalent abstract class.
    let iface = analyze("interface I { void m(); }");
    let cls = analyze("abstract class C { abstract void m(); }");
    let i = serde_json::to_value(mehen_report::metrics_json::loc(&iface.root.metrics)).unwrap();
    let c = serde_json::to_value(mehen_report::metrics_json::loc(&cls.root.metrics)).unwrap();
    assert_eq!(
        i["lloc"], c["lloc"],
        "interface method LLOC should match the equivalent abstract class method"
    );
    // Annotation element + constant each count: type decl + method + constant.
    let anno = analyze("@interface An { String v(); int X = 1; }");
    let av = serde_json::to_value(mehen_report::metrics_json::loc(&anno.root.metrics)).unwrap();
    assert_eq!(av["lloc"], serde_json::json!(3.0));
}

#[test]
fn for_init_suppression_does_not_leak_into_nested_lambda_body() {
    // Regression (PR #160 review): the classic-`for` header suppresses its
    // initializer declaration's own LLOC (it is part of the `for` statement's
    // single logical line). That suppression must apply ONLY to the direct
    // `forInit` declaration — not to real local declarations nested inside a
    // lambda/anonymous-class body that happens to live in the header
    // initializer. Here the lambda body's `int x = 0;` is genuine code and must
    // count. The same lambda scores identically whether it initializes a `for`
    // header variable or a plain field.
    fn closure_lloc(sp: &mehen_core::MetricSpace) -> Option<f64> {
        if sp.kind == mehen_core::SpaceKind::Closure {
            return Some(
                sp.metrics
                    .get(&mehen_core::MetricKey::new("loc.lloc"))
                    .map(|m| m.as_f64())
                    .unwrap_or(0.0),
            );
        }
        sp.spaces.iter().find_map(closure_lloc)
    }
    let in_for = analyze(
        "class C {
             void f() {
                 for (java.util.function.Supplier<Integer> s = () -> { int x = 0; return x; }; ; ) { break; }
             }
         }",
    );
    let plain = analyze(
        "class C {
             java.util.function.Supplier<Integer> s = () -> { int x = 0; return x; };
         }",
    );
    // The lambda body has two logical lines (`int x = 0;` + `return x;`); the
    // for-init suppression must not drop the declaration.
    assert_eq!(
        closure_lloc(&in_for.root),
        Some(2.0),
        "the lambda body's declaration must count even inside a for-init"
    );
    assert_eq!(
        closure_lloc(&in_for.root),
        closure_lloc(&plain.root),
        "a lambda body scores the same in a for-init as in a field initializer"
    );
}

#[test]
fn block_only_statement_is_not_its_own_lloc() {
    // A bare `{ … }` block statement is not a logical line; the inner
    // statements each count.
    let a = analyze(
        "class C {
             void f() {
                 {
                     int x = 1;
                 }
             }
         }",
    );
    let loc = mehen_report::metrics_json::loc(&a.root.metrics);
    insta::assert_json_snapshot!(loc, @r#"
    {
      "sloc": 7.0,
      "ploc": 7.0,
      "lloc": 3.0,
      "cloc": 0.0,
      "blank": 0.0,
      "sloc_average": 2.3333333333333335,
      "ploc_average": 2.3333333333333335,
      "lloc_average": 1.0,
      "cloc_average": 0.0,
      "blank_average": 0.0,
      "sloc_min": 5.0,
      "sloc_max": 5.0,
      "cloc_min": 0.0,
      "cloc_max": 0.0,
      "ploc_min": 5.0,
      "ploc_max": 5.0,
      "lloc_min": 2.0,
      "lloc_max": 2.0,
      "blank_min": 0.0,
      "blank_max": 0.0
    }
    "#);
}

#[test]
fn a_unicode_separator_in_a_block_comment_is_not_a_row_break() {
    // REGRESSION from the C# work in this PR. `loc_tokens` in the shared `mehen-antlr`
    // crate counted five line terminators inline when finding a block comment's end row
    // — but which characters break a row is per-language policy, and Java passes
    // `LineIndex::new` (LF/CRLF only, matching the JLS \u000A/\u000D/\u2028?/no).
    //
    // So `/*a<U+2028>b*/` was reported as covering two comment rows in a ONE-row file:
    // CLOC 2 against SLOC 1, which is impossible, and which also skews
    // `blank = sloc - ploc - only_comment` and every MI variant downstream. The end row
    // now comes from the same `LineIndex` the start row does.
    for separator in ['\u{85}', '\u{2028}', '\u{2029}'] {
        let a = analyze(&format!("class C {{ /*a{separator}b*/ }}"));
        let loc = mehen_report::metrics_json::loc(&a.root.metrics);
        assert_eq!(
            loc.cloc, 1.0,
            "U+{:04X} is not a row break for Java",
            separator as u32
        );
        assert!(
            loc.cloc <= loc.sloc,
            "U+{:04X}: CLOC {} must not exceed SLOC {}",
            separator as u32,
            loc.cloc,
            loc.sloc
        );
    }

    // The control: LF *is* a row break, so the same comment covers two rows.
    let lf = mehen_report::metrics_json::loc(&analyze("class C { /*a\nb*/ }").root.metrics);
    assert_eq!(lf.cloc, 2.0);
}
