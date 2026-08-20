// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the PowerShell analyzer (plan §5.4).
//!
//! PowerShell flows through the shared `LanguageRules` walker, which
//! records evidence centrally — these tests pin the reason-code shape
//! and the "evidence sums to the metric" invariant for every
//! event-shaped family.

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_powershell::PowerShellAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    PowerShellAnalyzer::new()
        .analyze(
            &SourceFile::new("s.ps1".into(), Language::PowerShell, source.to_string()),
            config,
        )
        .expect("PowerShell analysis succeeds")
}

fn metric(analysis: &LanguageAnalysis, key: &str) -> f64 {
    analysis
        .root
        .metrics
        .get(&MetricKey::new(key))
        .unwrap_or_else(|| panic!("missing metric {key}"))
        .as_f64()
}

fn evidence_sum(analysis: &LanguageAnalysis, key: &str) -> f64 {
    analysis
        .contributions
        .iter()
        .filter(|item| item.metric.as_str() == key)
        .map(|item| item.amount)
        .sum()
}

const FIXTURE: &str = "\
function Get-Thing($a, $b) {
    if ($a -and $b) {
        return 1
    }
    return 2
}
";

#[test]
fn evidence_sums_match_published_metrics() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(!analysis.contributions.is_empty());

    // Families whose rolled-up value is exactly the sum of their
    // per-event evidence.
    for key in [
        "cyclomatic.sum",
        "cognitive.sum",
        "nexit.sum",
        "abc.assignments",
        "abc.branches",
        "abc.conditions",
        "nom.functions",
        "nom.closures",
        "nargs",
    ] {
        assert_eq!(
            evidence_sum(&analysis, key),
            metric(&analysis, key),
            "evidence for `{key}` must sum to the published value",
        );
    }
}

#[test]
fn reasons_are_powershell_namespaced_with_node_kinds() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    assert!(reasons.contains(&"powershell.cyclomatic.if_statement"));
    assert!(reasons.contains(&"powershell.cognitive.if_statement"));
    assert!(reasons.contains(&"powershell.nexit.flow_control_statement"));
    assert!(reasons.contains(&"powershell.nom.function.function_statement"));
    assert!(reasons.contains(&"powershell.nargs.function.function_statement"));
    assert!(
        reasons
            .iter()
            .all(|reason| reason.starts_with("powershell."))
    );
}

#[test]
fn spans_are_sane_and_source_ordered() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(analysis.contributions.iter().all(|item| {
        item.span.start_byte <= item.span.end_byte
            && item.span.end_byte as usize <= FIXTURE.len()
            && item.span.start_line >= 1
            && item.span.start_line <= item.span.end_line
    }));
    assert!(analysis.contributions.windows(2).all(|pair| {
        (pair[0].span.start_byte, pair[0].span.end_byte)
            <= (pair[1].span.start_byte, pair[1].span.end_byte)
    }));
}

#[test]
fn class_members_evidence_npa_and_npm() {
    let source = "\
class Point {
    [int]$X
    [int]$Y
    [int] Sum() { return 42 }
}
";
    let analysis = analyze(source, &AnalysisConfig::production());
    assert_eq!(evidence_sum(&analysis, "npa"), metric(&analysis, "npa"));
    assert_eq!(evidence_sum(&analysis, "npm"), metric(&analysis, "npm"));
    assert_eq!(evidence_sum(&analysis, "npa"), 2.0);
    assert_eq!(evidence_sum(&analysis, "npm"), 1.0);
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "powershell.npa.class_property_definition")
    );
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "powershell.npm.class_method_definition")
    );
}

#[test]
fn benchmark_profile_skips_evidence_without_changing_metrics() {
    let production = analyze(FIXTURE, &AnalysisConfig::production());
    let benchmark = analyze(FIXTURE, &AnalysisConfig::benchmark());

    assert!(!production.contributions.is_empty());
    assert!(benchmark.contributions.is_empty());
    for key in ["cyclomatic", "cognitive.sum", "nexit", "abc", "nom"] {
        assert_eq!(
            metric(&production, key),
            metric(&benchmark, key),
            "evidence collection must not change `{key}`",
        );
    }
}

#[test]
fn cognitive_amounts_carry_nesting_depth() {
    // A doubly-nested `if` pays nesting+1 = 2 on the inner node —
    // the §5.4 "why did cognitive move +2 here" answer.
    let source = "\
function Test-Nesting($a, $b) {
    if ($a) {
        if ($b) {
            return 1
        }
    }
    return 2
}
";
    let analysis = analyze(source, &AnalysisConfig::production());
    let cognitive: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.metric.as_str() == "cognitive.sum")
        .map(|item| item.amount)
        .collect();
    assert_eq!(cognitive, vec![1.0, 2.0]);
    assert_eq!(metric(&analysis, "cognitive.sum"), 3.0);
}
