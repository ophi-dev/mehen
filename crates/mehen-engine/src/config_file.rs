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
//! columns. Spellings are canonicalized before matching, so a
//! `cognitive.sum` threshold gates a `cognitive` column (both read the
//! same published key). A metric the analyzed file does not publish
//! (e.g. a `sql.*` threshold against a Python file) is skipped — a
//! missing measurement is never treated as `0`.
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
//! the enclosing git repository root (the upper boundary — a config
//! above it cannot belong to the project). Outside a repository only
//! the current directory is checked. `mehen.toml` is preferred over
//! `.mehen.toml`; `--config <PATH>` bypasses discovery.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use miette::{LabeledSpan, NamedSource};

use mehen_core::{Language, MetricKey, MetricSpace, Polarity, keys};

use crate::metric_selector::{is_higher_is_better_metric, metric_set_key_for};

/// Recognized configuration file names, in preference order.
pub(crate) const CONFIG_FILE_NAMES: &[&str] = &["mehen.toml", ".mehen.toml"];

/// Every metric key the shared source-code publishers
/// (`mehen-metrics::state`) emit onto a root `MetricSpace`, in the
/// exact published spelling. Configured thresholds must resolve to one
/// of these (or to a `history.*` / `sql.*` / `markdown.*` key) — a
/// name no analyzer can ever publish is rejected at load time, because
/// a threshold that can never read a value is a gate that can never
/// fire.
///
/// Kept in sync with the publishers by
/// `validator_accepts_every_key_real_analyzers_publish` below, which
/// runs real analyses and asserts every published root key validates.
const PUBLISHED_METRIC_KEYS: &[&str] = &[
    // cyclomatic
    "cyclomatic",
    "cyclomatic.sum",
    "cyclomatic.min",
    "cyclomatic.max",
    "cyclomatic.avg",
    // cognitive
    "cognitive",
    "cognitive.sum",
    "cognitive.average",
    "cognitive.min",
    "cognitive.max",
    // loc family — the bare `loc` key (a mirror of `loc.sloc`) is
    // published but deliberately NOT configurable: the GitHub Action
    // ecosystem treats bare `loc` as a legacy alias for `loc.lloc`,
    // so accepting it would gate a different measurement than the
    // name suggests. Use the precise `loc.*` members instead.
    "loc.lloc",
    "loc.sloc",
    "loc.ploc",
    "loc.cloc",
    "loc.blank",
    "loc.lloc.min",
    "loc.lloc.max",
    "loc.lloc.avg",
    "loc.sloc.min",
    "loc.sloc.max",
    "loc.sloc.avg",
    "loc.ploc.min",
    "loc.ploc.max",
    "loc.ploc.avg",
    "loc.cloc.min",
    "loc.cloc.max",
    "loc.cloc.avg",
    "loc.blank.min",
    "loc.blank.max",
    "loc.blank.avg",
    // halstead
    "halstead.volume",
    "halstead.difficulty",
    "halstead.effort",
    "halstead.vocabulary",
    "halstead.length",
    "halstead.n1",
    "halstead.N1",
    "halstead.n2",
    "halstead.N2",
    "halstead.estimated_program_length",
    "halstead.purity_ratio",
    "halstead.level",
    "halstead.time",
    "halstead.bugs",
    // maintainability index (no bare `mi` is published)
    "mi.visual_studio",
    "mi.original",
    "mi.sei",
    // abc
    "abc",
    "abc.assignments",
    "abc.branches",
    "abc.conditions",
    "abc.assignments_average",
    "abc.branches_average",
    "abc.conditions_average",
    "abc.assignments_min",
    "abc.assignments_max",
    "abc.branches_min",
    "abc.branches_max",
    "abc.conditions_min",
    "abc.conditions_max",
    // nargs
    "nargs",
    "nargs.total_functions",
    "nargs.total_closures",
    "nargs.average_functions",
    "nargs.average_closures",
    "nargs.average",
    "nargs.functions_min",
    "nargs.functions_max",
    "nargs.closures_min",
    "nargs.closures_max",
    // nom — the bare `nom` key (functions + closures total) is
    // published but deliberately NOT configurable: the GitHub Action
    // ecosystem treats bare `nom` as a legacy alias for
    // `nom.functions`, so accepting it would gate a different
    // measurement than the name suggests. Use the precise members.
    "nom.functions",
    "nom.closures",
    "nom.functions_average",
    "nom.closures_average",
    "nom.average",
    "nom.functions_min",
    "nom.functions_max",
    "nom.closures_min",
    "nom.closures_max",
    // nexit
    "nexit",
    "nexit.sum",
    "nexit.average",
    "nexit.min",
    "nexit.max",
    // npa
    "npa",
    "npa.classes",
    "npa.interfaces",
    "npa.class_attributes",
    "npa.interface_attributes",
    "npa.classes_average",
    "npa.interfaces_average",
    "npa.total_attributes",
    "npa.average",
    // npm
    "npm",
    "npm.classes",
    "npm.interfaces",
    "npm.class_methods",
    "npm.interface_methods",
    "npm.classes_average",
    "npm.interfaces_average",
    "npm.total_methods",
    "npm.average",
    // wmc
    "wmc",
    "wmc.classes",
    "wmc.interfaces",
];

/// Why a metric name failed to resolve to a published key.
pub(crate) enum ResolveError {
    /// A `history.*` name outside the fixed family.
    UnknownHistory,
    /// A `sql.*` / `markdown.*` name the owning analyzer never
    /// publishes.
    UnknownNamespaced,
    /// A namespace this build cannot analyze (`sql.*` without the
    /// `lang-sql` feature) — the gate could never fire.
    #[cfg(not(feature = "lang-sql"))]
    UnavailableNamespace,
    /// Everything else the resolver cannot map to a published key.
    Unknown,
}

/// Resolve a configured (or selected) metric name to the canonical
/// key the analyzers actually publish.
///
/// - `history.*` names must be in the fixed [`keys::HISTORY_ALL`]
///   family and resolve to themselves.
/// - `sql.*` / `markdown.*` names resolve to themselves: the
///   language-owned namespaces are extensible, so their members
///   cannot be enumerated here.
/// - Everything else maps through [`metric_set_key_for`] (`cognitive`
///   → `cognitive.sum`) and must land on a [`PUBLISHED_METRIC_KEYS`]
///   entry, directly or via an aggregate-spelling alias: the
///   underscore sub-bucket form (`nom.functions.max` →
///   `nom.functions_max`) and the `avg` ↔ `average` pair
///   (`nexit.avg` → `nexit.average`).
///
/// Because evaluation reads exactly the canonical key, "accepted at
/// load time" and "readable at evaluation time" agree by
/// construction: a name this function accepts can fire, a name it
/// rejects never could. `parse_metric_selectors` resolves `--metric`
/// names through the same function, so every key the config can gate
/// is also selectable as a diff/top-offenders column.
pub(crate) fn canonical_metric_key(name: &str) -> Result<String, ResolveError> {
    if name == "history" || name.starts_with("history.") {
        return if keys::HISTORY_ALL.contains(&name) {
            Ok(name.to_string())
        } else {
            Err(ResolveError::UnknownHistory)
        };
    }
    if name.starts_with("sql.") {
        // The SQL analyzer owns its namespace: validate against its
        // published catalogue (fixed keys + enum-backed dynamic
        // families) so a typo can never become a gate that cannot
        // fire. A build without the SQL analyzer cannot analyze SQL
        // files at all, so any `sql.*` threshold would be a dead gate
        // there — rejected rather than accepted verbatim.
        #[cfg(feature = "lang-sql")]
        {
            return if mehen_sql::is_published_metric_key(name) {
                Ok(name.to_string())
            } else {
                Err(ResolveError::UnknownNamespaced)
            };
        }
        #[cfg(not(feature = "lang-sql"))]
        {
            return Err(ResolveError::UnavailableNamespace);
        }
    }
    if name.starts_with("markdown.") {
        return if mehen_markdown::is_published_metric_key(name) {
            Ok(name.to_string())
        } else {
            Err(ResolveError::UnknownNamespaced)
        };
    }
    let key = metric_set_key_for(name);
    if PUBLISHED_METRIC_KEYS.contains(&key) {
        return Ok(key.to_string());
    }
    if let Some((base, suffix)) = key.rsplit_once('.')
        && matches!(suffix, "min" | "max" | "avg" | "average" | "sum")
    {
        let mut candidates = vec![format!("{base}_{suffix}")];
        let alternate = match suffix {
            "avg" => Some("average"),
            "average" => Some("avg"),
            _ => None,
        };
        if let Some(alternate) = alternate {
            candidates.push(format!("{base}.{alternate}"));
            candidates.push(format!("{base}_{alternate}"));
        }
        if let Some(hit) = candidates
            .into_iter()
            .find(|candidate| PUBLISHED_METRIC_KEYS.contains(&candidate.as_str()))
        {
            return Ok(hit);
        }
    }
    Err(ResolveError::Unknown)
}

/// The canonical key for filter matching, falling back to the raw
/// name for anything unresolvable (a selector the engine accepted is
/// never rejected here — worst case it matches by its own spelling).
fn canonical_for_match(name: &str) -> String {
    canonical_metric_key(name).unwrap_or_else(|_| name.to_string())
}

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
/// source. Spans come straight from the span-preserving TOML parse
/// tree ([`toml::de::DeTable`]), so every label points at the exact
/// occurrence — never at a same-spelled key elsewhere in the file.
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

    /// An error labeled at an exact byte range from the parse tree.
    fn error_at(
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

/// One crossed threshold: the measured value, the configured limit,
/// and enough context to render an actionable report line. Serialized
/// verbatim into `mehen diff --output-format json` under
/// `threshold_violations` so machine consumers (e.g. the GitHub
/// Action) can distinguish a quality-gate exit from an analysis
/// failure.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ThresholdBreach {
    /// Display path of the offending file.
    pub path: String,
    /// The metric's dotted key path within [`ThresholdBreach::source_table`]
    /// (e.g. `loc.lloc`). Derived from the parse tree, so every TOML
    /// spelling — dotted keys, nested headers, inline tables, quoting,
    /// escapes — reports the same path.
    pub metric: String,
    /// Measured value at the evaluated (head) side.
    pub value: f64,
    /// Configured limit.
    pub limit: f64,
    /// Whether the limit is a maximum (`HigherIsWorse`) or a minimum
    /// (`HigherIsBetter`).
    pub polarity: Polarity,
    /// Key path of the threshold table that set the limit —
    /// `thresholds`, or `languages.py.thresholds` with the language
    /// alias preserved (it is a real parsed key, not a spelling
    /// variant). Rendered as `set by <path>` in the report; combined
    /// with [`ThresholdBreach::metric`] it forms the entry's full
    /// configuration key path.
    pub source_table: String,
}

/// One configured limit: the dotted metric path and the table it sits
/// in (both parse-tree data, for reporting) plus the numeric limit.
/// Keyed by canonical metric key in the policy tables.
#[derive(Debug, Clone)]
struct ThresholdEntry {
    /// Dotted metric path within `table` (e.g. `loc.lloc`) — every
    /// TOML spelling of the same entry normalizes to this path.
    spelling: String,
    /// Key path of the owning threshold table (`thresholds`,
    /// `languages.py.thresholds` with the alias preserved).
    table: String,
    limit: f64,
}

/// Per-metric limits: a global table plus per-language overrides.
/// Tables are keyed by *canonical* metric key (the published
/// spelling), so `cognitive` and `cognitive.sum` are one logical
/// threshold everywhere: duplicate detection, override resolution,
/// and output-column matching.
#[derive(Debug, Clone, Default)]
pub struct ThresholdPolicy {
    global: BTreeMap<String, ThresholdEntry>,
    /// Sorted by canonical language id for deterministic iteration.
    per_language: Vec<(Language, BTreeMap<String, ThresholdEntry>)>,
}

impl ThresholdPolicy {
    /// True when no thresholds are configured at all.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.per_language.iter().all(|(_, t)| t.is_empty())
    }

    /// The effective `canonical metric → entry` map for one language:
    /// the language override wins over the global limit, metric by
    /// metric.
    fn resolved_for(&self, language: Language) -> BTreeMap<&str, &ThresholdEntry> {
        let mut resolved: BTreeMap<&str, &ThresholdEntry> = self
            .global
            .iter()
            .map(|(canonical, entry)| (canonical.as_str(), entry))
            .collect();
        if let Some((_, overrides)) = self.per_language.iter().find(|(l, _)| *l == language) {
            for (canonical, entry) in overrides {
                resolved.insert(canonical.as_str(), entry);
            }
        }
        resolved
    }

    /// Evaluate the policy against one file's root metric set.
    ///
    /// `only_metrics` restricts evaluation to the metrics a command
    /// actually reports (`mehen diff` / `mehen top-offenders` pass
    /// their selected column names; `mehen metrics` passes `None`
    /// because its report carries the full metric set). Matching is
    /// canonical on both sides, so a configured `cognitive.sum` gates
    /// a selected `cognitive` column — they read the same published
    /// key.
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
        let selected: Option<Vec<String>> =
            only_metrics.map(|names| names.iter().map(|name| canonical_for_match(name)).collect());
        let mut breaches = Vec::new();
        for (canonical, entry) in self.resolved_for(language) {
            if let Some(selected) = &selected
                && !selected.iter().any(|name| name == canonical)
            {
                continue;
            }
            let Some(value) = root
                .metrics
                .get(&MetricKey::new(canonical))
                .map(|v| v.as_f64())
            else {
                continue;
            };
            // Undefined measurements publish as NaN (e.g. interface
            // averages over a zero interface count): NaN compares
            // false under every polarity, which would silently pass
            // the gate — skip them as unmeasurable instead.
            if !value.is_finite() {
                continue;
            }
            if !value_is_measurable(root, canonical) {
                continue;
            }
            let polarity = polarity_for_metric(canonical);
            let violated = match polarity {
                Polarity::HigherIsWorse => value > entry.limit,
                Polarity::HigherIsBetter => value < entry.limit,
            };
            if violated {
                breaches.push(ThresholdBreach {
                    path: path.to_string(),
                    metric: entry.spelling.clone(),
                    value,
                    limit: entry.limit,
                    polarity,
                    source_table: entry.table.clone(),
                });
            }
        }
        breaches
    }
}

/// Whether a present metric value is a real measurement for this
/// space, as opposed to a published N/A sentinel.
///
/// - `sql.modularity_health` emits `0.0` when the file has no CTEs
///   (the score is only meaningful for CTE-bearing files, per
///   `mehen-sql::composite`); applicability is read from the
///   co-published `sql.cte.count`.
/// - `halstead.level` (`L = 1/D`) emits `0.0` when the difficulty is
///   zero — an empty or token-free file where the ratio is undefined;
///   applicability is read from the co-published
///   `halstead.difficulty`.
///
/// Gating a sentinel under a higher-is-better minimum would fail
/// every inapplicable file, while a genuine low score must keep
/// gating.
fn value_is_measurable(root: &MetricSpace, canonical: &str) -> bool {
    let applicability_key = match canonical {
        "sql.modularity_health" => "sql.cte.count",
        "halstead.level" => "halstead.difficulty",
        _ => return true,
    };
    root.metrics
        .get(&MetricKey::new(applicability_key))
        .is_none_or(|gate| gate.as_f64() > 0.0)
}

/// Whether a configured limit is a minimum (higher-is-better metric)
/// or a maximum (everything else). Shares the ranking/diff polarity
/// source of truth ([`is_higher_is_better_metric`]): `mi.*`, the
/// Halstead program level, and the enumerated namespaced quality
/// scores are higher-is-better. Applied to canonical keys.
fn polarity_for_metric(name: &str) -> Polarity {
    if is_higher_is_better_metric(name) {
        Polarity::HigherIsBetter
    } else {
        Polarity::HigherIsWorse
    }
}

/// Load the configuration for this invocation.
///
/// With `explicit` (the `--config` flag) the file must exist and
/// parse. Otherwise the configuration is discovered by walking from
/// the current working directory up to the enclosing git repository
/// root (or checking only the working directory outside a
/// repository); no file found means no configuration (`Ok(None)`),
/// which leaves every command's behavior unchanged.
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

/// Walk from `start` up to the enclosing git repository's work dir
/// (inclusive), returning the first configuration file found.
/// `mehen.toml` wins over `.mehen.toml` within the same directory
/// (with a warning when both exist).
///
/// The repository root is the upper boundary: a config above it can
/// never belong to this project, and an unbounded walk to the
/// filesystem root would probe every ancestor and could pick up an
/// unrelated file (e.g. a stray `~/mehen.toml`). Outside a repository
/// only `start` itself is checked.
fn discover_config_path(start: &Path) -> Option<PathBuf> {
    // Resolve symlinks so the boundary comparison is exact — macOS
    // tempdirs live behind `/var -> /private/var`, and `gix` reports
    // the work dir as discovered, which may differ in spelling from
    // `start`.
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let boundary = repository_boundary(&start).unwrap_or_else(|| start.clone());
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
        if dir == boundary {
            break;
        }
    }
    None
}

/// The enclosing git repository's work dir, canonicalized — the
/// inclusive upper boundary for config discovery. `None` when `start`
/// is not inside a repository (or the repository is bare or
/// unreadable), which limits discovery to `start` itself.
///
/// Uses `gix::discover` directly rather than
/// `mehen_git::open_repo_at`: the latter rejects shallow clones for
/// the history features, but a shallow CI checkout still has a
/// well-defined configuration boundary.
fn repository_boundary(start: &Path) -> Option<PathBuf> {
    let repo = gix::discover(start).ok()?;
    let workdir = repo.workdir()?.to_path_buf();
    std::fs::canonicalize(&workdir).ok()
}

/// Parse and validate configuration text. `path` is used for the
/// diagnostic source name and report messages.
///
/// Parsing goes through the span-preserving [`toml::de::DeTable`]
/// tree, so every diagnostic label and every breach's `[table]`
/// attribution derives from the exact source location of the entry —
/// not from a text search that a same-spelled key elsewhere in the
/// file could defeat.
fn parse_config(text: &str, path: &Path) -> Result<ConfigFile, ConfigError> {
    let ctx = ErrorContext { text, path };
    let root = match toml::de::DeTable::parse(text) {
        Ok(root) => root,
        Err(e) => {
            let message = format!("invalid TOML: {}", e.message());
            let error = match e.span() {
                Some(span) => ctx.error_at(span, "syntax error here", message),
                None => ctx.error(message),
            };
            return Err(error.with_help("fix the TOML syntax; see https://toml.io for the format"));
        }
    };

    let mut global: BTreeMap<String, ThresholdEntry> = BTreeMap::new();
    let mut per_language: Vec<(Language, BTreeMap<String, ThresholdEntry>)> = Vec::new();

    for (key, value) in root.get_ref() {
        let key_str: &str = key.get_ref().as_ref();
        match key_str {
            "thresholds" => {
                let table = expect_table(value, "thresholds", &ctx)?;
                collect_thresholds(table, "thresholds", None, &mut global, &ctx)?;
            }
            "languages" => {
                let table = expect_table(value, "languages", &ctx)?;
                for (lang_key, lang_value) in table {
                    let lang_str: &str = lang_key.get_ref().as_ref();
                    // Path segments render TOML-quoted when needed:
                    // the accepted `c#` alias is not a valid bare key.
                    let lang_segment = toml_key_segment(lang_str);
                    let language = lang_str.parse::<Language>().map_err(|_| {
                        ctx.error_at(
                            lang_key.span(),
                            "not a recognized language",
                            format!("unknown language `{lang_str}` in [languages]"),
                        )
                        .with_help(
                            "use a language identifier such as python, typescript, rust, go, … \
                             (aliases like `py`, `ts`, `rb` are accepted)",
                        )
                    })?;
                    if per_language.iter().any(|(l, _)| *l == language) {
                        return Err(ctx
                            .error_at(
                                lang_key.span(),
                                "same language configured twice",
                                format!(
                                    "duplicate [languages.{lang_segment}] section — another key \
                                     already configures `{}`",
                                    language.canonical()
                                ),
                            )
                            .with_help(
                                "language aliases refer to the same language; keep one section \
                                 per language",
                            ));
                    }
                    let lang_ctx = format!("languages.{lang_segment}");
                    let lang_table = expect_table(lang_value, &lang_ctx, &ctx)?;
                    let mut thresholds: BTreeMap<String, ThresholdEntry> = BTreeMap::new();
                    for (sub_key, sub_value) in lang_table {
                        match sub_key.get_ref().as_ref() {
                            "thresholds" => {
                                let threshold_ctx = format!("{lang_ctx}.thresholds");
                                let table = expect_table(sub_value, &threshold_ctx, &ctx)?;
                                collect_thresholds(
                                    table,
                                    &threshold_ctx,
                                    Some(language),
                                    &mut thresholds,
                                    &ctx,
                                )?;
                            }
                            other => {
                                return Err(ctx
                                    .error_at(
                                        sub_key.span(),
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
                        key.span(),
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

fn expect_table<'v, 'i>(
    value: &'v toml::Spanned<toml::de::DeValue<'i>>,
    context: &str,
    ctx: &ErrorContext<'_>,
) -> Result<&'v toml::de::DeTable<'i>, ConfigError> {
    match value.get_ref() {
        toml::de::DeValue::Table(table) => Ok(table),
        other => Err(ctx.error_at(
            value.span(),
            "not a table",
            format!(
                "`{context}` must be a table (as in [{context}]), got {}",
                other.type_str()
            ),
        )),
    }
}

/// Flatten one thresholds table into canonical `metric → limit`
/// entries.
///
/// TOML turns a dotted key (`loc.lloc = 500`) into nested tables, so
/// nested tables are folded back into dotted metric names — the
/// quoted spelling (`"loc.lloc" = 500`), the dotted spelling, a
/// nested header (`[thresholds.loc]` with `lloc = 500`), and an
/// inline table (`loc = { lloc = 500 }`) are all the same entry.
/// Because equivalence extends to canonical aliases (`cognitive` vs
/// `cognitive.sum`), two spellings of one logical metric in the same
/// table are rejected as a duplicate instead of one silently
/// overwriting the other.
fn collect_thresholds(
    table: &toml::de::DeTable<'_>,
    context: &str,
    language: Option<Language>,
    out: &mut BTreeMap<String, ThresholdEntry>,
    ctx: &ErrorContext<'_>,
) -> Result<(), ConfigError> {
    fn walk(
        table: &toml::de::DeTable<'_>,
        prefix: &str,
        context: &str,
        language: Option<Language>,
        out: &mut BTreeMap<String, ThresholdEntry>,
        ctx: &ErrorContext<'_>,
    ) -> Result<(), ConfigError> {
        for (key, value) in table {
            let key_str: &str = key.get_ref().as_ref();
            let metric = if prefix.is_empty() {
                key_str.to_string()
            } else {
                format!("{prefix}.{key_str}")
            };
            // Attribution is pure parse-tree data: the threshold
            // table's key path (`context`, language aliases
            // preserved) plus the dotted metric path within it. TOML
            // spellings — dotted keys, nested headers, inline tables,
            // quoting, escapes — all normalize to the same paths, so
            // the report never claims a literal spelling and cannot
            // point at anything that does not exist semantically.
            match value.get_ref() {
                toml::de::DeValue::Integer(i) => {
                    // `as_str` keeps the lexical spelling: strip legal
                    // digit separators (`1_000`) and the radix prefix
                    // of hexadecimal/octal/binary spellings (`0x10`)
                    // before conversion — `from_str_radix` accepts
                    // bare digits only.
                    let raw = i.as_str().replace('_', "");
                    let digits = raw
                        .strip_prefix("0x")
                        .or_else(|| raw.strip_prefix("0o"))
                        .or_else(|| raw.strip_prefix("0b"))
                        .unwrap_or(&raw);
                    let limit = i64::from_str_radix(digits, i.radix())
                        .map(|v| v as f64)
                        .unwrap_or(f64::NAN);
                    insert_threshold(&metric, key, limit, context, language, out, ctx)?;
                }
                toml::de::DeValue::Float(f) => {
                    let limit = f
                        .as_str()
                        .replace('_', "")
                        .parse::<f64>()
                        .unwrap_or(f64::NAN);
                    insert_threshold(&metric, key, limit, context, language, out, ctx)?;
                }
                toml::de::DeValue::Table(nested) => {
                    walk(nested, &metric, context, language, out, ctx)?;
                }
                other => {
                    return Err(ctx
                        .error_at(
                            value.span(),
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

    walk(table, "", context, language, out, ctx)
}

/// A key segment as it must be written in a TOML path: bare when the
/// characters allow it, quoted-and-escaped otherwise. The accepted
/// language alias `c#` cannot be a bare key (`#` starts a comment), so
/// a path like `languages."c#".thresholds` must quote it or the
/// reported configuration path could not identify the table.
fn toml_key_segment(segment: &str) -> String {
    let bare = !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        segment.to_string()
    } else {
        format!("\"{}\"", segment.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Every language mehen can identify, for the "can any enabled
/// analyzer publish this key?" reachability check on global
/// thresholds.
const ALL_LANGUAGES: &[Language] = &[
    Language::Python,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Jsx,
    Language::Php,
    Language::Ruby,
    Language::Rust,
    Language::Go,
    Language::Kotlin,
    Language::Java,
    Language::CSharp,
    Language::PowerShell,
    Language::C,
    Language::Markdown,
    Language::Sql,
];

/// Whether files of `language` can ever publish `canonical` onto their
/// root metric set. The engine-injected `history.*` family applies to
/// every language (it needs no analyzer); `sql.*` and `markdown.*`
/// belong to their owning analyzers (which publish nothing else); the
/// class-shape families (`npa`, `npm`, `wmc`) publish only when a
/// class-like construct exists — the C and Go grammars have none; and
/// any static key additionally requires the language's analyzer to be
/// compiled into this build.
fn language_can_publish(language: Language, canonical: &str) -> bool {
    if canonical.starts_with("history.") {
        // Git-only history keys need no analyzer; the static-dependent
        // composites (`history.hotspot`, `history.churn.relative`)
        // read analyzer inputs and are omitted when static analysis is
        // unavailable — exactly what `selector_available` encodes.
        return crate::history_metrics::selector_available(canonical, false, true)
            || crate::AnalyzerRegistry::default_set().has_analyzer_for(language);
    }
    if !crate::AnalyzerRegistry::default_set().has_analyzer_for(language) {
        return false;
    }
    match language {
        Language::Sql => canonical.starts_with("sql."),
        Language::Markdown => canonical.starts_with("markdown."),
        _ => {
            if canonical.starts_with("sql.") || canonical.starts_with("markdown.") {
                return false;
            }
            if is_class_family_key(canonical) && matches!(language, Language::C | Language::Go) {
                return false;
            }
            // The interface-scoped members of the class families
            // measure interface-like spaces (interfaces, traits);
            // Python, Ruby, and PowerShell walkers never open one, so
            // those overrides could never fire.
            if is_interface_scoped_key(canonical)
                && matches!(
                    language,
                    Language::Python | Language::Ruby | Language::PowerShell
                )
            {
                return false;
            }
            true
        }
    }
}

/// The class-shape metric families, published only for files with
/// class-like constructs (classes, interfaces, traits, impls).
fn is_class_family_key(canonical: &str) -> bool {
    ["npa", "npm", "wmc"]
        .iter()
        .any(|family| canonical == *family || canonical.starts_with(&format!("{family}.")))
}

/// The interface-scoped members of the class families
/// (`npa.interfaces`, `npm.interface_methods`, `wmc.interfaces`, …),
/// which measure `SpaceKind::Interface` / `SpaceKind::Trait` spaces
/// exclusively.
fn is_interface_scoped_key(canonical: &str) -> bool {
    let Some((_, member)) = canonical.split_once('.') else {
        return false;
    };
    is_class_family_key(canonical) && member.starts_with("interface")
}

fn insert_threshold(
    metric: &str,
    key: &toml::Spanned<toml::de::DeString<'_>>,
    limit: f64,
    context: &str,
    language: Option<Language>,
    out: &mut BTreeMap<String, ThresholdEntry>,
    ctx: &ErrorContext<'_>,
) -> Result<(), ConfigError> {
    let canonical = validate_metric_name(metric, key.span(), context, ctx)?;
    // A global threshold no enabled analyzer can ever publish — e.g.
    // `wmc` in a build whose only compiled grammars have no class-like
    // constructs — would be a gate that can never fire. (`history.*`
    // passes: it needs no analyzer.)
    if language.is_none()
        && !ALL_LANGUAGES
            .iter()
            .any(|candidate| language_can_publish(*candidate, &canonical))
    {
        return Err(ctx
            .error_at(
                key.span(),
                "unreachable in this build",
                format!(
                    "`{metric}` in [{context}] can never fire: no analyzer compiled into \
                     this build publishes it"
                ),
            )
            .with_help(
                "enable the owning language feature (or use a metric one of the compiled \
                 analyzers publishes)",
            ));
    }
    // A language override naming a metric its files can never publish
    // (`sql.*` under [languages.python], `cognitive` under
    // [languages.sql]) would be a gate that can never fire.
    if let Some(language) = language
        && !language_can_publish(language, &canonical)
    {
        let help = if !crate::AnalyzerRegistry::default_set().has_analyzer_for(language) {
            format!(
                "this build was compiled without the {} analyzer; its files cannot be \
                 analyzed, so this threshold can never fire (git-only `history.*` \
                 thresholds remain valid)",
                language.canonical()
            )
        } else if canonical.starts_with("sql.") {
            "`sql.*` metrics are published by the SQL analyzer; move the threshold to \
             [languages.sql.thresholds] or the global [thresholds] table (global limits apply \
             only to files that publish the metric)"
                .to_string()
        } else if canonical.starts_with("markdown.") {
            "`markdown.*` metrics are published by the Markdown analyzer; move the threshold \
             to [languages.markdown.thresholds] or the global [thresholds] table"
                .to_string()
        } else if is_interface_scoped_key(&canonical) {
            format!(
                "the interface-scoped members measure interface-like spaces (interfaces, \
                 traits); the {} grammar never opens one",
                language.canonical()
            )
        } else if is_class_family_key(&canonical) {
            format!(
                "the class-shape families (npa, npm, wmc) publish only for languages with \
                 class-like constructs; the {} grammar has none",
                language.canonical()
            )
        } else {
            format!(
                "`{canonical}` is published by the source-code analyzers; {} files publish a \
                 different metric family",
                language.canonical()
            )
        };
        return Err(ctx
            .error_at(
                key.span(),
                "never published for this language",
                format!(
                    "`{metric}` in [{context}] can never fire: {} files do not publish it",
                    language.canonical()
                ),
            )
            .with_help(help));
    }
    if !limit.is_finite() {
        return Err(ctx
            .error_at(
                key.span(),
                "non-finite limit",
                format!("`{context}.{metric}` must be a finite number, got `{limit}`"),
            )
            .with_help("use a finite numeric limit; `inf` and `nan` cannot gate a metric"));
    }
    if let Some(existing) = out.get(&canonical) {
        let spelled = if existing.spelling == metric {
            format!("`{metric}` appears twice")
        } else {
            format!(
                "`{metric}` and `{}` are spellings of the same metric (`{canonical}`)",
                existing.spelling
            )
        };
        return Err(ctx
            .error_at(
                key.span(),
                "duplicate threshold",
                format!("duplicate threshold for `{canonical}` in [{context}]: {spelled}"),
            )
            .with_help(
                "keep one limit per metric and table; contradictory duplicates would silently \
                 disable the stricter gate",
            ));
    }
    out.insert(
        canonical,
        ThresholdEntry {
            spelling: metric.to_string(),
            table: context.to_string(),
            limit,
        },
    );
    Ok(())
}

/// Validate a configured metric name and return its canonical
/// published key. A name no analyzer can publish is rejected at load
/// time with a suggestion — otherwise the threshold would be a gate
/// that can never fire. `span` is the key's exact source location.
fn validate_metric_name(
    name: &str,
    span: std::ops::Range<usize>,
    context: &str,
    ctx: &ErrorContext<'_>,
) -> Result<String, ConfigError> {
    match canonical_metric_key(name) {
        Ok(canonical) => Ok(canonical),
        Err(ResolveError::UnknownHistory) => Err(ctx
            .error_at(
                span,
                "not a history metric",
                format!("unknown history metric `{name}` in [{context}]"),
            )
            .with_help(format!(
                "the fixed `history.*` family is: {}",
                keys::HISTORY_ALL.join(", ")
            ))),
        #[cfg(not(feature = "lang-sql"))]
        Err(ResolveError::UnavailableNamespace) => Err(ctx
            .error_at(
                span,
                "unavailable in this build",
                format!("unavailable metric `{name}` in [{context}]"),
            )
            .with_help(
                "this build was compiled without the SQL analyzer (`lang-sql` feature); a \
                 `sql.*` threshold could never fire",
            )),
        Err(ResolveError::UnknownNamespaced) => {
            let candidates = namespaced_candidates(name);
            let help = match closest_candidate(name, candidates) {
                Some(candidate) => format!("did you mean `{candidate}`?"),
                None => format!(
                    "the analyzer that owns this namespace never publishes `{name}`; see \
                     the metric reference for the published keys"
                ),
            };
            Err(ctx
                .error_at(
                    span,
                    "not a published metric",
                    format!("unknown metric `{name}` in [{context}]"),
                )
                .with_help(help))
        }
        Err(ResolveError::Unknown) => {
            let mut candidates: Vec<&str> = PUBLISHED_METRIC_KEYS.to_vec();
            candidates.extend_from_slice(keys::HISTORY_ALL);
            // A name whose family publishes members gets the family
            // listing (`mi` → `mi.visual_studio`, …): an edit-distance
            // pick like `wmc` for `mi` would point away from the
            // obvious intent.
            let help = match family_members(name) {
                Some(members) => format!(
                    "no analyzer publishes `{name}`; its family publishes: {}",
                    members.join(", ")
                ),
                None => match closest_candidate(name, &candidates) {
                    Some(candidate) => format!("did you mean `{candidate}`?"),
                    None => "use a key mehen publishes: the source-code families \
                             (cognitive, cyclomatic, loc.*, halstead.*, mi.*, abc, nargs, \
                             nexit, nom.*, npa.*, npm.*, wmc), the fixed `history.*` keys, \
                             or a namespaced `sql.*` / `markdown.*` key"
                        .to_string(),
                },
            };
            Err(ctx
                .error_at(
                    span,
                    "not a published metric",
                    format!("unknown metric `{name}` in [{context}]"),
                )
                .with_help(help))
        }
    }
}

/// The published-key candidates for a namespaced (`sql.*` /
/// `markdown.*`) suggestion, from the owning analyzer's catalogue.
fn namespaced_candidates(name: &str) -> &'static [&'static str] {
    if name.starts_with("sql.") {
        #[cfg(feature = "lang-sql")]
        {
            return mehen_sql::PUBLISHED_METRIC_KEYS;
        }
    }
    if name.starts_with("markdown.") {
        return mehen_markdown::PUBLISHED_METRIC_KEYS;
    }
    &[]
}

/// Published keys sharing the name's family (`mi` → `mi.visual_studio`,
/// `mi.original`, `mi.sei`; `cognitive.maximum` → the `cognitive`
/// members). Drives the help text for near-miss names too far away for
/// the edit-distance suggestion.
fn family_members(name: &str) -> Option<Vec<&'static str>> {
    let base = name.split('.').next().unwrap_or(name);
    let members: Vec<&'static str> = PUBLISHED_METRIC_KEYS
        .iter()
        .copied()
        .filter(|key| *key == base || key.starts_with(&format!("{base}.")))
        .collect();
    if members.is_empty() {
        None
    } else {
        Some(members)
    }
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

/// Sort breaches by path then metric — the deterministic order shared
/// by the stderr report and the JSON `threshold_violations` payload.
pub(crate) fn sort_breaches(breaches: &mut [ThresholdBreach]) {
    breaches.sort_by(|a, b| {
        (a.path.as_str(), a.metric.as_str()).cmp(&(b.path.as_str(), b.metric.as_str()))
    });
}

/// Render the violation report printed to stderr before exiting 1.
///
/// Sorted by path then metric for determinism, grouped per file; each
/// line names the measured value, the crossed limit, and the exact
/// config table that set it (`[thresholds]` or
/// `[languages.<lang>.thresholds]`, alias spelling preserved).
pub fn render_threshold_report(breaches: &mut [ThresholdBreach], config_path: &Path) -> String {
    sort_breaches(breaches);

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
            Polarity::HigherIsWorse => format!("exceeds max {}", format_number(breach.limit)),
            Polarity::HigherIsBetter => format!("below min {}", format_number(breach.limit)),
        };
        message.push_str(&format!(
            "\n  {} = {} — {comparison}  (set by {})",
            breach.metric,
            format_number(breach.value),
            breach.source_table
        ));
    }
    let report = ThresholdReport {
        message,
        help: Some(
            "adjust or remove the limit at the configuration path shown, or bring the file \
             back within it."
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

/// Shortest exact decimal for a metric value or limit (Rust's `f64`
/// `Display` round-trips): `23` stays `23`, `12.4` stays `12.4`, and a
/// close crossing like `0.504` over a `0.503` limit keeps every digit
/// instead of rounding both sides to an impossible-looking `0.50 >
/// 0.50`.
fn format_number(v: f64) -> String {
    format!("{v}")
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

    fn global_limit(config: &ConfigFile, canonical: &str) -> Option<f64> {
        config
            .thresholds
            .global
            .get(canonical)
            .map(|entry| entry.limit)
    }

    #[test]
    fn parses_global_thresholds_with_dotted_and_quoted_keys() {
        let config =
            parse("[thresholds]\ncognitive = 15\nloc.lloc = 500\n\"mi.visual_studio\" = 40.5\n")
                .expect("valid config");
        // `cognitive` canonicalizes to its published rollup key.
        assert_eq!(global_limit(&config, "cognitive.sum"), Some(15.0));
        assert_eq!(global_limit(&config, "loc.lloc"), Some(500.0));
        assert_eq!(global_limit(&config, "mi.visual_studio"), Some(40.5));
    }

    #[test]
    #[cfg(feature = "lang-python")]
    fn parses_language_override_with_alias() {
        let config = parse("[languages.py.thresholds]\ncognitive = 10\n").expect("valid config");
        assert_eq!(config.thresholds.per_language.len(), 1);
        let (language, thresholds) = &config.thresholds.per_language[0];
        assert_eq!(*language, Language::Python);
        let entry = thresholds.get("cognitive.sum").expect("entry present");
        assert_eq!(entry.limit, 10.0);
        // The table path preserves the alias exactly as written.
        assert_eq!(entry.table, "languages.py.thresholds");
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
    fn rejects_unpublished_bare_family_root() {
        // No analyzer publishes a bare `mi` key — accepting it would
        // create a gate that can never fire.
        let err = parse("[thresholds]\nmi = 40\n").unwrap_err();
        assert!(err.to_string().contains("unknown metric `mi`"));
        let rendered = rendered(&err);
        assert!(
            rendered.contains("mi.visual_studio")
                && rendered.contains("mi.original")
                && rendered.contains("mi.sei"),
            "help must list the family's published keys: {rendered}"
        );
    }

    #[test]
    fn rejects_unpublished_aggregate_spelling() {
        // `cognitive.maximum` is a plausible near-miss of
        // `cognitive.max` that no analyzer publishes.
        let err = parse("[thresholds]\n\"cognitive.maximum\" = 1\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown metric `cognitive.maximum`")
        );
        assert!(
            rendered(&err).contains("cognitive.max"),
            "help must point at the published spelling"
        );
    }

    #[test]
    fn rejects_duplicate_metric_across_spellings() {
        // TOML sees two distinct keys; canonically they are one
        // threshold — the stricter gate must not be silently
        // overwritten.
        let err = parse("[thresholds]\n\"loc.lloc\" = 1000000000\nloc.lloc = 0\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("duplicate threshold for `loc.lloc`"),
            "{err}"
        );

        let err = parse("[thresholds]\ncognitive = 5\n\"cognitive.sum\" = 6\n").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("duplicate threshold for `cognitive.sum`")
                && message.contains("`cognitive.sum` and `cognitive` are spellings"),
            "{message}"
        );
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
    #[cfg(feature = "lang-python")]
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
    #[cfg(feature = "lang-sql")]
    fn accepts_namespaced_and_published_member_metrics() {
        let config = parse(
            "[thresholds]\n\"sql.change_risk_score\" = 3\n\"history.hotspot\" = 100\nnargs = 6\n\"cognitive.max\" = 20\n\"nom.functions.max\" = 9\n",
        )
        .expect("valid config");
        assert_eq!(config.thresholds.global.len(), 5);
        // Aggregate aliases canonicalize to the published spelling.
        assert_eq!(global_limit(&config, "nom.functions_max"), Some(9.0));
    }

    #[test]
    #[cfg(all(feature = "lang-python", feature = "lang-rust"))]
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
        assert_eq!(breaches[0].source_table, "languages.python.thresholds");
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
    fn evaluate_matches_filter_by_canonical_key() {
        // A `cognitive.sum` threshold and a selected `cognitive`
        // column read the same published key — the gate must fire
        // even though the raw spellings differ.
        let config = parse("[thresholds]\n\"cognitive.sum\" = 5\n").expect("valid config");
        let space = space_with(&[("cognitive.sum", 50.0)]);
        let breaches =
            config
                .thresholds
                .evaluate("a.py", Language::Python, &space, Some(&["cognitive"]));
        assert_eq!(breaches.len(), 1, "canonical spellings must match");
        // The report keeps the user's spelling.
        assert_eq!(breaches[0].metric, "cognitive.sum");
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
    fn aggregate_alias_reads_underscore_sub_bucket_key() {
        // `nom.functions.max` canonicalizes to the published
        // `nom.functions_max` at load time, so evaluation is a direct
        // key read.
        let config = parse("[thresholds]\n\"nom.functions.max\" = 5\n").expect("valid config");
        let space = space_with(&[("nom.functions_max", 9.0)]);
        let breaches = config
            .thresholds
            .evaluate("a.py", Language::Python, &space, None);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].value, 9.0);
    }

    #[test]
    fn every_toml_spelling_normalizes_to_the_same_semantic_path() {
        // Dotted keys (with legal whitespace, quoting, and escapes),
        // nested headers, and inline tables are all the same TOML
        // entry; the breach reports the parse-tree path (`loc.lloc`
        // set by `thresholds`) for every one of them — no spelling
        // recovery from source text is involved.
        let space = space_with(&[("loc.lloc", 640.0)]);
        for config_text in [
            "[thresholds]\nloc.lloc = 500\n",
            "[thresholds]\n\"loc.lloc\" = 500\n",
            "[thresholds]\nloc . lloc = 500\n",
            "[thresholds]\n\"loc\" . lloc = 500\n",
            "[thresholds]\nloc.\"lloc\" = 500\n",
            // Basic-string escapes normalize in the parse tree
            // (`"lo\u0063"` is the key `loc`).
            "[thresholds]\n\"lo\\u0063\".lloc = 500\n",
            "[thresholds.loc]\nlloc = 500\n",
            "thresholds = { loc = { lloc = 500 } }\n",
            "[thresholds]\nloc = { lloc = 500 }\n",
        ] {
            let config = parse(config_text).expect("valid config");
            let breaches = config
                .thresholds
                .evaluate("a.py", Language::Python, &space, None);
            assert_eq!(breaches.len(), 1, "{config_text}");
            assert_eq!(breaches[0].metric, "loc.lloc", "{config_text}");
            assert_eq!(breaches[0].source_table, "thresholds", "{config_text}");
        }
    }

    #[test]
    #[cfg(feature = "lang-python")]
    fn override_attribution_is_not_fooled_by_spellings_elsewhere() {
        // The dotted `loc.lloc` occurrence in the *global* table must
        // not affect the override's attribution: paths come from the
        // parse tree, never from searching the source text.
        let config = parse(
            "[thresholds]\n\"loc.lloc\" = 1000\n\n[languages.py.thresholds.loc]\nlloc = 10\n",
        )
        .expect("valid config");
        let space = space_with(&[("loc.lloc", 640.0)]);
        let breaches = config
            .thresholds
            .evaluate("a.py", Language::Python, &space, None);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].metric, "loc.lloc");
        assert_eq!(breaches[0].source_table, "languages.py.thresholds");
        assert_eq!(breaches[0].limit, 10.0);
    }

    #[test]
    fn halstead_level_is_gated_as_a_minimum() {
        // Program level is inverse difficulty (L = 1/D): larger is
        // healthier, unlike the rest of the halstead family.
        let config = parse("[thresholds]\n\"halstead.level\" = 0.2\n").expect("valid config");
        let below = space_with(&[("halstead.level", 0.1)]);
        let breaches = config
            .thresholds
            .evaluate("a.py", Language::Python, &below, None);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].polarity, Polarity::HigherIsBetter);
        let above = space_with(&[("halstead.level", 0.3)]);
        assert!(
            config
                .thresholds
                .evaluate("a.py", Language::Python, &above, None)
                .is_empty()
        );
    }

    #[test]
    #[cfg(feature = "lang-sql")]
    fn sql_modularity_na_sentinel_is_not_gated() {
        let config = parse("[thresholds]\n\"sql.modularity_health\" = 50\n").expect("valid config");
        // No CTEs: the published 0.0 is an N/A sentinel, not a score —
        // a minimum must not fail every ordinary non-CTE SQL file.
        let na = space_with(&[("sql.modularity_health", 0.0), ("sql.cte.count", 0.0)]);
        assert!(
            config
                .thresholds
                .evaluate("q.sql", Language::Sql, &na, None)
                .is_empty()
        );
        // A CTE-bearing file still gates — including a genuine zero.
        let scored = space_with(&[("sql.modularity_health", 0.0), ("sql.cte.count", 2.0)]);
        assert_eq!(
            config
                .thresholds
                .evaluate("q.sql", Language::Sql, &scored, None)
                .len(),
            1
        );
        let low = space_with(&[("sql.modularity_health", 30.0), ("sql.cte.count", 2.0)]);
        assert_eq!(
            config
                .thresholds
                .evaluate("q.sql", Language::Sql, &low, None)
                .len(),
            1
        );
    }

    #[test]
    fn integer_and_float_digit_separators_parse() {
        let config = parse("[thresholds]\ncognitive = 1_000\n\"loc.lloc\" = 1_500.5\n")
            .expect("digit separators are legal TOML");
        assert_eq!(global_limit(&config, "cognitive.sum"), Some(1000.0));
        assert_eq!(global_limit(&config, "loc.lloc"), Some(1500.5));
    }

    #[test]
    fn non_decimal_integer_spellings_parse() {
        let config = parse(
            "[thresholds]\ncognitive = 0x10\n\"loc.lloc\" = 0o20\n\"loc.sloc\" = 0b1000\nnargs = 0xdead_beef\n",
        )
        .expect("hex/octal/binary integers are legal TOML");
        assert_eq!(global_limit(&config, "cognitive.sum"), Some(16.0));
        assert_eq!(global_limit(&config, "loc.lloc"), Some(16.0));
        assert_eq!(global_limit(&config, "loc.sloc"), Some(8.0));
        assert_eq!(global_limit(&config, "nargs"), Some(3735928559.0));
    }

    #[test]
    #[cfg(feature = "lang-sql")]
    fn rejects_unpublished_namespaced_metrics_with_suggestion() {
        // The owning analyzers' catalogues validate `sql.*` and
        // `markdown.*` names, so a typo cannot become a gate that
        // never fires.
        let err = parse("[thresholds]\n\"sql.modularit_health\" = 50\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown metric `sql.modularit_health`")
        );
        assert!(
            rendered(&err).contains("did you mean `sql.modularity_health`?"),
            "{}",
            rendered(&err)
        );

        let err = parse("[thresholds]\n\"markdown.links.borken\" = 1\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown metric `markdown.links.borken`")
        );
    }

    #[test]
    #[cfg(feature = "lang-sql")]
    fn accepts_namespaced_dynamic_family_members() {
        let config = parse(
            "[thresholds]\n\"sql.statement.kind_count.select\" = 20\n\"sql.dialect.is_postgres\" = 1\n\"markdown.loc.tloc\" = 400\n",
        )
        .expect("published dynamic-family keys are valid");
        assert_eq!(config.thresholds.global.len(), 3);
    }

    #[test]
    #[cfg(all(
        feature = "lang-sql",
        feature = "lang-python",
        feature = "lang-c",
        feature = "lang-go"
    ))]
    fn rejects_language_incompatible_thresholds() {
        // A language override naming a metric its files can never
        // publish would be a permanently dead gate.
        let err =
            parse("[languages.python.thresholds]\n\"sql.change_risk_score\" = 3\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");
        assert!(
            rendered(&err).contains("published by the SQL analyzer"),
            "{}",
            rendered(&err)
        );

        let err = parse("[languages.sql.thresholds]\ncognitive = 5\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");

        let err = parse("[languages.markdown.thresholds]\n\"loc.lloc\" = 100\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");

        // Class-shape families need class-like constructs; the C and
        // Go grammars have none, so those overrides are dead gates.
        let err = parse("[languages.c.thresholds]\nwmc = 1\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");
        assert!(
            rendered(&err).contains("class-like constructs"),
            "{}",
            rendered(&err)
        );
        let err = parse("[languages.go.thresholds]\n\"npm.classes\" = 1\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");
        // Class-capable languages keep the whole catalogue.
        assert!(parse("[languages.python.thresholds]\nwmc = 5\n").is_ok());
        // …except the interface-scoped members: Python classes exist,
        // interface-like spaces do not.
        let err =
            parse("[languages.python.thresholds]\n\"npa.interfaces_average\" = 2\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");
        assert!(
            rendered(&err).contains("never opens one"),
            "{}",
            rendered(&err)
        );
        assert!(parse("[languages.python.thresholds]\n\"npa.classes_average\" = 2\n").is_ok());

        // The owning language and the engine-injected history family
        // stay valid, and global cross-language thresholds are
        // untouched (they apply only to files that publish the key).
        let config = parse(
            "[thresholds]\n\"sql.change_risk_score\" = 3\n\n[languages.sql.thresholds]\n\"sql.cognitive_complexity\" = 40\n\n[languages.python.thresholds]\n\"history.hotspot\" = 100\n",
        )
        .expect("compatible thresholds are valid");
        assert!(!config.thresholds.is_empty());
    }

    #[test]
    #[cfg(not(feature = "lang-rust"))]
    fn rejects_static_overrides_for_uncompiled_analyzers() {
        // Feature-reduced builds cannot analyze the language at all,
        // so any static override for it is a permanently dead gate —
        // including the static-dependent history composites (hotspot
        // reads the analyzer's cognitive sum). Git-only history keys
        // need no analyzer and stay valid.
        let err = parse("[languages.rust.thresholds]\ncognitive = 1\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");
        assert!(
            rendered(&err).contains("compiled without the rust analyzer"),
            "{}",
            rendered(&err)
        );
        let err = parse("[languages.rust.thresholds]\n\"history.hotspot\" = 9\n").unwrap_err();
        assert!(err.to_string().contains("can never fire"), "{err}");
        assert!(parse("[languages.rust.thresholds]\n\"history.churn.abs\" = 40\n").is_ok());
    }

    #[test]
    fn rejects_ambiguous_bare_aliases_with_family_help() {
        // Bare `loc` / `nom` are published root keys, but the Action
        // ecosystem treats them as aliases for `loc.lloc` /
        // `nom.functions` — configuring them would gate a different
        // measurement than the name suggests.
        let err = parse("[thresholds]\nloc = 100\n").unwrap_err();
        assert!(err.to_string().contains("unknown metric `loc`"));
        assert!(
            rendered(&err).contains("loc.sloc"),
            "help must list the precise family members: {}",
            rendered(&err)
        );
        let err = parse("[thresholds]\nnom = 10\n").unwrap_err();
        assert!(
            rendered(&err).contains("nom.functions"),
            "{}",
            rendered(&err)
        );
    }

    #[test]
    fn halstead_level_zero_difficulty_sentinel_is_not_gated() {
        // `level = 1/D` is undefined at D == 0 and published as 0.0;
        // a configured minimum must not fail empty/token-free files.
        let config = parse("[thresholds]\n\"halstead.level\" = 0.2\n").expect("valid config");
        let sentinel = space_with(&[("halstead.level", 0.0), ("halstead.difficulty", 0.0)]);
        assert!(
            config
                .thresholds
                .evaluate("a.py", Language::Python, &sentinel, None)
                .is_empty()
        );
        // A measured low level on a real file still gates.
        let low = space_with(&[("halstead.level", 0.05), ("halstead.difficulty", 20.0)]);
        assert_eq!(
            config
                .thresholds
                .evaluate("a.py", Language::Python, &low, None)
                .len(),
            1
        );
    }

    #[test]
    #[cfg(feature = "lang-csharp")]
    fn csharp_alias_paths_render_toml_quoted() {
        // `c#` cannot be a bare TOML key (`#` starts a comment): the
        // reported path must quote it or it could not identify the
        // table.
        let config = parse("[languages.\"c#\".thresholds]\ncognitive = 1\n")
            .expect("quoted alias section is valid");
        let space = space_with(&[("cognitive.sum", 5.0)]);
        let breaches = config
            .thresholds
            .evaluate("a.cs", Language::CSharp, &space, None);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].source_table, "languages.\"c#\".thresholds");
    }

    #[test]
    fn evaluate_skips_non_finite_measurements() {
        // Undefined measurements publish as NaN (e.g. an interface
        // average over a zero interface count); NaN compares false
        // under both polarities and must be skipped as unmeasurable,
        // not silently passed.
        let config = parse("[thresholds]\ncognitive = 2\n\"mi.visual_studio\" = 40\n")
            .expect("valid config");
        let space = space_with(&[("cognitive.sum", f64::NAN), ("mi.visual_studio", f64::NAN)]);
        assert!(
            config
                .thresholds
                .evaluate("a.ts", Language::TypeScript, &space, None)
                .is_empty()
        );
    }

    #[test]
    fn report_preserves_precision_on_close_crossings() {
        // Rounding both sides to two decimals would render an
        // impossible-looking `0.5 — exceeds max 0.5`.
        let mut breaches = vec![ThresholdBreach {
            path: "a.py".to_string(),
            metric: "sql.change_risk_score".to_string(),
            value: 0.504,
            limit: 0.503,
            polarity: Polarity::HigherIsWorse,
            source_table: "thresholds".to_string(),
        }];
        let report = render_threshold_report(&mut breaches, Path::new("mehen.toml"));
        assert!(
            report.contains("sql.change_risk_score = 0.504 — exceeds max 0.503"),
            "{report}"
        );
    }

    #[test]
    fn validator_accepts_every_key_real_analyzers_publish() {
        // The `PUBLISHED_METRIC_KEYS` catalogue must not drift behind
        // the publishers in `mehen-metrics::state`: analyze real
        // sources (functions for the function families, classes for
        // npa/npm/wmc) and require every published root key to
        // canonicalize to itself.
        use mehen_core::{AnalysisConfig, SourceFile};

        let registry = crate::AnalyzerRegistry::default_set();
        let samples: &[(&str, Language, &str)] = &[
            (
                "sample.py",
                Language::Python,
                "def foo(x):\n    if x:\n        return 1\n    return 2\n",
            ),
            (
                "Sample.java",
                Language::Java,
                "public class Sample {\n    private int count;\n    public int get() { return count > 0 ? count : 0; }\n}\n",
            ),
        ];
        for (name, language, body) in samples {
            // Feature-reduced builds skip languages they don't compile.
            let Some(analyzer) = registry.analyzer_for(*language) else {
                continue;
            };
            let source = SourceFile::new(
                camino::Utf8PathBuf::from(*name),
                *language,
                (*body).to_string(),
            );
            let analysis = analyzer
                .analyze(&source, &AnalysisConfig::default())
                .expect("analysis succeeds");
            for (key, _) in analysis.root.metrics.iter() {
                let key = key.as_str();
                // Published on the root but deliberately not
                // configurable: the GitHub Action ecosystem treats
                // these bare names as legacy aliases for `loc.lloc` /
                // `nom.functions`, so accepting them would gate a
                // different measurement than the name suggests.
                if matches!(key, "loc" | "nom") {
                    assert!(
                        canonical_metric_key(key).is_err(),
                        "ambiguous alias `{key}` must stay non-configurable"
                    );
                    continue;
                }
                let canonical = canonical_metric_key(key)
                    .unwrap_or_else(|_| panic!("published key `{key}` must validate"));
                // The canonical form must be readable from the same
                // space (bare `cognitive` aliases to its published
                // `cognitive.sum` rollup — both keys carry the value).
                assert!(
                    analysis
                        .root
                        .metrics
                        .get(&MetricKey::new(canonical.as_str()))
                        .is_some(),
                    "canonical `{canonical}` of published `{key}` must be readable"
                );
            }
        }
        // The engine-published history family validates too.
        for key in keys::HISTORY_ALL {
            assert!(canonical_metric_key(key).is_ok(), "{key} must validate");
        }
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
                source_table: "thresholds".to_string(),
            },
            ThresholdBreach {
                path: "src/app/core.py".to_string(),
                metric: "loc.lloc".to_string(),
                value: 640.0,
                limit: 500.0,
                polarity: Polarity::HigherIsWorse,
                source_table: "thresholds".to_string(),
            },
            ThresholdBreach {
                path: "src/app/core.py".to_string(),
                metric: "cognitive".to_string(),
                value: 23.0,
                limit: 15.0,
                polarity: Polarity::HigherIsWorse,
                source_table: "languages.py.thresholds".to_string(),
            },
        ];
        let report = render_threshold_report(&mut breaches, Path::new("/repo/mehen.toml"));
        assert!(
            report.contains("3 metric threshold violations (config: /repo/mehen.toml)"),
            "{report}"
        );
        // The language alias is preserved (it is a real parsed key)
        // and the pointer is a semantic key path, not a spelling claim.
        let cognitive = "cognitive = 23 — exceeds max 15  (set by languages.py.thresholds)";
        let lloc = "loc.lloc = 640 — exceeds max 500  (set by thresholds)";
        let mi = "mi.visual_studio = 12.4 — below min 40  (set by thresholds)";
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
            source_table: "thresholds".to_string(),
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
        init_git(dir.path());
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).expect("mkdirs");
        std::fs::write(
            dir.path().join("mehen.toml"),
            "[thresholds]\ncognitive = 1\n",
        )
        .expect("write config");
        let found = discover_config_path(&nested).expect("config discovered");
        assert_eq!(
            std::fs::canonicalize(found).expect("canonical found"),
            std::fs::canonicalize(dir.path().join("mehen.toml")).expect("canonical expected")
        );

        // A closer `.mehen.toml` shadows the ancestor's `mehen.toml`.
        std::fs::write(nested.join(".mehen.toml"), "[thresholds]\ncognitive = 2\n")
            .expect("write hidden config");
        let found = discover_config_path(&nested).expect("config discovered");
        assert_eq!(
            std::fs::canonicalize(found).expect("canonical found"),
            std::fs::canonicalize(nested.join(".mehen.toml")).expect("canonical expected")
        );
    }

    #[test]
    fn discovery_stops_at_the_repository_root() {
        // outer/mehen.toml sits *above* the repository at outer/repo:
        // it cannot belong to the project and must not be picked up.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("mehen.toml"),
            "[thresholds]\ncognitive = 1\n",
        )
        .expect("write outer config");
        let repo = dir.path().join("repo");
        let nested = repo.join("src");
        std::fs::create_dir_all(&nested).expect("mkdirs");
        init_git(&repo);

        assert_eq!(discover_config_path(&nested), None);

        // A config at the repository root (the boundary itself) is
        // still discovered.
        std::fs::write(repo.join("mehen.toml"), "[thresholds]\ncognitive = 2\n")
            .expect("write repo config");
        let found = discover_config_path(&nested).expect("repo-root config discovered");
        assert_eq!(
            std::fs::canonicalize(found).expect("canonical found"),
            std::fs::canonicalize(repo.join("mehen.toml")).expect("canonical expected")
        );
    }

    #[test]
    fn discovery_outside_a_repository_checks_only_the_start_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("mehen.toml"),
            "[thresholds]\ncognitive = 1\n",
        )
        .expect("write parent config");
        let nested = dir.path().join("a");
        std::fs::create_dir_all(&nested).expect("mkdirs");

        // No repository anywhere up the tempdir chain: the parent's
        // config is out of reach…
        assert_eq!(discover_config_path(&nested), None);

        // …but a config in the start directory itself is found.
        std::fs::write(nested.join("mehen.toml"), "[thresholds]\ncognitive = 2\n")
            .expect("write local config");
        let found = discover_config_path(&nested).expect("local config discovered");
        assert_eq!(
            std::fs::canonicalize(found).expect("canonical found"),
            std::fs::canonicalize(nested.join("mehen.toml")).expect("canonical expected")
        );
    }

    fn init_git(path: &Path) {
        let output = std::process::Command::new("git")
            .current_dir(path)
            .args(["init", "-q", "-b", "main"])
            .output()
            .expect("failed to run git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
