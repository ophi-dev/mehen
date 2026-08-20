// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Ruby analyzer (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_ruby::RubyAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    RubyAnalyzer::new()
        .analyze(
            &SourceFile::new("s.rb".into(), Language::Ruby, source.to_string()),
            config,
        )
        .expect("Ruby analysis succeeds")
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
class Classifier
  def classify(a, b)
    if a > 0 && b > 0
      return 1
    else
      a += b
    end
    total = 0
    [1, 2].each { |x| total += x }
    handler = ->(y) { y * 2 }
    return handler.call(total) rescue 0
  end
end
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
fn reasons_are_ruby_namespaced_with_prism_node_names() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "ruby.cyclomatic.if_node",
        "ruby.cyclomatic.&&",
        "ruby.cognitive.if_node",
        "ruby.cognitive.else_node",
        "ruby.cyclomatic.rescue_modifier_node",
        "ruby.nexit.return_node",
        "ruby.abc.assignment.local_variable_write_node",
        "ruby.abc.assignment.local_variable_operator_write_node",
        "ruby.abc.branch.call_node",
        "ruby.abc.condition.if_node",
        "ruby.abc.condition.>",
        "ruby.nom.function.def_node",
        "ruby.nom.closure.block_node",
        "ruby.nom.closure.lambda_node",
        "ruby.nargs.function.def_node",
        "ruby.nargs.closure.block_node",
        "ruby.nargs.closure.lambda_node",
        "ruby.npm.def_node",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("ruby.")));
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
    // `a && b && c` collapses into one boolean run: first `&&` +1,
    // repeat 0 (recorded as nothing).
    let source = "\
def f(a, b, c)
  if a && b && c
    return 1
  end
  0
end
";
    let analysis = analyze(source, &AnalysisConfig::production());
    let boolean: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "ruby.cognitive.&&")
        .map(|item| item.amount)
        .collect();
    assert_eq!(boolean, vec![1.0]);
    assert_eq!(metric(&analysis, "cognitive.sum"), 2.0); // if + first &&
    assert_eq!(evidence_sum(&analysis, "cognitive"), 2.0);
}

#[test]
fn modifier_forms_record_flat_cognitive_increments() {
    // `x if y` — modifier form pays +1 without nesting.
    let source = "def f(x)\n  return 1 if x\n  0\nend\n";
    let analysis = analyze(source, &AnalysisConfig::production());
    let if_cognitive: Vec<f64> = analysis
        .contributions
        .iter()
        .filter(|item| {
            item.metric.as_str() == "cognitive" && item.reason.as_str() == "ruby.cognitive.if_node"
        })
        .map(|item| item.amount)
        .collect();
    assert_eq!(if_cognitive, vec![1.0]);
}

#[test]
fn benchmark_profile_skips_evidence_without_changing_metrics() {
    let production = analyze(FIXTURE, &AnalysisConfig::production());
    let benchmark = analyze(FIXTURE, &AnalysisConfig::benchmark());

    assert!(!production.contributions.is_empty());
    assert!(benchmark.contributions.is_empty());
    for key in ["cyclomatic.sum", "cognitive.sum", "nexit.sum", "abc", "npm"] {
        assert_eq!(
            metric(&production, key),
            metric(&benchmark, key),
            "evidence collection must not change `{key}`",
        );
    }
}
