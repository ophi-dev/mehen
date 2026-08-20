// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the C# analyzer (plan §5.4).
//!
//! Pins the reason-code shape (`csharp.<family>.<detail>`, detail = the
//! grammar rule's snake_case name, or the operator spelling at token-level
//! sites) and the "evidence sums to the metric" invariant for every
//! event-shaped family the walker computes.

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_csharp::CSharpAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    CSharpAnalyzer::new()
        .analyze(
            &SourceFile::new("S.cs".into(), Language::CSharp, source.to_string()),
            config,
        )
        .expect("C# analysis succeeds")
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
public class Widget
{
    public int Count;

    public int Size { get; set; } = 3;

    public int Classify(int a, int b)
    {
        int total = 0;
        if (a > 0 && b > 0)
        {
            total = a + b;
        }
        else
        {
            throw new System.ArgumentException(\"bad\");
        }
        var scale = (int x) => x * 2;
        total += scale(total);
        return total > 10 ? total : 0;
    }
}
";

#[test]
fn evidence_sums_match_published_metrics() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(
        analysis.diagnostics.is_empty(),
        "fixture must parse cleanly, got {:?}",
        analysis.diagnostics
    );
    assert!(!analysis.contributions.is_empty());

    // Families whose rolled-up value is exactly the sum of their per-event
    // evidence. (Cyclomatic is deliberately absent: its published value adds
    // a +1 base per space on top of the evidenced decisions.)
    for key in [
        "cognitive.sum",
        "nexit.sum",
        "abc.assignments",
        "abc.branches",
        "abc.conditions",
        "nom.functions",
        "nom.closures",
        "nargs",
        "npa",
        "npm",
    ] {
        assert_eq!(
            evidence_sum(&analysis, key.trim_end_matches(".sum")),
            metric(&analysis, key),
            "evidence for `{key}` must sum to the published value",
        );
    }
}

#[test]
fn reasons_are_csharp_namespaced_with_rule_names() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        // Cyclomatic: rule-level decision + the token-level `&&`.
        "csharp.cyclomatic.if_statement",
        "csharp.cyclomatic.&&",
        "csharp.cyclomatic.conditional_expression",
        // Cognitive: nesting constructs, the flat `else`, the boolean run.
        "csharp.cognitive.if_statement",
        "csharp.cognitive.else",
        "csharp.cognitive.&&",
        "csharp.cognitive.conditional_expression",
        // NExit: `throw x;` is spelled through `throw_expression` in this
        // grammar; the expression-bodied lambda is its own exit.
        "csharp.nexit.return_statement",
        "csharp.nexit.throw_expression",
        "csharp.nexit.parenthesized_lambda_expression",
        // ABC.
        "csharp.abc.assignment.local_variable_declarator",
        "csharp.abc.assignment.assignment_expression",
        "csharp.abc.assignment.property_declaration",
        "csharp.abc.branch.invocation_expression",
        "csharp.abc.branch.object_creation_expression",
        "csharp.abc.condition.if_statement",
        "csharp.abc.condition.&&",
        "csharp.abc.condition.>",
        "csharp.abc.condition.conditional_expression",
        // NOM / NArgs.
        "csharp.nom.function.method_declaration",
        "csharp.nom.function.accessor_declaration",
        "csharp.nom.closure.parenthesized_lambda_expression",
        "csharp.nargs.function.method_declaration",
        "csharp.nargs.closure.parenthesized_lambda_expression",
        // NPA / NPM (public members only).
        "csharp.npa.field_declaration",
        "csharp.npm.property_declaration",
        "csharp.npm.method_declaration",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("csharp.")));
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
    // A doubly-nested `if` pays nesting+1 = 2 on the inner node — the §5.4
    // "why did cognitive move +2 here" answer.
    let source = "\
public class Nest
{
    public int Check(int a, int b)
    {
        if (a > 0)
        {
            if (b > 0)
            {
                return 1;
            }
        }
        return 2;
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
