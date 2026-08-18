// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Deterministic selection over the candidate pool. Every rule is
//! order-independent or keyed on sorted data, so permuting candidate
//! arrival order can never change the outcome:
//!
//! 1. **Canonical dedupe** — one entry per on-disk file; `ToolConfig`
//!    origin outranks `Scan` for the same file.
//! 2. **Same-directory format supersede** — one test run often emits
//!    the same data in several formats at once (Jest: `lcov.info` +
//!    `coverage-final.json` + `clover.xml`); only the format highest in
//!    [`CoverageFormat::DETECTION_ORDER`] survives per directory.
//!    Parsing all three would triple cost to learn nothing.
//! 3. **`TestResults/<run>/` re-run clusters** — sibling run
//!    directories under a common `TestResults/` parent holding
//!    identically-named reports are re-runs of the same producer: the
//!    newest report mtime wins, lexicographically smallest path breaks
//!    ties. (Newest-*mtime*-wins is scoped to exactly this shape — git
//!    checkouts don't preserve mtimes, so a global newest-wins rule
//!    would be meaningless on fresh CI clones.)

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use mehen_coverage::CoverageFormat;

use crate::{DiscoveredReport, DiscoveryDiagnostics, RejectReason, Rejected, ReportOrigin};

/// A validated candidate awaiting selection.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub path: Utf8PathBuf,
    /// On-disk identity for deduplication.
    pub canonical: std::path::PathBuf,
    pub format: CoverageFormat,
    pub origin: ReportOrigin,
    pub size_bytes: u64,
    pub mtime: Option<std::time::SystemTime>,
}

/// Position in the detection priority order — lower is higher priority.
fn priority(format: CoverageFormat) -> usize {
    CoverageFormat::DETECTION_ORDER
        .iter()
        .position(|&f| f == format)
        .unwrap_or(usize::MAX)
}

pub(crate) fn select(
    candidates: Vec<Candidate>,
    diagnostics: &mut DiscoveryDiagnostics,
) -> Vec<DiscoveredReport> {
    // 1. Canonical dedupe. BTreeMap keys give deterministic iteration;
    //    for one file, ToolConfig origin wins, then the smaller walked
    //    path spelling.
    let mut by_identity: BTreeMap<std::path::PathBuf, Candidate> = BTreeMap::new();
    for candidate in candidates {
        match by_identity.entry(candidate.canonical.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let kept = slot.get();
                let replace = (candidate_rank(&candidate)) < (candidate_rank(kept));
                if replace {
                    slot.insert(candidate);
                }
            }
        }
    }
    let mut pool: Vec<Candidate> = by_identity.into_values().collect();
    pool.sort_by(|a, b| a.path.cmp(&b.path));

    // 2. Same-directory format supersede. First pass records, per
    //    directory, the best (lowest) format priority and the
    //    lexicographically first candidate carrying it; second pass
    //    drops everything with a worse format, attributing the keeper.
    let mut best_in_dir: BTreeMap<Utf8PathBuf, (usize, Utf8PathBuf)> = BTreeMap::new();
    for candidate in &pool {
        if let Some(parent) = candidate.path.parent() {
            let rank = priority(candidate.format);
            match best_in_dir.entry(parent.to_path_buf()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert((rank, candidate.path.clone()));
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    // The pool is path-sorted, so on equal rank the
                    // existing (earlier) path stays.
                    if rank < slot.get().0 {
                        slot.insert((rank, candidate.path.clone()));
                    }
                }
            }
        }
    }
    let mut survivors: Vec<Candidate> = Vec::with_capacity(pool.len());
    for candidate in pool {
        let best = candidate
            .path
            .parent()
            .and_then(|parent| best_in_dir.get(parent));
        match best {
            Some((rank, keeper)) if priority(candidate.format) > *rank => {
                diagnostics.rejected.push(Rejected {
                    path: candidate.path,
                    reason: RejectReason::Superseded(keeper.clone()),
                });
            }
            _ => survivors.push(candidate),
        }
    }

    // 3. TestResults re-run clusters. Key: (path of the TestResults
    //    ancestor, path relative to the run directory). The newest
    //    mtime wins; missing mtimes sort oldest; ties break on the
    //    lexicographically smallest path.
    let mut clusters: BTreeMap<(Utf8PathBuf, Utf8PathBuf), Vec<Candidate>> = BTreeMap::new();
    let mut unclustered: Vec<Candidate> = Vec::new();
    for candidate in survivors {
        match test_results_cluster_key(&candidate.path) {
            Some(key) => clusters.entry(key).or_default().push(candidate),
            None => unclustered.push(candidate),
        }
    }
    let mut selected = unclustered;
    for (_, mut cluster) in clusters {
        // Sort so the winner is first: newest mtime, then smallest path.
        cluster.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
        let mut iter = cluster.into_iter();
        let winner = iter.next().expect("cluster groups are never empty");
        let winner_path = winner.path.clone();
        selected.push(winner);
        for loser in iter {
            diagnostics.rejected.push(Rejected {
                path: loser.path,
                reason: RejectReason::OlderRun(winner_path.clone()),
            });
        }
    }

    // 4. Final deterministic order: origin tier, then path.
    selected.sort_by(|a, b| {
        origin_tier(&a.origin)
            .cmp(&origin_tier(&b.origin))
            .then_with(|| a.path.cmp(&b.path))
    });
    selected
        .into_iter()
        .map(|c| DiscoveredReport {
            path: c.path,
            format: c.format,
            origin: c.origin,
            size_bytes: c.size_bytes,
            mtime: c.mtime,
        })
        .collect()
}

/// Dedupe rank for two candidates naming the same on-disk file: lower
/// wins. Tool-config attribution beats the scan; then the smaller
/// walked spelling.
fn candidate_rank(candidate: &Candidate) -> (u8, &Utf8PathBuf) {
    (origin_tier(&candidate.origin), &candidate.path)
}

fn origin_tier(origin: &ReportOrigin) -> u8 {
    match origin {
        ReportOrigin::ToolConfig(_) => 0,
        ReportOrigin::Scan => 1,
    }
}

/// If the path has the shape `<prefix>/TestResults/<run>/<rest…>`,
/// return the cluster key `(prefix/TestResults, rest)`.
fn test_results_cluster_key(path: &camino::Utf8Path) -> Option<(Utf8PathBuf, Utf8PathBuf)> {
    let components: Vec<&str> = path.components().map(|c| c.as_str()).collect();
    // Find the *last* TestResults component with at least a run dir and
    // a file below it.
    let idx = components
        .iter()
        .rposition(|&c| c == "TestResults")
        .filter(|&idx| idx + 2 < components.len())?;
    let ancestor: Utf8PathBuf = components[..=idx].iter().collect();
    let rest: Utf8PathBuf = components[idx + 2..].iter().collect();
    Some((ancestor, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(path: &str, format: CoverageFormat) -> Candidate {
        Candidate {
            path: Utf8PathBuf::from(path),
            canonical: std::path::PathBuf::from(path),
            format,
            origin: ReportOrigin::Scan,
            size_bytes: 10,
            mtime: None,
        }
    }

    #[test]
    fn same_directory_multi_format_keeps_highest_priority() {
        // The Jest triple: one run, three artifacts, one survivor.
        let pool = vec![
            candidate("coverage/clover.xml", CoverageFormat::Clover),
            candidate("coverage/coverage-final.json", CoverageFormat::Istanbul),
            candidate("coverage/lcov.info", CoverageFormat::Lcov),
        ];
        let mut diagnostics = DiscoveryDiagnostics::default();
        let selected = select(pool, &mut diagnostics);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].path, "coverage/lcov.info");
        assert_eq!(selected[0].format, CoverageFormat::Lcov);
        assert_eq!(diagnostics.rejected.len(), 2);
        for rejected in &diagnostics.rejected {
            assert!(matches!(
                &rejected.reason,
                RejectReason::Superseded(kept) if kept == "coverage/lcov.info"
            ));
        }
    }

    #[test]
    fn same_format_same_directory_all_survive() {
        // simplecov-lcov per-file mode: many .lcov files in one dir —
        // they are disjoint parts of one run and all merge.
        let pool = vec![
            candidate("coverage/lcov/a.lcov", CoverageFormat::Lcov),
            candidate("coverage/lcov/b.lcov", CoverageFormat::Lcov),
        ];
        let mut diagnostics = DiscoveryDiagnostics::default();
        let selected = select(pool, &mut diagnostics);
        assert_eq!(selected.len(), 2);
        assert!(diagnostics.rejected.is_empty());
    }

    #[test]
    fn test_results_reruns_keep_newest() {
        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let new_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000);
        let mut older = candidate(
            "TestResults/aaaa/coverage.cobertura.xml",
            CoverageFormat::Cobertura,
        );
        older.mtime = Some(old_time);
        let mut newer = candidate(
            "TestResults/bbbb/coverage.cobertura.xml",
            CoverageFormat::Cobertura,
        );
        newer.mtime = Some(new_time);

        for permutation in [
            vec![older.clone(), newer.clone()],
            vec![newer.clone(), older.clone()],
        ] {
            let mut diagnostics = DiscoveryDiagnostics::default();
            let selected = select(permutation, &mut diagnostics);
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].path, "TestResults/bbbb/coverage.cobertura.xml");
            assert_eq!(diagnostics.rejected.len(), 1);
            assert!(matches!(
                &diagnostics.rejected[0].reason,
                RejectReason::OlderRun(kept) if kept == "TestResults/bbbb/coverage.cobertura.xml"
            ));
        }
    }

    #[test]
    fn multi_project_test_results_keep_one_per_assembly() {
        // Two projects, each with its own TestResults tree: separate
        // clusters, both survive.
        let a = candidate(
            "svc-a/TestResults/1111/coverage.cobertura.xml",
            CoverageFormat::Cobertura,
        );
        let b = candidate(
            "svc-b/TestResults/2222/coverage.cobertura.xml",
            CoverageFormat::Cobertura,
        );
        let mut diagnostics = DiscoveryDiagnostics::default();
        let selected = select(vec![a, b], &mut diagnostics);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn tool_config_origin_wins_dedupe_and_sorts_first() {
        let scan = candidate("build/logs/clover.xml", CoverageFormat::Clover);
        let mut config = candidate("build/logs/clover.xml", CoverageFormat::Clover);
        config.origin = ReportOrigin::ToolConfig(Utf8PathBuf::from("phpunit.xml"));

        for permutation in [
            vec![scan.clone(), config.clone()],
            vec![config.clone(), scan.clone()],
        ] {
            let mut diagnostics = DiscoveryDiagnostics::default();
            let selected = select(permutation, &mut diagnostics);
            assert_eq!(selected.len(), 1);
            assert!(
                matches!(&selected[0].origin, ReportOrigin::ToolConfig(c) if c == "phpunit.xml")
            );
        }
    }

    #[test]
    fn cluster_key_shapes() {
        assert_eq!(
            test_results_cluster_key(Utf8PathBuf::from("TestResults/x/coverage.xml").as_path()),
            Some((
                Utf8PathBuf::from("TestResults"),
                Utf8PathBuf::from("coverage.xml")
            ))
        );
        assert_eq!(
            test_results_cluster_key(
                Utf8PathBuf::from("a/TestResults/run-1/sub/coverage.info").as_path()
            ),
            Some((
                Utf8PathBuf::from("a/TestResults"),
                Utf8PathBuf::from("sub/coverage.info")
            ))
        );
        // A file directly inside TestResults/ has no run directory.
        assert_eq!(
            test_results_cluster_key(Utf8PathBuf::from("TestResults/coverage.xml").as_path()),
            None
        );
    }
}
