// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Halstead tests for the ANTLR Kotlin walker.
//!
//! The ANTLR backend tokenizes Kotlin more completely than the former
//! tree-sitter walker: every lexical token is classified, so closing
//! delimiters (`)`, `}`) count as distinct Halstead operators alongside
//! their opening forms. Operand counts are unchanged. These snapshots
//! reflect the richer (and more classically Halstead-complete) operator
//! vocabulary — an intentional improvement over the tree-sitter numbers,
//! per the ANTLR migration's "improve where the grammar allows" policy.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_kotlin::KotlinAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = KotlinAnalyzer::new();
    let file = SourceFile::new("foo.kt".into(), Language::Kotlin, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

/// Regression: `get`/`set` accessor keywords must be Halstead operators
/// and `field` must be an operand. The ANTLR walker classifies a token
/// reached via the `simpleIdentifier` rule as an operand, so Kotlin soft
/// keywords used as names (`value`, `field`) count as operands while the
/// `get`/`set` accessor keywords (reached via the `getter`/`setter` rules)
/// stay operators.
///
/// Operands (distinct by text): `C`, `x`, `Int`, `0`, `field`, `value` = 6.
/// Operators (distinct token types): `class`, `{`, `}`, `var`, `:`, `=`,
/// `get`, `(`, `)`, `set` = 10. (The ANTLR lexer counts both opening and
/// closing delimiters, unlike the former tree-sitter walker.)
#[test]
fn kotlin_accessor_tokens_contribute_to_halstead() {
    let a = analyze(
        "class C {
             var x: Int = 0
                 get() = field
                 set(value) { field = value }
         }",
    );
    let h = mehen_report::metrics_json::halstead(&a.root.metrics);
    assert_eq!(h.n1, 10.0, "distinct operators");
    assert_eq!(h.big_n1, 16.0);
    assert_eq!(h.n2, 6.0, "distinct operands (C/x/Int/0/field/value)");
    assert_eq!(h.big_n2, 8.0);
}

#[test]
fn kotlin_operators_and_operands() {
    let a = analyze(
        "fun add(a: Int, b: Int): Int {
             return a + b
         }",
    );
    let h = mehen_report::metrics_json::halstead(&a.root.metrics);
    // Only core counts are locked in; derived measures shift with the
    // vocabulary in ways that aren't meaningful to assert.
    insta::assert_json_snapshot!(
        h,
        {
            ".estimated_program_length" => "[masked]",
            ".purity_ratio" => "[masked]",
            ".volume" => "[masked]",
            ".difficulty" => "[masked]",
            ".level" => "[masked]",
            ".effort" => "[masked]",
            ".time" => "[masked]",
            ".bugs" => "[masked]"
        },
        @r###"
    {
      "n1": 9.0,
      "N1": 11.0,
      "n2": 4.0,
      "N2": 8.0,
      "length": 19.0,
      "estimated_program_length": "[masked]",
      "purity_ratio": "[masked]",
      "vocabulary": 13.0,
      "volume": "[masked]",
      "difficulty": "[masked]",
      "level": "[masked]",
      "effort": "[masked]",
      "time": "[masked]",
      "bugs": "[masked]"
    }"###
    );
}

/// Regression: raw/triple-quoted string delimiters (`"""`) are skipped in
/// Halstead just like ordinary `"` delimiters, so a raw string records the
/// same operator counts as an equivalent ordinary string (no inflation).
#[test]
fn kotlin_raw_string_delimiters_excluded_from_halstead() {
    let raw = analyze("fun f() = \"\"\"hello\"\"\"\n");
    let ord = analyze("fun f() = \"hello\"\n");
    let rh = mehen_report::metrics_json::halstead(&raw.root.metrics);
    let oh = mehen_report::metrics_json::halstead(&ord.root.metrics);
    assert_eq!(
        (rh.n1, rh.big_n1),
        (oh.n1, oh.big_n1),
        "raw-string delimiters must not add Halstead operators vs. ordinary strings"
    );
}

/// Regression: a simple string-template reference (`"$x"`) lexes as a single
/// `LINE_STR_REF` token holding the interpolated identifier, which must be a
/// Halstead operand (like the `x` in the `"${x}"` form) rather than falling
/// through to the operator default.
#[test]
fn kotlin_simple_string_template_ref_is_operand() {
    // `fun f(x: Int) = "$x"`. Operands (distinct text): `f`, `x`, `Int`, and
    // the `$x` ref token (text `$x`) = 4. The point is that the ref counts as
    // an *operand* — before the fix it fell through to the operator default,
    // so it would have inflated n1 and been absent from n2.
    let with_ref = analyze("fun f(x: Int) = \"$x\"\n");
    let h = mehen_report::metrics_json::halstead(&with_ref.root.metrics);
    assert_eq!(h.n2, 4.0, "the `$x` ref must be counted as an operand");
    // Sanity: an ordinary string literal of the same shape has the same
    // operator count — the ref didn't leak into operators.
    let plain = analyze("fun f(x: Int) = \"hi\"\n");
    let ph = mehen_report::metrics_json::halstead(&plain.root.metrics);
    assert_eq!(h.n1, ph.n1, "the ref must not be classified as an operator");
}

/// Regression: string *content* tokens — escaped chars (`\n` →
/// `LINE_STR_ESCAPED_CHAR`) and the literal `"` runs in raw strings
/// (`MULTI_LINE_STRING_QUOTE`) — are Halstead operands (the literal's value),
/// not operators. An escape must not inflate the operator count vs. an
/// equivalent plain string.
#[test]
fn kotlin_string_escape_content_is_operand_not_operator() {
    let esc = analyze("fun f() = \"a\\nb\"\n");
    let plain = analyze("fun f() = \"ab\"\n");
    let eh = mehen_report::metrics_json::halstead(&esc.root.metrics);
    let ph = mehen_report::metrics_json::halstead(&plain.root.metrics);
    assert_eq!(
        eh.n1, ph.n1,
        "the \\n escape must be an operand, not an extra operator"
    );
}

/// Analyze as a `.kts` script so the shebang is parsed via the `script`
/// entry rule.
fn analyze_kts(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = KotlinAnalyzer::new();
    let file = SourceFile::new("foo.kts".into(), Language::Kotlin, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

/// Regression: a `.kts` shebang (`#!/usr/bin/env kotlin`) is an interpreter
/// directive, not a Kotlin operator/operand. It must not contribute to the
/// Halstead vocabulary — a script with a shebang has the same operator and
/// operand counts as the same script without one.
#[test]
fn kotlin_shebang_excluded_from_halstead() {
    let with_shebang = analyze_kts("#!/usr/bin/env kotlin\nval x = 1\nprintln(x)\n");
    let without = analyze_kts("val x = 1\nprintln(x)\n");
    let wh = mehen_report::metrics_json::halstead(&with_shebang.root.metrics);
    let oh = mehen_report::metrics_json::halstead(&without.root.metrics);
    assert_eq!(
        (wh.n1, wh.big_n1, wh.n2, wh.big_n2),
        (oh.n1, oh.big_n1, oh.n2, oh.big_n2),
        "the shebang must not change any Halstead count"
    );
}

/// Regression: an empty string literal (`""` / `""""""`) emits only
/// delimiter tokens (all skipped) and no content token, so it would record
/// no Halstead operand at all — undercounting Halstead/MI for the common
/// empty-string default. An empty literal must count as one operand, like a
/// non-empty literal of the same shape.
#[test]
fn kotlin_empty_string_literal_is_operand() {
    let empty = analyze("fun f() = \"\"\n");
    let nonempty = analyze("fun f() = \"x\"\n");
    let raw_empty = analyze("fun f() = \"\"\"\"\"\"\n");
    let eh = mehen_report::metrics_json::halstead(&empty.root.metrics);
    let nh = mehen_report::metrics_json::halstead(&nonempty.root.metrics);
    let rh = mehen_report::metrics_json::halstead(&raw_empty.root.metrics);
    // `f` + the literal = 2 operands, total 2, same as a non-empty literal.
    assert_eq!((eh.n2, eh.big_n2), (2.0, 2.0), "empty `\"\"` is an operand");
    assert_eq!((eh.n2, eh.big_n2), (nh.n2, nh.big_n2));
    assert_eq!(
        (rh.n2, rh.big_n2),
        (2.0, 2.0),
        "empty raw `\"\"\"\"\"\"` is an operand"
    );
}

/// Regression: a labeled receiver (`this@Outer` / `super@Outer`) lexes as a
/// single `THIS_AT`/`SUPER_AT` token. It names the same receiver value as
/// bare `this`/`super`, so it must be a Halstead operand — not fall through
/// to the operator default (which both inflates n1 and drops the receiver
/// from the operand set).
#[test]
fn kotlin_labeled_receiver_is_operand() {
    let labeled =
        analyze("class Outer {\n  inner class Inner {\n    fun f() = this@Outer\n  }\n}\n");
    let bare = analyze("class Outer {\n  inner class Inner {\n    fun f() = this\n  }\n}\n");
    let lh = mehen_report::metrics_json::halstead(&labeled.root.metrics);
    let bh = mehen_report::metrics_json::halstead(&bare.root.metrics);
    // `this@Outer` must classify exactly like bare `this`.
    assert_eq!(
        (lh.n1, lh.big_n1, lh.n2, lh.big_n2),
        (bh.n1, bh.big_n1, bh.n2, bh.big_n2),
        "this@Outer must classify like bare this (operand, not operator)"
    );
}

/// Regression: an *explicit* `;` statement separator (`val a = 1; val b = 2`)
/// is a typed punctuator, peer to `,`/`.`/`:`/`(` — it must count as a
/// Halstead operator. (Only `NL`, which Kotlin emits pervasively as
/// structural whitespace, is skipped.) An explicit semicolon therefore adds
/// exactly one distinct operator vs. the newline-separated form.
#[test]
fn kotlin_explicit_semicolon_is_operator() {
    let semi = analyze("fun f() { val a = 1; val b = 2 }\n");
    let newline = analyze("fun f() { val a = 1\n val b = 2 }\n");
    let sh = mehen_report::metrics_json::halstead(&semi.root.metrics);
    let nh = mehen_report::metrics_json::halstead(&newline.root.metrics);
    assert_eq!(
        sh.n1,
        nh.n1 + 1.0,
        "the explicit `;` must add one distinct operator"
    );
    assert_eq!(sh.big_n1, nh.big_n1 + 1.0, "and one operator occurrence");
}
