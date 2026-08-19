// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Shared metric selection primitives used by `diff` and `top-offenders`.
//!
//! A *selector* is a known metric name (e.g. `loc.lloc`) bundled with a
//! display label and a [`Polarity`] (whether higher or lower values are
//! "better"). Production diff/top-offenders pipelines read the
//! `MetricSpace::metrics` map via [`read_metric`].

use mehen_core::{MetricKey, MetricSpace};

/// Whether a metric is "better" when higher or lower.
///
/// Used by callers to interpret deltas/rankings (e.g. `Cyclomatic` is
/// [`Polarity::LowerIsBetter`], while `Mi` is [`Polarity::HigherIsBetter`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Polarity {
    LowerIsBetter,
    HigherIsBetter,
}

/// A selector for a single metric column: name, display label, polarity.
#[derive(Debug, Clone)]
pub(crate) struct MetricSelector {
    pub name: &'static str,
    pub label: &'static str,
    pub polarity: Polarity,
}

type MetricDef = (&'static str, &'static str, Polarity);

/// Catalogue of metrics that can be referenced by name from the CLI.
pub(crate) const KNOWN_METRICS: &[MetricDef] = &[
    ("cyclomatic", "Cyclomatic", Polarity::LowerIsBetter),
    ("cognitive", "Cognitive", Polarity::LowerIsBetter),
    ("nom.functions", "Functions", Polarity::LowerIsBetter),
    ("loc.lloc", "LLOC", Polarity::LowerIsBetter),
    ("mi.original", "MI (Original)", Polarity::HigherIsBetter),
    ("mi.sei", "MI (SEI)", Polarity::HigherIsBetter),
    ("mi.visual_studio", "MI", Polarity::HigherIsBetter),
    ("halstead.volume", "Halstead Vol", Polarity::LowerIsBetter),
    ("abc", "ABC", Polarity::LowerIsBetter),
    // Default-set history columns get curated labels; the rest of the
    // `history.*` family is reachable via the namespaced-key path.
    ("history.hotspot", "Hotspot", Polarity::LowerIsBetter),
    ("history.churn.relative", "Churn", Polarity::LowerIsBetter),
];

/// Default metric set for `diff` (kept here so both diff and top-offenders
/// can surface the same fallback from a single source of truth).
///
/// One column per orthogonal dimension (research foundation §9.3/§9.4):
/// control-flow understandability (`cognitive`), computational volume
/// (`abc`), the one deliberate composite rollup (`mi.visual_studio`), and
/// the two change-risk signals a *diff* comment actually needs — whether
/// this is a fragile, frequently-touched file (`history.hotspot` =
/// cognitive × commit frequency) and how much of the file the change
/// moves, size-normalized (`history.churn.relative`). `cyclomatic` was
/// dropped in favor of cognitive (the two correlate strongly) and
/// `nom.functions`/`loc.lloc` in favor of the missing axes. The history
/// columns trigger the repository history walk on default diffs.
pub(crate) const DEFAULT_METRICS: &[&str] = &[
    "cognitive",
    "abc",
    "mi.visual_studio",
    "history.hotspot",
    "history.churn.relative",
];

/// Default selectors for SQL files. The source-code defaults
/// ([`DEFAULT_METRICS`]) are all keys the SQL analyzer never publishes, so a
/// SQL file diffed with them reads `0.0` for every column and is dropped as
/// "unchanged". SQL files instead default to the first-release composite set
/// (research foundation §15) so `mehen diff` surfaces SQL changes without the
/// caller needing to know the `sql.*` keys (Codex P2).
pub(crate) const DEFAULT_SQL_METRICS: &[&str] = &[
    "sql.change_risk_score",
    "sql.maintainability_index",
    "sql.review_burden_index",
    "sql.cognitive_complexity",
    "sql.loc.code",
];

/// Default metric specs for a language, used when the caller passes no
/// explicit `--metric`. SQL owns a disjoint metric namespace, so it gets its
/// own defaults; every other language uses the source-code defaults.
pub(crate) fn default_metrics_for_language(
    language: mehen_core::Language,
) -> &'static [&'static str] {
    match language {
        mehen_core::Language::Sql => DEFAULT_SQL_METRICS,
        _ => DEFAULT_METRICS,
    }
}

/// Resolve the default selectors for a language (no explicit `--metric`).
///
/// SQL defaults are `'static` names, so they are built into `MetricSelector`s
/// directly (no `String` allocation / `Box::leak` round-trip through the
/// namespaced-parsing path). Other languages use the source-code catalogue.
pub(crate) fn default_selectors_for_language(
    language: mehen_core::Language,
) -> Vec<MetricSelector> {
    default_metrics_for_language(language)
        .iter()
        .map(|&name| {
            // A KNOWN_METRICS entry carries a curated label/polarity; a
            // namespaced key (`sql.*`) is its own label with a by-key polarity.
            match KNOWN_METRICS.iter().find(|(n, ..)| *n == name) {
                Some(&(n, label, polarity)) => MetricSelector {
                    name: n,
                    label,
                    polarity,
                },
                None => MetricSelector {
                    name,
                    label: name,
                    polarity: default_namespaced_polarity(name),
                },
            }
        })
        .collect()
}

/// Parse a list of metric specs into resolved [`MetricSelector`]s.
///
/// A spec is a bare metric name (`cognitive`) or a polarity-prefixed name
/// (`+nom.functions`, `-mi.visual_studio`). Unknown names emit a warning and
/// are skipped.
///
/// When `specs` is empty, [`DEFAULT_METRICS`] is used as a fallback. This is
/// the contract `diff` expects. Callers that want "no fallback" (e.g.
/// `top-offenders`, where `--metric` is required) should enforce that at the
/// CLI layer before calling this function.
pub(crate) fn parse_metric_selectors(specs: &[String]) -> Vec<MetricSelector> {
    let specs: Vec<&str> = if specs.is_empty() {
        DEFAULT_METRICS.to_vec()
    } else {
        specs.iter().map(|s| s.as_str()).collect()
    };

    let mut selectors = Vec::new();
    for spec in specs {
        let (polarity_override, name) = if let Some(rest) = spec.strip_prefix('+') {
            (Some(Polarity::HigherIsBetter), rest)
        } else if let Some(rest) = spec.strip_prefix('-') {
            (Some(Polarity::LowerIsBetter), rest)
        } else {
            (None, spec)
        };

        if let Some(&(n, label, default_polarity)) = KNOWN_METRICS.iter().find(|(n, ..)| *n == name)
        {
            selectors.push(MetricSelector {
                name: n,
                label,
                polarity: polarity_override.unwrap_or(default_polarity),
            });
        } else if let Ok(canonical) = crate::config_file::canonical_metric_key(name) {
            // Any other key the analyzers publish — the same catalogue
            // `mehen.toml` threshold validation resolves against: the
            // source-code families (`cognitive.max`, `loc.sloc`,
            // `nom.functions.max` — aggregate aliases resolve to their
            // published spelling), the fixed `history.*` family, and
            // the analyzer-owned `sql.*` / `markdown.*` catalogues.
            // Routing the namespaced families through the catalogue —
            // instead of accepting any prefixed name verbatim —
            // rejects typos like `sql.modularit_health` here, so a
            // mistyped CI column cannot silently defeat the correctly
            // configured threshold on the real key. The selector reads
            // the canonical key; the user's spelling stays as the
            // column label.
            let canonical: &'static str = Box::leak(canonical.into_boxed_str());
            let label: &'static str = Box::leak(name.to_string().into_boxed_str());
            let default_polarity = if is_higher_is_better_metric(canonical) {
                Polarity::HigherIsBetter
            } else {
                Polarity::LowerIsBetter
            };
            selectors.push(MetricSelector {
                name: canonical,
                label,
                polarity: polarity_override.unwrap_or(default_polarity),
            });
        } else {
            log::warn!("Unknown metric '{name}', skipping.");
        }
    }

    selectors
}

/// Namespaced (`sql.*` / `markdown.*` / `history.*` / `coverage.*`)
/// metric keys where a
/// *larger* value is healthier. Substring inference is too crude (e.g.
/// `markdown.maintainability.artifact_debt_score` is a penalty despite
/// containing "maintainability", and `sql.dialect.confidence` is
/// higher-is-better), so the higher-is-better metrics are enumerated by exact
/// key and everything else defaults to higher-is-worse. This is the single
/// source of truth shared by the `diff` selector polarity and the
/// `top-offenders` ranking polarity ([`crate::top_offenders`]).
pub(crate) const NAMESPACED_HIGHER_IS_BETTER: &[&str] = &[
    // SQL composite/quality scores where larger is healthier.
    "sql.maintainability_index",
    "sql.modularity_health",
    "sql.select.output_alias_coverage",
    "sql.dialect.confidence",
    // Markdown quality scores where larger is healthier.
    "markdown.maintainability.documentation_maintainability_index",
    "markdown.maintainability.section_balance_score",
    "markdown.maintainability.good_scaffold_score",
    "markdown.grounding.repository_grounding_score",
    "markdown.grounding.evidence_coverage_score",
    "markdown.links.information_scent_score",
    // History process metrics where larger is healthier (research
    // foundation §8): long-stable code and concentrated ownership are
    // the low-risk end; every other `history.*` signal is a risk count.
    mehen_core::keys::HISTORY_AGE_MONTHS,
    mehen_core::keys::HISTORY_OWNERSHIP,
    // Coverage *rates* — more covered code is always the healthier
    // direction, so configured thresholds become minimums
    // (`coverage.line = 80`). Deliberately only the three rates: for
    // the raw `.covered`/`.total` counters a fixed polarity would gate
    // a different measurement than the name suggests (a minimum
    // "total instrumented lines" is not a coverage gate), so counters
    // keep the neutral default and users flip with `+`/`-` when
    // ranking by them.
    mehen_core::keys::COVERAGE_LINE,
    mehen_core::keys::COVERAGE_BRANCH,
    mehen_core::keys::COVERAGE_FUNCTION,
];

/// Whether a namespaced metric key is higher-is-better (see
/// [`NAMESPACED_HIGHER_IS_BETTER`]).
pub(crate) fn is_namespaced_higher_is_better(name: &str) -> bool {
    NAMESPACED_HIGHER_IS_BETTER.contains(&name)
}

/// Whether a metric key — source-code or namespaced — is
/// higher-is-better. The single source of truth shared by the config
/// threshold polarity, the published-catalogue selector branch, and
/// the post-1.0 ranking polarity: `mi.*` variants, the Halstead
/// program level (`L = 1/D` — inverse difficulty, so larger is the
/// healthier direction, unlike the rest of the `halstead.*` family),
/// and the enumerated namespaced quality scores (including the three
/// `coverage.*` rates — see [`NAMESPACED_HIGHER_IS_BETTER`] for why
/// the coverage counters are not listed).
pub(crate) fn is_higher_is_better_metric(key: &str) -> bool {
    key == "mi"
        || key.starts_with("mi.")
        || key == "halstead.level"
        || is_namespaced_higher_is_better(key)
}

/// Default polarity for a namespaced metric, by *exact* key. Users can always
/// override with a `+`/`-` prefix.
fn default_namespaced_polarity(name: &str) -> Polarity {
    if is_namespaced_higher_is_better(name) {
        Polarity::HigherIsBetter
    } else {
        Polarity::LowerIsBetter
    }
}

/// Translate a CLI selector name (e.g. `cyclomatic`, `nom.functions`,
/// `mi.visual_studio`) to the `MetricSet` key the shared walker
/// publishes onto the root `MetricSpace`.
///
/// Most names map verbatim; the rolled-up scalar metrics
/// (`cyclomatic`, `cognitive`) live under their `*.sum` key. Any
/// unknown selector (e.g. a namespaced `sql.*`/`markdown.*` key) falls
/// back to its bare name; missing keys read as `0.0` from `read_metric`.
///
/// The result borrows from `name` for the fallback case, so this never
/// allocates — `read_metric` builds a `MetricKey` from it immediately.
/// (A previous version returned `&'static str` and `Box::leak`ed the
/// fallback, leaking one string per metric-read on namespaced selectors.)
pub(crate) fn metric_set_key_for(name: &str) -> &str {
    match name {
        "cyclomatic" => "cyclomatic.sum",
        "cognitive" => "cognitive.sum",
        "nom.functions" => "nom.functions",
        "loc.lloc" => "loc.lloc",
        "mi.original" => "mi.original",
        "mi.sei" => "mi.sei",
        "mi.visual_studio" => "mi.visual_studio",
        "halstead.volume" => "halstead.volume",
        "abc" => "abc",
        other => other,
    }
}

/// Read a selector's value from the root `MetricSpace`'s `MetricSet`.
///
/// Returns `0.0` for any key the analyzer didn't publish — matching
/// the legacy reader, which fell through to `Default`-initialized
/// `FuncSpace` fields when an analyzer left a metric blank.
pub(crate) fn read_metric(root: &MetricSpace, selector: &MetricSelector) -> f64 {
    let key = metric_set_key_for(selector.name);
    root.metrics
        .get(&MetricKey::new(key))
        .map(|v| v.as_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_specs_empty() {
        let selectors = parse_metric_selectors(&[]);
        assert_eq!(selectors.len(), DEFAULT_METRICS.len());
        for (sel, expected) in selectors.iter().zip(DEFAULT_METRICS.iter()) {
            assert_eq!(sel.name, *expected);
        }
    }

    #[test]
    fn sql_files_default_to_sql_metrics_not_source_code_metrics() {
        // A SQL file diffed with the source-code defaults would read 0 for
        // every column (the SQL analyzer publishes none of them) and be
        // dropped as unchanged. SQL must default to its own composite set.
        let sql = default_selectors_for_language(mehen_core::Language::Sql);
        let names: Vec<&str> = sql.iter().map(|s| s.name).collect();
        assert_eq!(names, DEFAULT_SQL_METRICS);
        // `sql.maintainability_index` is a higher-is-better quality score.
        let mi = sql
            .iter()
            .find(|s| s.name == "sql.maintainability_index")
            .expect("maintainability default present");
        assert_eq!(mi.polarity, Polarity::HigherIsBetter);
        // A non-SQL language keeps the source-code defaults.
        let ts = default_selectors_for_language(mehen_core::Language::TypeScript);
        let ts_names: Vec<&str> = ts.iter().map(|s| s.name).collect();
        assert_eq!(ts_names, DEFAULT_METRICS);
    }

    #[test]
    fn default_history_columns_have_curated_labels_and_trigger_walk() {
        // The §9.4 default set surfaces the two change-risk columns with
        // human labels (not raw keys) in the PR-comment table header.
        let selectors = parse_metric_selectors(&[]);
        let hotspot = selectors
            .iter()
            .find(|s| s.name == "history.hotspot")
            .expect("hotspot default present");
        assert_eq!(hotspot.label, "Hotspot");
        assert_eq!(hotspot.polarity, Polarity::LowerIsBetter);
        let churn = selectors
            .iter()
            .find(|s| s.name == "history.churn.relative")
            .expect("churn default present");
        assert_eq!(churn.label, "Churn");
        assert_eq!(churn.polarity, Polarity::LowerIsBetter);
        // The default set must request history so `run_diff` walks it.
        assert!(
            crate::history_metrics::names_want_history(selectors.iter().map(|s| s.name)),
            "defaults must trigger the history walk"
        );
    }

    #[test]
    fn polarity_prefix_overrides_default() {
        let specs = vec!["+loc.lloc".to_string(), "-mi.visual_studio".to_string()];
        let selectors = parse_metric_selectors(&specs);
        assert_eq!(selectors.len(), 2);
        assert_eq!(selectors[0].name, "loc.lloc");
        assert_eq!(selectors[0].polarity, Polarity::HigherIsBetter);
        assert_eq!(selectors[1].name, "mi.visual_studio");
        assert_eq!(selectors[1].polarity, Polarity::LowerIsBetter);
    }

    #[test]
    fn unknown_metric_is_skipped() {
        let specs = vec!["nonexistent".to_string()];
        let selectors = parse_metric_selectors(&specs);
        assert!(selectors.is_empty());
    }

    #[test]
    fn published_catalogue_keys_are_accepted_as_selectors() {
        // Every key the shared publishers emit — the catalogue
        // `mehen.toml` validation resolves against — must be
        // selectable as a column, or a documented, configurable
        // threshold could never fire in diff/top-offenders.
        let specs = vec![
            "cognitive.max".to_string(),
            "loc.sloc".to_string(),
            "nom.functions.max".to_string(),
        ];
        let selectors = parse_metric_selectors(&specs);
        let names: Vec<&str> = selectors.iter().map(|s| s.name).collect();
        // Aggregate aliases resolve to the published spelling; labels
        // keep the user's spelling.
        assert_eq!(names, ["cognitive.max", "loc.sloc", "nom.functions_max"]);
        assert_eq!(selectors[2].label, "nom.functions.max");
        for selector in &selectors {
            assert_eq!(selector.polarity, Polarity::LowerIsBetter);
        }
        // Unpublished near-misses stay rejected — including namespaced
        // typos, which previously slipped through by prefix and could
        // silently defeat the configured gate on the real key.
        assert!(parse_metric_selectors(&["cognitive.maximum".to_string()]).is_empty());
        assert!(parse_metric_selectors(&["sql.modularit_health".to_string()]).is_empty());
        assert!(parse_metric_selectors(&["markdown.links.borken".to_string()]).is_empty());
    }

    #[test]
    fn mistyped_history_metric_is_rejected() {
        // The `history.*` family is fixed and enumerated: a typo must
        // be rejected up front, not accepted by prefix — accepting it
        // would trigger the expensive history walk and then read the
        // unpublished key as `0.0` (an all-zero ranking / an empty
        // diff instead of a warning).
        let specs = vec!["history.commit_frequncy".to_string()];
        assert!(parse_metric_selectors(&specs).is_empty());
        // Every real key is still accepted verbatim.
        for key in mehen_core::keys::HISTORY_ALL {
            let selectors = parse_metric_selectors(&[key.to_string()]);
            assert_eq!(selectors.len(), 1, "{key} must parse");
            assert_eq!(selectors[0].name, *key);
        }
    }

    #[test]
    fn bare_mi_is_unknown() {
        // `mi` by itself isn't a leaf — you must pick a variant.
        let specs = vec!["mi".to_string()];
        let selectors = parse_metric_selectors(&specs);
        assert!(selectors.is_empty());
    }

    #[test]
    fn namespaced_sql_and_markdown_metrics_are_accepted() {
        // Language-owned `sql.*`/`markdown.*` keys aren't in KNOWN_METRICS but
        // must be usable as `top-offenders`/`diff` selectors.
        let specs = vec![
            "sql.change_risk_score".to_string(),
            "markdown.review.review_criticality_index".to_string(),
        ];
        let selectors = parse_metric_selectors(&specs);
        assert_eq!(selectors.len(), 2);
        assert_eq!(selectors[0].name, "sql.change_risk_score");
        assert_eq!(
            selectors[1].name,
            "markdown.review.review_criticality_index"
        );
    }

    #[test]
    fn namespaced_metric_default_polarity() {
        // Risk/complexity scores are higher-is-worse; health/maintainability
        // scores are higher-is-better.
        assert_eq!(
            default_namespaced_polarity("sql.change_risk_score"),
            Polarity::LowerIsBetter
        );
        assert_eq!(
            default_namespaced_polarity("sql.maintainability_index"),
            Polarity::HigherIsBetter
        );
        assert_eq!(
            default_namespaced_polarity("sql.modularity_health"),
            Polarity::HigherIsBetter
        );
    }

    #[test]
    fn history_metrics_are_accepted_as_namespaced_selectors() {
        // The engine-owned `history.*` family is not in KNOWN_METRICS but
        // must be usable as a `diff`/`top-offenders` selector.
        let specs = vec![
            "history.churn.abs".to_string(),
            "history.hotspot".to_string(),
            "history.age_months".to_string(),
        ];
        let selectors = parse_metric_selectors(&specs);
        assert_eq!(selectors.len(), 3);
        assert_eq!(selectors[0].name, "history.churn.abs");
        assert_eq!(selectors[1].name, "history.hotspot");
        assert_eq!(selectors[2].name, "history.age_months");
    }

    #[test]
    fn history_metric_default_polarity() {
        // Long-stable code and concentrated ownership are the healthy end;
        // every other history signal is a risk count.
        assert_eq!(
            default_namespaced_polarity("history.age_months"),
            Polarity::HigherIsBetter
        );
        assert_eq!(
            default_namespaced_polarity("history.ownership"),
            Polarity::HigherIsBetter
        );
        for risk in [
            "history.churn.abs",
            "history.churn.relative",
            "history.authors",
            "history.minor_contributors",
            "history.commit_frequency",
            "history.hotspot",
            "history.sum_of_coupling",
            "history.twr",
            "history.bugfix_commits",
        ] {
            assert_eq!(
                default_namespaced_polarity(risk),
                Polarity::LowerIsBetter,
                "selector {risk}"
            );
        }
    }
}
