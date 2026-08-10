// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Engine-level enrichment publishing the `history.*` metric family
//! (research foundation §6) onto per-file root `MetricSpace`s.
//!
//! History metrics are repository-scope process metrics: they cannot
//! come from a `LanguageAnalyzer` (which only sees one file's
//! content), so the diff/top-offenders orchestrators compute one
//! [`mehen_git::RepositoryHistory`] per revision and fold the per-file
//! values into each file's metric set *after* static analysis. The
//! walk is comparatively expensive (one tree diff per commit), so
//! callers only trigger it when a `history.*` selector or threshold is
//! actually requested — see [`names_want_history`].
//!
//! Two keys are composites over the static suite and are therefore
//! computed here rather than in `mehen-git`:
//!
//! * `history.churn.relative` — absolute churn normalized by the
//!   file's SLOC at the same revision (Nagappan & Ball's
//!   defect-predictive *relative* churn, §6.1).
//! * `history.hotspot` — `cognitive.sum × commit_frequency` (§6.5):
//!   mehen upgrades the classic LOC×frequency hotspot with its real
//!   complexity metric.

use mehen_core::{MetricKey, MetricSet, keys};
use mehen_git::FileHistory;

/// Whether any requested metric name/key belongs to the `history.*`
/// family — the trigger for running the (comparatively expensive)
/// repository history walk.
pub(crate) fn names_want_history<'a>(mut names: impl Iterator<Item = &'a str>) -> bool {
    names.any(|name| name.starts_with("history."))
}

/// Publish the `history.*` family onto a file's root metric set.
///
/// `file` is the walked per-file history at the same revision the
/// metric set was computed from; `head_seconds` is that revision's
/// committer timestamp (the deterministic "now" for code age). Files
/// untouched by any walked commit publish nothing — selectors then
/// read the family as `0.0` via the missing-key fallback.
pub(crate) fn inject_history_metrics(
    metrics: &mut MetricSet,
    file: &FileHistory,
    head_seconds: i64,
) {
    let read = |metrics: &MetricSet, key: &str| {
        metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or(0.0)
    };
    // Composite inputs come from whichever family the analyzer
    // publishes: the shared source-code suite (`loc.sloc`,
    // `cognitive.sum`) or SQL's own namespace (`sql.loc.code`,
    // `sql.cognitive_complexity`). Without the SQL fallback, relative
    // churn would silently equal absolute churn and every SQL hotspot
    // would read zero (Codex P2).
    let read_first = |keys: &[&str]| {
        keys.iter()
            .map(|key| read(metrics, key))
            .find(|&v| v != 0.0)
            .unwrap_or(0.0)
    };
    // Relative churn normalizes by the file's current size; a file
    // whose analyzer published no (or zero) code-line count falls back
    // to a denominator of 1 so the value stays finite and deterministic.
    let sloc = read_first(&[keys::LOC_SLOC, keys::SQL_LOC_CODE]).max(1.0);
    let cognitive_sum = read_first(&[keys::COGNITIVE_SUM, keys::SQL_COGNITIVE_COMPLEXITY]);

    let churn_abs = file.churn_abs();
    metrics.insert(keys::HISTORY_CHURN_ABS, churn_abs);
    metrics.insert(keys::HISTORY_CHURN_RELATIVE, churn_abs as f64 / sloc);
    metrics.insert(keys::HISTORY_AGE_MONTHS, file.age_months(head_seconds));
    metrics.insert(keys::HISTORY_AUTHORS, file.authors);
    metrics.insert(keys::HISTORY_MINOR_CONTRIBUTORS, file.minor_contributors);
    metrics.insert(keys::HISTORY_OWNERSHIP, file.ownership);
    metrics.insert(keys::HISTORY_COMMIT_FREQUENCY, file.commit_frequency);
    metrics.insert(
        keys::HISTORY_HOTSPOT,
        cognitive_sum * file.commit_frequency as f64,
    );
    metrics.insert(keys::HISTORY_SUM_OF_COUPLING, file.sum_of_coupling);
    metrics.insert(keys::HISTORY_TWR, file.twr);
    metrics.insert(keys::HISTORY_BUGFIX_COMMITS, file.bugfix_commits);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_history() -> FileHistory {
        FileHistory {
            commit_frequency: 4,
            churn_added: 30,
            churn_removed: 10,
            authors: 2,
            minor_contributors: 1,
            ownership: 0.75,
            last_change_seconds: 0,
            sum_of_coupling: 3,
            bugfix_commits: 2,
            twr: 0.5,
        }
    }

    fn read(metrics: &MetricSet, key: &str) -> f64 {
        metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or_else(|| panic!("missing key {key}"))
    }

    #[test]
    fn names_want_history_detects_family_keys() {
        assert!(names_want_history(
            ["cognitive", "history.churn.abs"].into_iter()
        ));
        assert!(!names_want_history(
            ["cognitive", "loc.lloc", "sql.change_risk_score"].into_iter()
        ));
        assert!(!names_want_history(std::iter::empty()));
    }

    #[test]
    fn injects_all_eleven_family_keys() {
        let mut metrics = MetricSet::default();
        metrics.insert("loc.sloc", 20.0);
        metrics.insert("cognitive.sum", 8.0);
        inject_history_metrics(&mut metrics, &sample_history(), 2_629_746);

        assert_eq!(read(&metrics, keys::HISTORY_CHURN_ABS), 40.0);
        // 40 churned lines over 20 SLOC.
        assert_eq!(read(&metrics, keys::HISTORY_CHURN_RELATIVE), 2.0);
        // One average month since last change.
        assert!((read(&metrics, keys::HISTORY_AGE_MONTHS) - 1.0).abs() < 1e-9);
        assert_eq!(read(&metrics, keys::HISTORY_AUTHORS), 2.0);
        assert_eq!(read(&metrics, keys::HISTORY_MINOR_CONTRIBUTORS), 1.0);
        assert_eq!(read(&metrics, keys::HISTORY_OWNERSHIP), 0.75);
        assert_eq!(read(&metrics, keys::HISTORY_COMMIT_FREQUENCY), 4.0);
        // cognitive.sum (8) × commit_frequency (4).
        assert_eq!(read(&metrics, keys::HISTORY_HOTSPOT), 32.0);
        assert_eq!(read(&metrics, keys::HISTORY_SUM_OF_COUPLING), 3.0);
        assert_eq!(read(&metrics, keys::HISTORY_TWR), 0.5);
        assert_eq!(read(&metrics, keys::HISTORY_BUGFIX_COMMITS), 2.0);
    }

    #[test]
    fn missing_static_metrics_keep_composites_finite() {
        // No loc.sloc / cognitive.sum published (e.g. analyzer without
        // those families): relative churn divides by 1, hotspot is 0.
        let mut metrics = MetricSet::default();
        inject_history_metrics(&mut metrics, &sample_history(), 0);
        assert_eq!(read(&metrics, keys::HISTORY_CHURN_RELATIVE), 40.0);
        assert_eq!(read(&metrics, keys::HISTORY_HOTSPOT), 0.0);
    }

    #[test]
    fn sql_files_use_their_own_namespace_for_composites() {
        // The SQL analyzer publishes `sql.loc.code` / `sql.cognitive_complexity`
        // instead of `loc.sloc` / `cognitive.sum`; the composites must read
        // those so SQL relative churn and hotspots aren't degenerate.
        let mut metrics = MetricSet::default();
        metrics.insert("sql.loc.code", 10.0);
        metrics.insert("sql.cognitive_complexity", 5.0);
        inject_history_metrics(&mut metrics, &sample_history(), 0);
        // 40 churned lines over 10 SQL code lines.
        assert_eq!(read(&metrics, keys::HISTORY_CHURN_RELATIVE), 4.0);
        // sql.cognitive_complexity (5) × commit_frequency (4).
        assert_eq!(read(&metrics, keys::HISTORY_HOTSPOT), 20.0);
    }
}
