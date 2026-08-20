// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the PHP analyzer (plan §5.4).
//!
//! PHP flows through the mago-syntax `Walker` visitor, which records
//! evidence next to every stat increment — these tests pin the
//! reason-code shape and the "evidence sums to the metric" invariant
//! for every event-shaped family.

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_php::PhpAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    PhpAnalyzer::new()
        .analyze(
            &SourceFile::new("s.php".into(), Language::Php, source.to_string()),
            config,
        )
        .expect("PHP analysis succeeds")
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

const FIXTURE: &str = r#"<?php

function classify(int $a, int $b): int
{
    if ($a > 0 && $b > 0) {
        return 1;
    } elseif ($a < 0) {
        return -1;
    } else {
        $a = 2;
    }
    $total = 0;
    $double = function (int $x): int {
        return $x * 2;
    };
    return $double($total + $a);
}

class Point
{
    public int $x = 0;
    private int $y = 0;

    public function sum(): int
    {
        return $this->x + $this->y;
    }
}
"#;

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
fn reasons_are_php_namespaced_with_construct_names() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "php.cyclomatic.if",
        "php.cyclomatic.elseif",
        "php.cyclomatic.&&",
        "php.cognitive.if",
        "php.cognitive.elseif",
        "php.cognitive.else",
        "php.cognitive.&&",
        "php.nexit.return",
        "php.abc.assignment.assignment",
        "php.abc.branch.call",
        "php.abc.condition.if",
        "php.abc.condition.elseif",
        "php.abc.condition.else",
        "php.abc.condition.>",
        "php.abc.condition.<",
        "php.nom.function.function",
        "php.nom.function.method",
        "php.nom.closure.closure",
        "php.nargs.function.function",
        "php.nargs.closure.closure",
        "php.npa.property",
        "php.npm.method",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("php.")));
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
fn promoted_constructor_properties_evidence_npa() {
    // PHP 8 constructor property promotion: `public int $id` in the
    // ctor declares a real (public) class property. Only public
    // members are evidenced — `private string $name` counts toward
    // the attribute denominator but not NPA.
    let source = r#"<?php
class User
{
    public function __construct(
        public int $id,
        private string $name
    ) {}

    public function id(): int
    {
        return $this->id;
    }

    private function secret(): string
    {
        return $this->name;
    }
}
"#;
    let analysis = analyze(source, &AnalysisConfig::production());
    assert_eq!(evidence_sum(&analysis, "npa"), metric(&analysis, "npa"));
    assert_eq!(evidence_sum(&analysis, "npm"), metric(&analysis, "npm"));
    assert_eq!(evidence_sum(&analysis, "npa"), 1.0);
    assert_eq!(evidence_sum(&analysis, "npm"), 2.0);
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "php.npa.promoted_property")
    );
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "php.npm.method")
    );
}

#[test]
fn cognitive_amounts_carry_nesting_depth() {
    // A doubly-nested `if` pays nesting+1 = 2 on the inner node —
    // the §5.4 "why did cognitive move +2 here" answer.
    let source = r#"<?php
function nested(int $a, int $b): int
{
    if ($a > 0) {
        if ($b > 0) {
            return 1;
        }
    }
    return 2;
}
"#;
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

#[test]
fn spaced_else_if_suppresses_nesting_but_keeps_decision() {
    // The spaced `else if` form parses as an `If` nested in an else
    // clause. The inner `if` is still a cyclomatic decision, but its
    // structural cognitive +1 was already paid by the outer if's
    // else-clause rule — so cognitive evidence carries exactly the
    // outer `if` (+1) and the `else` (+1), nothing for the inner if.
    let source = r#"<?php
function pick(int $a): int
{
    if ($a > 0) {
        return 1;
    } else if ($a < 0) {
        return -1;
    }
    return 0;
}
"#;
    let analysis = analyze(source, &AnalysisConfig::production());

    let decisions: Vec<&str> = analysis
        .contributions
        .iter()
        .filter(|item| {
            item.metric.as_str() == "cyclomatic.sum"
                // Per-space McCabe base rows are not decision events.
                && !item.reason.as_str().starts_with("php.cyclomatic.base.")
        })
        .map(|item| item.reason.as_str())
        .collect();
    assert_eq!(decisions, vec!["php.cyclomatic.if", "php.cyclomatic.if"]);

    let cognitive: Vec<(&str, f64)> = analysis
        .contributions
        .iter()
        .filter(|item| item.metric.as_str() == "cognitive.sum")
        .map(|item| (item.reason.as_str(), item.amount))
        .collect();
    assert_eq!(
        cognitive,
        vec![("php.cognitive.if", 1.0), ("php.cognitive.else", 1.0)]
    );
    assert_eq!(
        evidence_sum(&analysis, "cognitive.sum"),
        metric(&analysis, "cognitive.sum"),
    );
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
