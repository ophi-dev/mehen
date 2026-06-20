// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NArgs tests for the tree-sitter-kotlin walker.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_kotlin::KotlinAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = KotlinAnalyzer::new();
    let file = SourceFile::new("foo.kt".into(), Language::Kotlin, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn kotlin_counts_function_constructor_and_lambda_parameters() {
    let a = analyze(
        "class C {
             constructor(a: Int, b: Int)
         }

         fun f(a: Int, b: String = \"x\", vararg xs: Int) {}

         fun g(items: List<Int>) {
             items.map { item -> item + 1 }
         }",
    );
    let nargs = mehen_report::metrics_json::nargs(&a.root.metrics);
    assert_eq!(nargs.total_functions, 6.0);
    assert_eq!(nargs.total_closures, 1.0);
    assert_eq!(nargs.functions_max, 3.0);
    assert_eq!(nargs.closures_max, 1.0);
}

/// Regression: a setter's lone parameter sits directly under `setter`
/// (`set(value)`), and an anonymous function's parameters live under
/// `parametersWithOptionalType`, not `functionValueParameters`. Both must
/// be counted — they previously reported `nargs=0`.
#[test]
fn kotlin_counts_setter_and_anonymous_function_parameters() {
    let a = analyze(
        "class C {
             var x: Int = 0
                 set(value) { field = value }
         }

         val h = fun(a: Int, b: Int): Int { return a + b }",
    );
    let nargs = mehen_report::metrics_json::nargs(&a.root.metrics);
    // setter `value` (1) + anonymous function `a, b` (2).
    assert_eq!(nargs.total_functions, 3.0);
    assert_eq!(nargs.functions_max, 2.0);
}
