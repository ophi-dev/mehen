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
/// source-code families (Markdown's `markdown.*`, SQL's `sql.*`) are
/// exempt from the pivot: `MetricsFamilies::from_metrics` only reads the
/// source-code keys (`cyclomatic`, `halstead.*`, …), so pivoting their
/// reports would replace the real `markdown.*`/`sql.*` values under
/// `metrics` with an all-zero source-code block. For those languages the
/// serialized `metrics` map (from `root.metrics`) is left untouched.
pub fn render_metrics_json(report: &MetricsReport, pretty: bool) -> serde_json::Result<String> {
    let value = serde_json::to_value(report)?;
    let value = if publishes_own_family(report.language) {
        value
    } else {
        let mut value = value;
        let families = serde_json::to_value(MetricsFamilies::from_metrics(&report.root.metrics))?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert("metrics".to_string(), families);
        }
        value
    };
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
        AnalysisBackend, MetricKey, MetricSpace, MetricsReport, SourceSpan, SpaceId, SpaceKind,
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
        }
    }

    #[test]
    fn sql_report_preserves_flat_family_and_skips_source_code_pivot() {
        // Regression: `render_metrics_json` used to unconditionally pivot
        // every report through `MetricsFamilies::from_metrics`, which reads
        // only source-code keys (`cyclomatic`, `halstead.*`, …). For SQL —
        // which publishes its own `sql.*` family — that replaced the real
        // metric map under `metrics` with an all-zero source-code block.
        let report = report_with(Language::Sql, "sql.cte.count", 3.0);
        let json = render_metrics_json(&report, false).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The real `sql.*` value survives under `root.metrics`.
        assert_eq!(value["root"]["metrics"]["sql.cte.count"], 3.0);
        // No misleading all-zero source-code `metrics` block was inserted.
        assert!(
            value.get("metrics").is_none(),
            "SQL report must not carry a pivoted source-code `metrics` block; got {value}"
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
}
