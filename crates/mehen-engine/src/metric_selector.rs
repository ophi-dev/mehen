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
];

/// Default metric set for `diff` (kept here so both diff and top-offenders
/// can surface the same fallback from a single source of truth).
pub(crate) const DEFAULT_METRICS: &[&str] = &[
    "cyclomatic",
    "cognitive",
    "nom.functions",
    "loc.lloc",
    "mi.visual_studio",
];

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
        } else if is_namespaced_metric(name) {
            // Language-owned families (`sql.*`, `markdown.*`) publish flat keys
            // that aren't in the source-code `KNOWN_METRICS` catalogue. Accept
            // any such key verbatim so e.g. `--metric sql.change_risk_score`
            // works. `read_metric` reads the bare key from the `MetricSpace`.
            // The name/label must be `'static`; leak the (short-lived, CLI-run)
            // string, matching `metric_set_key_for`'s fallback.
            let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
            selectors.push(MetricSelector {
                name: leaked,
                label: leaked,
                polarity: polarity_override.unwrap_or_else(|| default_namespaced_polarity(name)),
            });
        } else {
            log::warn!("Unknown metric '{name}', skipping.");
        }
    }

    selectors
}

/// Whether `name` is a language-owned namespaced metric key (`sql.*`,
/// `markdown.*`) that the CLI should accept even though it is not in the
/// source-code `KNOWN_METRICS` catalogue.
fn is_namespaced_metric(name: &str) -> bool {
    name.starts_with("sql.") || name.starts_with("markdown.")
}

/// Default polarity for a namespaced metric, by *exact* key. Substring
/// inference is too crude (e.g. `markdown.maintainability.artifact_debt_score`
/// is a penalty despite containing "maintainability", and `sql.dialect.confidence`
/// is higher-is-better), so the higher-is-better metrics are enumerated and
/// everything else defaults to higher-is-worse. Users can always override with
/// a `+`/`-` prefix.
fn default_namespaced_polarity(name: &str) -> Polarity {
    const HIGHER_IS_BETTER: &[&str] = &[
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
    ];
    if HIGHER_IS_BETTER.contains(&name) {
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
/// unknown selector falls back to its bare name; missing keys read as
/// `0.0` from `read_metric`.
pub(crate) fn metric_set_key_for(name: &str) -> &'static str {
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
        other => Box::leak(other.to_string().into_boxed_str()),
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
}
