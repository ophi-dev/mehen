// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Engine-level enrichment publishing the `coverage.*` metric family
//! onto per-file `MetricSpace`s from ingested coverage reports.
//!
//! Coverage metrics are report-scope: they cannot come from a
//! `LanguageAnalyzer` (which only sees one file's content), so the
//! orchestrators fold per-file coverage into each file's metric set
//! *after* static analysis — the same enrichment slot the `history.*`
//! family occupies. Ingestion (discovery, parsing, merging, path
//! matching) is comparatively expensive, so callers only trigger it
//! when coverage is actually requested — see [`names_want_coverage`]
//! and [`CoverageOpts`].
//!
//! Unlike history, coverage is also injected **per function space**:
//! every `Function`/`Closure` space whose line span intersects the
//! report's line records receives its own `coverage.line` /
//! `coverage.branch` keys. That per-function attribution is what a
//! CRAP composite (`cyclomatic² × (1 − coverage/100)³ + cyclomatic`)
//! will read in a follow-up — the same injection-time-composite
//! pattern as `history.hotspot`.
//!
//! Availability honesty (the `history.*` doctrine, unchanged): a file
//! absent from every report publishes nothing and reads as
//! unmeasured; a file present with zero covered lines publishes an
//! honest `0.0`; a function span with no instrumented lines publishes
//! nothing. Each sub-family (`line`, `branch`, `function`) is
//! published only when its report actually measured that dimension —
//! a Go coverprofile has no branch records, so `coverage.branch`
//! stays absent rather than reading a fabricated 100% or 0%.

use camino::{Utf8Path, Utf8PathBuf};
use mehen_core::{MetricSpace, SpaceKind, keys};
use mehen_coverage::{CoverageIndex, FileCoverage, FileMatch, SpanTotals};

/// Whether any requested metric name/key belongs to the `coverage.*`
/// family — the trigger for report discovery/parsing when the CLI
/// leaves coverage mode unset.
pub(crate) fn names_want_coverage<'a>(mut names: impl Iterator<Item = &'a str>) -> bool {
    // Only *valid* coverage keys trigger ingestion: a typo'd key can
    // never read a published value, so discovering and parsing
    // reports for it would be pure cost.
    names.any(|name| name.starts_with("coverage.") && !is_unknown_coverage_key(name))
}

/// A `coverage`-rooted name outside the fixed family
/// (`mehen_core::keys::COVERAGE_ALL`) — including the bare family root
/// `coverage`, which is not a leaf. The CLI selector parser rejects
/// these up front; the public engine boundaries accept arbitrary
/// strings, so they must be checked again there.
pub(crate) fn is_unknown_coverage_key(name: &str) -> bool {
    (name == "coverage" || name.starts_with("coverage."))
        && !mehen_core::keys::COVERAGE_ALL.contains(&name)
}

/// Whether an engine-boundary selector cannot read a published
/// coverage value at all: a `coverage`-rooted key outside the fixed
/// family, **or** a valid key with a non-root aggregator — like
/// history, top-offenders/diff read root keys only, so
/// `coverage.line.max` parses (key `coverage.line`, aggregator `Max`)
/// yet can never resolve.
pub(crate) fn is_invalid_coverage_selector(selector: &mehen_core::MetricSelector) -> bool {
    let key = selector.key.as_str();
    if key != "coverage" && !key.starts_with("coverage.") {
        return false;
    }
    is_unknown_coverage_key(key)
        || !matches!(selector.aggregator, mehen_core::SelectorAggregator::Root)
}

/// Publish one covered/total dimension: the rate under `rate_key`
/// plus the two counters. Published only when the dimension was
/// actually measured (`total > 0`) — an absent dimension must read as
/// unmeasured, never as 0% or 100%.
fn publish(
    metrics: &mut mehen_core::MetricSet,
    totals: SpanTotals,
    rate_key: &str,
    covered_key: &str,
    total_key: &str,
) {
    let Some(rate) = totals.rate() else {
        return;
    };
    metrics.insert(rate_key, rate);
    metrics.insert(covered_key, totals.covered as i64);
    metrics.insert(total_key, totals.total as i64);
}

/// Publish the `coverage.*` family onto a file's metric tree.
///
/// The root (`Unit`) space receives the whole-file line, branch-arm,
/// and function dimensions; every nested `Function`/`Closure` space
/// receives line and branch dimensions scoped to its line span.
/// `file` must be normalized (guaranteed by the parser/merge layer).
pub(crate) fn inject_coverage_metrics(root: &mut MetricSpace, file: &FileCoverage) {
    publish(
        &mut root.metrics,
        file.line_totals(),
        keys::COVERAGE_LINE,
        keys::COVERAGE_LINE_COVERED,
        keys::COVERAGE_LINE_TOTAL,
    );
    publish(
        &mut root.metrics,
        file.branch_totals(),
        keys::COVERAGE_BRANCH,
        keys::COVERAGE_BRANCH_COVERED,
        keys::COVERAGE_BRANCH_TOTAL,
    );
    publish(
        &mut root.metrics,
        file.function_totals(),
        keys::COVERAGE_FUNCTION,
        keys::COVERAGE_FUNCTION_COVERED,
        keys::COVERAGE_FUNCTION_TOTAL,
    );
    inject_into_children(&mut root.spaces, file);
}

fn inject_into_children(spaces: &mut [MetricSpace], file: &FileCoverage) {
    for space in spaces {
        if matches!(space.kind, SpaceKind::Function | SpaceKind::Closure)
            && space.span.start_line > 0
            && space.span.end_line >= space.span.start_line
        {
            publish(
                &mut space.metrics,
                file.span_line_totals(space.span.start_line, space.span.end_line),
                keys::COVERAGE_LINE,
                keys::COVERAGE_LINE_COVERED,
                keys::COVERAGE_LINE_TOTAL,
            );
            publish(
                &mut space.metrics,
                file.span_branch_totals(space.span.start_line, space.span.end_line),
                keys::COVERAGE_BRANCH,
                keys::COVERAGE_BRANCH_COVERED,
                keys::COVERAGE_BRANCH_TOTAL,
            );
        }
        // Functions nest inside classes/impls (and inside each other):
        // recurse unconditionally.
        inject_into_children(&mut space.spaces, file);
    }
}

/// Whether a selector can be honestly valued given what backs the
/// metric space, coverage included: `coverage`-rooted selectors need a
/// matched coverage entry (and must be a known family key); everything
/// else defers to [`crate::history_metrics::selector_available`].
pub(crate) fn selector_available_with_coverage(
    name: &str,
    statics: bool,
    history: bool,
    coverage: bool,
) -> bool {
    if name == "coverage" || name.starts_with("coverage.") {
        return coverage && !is_unknown_coverage_key(name);
    }
    crate::history_metrics::selector_available(name, statics, history)
}

/// Enrich a single-file metrics report with coverage — the
/// `mehen metrics` entry point.
///
/// Resolution: the `--coverage` flag, then the `[coverage]` config
/// section, then the lazy trigger (a configured `coverage.*`
/// threshold). Returns whether coverage data was injected. An
/// explicit report path that is missing or unparsable is an error;
/// everything else (no reports found, file not matched) degrades to
/// logs and an untouched report.
pub fn enrich_metrics_with_coverage(
    report: &mut mehen_core::MetricsReport,
    opts: &CoverageOpts,
    config: Option<&crate::config_file::ConfigFile>,
) -> Result<bool, CoverageSetupError> {
    let mode = opts.mode().map_err(CoverageSetupError)?;
    let coverage_config = config.and_then(|c| c.coverage.as_ref());
    // `mehen metrics` has no selector concept: the lazy trigger is a
    // configured coverage threshold, which the enriched root will be
    // gated against right after rendering.
    let wanted = config.is_some_and(|c| {
        c.thresholds
            .any_metric(|name| name.starts_with("coverage."))
    });
    let Some(root_dir) = coverage_root_for(report.path.as_path()) else {
        return Ok(false);
    };
    let Some(context) = resolve_coverage(&mode, coverage_config, &[root_dir], wanted)? else {
        return Ok(false);
    };
    let Some(file_coverage) = coverage_for_file(&context, report.path.as_path()) else {
        log::info!(
            "no coverage data matched `{}` across {} ingested report file entr{}",
            report.path,
            context.index.len(),
            if context.index.len() == 1 { "y" } else { "ies" }
        );
        return Ok(false);
    };
    inject_coverage_metrics(&mut report.root, &file_coverage);
    Ok(true)
}

// ─── Coverage input resolution (CLI flag + config + discovery) ───

/// The `--coverage` flag, shared by `mehen metrics` and
/// `mehen top-offenders`.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct CoverageOpts {
    /// Coverage input: 'auto' discovers report files (LCOV, Cobertura,
    /// JaCoCo, Clover, Istanbul, Go coverprofile); 'off' disables
    /// coverage; one or more report paths (--coverage=lcov.info) use
    /// exactly those files. Bare `--coverage` means 'auto'. When
    /// omitted, coverage loads lazily — only if a coverage.* metric or
    /// threshold asks for it.
    #[arg(
        long = "coverage",
        value_name = "PATH|auto|off",
        num_args = 0..=1,
        default_missing_value = "auto",
        // The house negatable-flag style (`--ignore-git-attributes`),
        // and load-bearing here: without it, `--coverage src/` would
        // swallow a positional path as the flag value.
        require_equals = true,
        action = clap::ArgAction::Append
    )]
    coverage: Vec<String>,
}

/// The resolved coverage request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoverageMode {
    /// No flag: load only when a `coverage.*` selector/threshold (or a
    /// `[coverage]` config section) asks.
    Unset,
    /// Force ingestion (discovery + configured reports).
    Auto,
    /// Coverage disabled, regardless of config and selectors.
    Off,
    /// Exactly these report files; discovery does not run.
    Explicit(Vec<Utf8PathBuf>),
}

impl CoverageOpts {
    /// Parse the repeated flag values into a single mode. `auto`/`off`
    /// are exclusive — with each other and with paths.
    pub(crate) fn mode(&self) -> Result<CoverageMode, String> {
        let mut auto = false;
        let mut off = false;
        let mut paths: Vec<Utf8PathBuf> = Vec::new();
        for value in &self.coverage {
            match value.as_str() {
                "auto" => auto = true,
                "off" | "none" => off = true,
                path => paths.push(Utf8PathBuf::from(path)),
            }
        }
        match (auto, off, paths.is_empty()) {
            (false, false, true) => Ok(CoverageMode::Unset),
            (true, false, true) => Ok(CoverageMode::Auto),
            (false, true, true) => Ok(CoverageMode::Off),
            (false, false, false) => Ok(CoverageMode::Explicit(paths)),
            _ => Err(
                "--coverage values conflict: 'auto', 'off', and explicit report paths are \
                 mutually exclusive"
                    .to_string(),
            ),
        }
    }
}

/// Everything the orchestrators need after ingestion: the
/// calculate-once query index over every parsed, merged report, plus
/// the canonicalized discovery roots used to re-spell absolute query
/// paths repo-relative. Report inventory and discovery diagnostics
/// are surfaced as logs at resolve time (a structured
/// `coverage_ingestion` JSON block is a planned follow-up).
pub(crate) struct CoverageContext {
    pub index: CoverageIndex,
    /// Canonicalized root directories (repository workdirs), for the
    /// repo-relative retry in [`coverage_for_file`].
    pub roots: Vec<std::path::PathBuf>,
}

impl CoverageContext {
    pub(crate) fn new(index: CoverageIndex, roots: &[Utf8PathBuf]) -> Self {
        Self {
            index,
            roots: roots
                .iter()
                .filter_map(|root| std::fs::canonicalize(root.as_std_path()).ok())
                .collect(),
        }
    }
}

/// A fatal coverage-setup problem (user-attributable: an explicit
/// report path that is missing or unparsable). Discovered-report
/// problems never take this path — they degrade to warnings.
#[derive(Debug)]
pub struct CoverageSetupError(pub String);

impl std::fmt::Display for CoverageSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CoverageSetupError {}

/// Read, sniff, and parse one report file. The single implementation
/// behind every ingestion path — the CLI's hard-error explicit reports,
/// discovered reports that degrade to warnings, and the library
/// boundary's diagnostic records — so the sniff window, format
/// detection, and parse behavior can never drift apart between them.
pub(crate) fn ingest_report(
    path: &Utf8Path,
) -> Result<(mehen_coverage::CoverageFormat, mehen_coverage::CoverageData), String> {
    let bytes = std::fs::read(path.as_std_path())
        .map_err(|e| format!("cannot read coverage report `{path}`: {e}"))?;
    let head_len = bytes.len().min(4096);
    let Some(format) = mehen_coverage::detect_format(path, &bytes[..head_len]) else {
        return Err(format!(
            "unrecognized coverage report format: `{path}` (supported: LCOV, Go coverprofile, \
             Istanbul JSON, JaCoCo/Clover/Cobertura XML)"
        ));
    };
    let data = mehen_coverage::parse_report(format, &bytes)
        .map_err(|e| format!("failed to parse coverage report `{path}`: {e}"))?;
    Ok((format, data))
}

/// Resolve the coverage request into a queryable index.
///
/// * `mode` — the CLI flag ([`CoverageOpts::mode`]).
/// * `config` — the `[coverage]` section of `mehen.toml`, if any.
/// * `roots` — directories to discover under (typically the enclosing
///   repository workdir per analysis root).
/// * `wanted` — whether a `coverage.*` selector or threshold asked for
///   coverage (the lazy trigger when the flag is unset).
///
/// Returns `Ok(None)` when coverage is off or nothing requested it.
pub(crate) fn resolve_coverage(
    mode: &CoverageMode,
    config: Option<&crate::config_file::CoverageConfig>,
    roots: &[Utf8PathBuf],
    wanted: bool,
) -> Result<Option<CoverageContext>, CoverageSetupError> {
    let config_reports: &[Utf8PathBuf] = config.map(|c| c.reports.as_slice()).unwrap_or(&[]);
    let config_discover = config.is_none_or(crate::config_file::CoverageConfig::discover);

    let (explicit, run_discovery) = match mode {
        CoverageMode::Off => return Ok(None),
        CoverageMode::Explicit(paths) => (paths.clone(), false),
        CoverageMode::Auto => (config_reports.to_vec(), config_discover),
        CoverageMode::Unset => {
            // Lazy path: a coverage.* selector/threshold, or a
            // [coverage] config section that opts in, enables the run.
            let config_opts_in = config.is_some_and(crate::config_file::CoverageConfig::opts_in);
            if !wanted && !config_opts_in {
                return Ok(None);
            }
            (config_reports.to_vec(), config_discover)
        }
    };

    let mut parsed: Vec<mehen_coverage::CoverageData> = Vec::new();
    let mut reports: Vec<(Utf8PathBuf, mehen_coverage::CoverageFormat)> = Vec::new();

    // Explicit reports (CLI paths or config `reports`): every failure
    // is a hard, user-attributable error — an explicit gate input that
    // silently disappears is a broken CI gate.
    for path in &explicit {
        let (format, data) = ingest_report(path).map_err(CoverageSetupError)?;
        reports.push((path.clone(), format));
        parsed.push(data);
    }

    // Discovered reports: failures degrade to warnings — auto-discovery
    // must never fail a run.
    if run_discovery {
        let outcome =
            mehen_coverage_discovery::discover(&mehen_coverage_discovery::DiscoveryOptions {
                roots: roots.to_vec(),
                extra_patterns: config.map(|c| c.extra_patterns.clone()).unwrap_or_default(),
                caps: mehen_coverage_discovery::DiscoveryCaps::default(),
            });
        let head_time = newest_head_commit_time(roots);
        for report in &outcome.reports {
            // Skip a discovered file we already parsed explicitly.
            if explicit.iter().any(|p| p == &report.path) {
                continue;
            }
            // Discovered-report failures degrade to warnings; the shared
            // ingest path keeps sniffing/parsing identical to the
            // hard-error explicit branch above.
            match ingest_report(&report.path) {
                Ok((format, data)) => {
                    warn_if_stale(report, head_time, config);
                    reports.push((report.path.clone(), format));
                    parsed.push(data);
                }
                Err(message) => {
                    log::warn!("skipping discovered coverage report: {message}");
                }
            }
        }
    }

    if parsed.is_empty() {
        log::info!("no coverage reports found; coverage metrics will be absent");
        return Ok(None);
    }

    log::info!(
        "coverage: ingesting {} report(s): {}",
        reports.len(),
        reports
            .iter()
            .map(|(p, f)| format!("{p} ({f})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let merged = mehen_coverage::merge::merge_reports(parsed);
    Ok(Some(CoverageContext::new(
        CoverageIndex::build(merged),
        roots,
    )))
}

/// Ingest the explicit base-revision reports (`mehen diff
/// --base-coverage`) into a queryable index.
///
/// Explicit-path semantics match [`CoverageMode::Explicit`]: every
/// failure is a hard, user-attributable error — a base report that
/// silently disappears would quietly demote every coverage trend to a
/// "new measurement". No discovery runs for the base side: the working
/// tree holds *head* artifacts, and reading them as the base
/// measurement would fabricate zero-delta trends.
///
/// Staleness is the [`warn_if_stale`] doctrine applied to the base
/// side, judged against the *base commit's* committer time rather than
/// the head clock: a report written before the base commit existed
/// cannot describe that commit's code — recency-based retrieval
/// fallbacks (e.g. a CI cache prefix key falling back to an older
/// default-branch entry) land exactly here — so base-side line
/// attribution may be shifted. Warn-only, gated by the same
/// `stale-warning` config key.
pub(crate) fn resolve_base_coverage(
    paths: &[Utf8PathBuf],
    roots: &[Utf8PathBuf],
    base_commit_time: Option<std::time::SystemTime>,
    config: Option<&crate::config_file::CoverageConfig>,
) -> Result<Option<CoverageContext>, CoverageSetupError> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut parsed: Vec<mehen_coverage::CoverageData> = Vec::new();
    let mut reports: Vec<(Utf8PathBuf, mehen_coverage::CoverageFormat)> = Vec::new();
    for path in paths {
        let (format, data) = ingest_report(path).map_err(CoverageSetupError)?;
        if config.is_none_or(|c| c.stale_warning)
            && let Ok(mtime) = std::fs::metadata(path.as_std_path()).and_then(|m| m.modified())
            && let Some(base_time) = base_commit_time
            && mtime < base_time
        {
            log::warn!(
                "base coverage report `{path}` predates the base commit — it likely describes \
                 an older revision, so base-side line attribution may be shifted (disable this \
                 warning with `stale-warning = false` under [coverage])"
            );
        }
        reports.push((path.clone(), format));
        parsed.push(data);
    }
    log::info!(
        "base coverage: ingesting {} report(s): {}",
        reports.len(),
        reports
            .iter()
            .map(|(p, f)| format!("{p} ({f})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let merged = mehen_coverage::merge::merge_reports(parsed);
    Ok(Some(CoverageContext::new(
        CoverageIndex::build(merged),
        roots,
    )))
}

/// Warn-only staleness: a report older than the newest HEAD commit
/// across the roots likely predates the code under analysis, so line
/// attribution may be shifted. mtime-based and heuristic — never an
/// exclusion.
fn warn_if_stale(
    report: &mehen_coverage_discovery::DiscoveredReport,
    head_time: Option<std::time::SystemTime>,
    config: Option<&crate::config_file::CoverageConfig>,
) {
    if !config.is_none_or(|c| c.stale_warning) {
        return;
    }
    if let (Some(mtime), Some(head)) = (report.mtime, head_time)
        && mtime < head
    {
        log::warn!(
            "coverage report `{}` predates the newest analyzed commit — line attribution may \
             be shifted; regenerate the report (disable this warning with `stale-warning = \
             false` under [coverage])",
            report.path
        );
    }
}

/// The newest HEAD committer timestamp across the discovery roots
/// (the deterministic "now" that survives CI clones). `None` outside
/// a repository or when HEAD is unborn.
fn newest_head_commit_time(roots: &[Utf8PathBuf]) -> Option<std::time::SystemTime> {
    roots
        .iter()
        .filter_map(|root| {
            let repo = gix::discover(root.as_std_path()).ok()?;
            let commit = repo.head_commit().ok()?;
            let seconds = commit.time().ok()?.seconds;
            u64::try_from(seconds)
                .ok()
                .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
        })
        .max()
}

/// The discovery/repository root for one analyzed path: the enclosing
/// repository workdir when there is one (coverage artifacts
/// conventionally live at the repo root even when analyzing `./src`),
/// else the path's own directory.
pub(crate) fn coverage_root_for(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    let start = if dir.as_str().is_empty() {
        Utf8PathBuf::from(".")
    } else {
        dir
    };
    let workdir = gix::discover(start.as_std_path())
        .ok()
        .and_then(|repo| repo.workdir().map(std::path::Path::to_path_buf))
        .and_then(|workdir| Utf8PathBuf::try_from(workdir).ok());
    Some(workdir.unwrap_or(start))
}

/// Look up a file's coverage, logging the ambiguous case.
///
/// Two-step query: the path as spelled first; then — when it is
/// absolute (or resolves to be) — re-spelled relative to each
/// canonicalized root. The retry is what connects a *local* absolute
/// spelling to a report written on a *different* machine: neither
/// `/local/checkout/src/app.py` nor `/ci/work/repo/src/app.py` is a
/// component-suffix of the other, but the repo-relative `src/app.py`
/// is a suffix of both. Stripping is deliberately root-scoped (the
/// grcov `--prefix-dir` idea, automated) rather than a blanket
/// filename-only match, which would mis-attribute same-named files
/// (`src/util.py` vs `tests/util.py`) across directories.
pub(crate) fn coverage_for_file<'a>(
    context: &'a CoverageContext,
    path: &Utf8Path,
) -> Option<std::borrow::Cow<'a, FileCoverage>> {
    enum Resolution<'a> {
        Found(std::borrow::Cow<'a, FileCoverage>),
        Ambiguous(usize),
        NotFound,
    }
    fn resolve<'a>(index: &'a CoverageIndex, query: &Utf8Path) -> Resolution<'a> {
        match index.file(query) {
            FileMatch::Found { coverage } => Resolution::Found(coverage),
            FileMatch::Ambiguous { candidates } => Resolution::Ambiguous(candidates),
            FileMatch::NotFound => Resolution::NotFound,
        }
    }

    let mut resolution = resolve(&context.index, path);
    if matches!(resolution, Resolution::NotFound)
        && let Ok(canonical) = std::fs::canonicalize(path.as_std_path())
    {
        for root in &context.roots {
            if let Ok(relative) = canonical.strip_prefix(root)
                && let Some(relative) = camino::Utf8Path::from_path(relative)
            {
                resolution = resolve(&context.index, relative);
                if !matches!(resolution, Resolution::NotFound) {
                    break;
                }
            }
        }
    }

    match resolution {
        Resolution::Found(coverage) => Some(coverage),
        Resolution::Ambiguous(candidates) => {
            log::warn!(
                "coverage for `{path}` is ambiguous ({candidates} report entries match with \
                 equal specificity); treating the file as unmeasured"
            );
            None
        }
        Resolution::NotFound => None,
    }
}

#[cfg(test)]
mod tests {
    use mehen_core::{MetricSet, SourceSpan, SpaceId};
    use mehen_coverage::{BranchCoverage, FunctionCoverage, LineCoverage};

    use super::*;

    fn space(kind: SpaceKind, start_line: u32, end_line: u32) -> MetricSpace {
        MetricSpace {
            id: SpaceId(0),
            kind,
            name: None,
            span: SourceSpan {
                start_byte: 0,
                end_byte: 0,
                start_line,
                end_line,
            },
            metrics: MetricSet::default(),
            spaces: Vec::new(),
        }
    }

    fn sample_file() -> FileCoverage {
        let mut file = FileCoverage::new("src/app.py".to_string());
        file.lines = vec![
            LineCoverage {
                line_number: 1,
                hit_count: 3,
            },
            LineCoverage {
                line_number: 5,
                hit_count: 1,
            },
            LineCoverage {
                line_number: 6,
                hit_count: 0,
            },
            LineCoverage {
                line_number: 12,
                hit_count: 0,
            },
        ];
        file.branches = vec![
            BranchCoverage {
                line_number: 5,
                branch_index: 0,
                hit_count: 1,
            },
            BranchCoverage {
                line_number: 5,
                branch_index: 1,
                hit_count: 0,
            },
        ];
        file.functions = vec![
            FunctionCoverage {
                name: "hit".to_string(),
                start_line: Some(5),
                end_line: None,
                hit_count: 1,
            },
            FunctionCoverage {
                name: "missed".to_string(),
                start_line: Some(12),
                end_line: None,
                hit_count: 0,
            },
        ];
        file.normalize();
        file
    }

    fn read(metrics: &MetricSet, key: &str) -> f64 {
        metrics
            .get(&mehen_core::MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or_else(|| panic!("missing key {key}"))
    }

    #[test]
    fn injects_all_three_dimensions_at_root() {
        let mut root = space(SpaceKind::Unit, 1, 20);
        inject_coverage_metrics(&mut root, &sample_file());

        // 2 of 4 instrumentable lines hit.
        assert_eq!(read(&root.metrics, keys::COVERAGE_LINE), 50.0);
        assert_eq!(read(&root.metrics, keys::COVERAGE_LINE_COVERED), 2.0);
        assert_eq!(read(&root.metrics, keys::COVERAGE_LINE_TOTAL), 4.0);
        // 1 of 2 branch arms taken.
        assert_eq!(read(&root.metrics, keys::COVERAGE_BRANCH), 50.0);
        // 1 of 2 recorded functions executed.
        assert_eq!(read(&root.metrics, keys::COVERAGE_FUNCTION), 50.0);
    }

    #[test]
    fn function_spaces_get_span_scoped_line_and_branch_coverage() {
        let mut root = space(SpaceKind::Unit, 1, 20);
        let mut class = space(SpaceKind::Class, 4, 15);
        // `hit` spans lines 5..=8: lines 5 (hit) and 6 (missed).
        class.spaces.push(space(SpaceKind::Function, 5, 8));
        // `missed` spans lines 12..=14: line 12 (missed).
        class.spaces.push(space(SpaceKind::Function, 12, 14));
        root.spaces.push(class);

        inject_coverage_metrics(&mut root, &sample_file());

        let class = &root.spaces[0];
        // Class spaces are not annotated (root + functions only)…
        assert!(
            class
                .metrics
                .get(&mehen_core::MetricKey::new(keys::COVERAGE_LINE))
                .is_none()
        );
        // …but the functions nested inside them are.
        let hit = &class.spaces[0];
        assert_eq!(read(&hit.metrics, keys::COVERAGE_LINE), 50.0);
        assert_eq!(read(&hit.metrics, keys::COVERAGE_BRANCH), 50.0);
        let missed = &class.spaces[1];
        assert_eq!(read(&missed.metrics, keys::COVERAGE_LINE), 0.0);
        // No branch records within 12..=14 → dimension absent, not 0.
        assert!(
            missed
                .metrics
                .get(&mehen_core::MetricKey::new(keys::COVERAGE_BRANCH))
                .is_none()
        );
    }

    #[test]
    fn uninstrumented_function_span_publishes_nothing() {
        // Lines 15..=19 have no instrumentable records: the function
        // must read as unmeasured (the CRAP `--missing` policy hook),
        // never as 0% or 100%.
        let mut root = space(SpaceKind::Unit, 1, 20);
        root.spaces.push(space(SpaceKind::Function, 15, 19));
        inject_coverage_metrics(&mut root, &sample_file());
        assert!(
            root.spaces[0]
                .metrics
                .get(&mehen_core::MetricKey::new(keys::COVERAGE_LINE))
                .is_none()
        );
    }

    #[test]
    fn missing_dimension_stays_absent_at_root() {
        // A Go coverprofile has line records only.
        let mut file = FileCoverage::new("pkg/a.go".to_string());
        file.lines = vec![LineCoverage {
            line_number: 1,
            hit_count: 1,
        }];
        file.normalize();
        let mut root = space(SpaceKind::Unit, 1, 5);
        inject_coverage_metrics(&mut root, &file);
        assert_eq!(read(&root.metrics, keys::COVERAGE_LINE), 100.0);
        for absent in [keys::COVERAGE_BRANCH, keys::COVERAGE_FUNCTION] {
            assert!(
                root.metrics
                    .get(&mehen_core::MetricKey::new(absent))
                    .is_none(),
                "{absent} must stay absent"
            );
        }
    }

    #[test]
    fn names_want_coverage_detects_family_keys() {
        assert!(names_want_coverage(
            ["cognitive", "coverage.line"].into_iter()
        ));
        assert!(!names_want_coverage(
            ["cognitive", "history.churn.abs"].into_iter()
        ));
        // A typo'd coverage key must not trigger ingestion.
        assert!(!names_want_coverage(["coverage.lines"].into_iter()));
        assert!(!names_want_coverage(std::iter::empty()));
    }

    #[test]
    fn every_family_key_is_known() {
        for key in mehen_core::keys::COVERAGE_ALL {
            assert!(!is_unknown_coverage_key(key), "{key} must be known");
        }
        assert!(is_unknown_coverage_key("coverage.lines"));
        assert!(is_unknown_coverage_key("coverage.statement"));
        // The bare family root is not a leaf and must not read the
        // missing-key 0.0 fallback as an available metric.
        assert!(is_unknown_coverage_key("coverage"));
        assert!(!is_unknown_coverage_key("cognitive"));
    }

    #[test]
    fn bare_family_roots_are_rejected_at_the_engine_boundary() {
        // `coverage` / `history` parse as bare keys with the Root
        // aggregator; without explicit handling they would slip past
        // the prefix guards and rank on fabricated zeros.
        let coverage: mehen_core::MetricSelector = "coverage".parse().unwrap();
        assert!(is_invalid_coverage_selector(&coverage));
        assert!(!selector_available_with_coverage(
            "coverage", true, true, true
        ));

        let history: mehen_core::MetricSelector = "history".parse().unwrap();
        assert!(crate::history_metrics::is_invalid_history_selector(
            &history
        ));
        assert!(!crate::history_metrics::selector_available(
            "history", true, true
        ));
    }

    #[test]
    fn coverage_mode_parsing() {
        let mode = |values: &[&str]| CoverageOpts {
            coverage: values.iter().map(ToString::to_string).collect(),
        };
        assert_eq!(mode(&[]).mode().unwrap(), CoverageMode::Unset);
        assert_eq!(mode(&["auto"]).mode().unwrap(), CoverageMode::Auto);
        assert_eq!(mode(&["off"]).mode().unwrap(), CoverageMode::Off);
        assert_eq!(
            mode(&["lcov.info", "qa/coverage.xml"]).mode().unwrap(),
            CoverageMode::Explicit(vec![
                Utf8PathBuf::from("lcov.info"),
                Utf8PathBuf::from("qa/coverage.xml")
            ])
        );
        assert!(mode(&["auto", "off"]).mode().is_err());
        assert!(mode(&["auto", "lcov.info"]).mode().is_err());
    }
}
