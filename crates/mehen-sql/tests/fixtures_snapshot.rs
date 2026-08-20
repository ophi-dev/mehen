// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Golden-fixture snapshot tests (research foundation §14.1).
//!
//! Each fixture under `tests/fixtures/` is analyzed and its full `sql.*`
//! metric map snapshotted, so any change in metric output is reviewable as a
//! diff. The `MetricSet` serializes as an ordered map, so snapshots are
//! deterministic without an explicit sort.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_sql::SqlAnalyzer;

fn analyze(name: &str, source: &str) -> mehen_core::LanguageAnalysis {
    let analyzer = SqlAnalyzer::new();
    let file = SourceFile::new(
        format!("{name}.sql").into(),
        Language::Sql,
        source.to_string(),
    );
    analyzer
        .analyze(&file, &AnalysisConfig::production())
        .expect("analysis ok")
}

macro_rules! fixture_snapshot {
    ($test:ident, $file:literal) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!("fixtures/", $file, ".sql"));
            let analysis = analyze($file, source);
            // Snapshot the full metric map. Backend + diagnostics are part of
            // the contract too, so include them. `sort_maps` makes the JSON
            // object key order deterministic so `cargo insta --check` (which
            // compares against the on-disk, key-sorted `.snap`) matches the
            // in-memory `serde_json::json!` insertion order.
            insta::with_settings!({sort_maps => true}, {
                insta::assert_json_snapshot!(
                    $file,
                    serde_json::json!({
                        "backend": analysis.backend.label(),
                        "diagnostics": analysis
                            .diagnostics
                            .iter()
                            .map(|d| d.code.clone())
                            .collect::<Vec<_>>(),
                        "metrics": &analysis.root.metrics,
                        "spaces": analysis
                            .root
                            .spaces
                            .iter()
                            .map(|s| serde_json::json!({
                                "kind": s.kind.as_str(),
                                "name": s.name,
                                "start_line": s.span.start_line,
                                "end_line": s.span.end_line,
                            }))
                            .collect::<Vec<_>>(),
                    })
                );
            });
        }
    };
}

fixture_snapshot!(simple_select, "simple_select");
fixture_snapshot!(analytics_cte_chain, "analytics_cte_chain");
fixture_snapshot!(migration_destructive, "migration_destructive");
fixture_snapshot!(correlated_subquery, "correlated_subquery");
fixture_snapshot!(set_ops_unions, "set_ops_unions");
fixture_snapshot!(dialect_directive, "dialect_directive");
fixture_snapshot!(plsql_procedure_control_flow, "plsql_procedure_control_flow");
fixture_snapshot!(tsql_procedure_control_flow, "tsql_procedure_control_flow");
