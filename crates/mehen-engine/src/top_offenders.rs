// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen top-offenders` orchestrator.
//!
//! Phase 5 implementation: walks the input paths, detects each file's
//! language, runs analysis through the registry, and ranks the files by
//! the requested metric selectors. Per the rewrite plan §2.4:
//! deterministic sorted output, ties broken by subsequent selectors.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use camino::Utf8PathBuf;
use mehen_core::{
    AnalysisErrorRecord, DiffSide, Language, MetricKey, ParseDiagnostic, Polarity, SourceFile,
};
use mehen_metrics::{MetricSelector, SelectorAggregator};

use crate::detection::detect_language;
use crate::registry::AnalyzerRegistry;
use mehen_core::{TopOffenderEntry, TopOffendersInput, TopOffendersReport};

/// Run `mehen top-offenders` against `input.paths` and return a ranked
/// report.
pub fn rank_top_offenders(input: TopOffendersInput) -> TopOffendersReport {
    let registry = Arc::new(AnalyzerRegistry::default_set());
    let mut entries: Vec<TopOffenderEntry> = Vec::new();
    let mut analysis_errors: Vec<AnalysisErrorRecord> = Vec::new();
    // The engine boundary accepts arbitrary selector strings: a typo'd
    // `history.*` key (the family is fixed — `keys::HISTORY_ALL`) can
    // never read a published value, so it is surfaced as an analysis
    // error, scores as uncomputable (`None`), and never triggers the
    // repository walk below.
    for selector in &input.selectors {
        if crate::history_metrics::is_invalid_history_selector(selector) {
            analysis_errors.push(AnalysisErrorRecord {
                path: Utf8PathBuf::new(),
                side: DiffSide::Head,
                diagnostics: vec![ParseDiagnostic::warning(
                    "engine.unknown_metric",
                    format!(
                        "unresolvable history selector `{selector}` (the fixed `history.*` keys publish root values only)",
                    ),
                )],
            });
        }
    }
    // `history.*` selectors need repository histories. Root-load
    // failures surface as `analysis_errors` (this API has no fatal
    // channel); per-file lazy discovery still covers repositories the
    // eager pass missed. Only *resolvable* history selectors trigger
    // the walk — an invalid one can never read a value, so walking
    // for it would be pure cost.
    let histories = if input.selectors.iter().any(|s| {
        s.key.as_str().starts_with("history.")
            && !crate::history_metrics::is_invalid_history_selector(s)
    }) {
        let loaded = RepoHistories::new();
        for root in &input.paths {
            if let Err(e) = loaded.load_root(root.as_std_path()) {
                analysis_errors.push(AnalysisErrorRecord {
                    path: root.clone(),
                    side: DiffSide::Head,
                    diagnostics: vec![ParseDiagnostic::warning(
                        "engine.history_unavailable",
                        format!("history metrics unavailable for {root}: {e}"),
                    )],
                });
            }
        }
        Some(loaded)
    } else {
        None
    };
    // Dedup files across roots. Without this, callers passing
    // overlapping paths (`.` plus `src`, or a directory plus a file
    // inside it) would rank the same file multiple times, crowding
    // out other files once `max_results` is applied.
    //
    // All roots share one normalized walk, then each result is mapped back
    // through the first matching input root. Canonical keys collapse
    // different *spellings* of one path (overlapping roots, directory
    // symlinks) but keep a tracked symlink distinct from its target —
    // each is its own repository entry with its own history.
    let mut seen: HashSet<Utf8PathBuf> = HashSet::new();

    for entry in walk_paths(&input.paths, &input.include, &input.exclude) {
        if !seen.insert(canonical_key(&entry)) {
            continue;
        }
        let Some(language) = detect_language(entry.as_path()) else {
            continue;
        };
        let analyzer = registry.analyzer_for(language);
        if analyzer.is_none() {
            // Language detected but no analyzer registered (the
            // owning crate is feature-gated off in this build).
            // Surface as a non-fatal `analysis_error` so callers
            // can distinguish "no offenders" from "offenders
            // silently skipped" — matching the diff path's
            // `record_unavailable` (rewrite plan §3.5). History
            // metrics need no parser, so the file may still rank
            // below on Git-only selectors.
            record_unavailable(&mut analysis_errors, &entry, language);
        }
        // History metrics don't depend on decoding or parsing the
        // blob: a recognized file whose contents static analysis
        // cannot handle — or whose language's analyzer is
        // feature-gated off — still has repository history, and a
        // history selector must rank it on real values (via an empty
        // metric space) instead of silently dropping it. Static-only
        // rankings keep skipping such files.
        let history_entry = histories.as_ref().and_then(|h| h.file(entry.as_std_path()));
        let analyzed_root = analyzer.and_then(|analyzer| {
            let text = std::fs::read_to_string(entry.as_std_path()).ok()?;
            let source = SourceFile::new(entry.clone(), language, text);
            let analysis = analyzer.analyze(&source, &input.config).ok()?;
            // Migrated analyzers can return `Ok(...)` with a
            // partial tree alongside an `Error`/`Fatal`
            // diagnostic when the file doesn't parse cleanly.
            // Per §9.3 those analyses are incomplete; surfacing
            // them in the offender list as if they were measured
            // would mislead CI/policy callers.
            if crate::diff::has_blocking_diagnostic(&analysis.diagnostics) {
                return None;
            }
            Some(analysis.root)
        });
        let statics_available = analyzed_root.is_some();
        let history_available = history_entry.is_some();
        let mut root = match (analyzed_root, history_available) {
            (Some(root), _) => root,
            (None, true) => mehen_core::MetricSpace::new(
                mehen_core::SpaceId(0),
                mehen_core::SpaceKind::Unit,
                mehen_core::SourceSpan::empty(),
            ),
            (None, false) => continue,
        };

        // Fold the `history.*` family into the metric set so history
        // selectors rank on real values. The static-dependent
        // composites are omitted when no real analysis backs the
        // space (see `inject_history_metrics`).
        if let Some((fh, head_seconds)) = history_entry {
            crate::history_metrics::inject_history_metrics(
                &mut root.metrics,
                &fh,
                head_seconds,
                statics_available,
            );
        }

        let scores: Vec<Option<f64>> = input
            .selectors
            .iter()
            .map(|s| {
                // A selector the space cannot back — any static
                // metric on a history-only fallback, any `history.*`
                // metric on a file without recorded Git history, or a
                // history selector no enrichment can resolve (typo'd
                // key / non-root aggregator) — has no measurable
                // value, and the missing-key `0.0` fallback must not
                // rank the file on a fabricated one (worst-possible
                // MI on an undecodable file; zero-age "worst
                // offender" for an untracked file).
                if crate::history_metrics::is_invalid_history_selector(s)
                    || !crate::history_metrics::selector_available(
                        s.key.as_str(),
                        statics_available,
                        history_available,
                    )
                {
                    None
                } else {
                    Some(read_metric(s, &root))
                }
            })
            .collect();

        entries.push(TopOffenderEntry {
            path: entry,
            language,
            scores,
        });
    }

    let polarities: Vec<Polarity> = input.selectors.iter().map(default_polarity_for).collect();
    entries.sort_by(|a, b| cmp_entries(a, b, &polarities));
    if entries.len() > input.max_results {
        entries.truncate(input.max_results);
    }

    // Lazily discovered repositories whose history was unavailable
    // (e.g. a shallow nested clone) must be visible to callers — their
    // files were ranked on absent history, not a real zero.
    if let Some(histories) = histories.as_ref() {
        for (location, message) in histories.take_failures() {
            analysis_errors.push(AnalysisErrorRecord {
                path: Utf8PathBuf::from_path_buf(location.clone())
                    .unwrap_or_else(|_| Utf8PathBuf::from(location.to_string_lossy().into_owned())),
                side: DiffSide::Head,
                diagnostics: vec![ParseDiagnostic::warning(
                    "engine.history_unavailable",
                    format!("history metrics unavailable: {message}"),
                )],
            });
        }
    }

    TopOffendersReport {
        schema_version: "1.0".to_string(),
        selectors: input.selectors.iter().map(|s| s.to_string()).collect(),
        entries,
        analysis_errors,
    }
}

/// Dedup key across overlapping roots and directory symlinks: the
/// *parent* is canonicalized but the final component is preserved, so
/// two spellings of one file collapse while a tracked symlink and its
/// target remain distinct entries (each with its own history — see
/// `canonical_file_path`).
fn canonical_key(path: &Utf8PathBuf) -> Utf8PathBuf {
    canonical_file_path(path.as_std_path())
        .and_then(|canonical| Utf8PathBuf::from_path_buf(canonical).ok())
        .unwrap_or_else(|| path.clone())
}

/// Push an `engine.analyzer_unavailable` record for `path` so callers
/// can tell that a file was skipped because the owning language crate
/// is feature-gated off (mirroring the diff path's behavior).
fn record_unavailable(
    errors: &mut Vec<AnalysisErrorRecord>,
    path: &Utf8PathBuf,
    language: Language,
) {
    errors.push(AnalysisErrorRecord {
        path: path.clone(),
        // `top-offenders` has no base/head distinction; pick `Head`
        // by convention so the JSON shape stays compatible with diff.
        side: DiffSide::Head,
        diagnostics: vec![ParseDiagnostic::warning(
            "engine.analyzer_unavailable",
            format!(
                "no analyzer registered for `{}` in this build",
                language.canonical()
            ),
        )],
    });
}

fn walk_paths(roots: &[Utf8PathBuf], include: &[String], exclude: &[String]) -> Vec<Utf8PathBuf> {
    walk_files(&FilesData {
        include: mk_globset(include),
        exclude: mk_globset(exclude),
        paths: roots
            .iter()
            .map(|root| root.as_std_path().to_path_buf())
            .collect(),
        respect_ignores: true,
    })
    .into_iter()
    .filter_map(|path| Utf8PathBuf::try_from(path).ok())
    .collect()
}

/// Order entries from most concerning to least.
///
/// "Most concerning" depends on the metric's polarity. For
/// `HigherIsWorse` metrics (cyclomatic, cognitive, halstead.volume,
/// loc.*) a larger value is worse, so they sort descending. For
/// `HigherIsBetter` metrics (mi.original, mi.sei, mi.visual_studio)
/// a smaller value is worse, so they sort ascending.
///
/// Cascade through every selector so secondary keys break ties on
/// the primary, tertiary keys break ties on the secondary, etc.
/// Path tie-breaks last for determinism.
fn cmp_entries(
    a: &TopOffenderEntry,
    b: &TopOffenderEntry,
    polarities: &[Polarity],
) -> std::cmp::Ordering {
    for (i, polarity) in polarities.iter().enumerate() {
        let av = a.scores.get(i).copied().flatten();
        let bv = b.scores.get(i).copied().flatten();
        let ord = match (av, bv) {
            // An uncomputable score ranks as least concerning under
            // either polarity — it is *absent*, not a zero (which
            // would be the *most* concerning value for a
            // higher-is-better metric).
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
            (Some(av), Some(bv)) => {
                let base = av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal);
                match polarity {
                    // Worst-first: larger value is more concerning, so a > b
                    // should put `a` first → reverse the natural ordering.
                    Polarity::HigherIsWorse => base.reverse(),
                    // Worst-first: smaller value is more concerning, so a < b
                    // should put `a` first → use the natural ordering.
                    Polarity::HigherIsBetter => base,
                }
            }
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.path.cmp(&b.path)
}

/// Resolve a metric's "higher is worse / better" polarity from its
/// key. Maintainability-index variants (`mi.*`) are higher-is-better;
/// the language-owned `sql.*`/`markdown.*` quality scores
/// (`sql.maintainability_index`, `sql.modularity_health`, …) are too —
/// otherwise `rank_top_offenders` would surface the *healthiest* SQL/doc
/// files as the worst offenders (Codex P2). Every other metric the engine
/// publishes (cyclomatic, cognitive, loc.*, halstead.*, abc, nom, nargs,
/// nexit, npa, npm, wmc) is higher-is-worse. This mirrors the legacy
/// `KNOWN_METRICS` catalog and the rewrite plan §5.1 metric contract.
fn default_polarity_for(selector: &MetricSelector) -> Polarity {
    let key = selector.key.as_str();
    if key.starts_with("mi.")
        || key == "mi"
        || crate::metric_selector::is_namespaced_higher_is_better(key)
    {
        Polarity::HigherIsBetter
    } else {
        Polarity::HigherIsWorse
    }
}

pub(crate) fn read_metric(selector: &MetricSelector, root: &mehen_core::MetricSpace) -> f64 {
    let lookup = |key: &MetricKey| root.metrics.get(key).map(|v| v.as_f64());
    match selector.aggregator {
        SelectorAggregator::Root => lookup(&selector.key).unwrap_or(0.0),
        SelectorAggregator::Sum => suffixed_lookup(&selector.key, &["sum"], &lookup),
        SelectorAggregator::Min => suffixed_lookup(&selector.key, &["min"], &lookup),
        SelectorAggregator::Max => suffixed_lookup(&selector.key, &["max"], &lookup),
        // Per `mehen-metrics::state`, average is published as either
        // `<key>.avg` (cyclomatic, loc.*) or `<key>.average`
        // (cognitive, nom, nargs, nexit, npa, npm). Try the short form
        // first to match the selector spelling, then fall back.
        SelectorAggregator::Avg => suffixed_lookup(&selector.key, &["avg", "average"], &lookup),
    }
}

/// Look the selector key up under each suffix in order (e.g.
/// `["avg", "average"]` for the avg aggregator), returning the first
/// hit. `0.0` if none match — keeps the behavior of a missing metric
/// the same as a missing root-level key.
///
/// For each suffix the lookup tries the dotted form first
/// (`<base>.<suffix>`) and falls back to the underscore form
/// (`<base>_<suffix>`). The underscore form is what the shared
/// publishers in `mehen-metrics::state` use for sub-bucket aggregates:
/// `nom.functions_max`, `nom.closures_min`, `abc.assignments_average`,
/// `npa.classes_average`, `npm.interfaces_average`, `nargs.functions_max`,
/// etc. Without the fallback, selectors like `nom.functions.max` would
/// silently read `0.0` even when the analyzer published the value,
/// misordering top-offenders rankings and suppressing diff-threshold
/// violations.
fn suffixed_lookup(
    base: &MetricKey,
    suffixes: &[&str],
    lookup: &dyn Fn(&MetricKey) -> Option<f64>,
) -> f64 {
    for suffix in suffixes {
        let dotted = MetricKey::new(format!("{base}.{suffix}"));
        if let Some(v) = lookup(&dotted) {
            return v;
        }
        let underscored = MetricKey::new(format!("{base}_{suffix}"));
        if let Some(v) = lookup(&underscored) {
            return v;
        }
    }
    0.0
}

// ── pre-1.0 CLI orchestrator (`mehen top-offenders`) ───────────────────
//
// Everything below drives the published `mehen top-offenders` subcommand
// and was hoisted out of `legacy/top_offenders.rs` into this module so
// the CLI shares the same module tree as the post-1.0 `rank_top_offenders`
// entry point above. Names that overlap with the post-1.0 surface
// (`MetricSelector`, `read_metric`) are imported under aliases.

use std::cmp::Ordering;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Mutex;
use std::thread::available_parallelism;

use crate::concurrent_files::{ConcurrentRunner, FilesData, mk_globset, walk_files};
use crate::metric_selector::{
    MetricSelector as CliMetricSelector, Polarity as SelectorPolarity, parse_metric_selectors,
    read_metric as read_selector_metric,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum TopOffendersFormat {
    Markdown,
    Json,
}

#[derive(clap::Args, Debug)]
pub struct TopOffendersOpts {
    /// Metric to rank by. Repeatable; order matters — the first `--metric` is
    /// the primary sort key, the next breaks ties, etc.
    ///
    /// Prefix with `+` to flip a metric to higher-is-better (best at top) or
    /// `-` for lower-is-better. Without a prefix the metric's default polarity
    /// is used. Known names: `cyclomatic`, `cognitive`, `nom.functions`,
    /// `loc.lloc`, `mi.original`, `mi.sei`, `mi.visual_studio`,
    /// `halstead.volume`, `abc`. Namespaced keys (`sql.*`, `markdown.*`,
    /// `history.*`) are accepted verbatim; `history.*` metrics require a git
    /// repository and trigger a history walk of `HEAD`.
    #[clap(
        long = "metric",
        short = 'M',
        required = true,
        num_args = 1,
        allow_hyphen_values = true
    )]
    metrics: Vec<String>,

    /// Maximum number of offenders to return.
    #[clap(long, default_value_t = 10)]
    max_results: usize,

    /// Output format.
    #[clap(long, short = 'O', value_enum, default_value_t = TopOffendersFormat::Markdown)]
    output_format: TopOffendersFormat,

    /// Glob to include files. Repeat the flag for multiple patterns.
    #[clap(long, short = 'I', num_args = 1)]
    include: Vec<String>,

    /// Glob to exclude files. Repeat the flag for multiple patterns.
    #[clap(long, short = 'X', num_args = 1)]
    exclude: Vec<String>,

    /// Do not respect ignore files or generated/vendored/binary Git
    /// attributes while walking directories.
    #[clap(long)]
    no_ignore: bool,

    /// Number of parser jobs.
    #[clap(long, short = 'j')]
    num_jobs: Option<usize>,

    /// Language type override (skip auto-detection).
    #[clap(long, short)]
    language_type: Option<String>,

    /// One or more files or directories to analyze.
    #[clap(required = true, num_args = 1..)]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CliMetricValue {
    name: &'static str,
    label: &'static str,
    /// `None` (JSON `null`): the metric could not be computed for
    /// this file — a static-dependent history composite on a file
    /// whose static analysis is unavailable. Rendered as `n/a` and
    /// ranked as least concerning, never as a fabricated zero.
    value: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct FileOffender {
    path: PathBuf,
    metrics: Vec<CliMetricValue>,
}

struct TopOffendersCfg {
    selectors: Vec<CliMetricSelector>,
    language_override: Option<Language>,
    registry: Arc<AnalyzerRegistry>,
    /// Per-repository histories at `HEAD` for every repository the
    /// input roots belong to — present only when a `history.*` metric
    /// was requested. Shared with the orchestrator, which drains
    /// recorded lazy-discovery failures after the run.
    history: Option<Arc<RepoHistories>>,
    results: Arc<Mutex<Vec<FileOffender>>>,
}

/// Lazily discovered `HEAD` histories, one per repository work dir.
///
/// Each analyzed file is mapped to its *innermost* containing
/// repository by discovering from the file's (symlink-preserving,
/// canonicalized) parent directory, so nested repositories found
/// during traversal read their own history instead of zeros.
/// Discovery results and walked histories are cached; repositories
/// that cannot be opened or walked (e.g. a shallow nested clone) read
/// the family as absent.
struct RepoHistories {
    state: Mutex<RepoHistoriesState>,
}

#[derive(Default)]
struct RepoHistoriesState {
    /// Canonical parent dir → canonical work dir (`None`: not in a
    /// repository, or discovery failed).
    dir_to_workdir: HashMap<PathBuf, Option<PathBuf>>,
    /// Canonical work dir → lazily initialized `HEAD` history
    /// (`None` inside an initialized cell: walk failed). The
    /// `OnceLock` serializes each repository's cold walk across
    /// workers while different repositories initialize concurrently.
    histories: HashMap<PathBuf, Arc<std::sync::OnceLock<Option<mehen_git::RepositoryHistory>>>>,
    /// Lazily discovered repositories that exist but whose history is
    /// unavailable (shallow nested clone, walk failure). Recorded once
    /// per location so callers can surface them instead of silently
    /// ranking those files on zero-valued history.
    failures: Vec<(PathBuf, String)>,
}

impl RepoHistories {
    fn new() -> Self {
        Self {
            state: Mutex::new(RepoHistoriesState::default()),
        }
    }

    /// Eagerly load the repository containing an explicitly analyzed
    /// root, propagating errors — a requested history ranking must not
    /// silently be all zeros because a root isn't in a (full) clone.
    ///
    /// A directory root (or a symlink to one) discovers from its
    /// canonicalized target; a file or file-symlink root discovers
    /// from its *lexical* parent so a tracked symlink pointing outside
    /// its repository still resolves to the repository that tracks it.
    fn load_root(&self, root: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = std::fs::metadata(root)
            .map_err(|e| format!("cannot resolve path {}: {e}", root.display()))?;
        let discover_from = if metadata.is_dir() {
            std::fs::canonicalize(root)?
        } else {
            let parent = match root.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            std::fs::canonicalize(parent)?
        };
        let repo = match mehen_git::open_repo_at(&discover_from) {
            Ok(repo) => repo,
            // A directory root that is not itself inside Git may still
            // *contain* repositories: per-file lookups discover the
            // innermost repository lazily (see `file`), so an eager
            // hard failure here would reject valid layouts like a
            // container directory of checkouts. Files directly under
            // such a root simply have no history. Genuine open
            // failures (untrusted or unreadable repositories, shallow
            // clones) still propagate.
            Err(mehen_git::GitError::RepoNotFound) if metadata.is_dir() => {
                let mut state = self.state.lock().expect("repo histories mutex poisoned");
                state.dir_to_workdir.insert(discover_from, None);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let workdir = repo
            .workdir()
            .ok_or("repository has no work dir (bare repository)")?
            .to_path_buf();
        let canonical_workdir = std::fs::canonicalize(&workdir)?;
        let cell = {
            let mut state = self.state.lock().expect("repo histories mutex poisoned");
            state
                .dir_to_workdir
                .insert(discover_from, Some(canonical_workdir.clone()));
            state
                .histories
                .entry(canonical_workdir)
                .or_default()
                .clone()
        };
        // Walk outside the state lock (other repositories keep
        // loading); the cell serializes duplicate initializers.
        let mut walk_error: Option<mehen_git::GitError> = None;
        cell.get_or_init(|| match mehen_git::collect_history(&repo, "HEAD") {
            Ok(history) => Some(history),
            Err(e) => {
                walk_error = Some(e);
                None
            }
        });
        match walk_error {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// The per-file history entry and that repository's deterministic
    /// "now" for one analyzed file. Untracked files and files outside
    /// every discoverable repository read as absent.
    ///
    /// The lock is held only for cache reads/writes — repository
    /// discovery and (expensive) cold history walks run unlocked so
    /// concurrent workers analyzing other repositories never serialize
    /// behind one walk. Each repository's cold walk runs exactly once:
    /// workers racing the same repository block on its `OnceLock`
    /// cell rather than launching duplicate walks.
    fn file(&self, file_path: &Path) -> Option<(mehen_git::FileHistory, i64)> {
        let canonical = canonical_file_path(file_path)?;
        let parent = canonical.parent()?.to_path_buf();

        let cached_workdir = {
            let state = self.state.lock().expect("repo histories mutex poisoned");
            state.dir_to_workdir.get(&parent).cloned()
        };
        let workdir = match cached_workdir {
            Some(cached) => cached,
            None => {
                // A directory outside any repository is a normal case
                // (RepoNotFound stays silent); a repository that
                // exists but can't be used — e.g. a shallow nested
                // clone — is a real failure that must not silently
                // read as zero history.
                let discovered = match mehen_git::open_repo_at(&parent) {
                    Ok(repo) => repo.workdir().and_then(|wd| std::fs::canonicalize(wd).ok()),
                    Err(mehen_git::GitError::RepoNotFound) => None,
                    Err(e) => {
                        log::warn!("history unavailable under {}: {e}", parent.display());
                        let mut state = self.state.lock().expect("repo histories mutex poisoned");
                        state.failures.push((parent.clone(), e.to_string()));
                        None
                    }
                };
                let mut state = self.state.lock().expect("repo histories mutex poisoned");
                state
                    .dir_to_workdir
                    .entry(parent)
                    .or_insert(discovered)
                    .clone()
            }
        }?;

        // Per-worktree cold-walk coordination: the first worker to
        // reach a repository initializes its `OnceLock` while others
        // block on that cell only — different repositories still load
        // concurrently, and a large repository is walked exactly once
        // instead of once per worker that races the cold cache.
        let cell = {
            let mut state = self.state.lock().expect("repo histories mutex poisoned");
            state.histories.entry(workdir.clone()).or_default().clone()
        };
        let history = cell
            .get_or_init(|| {
                match mehen_git::open_repo_at(&workdir)
                    .and_then(|repo| mehen_git::collect_history(&repo, "HEAD"))
                {
                    Ok(history) => Some(history),
                    Err(e) => {
                        log::warn!("history walk failed for {}: {e}", workdir.display());
                        let mut state = self.state.lock().expect("repo histories mutex poisoned");
                        state.failures.push((workdir.clone(), e.to_string()));
                        None
                    }
                }
            })
            .as_ref()?;
        let relative = canonical.strip_prefix(&workdir).ok()?;
        // `tracked_file`, not `file`: a workspace path may be an
        // untracked file (or a symlink) occupying a spot whose tracked
        // blob HEAD deleted — the dead occupant's history is not this
        // file's.
        history
            .tracked_file(relative)
            .map(|fh| (fh, history.head_seconds))
    }

    /// Drain the recorded lazy-discovery failures (repositories that
    /// exist but whose history was unavailable).
    fn take_failures(&self) -> Vec<(PathBuf, String)> {
        let mut state = self.state.lock().expect("repo histories mutex poisoned");
        std::mem::take(&mut state.failures)
    }
}

/// Canonicalize a file path *without resolving the final component*:
/// a tracked symlink like `alias.py -> real.py` must keep its own
/// (empty) history rather than borrowing the target file's churn and
/// authorship. Directory components are still resolved so the result
/// is comparable with the canonicalized repository work dir.
fn canonical_file_path(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Some(std::fs::canonicalize(parent).ok()?.join(file_name))
}

fn act_on_file(path: PathBuf, cfg: &TopOffendersCfg) -> std::io::Result<()> {
    let utf8_path = match Utf8PathBuf::try_from(path.clone()) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let language = match cfg.language_override {
        Some(l) => l,
        None => match detect_language(&utf8_path) {
            Some(l) => l,
            None => return Ok(()),
        },
    };

    let analyzer = cfg.registry.analyzer_for(language);

    // History metrics don't depend on decoding or parsing the blob:
    // a recognized file whose contents static analysis cannot handle
    // (e.g. non-UTF-8 but non-binary) — or whose language's analyzer
    // is feature-gated off in this build — still has repository
    // history, and a history selector must rank it on real values
    // instead of silently dropping it. Static-only rankings keep
    // skipping such files (an all-zero row would be noise).
    let history_entry = cfg.history.as_ref().and_then(|h| h.file(&path));

    let analyzed_root = analyzer.and_then(|analyzer| {
        let text = std::fs::read_to_string(&path).ok()?;
        let source = SourceFile::new(utf8_path, language, text);
        let analysis = analyzer
            .analyze(&source, &mehen_core::AnalysisConfig::default())
            .ok()?;
        // A partial tree behind an `Error`/`Fatal` diagnostic is an
        // incomplete measurement (§9.3): ranking on it — or feeding
        // its truncated cognitive/SLOC values into the history
        // composites — would mislead; fall back to history-only.
        if crate::diff::has_blocking_diagnostic(&analysis.diagnostics) {
            return None;
        }
        Some(analysis.root)
    });
    let statics_available = analyzed_root.is_some();
    let history_available = history_entry.is_some();
    let mut root = match (analyzed_root, history_available) {
        (Some(root), _) => root,
        (None, true) => mehen_core::MetricSpace::new(
            mehen_core::SpaceId(0),
            mehen_core::SpaceKind::Unit,
            mehen_core::SourceSpan::empty(),
        ),
        (None, false) => return Ok(()),
    };

    // Fold the `history.*` family into the metric set so history
    // selectors rank on real values. Files without recorded history
    // (untracked, outside every known work dir) read the family as
    // *unavailable* below; the static-dependent composites are
    // omitted when no real analysis backs the space (see
    // `inject_history_metrics`).
    if let Some((fh, head_seconds)) = history_entry {
        crate::history_metrics::inject_history_metrics(
            &mut root.metrics,
            &fh,
            head_seconds,
            statics_available,
        );
    }

    let metrics: Vec<CliMetricValue> = cfg
        .selectors
        .iter()
        .map(|sel| CliMetricValue {
            name: sel.name,
            label: sel.label,
            // A selector the space cannot back — any static metric on
            // a history-only fallback, or any `history.*` metric on a
            // file without recorded Git history — has no measurable
            // value, and the missing-key `0.0` fallback must not rank
            // the file on a fabricated one.
            value: if crate::history_metrics::selector_available(
                sel.name,
                statics_available,
                history_available,
            ) {
                Some(read_selector_metric(&root, sel))
            } else {
                None
            },
        })
        .collect();

    cfg.results
        .lock()
        .expect("top-offenders results mutex poisoned")
        .push(FileOffender { path, metrics });

    Ok(())
}

fn cmp_offenders(a: &FileOffender, b: &FileOffender, selectors: &[CliMetricSelector]) -> Ordering {
    for (i, sel) in selectors.iter().enumerate() {
        let av = a.metrics.get(i).and_then(|m| m.value);
        let bv = b.metrics.get(i).and_then(|m| m.value);
        let ord = match (av, bv) {
            // An uncomputable value ranks as least concerning under
            // either polarity — absent, not zero.
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
            (Some(av), Some(bv)) => {
                let base = av.total_cmp(&bv);
                match sel.polarity {
                    SelectorPolarity::LowerIsBetter => base.reverse(),
                    SelectorPolarity::HigherIsBetter => base,
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.path.cmp(&b.path)
}

fn print_json_offenders(offenders: &[FileOffender]) {
    let json =
        serde_json::to_string_pretty(offenders).expect("offender list is always serializable");
    writeln!(std::io::stdout().lock(), "{json}").expect("failed to write to stdout");
}

fn print_markdown_offenders(offenders: &[FileOffender], selectors: &[CliMetricSelector]) {
    let mut out = String::new();

    if offenders.is_empty() {
        out.push_str("## Top Offenders\n\nNo matching files found.\n");
        write!(std::io::stdout().lock(), "{out}").expect("failed to write to stdout");
        return;
    }

    let metric_list = selectors
        .iter()
        .map(|s| s.name)
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("## Top Offenders (by {metric_list})\n\n"));

    out.push_str("| File |");
    for sel in selectors {
        out.push_str(&format!(" {} |", sel.label));
    }
    out.push('\n');

    out.push_str("|---|");
    for _ in selectors {
        out.push_str("---:|");
    }
    out.push('\n');

    for o in offenders {
        out.push_str(&format!("| {} |", o.path.display()));
        for mv in &o.metrics {
            out.push_str(&format!(
                " {} |",
                match mv.value {
                    Some(v) => format_value(v),
                    // Uncomputable for this file (see `CliMetricValue::value`).
                    None => "n/a".to_string(),
                }
            ));
        }
        out.push('\n');
    }

    write!(std::io::stdout().lock(), "{out}").expect("failed to write to stdout");
}

fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v == v.trunc() && v.abs() < 1e18 {
        format!("{}", v as i64)
    } else {
        format!("{:.2}", v)
    }
}

fn resolve_num_jobs(requested: Option<usize>, available: Option<usize>) -> usize {
    requested.unwrap_or_else(|| available.unwrap_or(2))
}

/// Resolve a `--language` CLI override (e.g. `ps1`, `python`) to the
/// `Language` enum. The legacy spelling is accepted via the
/// `language_aliases()` table in `mehen-core`.
fn parse_language_override(raw: &str) -> Option<Language> {
    raw.parse::<Language>().ok()
}

pub fn run_top_offenders(opts: TopOffendersOpts) {
    let selectors = parse_metric_selectors(&opts.metrics);
    if selectors.is_empty() {
        log::error!("No valid metrics selected. See `mehen top-offenders --help`.");
        process::exit(1);
    }

    let language_override = match opts.language_type.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => match parse_language_override(raw) {
            Some(language) => Some(language),
            None => {
                log::error!("Unknown language type '{raw}'.");
                process::exit(1);
            }
        },
        None => None,
    };

    let num_jobs = resolve_num_jobs(
        opts.num_jobs,
        available_parallelism().ok().map(|threads| threads.get()),
    );

    let include = mk_globset(opts.include);
    let exclude = mk_globset(opts.exclude);

    // A `history.*` metric was explicitly requested: the ranking is
    // meaningless without the repository walk, so failing to load it
    // is a hard error rather than a silent all-zeros column.
    let history = if crate::history_metrics::names_want_history(selectors.iter().map(|s| s.name)) {
        let histories = RepoHistories::new();
        for root in &opts.paths {
            if let Err(e) = histories.load_root(root) {
                log::error!("history metrics unavailable for {}: {e}", root.display());
                process::exit(1);
            }
        }
        Some(Arc::new(histories))
    } else {
        None
    };

    let results: Arc<Mutex<Vec<FileOffender>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(AnalyzerRegistry::default_set());

    let cfg = TopOffendersCfg {
        selectors: selectors.clone(),
        language_override,
        registry,
        history: history.clone(),
        results: results.clone(),
    };

    let files_data = FilesData {
        include,
        exclude,
        paths: opts.paths,
        respect_ignores: !opts.no_ignore,
    };

    if let Err(e) = ConcurrentRunner::new(num_jobs, act_on_file).run(cfg, files_data) {
        log::error!("{e}");
        process::exit(1);
    }

    // A history metric was explicitly requested; a repository whose
    // history could not be loaded during lazy discovery (e.g. a
    // shallow nested clone found mid-traversal) means part of the
    // ranking silently ran on absent history — that must fail the
    // command, matching the eager root-load semantics.
    if let Some(histories) = history.as_ref() {
        let failures = histories.take_failures();
        if !failures.is_empty() {
            for (location, message) in &failures {
                log::error!(
                    "history metrics unavailable for {}: {message}",
                    location.display()
                );
            }
            process::exit(1);
        }
    }

    let mut offenders = Arc::try_unwrap(results)
        .expect("results Arc still has outstanding references")
        .into_inner()
        .expect("results mutex poisoned");

    offenders.sort_by(|a, b| cmp_offenders(a, b, &selectors));
    offenders.truncate(opts.max_results);

    match opts.output_format {
        TopOffendersFormat::Json => print_json_offenders(&offenders),
        TopOffendersFormat::Markdown => print_markdown_offenders(&offenders, &selectors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::Language;

    fn entry(path: &str, scores: &[f64]) -> TopOffenderEntry {
        TopOffenderEntry {
            path: Utf8PathBuf::from(path),
            language: Language::Rust,
            scores: scores.iter().copied().map(Some).collect(),
        }
    }

    const HIW2: &[Polarity] = &[Polarity::HigherIsWorse, Polarity::HigherIsWorse];
    const HIW3: &[Polarity] = &[
        Polarity::HigherIsWorse,
        Polarity::HigherIsWorse,
        Polarity::HigherIsWorse,
    ];

    #[test]
    fn primary_score_ranks_first() {
        let mut xs = [entry("a.rs", &[10.0, 0.0]), entry("b.rs", &[20.0, 0.0])];
        xs.sort_by(|a, b| cmp_entries(a, b, HIW2));
        assert_eq!(xs[0].path, "b.rs");
        assert_eq!(xs[1].path, "a.rs");
    }

    #[test]
    fn secondary_selector_breaks_ties_on_primary() {
        // All three files tie on primary `loc.lloc = 100.0`. The
        // secondary `cognitive` selector must determine the order;
        // the file with the highest cognitive score is most
        // concerning.
        let mut xs = [
            entry("a.rs", &[100.0, 5.0]),
            entry("b.rs", &[100.0, 30.0]),
            entry("c.rs", &[100.0, 12.0]),
        ];
        xs.sort_by(|a, b| cmp_entries(a, b, HIW2));
        assert_eq!(xs[0].path, "b.rs");
        assert_eq!(xs[1].path, "c.rs");
        assert_eq!(xs[2].path, "a.rs");
    }

    #[test]
    fn tertiary_selector_breaks_ties_on_secondary() {
        let mut xs = [
            entry("a.rs", &[10.0, 5.0, 1.0]),
            entry("b.rs", &[10.0, 5.0, 9.0]),
            entry("c.rs", &[10.0, 5.0, 4.0]),
        ];
        xs.sort_by(|a, b| cmp_entries(a, b, HIW3));
        assert_eq!(xs[0].path, "b.rs");
        assert_eq!(xs[1].path, "c.rs");
        assert_eq!(xs[2].path, "a.rs");
    }

    #[test]
    fn fully_tied_falls_through_to_path() {
        let mut xs = [
            entry("zzz.rs", &[42.0, 7.0]),
            entry("aaa.rs", &[42.0, 7.0]),
            entry("mmm.rs", &[42.0, 7.0]),
        ];
        xs.sort_by(|a, b| cmp_entries(a, b, HIW2));
        assert_eq!(xs[0].path, "aaa.rs");
        assert_eq!(xs[1].path, "mmm.rs");
        assert_eq!(xs[2].path, "zzz.rs");
    }

    #[test]
    fn nan_score_is_treated_as_equal() {
        let mut xs = [
            entry("a.rs", &[f64::NAN, 5.0]),
            entry("b.rs", &[f64::NAN, 30.0]),
        ];
        xs.sort_by(|a, b| cmp_entries(a, b, HIW2));
        // NaN primaries compare equal; secondary breaks the tie.
        assert_eq!(xs[0].path, "b.rs");
        assert_eq!(xs[1].path, "a.rs");
    }

    #[test]
    fn uncomputable_score_ranks_least_concerning_under_either_polarity() {
        // `None` marks a score that could not be computed (e.g. a
        // history composite without static analysis). It must sort
        // *after* every real value — including under higher-is-better
        // polarity, where the old `0.0` fallback would have ranked
        // the file as the very worst offender.
        let none_entry = |path: &str| TopOffenderEntry {
            path: Utf8PathBuf::from(path),
            language: Language::Rust,
            scores: vec![None],
        };
        for polarity in [Polarity::HigherIsWorse, Polarity::HigherIsBetter] {
            let mut xs = [
                none_entry("na.rs"),
                entry("real_low.rs", &[1.0]),
                entry("real_high.rs", &[50.0]),
            ];
            xs.sort_by(|a, b| cmp_entries(a, b, &[polarity]));
            assert_eq!(
                xs[2].path, "na.rs",
                "uncomputable score must rank last for {polarity:?}"
            );
        }
    }

    #[test]
    fn higher_is_better_metric_sorts_smallest_first() {
        // For maintainability index a low value is the worst offender,
        // so `bad.rs` (mi = 10) must rank above `good.rs` (mi = 120).
        let mut xs = [
            entry("good.rs", &[120.0]),
            entry("bad.rs", &[10.0]),
            entry("mid.rs", &[60.0]),
        ];
        xs.sort_by(|a, b| cmp_entries(a, b, &[Polarity::HigherIsBetter]));
        assert_eq!(xs[0].path, "bad.rs");
        assert_eq!(xs[1].path, "mid.rs");
        assert_eq!(xs[2].path, "good.rs");
    }

    #[test]
    fn mixed_polarities_sort_each_axis_independently() {
        // Primary loc.lloc (lower-is-worse): 200 > 10, so high-LOC
        // files rank first. Secondary mi (higher-is-worse): when LOC
        // ties, the file with the *lower* mi should rank first.
        let mut xs = [
            entry("low_loc_high_mi.rs", &[10.0, 120.0]),
            entry("high_loc_high_mi.rs", &[200.0, 120.0]),
            entry("high_loc_low_mi.rs", &[200.0, 30.0]),
        ];
        xs.sort_by(|a, b| cmp_entries(a, b, &[Polarity::HigherIsWorse, Polarity::HigherIsBetter]));
        assert_eq!(xs[0].path, "high_loc_low_mi.rs");
        assert_eq!(xs[1].path, "high_loc_high_mi.rs");
        assert_eq!(xs[2].path, "low_loc_high_mi.rs");
    }

    #[test]
    fn default_polarity_treats_mi_variants_as_higher_is_better() {
        for s in ["mi.original", "mi.sei", "mi.visual_studio", "mi"] {
            assert_eq!(
                default_polarity_for(&sel(s)),
                Polarity::HigherIsBetter,
                "selector {s}",
            );
        }
    }

    #[test]
    fn default_polarity_treats_other_metrics_as_higher_is_worse() {
        for s in [
            "cyclomatic",
            "cognitive",
            "loc.lloc",
            "halstead.volume",
            "abc",
            "nom.functions",
        ] {
            assert_eq!(
                default_polarity_for(&sel(s)),
                Polarity::HigherIsWorse,
                "selector {s}",
            );
        }
    }

    fn space_with_metrics(pairs: &[(&str, f64)]) -> mehen_core::MetricSpace {
        use mehen_core::{MetricSpace, SourceSpan, SpaceId, SpaceKind};
        let mut space = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        for (k, v) in pairs {
            space.metrics.insert(MetricKey::new(*k), *v);
        }
        space
    }

    fn sel(s: &str) -> MetricSelector {
        s.parse().unwrap()
    }

    #[test]
    fn root_aggregator_reads_bare_key() {
        let space = space_with_metrics(&[("loc.lloc", 42.0), ("loc.lloc.max", 999.0)]);
        assert_eq!(read_metric(&sel("loc.lloc"), &space), 42.0);
    }

    #[test]
    fn sql_quality_scores_are_higher_is_better_for_ranking() {
        // Regression: `default_polarity_for` only knew `mi.*`, so the SQL
        // quality scores were ranked higher-is-worse — surfacing the
        // *healthiest* SQL files as the top offenders (Codex P2).
        assert_eq!(
            default_polarity_for(&sel("sql.maintainability_index")),
            Polarity::HigherIsBetter
        );
        assert_eq!(
            default_polarity_for(&sel("sql.modularity_health")),
            Polarity::HigherIsBetter
        );
        // A risk score stays higher-is-worse (larger = more offending).
        assert_eq!(
            default_polarity_for(&sel("sql.change_risk_score")),
            Polarity::HigherIsWorse
        );
        // mi.* is unchanged.
        assert_eq!(
            default_polarity_for(&sel("mi.visual_studio")),
            Polarity::HigherIsBetter
        );
    }

    #[test]
    fn sum_aggregator_reads_sum_suffixed_key() {
        let space = space_with_metrics(&[
            ("cyclomatic", 1.0),
            ("cyclomatic.sum", 17.0),
            ("cyclomatic.max", 9.0),
        ]);
        assert_eq!(read_metric(&sel("cyclomatic.sum"), &space), 17.0);
    }

    #[test]
    fn min_aggregator_reads_min_suffixed_key() {
        let space = space_with_metrics(&[
            ("loc.lloc", 100.0),
            ("loc.lloc.min", 3.0),
            ("loc.lloc.max", 50.0),
        ]);
        assert_eq!(read_metric(&sel("loc.lloc.min"), &space), 3.0);
    }

    #[test]
    fn max_aggregator_reads_max_suffixed_key() {
        let space = space_with_metrics(&[
            ("loc.lloc", 100.0),
            ("loc.lloc.min", 3.0),
            ("loc.lloc.max", 50.0),
        ]);
        assert_eq!(read_metric(&sel("loc.lloc.max"), &space), 50.0);
    }

    #[test]
    fn avg_aggregator_prefers_avg_then_average() {
        // `cyclomatic` publishes `.avg`; `cognitive` publishes
        // `.average`. The aggregator must locate either spelling so
        // selectors written `cognitive.avg` still resolve to the
        // analyzer's `cognitive.average` value.
        let cyclomatic = space_with_metrics(&[("cyclomatic.avg", 2.5)]);
        assert_eq!(read_metric(&sel("cyclomatic.avg"), &cyclomatic), 2.5);

        let cognitive = space_with_metrics(&[("cognitive.average", 3.5)]);
        assert_eq!(read_metric(&sel("cognitive.avg"), &cognitive), 3.5);
    }

    #[test]
    fn missing_aggregated_key_falls_back_to_zero() {
        // When the analyzer didn't publish the requested aggregation,
        // matches the existing root-key contract: 0.0 instead of
        // panicking, so a single missing metric doesn't break the
        // whole rank pass.
        let space = space_with_metrics(&[("loc.lloc", 100.0)]);
        assert_eq!(read_metric(&sel("loc.lloc.max"), &space), 0.0);
    }

    #[test]
    fn min_max_aggregators_resolve_underscore_subbucket_keys() {
        // Regression: `mehen-metrics::state::publish_nom` writes
        // `nom.functions_min`, `nom.functions_max`, `nom.closures_min`,
        // `nom.closures_max` (underscore suffixes), and `publish_abc` /
        // `publish_npa` / `publish_npm` / `publish_nargs` follow the
        // same convention for their sub-bucket aggregates. Pre-fix the
        // suffix lookup only tried the dotted form
        // (`nom.functions.max`), so any selector targeting one of
        // those buckets — `nom.functions.max`, `abc.assignments.min`,
        // `npa.classes.average`, etc. — silently read 0.0. That
        // misordered top-offenders rankings and suppressed
        // diff-threshold violations whenever users gated on a
        // sub-bucket aggregate.
        let space = space_with_metrics(&[
            ("nom.functions", 12.0),
            ("nom.functions_min", 1.0),
            ("nom.functions_max", 7.0),
            ("nom.functions_average", 3.5),
            ("nom.closures_max", 4.0),
            ("abc.assignments_max", 9.0),
            ("npa.classes_average", 2.25),
            ("nargs.functions_max", 6.0),
        ]);
        assert_eq!(read_metric(&sel("nom.functions.min"), &space), 1.0);
        assert_eq!(read_metric(&sel("nom.functions.max"), &space), 7.0);
        assert_eq!(read_metric(&sel("nom.functions.avg"), &space), 3.5);
        assert_eq!(read_metric(&sel("nom.closures.max"), &space), 4.0);
        assert_eq!(read_metric(&sel("abc.assignments.min"), &space), 0.0);
        assert_eq!(read_metric(&sel("abc.assignments.max"), &space), 9.0);
        assert_eq!(read_metric(&sel("npa.classes.avg"), &space), 2.25);
        assert_eq!(read_metric(&sel("nargs.functions.max"), &space), 6.0);
    }

    #[test]
    fn dotted_form_takes_precedence_over_underscore() {
        // If both forms exist, the dotted form wins — `cyclomatic.max`
        // is the canonical convention; underscore is only a fallback
        // for the sub-bucket families. This guards against a future
        // analyzer bug where a publisher accidentally writes both
        // forms with different values: the canonical key still drives
        // ranking.
        let space = space_with_metrics(&[("nom.functions.max", 11.0), ("nom.functions_max", 99.0)]);
        assert_eq!(read_metric(&sel("nom.functions.max"), &space), 11.0);
    }

    #[test]
    fn rank_top_offenders_ranks_history_selectors_on_real_values() {
        // The exported API must honor `history.*` selectors just like
        // the CLI path — a library caller supplying
        // `history.commit_frequency` gets a history-based ranking, not
        // all zeros in alphabetical order.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "Mehen Test")
                .env("GIT_AUTHOR_EMAIL", "test@mehen.invalid")
                .env("GIT_COMMITTER_NAME", "Mehen Test")
                .env("GIT_COMMITTER_EMAIL", "test@mehen.invalid")
                .output()
                .expect("failed to run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        // aaa_calm.py sorts first alphabetically but has one commit;
        // zzz_busy.py has two — the history ranking must invert the
        // alphabetical order.
        std::fs::write(dir.path().join("aaa_calm.py"), "a = 1\n").unwrap();
        std::fs::write(dir.path().join("zzz_busy.py"), "z = 1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        std::fs::write(dir.path().join("zzz_busy.py"), "z = 1\ny = 2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "two"]);

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("history.commit_frequency")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        assert!(report.analysis_errors.is_empty(), "no history-load errors");
        let ranked: Vec<(&str, f64)> = report
            .entries
            .iter()
            .map(|e| {
                (
                    e.path.file_name().unwrap_or(""),
                    e.scores[0].expect("score computed"),
                )
            })
            .collect();
        assert_eq!(
            ranked,
            vec![("zzz_busy.py", 2.0), ("aaa_calm.py", 1.0)],
            "history selector must drive the ranking"
        );
    }

    #[test]
    fn rank_top_offenders_ranks_undecodable_files_on_history() {
        // A recognized file whose contents static analysis cannot
        // decode still has repository history — the exported API must
        // rank it (empty metric space + injection) instead of
        // silently dropping it.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "Mehen Test")
                .env("GIT_AUTHOR_EMAIL", "test@mehen.invalid")
                .env("GIT_COMMITTER_NAME", "Mehen Test")
                .env("GIT_COMMITTER_EMAIL", "test@mehen.invalid")
                .output()
                .expect("failed to run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("latin.py"), b"# caf\xe9\nx = 1\n").unwrap();
        std::fs::write(dir.path().join("plain.py"), "y = 1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        std::fs::write(dir.path().join("latin.py"), b"# caf\xe9\nx = 1\nz = 2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "two"]);

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("history.commit_frequency")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        let ranked: Vec<(&str, f64)> = report
            .entries
            .iter()
            .map(|e| {
                (
                    e.path.file_name().unwrap_or(""),
                    e.scores[0].expect("score computed"),
                )
            })
            .collect();
        assert_eq!(
            ranked,
            vec![("latin.py", 2.0), ("plain.py", 1.0)],
            "undecodable file must rank on its history"
        );
    }

    #[test]
    fn rank_top_offenders_reports_composites_as_uncomputable_without_statics() {
        // The static-dependent composites (`history.hotspot`,
        // `history.churn.relative`) cannot be valued for a file whose
        // static analysis is unavailable — the score must surface as
        // `None` (JSON `null`) and rank least concerning, not as a
        // fabricated `0.0` read through the missing-key fallback.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "Mehen Test")
                .env("GIT_AUTHOR_EMAIL", "test@mehen.invalid")
                .env("GIT_COMMITTER_NAME", "Mehen Test")
                .env("GIT_COMMITTER_EMAIL", "test@mehen.invalid")
                .output()
                .expect("failed to run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        // latin.py is undecodable (Latin-1) but *busier* in history;
        // plain.py decodes and has a real (possibly zero) hotspot.
        std::fs::write(dir.path().join("latin.py"), b"# caf\xe9\nx = 1\n").unwrap();
        std::fs::write(dir.path().join("plain.py"), "y = 1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        std::fs::write(dir.path().join("latin.py"), b"# caf\xe9\nx = 1\nz = 2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "two"]);

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![
                sel("history.hotspot"),
                sel("history.churn.relative"),
                sel("loc.lloc"),
            ],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        let latin = report
            .entries
            .iter()
            .find(|e| e.path.file_name() == Some("latin.py"))
            .expect("undecodable file still ranked (history-only)");
        assert_eq!(
            latin.scores,
            vec![None, None, None],
            "composites *and* plain statics must be uncomputable, not zero"
        );
        let plain = report
            .entries
            .iter()
            .find(|e| e.path.file_name() == Some("plain.py"))
            .expect("decodable file ranked");
        assert!(
            plain.scores.iter().all(|s| s.is_some()),
            "statically analyzed file keeps real composite values"
        );
        // Least concerning: the uncomputable entry sorts after the
        // real (even zero-valued) one.
        assert_eq!(
            report.entries.last().map(|e| e.path.file_name()),
            Some(Some("latin.py"))
        );
    }

    #[test]
    fn rank_top_offenders_marks_history_unavailable_for_untracked_files() {
        // An untracked file has statics but no recorded Git history:
        // its `history.*` scores must read as uncomputable (`None`)
        // and rank least concerning — `history.age_months = 0` or
        // `history.ownership = 0` would otherwise crown it the worst
        // offender and crowd real tracked files out of the ranking.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "Mehen Test")
                .env("GIT_AUTHOR_EMAIL", "test@mehen.invalid")
                .env("GIT_COMMITTER_NAME", "Mehen Test")
                .env("GIT_COMMITTER_EMAIL", "test@mehen.invalid")
                .output()
                .expect("failed to run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("tracked.py"), "x = 1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        // Present in the workspace only.
        std::fs::write(dir.path().join("untracked.py"), "y = 1\n").unwrap();

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("history.commit_frequency")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        let score = |name: &str| {
            report
                .entries
                .iter()
                .find(|e| e.path.file_name() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing from {:?}", report.entries))
                .scores[0]
        };
        assert_eq!(score("tracked.py"), Some(1.0));
        assert_eq!(
            score("untracked.py"),
            None,
            "no Git history means no measurable history score"
        );
        assert_eq!(
            report.entries.last().map(|e| e.path.file_name()),
            Some(Some("untracked.py")),
            "unmeasured files rank least concerning"
        );
    }

    #[test]
    fn rank_top_offenders_rejects_unknown_history_selectors() {
        // The engine boundary accepts arbitrary selector strings: a
        // typo'd history key must surface as an analysis error and
        // score as uncomputable — never as an all-zero ranking after
        // a pointless repository walk.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("plain.py"), "y = 1\n").unwrap();

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![
                sel("history.commit_frequncy"),
                // Valid key, unsupported aggregator: enrichment
                // publishes root keys only.
                sel("history.commit_frequency.max"),
            ],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        assert!(
            report.analysis_errors.iter().any(|record| {
                record
                    .diagnostics
                    .iter()
                    .any(|d| d.code == "engine.unknown_metric")
            }),
            "the typo must be surfaced: {:?}",
            report.analysis_errors
        );
        let plain = report
            .entries
            .iter()
            .find(|e| e.path.file_name() == Some("plain.py"))
            .expect("statically analyzed file still listed");
        assert_eq!(
            plain.scores,
            vec![None, None],
            "unresolvable selectors must score as uncomputable, not zero"
        );
    }

    #[test]
    fn rank_top_offenders_skips_files_with_blocking_diagnostics() {
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        // Valid Python file: should appear in the offender list.
        std::fs::write(
            dir.path().join("ok.py"),
            "def f():\n    if True:\n        return 1\n",
        )
        .unwrap();
        // Syntax error: ruff returns Ok(LanguageAnalysis) with an
        // Error-severity diagnostic and a partial tree. Pre-fix this
        // file would be ranked alongside ok.py with bogus partial
        // metrics; post-fix it must be skipped.
        std::fs::write(dir.path().join("broken.py"), "def f(:\n    return 1\n").unwrap();

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("loc.lloc")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        let paths: Vec<&str> = report
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap_or(""))
            .collect();
        assert!(
            paths.contains(&"ok.py"),
            "expected ok.py in entries, got {paths:?}"
        );
        assert!(
            !paths.contains(&"broken.py"),
            "broken.py should be skipped due to blocking diagnostic, got {paths:?}"
        );
    }

    #[test]
    fn rank_top_offenders_skips_gitignored_and_attributed_files() {
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        gix::init(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
generated.py linguist-generated
vendored.py linguist-vendored
binary.py binary
",
        )
        .unwrap();
        std::fs::write(dir.path().join("kept.py"), "x = 1\n").unwrap();
        std::fs::write(
            dir.path().join("node_modules/generated.py"),
            "def generated():\n    if True:\n        return 1\n",
        )
        .unwrap();
        for name in ["generated.py", "vendored.py", "binary.py"] {
            std::fs::write(
                dir.path().join(name),
                "def excluded():\n    if True:\n        return 1\n",
            )
            .unwrap();
        }

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("loc.lloc")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        let names: Vec<&str> = report
            .entries
            .iter()
            .filter_map(|entry| entry.path.file_name())
            .collect();

        assert_eq!(names, vec!["kept.py"]);
    }

    #[test]
    fn rank_top_offenders_dedupes_overlapping_roots() {
        // Regression: when callers pass overlapping roots (a directory
        // plus a child directory, or a directory plus an explicit file
        // inside it), `rank_top_offenders` previously analyzed and
        // pushed each matching file once per root, crowding out other
        // files at `max_results` truncation. Post-fix the dedup set
        // collapses every spelling of the same canonical path to one
        // entry.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.py"), "x = 1\n").unwrap();
        std::fs::write(sub.join("b.py"), "y = 2\n").unwrap();

        let outer = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let inner = Utf8PathBuf::from_path_buf(sub.clone()).unwrap();
        let explicit_file = Utf8PathBuf::from_path_buf(sub.join("a.py")).unwrap();

        let input = TopOffendersInput {
            // Overlapping inputs: root + child directory + explicit
            // file inside the child. Without dedup, `a.py` appears
            // three times in `entries`.
            paths: vec![outer, inner, explicit_file],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("loc.lloc")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        let names: Vec<&str> = report
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap_or(""))
            .collect();

        let a_count = names.iter().filter(|n| **n == "a.py").count();
        let b_count = names.iter().filter(|n| **n == "b.py").count();
        assert_eq!(
            a_count, 1,
            "a.py must be ranked exactly once, got {names:?}"
        );
        assert_eq!(
            b_count, 1,
            "b.py must be ranked exactly once, got {names:?}"
        );
        assert_eq!(
            report.entries.len(),
            2,
            "expected 2 unique offenders across overlapping roots, got {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rank_top_offenders_keeps_symlink_aliases_distinct() {
        use std::os::unix::fs::symlink;

        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("source.py"), "x = 1\n").unwrap();
        symlink("source.py", dir.path().join("alias.py")).unwrap();

        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec![sel("loc.lloc")],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);

        // A tracked symlink is its own repository entry with its own
        // (empty) history — collapsing it into its target would make
        // history rankings depend on traversal order and diverge from
        // the CLI path, which reports both identities. Dedup still
        // collapses different *spellings* of one path (overlapping
        // roots, directory symlinks) via parent canonicalization.
        let mut names: Vec<&str> = report
            .entries
            .iter()
            .filter_map(|e| e.path.file_name())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["alias.py", "source.py"],
            "a file and its symlink alias are distinct identities"
        );
    }

    #[test]
    fn walk_paths_applies_exclude_patterns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kept = dir.path().join("kept.py");
        let skipped = dir.path().join("skipped.py");
        std::fs::write(&kept, "x = 1\n").unwrap();
        std::fs::write(&skipped, "x = 1\n").unwrap();

        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let result = walk_paths(
            std::slice::from_ref(&root),
            &[],
            &["**/skipped.py".to_string()],
        );
        let names: Vec<&str> = result.iter().filter_map(|p| p.file_name()).collect();
        assert!(names.contains(&"kept.py"), "expected kept.py in {names:?}");
        assert!(
            !names.contains(&"skipped.py"),
            "skipped.py should be excluded, got {names:?}"
        );
    }

    #[test]
    fn walk_paths_applies_include_patterns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let py = dir.path().join("a.py");
        let rs = dir.path().join("a.rs");
        std::fs::write(&py, "x = 1\n").unwrap();
        std::fs::write(&rs, "fn main() {}\n").unwrap();

        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let result = walk_paths(std::slice::from_ref(&root), &["**/*.py".to_string()], &[]);
        let names: Vec<&str> = result.iter().filter_map(|p| p.file_name()).collect();
        assert!(names.contains(&"a.py"), "expected a.py in {names:?}");
        assert!(
            !names.contains(&"a.rs"),
            "a.rs should not be included, got {names:?}"
        );
    }

    #[test]
    fn walk_paths_empty_filters_keep_all_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn main() {}\n").unwrap();

        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let result = walk_paths(std::slice::from_ref(&root), &[], &[]);
        let names: Vec<&str> = result.iter().filter_map(|p| p.file_name()).collect();
        assert!(names.contains(&"a.py"));
        assert!(names.contains(&"b.rs"));
    }

    #[test]
    fn walk_paths_filters_a_single_file_root() {
        // When `root` itself is a file, the include/exclude patterns
        // still apply: an excluded file must not appear in the list.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vendored.py");
        std::fs::write(&path, "x = 1\n").unwrap();
        let root = Utf8PathBuf::from_path_buf(path).unwrap();
        let result = walk_paths(
            std::slice::from_ref(&root),
            &[],
            &["**/vendored.py".to_string()],
        );
        assert!(
            result.is_empty(),
            "single-file root must respect exclude, got {result:?}"
        );
    }

    // ── pre-1.0 CLI orchestrator tests ─────────────────────────────────

    fn cli_selector(name: &'static str, polarity: SelectorPolarity) -> CliMetricSelector {
        CliMetricSelector {
            name,
            label: name,
            polarity,
        }
    }

    fn offender(path: &str, values: &[(&'static str, f64)]) -> FileOffender {
        FileOffender {
            path: PathBuf::from(path),
            metrics: values
                .iter()
                .map(|(n, v)| CliMetricValue {
                    name: n,
                    label: n,
                    value: Some(*v),
                })
                .collect(),
        }
    }

    #[test]
    fn cli_lower_is_better_puts_largest_value_first() {
        let selectors = [cli_selector("loc.lloc", SelectorPolarity::LowerIsBetter)];
        let mut xs = [
            offender("small.rs", &[("loc.lloc", 10.0)]),
            offender("huge.rs", &[("loc.lloc", 1000.0)]),
            offender("medium.rs", &[("loc.lloc", 100.0)]),
        ];
        xs.sort_by(|a, b| cmp_offenders(a, b, &selectors));
        assert_eq!(xs[0].path, PathBuf::from("huge.rs"));
        assert_eq!(xs[1].path, PathBuf::from("medium.rs"));
        assert_eq!(xs[2].path, PathBuf::from("small.rs"));
    }

    #[test]
    fn cli_higher_is_better_puts_smallest_value_first() {
        let selectors = [cli_selector(
            "mi.visual_studio",
            SelectorPolarity::HigherIsBetter,
        )];
        let mut xs = [
            offender("good.rs", &[("mi", 120.0)]),
            offender("bad.rs", &[("mi", 10.0)]),
            offender("mid.rs", &[("mi", 60.0)]),
        ];
        xs.sort_by(|a, b| cmp_offenders(a, b, &selectors));
        assert_eq!(xs[0].path, PathBuf::from("bad.rs"));
        assert_eq!(xs[1].path, PathBuf::from("mid.rs"));
        assert_eq!(xs[2].path, PathBuf::from("good.rs"));
    }

    #[test]
    fn cli_ties_on_primary_metric_fall_through_to_secondary() {
        let selectors = [
            cli_selector("loc.lloc", SelectorPolarity::LowerIsBetter),
            cli_selector("cognitive", SelectorPolarity::LowerIsBetter),
        ];
        let mut xs = [
            offender("a.rs", &[("loc.lloc", 100.0), ("cognitive", 5.0)]),
            offender("b.rs", &[("loc.lloc", 100.0), ("cognitive", 30.0)]),
            offender("c.rs", &[("loc.lloc", 50.0), ("cognitive", 999.0)]),
        ];
        xs.sort_by(|a, b| cmp_offenders(a, b, &selectors));
        assert_eq!(xs[0].path, PathBuf::from("b.rs"));
        assert_eq!(xs[1].path, PathBuf::from("a.rs"));
        assert_eq!(xs[2].path, PathBuf::from("c.rs"));
    }

    #[test]
    fn cli_all_tied_breaks_by_path_for_determinism() {
        let selectors = [cli_selector("loc.lloc", SelectorPolarity::LowerIsBetter)];
        let mut xs = [
            offender("zzz.rs", &[("loc.lloc", 42.0)]),
            offender("aaa.rs", &[("loc.lloc", 42.0)]),
            offender("mmm.rs", &[("loc.lloc", 42.0)]),
        ];
        xs.sort_by(|a, b| cmp_offenders(a, b, &selectors));
        assert_eq!(xs[0].path, PathBuf::from("aaa.rs"));
        assert_eq!(xs[1].path, PathBuf::from("mmm.rs"));
        assert_eq!(xs[2].path, PathBuf::from("zzz.rs"));
    }

    #[test]
    fn cli_mixed_polarities_sort_each_axis_independently() {
        let selectors = [
            cli_selector("loc.lloc", SelectorPolarity::LowerIsBetter),
            cli_selector("mi.visual_studio", SelectorPolarity::HigherIsBetter),
        ];
        let mut xs = [
            offender("low_loc_high_mi.rs", &[("loc", 10.0), ("mi", 120.0)]),
            offender("high_loc_high_mi.rs", &[("loc", 200.0), ("mi", 120.0)]),
            offender("high_loc_low_mi.rs", &[("loc", 200.0), ("mi", 30.0)]),
        ];
        xs.sort_by(|a, b| cmp_offenders(a, b, &selectors));
        assert_eq!(xs[0].path, PathBuf::from("high_loc_low_mi.rs"));
        assert_eq!(xs[1].path, PathBuf::from("high_loc_high_mi.rs"));
        assert_eq!(xs[2].path, PathBuf::from("low_loc_high_mi.rs"));
    }

    #[test]
    fn cli_format_value_renders_integers_without_decimals() {
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(1.5), "1.50");
        assert_eq!(format_value(100.567), "100.57");
    }

    #[test]
    fn cli_explicit_num_jobs_is_not_predecremented() {
        assert_eq!(resolve_num_jobs(Some(8), Some(16)), 8);
    }

    #[test]
    fn cli_num_jobs_falls_back_to_conservative_thread_count() {
        assert_eq!(resolve_num_jobs(None, None), 2);
    }

    #[test]
    fn cli_no_ignore_is_opt_in() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            opts: TopOffendersOpts,
        }

        let default =
            <TestCli as clap::Parser>::try_parse_from(["mehen", "--metric", "loc.lloc", "."])
                .unwrap();
        assert!(!default.opts.no_ignore);

        let disabled = <TestCli as clap::Parser>::try_parse_from([
            "mehen",
            "--metric",
            "loc.lloc",
            "--no-ignore",
            ".",
        ])
        .unwrap();
        assert!(disabled.opts.no_ignore);
    }

    #[test]
    fn record_unavailable_emits_warning_record() {
        // Regression: when language detection succeeds but no analyzer
        // is registered (feature-gated build), the file must surface
        // as a non-fatal `analysis_error` so callers can tell that an
        // offender was silently dropped, instead of believing the
        // ranking is complete. Mirrors `mehen-engine::diff`'s
        // `record_unavailable`.
        let mut errors: Vec<AnalysisErrorRecord> = Vec::new();
        record_unavailable(
            &mut errors,
            &Utf8PathBuf::from("src/main.kt"),
            Language::Kotlin,
        );
        assert_eq!(errors.len(), 1);
        let rec = &errors[0];
        assert_eq!(rec.path, Utf8PathBuf::from("src/main.kt"));
        assert_eq!(rec.side, DiffSide::Head);
        assert_eq!(rec.diagnostics.len(), 1);
        assert_eq!(rec.diagnostics[0].code, "engine.analyzer_unavailable");
        assert!(
            rec.diagnostics[0].message.contains("kotlin"),
            "message must name the unavailable language; got: {}",
            rec.diagnostics[0].message
        );
        assert_eq!(
            rec.diagnostics[0].severity,
            mehen_core::DiagnosticSeverity::Warning,
            "unavailable analyzer is non-fatal"
        );
    }

    #[test]
    fn rank_top_offenders_includes_empty_analysis_errors_when_clean() {
        // A clean run with all analyzers available produces an
        // `analysis_errors` field — even when empty — so JSON
        // consumers can rely on its presence.
        use mehen_core::{AnalysisConfig, TopOffendersInput};

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ok.py"), "x = 1\n").unwrap();
        let input = TopOffendersInput {
            paths: vec![Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()],
            include: Vec::new(),
            exclude: Vec::new(),
            selectors: vec!["loc.lloc".parse().unwrap()],
            max_results: 10,
            config: AnalysisConfig::default(),
        };
        let report = rank_top_offenders(input);
        assert!(report.analysis_errors.is_empty());
    }
}
