// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! The bounded artifact scan: a dedicated serial walk with the inverse
//! of mehen's source-walk policy (every ignore rule off, hidden entries
//! visible), an explicit prune list, targeted descent inside `target/`
//! and `build/`, and deterministic name-sorted traversal.

use std::collections::BTreeSet;
use std::io::Read;

use camino::{Utf8Path, Utf8PathBuf};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::select::Candidate;
use crate::{DiscoveryCaps, DiscoveryDiagnostics, RejectReason, Rejected, ReportOrigin};

/// Filename/location patterns for the artifact scan, matched against the
/// root-relative path. Breadth is cheap — every match is confirmed by
/// content sniffing before anything believes it — but each entry is
/// still justified by a real tool convention:
///
/// * LCOV: `lcov -o`, cargo-llvm-cov/tarpaulin, simplecov-lcov
///   (`coverage/lcov/<project>.lcov`), coverlet lcov (`coverage.info`),
///   coverage.py (`coverage.lcov`).
/// * Go: `go test -coverprofile` has *no* default filename; these are
///   the community's dominant spellings (`coverage.out`, `cover.out`,
///   `coverage.txt`, `profile.cov`, `c.out`, `*.coverprofile`) — all
///   guarded by the `mode:` content sniff.
/// * Istanbul: Jest/Vitest/nyc/c8 `coverage-final.json` plus raw
///   `.nyc_output/*.json` shards.
/// * JaCoCo: Maven `target/site/jacoco/jacoco.xml`, Gradle
///   `build/reports/jacoco/test/jacocoTestReport.xml`, Kotlin Kover's
///   JaCoCo-compatible `build/reports/kover/*.xml`, Pester 5's default
///   `coverage.xml` (JaCoCo format — content sniffing separates it from
///   coverage.py's Cobertura file of the same name).
/// * Clover: PHPUnit/Jenkins `clover.xml` (`build/logs/clover.xml`),
///   Jest/Vitest clover reporters.
/// * Cobertura: coverage.py `coverage.xml`, coverlet
///   `TestResults/<guid>/coverage.cobertura.xml`, gcovr/tarpaulin
///   `cobertura.xml`.
const ARTIFACT_PATTERNS: &[&str] = &[
    // LCOV
    "**/lcov.info",
    "**/coverage.info",
    "**/*.lcov",
    // Go coverprofile
    "**/coverage.out",
    "**/cover.out",
    "**/coverage.txt",
    "**/profile.cov",
    "**/c.out",
    "**/*.coverprofile",
    // Istanbul JSON
    "**/coverage-final.json",
    "**/.nyc_output/*.json",
    // JaCoCo XML (+ Pester's coverage.xml, sniff-disambiguated)
    "**/jacoco.xml",
    "**/jacocoTestReport.xml",
    "**/site/jacoco/*.xml",
    "**/reports/jacoco/**/*.xml",
    "**/reports/kover/*.xml",
    "**/coverage.xml",
    // Clover XML
    "**/clover.xml",
    // Cobertura XML
    "**/cobertura.xml",
    "**/coverage.cobertura.xml",
];

/// Directory names never descended into. No mainstream coverage tool
/// defaults report output into any of these, and several are enormous;
/// an `extra-patterns` entry whose first component names one lifts it
/// from the list for that run.
const PRUNE_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    "vendor",
    ".venv",
    "venv",
    ".tox",
    ".nox",
    ".direnv",
    "__pycache__",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    ".gradle",
    ".m2",
    ".cargo",
    ".rustup",
    ".npm",
    ".yarn",
    ".pnpm-store",
    ".idea",
    ".vscode",
    ".terraform",
];

/// Whether to descend into a directory, given its name and its parent's
/// name. Implements the prune list plus targeted descent for the two
/// huge-but-relevant build trees:
///
/// * `target/` (Cargo *and* Maven): only `llvm-cov/` (cargo-llvm-cov
///   HTML/artifacts), `tarpaulin/`, and `site/` (Maven
///   `target/site/jacoco/`) can contain reports — `target/debug` alone
///   is routinely 50k+ dirents of compiler output.
/// * `build/` (Gradle/CMake/Jenkins conventions): only `reports/`,
///   `logs/`, and `coverage/`.
/// * `coverage/tmp/` holds c8/nyc raw V8 output (never final reports).
fn should_descend(name: &str, parent_name: Option<&str>, prune: &BTreeSet<&str>) -> bool {
    if prune.contains(name) {
        return false;
    }
    match parent_name {
        Some("target") => matches!(name, "llvm-cov" | "tarpaulin" | "site"),
        Some("build") => matches!(name, "reports" | "logs" | "coverage"),
        Some("coverage") => name != "tmp",
        _ => true,
    }
}

/// Build the artifact-pattern matcher, appending configured extras.
/// Invalid or empty globs are dropped with a warning (mirroring the
/// engine's `mk_globset` behavior).
fn build_globset(extra_patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in ARTIFACT_PATTERNS
        .iter()
        .copied()
        .chain(extra_patterns.iter().map(String::as_str))
    {
        if pattern.is_empty() {
            continue;
        }
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(error) => log::warn!("invalid coverage scan pattern '{pattern}': {error}"),
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

/// The prune set for this run: the built-in list minus any directory
/// name that an extra pattern explicitly tunnels into (its first
/// literal component).
fn prune_set(extra_patterns: &[String]) -> BTreeSet<&'static str> {
    let mut prune: BTreeSet<&'static str> = PRUNE_DIRS.iter().copied().collect();
    for pattern in extra_patterns {
        if let Some(first) = pattern.split('/').next()
            && !first.contains(['*', '?', '[', '{'])
            && let Some(&name) = prune.iter().find(|&&p| p == first)
        {
            prune.remove(name);
        }
    }
    prune
}

/// Shared walk budget across roots.
struct Budget {
    dirents_left: u64,
    sniffs_left: u32,
}

/// Scan every root, feeding validated candidates into `candidates`.
pub(crate) fn scan_roots(
    roots: &[&Utf8Path],
    extra_patterns: &[String],
    caps: &DiscoveryCaps,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut DiscoveryDiagnostics,
) {
    let globset = build_globset(extra_patterns);
    let prune = prune_set(extra_patterns);
    let canonical_roots: Vec<std::path::PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root.as_std_path()).ok())
        .collect();
    let mut budget = Budget {
        dirents_left: caps.max_dirents,
        sniffs_left: caps.max_candidates,
    };
    // Per-directory accepted-candidate counter (bounds `.nyc_output`
    // shard floods deterministically — the walk is name-sorted).
    let mut per_dir: std::collections::BTreeMap<Utf8PathBuf, u32> =
        std::collections::BTreeMap::new();

    for root in roots {
        if budget.dirents_left == 0 || budget.sniffs_left == 0 {
            break;
        }
        scan_one_root(
            root,
            &globset,
            &prune,
            &canonical_roots,
            caps,
            &mut budget,
            &mut per_dir,
            candidates,
            diagnostics,
        );
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal walk plumbing; a context struct would only rename the coupling"
)]
fn scan_one_root(
    root: &Utf8Path,
    globset: &GlobSet,
    prune: &BTreeSet<&str>,
    canonical_roots: &[std::path::PathBuf],
    caps: &DiscoveryCaps,
    budget: &mut Budget,
    per_dir: &mut std::collections::BTreeMap<Utf8PathBuf, u32>,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut DiscoveryDiagnostics,
) {
    let mut builder = WalkBuilder::new(root.as_std_path());
    // The inverse of the source walk: gitignored directories (coverage/,
    // target/, TestResults/) and hidden entries (.nyc_output/) MUST be
    // visited, so every standard filter is off. Symlinked directories
    // are never followed (loops, escapes, nondeterminism).
    builder
        .standard_filters(false)
        .follow_links(false)
        .max_depth(Some(caps.max_depth))
        .sort_by_file_name(std::cmp::Ord::cmp);

    let prune_for_filter: BTreeSet<String> = prune.iter().map(ToString::to_string).collect();
    builder.filter_entry(move |entry| {
        if entry.depth() == 0
            || !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
        {
            return true;
        }
        let Some(name) = entry.file_name().to_str() else {
            return false; // non-UTF-8 directory: skip subtree
        };
        let parent_name = entry
            .path()
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        let prune_ref: BTreeSet<&str> = prune_for_filter.iter().map(String::as_str).collect();
        should_descend(name, parent_name, &prune_ref)
    });

    for result in builder.build() {
        if budget.dirents_left == 0 {
            push_cap(diagnostics, "dirents");
            return;
        }
        budget.dirents_left -= 1;

        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("coverage scan failed to walk an entry: {error}");
                continue;
            }
        };
        let is_file = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
            || (entry.path_is_symlink() && entry.path().is_file());
        if !is_file {
            continue;
        }
        let Some(utf8) = Utf8Path::from_path(entry.path()) else {
            log::warn!(
                "skipping non-UTF-8 coverage candidate path: {}",
                entry.path().display()
            );
            continue;
        };
        let Ok(relative) = utf8.strip_prefix(root) else {
            continue;
        };
        if !globset.is_match(relative.as_std_path()) {
            continue;
        }

        diagnostics.candidates_matched += 1;
        if budget.sniffs_left == 0 {
            push_cap(diagnostics, "candidates");
            return;
        }
        budget.sniffs_left -= 1;

        let parent = utf8
            .parent()
            .map_or_else(|| root.to_path_buf(), Utf8Path::to_path_buf);
        let dir_count = per_dir.entry(parent).or_insert(0);
        if *dir_count >= caps.max_per_dir {
            push_cap(diagnostics, "per_dir_candidates");
            diagnostics.rejected.push(Rejected {
                path: utf8.to_path_buf(),
                reason: RejectReason::PerDirCandidateCap,
            });
            continue;
        }

        match validate_candidate(utf8, ReportOrigin::Scan, canonical_roots, caps) {
            Ok(candidate) => {
                *dir_count += 1;
                candidates.push(candidate);
            }
            Err(reason) => diagnostics.rejected.push(Rejected {
                path: utf8.to_path_buf(),
                reason,
            }),
        }
    }
}

fn push_cap(diagnostics: &mut DiscoveryDiagnostics, cap: &str) {
    if !diagnostics.caps_hit.iter().any(|c| c == cap) {
        diagnostics.caps_hit.push(cap.to_string());
    }
}

/// Validate one candidate file: regular-file check, symlink-escape
/// containment, size bounds, and content sniffing. Shared by the
/// artifact scan and tool-config introspection.
pub(crate) fn validate_candidate(
    path: &Utf8Path,
    origin: ReportOrigin,
    canonical_roots: &[std::path::PathBuf],
    caps: &DiscoveryCaps,
) -> Result<Candidate, RejectReason> {
    let symlink_meta =
        std::fs::symlink_metadata(path.as_std_path()).map_err(|_| RejectReason::SniffMismatch)?;
    let canonical = std::fs::canonicalize(path.as_std_path()).ok();
    if symlink_meta.file_type().is_symlink() {
        // A planted `lcov.info -> /etc/passwd` must not be read even
        // for sniffing: the resolved target has to stay under a root.
        let contained = canonical.as_ref().is_some_and(|resolved| {
            canonical_roots
                .iter()
                .any(|root| resolved.starts_with(root))
        });
        if !contained {
            return Err(RejectReason::SymlinkEscape);
        }
    }

    let metadata = std::fs::metadata(path.as_std_path()).map_err(|_| RejectReason::Empty)?;
    if !metadata.is_file() {
        return Err(RejectReason::SniffMismatch);
    }
    if metadata.len() == 0 {
        return Err(RejectReason::Empty);
    }
    if metadata.len() > caps.max_report_bytes {
        return Err(RejectReason::TooLarge);
    }

    // The 4 KiB sniff — the only content I/O discovery performs.
    let mut head = [0_u8; 4096];
    let mut file = std::fs::File::open(path.as_std_path()).map_err(|_| RejectReason::Empty)?;
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    let Some(format) = mehen_coverage::detect_format(path, &head[..filled]) else {
        return Err(RejectReason::SniffMismatch);
    };

    Ok(Candidate {
        path: path.to_path_buf(),
        canonical: canonical.unwrap_or_else(|| path.as_std_path().to_path_buf()),
        format,
        origin,
        size_bytes: metadata.len(),
        mtime: metadata.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prune() -> BTreeSet<&'static str> {
        PRUNE_DIRS.iter().copied().collect()
    }

    #[test]
    fn prune_list_blocks_and_extra_pattern_lifts() {
        let base = prune();
        assert!(!should_descend("node_modules", Some("repo"), &base));
        assert!(!should_descend(".git", Some("repo"), &base));

        let lifted = prune_set(&["node_modules/.cache/**/lcov.info".to_string()]);
        assert!(should_descend("node_modules", Some("repo"), &lifted));
        assert!(!should_descend(".git", Some("repo"), &lifted));
    }

    #[test]
    fn target_and_build_use_targeted_descent() {
        let p = prune();
        assert!(should_descend("llvm-cov", Some("target"), &p));
        assert!(should_descend("tarpaulin", Some("target"), &p));
        assert!(should_descend("site", Some("target"), &p));
        assert!(!should_descend("debug", Some("target"), &p));
        assert!(!should_descend("release", Some("target"), &p));

        assert!(should_descend("reports", Some("build"), &p));
        assert!(should_descend("logs", Some("build"), &p));
        assert!(should_descend("coverage", Some("build"), &p));
        assert!(!should_descend("classes", Some("build"), &p));

        // c8/nyc raw V8 staging: never contains final reports.
        assert!(!should_descend("tmp", Some("coverage"), &p));
        assert!(should_descend("lcov", Some("coverage"), &p));
    }

    #[test]
    fn artifact_patterns_match_expected_paths() {
        let set = build_globset(&[]);
        for hit in [
            "lcov.info",
            "coverage/lcov.info",
            "packages/app/coverage/lcov.info",
            "coverage/lcov/my-project.lcov",
            "coverage.out",
            "profile.cov",
            "c.out",
            "e2e.coverprofile",
            "coverage/coverage-final.json",
            ".nyc_output/8a1f.json",
            "target/site/jacoco/jacoco.xml",
            "build/reports/jacoco/test/jacocoTestReport.xml",
            "build/reports/kover/report.xml",
            "coverage.xml",
            "sub/coverage.xml",
            "build/logs/clover.xml",
            "TestResults/3d1c-42/coverage.cobertura.xml",
            "target/llvm-cov/lcov.info",
        ] {
            assert!(set.is_match(hit), "expected pattern hit: {hit}");
        }
        for miss in [
            "notes.txt",
            "report.xml",           // only inside kover/jacoco dirs
            "docs/install.info.md", // not a coverage name
            "coverage.json",        // coverlet's proprietary JSON
            "src/lib.rs",
        ] {
            assert!(!set.is_match(miss), "expected pattern miss: {miss}");
        }
    }
}
