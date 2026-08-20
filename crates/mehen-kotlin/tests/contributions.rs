// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Kotlin analyzer (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_kotlin::KotlinAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    KotlinAnalyzer::new()
        .analyze(
            &SourceFile::new("S.kt".into(), Language::Kotlin, source.to_string()),
            config,
        )
        .expect("Kotlin analysis succeeds")
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
class Classifier(val bias: Int) {
    fun classify(a: Int, b: Int): Int {
        if (a > 0 && b > 0) {
            return 1
        } else {
            return -1
        }
    }

    fun tally(items: List<Int>): Int {
        var total = 0
        for (item in items) {
            total += item
        }
        val doubled = items.map { it * 2 }
        return when {
            total > 10 -> total
            else -> doubled.size
        }
    }
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
fn reasons_are_kotlin_namespaced_with_grammar_names() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "kotlin.cyclomatic.if_expression",
        "kotlin.cyclomatic.for_statement",
        "kotlin.cyclomatic.when_entry",
        "kotlin.cyclomatic.&&",
        "kotlin.cognitive.if_expression",
        "kotlin.cognitive.for_statement",
        "kotlin.cognitive.when_expression",
        "kotlin.cognitive.else",
        "kotlin.nexit.return",
        "kotlin.abc.assignment.property_declaration",
        "kotlin.abc.assignment.assignment",
        "kotlin.abc.branch.call_suffix",
        "kotlin.abc.condition.if_expression",
        "kotlin.abc.condition.>",
        "kotlin.nom.function.function_declaration",
        "kotlin.nom.closure.lambda_literal",
        "kotlin.nargs.function.function_declaration",
        "kotlin.npa.class_parameter",
        "kotlin.npm.function_declaration",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("kotlin.")));
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
fn boolean_run_transitions_record_only_moved_deltas() {
    let source = "\
fun f(a: Boolean, b: Boolean, c: Boolean): Int {
    if (a && b && c) {
        return 1
    }
    return 0
}
";
    let analysis = analyze(source, &AnalysisConfig::production());
    let boolean: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "kotlin.cognitive.&&")
        .map(|item| item.amount)
        .collect();
    assert_eq!(boolean, vec![1.0]);
    assert_eq!(metric(&analysis, "cognitive.sum"), 2.0); // if + boolean run
}

#[test]
fn private_members_record_no_public_evidence() {
    let source = "\
class Vault(private val secret: Int, val open: Int) {
    private fun hidden() {}
    fun visible() {}
}
";
    let analysis = analyze(source, &AnalysisConfig::production());
    assert_eq!(evidence_sum(&analysis, "npa"), metric(&analysis, "npa"));
    assert_eq!(evidence_sum(&analysis, "npm"), metric(&analysis, "npm"));
    assert_eq!(evidence_sum(&analysis, "npa"), 1.0);
    assert_eq!(evidence_sum(&analysis, "npm"), 1.0);
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
        "npa",
        "npm",
        "wmc",
    ] {
        assert_eq!(
            metric(&production, key),
            metric(&benchmark, key),
            "evidence collection must not change `{key}`",
        );
    }
}
