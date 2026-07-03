// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! WMC (weighted methods per class) tests for the ANTLR Java walker.
//!
//! WMC sums the cyclomatic complexity of a class's methods. Interfaces are
//! excluded from WMC.

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
fn class_sums_method_cyclomatics() {
    // `simple` McCabe 1; `branchy` McCabe 1 + if(1) + &&(1) = 3. WMC = 4.
    let a = analyze(
        "class C {
             int simple() { return 1; }
             int branchy(int a, int b) {
                 if (a > 0 && b > 0) { return a; }
                 return b;
             }
         }",
    );
    let wmc = mehen_report::metrics_json::wmc(&a.root.metrics);
    insta::assert_json_snapshot!(wmc, @r#"
    {
      "classes": 4.0,
      "interfaces": 0.0,
      "total": 4.0
    }
    "#);
}
