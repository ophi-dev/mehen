// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Procedural-unit space extraction: routine definitions become
//! `SpaceKind::Function` spaces nested under their `sql.statement` space,
//! with package-body routines and DECLARE-section subprograms nested by
//! containment. These are the scopes per-function coverage enrichment
//! annotates (engine `inject_into_children` targets Function/Closure kinds,
//! recursing through the statement layer).

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, MetricKey, SourceFile, SpaceKind};
use mehen_sql::SqlAnalyzer;

fn analyze(sql: &str) -> mehen_core::LanguageAnalysis {
    SqlAnalyzer::new()
        .analyze(
            &SourceFile::new("t.sql".into(), Language::Sql, sql.to_string()),
            &AnalysisConfig::production(),
        )
        .expect("analysis ok")
}

/// Flatten `(depth, kind, name, start_line, end_line)` for assertion
/// convenience.
fn flatten(
    space: &mehen_core::MetricSpace,
    depth: usize,
    out: &mut Vec<(usize, String, Option<String>, u32, u32)>,
) {
    out.push((
        depth,
        space.kind.as_str().to_string(),
        space.name.clone(),
        space.span.start_line,
        space.span.end_line,
    ));
    for child in &space.spaces {
        flatten(child, depth + 1, out);
    }
}

fn tree(sql: &str) -> Vec<(usize, String, Option<String>, u32, u32)> {
    let analysis = analyze(sql);
    let mut out = Vec::new();
    for space in &analysis.root.spaces {
        flatten(space, 0, &mut out);
    }
    out
}

#[test]
fn oracle_standalone_function_nests_under_its_statement() {
    let sql = "create or replace function betwnstr(a_string varchar2, a_start_pos integer, a_end_pos integer)\n  return varchar2 is l_start_pos pls_integer := greatest(a_start_pos, 1);\nbegin\n  if a_end_pos is null then\n    return null;\n  end if;\n  return substr(a_string, l_start_pos, a_end_pos - l_start_pos + 1);\nend betwnstr;\n/\n";
    let spaces = tree(sql);
    assert_eq!(
        spaces,
        vec![
            (0, "sql.statement".into(), Some("procedural".into()), 1, 8),
            (1, "function".into(), Some("betwnstr".into()), 1, 8),
        ]
    );
}

#[test]
fn oracle_package_body_routines_are_function_spaces() {
    let sql = "-- sqlfluff:dialect:oracle\ncreate or replace package body pkg_demo is\n  function get_a return number is\n  begin\n    return 1;\n  end get_a;\n\n  procedure set_b(p number) is\n  begin\n    null;\n  end set_b;\nend pkg_demo;\n/\n";
    let spaces = tree(sql);
    assert_eq!(
        spaces,
        vec![
            (0, "sql.statement".into(), Some("procedural".into()), 2, 12),
            (1, "function".into(), Some("get_a".into()), 3, 6),
            (1, "function".into(), Some("set_b".into()), 8, 11),
        ]
    );
}

#[test]
fn oracle_nested_subprogram_nests_inside_its_parent() {
    let sql = "-- sqlfluff:dialect:oracle\ncreate or replace function outer_fn return number is\n  v number;\n  function inner_fn return number is\n  begin\n    return 2;\n  end inner_fn;\nbegin\n  v := inner_fn();\n  return v;\nend outer_fn;\n/\n";
    let spaces = tree(sql);
    assert_eq!(
        spaces,
        vec![
            (0, "sql.statement".into(), Some("procedural".into()), 2, 11),
            (1, "function".into(), Some("outer_fn".into()), 2, 11),
            (2, "function".into(), Some("inner_fn".into()), 4, 7),
        ]
    );
}

#[test]
fn tsql_procedure_takes_its_object_reference_name() {
    let sql = "-- sqlfluff:dialect:tsql\ncreate procedure dbo.do_thing @x int as\nbegin\n  select @x + 1;\nend\n";
    let spaces = tree(sql);
    assert_eq!(
        spaces,
        vec![
            (0, "sql.statement".into(), Some("procedural".into()), 2, 5),
            (1, "function".into(), Some("dbo.do_thing".into()), 2, 5),
        ]
    );
}

#[test]
fn postgres_function_with_opaque_body_still_spans_whole_statement() {
    let sql = "-- sqlfluff:dialect:postgres\ncreate or replace function add_one(i integer) returns integer as $$\nbegin\n  return i + 1;\nend;\n$$ language plpgsql;\n";
    let spaces = tree(sql);
    assert_eq!(
        spaces,
        vec![
            (0, "sql.statement".into(), Some("procedural".into()), 2, 6),
            (1, "function".into(), Some("add_one".into()), 2, 6),
        ]
    );
}

#[test]
fn plain_dml_produces_no_function_spaces() {
    let analysis = analyze("select a from t;\ninsert into t (a) values (1);\n");
    for space in &analysis.root.spaces {
        assert_eq!(space.kind, SpaceKind::Custom("sql.statement".into()));
        assert!(space.spaces.is_empty(), "DML must not grow function spaces");
    }
}

/// Oracle routines classify as `procedural` — before the Oracle statement
/// kinds joined `PROCEDURAL_DEFINITIONS`, an Oracle `CREATE FUNCTION`
/// classified as `create_other` and DML inside routine bodies leaked into
/// the object-touch scans.
#[test]
fn oracle_routine_classifies_procedural_and_body_dml_does_not_leak() {
    let sql = "-- sqlfluff:dialect:oracle\ncreate or replace procedure upd(p number) is\nbegin\n  update accounts set bal = bal - p;\nend upd;\n/\n";
    let analysis = analyze(sql);
    let get = |key: &str| {
        analysis
            .root
            .metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or_else(|| panic!("missing metric {key}"))
    };
    assert_eq!(get("sql.statement.kind_count.procedural"), 1.0);
    assert_eq!(get("sql.statement.kind_count.create_other"), 0.0);
    // The UPDATE inside the routine body must not count as a file-level
    // write touch (Phase 1 does not analyze routine bodies).
    assert_eq!(get("sql.object.write_count"), 0.0);
    assert_eq!(get("sql.dml.update_count"), 0.0);
}

/// SpaceIds stay unique and sequential across the statement + unit layers.
#[test]
fn space_ids_are_unique_across_statements_and_units() {
    let sql =
        "select 1;\ncreate or replace function f return number is\nbegin\n  return 1;\nend f;\n/\n";
    let analysis = analyze(sql);
    let mut ids = Vec::new();
    fn collect(space: &mehen_core::MetricSpace, ids: &mut Vec<u32>) {
        ids.push(space.id.0);
        for child in &space.spaces {
            collect(child, ids);
        }
    }
    collect(&analysis.root, &mut ids);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "duplicate SpaceId: {ids:?}");
}

/// Per-routine procedural composites land on the unit's Function space
/// (Phase 3): the file-level aggregates attribute to the innermost unit
/// containing each increment, plus the unit's own entry path.
#[test]
fn function_spaces_carry_per_unit_procedural_metrics() {
    let analysis = analyze(include_str!("fixtures/plsql_procedure_control_flow.sql"));
    let statement = &analysis.root.spaces[0];
    let unit = &statement.spaces[0];
    assert_eq!(unit.kind, SpaceKind::Function);
    let get = |key: &str| {
        unit.metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or_else(|| panic!("missing unit metric {key}"))
    };
    // The single routine owns every increment (hand trace in
    // tests/metrics.rs::plsql_procedural_family_counts).
    assert_eq!(get("sql.procedural.cyclomatic_complexity"), 12.0);
    assert_eq!(get("sql.procedural.cognitive_complexity"), 9.0);
    // The embedded UPDATE…WHERE gives a small query-structural score, and the
    // file-level max is exactly this unit's score.
    let embedded = get("sql.structural_complexity");
    assert!(embedded > 0.0);
    assert_eq!(
        analysis
            .root
            .metrics
            .get(&MetricKey::new(
                "sql.structural_complexity.max_embedded_query"
            ))
            .map(|v| v.as_f64()),
        Some(embedded)
    );
}

/// Subprograms nested in a container split the attribution: each unit gets
/// its own entry, and increments land in the *innermost* enclosing unit.
#[test]
fn nested_subprogram_attribution_is_innermost() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               create or replace function outer_fn return number is\n\
                 v number;\n\
                 function inner_fn return number is\n\
                 begin\n\
                   if v > 0 then\n\
                     return 2;\n\
                   end if;\n\
                   return 3;\n\
                 end inner_fn;\n\
               begin\n\
                 v := inner_fn();\n\
                 return v;\n\
               end outer_fn;\n\
               /\n";
    let analysis = analyze(sql);
    let statement = &analysis.root.spaces[0];
    let outer = &statement.spaces[0];
    let inner = &outer.spaces[0];
    let get = |space: &mehen_core::MetricSpace, key: &str| {
        space
            .metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or_else(|| panic!("missing {key}"))
    };
    // Inner: entry 1 + IF 1 = 2. Its RETURN statements and the IF belong to
    // it, not to outer_fn.
    assert_eq!(get(inner, "sql.procedural.cyclomatic_complexity"), 2.0);
    // Outer: its own entry only (the machine increments inside inner_fn's
    // byte range attribute to inner_fn).
    assert_eq!(get(outer, "sql.procedural.cyclomatic_complexity"), 1.0);
    // File-level = 3 = both entries + the IF.
    assert_eq!(
        analysis
            .root
            .metrics
            .get(&MetricKey::new("sql.procedural.cyclomatic_complexity"))
            .map(|v| v.as_f64()),
        Some(3.0)
    );
}
