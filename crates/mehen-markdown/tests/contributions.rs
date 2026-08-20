// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Contribution-evidence tests for the Markdown analyzer (plan §5.4).
//!
//! Markdown evidences its own metric family: MCC prose-structure events
//! (heading skips, oversized flat sections, over-long paragraphs, dense
//! link clusters) carry the exact weighted amount they added to the
//! positive MCC term, and each broken link adds +1 toward
//! `markdown.links.broken` (research §39.4).

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_markdown::MarkdownAnalyzer;

fn analyze(source: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    MarkdownAnalyzer::new()
        .analyze(
            &SourceFile::new("doc.md".into(), Language::Markdown, source.to_string()),
            config,
        )
        .expect("Markdown analysis succeeds")
}

fn metric(analysis: &LanguageAnalysis, key: &str) -> f64 {
    analysis
        .root
        .metrics
        .get(&MetricKey::new(key))
        .unwrap_or_else(|| panic!("missing metric {key}"))
        .as_f64()
}

const FIXTURE: &str = "\
# Guide

Intro prose with a [broken relative link](missing/file.md) and a
[broken anchor](#nowhere).

### Skipped Level

More prose under the skipped heading.
";

#[test]
fn broken_links_evidence_sums_to_the_published_count() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let broken_evidence: Vec<_> = analysis
        .contributions
        .iter()
        .filter(|item| item.metric.as_str() == "markdown.links.broken")
        .collect();
    assert_eq!(
        broken_evidence.iter().map(|item| item.amount).sum::<f64>(),
        metric(&analysis, "markdown.links.broken"),
    );
    assert_eq!(broken_evidence.len(), 2);

    let reasons: Vec<&str> = broken_evidence
        .iter()
        .map(|item| item.reason.as_str())
        .collect();
    assert!(reasons.contains(&"markdown.broken_link.relative"));
    assert!(reasons.contains(&"markdown.broken_link.internal"));
}

#[test]
fn heading_skip_evidence_points_at_the_offending_heading() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    let skips: Vec<_> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "markdown.heading_skip")
        .collect();
    assert_eq!(skips.len(), 1);
    assert_eq!(
        skips[0].metric.as_str(),
        "markdown.complexity.cognitive_complexity"
    );
    assert_eq!(skips[0].amount, 1.0);
    // The `### Skipped Level` heading sits on line 6.
    assert_eq!(skips[0].span.start_line, 6);
}

#[test]
fn structure_evidence_covers_oversized_sections_and_paragraphs() {
    let filler = "word ".repeat(801);
    let source = format!("# Title\n\n{filler}\n");
    let analysis = analyze(&source, &AnalysisConfig::production());
    let reasons: Vec<&str> = analysis
        .contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();
    assert!(reasons.contains(&"markdown.oversized_flat_section"));
    assert!(reasons.contains(&"markdown.overlong_paragraph"));
    assert!(reasons.iter().all(|reason| reason.starts_with("markdown.")));
}

#[test]
fn spans_are_sane_and_source_ordered() {
    let analysis = analyze(FIXTURE, &AnalysisConfig::production());
    assert!(!analysis.contributions.is_empty());
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
        "markdown.links.broken",
        "markdown.complexity.cognitive_complexity",
        "markdown.maintainability.documentation_maintainability_index",
    ] {
        assert_eq!(
            metric(&production, key),
            metric(&benchmark, key),
            "evidence collection must not change `{key}`",
        );
    }
}
