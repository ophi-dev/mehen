// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the C analyzer (plan §5.4).

use mehen_c::CAnalyzer;
use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    CAnalyzer::new()
        .analyze(
            &SourceFile::new("s.c".into(), Language::C, source.to_string()),
            config,
        )
        .expect("C analysis succeeds")
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
int classify(int a, int b) {
    if (a > 0 && b > 0) {
        return 1;
    } else {
        a += b;
    }
    for (int i = 0; i < a; i++) {
        b = helper(i);
    }
    return b > 0 ? b : 0;
}
";

#[test]
fn evidence_sums_match_published_metrics() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(!analysis.contributions.is_empty());

    for (evidence_key, metric_key) in [
        ("cyclomatic.sum", "cyclomatic.sum"),
        ("cognitive.sum", "cognitive.sum"),
        ("nexit.sum", "nexit.sum"),
        ("abc.assignments", "abc.assignments"),
        ("abc.branches", "abc.branches"),
        ("abc.conditions", "abc.conditions"),
        ("nom.functions", "nom.functions"),
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
fn decision_evidence_counts_match_cyclomatic() {
    // Per-space base rows (`c.cyclomatic.base.<kind>`) cover the +1 McCabe
    // constant per folded space, so cyclomatic evidence sums exactly.
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert_eq!(
        evidence_sum(&analysis, "cyclomatic.sum"),
        metric(&analysis, "cyclomatic.sum")
    );
    let mut bases: Vec<&str> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str().starts_with("c.cyclomatic.base."))
        .map(|item| item.reason.as_str())
        .collect();
    bases.sort_unstable();
    // One function space + one unit.
    assert_eq!(
        bases,
        vec!["c.cyclomatic.base.function", "c.cyclomatic.base.unit"]
    );
}

#[test]
fn reasons_are_c_namespaced_with_node_kinds() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "c.cyclomatic.if_statement",
        "c.cyclomatic.for_statement",
        "c.cyclomatic.conditional_expression",
        "c.cyclomatic.&&",
        "c.cognitive.if_statement",
        "c.cognitive.else_clause",
        "c.nexit.return_statement",
        "c.abc.assignment.assignment_expression",
        "c.abc.assignment.init_declarator",
        "c.abc.assignment.update_expression",
        "c.abc.branch.call_expression",
        "c.abc.condition.if_statement",
        "c.nom.function.function_definition",
        "c.nargs.function.function_definition",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("c.")));
}

#[test]
fn boolean_run_transitions_record_only_moved_deltas() {
    // `a && b && c` is one boolean run: the first `&&` pays +1, the
    // repeat pays nothing and records nothing.
    let source = "\
int f(int a, int b, int c) {
    if (a && b && c) {
        return 1;
    }
    return 0;
}
";
    let analysis = analyze(source, &AnalysisConfig::production());
    let boolean_evidence: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "c.cognitive.&&")
        .map(|item| item.amount)
        .collect();
    assert_eq!(boolean_evidence, vec![1.0]);
    assert_eq!(evidence_sum(&analysis, "cognitive.sum"), 2.0); // if + first &&
    assert_eq!(metric(&analysis, "cognitive.sum"), 2.0);
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
