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

fn analyze_diff_in_repo(input: DiffInput, repo: &gix::Repository) -> Result<DiffReport, DiffError> {
    let registry = Arc::new(AnalyzerRegistry::default_set());
    let changed = mehen_git::changed_files(repo, &input.from, &input.to).map_err(DiffError::Git)?;
    let mut git_attribute_filter =
        GitAttributeFilter::from_revision(repo, &input.to).map_err(|error| {
            DiffError::Git(GitError::Internal(format!(
                "failed to configure Git attribute filtering for {}: {error}",
                input.to
            )))
        })?;

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
        if git_attribute_filter
            .excludes_relative_path(&cf.path)
            .map_err(|error| {
                DiffError::Git(GitError::Internal(format!(
                    "failed to read Git attributes for {}: {error}",
                    cf.path.display()
                )))
            })?
        {
            continue;
        }

        let Some(language) = detect_language(&utf8_path) else {
            // Skip files we don't recognize.
            continue;
        };

        let base_text = if cf.status == ChangeStatus::Added {
            None
        } else {
            mehen_git::read_blob(repo, &input.from, &cf.path)
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
        if let Some(analysis) = head_analysis.as_ref()
            && !has_blocking_diagnostic(&analysis.diagnostics)
        {
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
}

impl FileDiff {
    fn all_unchanged(&self) -> bool {
        self.metrics.iter().all(|m| m.delta == 0.0)
    }

    /// Sort key: total function count descending, then path ascending.
    fn sort_key(&self) -> (std::cmp::Reverse<i64>, PathBuf) {
        let functions = self
            .metrics
            .iter()
            .find(|m| m.name == "nom.functions")
            .map(|m| m.current as i64)
            .unwrap_or(0);
        (std::cmp::Reverse(functions), self.path.clone())
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
    /// (default: cyclomatic,cognitive,nom.functions,loc.lloc,mi.visual_studio).
    /// Prefix with + for higher-is-better, - for lower-is-better.
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
    let changed = get_changed_files(&repo, &from_ref, &to_ref, &ci_ctx)?;

    // 3. Filter files
    let include = mk_globset(opts.include);
    let exclude = mk_globset(opts.exclude);
    let paths = normalize_path_filters(&opts.paths);
    // When the caller passes explicit `--metric` names, that one list applies
    // to every file. With no `--metric`, defaults are resolved *per file's
    // language*: SQL files publish only `sql.*` keys, so the source-code
    // defaults (`cyclomatic`, …) would read 0 for them and drop the file as
    // unchanged (Codex P2). `explicit_metrics` selects between the two modes.
    let explicit_metrics = !opts.metrics.is_empty();
    let selectors = parse_metric_selectors(&opts.metrics);
    let mut git_attribute_filter = opts
        .ignore_git_attributes
        .then(|| GitAttributeFilter::from_revision(&repo, &to_ref))
        .transpose()?;

    let registry = Arc::new(AnalyzerRegistry::default_set());
    let analysis_config = AnalysisConfig::default();

    let mut filtered: Vec<(mehen_git::ChangedFile, Utf8PathBuf, Language)> = Vec::new();
    let mut markdown_files: Vec<mehen_git::ChangedFile> = Vec::new();
    for cf in changed {
        let p = &cf.path;
        if !legacy_path_is_selected(p, &paths)
            || (!include.is_empty() && !include.is_match(p))
            || (!exclude.is_empty() && exclude.is_match(p))
        {
            continue;
        }

        if let Some(filter) = git_attribute_filter.as_mut()
            && filter.excludes_relative_path(p)?
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

        let baseline_space: Option<MetricSpace> = if is_new {
            None
        } else {
            match mehen_git::read_blob(&repo, &from_ref, &cf.path) {
                Ok(Some(bytes)) => analyze(bytes, "baseline"),
                Ok(None) => None,
                Err(e) => {
                    log::warn!("Skipping baseline for {}: {e}", cf.path.display());
                    None
                }
            }
        };

        let current_space: Option<MetricSpace> = if is_deleted {
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
                    is_new: is_new && baseline_space.is_none(),
                    is_deleted,
                }
            })
            .collect();

        diffs.push(FileDiff {
            path: cf.path.clone(),
            metrics: metric_diffs,
            is_new: is_new && baseline_space.is_none(),
            is_deleted,
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
            let base_metrics = if is_candidate_new {
                None
            } else {
                match mehen_git::read_blob(&repo, &from_ref, &cf.path) {
                    Ok(Some(bytes)) => Some(mehen_markdown::analyze_markdown(
                        &String::from_utf8_lossy(&bytes),
                        &cf.path,
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
                "push" => "HEAD~1".to_string(),
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
) -> Result<Vec<mehen_git::ChangedFile>, GitError> {
    // For push events with changed_files from payload, use those
    // directly. The CI extractor folds per-commit `added` / `modified`
    // / `removed` into a final per-path `ChangeStatus` so the diff
    // downstream renders new/deleted files correctly (PR #95
    // `pullrequestreview-4318662855`).
    if let Some(ctx) = ci_ctx
        && ctx.event_name == "push"
        && let Some(ref files) = ctx.changed_files
    {
        return Ok(files.clone());
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
",
        )
        .unwrap();
        for name in ["kept.md", "generated.md", "vendored.md", "binary.md"] {
            std::fs::write(dir.path().join(name), "# Base\n").unwrap();
        }
        git_ok(dir.path(), &["add", "-A"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        git_ok(dir.path(), &["tag", "attribute-base"]);

        for name in ["kept.md", "generated.md", "vendored.md", "binary.md"] {
            std::fs::write(dir.path().join(name), "# Head\n\nChanged.\n").unwrap();
        }
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
        let selectors = parse_metric_selectors(&[]);
        assert_eq!(selectors.len(), 5);
        assert_eq!(selectors[0].name, "cyclomatic");
        assert_eq!(selectors[1].name, "cognitive");
        assert_eq!(selectors[2].name, "nom.functions");
        assert_eq!(selectors[3].name, "loc.lloc");
        assert_eq!(selectors[4].name, "mi.visual_studio");
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
        };
        assert!(diff.all_unchanged());
    }

    #[test]
    fn test_resolve_refs_explicit() {
        let opts = DiffOpts {
            from: Some("abc".to_string()),
            to: Some("def".to_string()),
            metrics: vec![],
            paths: vec![],
            include: vec![],
            exclude: vec![],
            output_format: None,
            show_unchanged: false,
            ignore_git_attributes: true,
            fail_on: vec![],
        };
        let (from, to) = resolve_refs(&opts, &None);
        assert_eq!(from, "abc");
        assert_eq!(to, "def");
    }

    #[test]
    fn test_resolve_refs_no_ci() {
        let opts = DiffOpts {
            from: None,
            to: None,
            metrics: vec![],
            paths: vec![],
            include: vec![],
            exclude: vec![],
            output_format: None,
            show_unchanged: false,
            ignore_git_attributes: true,
            fail_on: vec![],
        };
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
            changed_files: None,
            pr_number: Some(42),
            repository: Some("owner/repo".to_string()),
        };
        let opts = DiffOpts {
            from: None,
            to: None,
            metrics: vec![],
            paths: vec![],
            include: vec![],
            exclude: vec![],
            output_format: None,
            show_unchanged: false,
            ignore_git_attributes: true,
            fail_on: vec![],
        };
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
            changed_files: None,
            pr_number: None,
            repository: Some("owner/repo".to_string()),
        };
        let opts = DiffOpts {
            from: None,
            to: None,
            metrics: vec![],
            paths: vec![],
            include: vec![],
            exclude: vec![],
            output_format: None,
            show_unchanged: false,
            ignore_git_attributes: true,
            fail_on: vec![],
        };
        let (from, to) = resolve_refs(&opts, &Some(ctx));
        assert_eq!(from, "HEAD~1");
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
