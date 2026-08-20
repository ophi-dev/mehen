// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Java analyzer (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_java::JavaAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    JavaAnalyzer::new()
        .analyze(
            &SourceFile::new("Demo.java".into(), Language::Java, source.to_string()),
            config,
        )
        .expect("Java analysis succeeds")
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
class Demo {
    public int total;

    public int classify(int a, int b) {
        int sign = 0;
        if (a > 0 && b > 0) {
            sign = 1;
        } else {
            sign = -1;
        }
        int scaled = sign > 0 ? a : b;
        java.util.function.IntUnaryOperator twice = x -> x * 2;
        total += twice.applyAsInt(scaled);
        if (total < 0) {
            throw new IllegalStateException(\"negative\");
        }
        return total;
    }
}
";

#[test]
fn evidence_sums_match_published_metrics() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(!analysis.contributions.is_empty());

    // Families whose rolled-up value is exactly the sum of their per-event
    // evidence. Cyclomatic includes the per-space McCabe base rows
    // (`java.cyclomatic.base.<kind>`), so it sums exactly too.
    for (evidence_key, metric_key) in [
        ("cyclomatic", "cyclomatic.sum"),
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
fn reasons_are_java_namespaced_with_construct_names() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "java.cyclomatic.if_statement",
        "java.cyclomatic.logical_and",
        "java.cyclomatic.ternary_expression",
        "java.cognitive.if_statement",
        "java.cognitive.else",
        "java.cognitive.logical_and",
        "java.cognitive.ternary_expression",
        "java.nexit.return_statement",
        "java.nexit.throw_statement",
        "java.abc.assignment.variable_declarator",
        "java.abc.assignment.assignment_expression",
        "java.abc.branch.method_call",
        "java.abc.branch.creator",
        "java.abc.condition.if_statement",
        "java.abc.condition.logical_and",
        "java.abc.condition.comparison",
        "java.nom.function.method_declaration",
        "java.nom.closure.lambda_expression",
        "java.nargs.function.method_declaration",
        "java.nargs.closure.lambda_expression",
        "java.npa.field_declaration",
        "java.npm.method_declaration",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("java.")));
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
fn cognitive_amounts_carry_nesting_depth() {
    // A doubly-nested `if` pays nesting+1 = 2 on the inner statement — the
    // §5.4 "why did cognitive move +2 here" answer.
    let source = "\
class Nest {
    int probe(int a, int b) {
        if (a > 0) {
            if (b > 0) {
                return 1;
            }
        }
        return 0;
    }
}
";
    let analysis = analyze(source, &AnalysisConfig::production());
    let cognitive: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.metric.as_str() == "cognitive")
        .map(|item| item.amount)
        .collect();
    assert_eq!(cognitive, vec![1.0, 2.0]);
    assert_eq!(metric(&analysis, "cognitive.sum"), 3.0);
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
