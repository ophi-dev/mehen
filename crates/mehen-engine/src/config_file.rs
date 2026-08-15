// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Repository-local configuration (`mehen.toml` / `.mehen.toml`).
//!
//! The configuration file carries per-metric thresholds, optionally
//! overridden per language:
//!
//! ```toml
//! [thresholds]
//! cognitive = 15        # higher-is-worse metrics: the value is a maximum
//! loc.lloc = 500        # dotted and "quoted.key" spellings are equivalent
//! mi.visual_studio = 40 # higher-is-better metrics: the value is a minimum
//!
//! [languages.python.thresholds]
//! cognitive = 10        # overrides the global limit for Python files only
//! ```
//!
//! Thresholds gate the metrics a command actually reports: `mehen
//! metrics` evaluates every configured threshold against the file's
//! root metric set, while `mehen diff` and `mehen top-offenders`
//! evaluate the thresholds whose metric is among the selected output
//! columns. A metric the analyzed file does not publish (e.g. a
//! `sql.*` threshold against a Python file) is skipped — a missing
//! measurement is never treated as `0`.
//!
//! Any crossed threshold fails the command with exit code 1 after a
//! human-readable report on stderr (see [`render_threshold_report`]).
//!
//! Errors and reports render through [`miette`]'s graphical handler:
//! configuration mistakes point at the offending key inside the TOML
//! source and carry a `help:` suggestion; colors engage only when
//! stderr is a terminal (and `NO_COLOR` is unset).
//!
//! Discovery starts at the current working directory and walks up to
//! the filesystem root, taking the first `mehen.toml` (preferred) or
//! `.mehen.toml` found; `--config <PATH>` bypasses discovery.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use miette::{LabeledSpan, NamedSource};

use mehen_core::{Language, MetricKey, MetricSpace, Polarity, keys};

use crate::metric_selector::{KNOWN_METRICS, is_namespaced_higher_is_better, metric_set_key_for};

/// Recognized configuration file names, in preference order.
pub(crate) const CONFIG_FILE_NAMES: &[&str] = &["mehen.toml", ".mehen.toml"];

/// Metric families whose members (`<root>` or `<root>.<sub>`) the
/// source-code analyzers publish onto root `MetricSpace`s. Used to
/// validate configured threshold names beyond the curated
/// [`KNOWN_METRICS`] catalogue (e.g. `nargs`, `loc.sloc`,
/// `cognitive.max`, `nom.functions_max`).
const METRIC_FAMILY_ROOTS: &[&str] = &[
    "abc",
    "cognitive",
    "cyclomatic",
    "halstead",
    "loc",
    "mi",
    "nargs",
    "nexit",
    "nom",
    "npa",
    "npm",
    "wmc",
];

/// A parsed and validated configuration file.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// The file the configuration was loaded from (absolute when the
    /// path could be canonicalized).
    pub path: PathBuf,
    /// Per-metric threshold policy (global + per-language overrides).
    pub thresholds: ThresholdPolicy,
}

/// A configuration loading/validation error.
///
/// Implements [`miette::Diagnostic`]: where the mistake maps to a spot
/// in the TOML source, the diagnostic carries the file as
/// `source_code` plus a label pointing at the offending key, and a
/// `help:` suggestion. Render with [`render_config_error`].
///
/// The payload is boxed so `Result<_, ConfigError>` stays
/// pointer-sized on the happy path (clippy `result_large_err`).
#[derive(Debug)]
pub struct ConfigError(Box<ConfigErrorInner>);

#[derive(Debug)]
struct ConfigErrorInner {
    message: String,
    help: Option<String>,
    source_code: Option<NamedSource<String>>,
    labels: Vec<LabeledSpan>,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(Box::new(ConfigErrorInner {
            message: message.into(),
            help: None,
            source_code: None,
            labels: Vec::new(),
        }))
    }

    fn with_help(mut self, help: impl Into<String>) -> Self {
        self.0.help = Some(help.into());
        self
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl core::error::Error for ConfigError {}

impl miette::Diagnostic for ConfigError {
    fn code(&self) -> Option<Box<dyn fmt::Display + '_>> {
        Some(Box::new("mehen::config"))
    }

    fn help(&self) -> Option<Box<dyn fmt::Display + '_>> {
        self.0
            .help
            .as_ref()
            .map(|help| Box::new(help) as Box<dyn fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.0
            .source_code
            .as_ref()
            .map(|source| source as &dyn miette::SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        if self.0.labels.is_empty() {
            None
        } else {
            Some(Box::new(self.0.labels.iter().cloned()))
        }
    }
}

/// Builds [`ConfigError`]s that point back into the configuration
/// source. Spans are recovered by locating the offending key text —
/// exact enough for a caret, with a graceful span-less fallback when
/// the key cannot be found verbatim.
struct ErrorContext<'a> {
    text: &'a str,
    path: &'a Path,
}

impl ErrorContext<'_> {
    fn named_source(&self) -> NamedSource<String> {
        NamedSource::new(self.path.display().to_string(), self.text.to_string())
    }

    /// An error without a source span (e.g. structural problems that
    /// have no single key to point at).
    fn error(&self, message: impl Into<String>) -> ConfigError {
        ConfigError::new(message)
    }

    /// An error labeled at the first occurrence of `key` in the
    /// source (falling back to the key's last dotted segment, then to
    /// a span-less error).
    fn error_at(
        &self,
        key: &str,
        label: impl Into<String>,
        message: impl Into<String>,
    ) -> ConfigError {
        let mut error = ConfigError::new(message);
        if let Some((offset, len)) = key_span(self.text, key) {
            error.0.source_code = Some(self.named_source());
            error.0.labels = vec![LabeledSpan::at(offset..offset + len, label.into())];
        }
        error
    }

    /// An error labeled at an exact byte range (used for TOML syntax
    /// errors, which carry their own span).
    fn error_at_span(
        &self,
        span: std::ops::Range<usize>,
        label: impl Into<String>,
        message: impl Into<String>,
    ) -> ConfigError {
        let mut error = ConfigError::new(message);
        error.0.source_code = Some(self.named_source());
        error.0.labels = vec![LabeledSpan::at(span, label.into())];
        error
    }
}

/// Locate `key` in the configuration text: the full spelling first
/// (`loc.lloc`), then the last dotted segment (`lloc`) for keys the
/// user wrote as nested tables.
fn key_span(text: &str, key: &str) -> Option<(usize, usize)> {
    if let Some(offset) = text.find(key) {
        return Some((offset, key.len()));
    }
    let segment = key.rsplit('.').next()?;
    text.find(segment).map(|offset| (offset, segment.len()))
}

/// Where a resolved limit came from — drives the `[thresholds]` /
/// `[languages.<lang>]` pointer in the violation report so the user
/// can jump straight to the line to adjust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// The global `[thresholds]` table.
    Global,
    /// A `[languages.<lang>.thresholds]` override.
    Language(Language),
}

/// One crossed threshold: the measured value, the configured limit,
/// and enough context to render an actionable report line.
#[derive(Debug, Clone)]
pub struct ThresholdBreach {
    /// Display path of the offending file.
    pub path: String,
    /// Configured metric name (config spelling, e.g. `cognitive`).
    pub metric: String,
    /// Measured value at the evaluated (head) side.
    pub value: f64,
    /// Configured limit.
    pub limit: f64,
    /// Whether the limit is a maximum (`HigherIsWorse`) or a minimum
    /// (`HigherIsBetter`).
    pub polarity: Polarity,
    /// Which config table supplied the limit.
    pub source: LimitSource,
}

/// Per-metric limits: a global table plus per-language overrides.
#[derive(Debug, Clone, Default)]
pub struct ThresholdPolicy {
    global: BTreeMap<String, f64>,
    /// Sorted by canonical language id for deterministic iteration.
    per_language: Vec<(Language, BTreeMap<String, f64>)>,
}

impl ThresholdPolicy {
    /// True when no thresholds are configured at all.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.per_language.iter().all(|(_, t)| t.is_empty())
    }

    /// The effective `metric → (limit, source)` table for one
    /// language: the language override wins over the global limit,
    /// metric by metric.
    fn resolved_for(&self, language: Language) -> BTreeMap<&str, (f64, LimitSource)> {
        let mut resolved: BTreeMap<&str, (f64, LimitSource)> = self
            .global
            .iter()
            .map(|(metric, limit)| (metric.as_str(), (*limit, LimitSource::Global)))
            .collect();
        if let Some((_, overrides)) = self.per_language.iter().find(|(l, _)| *l == language) {
            for (metric, limit) in overrides {
                resolved.insert(metric.as_str(), (*limit, LimitSource::Language(language)));
            }
        }
        resolved
    }

    /// Evaluate the policy against one file's root metric set.
    ///
    /// `only_metrics` restricts evaluation to the metrics a command
    /// actually reports (`mehen diff` / `mehen top-offenders` pass
    /// their selected column names; `mehen metrics` passes `None`
    /// because its report carries the full metric set).
    ///
    /// A threshold whose metric the space does not publish is
    /// skipped: an absent measurement must not be compared as `0.0` —
    /// under a higher-is-better limit that would fabricate a
    /// violation, under higher-is-worse it would fabricate a pass.
    pub fn evaluate(
        &self,
        path: &str,
        language: Language,
        root: &MetricSpace,
        only_metrics: Option<&[&str]>,
    ) -> Vec<ThresholdBreach> {
        let mut breaches = Vec::new();
        for (metric, (limit, source)) in self.resolved_for(language) {
            if let Some(filter) = only_metrics
                && !filter.contains(&metric)
            {
                continue;
            }
            let Some(value) = lookup_metric(root, metric) else {
                continue;
            };
            let polarity = polarity_for_metric(metric);
            let violated = match polarity {
                Polarity::HigherIsWorse => value > limit,
                Polarity::HigherIsBetter => value < limit,
            };
            if violated {
                breaches.push(ThresholdBreach {
                    path: path.to_string(),
                    metric: metric.to_string(),
                    value,
                    limit,
                    polarity,
                    source,
                });
            }
        }
        breaches
    }
}

/// Read a configured metric from a root metric set, `None` when the
/// analyzer did not publish it.
///
/// Mirrors the CLI selector read path: the rolled-up scalars map to
/// their `.sum` key ([`metric_set_key_for`]), and aggregate suffixes
/// fall back to the underscore sub-bucket spelling
/// (`nom.functions.max` → `nom.functions_max`) plus the `avg` ↔
/// `average` spelling pair — but unlike the ranking reader, a miss
/// stays a miss instead of turning into `0.0`.
fn lookup_metric(root: &MetricSpace, name: &str) -> Option<f64> {
    let key = metric_set_key_for(name);
    if let Some(v) = root.metrics.get(&MetricKey::new(key)) {
        return Some(v.as_f64());
    }
    let (base, suffix) = key.rsplit_once('.')?;
    if !matches!(suffix, "min" | "max" | "avg" | "average" | "sum") {
        return None;
    }
    let mut candidates = vec![format!("{base}_{suffix}")];
    let alt = match suffix {
        "avg" => Some("average"),
        "average" => Some("avg"),
        _ => None,
    };
    if let Some(alt) = alt {
        candidates.push(format!("{base}.{alt}"));
        candidates.push(format!("{base}_{alt}"));
    }
    candidates
        .into_iter()
        .find_map(|candidate| root.metrics.get(&MetricKey::new(candidate)))
        .map(|v| v.as_f64())
}

/// Whether a configured limit is a minimum (higher-is-better metric)
/// or a maximum (everything else). Mirrors the ranking/diff polarity
/// defaults: `mi.*` and the enumerated namespaced quality scores are
/// higher-is-better.
fn polarity_for_metric(name: &str) -> Polarity {
    if name == "mi" || name.starts_with("mi.") || is_namespaced_higher_is_better(name) {
        Polarity::HigherIsBetter
    } else {
        Polarity::HigherIsWorse
    }
}

/// Load the configuration for this invocation.
///
/// With `explicit` (the `--config` flag) the file must exist and
/// parse. Otherwise the configuration is discovered by walking from
/// the current working directory up to the filesystem root; no file
/// found means no configuration (`Ok(None)`), which leaves every
/// command's behavior unchanged.
pub fn load_config(explicit: Option<&Path>) -> Result<Option<ConfigFile>, ConfigError> {
    let path = match explicit {
        Some(path) => {
            if !path.is_file() {
                return Err(ConfigError::new(format!(
                    "config file not found: `{}`",
                    path.display()
                ))
                .with_help("check the path passed to --config; it should point at a mehen.toml"));
            }
            path.to_path_buf()
        }
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| ConfigError::new(format!("cannot resolve current directory: {e}")))?;
            match discover_config_path(&cwd) {
                Some(path) => path,
                None => return Ok(None),
            }
        }
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::new(format!("failed to read `{}`: {e}", path.display())))?;
    // Canonicalize for the report footer so "which file set this
    // limit?" has an unambiguous answer even when discovery walked up
    // from a nested working directory.
    let display_path = std::fs::canonicalize(&path).unwrap_or(path);
    parse_config(&text, &display_path).map(Some)
}

/// Walk from `start` to the filesystem root, returning the first
/// configuration file found. `mehen.toml` wins over `.mehen.toml`
/// within the same directory (with a warning when both exist).
fn discover_config_path(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let visible = dir.join(CONFIG_FILE_NAMES[0]);
        let hidden = dir.join(CONFIG_FILE_NAMES[1]);
        match (visible.is_file(), hidden.is_file()) {
            (true, true) => {
                log::warn!(
                    "both `mehen.toml` and `.mehen.toml` exist in {}; using `mehen.toml`",
                    dir.display()
                );
                return Some(visible);
            }
            (true, false) => return Some(visible),
            (false, true) => return Some(hidden),
            (false, false) => {}
        }
    }
    None
}

/// Parse and validate configuration text. `path` is used for the
/// diagnostic source name and report messages.
fn parse_config(text: &str, path: &Path) -> Result<ConfigFile, ConfigError> {
    let ctx = ErrorContext { text, path };
    let root: toml::Table = match text.parse() {
        Ok(root) => root,
        Err(e) => {
            let message = format!("invalid TOML: {}", e.message());
            let error = match e.span() {
                Some(span) => ctx.error_at_span(span, "syntax error here", message),
                None => ctx.error(message),
            };
            return Err(error.with_help("fix the TOML syntax; see https://toml.io for the format"));
        }
    };

    let mut global: BTreeMap<String, f64> = BTreeMap::new();
    let mut per_language: Vec<(Language, BTreeMap<String, f64>)> = Vec::new();

    for (key, value) in &root {
        match key.as_str() {
            "thresholds" => {
                let table = expect_table(value, "thresholds", &ctx)?;
                collect_thresholds(table, "thresholds", &mut global, &ctx)?;
            }
            "languages" => {
                let table = expect_table(value, "languages", &ctx)?;
                for (lang_key, lang_value) in table {
                    let language = lang_key.parse::<Language>().map_err(|_| {
                        ctx.error_at(
                            lang_key,
                            "not a recognized language",
                            format!("unknown language `{lang_key}` in [languages]"),
                        )
                        .with_help(
                            "use a language identifier such as python, typescript, rust, go, … \
                             (aliases like `py`, `ts`, `rb` are accepted)",
                        )
                    })?;
                    if per_language.iter().any(|(l, _)| *l == language) {
                        return Err(ctx
                            .error_at(
                                lang_key,
                                "same language configured twice",
                                format!(
                                    "duplicate [languages.{lang_key}] section — another key \
                                     already configures `{}`",
                                    language.canonical()
                                ),
                            )
                            .with_help(
                                "language aliases refer to the same language; keep one section \
                                 per language",
                            ));
                    }
                    let lang_ctx = format!("languages.{lang_key}");
                    let lang_table = expect_table(lang_value, &lang_ctx, &ctx)?;
                    let mut thresholds: BTreeMap<String, f64> = BTreeMap::new();
                    for (sub_key, sub_value) in lang_table {
                        match sub_key.as_str() {
                            "thresholds" => {
                                let threshold_ctx = format!("{lang_ctx}.thresholds");
                                let table = expect_table(sub_value, &threshold_ctx, &ctx)?;
                                collect_thresholds(table, &threshold_ctx, &mut thresholds, &ctx)?;
                            }
                            other => {
                                return Err(ctx
                                    .error_at(
                                        other,
                                        "unrecognized key",
                                        format!("unknown key `{other}` in [{lang_ctx}]"),
                                    )
                                    .with_help(format!(
                                        "expected `thresholds` (as in [{lang_ctx}.thresholds])"
                                    )));
                            }
                        }
                    }
                    per_language.push((language, thresholds));
                }
            }
            other => {
                let mut help = "expected `thresholds` or `languages`".to_string();
                if let Some(candidate) = closest_candidate(other, &["thresholds", "languages"]) {
                    help.push_str(&format!("; did you mean `{candidate}`?"));
                }
                return Err(ctx
                    .error_at(
                        other,
                        "unrecognized key",
                        format!("unknown top-level key `{other}`"),
                    )
                    .with_help(help));
            }
        }
    }

    per_language.sort_by_key(|(language, _)| language.canonical());

    Ok(ConfigFile {
        path: path.to_path_buf(),
        thresholds: ThresholdPolicy {
            global,
            per_language,
        },
    })
}

fn expect_table<'v>(
    value: &'v toml::Value,
    context: &str,
    ctx: &ErrorContext<'_>,
) -> Result<&'v toml::Table, ConfigError> {
    value.as_table().ok_or_else(|| {
        ctx.error_at(
            context,
            "not a table",
            format!(
                "`{context}` must be a table (as in [{context}]), got {}",
                value.type_str()
            ),
        )
    })
}

/// Flatten one thresholds table into `metric name → limit` entries.
///
/// TOML turns a dotted key (`loc.lloc = 500`) into nested tables, so
/// nested tables are folded back into dotted metric names — the
/// quoted spelling (`"loc.lloc" = 500`) and the dotted spelling are
/// equivalent.
fn collect_thresholds(
    table: &toml::Table,
    context: &str,
    out: &mut BTreeMap<String, f64>,
    ctx: &ErrorContext<'_>,
) -> Result<(), ConfigError> {
    fn walk(
        table: &toml::Table,
        prefix: &str,
        context: &str,
        out: &mut BTreeMap<String, f64>,
        ctx: &ErrorContext<'_>,
    ) -> Result<(), ConfigError> {
        for (key, value) in table {
            let metric = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Integer(i) => {
                    insert_threshold(&metric, *i as f64, context, out, ctx)?;
                }
                toml::Value::Float(f) => {
                    insert_threshold(&metric, *f, context, out, ctx)?;
                }
                toml::Value::Table(nested) => {
                    walk(nested, &metric, context, out, ctx)?;
                }
                other => {
                    return Err(ctx
                        .error_at(
                            &metric,
                            "limit must be numeric",
                            format!(
                                "`{context}.{metric}` must be a number (the metric's limit), \
                                 got {}",
                                other.type_str()
                            ),
                        )
                        .with_help(format!("write a plain number, e.g. `{metric} = 15`")));
                }
            }
        }
        Ok(())
    }

    walk(table, "", context, out, ctx)
}

fn insert_threshold(
    metric: &str,
    limit: f64,
    context: &str,
    out: &mut BTreeMap<String, f64>,
    ctx: &ErrorContext<'_>,
) -> Result<(), ConfigError> {
    validate_metric_name(metric, context, ctx)?;
    if !limit.is_finite() {
        return Err(ctx
            .error_at(
                metric,
                "non-finite limit",
                format!("`{context}.{metric}` must be a finite number, got `{limit}`"),
            )
            .with_help("use a finite numeric limit; `inf` and `nan` cannot gate a metric"));
    }
    out.insert(metric.to_string(), limit);
    Ok(())
}

/// Validate a configured metric name against the known metric
/// families so a typo fails at load time with a suggestion instead of
/// silently never firing.
fn validate_metric_name(
    name: &str,
    context: &str,
    ctx: &ErrorContext<'_>,
) -> Result<(), ConfigError> {
    if KNOWN_METRICS.iter().any(|(known, ..)| *known == name) {
        return Ok(());
    }
    // The engine-owned history family is fixed: validate against the
    // enumerated keys so `history.commit_frequncy` is caught here.
    if name.starts_with("history.") || name == "history" {
        if keys::HISTORY_ALL.contains(&name) {
            return Ok(());
        }
        return Err(ctx
            .error_at(
                name,
                "not a history metric",
                format!("unknown history metric `{name}` in [{context}]"),
            )
            .with_help(format!(
                "the fixed `history.*` family is: {}",
                keys::HISTORY_ALL.join(", ")
            )));
    }
    // Language-owned namespaces are extensible and accepted by prefix.
    if name.starts_with("sql.") || name.starts_with("markdown.") {
        return Ok(());
    }
    // Source-code families accept the root and any `<root>.<sub>`
    // member (aggregates like `cognitive.max`, sub-buckets like
    // `nom.functions_max`, spellings like `loc.sloc`).
    if METRIC_FAMILY_ROOTS
        .iter()
        .any(|root| name == *root || name.starts_with(&format!("{root}.")))
    {
        return Ok(());
    }

    let mut candidates: Vec<&str> = KNOWN_METRICS.iter().map(|(known, ..)| *known).collect();
    candidates.extend_from_slice(METRIC_FAMILY_ROOTS);
    candidates.extend_from_slice(keys::HISTORY_ALL);
    let mut help = String::new();
    if let Some(candidate) = closest_candidate(name, &candidates) {
        help.push_str(&format!("did you mean `{candidate}`? "));
    }
    help.push_str(&format!(
        "known families: {}, plus `sql.*`, `markdown.*`, and the fixed `history.*` keys",
        METRIC_FAMILY_ROOTS.join(", ")
    ));
    Err(ctx
        .error_at(
            name,
            "not a known metric",
            format!("unknown metric `{name}` in [{context}]"),
        )
        .with_help(help))
}

/// The candidate within edit distance 2 of `name`, if any (ties break
/// toward the earlier candidate).
fn closest_candidate<'c>(name: &str, candidates: &[&'c str]) -> Option<&'c str> {
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let distance = levenshtein(name, candidate);
        if distance <= 2 && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut previous_diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous_diagonal + usize::from(ca != cb);
            previous_diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(previous_diagonal + 1);
        }
    }
    row[b.len()]
}

/// The threshold violation report: a single diagnostic whose body
/// groups violations per file. The graphical handler renders the body
/// under a `│` gutter with the summary marked `×` and the guidance as
/// `help:`.
#[derive(Debug, miette::Diagnostic)]
#[diagnostic(code(mehen::thresholds))]
struct ThresholdReport {
    message: String,
    #[help]
    help: Option<String>,
}

impl fmt::Display for ThresholdReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl core::error::Error for ThresholdReport {}

/// Render the violation report printed to stderr before exiting 1.
///
/// Sorted by path then metric for determinism, grouped per file; each
/// line names the measured value, the crossed limit, and the config
/// table that set it (`[thresholds]` or `[languages.<lang>]`).
pub fn render_threshold_report(breaches: &mut [ThresholdBreach], config_path: &Path) -> String {
    breaches.sort_by(|a, b| {
        (a.path.as_str(), a.metric.as_str()).cmp(&(b.path.as_str(), b.metric.as_str()))
    });

    let plural = if breaches.len() == 1 { "" } else { "s" };
    let mut message = format!(
        "{} metric threshold violation{plural} (config: {})",
        breaches.len(),
        config_path.display()
    );
    let mut current_path: Option<&str> = None;
    for breach in breaches.iter() {
        if current_path != Some(breach.path.as_str()) {
            current_path = Some(breach.path.as_str());
            message.push_str(&format!("\n\n{}", breach.path));
        }
        let comparison = match breach.polarity {
            Polarity::HigherIsWorse => format!("exceeds max {}", format_limit(breach.limit)),
            Polarity::HigherIsBetter => format!("below min {}", format_limit(breach.limit)),
        };
        let source = match breach.source {
            LimitSource::Global => "[thresholds]".to_string(),
            LimitSource::Language(language) => format!("[languages.{}]", language.canonical()),
        };
        message.push_str(&format!(
            "\n  {} = {} — {comparison}  {source}",
            breach.metric,
            format_limit(breach.value)
        ));
    }
    let report = ThresholdReport {
        message,
        help: Some(
            "raise or remove the limit in the config table shown, or refactor the file below it."
                .to_string(),
        ),
    };
    render_diagnostic(&report)
}

/// Render a configuration error for stderr: source snippet with a
/// caret at the offending key (when available) plus a `help:` line.
pub fn render_config_error(error: &ConfigError) -> String {
    render_diagnostic(error)
}

/// Render any diagnostic through miette's graphical handler. Colors
/// and unicode decorations engage only when stderr is a terminal and
/// `NO_COLOR` is unset, so piped/captured output stays clean.
fn render_diagnostic(diagnostic: &dyn miette::Diagnostic) -> String {
    use std::io::IsTerminal;

    use miette::{GraphicalReportHandler, GraphicalTheme};

    let colors = std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal();
    let theme = if colors {
        GraphicalTheme::unicode()
    } else {
        GraphicalTheme::unicode_nocolor()
    };
    let mut rendered = String::new();
    GraphicalReportHandler::new_themed(theme)
        .render_report(&mut rendered, diagnostic)
        .expect("rendering a diagnostic into a String cannot fail");
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

/// Integer-like values render without decimals, everything else with
/// two (mirrors the top-offenders table formatting).
fn format_limit(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e18 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{SourceSpan, SpaceId, SpaceKind};

    fn parse(text: &str) -> Result<ConfigFile, ConfigError> {
        parse_config(text, Path::new("mehen.toml"))
    }

    /// The full rendered diagnostic (message + snippet + help), as the
    /// CLI would print it with captured (non-terminal) stderr.
    fn rendered(error: &ConfigError) -> String {
        render_config_error(error)
    }

    fn space_with(entries: &[(&str, f64)]) -> MetricSpace {
        let mut space = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        for (key, value) in entries {
            space.metrics.insert(*key, *value);
        }
        space
    }

    #[test]
    fn parses_global_thresholds_with_dotted_and_quoted_keys() {
        let config =
            parse("[thresholds]\ncognitive = 15\nloc.lloc = 500\n\"mi.visual_studio\" = 40.5\n")
                .expect("valid config");
        assert_eq!(config.thresholds.global.get("cognitive"), Some(&15.0));
        assert_eq!(config.thresholds.global.get("loc.lloc"), Some(&500.0));
        assert_eq!(
            config.thresholds.global.get("mi.visual_studio"),
            Some(&40.5)
        );
    }

    #[test]
    fn parses_language_override_with_alias() {
        let config = parse("[languages.py.thresholds]\ncognitive = 10\n").expect("valid config");
        assert_eq!(config.thresholds.per_language.len(), 1);
        let (language, thresholds) = &config.thresholds.per_language[0];
        assert_eq!(*language, Language::Python);
        assert_eq!(thresholds.get("cognitive"), Some(&10.0));
    }

    #[test]
    fn empty_and_missing_tables_yield_empty_policy() {
        assert!(parse("").expect("empty config").thresholds.is_empty());
        assert!(
            parse("[thresholds]\n")
                .expect("empty thresholds")
                .thresholds
                .is_empty()
        );
    }

    #[test]
    fn rejects_unknown_top_level_key_with_suggestion() {
        let err = parse("[threshold]\ncognitive = 15\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown top-level key `threshold`")
        );
        let rendered = rendered(&err);
        assert!(
            rendered.contains("did you mean `thresholds`?"),
            "{rendered}"
        );
        // The diagnostic points into the TOML source at the bad key.
        assert!(rendered.contains("[threshold]"), "{rendered}");
        assert!(rendered.contains("mehen.toml"), "{rendered}");
    }

    #[test]
    fn rejects_unknown_metric_with_suggestion() {
        let err = parse("[thresholds]\ncognitve = 15\n").unwrap_err();
        assert!(err.to_string().contains("unknown metric `cognitve`"));
        let rendered = rendered(&err);
        assert!(rendered.contains("did you mean `cognitive`?"), "{rendered}");
        assert!(rendered.contains("cognitve = 15"), "{rendered}");
    }

    #[test]
    fn rejects_mistyped_history_metric_listing_family() {
        let err = parse("[thresholds]\n\"history.commit_frequncy\" = 5\n").unwrap_err();
        assert!(err.to_string().contains("unknown history metric"));
        assert!(
            rendered(&err).contains("history.commit_frequency"),
            "help must list the real family keys"
        );
    }

    #[test]
    fn rejects_non_numeric_limit() {
        let err = parse("[thresholds]\ncognitive = \"high\"\n").unwrap_err();
        assert!(err.to_string().contains("must be a number"));
        assert!(
            rendered(&err).contains("write a plain number"),
            "help must show the expected shape"
        );
    }

    #[test]
    fn rejects_non_finite_limit() {
        let err = parse("[thresholds]\ncognitive = inf\n").unwrap_err();
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn rejects_invalid_toml_syntax_with_span() {
        let err = parse("[thresholds\ncognitive = 5\n").unwrap_err();
        assert!(err.to_string().contains("invalid TOML"));
        assert!(
            rendered(&err).contains("mehen.toml"),
            "syntax errors must name the file"
        );
    }

    #[test]
    fn rejects_unknown_language() {
        let err = parse("[languages.klingon.thresholds]\ncognitive = 5\n").unwrap_err();
        assert!(err.to_string().contains("unknown language `klingon`"));
        assert!(
            rendered(&err).contains("aliases like `py`, `ts`, `rb` are accepted"),
            "help must explain accepted identifiers"
        );
    }

    #[test]
    fn rejects_duplicate_language_via_alias() {
        let err = parse(
            "[languages.py.thresholds]\ncognitive = 5\n[languages.python.thresholds]\ncognitive = 6\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn rejects_unknown_key_inside_language_section() {
        let err = parse("[languages.python.limits]\ncognitive = 5\n").unwrap_err();
        assert!(err.to_string().contains("unknown key `limits`"));
        assert!(
            rendered(&err).contains("[languages.python.thresholds]"),
            "help must show the expected table name"
        );
    }

    #[test]
    fn accepts_namespaced_and_family_member_metrics() {
        let config = parse(
            "[thresholds]\n\"sql.change_risk_score\" = 3\n\"history.hotspot\" = 100\nnargs = 6\n\"cognitive.max\" = 20\n",
        )
        .expect("valid config");
        assert_eq!(config.thresholds.global.len(), 4);
    }

    #[test]
    fn language_override_wins_over_global() {
        let config =
            parse("[thresholds]\ncognitive = 15\n[languages.python.thresholds]\ncognitive = 10\n")
                .expect("valid config");
        let space = space_with(&[("cognitive.sum", 12.0)]);
        // Python resolves the override limit (10) → 12 violates.
        let breaches = config
            .thresholds
            .evaluate("a.py", Language::Python, &space, None);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].limit, 10.0);
        assert_eq!(breaches[0].source, LimitSource::Language(Language::Python));
        // Rust keeps the global limit (15) → 12 passes.
        let breaches = config
            .thresholds
            .evaluate("a.rs", Language::Rust, &space, None);
        assert!(breaches.is_empty());
    }

    #[test]
    fn evaluate_skips_metrics_the_space_does_not_publish() {
        // `mi.visual_studio` is higher-is-better: a fabricated 0.0 for
        // the missing key would fire a false violation.
        let config = parse("[thresholds]\n\"mi.visual_studio\" = 40\n").expect("valid config");
        let space = space_with(&[("cognitive.sum", 5.0)]);
        let breaches = config
            .thresholds
            .evaluate("a.py", Language::Python, &space, None);
        assert!(breaches.is_empty());
    }

    #[test]
    fn evaluate_flags_below_minimum_for_higher_is_better() {
        let config = parse("[thresholds]\n\"mi.visual_studio\" = 40\n").expect("valid config");
        let space = space_with(&[("mi.visual_studio", 12.4)]);
        let breaches = config
            .thresholds
            .evaluate("a.py", Language::Python, &space, None);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].polarity, Polarity::HigherIsBetter);
    }

    #[test]
    fn evaluate_respects_output_metric_filter() {
        let config =
            parse("[thresholds]\ncognitive = 5\n\"loc.lloc\" = 10\n").expect("valid config");
        let space = space_with(&[("cognitive.sum", 50.0), ("loc.lloc", 50.0)]);
        let breaches =
            config
                .thresholds
                .evaluate("a.py", Language::Python, &space, Some(&["cognitive"]));
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].metric, "cognitive");
    }

    #[test]
    fn evaluate_exact_limit_is_not_a_violation() {
        let config = parse("[thresholds]\ncognitive = 15\n").expect("valid config");
        let space = space_with(&[("cognitive.sum", 15.0)]);
        assert!(
            config
                .thresholds
                .evaluate("a.py", Language::Python, &space, None)
                .is_empty()
        );
    }

    #[test]
    fn lookup_falls_back_to_underscore_sub_bucket_spelling() {
        let space = space_with(&[("nom.functions_max", 9.0)]);
        assert_eq!(lookup_metric(&space, "nom.functions.max"), Some(9.0));
        assert_eq!(lookup_metric(&space, "nom.functions.min"), None);
    }

    #[test]
    fn report_orders_violations_and_names_config_source() {
        let mut breaches = vec![
            ThresholdBreach {
                path: "src/util.rs".to_string(),
                metric: "mi.visual_studio".to_string(),
                value: 12.4,
                limit: 40.0,
                polarity: Polarity::HigherIsBetter,
                source: LimitSource::Global,
            },
            ThresholdBreach {
                path: "src/app/core.py".to_string(),
                metric: "loc.lloc".to_string(),
                value: 640.0,
                limit: 500.0,
                polarity: Polarity::HigherIsWorse,
                source: LimitSource::Global,
            },
            ThresholdBreach {
                path: "src/app/core.py".to_string(),
                metric: "cognitive".to_string(),
                value: 23.0,
                limit: 15.0,
                polarity: Polarity::HigherIsWorse,
                source: LimitSource::Language(Language::Python),
            },
        ];
        let report = render_threshold_report(&mut breaches, Path::new("/repo/mehen.toml"));
        assert!(
            report.contains("3 metric threshold violations (config: /repo/mehen.toml)"),
            "{report}"
        );
        let cognitive = "cognitive = 23 — exceeds max 15  [languages.python]";
        let lloc = "loc.lloc = 640 — exceeds max 500  [thresholds]";
        let mi = "mi.visual_studio = 12.40 — below min 40  [thresholds]";
        for line in [cognitive, lloc, mi, "src/app/core.py", "src/util.rs"] {
            assert!(report.contains(line), "missing `{line}` in:\n{report}");
        }
        // Deterministic ordering: path, then metric.
        let position = |needle: &str| report.find(needle).expect("line present");
        assert!(position("src/app/core.py") < position(cognitive));
        assert!(position(cognitive) < position(lloc));
        assert!(position(lloc) < position("src/util.rs"));
        assert!(position("src/util.rs") < position(mi));
        assert!(report.contains("help:"), "{report}");
    }

    #[test]
    fn single_breach_report_uses_singular_wording() {
        let mut breaches = vec![ThresholdBreach {
            path: "a.py".to_string(),
            metric: "cognitive".to_string(),
            value: 3.0,
            limit: 1.0,
            polarity: Polarity::HigherIsWorse,
            source: LimitSource::Global,
        }];
        let report = render_threshold_report(&mut breaches, Path::new("mehen.toml"));
        assert!(
            report.contains("1 metric threshold violation (config: mehen.toml)"),
            "{report}"
        );
    }

    #[test]
    fn discovery_walks_up_and_prefers_visible_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).expect("mkdirs");
        std::fs::write(
            dir.path().join("mehen.toml"),
            "[thresholds]\ncognitive = 1\n",
        )
        .expect("write config");
        let found = discover_config_path(&nested).expect("config discovered");
        assert_eq!(found, dir.path().join("mehen.toml"));

        // A closer `.mehen.toml` shadows the ancestor's `mehen.toml`.
        std::fs::write(nested.join(".mehen.toml"), "[thresholds]\ncognitive = 2\n")
            .expect("write hidden config");
        let found = discover_config_path(&nested).expect("config discovered");
        assert_eq!(found, nested.join(".mehen.toml"));
    }
}
