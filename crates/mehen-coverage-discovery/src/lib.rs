// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Automatic, bounded discovery of coverage-report files.
//!
//! Coverage artifacts live exactly where mehen's source walk refuses to
//! look: gitignored directories (`coverage/`, `target/`, `build/`,
//! `TestResults/`) and hidden ones (`.nyc_output/`). This crate runs a
//! *dedicated* walk with the inverse policy — every ignore rule off,
//! hidden entries visible — while staying bounded and deterministic:
//!
//! * an explicit prune list (`node_modules`, `vendor`, `.venv`, `.git`,
//!   caches) that no coverage tool writes reports into;
//! * targeted descent inside the two huge-but-relevant trees: only
//!   `target/{llvm-cov,tarpaulin,site}` and
//!   `build/{reports,logs,coverage}` are entered;
//! * hard caps on depth, directory entries, sniffed candidates,
//!   per-directory candidates, and report size;
//! * `sort_by_file_name` traversal + order-independent selection rules,
//!   so the outcome is byte-identical across runs and platforms.
//!
//! Two tiers feed one candidate pool (an explicit `--coverage <path>` is
//! handled by the caller and bypasses discovery entirely):
//!
//! 1. **Tool-config introspection** — *declarative* configs only:
//!    the c8/nyc JSON rc family, `pyproject.toml`
//!    (`[tool.coverage.xml|lcov] output`), and `phpunit.xml`/`.dist`
//!    report elements. Executable configs (`jest.config.ts`,
//!    `.simplecov`, Gradle DSL, Pester scripts) are never executed and
//!    never regex-scraped — their tools' default output locations are
//!    already in the artifact-scan pattern table.
//! 2. **Artifact scan** — well-known report names/locations, each
//!    candidate confirmed by `mehen_coverage::detect_format` content
//!    sniffing (first 4 KiB) before it is believed.
//!
//! Selection collapses the pool deterministically: canonical-path
//! dedupe, same-directory multi-format supersede (one Jest run writes
//! `lcov.info` + `coverage-final.json` + `clover.xml`; only the
//! highest-priority format survives), and newest-run-wins inside
//! `TestResults/<guid>/` re-run clusters.

#![deny(unsafe_code)]

mod introspect;
mod select;
mod walk;

use camino::{Utf8Path, Utf8PathBuf};
use mehen_coverage::CoverageFormat;
use serde::Serialize;

/// Where a discovered report came from — recorded for diagnostics and
/// used as the primary selection/sort tier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case", tag = "origin", content = "config")]
pub enum ReportOrigin {
    /// Named by a tool's own declarative configuration file.
    ToolConfig(Utf8PathBuf),
    /// Found by the artifact scan.
    Scan,
}

/// One validated coverage report the caller should parse.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredReport {
    /// Path as walked (root-joined), forward-slash separated.
    pub path: Utf8PathBuf,
    /// Sniffed format — already confirmed against file content.
    pub format: CoverageFormat,
    /// Which tier produced the candidate.
    pub origin: ReportOrigin,
    /// Report size in bytes.
    #[serde(skip)]
    pub size_bytes: u64,
    /// Filesystem mtime, when the platform provides one. Used by the
    /// caller for the warn-only staleness check against the HEAD commit
    /// time (source mtimes are meaningless after a CI clone; the report
    /// artifact's own mtime is the only surviving signal).
    #[serde(skip)]
    pub mtime: Option<std::time::SystemTime>,
}

/// Why a candidate was dropped. Recorded, not fatal — discovery never
/// fails a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum RejectReason {
    /// Matched a filename pattern but no format sniffer accepted it
    /// (expected for e.g. plain-text `coverage.txt` summaries).
    SniffMismatch,
    /// Larger than [`DiscoveryCaps::max_report_bytes`].
    TooLarge,
    /// Zero-byte file.
    Empty,
    /// A file symlink whose target escapes every walk root.
    SymlinkEscape,
    /// A same-directory sibling of a higher-priority format supersedes
    /// this report (they describe the same test run).
    Superseded(Utf8PathBuf),
    /// An older `TestResults/<run>/` sibling of the kept report.
    OlderRun(Utf8PathBuf),
    /// A tool config names a report location that is missing, escapes
    /// the root, or fails validation.
    ToolConfigPathInvalid(Utf8PathBuf),
    /// The per-directory candidate cap dropped this file.
    PerDirCandidateCap,
}

/// A dropped candidate plus the reason.
#[derive(Debug, Clone, Serialize)]
pub struct Rejected {
    pub path: Utf8PathBuf,
    #[serde(flatten)]
    pub reason: RejectReason,
}

/// Walk/selection observability counters and records, serialized under
/// the `coverage_discovery` key of mehen's JSON output.
#[derive(Debug, Default, Serialize)]
pub struct DiscoveryDiagnostics {
    /// Directory entries visited across all roots.
    pub dirents_visited: u64,
    /// Candidates that matched a pattern or a tool config.
    pub candidates_matched: u32,
    /// Dropped candidates, sorted by path.
    pub rejected: Vec<Rejected>,
    /// Which caps fired, if any (`dirents`, `candidates`, …).
    pub caps_hit: Vec<String>,
}

/// The discovery result: reports to parse plus diagnostics.
#[derive(Debug, Default, Serialize)]
pub struct DiscoveryOutcome {
    /// Validated reports, sorted by (origin tier, path).
    pub reports: Vec<DiscoveredReport>,
    pub diagnostics: DiscoveryDiagnostics,
}

/// Bounds converting pathological repositories from "hangs" into
/// "warns". All defaults are deliberate; see the crate docs.
#[derive(Debug, Clone)]
pub struct DiscoveryCaps {
    /// Maximum directory depth below each root. The deepest idiomatic
    /// artifact path is ~7 components
    /// (`packages/<p>/build/reports/jacoco/test/jacocoTestReport.xml`);
    /// 12 leaves monorepo headroom.
    pub max_depth: usize,
    /// Maximum directory entries visited per `discover` call.
    pub max_dirents: u64,
    /// Maximum candidates content-sniffed per call (each sniff is one
    /// 4 KiB read).
    pub max_candidates: u32,
    /// Maximum candidates accepted per directory — bounds `.nyc_output`
    /// shard floods; the walk is name-sorted, so the lexicographically
    /// first shards win deterministically.
    pub max_per_dir: u32,
    /// Maximum size of a single report file.
    pub max_report_bytes: u64,
}

impl Default for DiscoveryCaps {
    fn default() -> Self {
        Self {
            max_depth: 12,
            max_dirents: 500_000,
            max_candidates: 256,
            max_per_dir: 64,
            max_report_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Input to [`discover`].
#[derive(Debug, Default)]
pub struct DiscoveryOptions {
    /// Directories to scan — typically one repository workdir per
    /// analysis root, canonicalized and deduplicated by the caller.
    pub roots: Vec<Utf8PathBuf>,
    /// Additive scan globs from configuration (matched relative to each
    /// root). A pattern whose first component is a literal directory
    /// name also lifts that name from the prune list, so
    /// `node_modules/.cache/**/lcov.info` actually reaches its target.
    pub extra_patterns: Vec<String>,
    /// Bounds; `Default::default()` for the documented caps.
    pub caps: DiscoveryCaps,
}

/// Discover coverage reports under the given roots.
///
/// Never fails: I/O problems, malformed configs, and cap overruns
/// degrade to [`DiscoveryDiagnostics`]. An empty outcome means "no
/// coverage available", which callers must keep distinct from 0%.
#[must_use]
pub fn discover(options: &DiscoveryOptions) -> DiscoveryOutcome {
    let mut diagnostics = DiscoveryDiagnostics::default();
    let mut candidates: Vec<select::Candidate> = Vec::new();

    // Deduplicate + order roots for deterministic multi-root budgets.
    let mut roots: Vec<&Utf8Path> = options
        .roots
        .iter()
        .map(Utf8PathBuf::as_path)
        .filter(|root| {
            let keep = root.is_dir();
            if !keep {
                log::warn!("coverage discovery root is not a directory, skipping: {root}");
            }
            keep
        })
        .collect();
    roots.sort_unstable();
    roots.dedup();

    // Tier 1: declarative tool-config introspection at each root.
    for root in &roots {
        introspect::introspect_root(root, &options.caps, &mut candidates, &mut diagnostics);
    }

    // Tier 2: bounded artifact scan.
    walk::scan_roots(
        &roots,
        &options.extra_patterns,
        &options.caps,
        &mut candidates,
        &mut diagnostics,
    );

    let reports = select::select(candidates, &mut diagnostics);
    diagnostics.rejected.sort_by(|a, b| a.path.cmp(&b.path));

    DiscoveryOutcome {
        reports,
        diagnostics,
    }
}
