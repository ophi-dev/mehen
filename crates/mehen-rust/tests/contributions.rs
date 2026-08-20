// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Rust analyzer (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_rust::RustAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    RustAnalyzer::new()
        .analyze(
            &SourceFile::new("s.rs".into(), Language::Rust, source.to_string()),
            config,
        )
        .expect("Rust analysis succeeds")
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
pub struct Point {
    pub x: i32,
    y: i32,
}

impl Point {
    pub fn total(&self) -> i32 {
        self.x + self.y
    }
}

fn classify(a: i32, b: i32) -> i32 {
    let mut total = 0;
    if a > 0 && b > 0 {
        total = 1;
    } else if a < 0 {
        total = -1;
    }
    match total {
        0 => total = 9,
        _ => total += 1,
    }
    let double = |x: i32| (x * 2).abs();
    if total > 100 {
        return double(total);
    }
    double(total)
}
";

#[test]
fn evidence_sums_match_published_metrics() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(!analysis.contributions.is_empty());

    // Families whose rolled-up value is exactly the sum of their
    // per-event evidence. Cyclomatic is excluded on purpose: the
    // published McCabe value adds a `+1` constant per space that has
    // no evidence event.
    for (evidence_key, metric_key) in [
        ("cognitive", "cognitive.sum"),
        ("nexit", "nexit.sum"),
        ("abc.assignments", "abc.assignments"),
        ("abc.branches", "abc.branches"),
        ("abc.conditions", "abc.conditions"),
        ("nom.functions", "nom.functions"),
        ("nom.closures", "nom.closures"),
        ("nargs", "nargs"),
        ("npa", "npa"),
        ("npm", "npm"),
    ] {
        assert_eq!(
            evidence_sum(&analysis, evidence_key),
            metric(&analysis, metric_key),
            "evidence for `{evidence_key}` must sum to `{metric_key}`",
        );
    }
}

#[test]
fn reasons_are_rust_namespaced_with_node_kinds() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "rust.cyclomatic.if_expr",
        "rust.cyclomatic.match_arm",
        "rust.cyclomatic.&&",
        "rust.cognitive.if_expr",
        "rust.cognitive.else_kw",
        "rust.cognitive.match_expr",
        "rust.cognitive.&&",
        "rust.nexit.return_expr",
        "rust.abc.assignment.let_stmt",
        "rust.abc.assignment.bin_expr",
        "rust.abc.branch.call_expr",
        "rust.abc.branch.method_call_expr",
        "rust.abc.condition.if_expr",
        "rust.abc.condition.match_expr",
        "rust.abc.condition.match_arm",
        "rust.abc.condition.>",
        "rust.nom.function.fn",
        "rust.nom.closure.closure_expr",
        "rust.nargs.function.fn",
        "rust.nargs.closure.closure_expr",
        "rust.npa.record_field",
        "rust.npm.fn",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("rust.")));
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
fn benchmark_profile_skips_evidence_without_changing_metrics() {
    let production = analyze(FIXTURE, &AnalysisConfig::production());
    let benchmark = analyze(FIXTURE, &AnalysisConfig::benchmark());

    assert!(!production.contributions.is_empty());
    assert!(benchmark.contributions.is_empty());
    for key in [
        "cyclomatic.sum",
        "cognitive.sum",
        "nexit.sum",
        "abc",
        "nom",
        "nargs",
        "npa",
        "npm",
    ] {
        assert_eq!(
            metric(&production, key),
            metric(&benchmark, key),
            "evidence collection must not change `{key}`",
        );
    }
}
