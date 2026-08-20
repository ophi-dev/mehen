// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-markdown` — Markdown documentation metrics analyzer.
//!
//! Per the rewrite plan §4.7 / §6.6 / §8.1, the pre-1.0
//! `src/markdown/**` analyzer was physically moved here intact. The
//! high-level entry point [`analyze_markdown`] retains its original
//! signature so existing fixtures and snapshot tests pass without
//! modification.
//!
//! Markdown parsing is centered on pulldown-cmark events. Metric passes that
//! need document semantics consume `document`; passes that need nested byte
//! spans use the compact structural tree fed by the same event stream.
//!
//! Embedded-code analysis is supplied by `mehen-engine` through
//! [`set_embedded_dispatch`], keeping this crate focused on Markdown
//! metrics instead of depending on each source-language analyzer crate.

#![allow(clippy::upper_case_acronyms)]

mod analyzer;
mod artifact_debt;
mod code_burden;
pub mod diagrams;
mod dmi;
mod document;
mod ecu;
mod embedded_code;
mod evidence;
mod filler;
mod good_scaffold;
mod grounding;
mod halstead;
mod kind;
mod links;
mod loc;
mod math_burden;
mod mathops;
mod mcc;
mod mrpc;
mod nearby;
pub mod prose;
mod rci;
mod section_balance;
mod sections;
mod source_text;
mod syntax_tree;
mod tables;
mod tree_helpers;
pub mod types;
mod visuals;
mod words;

pub use analyzer::analyze_markdown;
use analyzer::analyze_markdown_with_evidence;
pub use embedded_code::{EmbeddedFenceMetrics, FenceLanguage, set_embedded_dispatch};

use mehen_core::{
    AnalysisBackend, AnalysisConfig, ContributionCollector, Language, LanguageAnalysis,
    LanguageAnalyzer, MetricKey, MetricSet, MetricSpace, SourceFile, SourceSpan, SpaceId,
    SpaceKind, byte_offset_clamped,
};

use crate::types::{LinkClass, MarkdownMetrics};

/// Pulldown-cmark-backed Markdown analyzer for the engine registry.
///
/// The rich metric pipeline runs through [`analyze_markdown`] and
/// produces [`types::MarkdownMetrics`] (a Markdown-specific report
/// shape). This `LanguageAnalyzer::analyze` implementation drives that
/// same pipeline and translates the headline numbers (LOC family,
/// size, complexity, Halstead, links, visuals, tables, maintainability,
/// grounding, AI-era risk, review criticality) into the shared
/// `MetricSet` flat-key shape so `mehen metrics README.md` returns
/// real values instead of an empty space.
pub struct MarkdownAnalyzer;

impl MarkdownAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MarkdownAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for MarkdownAnalyzer {
    fn language(&self) -> Language {
        Language::Markdown
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::PulldownCmark
    }

    fn analyze(
        &self,
        source: &SourceFile,
        config: &AnalysisConfig,
    ) -> mehen_core::Result<LanguageAnalysis> {
        let span = SourceSpan {
            start_byte: 0,
            end_byte: byte_offset_clamped(source.text.len()),
            start_line: 1,
            end_line: source.line_index.line_count(),
        };
        let mut evidence = ContributionCollector::new(config.emit_contributions);
        let metrics =
            analyze_markdown_with_evidence(&source.text, source.path.as_std_path(), &mut evidence);
        record_broken_link_evidence(&metrics, source, &mut evidence);
        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, span);
        publish_markdown_metrics(&metrics, &mut root.metrics);
        Ok(LanguageAnalysis {
            language: Language::Markdown,
            backend: AnalysisBackend::PulldownCmark,
            diagnostics: Vec::new(),
            root,
            contributions: evidence.finish(),
        })
    }
}

/// Record one contribution per broken link (plan §5.4 / research §39.4):
/// each `LinkRecord` whose resolution failed adds +1 toward the published
/// `markdown.links.broken` count, spanning the link's source line. The
/// aggregate in `links.rs` counts exactly the `resolved == Some(false)`
/// records, so the evidence amounts sum to the metric by construction.
fn record_broken_link_evidence(
    metrics: &MarkdownMetrics,
    source: &SourceFile,
    evidence: &mut ContributionCollector,
) {
    if !evidence.is_enabled() {
        return;
    }
    let total_len = byte_offset_clamped(source.text.len());
    for record in &metrics.link_records {
        if !matches!(record.resolved, Some(false)) {
            continue;
        }
        let line = u32::try_from(record.line).unwrap_or(u32::MAX);
        let (start_byte, end_byte) = source
            .line_index
            .line_byte_range(line, total_len)
            .unwrap_or((0, 0));
        let span = SourceSpan {
            start_byte,
            end_byte,
            start_line: line,
            end_line: line,
        };
        evidence.record(
            "markdown.links.broken",
            span,
            1.0,
            format!("markdown.broken_link.{}", link_class_label(record.class)),
        );
    }
}

/// The snake_case label a [`LinkClass`] serializes as — reused for
/// broken-link evidence reason codes.
fn link_class_label(class: LinkClass) -> &'static str {
    match class {
        LinkClass::Internal => "internal",
        LinkClass::Relative => "relative",
        LinkClass::AbsoluteSameRepo => "absolute_same_repo",
        LinkClass::External => "external",
        LinkClass::ExternalVendor => "external_vendor",
        LinkClass::Scholarly => "scholarly",
        LinkClass::IssuePr => "issue_pr",
        LinkClass::Footnote => "footnote",
        LinkClass::UnresolvedReferenceUse => "unresolved_reference_use",
        LinkClass::ReferenceDefinition => "reference_definition",
    }
}

/// Publish the headline `MarkdownMetrics` numbers into the shared
/// `MetricSet` flat-key shape that the `mehen metrics --format json`
/// envelope serializes. Keys mirror the §23 export schema documented
/// in `docs/metrics/markdown/overview.mdx` so the registry-driven
/// path returns the same numeric values that `analyze_markdown` does.
///
/// The full `MarkdownMetrics` record (sections, link records, prose
/// detail, etc.) is intentionally not flattened — those live in the
/// Markdown-specific report shape that the diff renderer in
/// `mehen-report::github_markdown_docs` consumes directly. Flat-key
/// publishing covers the scalar headline values consumed by the CLI's
/// JSON output and any threshold/selector lookup.
fn publish_markdown_metrics(m: &MarkdownMetrics, target: &mut MetricSet) {
    // LOC family (§5).
    target.insert(MetricKey::new("markdown.loc.dloc"), m.loc.dloc);
    target.insert(MetricKey::new("markdown.loc.ploc"), m.loc.ploc);
    target.insert(MetricKey::new("markdown.loc.cloc"), m.loc.cloc);
    target.insert(MetricKey::new("markdown.loc.tloc"), m.loc.tloc);
    target.insert(MetricKey::new("markdown.loc.mloc"), m.loc.mloc);
    target.insert(MetricKey::new("markdown.loc.bloc"), m.loc.bloc);
    target.insert(MetricKey::new("markdown.loc.aloc"), m.loc.aloc);

    // LOC ratios (§5.1).
    target.insert(
        MetricKey::new("markdown.loc_ratios.artifact_line_ratio"),
        m.loc_ratios.artifact_line_ratio,
    );
    target.insert(
        MetricKey::new("markdown.loc_ratios.code_line_ratio"),
        m.loc_ratios.code_line_ratio,
    );
    target.insert(
        MetricKey::new("markdown.loc_ratios.table_line_ratio"),
        m.loc_ratios.table_line_ratio,
    );
    target.insert(
        MetricKey::new("markdown.loc_ratios.math_line_ratio"),
        m.loc_ratios.math_line_ratio,
    );
    target.insert(
        MetricKey::new("markdown.loc_ratios.blank_line_ratio"),
        m.loc_ratios.blank_line_ratio,
    );

    // Size (§4 / §6).
    target.insert(MetricKey::new("markdown.size.words"), m.size.words);
    target.insert(
        MetricKey::new("markdown.size.effective_content_units"),
        m.size.effective_content_units,
    );
    target.insert(MetricKey::new("markdown.size.sections"), m.size.sections);
    target.insert(MetricKey::new("markdown.size.headings"), m.size.headings);

    // Complexity (§7 / §8 / §9).
    target.insert(
        MetricKey::new("markdown.complexity.reading_path_complexity"),
        m.complexity.reading_path_complexity,
    );
    target.insert(
        MetricKey::new("markdown.complexity.reading_path_complexity_raw"),
        m.complexity.reading_path_complexity_raw,
    );
    target.insert(
        MetricKey::new("markdown.complexity.cognitive_complexity"),
        m.complexity.cognitive_complexity,
    );

    // Halstead under complexity (§9).
    let h = &m.complexity.halstead;
    target.insert(
        MetricKey::new("markdown.halstead.operators_distinct"),
        h.operators_distinct,
    );
    target.insert(
        MetricKey::new("markdown.halstead.operators_total"),
        h.operators_total,
    );
    target.insert(
        MetricKey::new("markdown.halstead.operands_distinct"),
        h.operands_distinct,
    );
    target.insert(
        MetricKey::new("markdown.halstead.operands_total"),
        h.operands_total,
    );
    target.insert(MetricKey::new("markdown.halstead.vocabulary"), h.vocabulary);
    target.insert(MetricKey::new("markdown.halstead.length"), h.length);
    target.insert(MetricKey::new("markdown.halstead.volume"), h.volume);
    target.insert(MetricKey::new("markdown.halstead.difficulty"), h.difficulty);
    target.insert(MetricKey::new("markdown.halstead.effort"), h.effort);
    target.insert(
        MetricKey::new("markdown.halstead.embedded_volume"),
        h.embedded_volume,
    );
    target.insert(
        MetricKey::new("markdown.halstead.total_volume"),
        h.total_volume,
    );

    // Links (§11).
    target.insert(MetricKey::new("markdown.links.total"), m.links.total);
    target.insert(MetricKey::new("markdown.links.broken"), m.links.broken);
    target.insert(
        MetricKey::new("markdown.links.link_debt_score"),
        m.links.link_debt_score,
    );
    target.insert(
        MetricKey::new("markdown.links.information_scent_score"),
        m.links.information_scent_score,
    );
    target.insert(
        MetricKey::new("markdown.links.review_burden"),
        m.links.review_burden,
    );

    // Visuals (§12).
    target.insert(MetricKey::new("markdown.visuals.images"), m.visuals.images);
    target.insert(
        MetricKey::new("markdown.visuals.diagrams"),
        m.visuals.diagrams,
    );
    target.insert(
        MetricKey::new("markdown.visuals.diagram_parse_error_count"),
        m.visuals.diagram_parse_error_count,
    );
    target.insert(
        MetricKey::new("markdown.visuals.visual_net_effect"),
        m.visuals.visual_net_effect,
    );

    // Tables (§13).
    target.insert(MetricKey::new("markdown.tables.count"), m.tables.count);
    target.insert(
        MetricKey::new("markdown.tables.max_cells"),
        m.tables.max_cells,
    );
    target.insert(
        MetricKey::new("markdown.tables.table_burden_score"),
        m.tables.table_burden_score,
    );
    target.insert(
        MetricKey::new("markdown.tables.hard_warnings"),
        m.tables.hard_warnings,
    );

    // Maintainability (§10 / §19 / §20 / §21).
    target.insert(
        MetricKey::new("markdown.maintainability.documentation_maintainability_index"),
        m.maintainability.documentation_maintainability_index,
    );
    target.insert(
        MetricKey::new("markdown.maintainability.section_balance_score"),
        m.maintainability.section_balance_score,
    );
    target.insert(
        MetricKey::new("markdown.maintainability.good_scaffold_score"),
        m.maintainability.good_scaffold_score,
    );
    target.insert(
        MetricKey::new("markdown.maintainability.artifact_debt_score"),
        m.maintainability.artifact_debt_score,
    );

    // Grounding (§15 / §16).
    target.insert(
        MetricKey::new("markdown.grounding.repository_grounding_score"),
        m.grounding.repository_grounding_score,
    );
    target.insert(
        MetricKey::new("markdown.grounding.evidence_coverage_score"),
        m.grounding.evidence_coverage_score,
    );

    // AI era (§17) and review (§18).
    target.insert(
        MetricKey::new("markdown.ai_era.filler_lazy_structure_risk"),
        m.ai_era.filler_lazy_structure_risk,
    );
    target.insert(
        MetricKey::new("markdown.review.review_criticality_index"),
        m.review.review_criticality_index,
    );
}

/// Every metric key the Markdown analyzer can publish, for
/// configuration validation and typo suggestions.
///
/// Kept honest by `published_key_catalogue_is_in_sync` in the tests
/// below, which analyzes a feature-rich document and asserts every
/// published key validates.
pub const PUBLISHED_METRIC_KEYS: &[&str] = &[
    "markdown.ai_era.filler_lazy_structure_risk",
    "markdown.complexity.cognitive_complexity",
    "markdown.complexity.reading_path_complexity",
    "markdown.complexity.reading_path_complexity_raw",
    "markdown.grounding.evidence_coverage_score",
    "markdown.grounding.repository_grounding_score",
    "markdown.halstead.difficulty",
    "markdown.halstead.effort",
    "markdown.halstead.embedded_volume",
    "markdown.halstead.length",
    "markdown.halstead.operands_distinct",
    "markdown.halstead.operands_total",
    "markdown.halstead.operators_distinct",
    "markdown.halstead.operators_total",
    "markdown.halstead.total_volume",
    "markdown.halstead.vocabulary",
    "markdown.halstead.volume",
    "markdown.links.broken",
    "markdown.links.information_scent_score",
    "markdown.links.link_debt_score",
    "markdown.links.review_burden",
    "markdown.links.total",
    "markdown.loc.aloc",
    "markdown.loc.bloc",
    "markdown.loc.cloc",
    "markdown.loc.dloc",
    "markdown.loc.mloc",
    "markdown.loc.ploc",
    "markdown.loc.tloc",
    "markdown.loc_ratios.artifact_line_ratio",
    "markdown.loc_ratios.blank_line_ratio",
    "markdown.loc_ratios.code_line_ratio",
    "markdown.loc_ratios.math_line_ratio",
    "markdown.loc_ratios.table_line_ratio",
    "markdown.maintainability.artifact_debt_score",
    "markdown.maintainability.documentation_maintainability_index",
    "markdown.maintainability.good_scaffold_score",
    "markdown.maintainability.section_balance_score",
    "markdown.review.review_criticality_index",
    "markdown.size.effective_content_units",
    "markdown.size.headings",
    "markdown.size.sections",
    "markdown.size.words",
    "markdown.tables.count",
    "markdown.tables.hard_warnings",
    "markdown.tables.max_cells",
    "markdown.tables.table_burden_score",
    "markdown.visuals.diagram_parse_error_count",
    "markdown.visuals.diagrams",
    "markdown.visuals.images",
    "markdown.visuals.visual_net_effect",
];

/// Whether the Markdown analyzer can publish `name` onto a
/// `MetricSpace`. Used by `mehen.toml` threshold validation so a typo
/// like `markdown.links.borken` is rejected at load time instead of
/// becoming a gate that can never fire.
pub fn is_published_metric_key(name: &str) -> bool {
    PUBLISHED_METRIC_KEYS.contains(&name)
}
