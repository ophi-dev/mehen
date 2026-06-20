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
