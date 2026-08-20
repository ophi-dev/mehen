// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Oxc-backed analyzers (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_typescript::{TsxAnalyzer, TypeScriptAnalyzer};

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    TypeScriptAnalyzer::new()
        .analyze(
            &SourceFile::new("s.ts".into(), Language::TypeScript, source.to_string()),
            config,
        )
        .expect("TypeScript analysis succeeds")
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
class Point {
    x: number = 0;
    private secret: number = 1;
    sum(a: number, b: number): number {
        if (a > 0 && b > 0) {
            return a + b;
        } else {
            this.x += 1;
        }
        const double = (y: number) => y * 2;
        return double(this.x) ? 1 : 0;
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
fn reasons_are_typescript_namespaced() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "typescript.cyclomatic.if_statement",
        "typescript.cyclomatic.&&",
        "typescript.cyclomatic.conditional_expression",
        "typescript.cognitive.if_statement",
        "typescript.cognitive.&&",
        "typescript.cognitive.conditional_expression",
        "typescript.nexit.return_statement",
        "typescript.abc.assignment.assignment_expression",
        "typescript.abc.assignment.variable_declarator",
        "typescript.abc.branch.call_expression",
        "typescript.abc.condition.if_statement",
        "typescript.abc.condition.>",
        "typescript.abc.condition.else_clause",
        "typescript.nom.function.method_definition",
        "typescript.nom.closure.arrow_function_expression",
        "typescript.nargs.function.method_definition",
        "typescript.nargs.closure.arrow_function_expression",
        "typescript.npa.property_definition",
        "typescript.npm.method_definition",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(
        reasons
            .iter()
            .all(|reason| reason.starts_with("typescript."))
    );
}

#[test]
fn private_members_record_no_public_evidence() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    // One public field (`x`) — the `private secret` moves total_na but
    // not the public headline count, so evidence covers `x` only.
    assert_eq!(evidence_sum(&analysis, "npa"), 1.0);
    assert_eq!(metric(&analysis, "npa"), 1.0);
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
    let source = "function f(a: boolean, b: boolean, c: boolean) {\n    if (a && b && c) {\n        return 1;\n    }\n    return 0;\n}\n";
    let analysis = analyze(source, &AnalysisConfig::production());
    let boolean: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "typescript.cognitive.&&")
        .map(|item| item.amount)
        .collect();
    assert_eq!(boolean, vec![1.0]);
    assert_eq!(metric(&analysis, "cognitive.sum"), 2.0); // if + boolean run
}

#[test]
fn tsx_files_carry_the_tsx_prefix() {
    let source =
        "function App({ on }: { on: boolean }) {\n    return on ? <b>1</b> : <i>0</i>;\n}\n";
    let analysis = TsxAnalyzer::new()
        .analyze(
            &SourceFile::new("s.tsx".into(), Language::Tsx, source.to_string()),
            &AnalysisConfig::production(),
        )
        .expect("TSX analysis succeeds");
    assert!(!analysis.contributions.is_empty());
    assert!(
        analysis
            .contributions
            .iter()
            .all(|item| item.reason.as_str().starts_with("tsx."))
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
