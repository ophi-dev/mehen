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
    /// Rolled-up cyclomatic complexity (`Σ decisions + 1` per folded
    /// space) as published by the shared walker. Contribution evidence
    /// attaches here — the bare per-space key does not move when a
    /// nested function's complexity changes.
    pub const CYCLOMATIC_SUM: &str = "cyclomatic.sum";
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
    /// ABC bucket sub-keys, published by `mehen-metrics::state::publish_abc`
    /// and referenced by contribution evidence — shared so the two cannot
    /// drift apart.
    pub const ABC_ASSIGNMENTS: &str = "abc.assignments";
    pub const ABC_BRANCHES: &str = "abc.branches";
    pub const ABC_CONDITIONS: &str = "abc.conditions";
    pub const NARGS: &str = "nargs";
    pub const NOM: &str = "nom";
    /// NOM bucket sub-keys, shared between `state::publish_nom` and
    /// contribution evidence.
    pub const NOM_FUNCTIONS: &str = "nom.functions";
    pub const NOM_CLOSURES: &str = "nom.closures";
    pub const NEXIT: &str = "nexit";
    /// Rolled-up exit count across folded spaces — the aggregate that
    /// moves when a function gains an exit; contribution evidence
    /// attaches here.
    pub const NEXIT_SUM: &str = "nexit.sum";
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

    // Test-coverage metrics (`coverage.*` family). Report-scope:
    // published by the engine's coverage enrichment from ingested
    // coverage reports (LCOV, Cobertura, JaCoCo, …), not by language
    // analyzers. Rates are percentages in `0.0..=100.0`; counts are
    // covered/total pairs. A file absent from every report publishes
    // nothing — "unmeasured" must stay distinguishable from "0%".
    pub const COVERAGE_LINE: &str = "coverage.line";
    pub const COVERAGE_LINE_COVERED: &str = "coverage.line.covered";
    pub const COVERAGE_LINE_TOTAL: &str = "coverage.line.total";
    pub const COVERAGE_BRANCH: &str = "coverage.branch";
    pub const COVERAGE_BRANCH_COVERED: &str = "coverage.branch.covered";
    pub const COVERAGE_BRANCH_TOTAL: &str = "coverage.branch.total";
    pub const COVERAGE_FUNCTION: &str = "coverage.function";
    pub const COVERAGE_FUNCTION_COVERED: &str = "coverage.function.covered";
    pub const COVERAGE_FUNCTION_TOTAL: &str = "coverage.function.total";

    /// The complete `coverage.*` family — fixed, like
    /// [`HISTORY_ALL`], so selector/threshold typos are rejected up
    /// front instead of triggering report discovery and parsing only
    /// to read an unpublished key.
    pub const COVERAGE_ALL: &[&str] = &[
        COVERAGE_LINE,
        COVERAGE_LINE_COVERED,
        COVERAGE_LINE_TOTAL,
        COVERAGE_BRANCH,
        COVERAGE_BRANCH_COVERED,
        COVERAGE_BRANCH_TOTAL,
        COVERAGE_FUNCTION,
        COVERAGE_FUNCTION_COVERED,
        COVERAGE_FUNCTION_TOTAL,
    ];
}
