// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen diff` orchestrator.
//!
//! Walks `mehen-git`'s changed-file list, analyzes each file at base and
//! head, and assembles a `DiffReport` (the post-1.0 [`analyze_diff`]
//! entry point). The pre-1.0 CLI orchestrator [`run_diff`] lives in
//! this same module so the two share the [`has_blocking_diagnostic`]
//! gate. Per the rewrite plan §4.6, per-file analysis is the
//! parallelism unit; the implementation runs serially and follow-up
//! commits will switch to a thread-per-file pool. The Markdown
//! documentation diff renderer in `mehen-report` consumes this report.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use camino::{Utf8Component, Utf8PathBuf};

use mehen_core::{
    AnalysisConfig, DiagnosticSeverity, Language, LanguageAnalysis, MetricSpace, ParseDiagnostic,
    SourceFile, Threshold, ThresholdEvaluation,
};
use mehen_git::{ChangeStatus, GitError};
use mehen_report::github_markdown_docs::{DocDiffFile, DocRenderCtx, render_doc_section};

use crate::ci;
use crate::concurrent_files::mk_globset;
use crate::detection::detect_language;
use crate::git_attributes::GitAttributeFilter;
use crate::history_metrics;
use crate::metric_selector::{
    MetricSelector, Polarity as SelectorPolarity, default_selectors_for_language,
    parse_metric_selectors, read_metric as read_selector_metric,
};
use crate::registry::AnalyzerRegistry;
use crate::top_offenders::read_metric;
use mehen_core::{
    AnalysisErrorRecord, DiffFile, DiffInput, DiffReport, DiffSide, ThresholdViolation,
};

/// Run `mehen diff` against the workspace and produce a report.
///
/// Errors flow through the report's `analysis_errors` array (per rewrite
/// plan review §3.5: `analysis_errors` separate from
/// `threshold_violations`); only IO/git-fatal failures bubble up as
/// `Err` so callers can short-circuit the rendering step.
pub fn analyze_diff(input: DiffInput) -> Result<DiffReport, DiffError> {
    let repo = mehen_git::open_repo().map_err(DiffError::Git)?;
    analyze_diff_in_repo(input, &repo)
}

struct RevisionGitAttributeFilters {
    /// `None` when the base revision doesn't resolve locally (the
    /// push-payload fallback after a force-push): baseline attributes
    /// are unavailable, and deleted rows are then not attribute-
    /// filtered rather than aborting the whole run.
    base: Option<GitAttributeFilter>,
    head: GitAttributeFilter,
}

impl RevisionGitAttributeFilters {
    fn new(
        repo: &gix::Repository,
        from: &str,
        to: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let base = if repo.rev_parse_single(from).is_ok() {
            Some(GitAttributeFilter::from_revision(repo, from)?)
        } else {
            log::warn!(
                "baseline Git attributes unavailable ({from} does not resolve locally); deleted files are not attribute-filtered"
            );
            None
        };
        Ok(Self {
            base,
            head: GitAttributeFilter::from_revision(repo, to)?,
        })
    }

    fn excludes(&mut self, file: &mehen_git::ChangedFile) -> std::io::Result<bool> {
        let filter = if file.status == ChangeStatus::Deleted {
            match self.base.as_mut() {
                Some(base) => base,
                None => return Ok(false),
            }
        } else {
            &mut self.head
        };
        filter.excludes_relative_path(&file.path)
    }

    /// Whether the base revision excludes `path`, for rename-source
    /// eligibility. `None` baseline attributes exclude nothing.
    fn base_excludes(&mut self, path: &Path) -> std::io::Result<bool> {
        match self.base.as_mut() {
            Some(base) => base.excludes_relative_path(path),
            None => Ok(false),
        }
    }

    /// Whether the head revision excludes `path`.
    fn head_excludes(&mut self, path: &Path) -> std::io::Result<bool> {
        self.head.excludes_relative_path(path)
    }
}

/// The result of [`split_boundary_renames`]: the adjusted change list
/// plus the split-rename *deletion* rows whose lineage history is
/// already carried by their paired destination row — injecting the
/// source lineage into those deletions too would count it twice
/// (a `+1` on the destination and a full `-N` on the source).
struct SplitChanges {
    files: Vec<mehen_git::ChangedFile>,
    history_suppressed_deletions: std::collections::HashSet<PathBuf>,
}

/// Split rename pairs whose two sides fall on different sides of a
/// reporting boundary back into a deletion + addition.
///
/// A joined rename row is keyed by its *destination*, so a rename to
/// an unsupported extension (`src/foo.py` → `archive/foo.txt`), out
/// of the selected `--paths` scope, or into git-attribute-excluded
/// territory (destination `linguist-generated` at head) would silently
/// swallow the source file's disappearance — and a rename *across
/// languages* (`.py` → `.rs`) would analyze the old blob with the new
/// language's analyzer. A rename is kept joined only when both sides
/// are selected, detect as the same language, and are
/// attribute-eligible at their own revision (source at base,
/// destination at head); otherwise each eligible side is reported on
/// its own.
fn split_boundary_renames(
    changed: Vec<mehen_git::ChangedFile>,
    selected: &dyn Fn(&Path) -> bool,
    mut attribute_filters: Option<&mut RevisionGitAttributeFilters>,
) -> std::io::Result<SplitChanges> {
    let language_of = |p: &Path| {
        Utf8PathBuf::try_from(p.to_path_buf())
            .ok()
            .and_then(|p| detect_language(&p))
    };
    let mut out = Vec::with_capacity(changed.len());
    let mut history_suppressed_deletions = std::collections::HashSet::new();
    for cf in changed {
        let Some(source) = cf.source_path.clone() else {
            out.push(cf);
            continue;
        };
        // Attribute eligibility is per-side and per-revision: the
        // source lived at base, the destination lives at head. Lookup
        // failures propagate — treating an unreadable historical
        // `.gitattributes` as "eligible" would silently bypass source
        // exclusions and compute metrics from incomplete data.
        let (src_attr_ok, dest_attr_ok) = match attribute_filters.as_deref_mut() {
            Some(filters) => (
                !filters.base_excludes(&source)?,
                !filters.head_excludes(&cf.path)?,
            ),
            None => (true, true),
        };
        let dest_ok = selected(&cf.path) && dest_attr_ok;
        let src_ok = selected(&source) && src_attr_ok;
        let dest_lang = language_of(&cf.path);
        let src_lang = language_of(&source);
        if dest_ok && src_ok && dest_lang.is_some() && dest_lang == src_lang {
            out.push(cf);
            continue;
        }
        let emit_source = src_ok && src_lang.is_some();
        let emit_dest = dest_ok && dest_lang.is_some();
        if emit_source {
            // When the paired destination row is also emitted, it
            // carries the lineage history (via its retained
            // `source_path`); the deletion row then reports the file
            // *leaving this path* for static metrics only.
            if emit_dest {
                history_suppressed_deletions.insert(source.clone());
            }
            out.push(mehen_git::ChangedFile {
                path: source.clone(),
                status: ChangeStatus::Deleted,
                source_path: None,
            });
        }
        if emit_dest {
            out.push(mehen_git::ChangedFile {
                path: cf.path,
                status: ChangeStatus::Added,
                // The static baseline must not cross the boundary (an
                // `Added` row reads no baseline blob), but the rename
                // identity is preserved so *history* enrichment can
                // still compare against the source lineage instead of
                // manufacturing a full-history spike.
                source_path: Some(source),
            });
        }
    }
    Ok(SplitChanges {
        files: out,
        history_suppressed_deletions,
    })
}

fn analyze_diff_in_repo(input: DiffInput, repo: &gix::Repository) -> Result<DiffReport, DiffError> {
    let registry = Arc::new(AnalyzerRegistry::default_set());
    let changed = mehen_git::changed_files(repo, &input.from, &input.to).map_err(DiffError::Git)?;
    let mut git_attribute_filters = RevisionGitAttributeFilters::new(repo, &input.from, &input.to)
        .map_err(|error| {
            DiffError::Git(GitError::Internal(format!(
                "failed to configure Git attribute filtering for {}..{}: {error}",
                input.from, input.to
            )))
        })?;
    let changed = split_boundary_renames(
        changed,
        &|p: &Path| {
            Utf8PathBuf::try_from(p.to_path_buf())
                .map(|utf8| path_is_selected(&utf8, &input.paths))
                .unwrap_or(false)
        },
        Some(&mut git_attribute_filters),
    )
    .map_err(|error| {
        DiffError::Git(GitError::Internal(format!(
            "failed to read Git attributes while splitting renames: {error}"
        )))
    })?
    // Thresholds evaluate the head analysis only; deleted rows have no
    // head side, so the deletion history suppression is irrelevant here.
    .files;
    // Thresholds against `history.*` keys need the repository history
    // at the head revision (thresholds are evaluated against the head
    // analysis only). Walked lazily — the family is opt-in.
    let wants_history = history_metrics::names_want_history(
        input.thresholds.iter().map(|t| t.selector.key.as_str()),
    );
    // `history.*` metrics change with every touch, not only when the
    // endpoint trees differ: a file modified and restored within the
    // range gained commit frequency and churn, and a head-side
    // threshold can newly trip even though the endpoint diff has no
    // row for it. Mirror the CLI diff's range-touch augmentation.
    let changed = if wants_history {
        let mut changed = changed;
        let already: std::collections::HashSet<&PathBuf> =
            changed.iter().map(|cf| &cf.path).collect();
        let extra: Vec<mehen_git::ChangedFile> =
            mehen_git::range_touched_files(repo, &input.from, &input.to)
                .map_err(DiffError::Git)?
                .into_iter()
                .filter(|path| !already.contains(path))
                .filter(|path| {
                    // Markdown routes to the documentation section and
                    // never evaluates source thresholds; undetected
                    // languages are skipped by the loop anyway.
                    Utf8PathBuf::try_from(path.clone())
                        .ok()
                        .and_then(|utf8| detect_language(&utf8))
                        .is_some_and(|language| !matches!(language, Language::Markdown))
                })
                .map(|path| mehen_git::ChangedFile {
                    path,
                    status: ChangeStatus::Modified,
                    source_path: None,
                })
                .collect();
        drop(already);
        changed.extend(extra);
        changed
    } else {
        changed
    };
    let head_history = if wants_history {
        Some(mehen_git::collect_history(repo, &input.to).map_err(DiffError::Git)?)
    } else {
        None
    };

    let mut report = DiffReport {
        schema_version: "1.0".to_string(),
        base: input.from.clone(),
        head: input.to.clone(),
        files: Vec::new(),
        markdown_files: Vec::new(),
        analysis_errors: Vec::new(),
        threshold_violations: Vec::new(),
    };

    for cf in changed {
        // mehen-git returns `PathBuf` paths; convert at the boundary.
        let Ok(utf8_path) = Utf8PathBuf::try_from(cf.path.clone()) else {
            continue;
        };

        // Filter by `--paths` prefix matching.
        if !path_is_selected(&utf8_path, &input.paths) {
            continue;
        }
        if git_attribute_filters.excludes(&cf).map_err(|error| {
            DiffError::Git(GitError::Internal(format!(
                "failed to read Git attributes for {}: {error}",
                cf.path.display()
            )))
        })? {
            continue;
        }

        let Some(language) = detect_language(&utf8_path) else {
            // Skip files we don't recognize.
            continue;
        };

        let base_text = if cf.status == ChangeStatus::Added {
            None
        } else {
            // Renamed files carry the baseline under their old path.
            let base_path = cf.source_path.as_deref().unwrap_or(cf.path.as_path());
            mehen_git::read_blob(repo, &input.from, base_path)
                .map_err(DiffError::Git)?
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let head_text = if cf.status == ChangeStatus::Deleted {
            None
        } else {
            mehen_git::read_blob(repo, &input.to, &cf.path)
                .map_err(DiffError::Git)?
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };

        let analyzer = registry.analyzer_for(language);
        let Some(analyzer) = analyzer else {
            // Language detected but no analyzer registered (feature off);
            // surface as a non-fatal analysis error.
            record_unavailable(&mut report, &utf8_path, language);
            continue;
        };

        let mut head_analysis: Option<LanguageAnalysis> = None;
        for (text, side) in [
            (base_text.as_deref(), DiffSide::Base),
            (head_text.as_deref(), DiffSide::Head),
        ] {
            let Some(text) = text else { continue };
            let source = SourceFile::new(utf8_path.clone(), language, text.to_string());
            match analyzer.analyze(&source, &input.config) {
                Ok(analysis) => {
                    collect_diagnostics(&mut report, &utf8_path, side, &analysis);
                    if matches!(side, DiffSide::Head) {
                        head_analysis = Some(analysis);
                    }
                }
                Err(err) => {
                    report.analysis_errors.push(AnalysisErrorRecord {
                        path: utf8_path.clone(),
                        side,
                        diagnostics: vec![ParseDiagnostic::error(
                            "analysis.error",
                            err.to_string(),
                        )],
                    });
                }
            }
        }

        // Threshold evaluation runs against the head analysis (the
        // post-change state) so policy gates like "head cyclomatic must
        // not exceed 30" mean what callers expect. Files with a
        // blocking diagnostic on the head side are skipped — the
        // analysis is incomplete and folding a partial number into a
        // policy decision would be a false positive.
        if let Some(analysis) = head_analysis.as_mut()
            && !has_blocking_diagnostic(&analysis.diagnostics)
        {
            // Fold `history.*` into the head metric set first so
            // history thresholds read real values.
            if let Some(history) = head_history.as_ref()
                && let Some(fh) = history.file(cf.path.as_path())
            {
                history_metrics::inject_history_metrics(
                    &mut analysis.root.metrics,
                    fh,
                    history.head_seconds,
                );
            }
            evaluate_thresholds(&mut report, &utf8_path, &input.thresholds, analysis);
        }

        if matches!(language, mehen_core::Language::Markdown) {
            report.markdown_files.push(DiffFile { path: utf8_path });
        } else {
            report.files.push(DiffFile { path: utf8_path });
        }
    }

    Ok(report)
}

/// Apply each `Threshold` to the head analysis's metrics and append a
/// `ThresholdViolation` to the report for every rule that fails. Done
/// per-file so the violation entry carries the originating path.
fn evaluate_thresholds(
    report: &mut DiffReport,
    path: &Utf8PathBuf,
    thresholds: &[Threshold],
    analysis: &LanguageAnalysis,
) {
    for threshold in thresholds {
        let actual = read_metric(&threshold.selector, &analysis.root);
        let violated = threshold.violated_by(actual);
        if violated {
            report.threshold_violations.push(ThresholdViolation {
                path: path.to_string(),
                evaluation: ThresholdEvaluation {
                    selector: threshold.selector.clone(),
                    actual,
                    limit: threshold.value,
                    polarity: threshold.polarity,
                    violated: true,
                },
            });
        }
    }
}

fn path_is_selected(path: &Utf8PathBuf, paths: &[Utf8PathBuf]) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|prefix| {
        let normalized = normalize_utf8_filter(prefix);
        // A prefix that normalizes to empty (e.g. `""`, `"."`,
        // `"././/"`) names the repo root — treat it as "match
        // everything", consistent with the CLI path filter.
        normalized.as_str().is_empty() || path.starts_with(&normalized)
    })
}

/// Strip `.` components from a `Utf8PathBuf` filter prefix so callers
/// can pass intuitive scopes like `"./src"` (or even `"."`) without
/// silently dropping every changed file from the report. Mirrors the
/// CLI-side [`normalize_path_filter`] used for the `--paths` flag.
fn normalize_utf8_filter(path: &Utf8PathBuf) -> Utf8PathBuf {
    let mut cleaned = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::Normal(part) => cleaned.push(part),
            other => cleaned.push(other.as_str()),
        }
    }
    cleaned
}

fn collect_diagnostics(
    report: &mut DiffReport,
    path: &Utf8PathBuf,
    side: DiffSide,
    analysis: &LanguageAnalysis,
) {
    // Surface every non-empty diagnostic batch — including
    // warning-only batches. Per plan §9.3 a `Warning` is
    // *informational* (CLI keeps exit 0 unless thresholds fail), but
    // it still has to be visible to callers; otherwise a Ruff-style
    // recoverable parse issue or a markdown cross-reference warning
    // is silently swallowed before it reaches the JSON output.
    // Severity-based exit-code routing happens at the CLI layer
    // against this same `analysis_errors` list, which carries the
    // severity on every entry via `ParseDiagnostic::severity`.
    if analysis.diagnostics.is_empty() {
        return;
    }
    report.analysis_errors.push(AnalysisErrorRecord {
        path: path.clone(),
        side,
        diagnostics: analysis.diagnostics.clone(),
    });
}

/// Classify a diagnostic batch for diff-side severity gating.
///
/// Per the diagnostic contract (rewrite plan §9.3), `Warning` is
/// informational, while `Error` or `Fatal` signals that the analysis is
/// incomplete — diff orchestrators must surface those (CLI exit 1, JSON
/// `analysis_errors`). Returns `true` iff any diagnostic in `diagnostics`
/// reaches the blocking threshold. Lives in the post-1.0 `diff` module
/// so it survives the legacy-engine teardown; the legacy diff path
/// re-uses it via `pub(crate)`.
pub(crate) fn has_blocking_diagnostic(diagnostics: &[ParseDiagnostic]) -> bool {
    diagnostics.iter().any(|d| {
        matches!(
            d.severity,
            mehen_core::DiagnosticSeverity::Error | mehen_core::DiagnosticSeverity::Fatal
        )
    })
}

fn record_unavailable(report: &mut DiffReport, path: &Utf8PathBuf, language: mehen_core::Language) {
    report.analysis_errors.push(AnalysisErrorRecord {
        path: path.clone(),
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

#[derive(Debug)]
pub enum DiffError {
    Git(GitError),
}

impl core::fmt::Display for DiffError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Git(e) => write!(f, "git: {e}"),
        }
    }
}

impl core::error::Error for DiffError {}

// ── pre-1.0 CLI orchestrator (`mehen diff`) ────────────────────────────
//
// Everything below drives the published `mehen diff` subcommand and was
// hoisted out of `legacy/diff.rs` into this module so the CLI and the
// post-1.0 `analyze_diff` entry point share `has_blocking_diagnostic`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum DiffFormat {
    Markdown,
    Json,
}

#[derive(Debug, Clone, serde::Serialize)]
struct MetricDiff {
    name: &'static str,
    label: &'static str,
    current: f64,
    baseline: f64,
    delta: f64,
    polarity: SelectorPolarity,
    is_new: bool,
    is_deleted: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct FileDiff {
    path: PathBuf,
    metrics: Vec<MetricDiff>,
    is_new: bool,
    is_deleted: bool,
    /// Head-side function count read directly from the analysis (not
    /// from the selected columns — the default set no longer includes
    /// `nom.functions`), kept out of the JSON payload. Drives the
    /// biggest-files-first report ordering.
    #[serde(skip)]
    functions: i64,
}

impl FileDiff {
    fn all_unchanged(&self) -> bool {
        self.metrics.iter().all(|m| m.delta == 0.0)
    }

    /// Sort key: total function count descending, then path ascending.
    fn sort_key(&self) -> (std::cmp::Reverse<i64>, PathBuf) {
        (std::cmp::Reverse(self.functions), self.path.clone())
    }
}

#[derive(clap::Args, Debug)]
pub struct DiffOpts {
    /// Base revision to compare from.
    #[clap(long)]
    from: Option<String>,
    /// Head revision to compare to.
    #[clap(long)]
    to: Option<String>,
    /// Comma-separated metrics to compare
    /// (default: cognitive,abc,mi.visual_studio,history.hotspot,history.churn.relative).
    /// Prefix with + for higher-is-better, - for lower-is-better.
    /// Namespaced keys (`sql.*`, `markdown.*`, `history.*`) are accepted
    /// verbatim; `history.*` metrics (including two of the defaults)
    /// trigger a git history walk of both revisions.
    #[clap(long, short = 'M', value_delimiter = ',')]
    metrics: Vec<String>,
    /// Repository-relative files or directories to compare.
    #[clap(long, short, value_parser, num_args(0..))]
    paths: Vec<PathBuf>,
    /// Glob to include files.
    #[clap(long, short = 'I', num_args(0..))]
    include: Vec<String>,
    /// Glob to exclude files.
    #[clap(long, short = 'X', num_args(0..))]
    exclude: Vec<String>,
    /// Output format.
    #[clap(long, short = 'O', value_enum)]
    output_format: Option<DiffFormat>,
    /// Show files where all metrics are unchanged.
    #[clap(long)]
    show_unchanged: bool,
    /// Skip generated, vendored, and binary files marked via Git attributes.
    #[clap(
        long = "ignore-git-attributes",
        visible_alias = "ignore-generated",
        default_value_t = true,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    ignore_git_attributes: bool,
    /// Exit non-zero when the named thresholds are crossed
    /// (comma-separated: `dmi-drop`, `new-broken-link`, `filler-high`, `all`).
    #[clap(
        long,
        value_delimiter = ',',
        value_parser = parse_fail_on_flag,
    )]
    fail_on: Vec<FailOn>,
}

/// Identifies one of the documented doc-metric CI gates. Any other value is
/// rejected by clap at parse time rather than being silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FailOn {
    DmiDrop,
    NewBrokenLink,
    FillerHigh,
    All,
}

impl FailOn {
    fn as_str(self) -> &'static str {
        match self {
            Self::DmiDrop => "dmi-drop",
            Self::NewBrokenLink => "new-broken-link",
            Self::FillerHigh => "filler-high",
            Self::All => "all",
        }
    }
}

/// Custom clap value parser so misspelled flags (e.g. `new-borken-link`)
/// produce an `InvalidValue` error at CLI-parse time instead of being
/// silently dropped downstream.
fn parse_fail_on_flag(raw: &str) -> Result<FailOn, clap::Error> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "dmi-drop" => Ok(FailOn::DmiDrop),
        "new-broken-link" => Ok(FailOn::NewBrokenLink),
        "filler-high" => Ok(FailOn::FillerHigh),
        "all" => Ok(FailOn::All),
        other => Err(clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            format!(
                "unknown --fail-on value `{other}`; expected one of: dmi-drop, new-broken-link, filler-high, all\n"
            ),
        )),
    }
}

pub fn run_diff(opts: DiffOpts) {
    if let Err(e) = run_diff_inner(opts) {
        log::error!("{e}");
        std::process::exit(1);
    }
}

fn run_diff_inner(opts: DiffOpts) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Resolve refs
    let ci_ctx = ci::detect();
    let (from_ref, to_ref) = resolve_refs(&opts, &ci_ctx);

    // 2. Get changed file list
    let repo = mehen_git::open_repo()?;
    let from_label = mehen_git::friendly_ref_label(&repo, &from_ref);
    // The push payload describes the *event's* range; explicit
    // `--from`/`--to` overrides compare a different one, where the
    // payload holds no authority (an empty fold there must not blank
    // out a real requested diff).
    let refs_from_event = opts.from.is_none() && opts.to.is_none();
    let changed = get_changed_files(&repo, &from_ref, &to_ref, &ci_ctx, refs_from_event)?;
    // `history.*` metrics change with every touch, not only when the
    // endpoint trees differ: a file modified in one range commit and
    // reverted in a later one gained commit frequency, churn, and
    // possibly bug-fix risk between the revisions, yet produces no
    // endpoint diff row. When the request can read history columns
    // (an explicit history selector, or default metrics — whose
    // source-code set includes them), add such surviving touched
    // paths as `Modified` rows: their static deltas are zero, and
    // rows whose selected metrics all read zero still drop out of
    // the report as unchanged.
    let may_want_history = if opts.metrics.is_empty() {
        true
    } else {
        history_metrics::names_want_history(
            parse_metric_selectors(&opts.metrics).iter().map(|s| s.name),
        )
    };
    // An authoritatively-empty push payload (branch created at an
    // existing commit, or an add-then-remove push) means *nothing
    // changed in this event* — and `resolve_refs`'s `HEAD~1` last
    // resort is a guess at a range, not the event's range, so walking
    // it would repopulate the report with the tip's previous commit.
    let payload_authoritative_empty = refs_from_event
        && ci_ctx.as_ref().is_some_and(|ctx| {
            ctx.event_name == "push"
                && ctx
                    .changed_files
                    .as_ref()
                    .is_some_and(|files| files.is_empty())
        });
    let changed = if may_want_history
        && !payload_authoritative_empty
        && repo.rev_parse_single(from_ref.as_str()).is_ok()
        && repo.rev_parse_single(to_ref.as_str()).is_ok()
    {
        let mut changed = changed;
        let already: std::collections::HashSet<&PathBuf> =
            changed.iter().map(|cf| &cf.path).collect();
        let explicit = !opts.metrics.is_empty();
        let extra: Vec<mehen_git::ChangedFile> =
            mehen_git::range_touched_files(&repo, &from_ref, &to_ref)?
                .into_iter()
                .filter(|path| !already.contains(path))
                .filter(|path| {
                    // Only languages whose effective selectors read
                    // history columns benefit from a synthetic row.
                    // Markdown always routes to the documentation
                    // pipeline (fixed columns, no history, no
                    // unchanged-row filter), so a restored-content
                    // README must never be resurrected here; under
                    // default metrics, SQL's history-free defaults
                    // exclude it too.
                    let Ok(utf8_path) = Utf8PathBuf::try_from(path.clone()) else {
                        return false;
                    };
                    let Some(language) = detect_language(&utf8_path) else {
                        return false;
                    };
                    if matches!(language, Language::Markdown) {
                        return false;
                    }
                    explicit
                        || history_metrics::names_want_history(
                            crate::metric_selector::default_metrics_for_language(language)
                                .iter()
                                .copied(),
                        )
                })
                .map(|path| mehen_git::ChangedFile {
                    path,
                    status: ChangeStatus::Modified,
                    source_path: None,
                })
                .collect();
        drop(already);
        changed.extend(extra);
        changed
    } else {
        changed
    };

    // 3. Filter files
    let include = mk_globset(opts.include);
    let exclude = mk_globset(opts.exclude);
    let paths = normalize_path_filters(&opts.paths);
    let mut git_attribute_filters = opts
        .ignore_git_attributes
        .then(|| RevisionGitAttributeFilters::new(&repo, &from_ref, &to_ref))
        .transpose()?;
    // Shared selection predicate — the rename splitter and the main
    // filter loop must agree, or a rename could be split here and then
    // dropped there (or vice versa).
    let is_selected = |p: &Path| {
        legacy_path_is_selected(p, &paths)
            && (include.is_empty() || include.is_match(p))
            && (exclude.is_empty() || !exclude.is_match(p))
    };
    // Rename pairs straddling a path/language/attribute boundary fall
    // back to a deletion + addition so neither side silently disappears.
    let SplitChanges {
        files: changed,
        mut history_suppressed_deletions,
    } = split_boundary_renames(changed, &is_selected, git_attribute_filters.as_mut())?;
    // When the caller passes explicit `--metric` names, that one list applies
    // to every file. With no `--metric`, defaults are resolved *per file's
    // language*: SQL files publish only `sql.*` keys, so the source-code
    // defaults (`cyclomatic`, …) would read 0 for them and drop the file as
    // unchanged (Codex P2). `explicit_metrics` selects between the two modes.
    let explicit_metrics = !opts.metrics.is_empty();
    let selectors = parse_metric_selectors(&opts.metrics);

    let registry = Arc::new(AnalyzerRegistry::default_set());
    let analysis_config = AnalysisConfig::default();

    let mut filtered: Vec<(mehen_git::ChangedFile, Utf8PathBuf, Language)> = Vec::new();
    let mut markdown_files: Vec<mehen_git::ChangedFile> = Vec::new();
    for cf in changed {
        let p = &cf.path;
        if !is_selected(p) {
            continue;
        }

        if let Some(filters) = git_attribute_filters.as_mut()
            && filters.excludes(&cf)?
        {
            continue;
        }

        // Convert the git path to UTF-8 once at the boundary; non-UTF-8
        // paths are rare and we drop them rather than fail the diff.
        let Ok(utf8_path) = Utf8PathBuf::try_from(p.clone()) else {
            continue;
        };
        let Some(language) = detect_language(&utf8_path) else {
            continue;
        };

        if matches!(language, Language::Markdown) {
            markdown_files.push(cf.clone());
            continue;
        }

        filtered.push((cf, utf8_path, language));
    }

    // History enrichment (`history.*`): repository-scope process
    // metrics computed by one revision walk per side and folded into
    // each file's metric set after static analysis. The walk costs one
    // tree diff per commit, so it runs only when a file that survived
    // filtering will actually read a history selector: any source file
    // under an explicit history-bearing `--metrics` list, or any file
    // whose *language defaults* include the history columns (SQL has
    // its own history-free defaults, and Markdown files use the
    // separate documentation pipeline — a SQL-only or docs-only diff
    // must not pay for two full-history walks it never reads).
    // Both sides are walked so history columns carry real deltas
    // (e.g. commits/churn gained between base and head) instead of
    // comparing against a phantom zero baseline.
    let file_wants_history =
        |(_, _, language): &(mehen_git::ChangedFile, Utf8PathBuf, Language)| {
            if explicit_metrics {
                history_metrics::names_want_history(selectors.iter().map(|s| s.name))
            } else {
                history_metrics::names_want_history(
                    crate::metric_selector::default_metrics_for_language(*language)
                        .iter()
                        .copied(),
                )
            }
        };

    // A split deletion's lineage is only "carried elsewhere" when its
    // paired destination row actually *reads* history columns: it must
    // have entered the history-enriched source-code pipeline above
    // (not been diverted to the documentation pipeline, or dropped by
    // attribute filters or a non-UTF-8 path), and its effective
    // selectors must include history metrics (a cross-language rename
    // into SQL's history-free defaults reads none). Otherwise the
    // deletion keeps its history as the lineage's only trace.
    {
        let history_consuming_sources: std::collections::HashSet<&PathBuf> = filtered
            .iter()
            .filter(|entry| file_wants_history(entry))
            .filter_map(|(cf, _, _)| cf.source_path.as_ref())
            .collect();
        history_suppressed_deletions.retain(|src| history_consuming_sources.contains(src));
    }

    let histories: Option<(
        Option<mehen_git::RepositoryHistory>,
        mehen_git::RepositoryHistory,
    )> = if filtered.iter().any(file_wants_history) {
        // The head walk is a hard requirement — it feeds every
        // history column. The *baseline* walk tolerates exactly one
        // failure mode: an unresolvable revision (the payload fallback
        // for a force-push keeps diffing with `from_ref` pointing at a
        // commit that no longer exists locally). Baseline history
        // columns then read as an empty baseline. Any other walk
        // failure (corrupt or missing historical objects) still aborts
        // — emitting full-history deltas from incomplete repository
        // data would be silent garbage.
        let base_history = match mehen_git::collect_history(&repo, &from_ref) {
            Ok(history) => Some(history),
            Err(GitError::RefNotFound(rev)) => {
                log::warn!(
                    "baseline history unavailable ({rev} does not resolve locally); history columns compare against an empty baseline"
                );
                None
            }
            Err(e) => return Err(e.into()),
        };
        Some((base_history, mehen_git::collect_history(&repo, &to_ref)?))
    } else {
        None
    };

    // 4. Compute metrics for each file via the per-language analyzer
    //    registry. The legacy `langs::get_function_spaces` pipeline is no
    //    longer used; we drive `LanguageAnalyzer::analyze` and read
    //    selector values out of the root `MetricSpace`'s `MetricSet`.
    //
    //    Recoverable parser errors are surfaced as
    //    `DiagnosticSeverity::Error` / `Fatal` by the per-language
    //    analyzers (plan §9.3). Track whether any analyzed side reported
    //    an error/fatal so the diff exits non-zero at the end — partial
    //    metrics from a broken parse must not pass CI silently.
    let mut diffs = Vec::new();
    let mut analysis_failed = false;
    // The union of selectors actually displayed, in first-seen order. With
    // explicit `--metric` this is just `selectors`; with per-language defaults
    // it accumulates each language's default columns as files are seen, so a
    // mixed PR shows source-code columns and SQL columns side by side (each
    // file populates only its own language's columns).
    let mut display_selectors: Vec<MetricSelector> = if explicit_metrics {
        selectors.clone()
    } else {
        Vec::new()
    };
    for (cf, utf8_path, language) in &filtered {
        let is_deleted = cf.status == ChangeStatus::Deleted;
        let is_new = cf.status == ChangeStatus::Added;
        // Renamed files carry the baseline under their old path — both
        // the baseline blob and the baseline history live there.
        let base_path = cf.source_path.as_deref().unwrap_or(cf.path.as_path());

        let analyzer = match registry.analyzer_for(*language) {
            Some(a) => a,
            None => continue,
        };

        // Selectors for *this* file: the explicit list, or this language's
        // defaults. Register any new default columns into the display union.
        let file_selectors: Vec<MetricSelector> = if explicit_metrics {
            selectors.clone()
        } else {
            let langs = default_selectors_for_language(*language);
            for sel in &langs {
                if !display_selectors.iter().any(|d| d.name == sel.name) {
                    display_selectors.push(sel.clone());
                }
            }
            langs
        };

        let mut analyze = |bytes: Vec<u8>, side: &str| -> Option<MetricSpace> {
            let text = String::from_utf8(bytes).ok()?;
            let source = SourceFile::new(utf8_path.clone(), *language, text);
            let analysis = match analyzer.analyze(&source, &analysis_config) {
                Ok(a) => a,
                Err(err) => {
                    log::error!("{} ({side}): analyzer failed: {err}", cf.path.display());
                    analysis_failed = true;
                    return None;
                }
            };
            for diag in &analysis.diagnostics {
                match diag.severity {
                    DiagnosticSeverity::Warning => log::warn!(
                        "{} ({side}): {}: {}",
                        cf.path.display(),
                        diag.code,
                        diag.message
                    ),
                    DiagnosticSeverity::Error | DiagnosticSeverity::Fatal => log::error!(
                        "{} ({side}): {}: {}",
                        cf.path.display(),
                        diag.code,
                        diag.message
                    ),
                }
            }
            if has_blocking_diagnostic(&analysis.diagnostics) {
                analysis_failed = true;
            }
            Some(analysis.root)
        };

        let mut baseline_space: Option<MetricSpace> = if is_new {
            None
        } else {
            match mehen_git::read_blob(&repo, &from_ref, base_path) {
                Ok(Some(bytes)) => analyze(bytes, "baseline"),
                Ok(None) => None,
                Err(e) => {
                    log::warn!("Skipping baseline for {}: {e}", cf.path.display());
                    None
                }
            }
        };

        let mut current_space: Option<MetricSpace> = if is_deleted {
            None
        } else {
            match mehen_git::read_blob(&repo, &to_ref, &cf.path) {
                Ok(Some(bytes)) => analyze(bytes, "current"),
                Ok(None) => None,
                Err(e) => {
                    log::warn!("Skipping current for {}: {e}", cf.path.display());
                    None
                }
            }
        };

        // Fold the `history.*` family into each side's metric set, each
        // against its own revision's history and head-relative "now".
        // The baseline side of a renamed file reads its old path.
        // The 🆕 flag is fixed *before* any baseline synthesis below —
        // it reflects blob availability, not history availability.
        let is_new_row = is_new && baseline_space.is_none();
        if let Some((base_history, head_history)) = histories.as_ref() {
            // A split rename (`Added` row carrying `source_path`, e.g.
            // a cross-language `a.py → a.rs`) has no baseline *blob*,
            // but its baseline *history* is the source lineage. Give
            // it a synthetic baseline space so history columns
            // compare against real values instead of manufacturing a
            // full-history spike. The history *composites* also read
            // static inputs at the baseline revision — hotspot needs
            // the source's cognitive complexity, relative churn its
            // size — so exactly those inputs are copied from an
            // analysis of the source blob; every displayed static
            // column still reads 0 there, keeping the row's
            // new-file presentation.
            if baseline_space.is_none()
                && cf.source_path.is_some()
                && base_history
                    .as_ref()
                    .is_some_and(|history| history.file(base_path).is_some())
            {
                let mut space = MetricSpace::new(
                    mehen_core::SpaceId(0),
                    mehen_core::SpaceKind::Unit,
                    mehen_core::SourceSpan::empty(),
                );
                if let Ok(Some(bytes)) = mehen_git::read_blob(&repo, &from_ref, base_path)
                    && let Ok(base_utf8) = Utf8PathBuf::try_from(base_path.to_path_buf())
                    && let Some(base_language) = detect_language(&base_utf8)
                    && let Some(base_analyzer) = registry.analyzer_for(base_language)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    let base_source = SourceFile::new(base_utf8, base_language, text);
                    if let Ok(base_analysis) = base_analyzer.analyze(&base_source, &analysis_config)
                    {
                        for key in [
                            mehen_core::keys::LOC_SLOC,
                            mehen_core::keys::COGNITIVE_SUM,
                            mehen_core::keys::SQL_LOC_CODE,
                            mehen_core::keys::SQL_COGNITIVE_COMPLEXITY,
                            mehen_core::keys::MARKDOWN_LOC_TLOC,
                            mehen_core::keys::MARKDOWN_COGNITIVE_COMPLEXITY,
                        ] {
                            if let Some(value) = base_analysis
                                .root
                                .metrics
                                .get(&mehen_core::MetricKey::new(key))
                            {
                                space.metrics.insert(key, value);
                            }
                        }
                    }
                }
                baseline_space = Some(space);
            }
            // History metrics don't depend on decoding or parsing the
            // blob: a side whose static analysis is unavailable (e.g.
            // non-UTF-8 but non-binary content the analyzer rejects)
            // still has valid repository history. Synthesize an empty
            // space for such a side so history-only selectors read the
            // real values instead of zero — static columns stay 0.
            let empty_space = || {
                MetricSpace::new(
                    mehen_core::SpaceId(0),
                    mehen_core::SpaceKind::Unit,
                    mehen_core::SourceSpan::empty(),
                )
            };
            if baseline_space.is_none()
                && !is_new
                && base_history
                    .as_ref()
                    .is_some_and(|history| history.file(base_path).is_some())
            {
                baseline_space = Some(empty_space());
            }
            if current_space.is_none() && !is_deleted && head_history.file(&cf.path).is_some() {
                current_space = Some(empty_space());
            }
            let mut sides: Vec<(
                Option<&mut MetricSpace>,
                &mehen_git::RepositoryHistory,
                &Path,
            )> = Vec::with_capacity(2);
            // A split-rename deletion row's lineage is already carried
            // by its paired destination row — injecting it here too
            // would double-count the history (a +1 on the destination
            // and a full -N on the source).
            let deletion_history_suppressed =
                is_deleted && history_suppressed_deletions.contains(&cf.path);
            if let Some(base_history) = base_history.as_ref()
                && !deletion_history_suppressed
            {
                sides.push((baseline_space.as_mut(), base_history, base_path));
            }
            sides.push((current_space.as_mut(), head_history, cf.path.as_path()));
            for (space, history, path) in sides {
                if let Some(space) = space
                    && let Some(fh) = history.file(path)
                {
                    history_metrics::inject_history_metrics(
                        &mut space.metrics,
                        fh,
                        history.head_seconds,
                    );
                }
            }
        }

        let metric_diffs: Vec<MetricDiff> = file_selectors
            .iter()
            .map(|sel| {
                let baseline = baseline_space
                    .as_ref()
                    .map(|s| read_selector_metric(s, sel))
                    .unwrap_or(0.0);
                let current = current_space
                    .as_ref()
                    .map(|s| read_selector_metric(s, sel))
                    .unwrap_or(0.0);
                MetricDiff {
                    name: sel.name,
                    label: sel.label,
                    current,
                    baseline,
                    delta: current - baseline,
                    polarity: sel.polarity,
                    is_new: is_new_row,
                    is_deleted,
                }
            })
            .collect();

        diffs.push(FileDiff {
            path: cf.path.clone(),
            metrics: metric_diffs,
            is_new: is_new_row,
            is_deleted,
            functions: current_space
                .as_ref()
                .and_then(|s| s.metrics.get(&mehen_core::MetricKey::new("nom.functions")))
                .map(|v| v.as_f64() as i64)
                .unwrap_or(0),
        });
    }

    // 5. Filter unchanged
    if !opts.show_unchanged {
        diffs.retain(|d| !d.all_unchanged());
    }

    // 6. Sort
    diffs.sort_by_key(|a| a.sort_key());

    // Markdown doc section — parallel pipeline for `.md`-like files.
    let doc_files: Vec<DocDiffFile> = {
        let mut out: Vec<DocDiffFile> = Vec::new();
        for cf in &markdown_files {
            let is_deleted = cf.status == ChangeStatus::Deleted;
            let is_candidate_new = cf.status == ChangeStatus::Added;
            // Renamed docs carry the baseline under their old path.
            let base_path = cf.source_path.as_deref().unwrap_or(cf.path.as_path());
            let base_metrics = if is_candidate_new {
                None
            } else {
                match mehen_git::read_blob(&repo, &from_ref, base_path) {
                    // Analyze the baseline *as its old path*: Markdown
                    // link/grounding metrics resolve relative
                    // references from the file's location, so a
                    // renamed doc's baseline must be evaluated from
                    // the directory it actually lived in — otherwise a
                    // link broken only by the move looks broken in the
                    // baseline too and `--fail-on new-broken-link`
                    // misses the regression.
                    Ok(Some(bytes)) => Some(mehen_markdown::analyze_markdown(
                        &String::from_utf8_lossy(&bytes),
                        base_path,
                    )),
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!("Skipping baseline for {}: {e}", cf.path.display());
                        None
                    }
                }
            };
            let head_metrics = if is_deleted {
                None
            } else {
                match mehen_git::read_blob(&repo, &to_ref, &cf.path) {
                    Ok(Some(bytes)) => Some(mehen_markdown::analyze_markdown(
                        &String::from_utf8_lossy(&bytes),
                        &cf.path,
                    )),
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!("Skipping current for {}: {e}", cf.path.display());
                        None
                    }
                }
            };
            let is_new = is_candidate_new && base_metrics.is_none();
            out.push(DocDiffFile {
                path: cf.path.clone(),
                head: head_metrics,
                base: base_metrics,
                is_new,
                is_deleted,
            });
        }
        out
    };

    // 7. Output
    let format = opts.output_format.unwrap_or(DiffFormat::Markdown);
    match format {
        DiffFormat::Markdown => {
            print_markdown(&diffs, &display_selectors, &from_label, &from_ref, &to_ref);
            if !doc_files.is_empty() {
                let mut ctx = DocRenderCtx::new(&from_label);
                let repo_url = ci_ctx
                    .as_ref()
                    .and_then(|c| c.repository.as_ref())
                    .map(|r| format!("https://github.com/{r}"));
                ctx.repo_url = repo_url.as_deref();
                ctx.head_sha = Some(&to_ref);
                if let Some(doc_md) = render_doc_section(&doc_files, &ctx) {
                    let mut stdout = std::io::stdout().lock();
                    writeln!(stdout).ok();
                    write!(stdout, "{doc_md}").ok();
                }
            }
        }
        DiffFormat::Json => {
            let doc_ref: Option<&[DocDiffFile]> = if doc_files.is_empty() {
                None
            } else {
                Some(&doc_files)
            };
            if let Err(e) = print_json(&diffs, doc_ref) {
                // Surface the error loudly — exit code 2 mirrors the
                // --fail-on gate and is distinct from the generic exit 1
                // that covers setup/IO errors in run_diff_inner.
                log::error!("diff: failed to emit JSON output: {e}");
                std::process::exit(2);
            }
        }
    }

    // --fail-on check.
    let failures = evaluate_fail_on(&opts.fail_on, &doc_files);
    if !failures.is_empty() {
        log::error!("--fail-on threshold crossed: {}", failures.join(", "));
        std::process::exit(2);
    }

    // Per the diagnostic contract (rewrite plan §9.3), recoverable
    // parser errors must surface as a non-zero exit so CI cannot pass
    // partial metrics computed from a known-broken parse. Exit 1 lines
    // up with the generic setup/IO bucket and is distinct from exit 2
    // (threshold gate). Diagnostics are already logged above; this gate
    // only flips the exit code.
    if analysis_failed {
        std::process::exit(1);
    }

    Ok(())
}

fn doc_json_payload(files: &[DocDiffFile]) -> Vec<serde_json::Value> {
    files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.path.to_string_lossy(),
                "is_new": f.is_new,
                "is_deleted": f.is_deleted,
                "base": f.base,
                "head": f.head,
            })
        })
        .collect()
}

fn evaluate_fail_on(flags: &[FailOn], docs: &[DocDiffFile]) -> Vec<String> {
    let mut enabled: std::collections::BTreeSet<FailOn> = std::collections::BTreeSet::new();
    for f in flags {
        match f {
            FailOn::All => {
                enabled.insert(FailOn::DmiDrop);
                enabled.insert(FailOn::NewBrokenLink);
                enabled.insert(FailOn::FillerHigh);
            }
            other => {
                enabled.insert(*other);
            }
        }
    }
    if enabled.is_empty() {
        return Vec::new();
    }
    // If the caller asked to gate on doc metrics but no markdown files are
    // in the diff, log a warning so users notice the flag silently matched
    // nothing. The gate itself still returns success (no docs → no metric
    // breach possible) so existing CI doesn't break.
    if docs.iter().all(|f| f.head.is_none()) {
        let flags: Vec<&str> = enabled.iter().copied().map(FailOn::as_str).collect();
        log::warn!(
            "--fail-on {flags:?} has no Markdown files in the diff; no doc-metric thresholds were evaluated"
        );
    }
    let mut failures: Vec<String> = Vec::new();
    for f in docs {
        let Some(head) = &f.head else { continue };
        let base = f.base.as_ref();
        if enabled.contains(&FailOn::DmiDrop)
            && let Some(b) = base
        {
            let hd = head.maintainability.documentation_maintainability_index;
            let bd = b.maintainability.documentation_maintainability_index;
            if bd - hd >= 3.0 {
                failures.push(format!("dmi-drop:{}", f.path.display()));
            }
        }
        if enabled.contains(&FailOn::NewBrokenLink) {
            // Identity-based diff keyed on (class, destination) — line
            // numbers MAY change without a new broken link (e.g. a doc
            // prepends content, shifting every link down one line). The CI
            // gate fires only when a key appears more often in head than in
            // base. Line numbers still flow through to the callout layer for
            // the PR comment; they just don't drive the fail-on decision.
            // See §39.4.
            let mut head_counts: std::collections::BTreeMap<
                (mehen_markdown::types::LinkClass, &str),
                usize,
            > = std::collections::BTreeMap::new();
            for l in &head.link_records {
                if matches!(l.resolved, Some(false)) {
                    *head_counts
                        .entry((l.class, l.destination.as_str()))
                        .or_insert(0) += 1;
                }
            }
            let mut base_counts: std::collections::BTreeMap<
                (mehen_markdown::types::LinkClass, &str),
                usize,
            > = std::collections::BTreeMap::new();
            if let Some(b) = base {
                for l in &b.link_records {
                    if matches!(l.resolved, Some(false)) {
                        *base_counts
                            .entry((l.class, l.destination.as_str()))
                            .or_insert(0) += 1;
                    }
                }
            }
            let has_new_broken = head_counts.iter().any(|(key, head_n)| {
                let base_n = base_counts.get(key).copied().unwrap_or(0);
                *head_n > base_n
            });
            if has_new_broken {
                failures.push(format!("new-broken-link:{}", f.path.display()));
            }
        }
        if enabled.contains(&FailOn::FillerHigh) && head.ai_era.filler_lazy_structure_risk >= 0.60 {
            failures.push(format!("filler-high:{}", f.path.display()));
        }
    }
    failures
}

// ── Ref resolution ─────────────────────────────────────────────────────

fn resolve_refs(opts: &DiffOpts, ci_ctx: &Option<ci::CiContext>) -> (String, String) {
    if let (Some(from), Some(to)) = (&opts.from, &opts.to) {
        return (from.clone(), to.clone());
    }

    if let Some(ctx) = ci_ctx {
        let to = opts
            .to
            .clone()
            .or_else(|| ctx.head_sha.clone())
            .unwrap_or_else(|| "HEAD".to_string());

        let from = opts
            .from
            .clone()
            .unwrap_or_else(|| match ctx.event_name.as_str() {
                // A multi-commit push must diff against the branch tip
                // *before* the push (the payload's `before` SHA), not
                // just the final commit's parent — otherwise renames
                // and baselines from earlier commits in the push are
                // invisible. A branch-creation push has no `before`;
                // the parent of the *first pushed commit* is the right
                // baseline there. `HEAD~1` remains the last resort.
                "push" => ctx
                    .before_sha
                    .clone()
                    .or_else(|| ctx.first_commit_sha.as_ref().map(|sha| format!("{sha}~1")))
                    .unwrap_or_else(|| "HEAD~1".to_string()),
                "pull_request" | "merge_group" => ctx
                    .base_ref
                    .as_ref()
                    .map(|b| format!("origin/{b}"))
                    .unwrap_or_else(|| "origin/main".to_string()),
                _ => "main".to_string(),
            });

        return (from, to);
    }

    let from = opts.from.clone().unwrap_or_else(|| "main".to_string());
    let to = opts.to.clone().unwrap_or_else(|| "HEAD".to_string());
    (from, to)
}

fn get_changed_files(
    repo: &gix::Repository,
    from: &str,
    to: &str,
    ci_ctx: &Option<ci::CiContext>,
    refs_from_event: bool,
) -> Result<Vec<mehen_git::ChangedFile>, GitError> {
    // For push events, the payload's folded per-path statuses (PR #95)
    // are a *fallback*: the real tree diff over the full push range is
    // strictly more accurate — it carries rename identity (including
    // break-rewrite recovery when a renamed file's old path was
    // reused), correct type-change handling, and blob-only filtering.
    // That applies to branch creations too: `resolve_refs` supplies
    // the first pushed commit's parent there, so a resolvable baseline
    // still yields a full-range tree diff with rename identity the
    // payload can never express. The payload is used only when the
    // refs don't resolve locally (a force-push discarded the `before`
    // commit, or the branch's first commit is a root commit) — and
    // only when the compared range *is* the event's range: explicit
    // `--from`/`--to` overrides ask about a different range the
    // payload knows nothing about.
    if refs_from_event
        && let Some(ctx) = ci_ctx
        && ctx.event_name == "push"
        && let Some(ref files) = ctx.changed_files
    {
        // An *empty* fold is authoritative: the push changed nothing
        // net (add-then-remove), or a branch was created pointing at a
        // commit that already existed — either way a ref-range diff
        // (e.g. the `HEAD~1` last resort) would misreport the tip's
        // last commit as this push's changes.
        if files.is_empty() {
            return Ok(files.clone());
        }
        // The payload fallback exists for exactly one failure mode:
        // the push *baseline* not resolving locally (a force-push
        // discarded the `before` commit, or a created branch's first
        // commit is a root commit). Any other tree-diff failure —
        // corrupt or missing objects in a resolvable range — must
        // propagate rather than silently degrade to the payload's
        // rename-less, type-change-less view.
        if repo.rev_parse_single(from).is_err() {
            log::warn!(
                "falling back to the push payload's changed files ({from} does not resolve locally)"
            );
            // With the baseline unreadable, every baseline blob read
            // downstream would fail and quietly turn `Modified` rows
            // into fabricated full-value-vs-zero deltas, while
            // `Deleted` rows (neither side analyzable) would vanish.
            // Degrade honestly instead: modified files are presented
            // as their current state only (an `Added` row, 🆕 in the
            // report), and deletions are dropped with a warning.
            let degraded = files
                .iter()
                .filter(|cf| {
                    if cf.status == ChangeStatus::Deleted {
                        log::warn!(
                            "dropping deleted file {} from the report: its baseline \
                             ({from}) is not available locally",
                            cf.path.display()
                        );
                        false
                    } else {
                        true
                    }
                })
                .map(|cf| mehen_git::ChangedFile {
                    path: cf.path.clone(),
                    status: ChangeStatus::Added,
                    source_path: cf.source_path.clone(),
                })
                .collect();
            return Ok(degraded);
        }
    }

    mehen_git::changed_files(repo, from, to)
}
fn normalize_path_filters(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| normalize_path_filter(path))
        .collect()
}

fn normalize_path_filter(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
            other => cleaned.push(other.as_os_str()),
        }
    }

    cleaned
}

/// Pre-1.0 path filter for CLI `--paths`: matches by `Path` prefix or
/// exact equality. Distinct from the post-1.0 [`path_is_selected`]
/// (which works on `Utf8PathBuf` for the `analyze_diff` entry point).
fn legacy_path_is_selected(path: &Path, paths: &[PathBuf]) -> bool {
    paths.is_empty()
        || paths.iter().any(|selected| {
            selected.as_os_str().is_empty() || path == selected || path.starts_with(selected)
        })
}

// ── Markdown output ────────────────────────────────────────────────────

fn print_markdown(
    diffs: &[FileDiff],
    selectors: &[MetricSelector],
    from_label: &str,
    from: &str,
    to: &str,
) {
    let mut out = String::new();

    // Source-code anchor (§39.1: sibling of the docs anchor).
    out.push_str("<!-- mehen-metrics -->\n");
    out.push_str(&format!(
        "## [Mehen](https://github.com/ophi-dev/mehen) Summary (`{from}`..`{to}`)\n\n"
    ));

    if diffs.is_empty() {
        out.push_str("No metric changes detected.\n");
        write!(std::io::stdout().lock(), "{out}").unwrap();
        return;
    }

    // Header
    out.push_str("| File |");
    for sel in selectors {
        out.push_str(&format!(" {} |", sel.label));
    }
    out.push('\n');

    // Separator
    out.push_str("|---|");
    for _ in selectors {
        out.push_str("---:|");
    }
    out.push('\n');

    // Rows. Each cell is looked up by selector *name* against the file's
    // metrics, so a file that doesn't publish a given column (e.g. a SQL file
    // under the `cyclomatic` column of a mixed PR) renders an em dash rather
    // than a misaligned value.
    for diff in diffs {
        out.push_str(&format!("| {} |", diff.path.display()));
        for sel in selectors {
            out.push(' ');
            match diff.metrics.iter().find(|m| m.name == sel.name) {
                Some(md) => out.push_str(&format_metric_cell(md, from_label)),
                None => out.push('\u{2013}'), // – (column not applicable to this file)
            }
            out.push_str(" |");
        }
        out.push('\n');
    }

    write!(std::io::stdout().lock(), "{out}").unwrap();
}

fn format_metric_cell(md: &MetricDiff, from: &str) -> String {
    let current = format_f64(md.current);

    if md.is_new {
        return format!("{current} \u{1F195}"); // 🆕
    }

    if md.is_deleted {
        let baseline = format_f64(md.baseline);
        let emoji = trend_emoji(md.delta, md.polarity);
        return format!("0 (was: {baseline}) {emoji}");
    }

    if md.delta == 0.0 {
        return format!("{current} \u{26AA}"); // ⚪
    }

    let baseline = format_f64(md.baseline);
    let emoji = trend_emoji(md.delta, md.polarity);
    format!("{current} ({from}: {baseline}) {emoji}")
}

fn trend_emoji(delta: f64, polarity: SelectorPolarity) -> &'static str {
    if delta == 0.0 {
        return "\u{26AA}"; // ⚪
    }
    match polarity {
        SelectorPolarity::LowerIsBetter => {
            if delta > 0.0 {
                "\u{1F534}" // 🔴
            } else {
                "\u{1F7E2}" // 🟢
            }
        }
        SelectorPolarity::HigherIsBetter => {
            if delta > 0.0 {
                "\u{1F7E2}" // 🟢
            } else {
                "\u{1F534}" // 🔴
            }
        }
    }
}

fn format_f64(v: f64) -> String {
    if v == v.trunc() {
        format!("{}", v as i64)
    } else {
        format!("{:.2}", v)
    }
}

// ── JSON output ────────────────────────────────────────────────────────

/// Emit a single JSON document with a `source_code` key and an optional
/// `markdown` key. Downstream consumers (`jq`, `serde_json`) see one top-level
/// object, not two concatenated arrays.
///
/// Serialization errors bubble up as `Err` so `run_diff_inner` exits
/// non-zero instead of silently writing an empty `""` to stdout.
fn print_json(
    diffs: &[FileDiff],
    docs: Option<&[DocDiffFile]>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut payload = serde_json::Map::new();
    payload.insert("source_code".to_string(), serde_json::to_value(diffs)?);
    if let Some(docs) = docs {
        payload.insert(
            "markdown".to_string(),
            serde_json::Value::Array(doc_json_payload(docs)),
        );
    }
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(payload))?;
    writeln!(std::io::stdout().lock(), "{json}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diagnostics_are_not_blocking() {
        assert!(!has_blocking_diagnostic(&[]));
    }

    #[test]
    fn warning_only_is_not_blocking() {
        let diags = vec![ParseDiagnostic::warning("python.style", "long line")];
        assert!(!has_blocking_diagnostic(&diags));
    }

    #[test]
    fn error_severity_is_blocking() {
        let diags = vec![ParseDiagnostic::error(
            "ruby.syntax_error",
            "unterminated string",
        )];
        assert!(has_blocking_diagnostic(&diags));
    }

    #[test]
    fn fatal_severity_is_blocking() {
        let diags = vec![ParseDiagnostic::fatal(
            "rust.parse_error",
            "tree-sitter-rust failed",
        )];
        assert!(has_blocking_diagnostic(&diags));
    }

    #[test]
    fn warning_mixed_with_error_is_blocking() {
        let diags = vec![
            ParseDiagnostic::warning("python.style", "long line"),
            ParseDiagnostic::error("python.syntax_error", "invalid syntax"),
        ];
        assert!(has_blocking_diagnostic(&diags));
    }

    use mehen_core::{
        AnalysisBackend, Language, MetricKey, MetricSpace, Polarity, SourceSpan, SpaceId, SpaceKind,
    };

    fn analysis_with_metric(key: &str, value: f64) -> LanguageAnalysis {
        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        root.metrics.insert(MetricKey::new(key), value);
        LanguageAnalysis {
            language: Language::Rust,
            backend: AnalysisBackend::TreeSitter,
            diagnostics: Vec::new(),
            root,
            contributions: Vec::new(),
        }
    }

    fn empty_report() -> DiffReport {
        DiffReport {
            schema_version: "1.0".to_string(),
            base: "HEAD~1".to_string(),
            head: "HEAD".to_string(),
            files: Vec::new(),
            markdown_files: Vec::new(),
            analysis_errors: Vec::new(),
            threshold_violations: Vec::new(),
        }
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(repo)
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
    }

    #[test]
    fn analyze_diff_skips_all_default_git_attribute_classes() {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
generated.md linguist-generated
vendored.md linguist-vendored
binary.md binary
deleted.md linguist-generated
",
        )
        .unwrap();
        for name in [
            "kept.md",
            "generated.md",
            "vendored.md",
            "binary.md",
            "deleted.md",
        ] {
            std::fs::write(dir.path().join(name), "# Base\n").unwrap();
        }
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        git_ok(dir.path(), &["tag", "attribute-base"]);

        for name in ["kept.md", "generated.md", "vendored.md", "binary.md"] {
            std::fs::write(dir.path().join(name), "# Head\n\nChanged.\n").unwrap();
        }
        std::fs::remove_file(dir.path().join("deleted.md")).unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
generated.md linguist-generated
vendored.md linguist-vendored
binary.md binary
",
        )
        .unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "head"]);
        git_ok(dir.path(), &["tag", "attribute-head"]);

        std::fs::write(
            dir.path().join(".gitattributes"),
            "* -linguist-generated -linguist-vendored -binary\n",
        )
        .unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "checkout"]);

        let repo = gix::discover(dir.path()).unwrap();
        let report = analyze_diff_in_repo(
            DiffInput {
                from: "attribute-base".to_string(),
                to: "attribute-head".to_string(),
                paths: Vec::new(),
                thresholds: Vec::new(),
                config: AnalysisConfig::default(),
            },
            &repo,
        )
        .unwrap();
        let paths: Vec<&str> = report
            .markdown_files
            .iter()
            .filter_map(|file| file.path.file_name())
            .collect();

        assert_eq!(paths, vec!["kept.md"]);
    }

    #[test]
    fn analyze_diff_evaluates_history_thresholds_against_head_history() {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);

        std::fs::write(dir.path().join("hot.py"), "x = 1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        git_ok(dir.path(), &["tag", "history-base"]);

        std::fs::write(dir.path().join("hot.py"), "x = 1\ny = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "head"]);
        git_ok(dir.path(), &["tag", "history-head"]);

        let repo = gix::discover(dir.path()).unwrap();
        let thresholds = vec![Threshold::new(
            "history.commit_frequency".parse().unwrap(),
            1.0,
            Polarity::HigherIsWorse,
        )];
        let report = analyze_diff_in_repo(
            DiffInput {
                from: "history-base".to_string(),
                to: "history-head".to_string(),
                paths: Vec::new(),
                thresholds,
                config: AnalysisConfig::default(),
            },
            &repo,
        )
        .unwrap();

        // hot.py was touched by 2 commits at head — above the limit of 1.
        assert_eq!(report.threshold_violations.len(), 1);
        let v = &report.threshold_violations[0];
        assert_eq!(v.path, "hot.py");
        assert_eq!(v.evaluation.actual, 2.0);
        assert!(v.evaluation.violated);
    }

    #[test]
    fn analyze_diff_history_thresholds_cover_restored_files() {
        // Modified in one range commit, restored in the next: the
        // endpoint trees are identical, but the head history gained
        // two commits and the threshold must still trip.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);

        std::fs::write(dir.path().join("wobbly.py"), "x = 1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        git_ok(dir.path(), &["tag", "restored-base"]);

        std::fs::write(dir.path().join("wobbly.py"), "x = 1\ny = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "grow"]);
        std::fs::write(dir.path().join("wobbly.py"), "x = 1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "restore"]);
        git_ok(dir.path(), &["tag", "restored-head"]);

        let repo = gix::discover(dir.path()).unwrap();
        let thresholds = vec![Threshold::new(
            "history.commit_frequency".parse().unwrap(),
            2.0,
            Polarity::HigherIsWorse,
        )];
        let report = analyze_diff_in_repo(
            DiffInput {
                from: "restored-base".to_string(),
                to: "restored-head".to_string(),
                paths: Vec::new(),
                thresholds,
                config: AnalysisConfig::default(),
            },
            &repo,
        )
        .unwrap();

        assert_eq!(report.threshold_violations.len(), 1);
        let v = &report.threshold_violations[0];
        assert_eq!(v.path, "wobbly.py");
        assert_eq!(v.evaluation.actual, 3.0);
        assert!(v.evaluation.violated);
    }

    #[test]
    fn split_boundary_renames_keeps_same_language_in_scope_renames_joined() {
        let rename = mehen_git::ChangedFile {
            path: PathBuf::from("src/after.py"),
            status: ChangeStatus::Modified,
            source_path: Some(PathBuf::from("src/before.py")),
        };
        let out = split_boundary_renames(vec![rename], &|_| true, None)
            .expect("no attribute filters")
            .files;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, PathBuf::from("src/after.py"));
        assert_eq!(out[0].status, ChangeStatus::Modified);
        assert_eq!(
            out[0].source_path.as_deref(),
            Some(Path::new("src/before.py"))
        );
    }

    #[test]
    fn split_boundary_renames_reports_rename_to_unsupported_extension_as_deletion() {
        // src/foo.py -> archive/foo.txt: the destination has no
        // detectable language, so the Python file's disappearance must
        // still be reported as a deletion instead of vanishing.
        let rename = mehen_git::ChangedFile {
            path: PathBuf::from("archive/foo.txt"),
            status: ChangeStatus::Modified,
            source_path: Some(PathBuf::from("src/foo.py")),
        };
        let out = split_boundary_renames(vec![rename], &|_| true, None)
            .expect("no attribute filters")
            .files;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, PathBuf::from("src/foo.py"));
        assert_eq!(out[0].status, ChangeStatus::Deleted);
        assert!(out[0].source_path.is_none());
    }

    #[test]
    fn split_boundary_renames_splits_cross_language_renames() {
        // A .py -> .rs rename must not analyze the Python baseline
        // with the Rust analyzer: both sides are reported separately.
        let rename = mehen_git::ChangedFile {
            path: PathBuf::from("src/port.rs"),
            status: ChangeStatus::Modified,
            source_path: Some(PathBuf::from("src/port.py")),
        };
        let out = split_boundary_renames(vec![rename], &|_| true, None)
            .expect("no attribute filters")
            .files;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, PathBuf::from("src/port.py"));
        assert_eq!(out[0].status, ChangeStatus::Deleted);
        assert_eq!(out[1].path, PathBuf::from("src/port.rs"));
        assert_eq!(out[1].status, ChangeStatus::Added);
    }

    #[test]
    fn split_boundary_renames_reports_rename_out_of_selected_scope_as_deletion() {
        // With `--paths src`, a rename src/keep.py -> attic/keep.py
        // must report the file leaving the scope.
        let rename = mehen_git::ChangedFile {
            path: PathBuf::from("attic/keep.py"),
            status: ChangeStatus::Modified,
            source_path: Some(PathBuf::from("src/keep.py")),
        };
        let selected = |p: &Path| p.starts_with("src");
        let out = split_boundary_renames(vec![rename], &selected, None)
            .expect("no attribute filters")
            .files;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, PathBuf::from("src/keep.py"));
        assert_eq!(out[0].status, ChangeStatus::Deleted);
    }

    #[test]
    fn split_boundary_renames_splits_when_destination_is_attribute_excluded() {
        // A rename whose destination becomes `linguist-generated` at
        // head must still report the analyzable source's deletion —
        // keeping the pair joined would let the head-side attribute
        // check drop the whole row.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);

        std::fs::write(dir.path().join("hand_written.py"), "x = 1\ny = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        git_ok(dir.path(), &["tag", "attr-rename-base"]);

        git_ok(dir.path(), &["mv", "hand_written.py", "generated.py"]);
        std::fs::write(
            dir.path().join(".gitattributes"),
            "generated.py linguist-generated\n",
        )
        .unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "generate"]);
        git_ok(dir.path(), &["tag", "attr-rename-head"]);

        let repo = gix::discover(dir.path()).unwrap();
        let report = analyze_diff_in_repo(
            DiffInput {
                from: "attr-rename-base".to_string(),
                to: "attr-rename-head".to_string(),
                paths: Vec::new(),
                thresholds: Vec::new(),
                config: AnalysisConfig::default(),
            },
            &repo,
        )
        .unwrap();

        let paths: Vec<&str> = report.files.iter().map(|f| f.path.as_str()).collect();
        // The source's deletion is reported; the attribute-excluded
        // destination is not.
        assert!(
            paths.contains(&"hand_written.py"),
            "source deletion must survive: {paths:?}"
        );
        assert!(
            !paths.contains(&"generated.py"),
            "attribute-excluded destination must be dropped: {paths:?}"
        );
    }

    /// A push-shaped [`ci::CiContext`] carrying a folded payload list.
    fn push_ctx(files: Vec<mehen_git::ChangedFile>) -> ci::CiContext {
        ci::CiContext {
            provider: ci::CiProvider::GitHubActions,
            event_name: "push".to_string(),
            base_ref: None,
            head_sha: None,
            before_sha: None,
            first_commit_sha: None,
            changed_files: Some(files),
            pr_number: None,
            repository: None,
        }
    }

    #[test]
    fn push_events_prefer_the_tree_diff_over_the_payload() {
        // A GitHub push payload reports a rename as removed + added
        // (and a reused source path as Modified) with no rename
        // identity. When the refs resolve locally, the real tree diff
        // must win so the diff compares against the old path's
        // baseline instead of a zero baseline / full-history spike.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("before.py"), "x = 1\ny = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        git_ok(dir.path(), &["tag", "payload-base"]);
        git_ok(dir.path(), &["mv", "before.py", "after.py"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "rename"]);
        git_ok(dir.path(), &["tag", "payload-head"]);

        let repo = gix::discover(dir.path()).unwrap();
        let ctx = Some(push_ctx(vec![
            mehen_git::ChangedFile {
                path: PathBuf::from("before.py"),
                status: ChangeStatus::Deleted,
                source_path: None,
            },
            mehen_git::ChangedFile {
                path: PathBuf::from("after.py"),
                status: ChangeStatus::Added,
                source_path: None,
            },
        ]));
        let out = get_changed_files(&repo, "payload-base", "payload-head", &ctx, true).unwrap();
        assert_eq!(out.len(), 1, "tree diff joins the rename: {out:?}");
        assert_eq!(out[0].path, PathBuf::from("after.py"));
        assert_eq!(out[0].status, ChangeStatus::Modified);
        assert_eq!(out[0].source_path.as_deref(), Some(Path::new("before.py")));
    }

    #[test]
    fn push_events_fall_back_to_the_payload_when_refs_unresolvable() {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        let repo = gix::discover(dir.path()).unwrap();
        let payload = vec![mehen_git::ChangedFile {
            path: PathBuf::from("a.py"),
            status: ChangeStatus::Added,
            source_path: None,
        }];
        let ctx = Some(push_ctx(payload.clone()));
        let out = get_changed_files(&repo, "no-such-ref", "also-missing", &ctx, true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, payload[0].path);
    }

    #[test]
    fn empty_push_payloads_are_authoritative_even_when_refs_resolve() {
        // A push whose fold is empty (add-then-remove, or a branch
        // created at an existing commit) changed nothing — the
        // resolvable HEAD~1 range would misreport the tip's last
        // commit as this push's changes.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("existing.py"), "x = 1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "one"]);
        std::fs::write(dir.path().join("tip.py"), "y = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "two"]);

        let repo = gix::discover(dir.path()).unwrap();
        let ctx = Some(push_ctx(Vec::new()));
        // HEAD~1..HEAD resolves and would report tip.py; the empty
        // payload must win.
        let out = get_changed_files(&repo, "HEAD~1", "HEAD", &ctx, true).unwrap();
        assert!(out.is_empty(), "empty payload is authoritative: {out:?}");
    }

    #[test]
    fn explicit_refs_ignore_the_push_payload_entirely() {
        // `--from`/`--to` overrides compare a range the event payload
        // knows nothing about: neither an empty fold (which would
        // blank out the report) nor an unresolvable-baseline fallback
        // may apply.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("existing.py"), "x = 1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "one"]);
        std::fs::write(dir.path().join("tip.py"), "y = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "two"]);

        let repo = gix::discover(dir.path()).unwrap();
        let ctx = Some(push_ctx(Vec::new()));
        let out = get_changed_files(&repo, "HEAD~1", "HEAD", &ctx, false).unwrap();
        assert_eq!(out.len(), 1, "explicit range must be diffed: {out:?}");
        assert_eq!(out[0].path, PathBuf::from("tip.py"));
    }

    #[test]
    fn unresolvable_baseline_payloads_degrade_modified_and_drop_deleted() {
        // With the baseline commit gone (force-push), every baseline
        // blob read would fail downstream: a `Modified` row would
        // fabricate a full-value-vs-zero delta and a `Deleted` row
        // would vanish silently. The fallback must degrade honestly:
        // modified files become current-state-only `Added` rows (🆕),
        // deletions are dropped with a warning.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        let repo = gix::discover(dir.path()).unwrap();
        let ctx = Some(push_ctx(vec![
            mehen_git::ChangedFile {
                path: PathBuf::from("kept.py"),
                status: ChangeStatus::Modified,
                source_path: None,
            },
            mehen_git::ChangedFile {
                path: PathBuf::from("gone.py"),
                status: ChangeStatus::Deleted,
                source_path: None,
            },
            mehen_git::ChangedFile {
                path: PathBuf::from("new.py"),
                status: ChangeStatus::Added,
                source_path: None,
            },
        ]));
        let out = get_changed_files(&repo, "no-such-ref", "also-missing", &ctx, true).unwrap();
        let mut rows: Vec<(&str, ChangeStatus)> = out
            .iter()
            .map(|f| (f.path.to_str().unwrap(), f.status))
            .collect();
        rows.sort_unstable_by_key(|(path, _)| *path);
        assert_eq!(
            rows,
            vec![
                ("kept.py", ChangeStatus::Added),
                ("new.py", ChangeStatus::Added)
            ]
        );
    }

    #[test]
    fn branch_creation_pushes_prefer_the_full_range_tree_diff() {
        // With `resolve_refs` supplying the first pushed commit's
        // parent as the baseline, a branch-creation push gets a
        // full-range tree diff (which carries rename identity the
        // payload can never express). The payload — deliberately
        // incomplete here — must lose when the baseline resolves.
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q", "-b", "main"]);
        git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("base.py"), "base = 0\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "main base"]);
        // The "pushed branch": two commits on top of main.
        std::fs::write(dir.path().join("first.py"), "a = 1\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "one"]);
        std::fs::write(dir.path().join("second.py"), "b = 2\n").unwrap();
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "two"]);

        let repo = gix::discover(dir.path()).unwrap();
        let ctx = push_ctx(vec![mehen_git::ChangedFile {
            path: PathBuf::from("first.py"),
            status: ChangeStatus::Added,
            source_path: None,
        }]);
        // The baseline resolve_refs would supply: first pushed
        // commit's parent = HEAD~2 here.
        let out = get_changed_files(&repo, "HEAD~2", "HEAD", &Some(ctx), true).unwrap();
        let mut paths: Vec<&str> = out.iter().filter_map(|f| f.path.to_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec!["first.py", "second.py"],
            "the resolvable full-range tree diff must win over the payload"
        );
    }

    fn analysis_with_diagnostics(diagnostics: Vec<ParseDiagnostic>) -> LanguageAnalysis {
        LanguageAnalysis {
            language: Language::Rust,
            backend: AnalysisBackend::TreeSitter,
            diagnostics,
            root: MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty()),
            contributions: Vec::new(),
        }
    }

    #[test]
    fn collect_diagnostics_records_warning_only_batches() {
        // Regression: prior gate dropped warning-only batches before
        // they reached `analysis_errors`, so a Ruff-style recoverable
        // parse warning or a markdown cross-reference warning would
        // never surface in `mehen diff --format json`. The
        // `analysis_errors` field carries `severity` per entry, so
        // CLI exit-code routing can still distinguish warning vs.
        // error vs. fatal — but emitting them is required so callers
        // can see them at all.
        let analysis =
            analysis_with_diagnostics(vec![ParseDiagnostic::warning("python.style", "long line")]);
        let mut report = empty_report();
        collect_diagnostics(
            &mut report,
            &Utf8PathBuf::from("src/main.py"),
            DiffSide::Head,
            &analysis,
        );
        assert_eq!(report.analysis_errors.len(), 1);
        let rec = &report.analysis_errors[0];
        assert_eq!(rec.path, Utf8PathBuf::from("src/main.py"));
        assert_eq!(rec.diagnostics.len(), 1);
        assert_eq!(rec.diagnostics[0].code, "python.style");
    }

    #[test]
    fn collect_diagnostics_skips_empty_batch() {
        let analysis = analysis_with_diagnostics(Vec::new());
        let mut report = empty_report();
        collect_diagnostics(
            &mut report,
            &Utf8PathBuf::from("src/main.py"),
            DiffSide::Head,
            &analysis,
        );
        assert!(report.analysis_errors.is_empty());
    }

    #[test]
    fn collect_diagnostics_records_blocking_batch() {
        let analysis = analysis_with_diagnostics(vec![
            ParseDiagnostic::warning("python.style", "long line"),
            ParseDiagnostic::error("python.syntax_error", "unexpected token"),
        ]);
        let mut report = empty_report();
        collect_diagnostics(
            &mut report,
            &Utf8PathBuf::from("src/main.py"),
            DiffSide::Base,
            &analysis,
        );
        assert_eq!(report.analysis_errors.len(), 1);
        // Both diagnostics are preserved, so CLI exit-code routing
        // still sees the error severity.
        assert_eq!(report.analysis_errors[0].diagnostics.len(), 2);
    }

    #[test]
    fn higher_is_worse_threshold_above_limit_violates() {
        let analysis = analysis_with_metric("cognitive.sum", 42.0);
        let thresholds = vec![Threshold::new(
            "cognitive.sum".parse().unwrap(),
            30.0,
            Polarity::HigherIsWorse,
        )];
        let mut report = empty_report();
        evaluate_thresholds(
            &mut report,
            &Utf8PathBuf::from("src/main.rs"),
            &thresholds,
            &analysis,
        );
        assert_eq!(report.threshold_violations.len(), 1);
        let v = &report.threshold_violations[0];
        assert_eq!(v.path, "src/main.rs");
        assert_eq!(v.evaluation.actual, 42.0);
        assert_eq!(v.evaluation.limit, 30.0);
        assert!(v.evaluation.violated);
    }

    #[test]
    fn higher_is_worse_threshold_at_or_below_limit_does_not_violate() {
        let analysis = analysis_with_metric("cognitive.sum", 30.0);
        let thresholds = vec![Threshold::new(
            "cognitive.sum".parse().unwrap(),
            30.0,
            Polarity::HigherIsWorse,
        )];
        let mut report = empty_report();
        evaluate_thresholds(
            &mut report,
            &Utf8PathBuf::from("src/main.rs"),
            &thresholds,
            &analysis,
        );
        assert!(report.threshold_violations.is_empty());
    }

    #[test]
    fn higher_is_better_threshold_below_limit_violates() {
        let analysis = analysis_with_metric("mi.visual_studio", 49.0);
        let thresholds = vec![Threshold::new(
            "mi.visual_studio".parse().unwrap(),
            50.0,
            Polarity::HigherIsBetter,
        )];
        let mut report = empty_report();
        evaluate_thresholds(
            &mut report,
            &Utf8PathBuf::from("src/main.rs"),
            &thresholds,
            &analysis,
        );
        assert_eq!(report.threshold_violations.len(), 1);
        assert!(report.threshold_violations[0].evaluation.violated);
    }

    #[test]
    fn multiple_thresholds_each_evaluated_independently() {
        let mut analysis = analysis_with_metric("cyclomatic.sum", 50.0);
        analysis
            .root
            .metrics
            .insert(MetricKey::new("cognitive.sum"), 5.0);
        let thresholds = vec![
            Threshold::new(
                "cyclomatic.sum".parse().unwrap(),
                10.0,
                Polarity::HigherIsWorse,
            ),
            Threshold::new(
                "cognitive.sum".parse().unwrap(),
                30.0,
                Polarity::HigherIsWorse,
            ),
        ];
        let mut report = empty_report();
        evaluate_thresholds(
            &mut report,
            &Utf8PathBuf::from("src/main.rs"),
            &thresholds,
            &analysis,
        );
        // Only cyclomatic.sum exceeds its limit; cognitive.sum is fine.
        assert_eq!(report.threshold_violations.len(), 1);
        assert_eq!(
            report.threshold_violations[0]
                .evaluation
                .selector
                .key
                .as_str(),
            "cyclomatic"
        );
    }

    #[test]
    fn empty_thresholds_produce_no_violations() {
        let analysis = analysis_with_metric("cognitive.sum", 999.0);
        let mut report = empty_report();
        evaluate_thresholds(
            &mut report,
            &Utf8PathBuf::from("src/main.rs"),
            &[],
            &analysis,
        );
        assert!(report.threshold_violations.is_empty());
    }

    #[test]
    fn path_is_selected_treats_curdir_as_match_all() {
        // Regression: callers that scope `analyze_diff` to "the whole
        // repo" by passing `"."` (or `"./src"` for "src and below")
        // used to silently match nothing because raw `starts_with`
        // never strips the `.` component. The normalized prefix
        // collapses `"."` to empty (= match all) and `"./src"` to
        // `"src"` so changed files are actually included.
        let changed = Utf8PathBuf::from("src/main.rs");

        // `"."` selects every file.
        assert!(path_is_selected(&changed, &[Utf8PathBuf::from(".")]));
        // `""` likewise — both spellings of "root" must match.
        assert!(path_is_selected(&changed, &[Utf8PathBuf::from("")]));
        // `"./src"` is a real prefix of `src/main.rs`.
        assert!(path_is_selected(&changed, &[Utf8PathBuf::from("./src")]));
        // A directory we're *not* under must still fail.
        assert!(!path_is_selected(&changed, &[Utf8PathBuf::from("./tests")]));
    }

    #[test]
    fn normalize_utf8_filter_strips_curdir_components() {
        assert_eq!(
            normalize_utf8_filter(&Utf8PathBuf::from("./src")),
            Utf8PathBuf::from("src"),
        );
        assert_eq!(
            normalize_utf8_filter(&Utf8PathBuf::from(".")),
            Utf8PathBuf::from(""),
        );
        assert_eq!(
            normalize_utf8_filter(&Utf8PathBuf::from("./a/./b")),
            Utf8PathBuf::from("a/b"),
        );
        assert_eq!(
            normalize_utf8_filter(&Utf8PathBuf::from("src")),
            Utf8PathBuf::from("src"),
        );
    }

    // ── pre-1.0 CLI orchestrator tests ─────────────────────────────────

    use clap::Parser as _;

    #[derive(clap::Parser, Debug)]
    struct TestDiffCli {
        #[command(flatten)]
        opts: DiffOpts,
    }

    #[test]
    fn test_parse_metric_selectors_defaults() {
        // The §9.4 default comment set: one column per orthogonal
        // dimension plus the two change-risk history signals.
        let selectors = parse_metric_selectors(&[]);
        assert_eq!(selectors.len(), 5);
        assert_eq!(selectors[0].name, "cognitive");
        assert_eq!(selectors[1].name, "abc");
        assert_eq!(selectors[2].name, "mi.visual_studio");
        assert_eq!(selectors[3].name, "history.hotspot");
        assert_eq!(selectors[4].name, "history.churn.relative");
    }

    #[test]
    fn test_parse_metric_selectors_custom() {
        let specs = vec!["mi.original".to_string(), "halstead.volume".to_string()];
        let selectors = parse_metric_selectors(&specs);
        assert_eq!(selectors.len(), 2);
        assert_eq!(selectors[0].name, "mi.original");
        assert_eq!(selectors[0].polarity, SelectorPolarity::HigherIsBetter);
        assert_eq!(selectors[1].name, "halstead.volume");
        assert_eq!(selectors[1].polarity, SelectorPolarity::LowerIsBetter);
    }

    #[test]
    fn test_parse_metric_selectors_all_mi_variants() {
        let specs = vec![
            "mi.original".to_string(),
            "mi.sei".to_string(),
            "mi.visual_studio".to_string(),
        ];
        let selectors = parse_metric_selectors(&specs);
        assert_eq!(selectors.len(), 3);
        assert_eq!(selectors[0].name, "mi.original");
        assert_eq!(selectors[1].name, "mi.sei");
        assert_eq!(selectors[2].name, "mi.visual_studio");
        for sel in &selectors {
            assert_eq!(sel.polarity, SelectorPolarity::HigherIsBetter);
        }
    }

    #[test]
    fn test_parse_metric_selectors_bare_mi_is_unknown() {
        let specs = vec!["mi".to_string()];
        let selectors = parse_metric_selectors(&specs);
        assert!(selectors.is_empty());
    }

    #[test]
    fn test_parse_metric_selectors_polarity_override() {
        let specs = vec![
            "+nom.functions".to_string(),
            "-mi.visual_studio".to_string(),
        ];
        let selectors = parse_metric_selectors(&specs);
        assert_eq!(selectors.len(), 2);
        assert_eq!(selectors[0].name, "nom.functions");
        assert_eq!(selectors[0].polarity, SelectorPolarity::HigherIsBetter);
        assert_eq!(selectors[1].name, "mi.visual_studio");
        assert_eq!(selectors[1].polarity, SelectorPolarity::LowerIsBetter);
    }

    #[test]
    fn test_parse_metric_selectors_unknown() {
        let specs = vec!["nonexistent".to_string()];
        let selectors = parse_metric_selectors(&specs);
        assert!(selectors.is_empty());
    }

    #[test]
    fn test_ignore_git_attributes_defaults_to_true() {
        let cli = TestDiffCli::try_parse_from(["mehen"]).unwrap();
        assert!(cli.opts.ignore_git_attributes);
    }

    #[test]
    fn test_ignore_git_attributes_accepts_bare_flag() {
        let cli = TestDiffCli::try_parse_from(["mehen", "--ignore-git-attributes"]).unwrap();
        assert!(cli.opts.ignore_git_attributes);
    }

    #[test]
    fn test_ignore_git_attributes_can_be_disabled() {
        let cli = TestDiffCli::try_parse_from(["mehen", "--ignore-git-attributes=false"]).unwrap();
        assert!(!cli.opts.ignore_git_attributes);
    }

    #[test]
    fn test_ignore_generated_remains_a_compatibility_alias() {
        let cli = TestDiffCli::try_parse_from(["mehen", "--ignore-generated=false"]).unwrap();
        assert!(!cli.opts.ignore_git_attributes);
    }

    #[test]
    fn test_trend_emoji_lower_is_better() {
        assert_eq!(
            trend_emoji(1.0, SelectorPolarity::LowerIsBetter),
            "\u{1F534}"
        );
        assert_eq!(
            trend_emoji(-1.0, SelectorPolarity::LowerIsBetter),
            "\u{1F7E2}"
        );
        assert_eq!(
            trend_emoji(0.0, SelectorPolarity::LowerIsBetter),
            "\u{26AA}"
        );
    }

    #[test]
    fn test_trend_emoji_higher_is_better() {
        assert_eq!(
            trend_emoji(1.0, SelectorPolarity::HigherIsBetter),
            "\u{1F7E2}"
        );
        assert_eq!(
            trend_emoji(-1.0, SelectorPolarity::HigherIsBetter),
            "\u{1F534}"
        );
        assert_eq!(
            trend_emoji(0.0, SelectorPolarity::HigherIsBetter),
            "\u{26AA}"
        );
    }

    #[test]
    fn test_format_f64_integer() {
        assert_eq!(format_f64(42.0), "42");
        assert_eq!(format_f64(0.0), "0");
    }

    #[test]
    fn test_format_f64_decimal() {
        assert_eq!(format_f64(2.75), "2.75");
        assert_eq!(format_f64(100.567), "100.57");
    }

    #[test]
    fn test_format_metric_cell_new() {
        let md = MetricDiff {
            name: "cyclomatic",
            label: "Cyclomatic",
            current: 5.0,
            baseline: 0.0,
            delta: 5.0,
            polarity: SelectorPolarity::LowerIsBetter,
            is_new: true,
            is_deleted: false,
        };
        assert_eq!(format_metric_cell(&md, "main"), "5 \u{1F195}");
    }

    #[test]
    fn test_format_metric_cell_unchanged() {
        let md = MetricDiff {
            name: "cyclomatic",
            label: "Cyclomatic",
            current: 5.0,
            baseline: 5.0,
            delta: 0.0,
            polarity: SelectorPolarity::LowerIsBetter,
            is_new: false,
            is_deleted: false,
        };
        assert_eq!(format_metric_cell(&md, "main"), "5 \u{26AA}");
    }

    #[test]
    fn test_format_metric_cell_increase_lower_is_better() {
        let md = MetricDiff {
            name: "cyclomatic",
            label: "Cyclomatic",
            current: 12.0,
            baseline: 8.0,
            delta: 4.0,
            polarity: SelectorPolarity::LowerIsBetter,
            is_new: false,
            is_deleted: false,
        };
        assert_eq!(format_metric_cell(&md, "main"), "12 (main: 8) \u{1F534}");
    }

    #[test]
    fn test_format_metric_cell_deleted() {
        let md = MetricDiff {
            name: "cyclomatic",
            label: "Cyclomatic",
            current: 0.0,
            baseline: 10.0,
            delta: -10.0,
            polarity: SelectorPolarity::LowerIsBetter,
            is_new: false,
            is_deleted: true,
        };
        assert_eq!(format_metric_cell(&md, "main"), "0 (was: 10) \u{1F7E2}");
    }

    #[test]
    fn test_file_diff_all_unchanged() {
        let diff = FileDiff {
            path: PathBuf::from("foo.rs"),
            metrics: vec![MetricDiff {
                name: "cyclomatic",
                label: "Cyclomatic",
                current: 5.0,
                baseline: 5.0,
                delta: 0.0,
                polarity: SelectorPolarity::LowerIsBetter,
                is_new: false,
                is_deleted: false,
            }],
            is_new: false,
            is_deleted: false,
            functions: 0,
        };
        assert!(diff.all_unchanged());
    }

    /// `DiffOpts` fixture for ref-resolution tests — only `from`/`to`
    /// vary; everything else is the clap default.
    fn resolve_refs_opts(from: Option<&str>, to: Option<&str>) -> DiffOpts {
        DiffOpts {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
            metrics: vec![],
            paths: vec![],
            include: vec![],
            exclude: vec![],
            output_format: None,
            show_unchanged: false,
            ignore_git_attributes: true,
            fail_on: vec![],
        }
    }

    #[test]
    fn test_resolve_refs_explicit() {
        let opts = resolve_refs_opts(Some("abc"), Some("def"));
        let (from, to) = resolve_refs(&opts, &None);
        assert_eq!(from, "abc");
        assert_eq!(to, "def");
    }

    #[test]
    fn test_resolve_refs_no_ci() {
        let opts = resolve_refs_opts(None, None);
        let (from, to) = resolve_refs(&opts, &None);
        assert_eq!(from, "main");
        assert_eq!(to, "HEAD");
    }

    #[test]
    fn test_resolve_refs_github_pr() {
        let ctx = ci::CiContext {
            provider: ci::CiProvider::GitHubActions,
            event_name: "pull_request".to_string(),
            base_ref: Some("develop".to_string()),
            head_sha: Some("abc123".to_string()),
            before_sha: None,
            first_commit_sha: None,
            changed_files: None,
            pr_number: Some(42),
            repository: Some("owner/repo".to_string()),
        };
        let opts = resolve_refs_opts(None, None);
        let (from, to) = resolve_refs(&opts, &Some(ctx));
        assert_eq!(from, "origin/develop");
        assert_eq!(to, "abc123");
    }

    #[test]
    fn test_resolve_refs_github_push() {
        let ctx = ci::CiContext {
            provider: ci::CiProvider::GitHubActions,
            event_name: "push".to_string(),
            base_ref: None,
            head_sha: Some("def456".to_string()),
            before_sha: None,
            first_commit_sha: None,
            changed_files: None,
            pr_number: None,
            repository: Some("owner/repo".to_string()),
        };
        let opts = resolve_refs_opts(None, None);
        let (from, to) = resolve_refs(&opts, &Some(ctx));
        assert_eq!(from, "HEAD~1");
        assert_eq!(to, "def456");
    }

    #[test]
    fn test_resolve_refs_github_push_uses_payload_before_sha() {
        // A multi-commit push must diff against the branch tip before
        // the push, not just the final commit's parent — otherwise
        // renames/baselines from earlier commits in the push vanish.
        let ctx = ci::CiContext {
            provider: ci::CiProvider::GitHubActions,
            event_name: "push".to_string(),
            base_ref: None,
            head_sha: Some("def456".to_string()),
            before_sha: Some("abc999".to_string()),
            first_commit_sha: None,
            changed_files: None,
            pr_number: None,
            repository: Some("owner/repo".to_string()),
        };
        let opts = resolve_refs_opts(None, None);
        let (from, to) = resolve_refs(&opts, &Some(ctx));
        assert_eq!(from, "abc999");
        assert_eq!(to, "def456");
    }

    #[test]
    fn test_resolve_refs_branch_creation_uses_first_pushed_parent() {
        // Branch creation has no `before`; the parent of the first
        // pushed commit is the right analysis baseline so files
        // changed only in earlier pushed commits still show deltas.
        let ctx = ci::CiContext {
            provider: ci::CiProvider::GitHubActions,
            event_name: "push".to_string(),
            base_ref: None,
            head_sha: Some("def456".to_string()),
            before_sha: None,
            first_commit_sha: Some("f1r5t".to_string()),
            changed_files: None,
            pr_number: None,
            repository: Some("owner/repo".to_string()),
        };
        let opts = resolve_refs_opts(None, None);
        let (from, to) = resolve_refs(&opts, &Some(ctx));
        assert_eq!(from, "f1r5t~1");
        assert_eq!(to, "def456");
    }

    #[test]
    fn test_normalize_path_filters() {
        let paths = normalize_path_filters(&[
            PathBuf::from("."),
            PathBuf::from("./internal"),
            PathBuf::from("cmd/tally/"),
        ]);

        assert_eq!(
            paths,
            vec![
                PathBuf::new(),
                PathBuf::from("internal"),
                PathBuf::from("cmd/tally")
            ]
        );
    }

    #[test]
    fn test_legacy_path_is_selected() {
        let paths = vec![PathBuf::from("internal"), PathBuf::from("main.go")];

        assert!(legacy_path_is_selected(
            Path::new("internal/config/config.go"),
            &paths
        ));
        assert!(legacy_path_is_selected(Path::new("main.go"), &paths));
        assert!(!legacy_path_is_selected(
            Path::new("internal2/config.go"),
            &paths
        ));
        assert!(!legacy_path_is_selected(
            Path::new("cmd/tally/main.go"),
            &paths
        ));

        let paths_with_root = vec![PathBuf::from("internal"), PathBuf::new()];
        assert!(legacy_path_is_selected(
            Path::new("cmd/tally/main.go"),
            &paths_with_root
        ));
    }

    #[test]
    fn test_diff_filter_reads_all_default_exclusion_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
*.rs linguist-generated
src/manual.rs -linguist-generated
src/false.rs linguist-generated=false
src/unspecified.rs !linguist-generated
src/value.txt linguist-generated=true
src/vendor.txt linguist-vendored
src/archive.txt binary
",
        )
        .unwrap();

        let mut filter = GitAttributeFilter::new(&repo).unwrap();

        assert!(
            filter
                .excludes_relative_path(Path::new("src/generated.rs"))
                .unwrap()
        );
        assert!(
            !filter
                .excludes_relative_path(Path::new("src/manual.rs"))
                .unwrap()
        );
        assert!(
            !filter
                .excludes_relative_path(Path::new("src/false.rs"))
                .unwrap()
        );
        assert!(
            !filter
                .excludes_relative_path(Path::new("src/unspecified.rs"))
                .unwrap()
        );
        for path in ["src/value.txt", "src/vendor.txt", "src/archive.txt"] {
            assert!(filter.excludes_relative_path(Path::new(path)).unwrap());
        }
    }

    // ── `--fail-on new-broken-link` gating tests ───────────────────────
    //
    // Ensure the CI gate keys on `(class, destination)` identity — a link
    // that merely shifts to a different line number MUST NOT trip the gate,
    // but a duplicate broken destination MUST.

    fn broken_link_for_fail_on(
        line: u64,
        class: mehen_markdown::types::LinkClass,
        destination: &str,
    ) -> mehen_markdown::types::LinkRecord {
        mehen_markdown::types::LinkRecord {
            line,
            class,
            destination: destination.to_string(),
            text: String::new(),
            is_image: false,
            is_bare_url: false,
            resolved: Some(false),
        }
    }

    fn minimal_md_metrics(path: &str) -> mehen_markdown::types::MarkdownMetrics {
        mehen_markdown::types::MarkdownMetrics {
            path: path.to_string(),
            loc: Default::default(),
            loc_ratios: Default::default(),
            size: Default::default(),
            ecu_inputs: Default::default(),
            sections: vec![],
            complexity: Default::default(),
            links: Default::default(),
            link_records: vec![],
            visuals: Default::default(),
            tables: Default::default(),
            maintainability: Default::default(),
            grounding: Default::default(),
            ai_era: Default::default(),
            review: Default::default(),
            artifacts: vec![],
            prose: Default::default(),
        }
    }

    #[test]
    fn fail_on_new_broken_link_ignores_line_only_shift() {
        let mut head = minimal_md_metrics("docs/a.md");
        head.link_records = vec![broken_link_for_fail_on(
            42,
            mehen_markdown::types::LinkClass::Relative,
            "./guide.md",
        )];
        let mut base = minimal_md_metrics("docs/a.md");
        base.link_records = vec![broken_link_for_fail_on(
            10,
            mehen_markdown::types::LinkClass::Relative,
            "./guide.md",
        )];

        let doc = DocDiffFile {
            path: PathBuf::from("docs/a.md"),
            head: Some(head),
            base: Some(base),
            is_new: false,
            is_deleted: false,
        };

        let flags = vec![FailOn::NewBrokenLink];
        let failures = evaluate_fail_on(&flags, std::slice::from_ref(&doc));
        assert!(
            failures.is_empty(),
            "line-only shift must not trip new-broken-link; got: {failures:?}",
        );
    }

    #[test]
    fn fail_on_new_broken_link_trips_on_new_occurrence() {
        // Head has 2 broken refs to the same destination; base has 1. The
        // second occurrence is net-new so the gate must fire.
        let mut head = minimal_md_metrics("docs/a.md");
        head.link_records = vec![
            broken_link_for_fail_on(10, mehen_markdown::types::LinkClass::Relative, "./g.md"),
            broken_link_for_fail_on(20, mehen_markdown::types::LinkClass::Relative, "./g.md"),
        ];
        let mut base = minimal_md_metrics("docs/a.md");
        base.link_records = vec![broken_link_for_fail_on(
            10,
            mehen_markdown::types::LinkClass::Relative,
            "./g.md",
        )];

        let doc = DocDiffFile {
            path: PathBuf::from("docs/a.md"),
            head: Some(head),
            base: Some(base),
            is_new: false,
            is_deleted: false,
        };

        let flags = vec![FailOn::NewBrokenLink];
        let failures = evaluate_fail_on(&flags, std::slice::from_ref(&doc));
        assert_eq!(failures.len(), 1);
        assert!(failures[0].starts_with("new-broken-link:"));
    }

    #[test]
    fn fail_on_new_broken_link_trips_on_brand_new_destination() {
        let mut head = minimal_md_metrics("docs/a.md");
        head.link_records = vec![broken_link_for_fail_on(
            5,
            mehen_markdown::types::LinkClass::Relative,
            "./added.md",
        )];
        let base = minimal_md_metrics("docs/a.md");

        let doc = DocDiffFile {
            path: PathBuf::from("docs/a.md"),
            head: Some(head),
            base: Some(base),
            is_new: false,
            is_deleted: false,
        };

        let flags = vec![FailOn::NewBrokenLink];
        let failures = evaluate_fail_on(&flags, std::slice::from_ref(&doc));
        assert_eq!(failures.len(), 1);
    }

    // ── print_json error-propagation ────────────────────────────────────

    #[test]
    fn print_json_happy_path_is_ok() {
        let diffs: Vec<FileDiff> = vec![FileDiff {
            path: PathBuf::from("a.rs"),
            metrics: vec![],
            is_new: false,
            is_deleted: false,
            functions: 0,
        }];
        let res = print_json(&diffs, None);
        assert!(res.is_ok(), "valid input must serialize cleanly");
    }

    #[test]
    fn print_json_returns_result_type() {
        // §39 regression guard: print_json must return `Result<_, _>` so
        // callers can exit non-zero on serialization failure. Before, the
        // emitter used `unwrap_or_default` and silently wrote an empty
        // JSON document to stdout when serde_json failed.
        let diffs: Vec<FileDiff> = vec![];
        let res: Result<(), Box<dyn std::error::Error>> = print_json(&diffs, None);
        assert!(res.is_ok());
    }

    // ── `--fail-on` CLI-parse validation ────────────────────────────────

    #[test]
    fn fail_on_parser_accepts_every_documented_value() {
        let cli = TestDiffCli::try_parse_from([
            "mehen",
            "--fail-on",
            "dmi-drop,new-broken-link,filler-high,all",
        ])
        .expect("every documented value must parse");
        assert_eq!(
            cli.opts.fail_on,
            vec![
                FailOn::DmiDrop,
                FailOn::NewBrokenLink,
                FailOn::FillerHigh,
                FailOn::All,
            ]
        );
    }

    #[test]
    fn fail_on_parser_trims_and_lowercases() {
        let cli = TestDiffCli::try_parse_from(["mehen", "--fail-on", "  Dmi-Drop , ALL "])
            .expect("case and whitespace must be normalized");
        assert_eq!(cli.opts.fail_on, vec![FailOn::DmiDrop, FailOn::All]);
    }

    #[test]
    fn fail_on_parser_rejects_unknown_value() {
        let err = TestDiffCli::try_parse_from(["mehen", "--fail-on", "new-borken-link"])
            .expect_err("unknown value must be rejected");
        assert!(
            matches!(
                err.kind(),
                clap::error::ErrorKind::InvalidValue | clap::error::ErrorKind::ValueValidation,
            ),
            "expected InvalidValue or ValueValidation, got: {:?}",
            err.kind(),
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("new-borken-link"),
            "error must mention the offending value, got: {rendered}"
        );
    }

    #[test]
    fn fail_on_parser_rejects_partial_match_in_list() {
        let err = TestDiffCli::try_parse_from(["mehen", "--fail-on", "dmi-drop,filler-hihg"])
            .expect_err("list with an invalid entry must be rejected");
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::InvalidValue | clap::error::ErrorKind::ValueValidation,
        ));
        assert!(err.to_string().contains("filler-hihg"));
    }
}
