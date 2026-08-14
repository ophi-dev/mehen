// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use core::fmt;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// A metric identifier in mehen's open metric namespace.
///
/// The shared contract names a *minimum* metric set for source-code languages
/// (`cyclomatic`, `cognitive`, `halstead.volume`, …). Language analyzers may
/// publish additional keys under the same namespace (for example,
/// `cloudformation.iam_spcm`, `terraform.dependency_depth`,
/// `markdown.heading_skip`).
///
/// Keys are stored as `SmolStr` so common keys are inline and free of
/// allocation, while custom namespaced keys remain available without changing
/// the type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricKey(SmolStr);

impl MetricKey {
    pub fn new(key: impl Into<SmolStr>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for MetricKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl From<&str> for MetricKey {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for MetricKey {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// Stable string keys for the source-code minimum metric family. Language
/// analyzers should prefer these constants over ad-hoc string literals so that
/// renames stay in one place.
pub mod keys {
    pub const CYCLOMATIC: &str = "cyclomatic";
    pub const COGNITIVE: &str = "cognitive";
    pub const LOC: &str = "loc";
    pub const LOC_LLOC: &str = "loc.lloc";
    pub const LOC_SLOC: &str = "loc.sloc";
    pub const LOC_PLOC: &str = "loc.ploc";
    pub const LOC_CLOC: &str = "loc.cloc";
    pub const LOC_BLANK: &str = "loc.blank";
    pub const HALSTEAD: &str = "halstead";
    pub const HALSTEAD_VOLUME: &str = "halstead.volume";
    pub const HALSTEAD_DIFFICULTY: &str = "halstead.difficulty";
    pub const HALSTEAD_EFFORT: &str = "halstead.effort";
    pub const HALSTEAD_VOCABULARY: &str = "halstead.vocabulary";
    pub const HALSTEAD_LENGTH: &str = "halstead.length";
    pub const MI_VS: &str = "mi.visual_studio";
    pub const MI_ORIGINAL: &str = "mi.original";
    pub const MI_SEI: &str = "mi.sei";
    pub const ABC: &str = "abc";
    pub const NARGS: &str = "nargs";
    pub const NOM: &str = "nom";
    pub const NEXIT: &str = "nexit";
    pub const NPA: &str = "npa";
    pub const NPM: &str = "npm";
    pub const WMC: &str = "wmc";
    /// Rolled-up cognitive complexity as published onto the root
    /// `MetricSpace` by the shared walker.
    pub const COGNITIVE_SUM: &str = "cognitive.sum";

    // SQL- and Markdown-analyzer-owned keys referenced by the engine's
    // history composites (relative churn and hotspot read each
    // language family's equivalents of `loc.sloc` / `cognitive.sum`).
    pub const SQL_LOC_CODE: &str = "sql.loc.code";
    pub const SQL_COGNITIVE_COMPLEXITY: &str = "sql.cognitive_complexity";
    pub const MARKDOWN_LOC_TLOC: &str = "markdown.loc.tloc";
    pub const MARKDOWN_COGNITIVE_COMPLEXITY: &str = "markdown.complexity.cognitive_complexity";

    // Git history process metrics (`history.*` family, research
    // foundation §6). Repository-scope: published by the engine's
    // history enrichment, not by language analyzers.
    pub const HISTORY_CHURN_ABS: &str = "history.churn.abs";
    pub const HISTORY_CHURN_RELATIVE: &str = "history.churn.relative";
    pub const HISTORY_AGE_MONTHS: &str = "history.age_months";
    pub const HISTORY_AUTHORS: &str = "history.authors";
    pub const HISTORY_MINOR_CONTRIBUTORS: &str = "history.minor_contributors";
    pub const HISTORY_OWNERSHIP: &str = "history.ownership";
    pub const HISTORY_COMMIT_FREQUENCY: &str = "history.commit_frequency";
    pub const HISTORY_HOTSPOT: &str = "history.hotspot";
    pub const HISTORY_SUM_OF_COUPLING: &str = "history.sum_of_coupling";
    pub const HISTORY_TWR: &str = "history.twr";
    pub const HISTORY_BUGFIX_COMMITS: &str = "history.bugfix_commits";

    /// The complete `history.*` family. Unlike the extensible
    /// language-owned namespaces, these engine-published keys are
    /// fixed — selector parsing validates `history.*` names against
    /// this set so a typo is rejected up front instead of triggering
    /// the expensive repository walk and reading `0.0` through the
    /// missing-key fallback.
    pub const HISTORY_ALL: &[&str] = &[
        HISTORY_CHURN_ABS,
        HISTORY_CHURN_RELATIVE,
        HISTORY_AGE_MONTHS,
        HISTORY_AUTHORS,
        HISTORY_MINOR_CONTRIBUTORS,
        HISTORY_OWNERSHIP,
        HISTORY_COMMIT_FREQUENCY,
        HISTORY_HOTSPOT,
        HISTORY_SUM_OF_COUPLING,
        HISTORY_TWR,
        HISTORY_BUGFIX_COMMITS,
    ];
}
