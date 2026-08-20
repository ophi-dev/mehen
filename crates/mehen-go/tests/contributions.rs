// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Go analyzer (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_go::GoAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    GoAnalyzer::new()
        .analyze(
            &SourceFile::new("s.go".into(), Language::Go, source.to_string()),
            config,
        )
        .expect("Go analysis succeeds")
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
package main

func classify(a int, b int) int {
    if a > 0 && b > 0 {
        return 1
    } else if a < 0 {
        return -1
    }
    total, count := 0, 0
    for i := 0; i < a; i++ {
        total += helper(i)
        count++
    }
    handler := func(x int) int { return x * 2 }
    return handler(total + count)
}
";

#[test]
fn evidence_sums_match_published_metrics() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(!analysis.contributions.is_empty());

    for (evidence_key, metric_key) in [
        ("cognitive", "cognitive.sum"),
        ("nexit", "nexit.sum"),
        ("abc.assignments", "abc.assignments"),
        ("abc.branches", "abc.branches"),
        ("abc.conditions", "abc.conditions"),
        ("nom.functions", "nom.functions"),
        ("nom.closures", "nom.closures"),
        ("nargs", "nargs"),
    ] {
        assert_eq!(
            evidence_sum(&analysis, evidence_key),
            metric(&analysis, metric_key),
            "evidence for `{evidence_key}` must sum to `{metric_key}`",
        );
    }
}

#[test]
fn reasons_are_go_namespaced_with_node_kinds() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "go.cyclomatic.if_statement",
        "go.cyclomatic.for_statement",
        "go.cyclomatic.&&",
        "go.cognitive.if_statement",
        "go.cognitive.else",
        "go.nexit.return_statement",
        "go.abc.assignment.short_var_declaration",
        "go.abc.assignment.assignment_statement",
        "go.abc.assignment.inc_statement",
        "go.abc.branch.call_expression",
        "go.abc.condition.if_statement",
        "go.nom.function.function_declaration",
        "go.nom.closure.func_literal",
        "go.nargs.function.function_declaration",
        "go.nargs.closure.func_literal",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("go.")));
}

#[test]
fn multi_target_assignments_carry_their_count() {
    // `total, count := 0, 0` is one evidence row with amount 2.
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let short_var: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "go.abc.assignment.short_var_declaration")
        .map(|item| item.amount)
        .collect();
    assert!(
        short_var.contains(&2.0),
        "expected a 2-target short_var_declaration row, got {short_var:?}",
    );
}

#[test]
fn benchmark_profile_skips_evidence_without_changing_metrics() {
    let production = analyze(FIXTURE, &AnalysisConfig::production());
    let benchmark = analyze(FIXTURE, &AnalysisConfig::benchmark());

    assert!(!production.contributions.is_empty());
    assert!(benchmark.contributions.is_empty());
    for key in ["cyclomatic.sum", "cognitive.sum", "nexit.sum", "abc"] {
        assert_eq!(
            metric(&production, key),
            metric(&benchmark, key),
            "evidence collection must not change `{key}`",
        );
    }
}
