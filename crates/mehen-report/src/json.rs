// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use mehen_core::{DiffReport, Language, MetricsReport};

use crate::metrics_json::MetricsFamilies;

/// Render a `MetricsReport` as JSON. Pretty-printed when `pretty=true`.
///
/// Emits the documented per-family shape (`metrics: { cyclomatic, … }`,
/// rewrite plan §9.1) pivoted from the flat keys the analyzer publishes
/// into `root.metrics`. The full `MetricSpace` tree remains available
/// under `root` so consumers that reference individual aggregator keys
/// (e.g. `cyclomatic.max`) keep working alongside the published
/// schema.
///
/// Languages that publish their own flat metric family instead of the
/// source-code families (Markdown's `markdown.*`, SQL's `sql.*`) are exempt
/// from the source-code pivot: `MetricsFamilies::from_metrics` only reads the
/// source-code keys (`cyclomatic`, `halstead.*`, …), so pivoting their reports
/// would replace the real `markdown.*`/`sql.*` values with an all-zero
/// source-code block. Instead, the top-level `metrics` object is populated
/// directly from the flat `root.metrics` map, so consumers reading
/// `.metrics["sql.cte.count"]` still see the language-owned values.
pub fn render_metrics_json(report: &MetricsReport, pretty: bool) -> serde_json::Result<String> {
    let mut value = serde_json::to_value(report)?;
    let metrics = if publishes_own_family(report.language) {
        // Flat map of the language-owned family (`sql.*` / `markdown.*`).
        serde_json::to_value(&report.root.metrics)?
    } else {
        // Pivot the source-code flat keys into the documented per-family shape.
        serde_json::to_value(MetricsFamilies::from_metrics(&report.root.metrics))?
    };
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("metrics".to_string(), metrics);
    }
    if pretty {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
}

/// Whether `language` publishes its own flat metric family (and therefore
/// must not be pivoted through the source-code `MetricsFamilies` shape).
fn publishes_own_family(language: Language) -> bool {
    matches!(language, Language::Markdown | Language::Sql)
}

/// Render a `DiffReport` as JSON. Pretty-printed when `pretty=true`.
pub fn render_diff_json(report: &DiffReport, pretty: bool) -> serde_json::Result<String> {
    if pretty {
        serde_json::to_string_pretty(report)
    } else {
        serde_json::to_string(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{
        AnalysisBackend, ContributionReason, MetricContribution, MetricKey, MetricSpace,
        MetricsReport, SourceSpan, SpaceId, SpaceKind,
    };

    fn report_with(language: Language, key: &str, value: f64) -> MetricsReport {
        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        root.metrics.insert(MetricKey::new(key), value);
        MetricsReport {
            schema_version: "1.0".to_string(),
            tool: "mehen".to_string(),
            path: "q.sql".into(),
            language,
            analysis_backend: AnalysisBackend::Sqruff,
            diagnostics: Vec::new(),
            root,
            contributions: Vec::new(),
        }
    }

    #[test]
    fn sql_report_exposes_flat_family_under_top_level_metrics() {
        // `render_metrics_json` must not pivot SQL/Markdown reports through the
        // source-code `MetricsFamilies` shape (which reads only `cyclomatic`,
        // `halstead.*`, … and would yield an all-zero block). Instead the
        // top-level `metrics` object is the flat `sql.*` map, so consumers
        // reading `.metrics["sql.cte.count"]` get the real value.
        let report = report_with(Language::Sql, "sql.cte.count", 3.0);
        let json = render_metrics_json(&report, false).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The flat family is exposed at the top-level `metrics` object …
        assert_eq!(value["metrics"]["sql.cte.count"], 3.0);
        // … and still present under `root.metrics`.
        assert_eq!(value["root"]["metrics"]["sql.cte.count"], 3.0);
        // No source-code family keys were injected.
        assert!(
            value["metrics"].get("cyclomatic").is_none(),
            "SQL report must not carry the source-code family block; got {value}"
        );
    }

    #[test]
    fn source_code_report_still_gets_family_pivot() {
        // Non-SQL/Markdown languages keep the documented per-family shape.
        let report = report_with(Language::Rust, "cyclomatic.sum", 5.0);
        let json = render_metrics_json(&report, false).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            value.get("metrics").is_some(),
            "source-code report must carry the pivoted `metrics` family block"
        );
    }

    #[test]
    fn contributions_are_serialized_only_when_present() {
        let empty = report_with(Language::Sql, "sql.change_risk_score", 0.0);
        let empty_json: serde_json::Value =
            serde_json::from_str(&render_metrics_json(&empty, false).unwrap()).unwrap();
        assert!(empty_json.get("contributions").is_none());

        let mut explained = report_with(Language::Sql, "sql.change_risk_score", 8.0);
        explained.contributions.push(MetricContribution {
            metric: MetricKey::new("sql.change_risk_score"),
            span: SourceSpan::new(0, 12, 1, 1),
            amount: 8.0,
            reason: ContributionReason::new("sql.change_risk.drop"),
        });
        let json: serde_json::Value =
            serde_json::from_str(&render_metrics_json(&explained, false).unwrap()).unwrap();
        assert_eq!(json["contributions"][0]["metric"], "sql.change_risk_score");
        assert_eq!(json["contributions"][0]["amount"], 8.0);
        assert_eq!(json["contributions"][0]["reason"], "sql.change_risk.drop");
        assert_eq!(json["contributions"][0]["span"]["start_line"], 1);
    }
}
