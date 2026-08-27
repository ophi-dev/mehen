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

/// Body increments that sqruff spills outside the routine's parsed range
/// (split sibling statements, top-level `Unparsable` runs) attribute to the
/// routine they continue — the T-SQL fixture's function space carries the
/// file's whole cyclomatic score, not just its entry (Codex P2, PR #257
/// round 2).
#[test]
fn tsql_spilled_body_attributes_to_its_routine() {
    let analysis = analyze(include_str!("fixtures/tsql_procedure_control_flow.sql"));
    // The routine's span extends through its spilled body regions (PR #257
    // round 8), outgrowing the header fragment's statement space — the
    // Function space sits at root level with the full-body span.
    let unit = analysis
        .root
        .spaces
        .iter()
        .find(|s| s.kind == SpaceKind::Function)
        .expect("routine Function space");
    assert_eq!(unit.name.as_deref(), Some("dbo.process_orders"));
    assert!(unit.span.end_line >= 31, "span covers the spilled body");
    let unit_cyclo = unit
        .metrics
        .get(&MetricKey::new("sql.procedural.cyclomatic_complexity"))
        .map(|v| v.as_f64())
        .expect("unit cyclomatic");
    let file_cyclo = analysis
        .root
        .metrics
        .get(&MetricKey::new("sql.procedural.cyclomatic_complexity"))
        .map(|v| v.as_f64())
        .expect("file cyclomatic");
    // Everything in the file belongs to the single routine.
    assert_eq!(unit_cyclo, file_cyclo);
    assert_eq!(unit_cyclo, 7.0);
}

/// A T-SQL routine whose body sqruff splits into sibling statements keeps a
/// `Function` space covering the *whole* body: continuation regions
/// attributed to the routine extend its span past the header fragment the
/// parser kept inside the definition node (Codex P1, PR #257 round 8).
/// When the extended span outgrows its host statement space the Function
/// space surfaces at the root instead — a full-scope space beats a nested
/// but truncated one for location-based consumers.
#[test]
fn tsql_split_body_extends_the_function_space() {
    let sql = "-- sqlfluff:dialect:tsql\n\
               create procedure dbo.split_me @x int as\n\
               begin\n\
                 declare @c int = 0;\n\
               \n\
                 if @x > 0\n\
                 begin\n\
                   set @c = 1;\n\
                 end\n\
               end\n";
    let spaces = tree(sql);
    let function = spaces
        .iter()
        .find(|(_, kind, _, _, _)| kind == "function")
        .expect("routine yields a Function space");
    assert_eq!(function.2.as_deref(), Some("dbo.split_me"));
    // Header fragment ends at line 4; the body continuation runs to the
    // trailing END. The space must cover the continuation.
    assert!(
        function.4 >= 9,
        "Function space ends at line {}, expected the full body",
        function.4
    );
}

/// Embedded-query scores follow innermost ownership: a nested subprogram's
/// query belongs to the nested unit alone, so an outer routine with no
/// query of its own scores 0 and cannot outrank its child (Codex P2,
/// PR #257 round 9).
#[test]
fn nested_routine_owns_its_embedded_queries() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               create or replace procedure outer_p is\n\
               \x20 function inner_f return number is\n\
               \x20   v number;\n\
               \x20 begin\n\
               \x20   select count(*) into v from orders o join lines l on l.oid = o.id;\n\
               \x20   return v;\n\
               \x20 end inner_f;\n\
               begin\n\
               \x20 null;\n\
               end outer_p;\n\
               /\n";
    let analysis = analyze(sql);
    let get = |space: &mehen_core::MetricSpace, key: &str| {
        space
            .metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or_else(|| panic!("missing {key}"))
    };
    let statement = &analysis.root.spaces[0];
    let outer = &statement.spaces[0];
    assert_eq!(outer.name.as_deref(), Some("outer_p"));
    let inner = &outer.spaces[0];
    assert_eq!(inner.name.as_deref(), Some("inner_f"));
    // The join-bearing SELECT scores on the inner unit…
    assert!(get(inner, "sql.structural_complexity") > 0.0);
    // …and not on the outer routine, which has no query of its own.
    assert_eq!(get(outer, "sql.structural_complexity"), 0.0);
}

/// A recovered package-body member ending with the common unnamed `END;`
/// still gets a bounded span: the initialization section's control flow
/// stays file-level instead of attributing to the last member (Codex P2,
/// PR #257 round 20).
#[test]
fn unnamed_end_bounds_a_recovered_member() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               create package body pkg as\n\
               \x20 procedure p is\n\
               \x20 begin\n\
               \x20   null;\n\
               \x20 end;\n\
               begin\n\
               \x20 if 1 = 1 then\n\
               \x20   null;\n\
               \x20 end if;\n\
               end pkg;\n\
               /\n";
    let analysis = analyze(sql);
    let member = analysis
        .root
        .spaces
        .iter()
        .flat_map(|s| std::iter::once(s).chain(s.spaces.iter()))
        .find(|s| s.kind == SpaceKind::Function)
        .expect("member Function space");
    assert_eq!(member.name.as_deref(), Some("p"));
    // The member ends at its own END; — before the init section's BEGIN.
    assert!(member.span.end_line <= 6, "member ends at line 6");
    // Its entry only: the init section's IF is file-level.
    let cyclo = member
        .metrics
        .get(&MetricKey::new("sql.procedural.cyclomatic_complexity"))
        .map(|v| v.as_f64())
        .expect("member cyclomatic");
    assert_eq!(cyclo, 1.0);
}

/// A declaration-level `CASE … END` initializer doesn't terminate a
/// recovered member early: only its executable `BEGIN` arms termination
/// (Codex P2, PR #257 round 21).
#[test]
fn declaration_case_does_not_truncate_a_recovered_member() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               create package body pkg as\n\
               \x20 procedure p is\n\
               \x20   x number := case when 1 = 1 then 1 else 0 end;\n\
               \x20 begin\n\
               \x20   null;\n\
               \x20 end;\n\
               end pkg;\n\
               /\n";
    let analysis = analyze(sql);
    let member = analysis
        .root
        .spaces
        .iter()
        .flat_map(|s| std::iter::once(s).chain(s.spaces.iter()))
        .find(|s| s.kind == SpaceKind::Function)
        .expect("member Function space");
    assert_eq!(member.name.as_deref(), Some("p"));
    // The member runs through its real body END (line 7), not the
    // declaration initializer's END (line 3).
    assert!(member.span.end_line >= 7, "member covers its body");
}

/// A nested subprogram inside a recovered member doesn't truncate the
/// outer span: both become units, the outer containing the inner
/// (Codex P2, PR #257 round 21).
#[test]
fn nested_recovered_member_keeps_the_outer_span() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               create package body pkg as\n\
               \x20 procedure outer_p is\n\
               \x20   procedure inner_p is\n\
               \x20   begin\n\
               \x20     null;\n\
               \x20   end;\n\
               \x20 begin\n\
               \x20   null;\n\
               \x20 end;\n\
               end pkg;\n\
               /\n";
    let analysis = analyze(sql);
    let mut functions: Vec<(String, u32, u32)> = Vec::new();
    fn walk(space: &mehen_core::MetricSpace, out: &mut Vec<(String, u32, u32)>) {
        if space.kind == SpaceKind::Function {
            out.push((
                space.name.clone().unwrap_or_default(),
                space.span.start_line,
                space.span.end_line,
            ));
        }
        for child in &space.spaces {
            walk(child, out);
        }
    }
    for space in &analysis.root.spaces {
        walk(space, &mut functions);
    }
    assert_eq!(functions.len(), 2, "outer and inner members: {functions:?}");
    let outer = functions.iter().find(|(n, _, _)| n == "outer_p").unwrap();
    let inner = functions.iter().find(|(n, _, _)| n == "inner_p").unwrap();
    // The outer member covers its own body END (line 10), past the inner.
    assert!(outer.2 >= 10, "outer spans through its body: {outer:?}");
    assert!(inner.2 <= 7, "inner ends at its own END: {inner:?}");
}
