// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! WMC tests for the tree-sitter-kotlin walker.

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
fn kotlin_wmc_class_sums_method_cyclomatics() {
    let a = analyze(
        "class C {
             fun a(x: Int): Int {
                 return if (x > 0) 1 else 0
             }
             fun b(): Int { return 1 }
         }",
    );
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    // class C -> a cyc = 2 (if), b cyc = 1 -> 3
    insta::assert_json_snapshot!(
        wmc,
        @r###"
    {
      "classes": 3.0,
      "interfaces": 0.0,
      "total": 3.0
    }"###
    );
}

/// Regression: a function inside an enum constant's anonymous body must not
/// roll into the enum's WMC. The entry body opens no space, so `local`
/// closes with the enum as parent — but it belongs to the entry's anonymous
/// subclass, so its cyclomatic is excluded from the enum's WMC. Only the
/// enum's own `shared` (cyclomatic 1) contributes.
#[test]
fn kotlin_wmc_excludes_enum_entry_body_functions() {
    let a = analyze(
        "enum class E {
             A {
                 fun local(x: Int): Int { return if (x > 0) 1 else 2 }
             };

             fun shared() {}
         }",
    );
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    // Only `E.shared` (cyclomatic 1) — `A.local` (cyclomatic 2) is excluded.
    assert_eq!(wmc.total, 1.0);
}
