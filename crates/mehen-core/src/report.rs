// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, MetricContribution,
    MetricSelector, MetricSpace, ParseDiagnostic, SourceFile, SourceSpan, SpaceId, SpaceKind,
    Threshold, ThresholdViolation,
};

/// Inputs to `analyze_metrics`.
#[derive(Clone, Debug)]
pub struct AnalyzeMetricsInput {
    pub source: SourceFile,
    pub config: AnalysisConfig,
}

/// `mehen metrics` JSON output shape (rewrite plan §9.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricsReport {
    pub schema_version: String,
    pub tool: String,
    pub path: Utf8PathBuf,
    pub language: Language,
    pub analysis_backend: AnalysisBackend,
    pub diagnostics: Vec<ParseDiagnostic>,
    pub root: MetricSpace,
    /// Source-resolved evidence emitted by the analyzer. Empty for analyzers
    /// or profiles that do not request contribution collection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributions: Vec<MetricContribution>,
}

impl MetricsReport {
    pub fn empty() -> Self {
        // Used as the seed shape in tests / docs. Production callers go
        // through `From<LanguageAnalysis>`.
        Self {
            schema_version: "1.0".to_string(),
            tool: "mehen".to_string(),
            path: Utf8PathBuf::new(),
            language: Language::Markdown,
            analysis_backend: AnalysisBackend::TreeSitter,
            diagnostics: Vec::new(),
            root: MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty()),
            contributions: Vec::new(),
        }
    }
}

impl From<LanguageAnalysis> for MetricsReport {
    fn from(analysis: LanguageAnalysis) -> Self {
        Self {
            schema_version: "1.0".to_string(),
            tool: "mehen".to_string(),
            path: Utf8PathBuf::new(),
            language: analysis.language,
            analysis_backend: analysis.backend,
            diagnostics: analysis.diagnostics,
            root: analysis.root,
            contributions: analysis.contributions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContributionReason, MetricContribution, MetricKey};

    #[test]
    fn language_analysis_conversion_preserves_contributions() {
        let contribution = MetricContribution {
            metric: MetricKey::new("sql.change_risk_score"),
            span: SourceSpan::new(0, 4, 1, 1),
            amount: 8.0,
            reason: ContributionReason::new("sql.change_risk.drop"),
        };
        let analysis = LanguageAnalysis {
            language: Language::Sql,
            backend: AnalysisBackend::Sqruff,
            diagnostics: Vec::new(),
            root: MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty()),
            contributions: vec![contribution.clone()],
        };

        let report = MetricsReport::from(analysis);
        assert_eq!(report.contributions, vec![contribution]);
    }
}

/// Inputs to `analyze_diff`.
///
/// Changed files marked `linguist-generated`, `linguist-vendored`, or
/// `binary` by Git attributes are excluded from analysis. Added and modified
/// files use attributes from the requested head revision; deleted files use
/// attributes from the base revision where they still exist.
#[derive(Clone, Debug)]
pub struct DiffInput {
    pub from: String,
    pub to: String,
    pub paths: Vec<Utf8PathBuf>,
    pub thresholds: Vec<Threshold>,
    pub config: AnalysisConfig,
}

/// `mehen diff --format json` output shape (rewrite plan §9.2).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiffReport {
    pub schema_version: String,
    pub base: String,
    pub head: String,
    pub files: Vec<DiffFile>,
    pub markdown_files: Vec<DiffFile>,
    pub analysis_errors: Vec<AnalysisErrorRecord>,
    pub threshold_violations: Vec<ThresholdViolation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: Utf8PathBuf,
    // Phase 5 fills in metric deltas. Kept skeletal here so diff JSON has a
    // documented shape even before the orchestrator lands.
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisErrorRecord {
    pub path: Utf8PathBuf,
    pub side: DiffSide,
    pub diagnostics: Vec<ParseDiagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffSide {
    Base,
    Head,
}

/// Inputs to `rank_top_offenders`.
///
/// Directory paths are traversed with standard ignore rules: hidden entries,
/// `.ignore`, `.gitignore`, `.git/info/exclude`, parent rules, and the user's
/// global Git excludes are respected. Files marked `linguist-generated`,
/// `linguist-vendored`, or `binary` by Git attributes are also excluded. An
/// explicitly supplied file path is still analyzed even when a default ignore
/// rule matches it.
#[derive(Clone, Debug)]
pub struct TopOffendersInput {
    pub paths: Vec<Utf8PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub selectors: Vec<MetricSelector>,
    pub max_results: usize,
    pub config: AnalysisConfig,
}

/// `mehen top-offenders --format json` output shape.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TopOffendersReport {
    pub schema_version: String,
    pub selectors: Vec<String>,
    pub entries: Vec<TopOffenderEntry>,
    /// Files dropped from the ranking with a non-fatal reason —
    /// e.g. the language was detected but no analyzer is registered
    /// (feature-gated build), or the analyzer returned a blocking
    /// diagnostic. Mirrors [`DiffReport::analysis_errors`] so callers
    /// can distinguish "no offenders" from "offenders silently
    /// skipped" (rewrite plan §3.5). `side` carries no real meaning
    /// here and is set to `Head` by convention.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub analysis_errors: Vec<AnalysisErrorRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopOffenderEntry {
    pub path: Utf8PathBuf,
    pub language: Language,
    /// One score per selector in [`TopOffendersInput::selectors`], in
    /// the same order. `scores[0]` is the primary ranking key; the rest
    /// break ties.
    pub scores: Vec<f64>,
}
