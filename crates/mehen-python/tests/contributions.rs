// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Python analyzer (plan §5.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_python::PythonAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    PythonAnalyzer::new()
        .analyze(
            &SourceFile::new("s.py".into(), Language::Python, source.to_string()),
            config,
        )
        .expect("Python analysis succeeds")
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
def classify(a, b):
    if a > 0 and b > 0:
        return 1
    elif a < 0 or b < 0:
        return -1
    else:
        total = 0
    total += a
    handler = lambda x: x * 2
    return handler(total)
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
fn reasons_are_python_namespaced_with_node_kinds() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();

    for expected in [
        "python.cyclomatic.stmt_if",
        "python.cyclomatic.elif_clause",
        "python.cyclomatic.and",
        "python.cyclomatic.or",
        "python.cognitive.stmt_if",
        "python.cognitive.elif_clause",
        "python.cognitive.else_clause",
        "python.cognitive.and",
        "python.cognitive.or",
        "python.nexit.stmt_return",
        "python.abc.assignment.stmt_assign",
        "python.abc.assignment.stmt_aug_assign",
        "python.abc.branch.expr_call",
        "python.abc.condition.expr_compare",
        "python.nom.function.stmt_function_def",
        "python.nom.closure.expr_lambda",
        "python.nargs.function.stmt_function_def",
        "python.nargs.closure.expr_lambda",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("python.")));
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
class Point:
    x: int = 0
    y = 1
    _hidden: int = 2

    def dist(self):
        return self.x + self.y

    def _internal(self):
        return 0
";
    let analysis = analyze(source, &AnalysisConfig::production());
    assert_eq!(evidence_sum(&analysis, "npa"), metric(&analysis, "npa"));
    assert_eq!(evidence_sum(&analysis, "npm"), metric(&analysis, "npm"));
    // `x` and `y` are public; `_hidden` is not. `dist` is public;
    // `_internal` is not. Non-public members must record no evidence.
    assert_eq!(evidence_sum(&analysis, "npa"), 2.0);
    assert_eq!(evidence_sum(&analysis, "npm"), 1.0);
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "python.npa.stmt_ann_assign")
    );
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "python.npa.stmt_assign")
    );
    assert!(
        analysis
            .contributions
            .iter()
            .any(|item| item.reason.as_str() == "python.npm.stmt_function_def")
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
    ] {
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
def nested(a, b):
    if a:
        if b:
            return 1
    return 2
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
fn kitchen_sink_families_stay_internally_consistent() {
    // Exercises every clause-shaped evidence site the simple fixture
    // misses: for/while/try else-branches, `finally`, `except`,
    // `match`/`case`, `with`, conditional expressions, comprehensions
    // and their filters, walrus assignments, annotated assignments,
    // PEP 695 type aliases, `raise`, and the legacy lambda-ancestor
    // bonus on a boolean chain nested inside two lambdas.
    let source = "\
type Alias = int

def process(items, flag):
    total = 0
    for item in items:
        if item > 0 and flag:
            total += item
        elif item < 0 or not flag:
            continue
        else:
            break
    else:
        total = -1
    while total > 100:
        total -= 2
    else:
        total += 1
    try:
        result = [x * 2 for x in items if x > 0]
    except ValueError as err:
        raise RuntimeError(\"bad\") from err
    except TypeError:
        pass
    else:
        result = []
    finally:
        total += 1
    match total:
        case 0:
            pass
        case _:
            pass
    with open(\"f\") as fh:
        data = fh.read() if flag else \"\"
    counted = (n := total + 1)
    limit: int = 10
    outer = lambda a: lambda b: b or a or flag
    return outer(counted)(limit)
";
    let analysis = analyze(source, &AnalysisConfig::production());

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

    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();
    for expected in [
        "python.cyclomatic.stmt_for",
        "python.cyclomatic.for_else",
        "python.cyclomatic.stmt_while",
        "python.cyclomatic.while_else",
        "python.cyclomatic.except_handler",
        "python.cyclomatic.match_case",
        "python.cyclomatic.expr_if",
        "python.cyclomatic.comprehension",
        "python.cyclomatic.comprehension_if",
        "python.cognitive.stmt_for",
        "python.cognitive.for_else",
        "python.cognitive.stmt_while",
        "python.cognitive.while_else",
        "python.cognitive.stmt_try",
        "python.cognitive.try_else",
        "python.cognitive.try_finally",
        "python.cognitive.except_handler",
        "python.cognitive.stmt_match",
        "python.cognitive.match_case",
        "python.cognitive.stmt_with",
        "python.cognitive.expr_if",
        "python.cognitive.bool_op_lambda_bonus",
        "python.nexit.stmt_raise",
        "python.abc.assignment.expr_named",
        "python.abc.assignment.stmt_ann_assign",
        "python.abc.assignment.stmt_type_alias",
        "python.abc.condition.except_handler",
        "python.abc.condition.stmt_try",
        "python.abc.condition.stmt_match",
        "python.abc.condition.match_case",
    ] {
        assert!(
            reasons.contains(&expected),
            "missing reason `{expected}` in {reasons:?}",
        );
    }
    assert!(reasons.iter().all(|reason| reason.starts_with("python.")));
}
