// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Per-family metric assertions with hand-verified expected values.
//!
//! These complement the snapshot suite: where snapshots catch *any* change,
//! these document *why* a metric has the value it does, so a regression names
//! the family it broke. Prior-art compatibility (research foundation §14.2):
//! CTE count, join count, subquery depth, CASE count, boolean operators,
//! window functions, CTE dependency depth, set operations, derived tables.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, MetricKey, SourceFile};
use mehen_sql::SqlAnalyzer;

fn metrics(sql: &str) -> mehen_core::MetricSet {
    let analyzer = SqlAnalyzer::new();
    let file = SourceFile::new("t.sql".into(), Language::Sql, sql.to_string());
    analyzer
        .analyze(&file, &AnalysisConfig::production())
        .expect("ok")
        .root
        .metrics
}

fn get(m: &mehen_core::MetricSet, key: &str) -> f64 {
    m.get(&MetricKey::new(key))
        .map(|v| v.as_f64())
        .unwrap_or(0.0)
}

#[test]
fn cte_count_and_dependency_depth() {
    // a -> b -> c chain: depth 3, 2 edges.
    let sql = "WITH a AS (SELECT 1 AS x), \
                    b AS (SELECT x FROM a), \
                    c AS (SELECT x FROM b) \
               SELECT x FROM c";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.cte.count"), 3.0);
    assert_eq!(get(&m, "sql.cte.dependency_edges"), 2.0);
    assert_eq!(get(&m, "sql.cte.max_dependency_depth"), 3.0);
    assert_eq!(get(&m, "sql.cte.unused_count"), 0.0);
}

#[test]
fn unused_cte_is_detected() {
    // `unused` is defined but never referenced.
    let sql = "WITH used AS (SELECT 1 AS x), \
                    unused AS (SELECT 2 AS y) \
               SELECT x FROM used";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.cte.count"), 2.0);
    assert_eq!(get(&m, "sql.cte.unused_count"), 1.0);
}

#[test]
fn join_count_and_kinds() {
    let sql = "SELECT * FROM a \
               INNER JOIN b ON a.id = b.id \
               LEFT JOIN c ON a.id = c.id \
               CROSS JOIN d";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.join.count"), 3.0);
    assert_eq!(get(&m, "sql.join.kind_count.inner"), 1.0);
    assert_eq!(get(&m, "sql.join.kind_count.left"), 1.0);
    assert_eq!(get(&m, "sql.join.kind_count.cross"), 1.0);
    assert_eq!(get(&m, "sql.join.outer_count"), 1.0);
    assert_eq!(get(&m, "sql.join.cross_count"), 1.0);
}

#[test]
fn case_count_and_nesting() {
    let sql = "SELECT CASE WHEN a > 1 THEN \
                          CASE WHEN b > 2 THEN 'x' ELSE 'y' END \
                      ELSE 'z' END \
               FROM t";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.case.count"), 2.0);
    assert_eq!(get(&m, "sql.case.max_depth"), 2.0);
}

#[test]
fn boolean_operator_count() {
    let sql = "SELECT * FROM t WHERE a = 1 AND b = 2 OR c = 3 AND d = 4";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.predicate.boolean_operator_count"), 3.0);
}

#[test]
fn window_function_count() {
    let sql = "SELECT ROW_NUMBER() OVER (PARTITION BY a ORDER BY b) AS rn, \
                      RANK() OVER (ORDER BY c) AS rk \
               FROM t";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.window.function_count"), 2.0);
}

#[test]
fn set_operation_count_and_union_all_ratio() {
    let sql = "SELECT a FROM x \
               UNION ALL SELECT a FROM y \
               UNION SELECT a FROM z";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.set_op.count"), 2.0);
    assert_eq!(get(&m, "sql.set_op.kind_count.union_all"), 1.0);
    assert_eq!(get(&m, "sql.set_op.kind_count.union"), 1.0);
    // 1 UNION ALL out of 2 union-family ops.
    assert!((get(&m, "sql.set_op.union_all_ratio") - 0.5).abs() < 1e-9);
}

#[test]
fn subquery_depth_and_count() {
    // Two levels of nesting in the FROM clause.
    let sql = "SELECT * FROM (SELECT * FROM (SELECT * FROM t) inner_q) outer_q";
    let m = metrics(sql);
    assert!(get(&m, "sql.subquery.count") >= 2.0);
    assert!(get(&m, "sql.subquery.max_depth") >= 2.0);
    assert_eq!(get(&m, "sql.derived_table.count"), 2.0);
}

#[test]
fn star_count_and_outer_star() {
    let sql = "SELECT * FROM (SELECT a, b FROM t) q";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.select.star_count"), 1.0);
    assert_eq!(get(&m, "sql.select.outer_star_count"), 1.0);
}

#[test]
fn destructive_dml_drives_change_risk() {
    let sql = "DROP TABLE t; TRUNCATE TABLE s; DELETE FROM u;";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.ddl.drop_count"), 1.0);
    assert_eq!(get(&m, "sql.ddl.truncate_count"), 1.0);
    assert_eq!(get(&m, "sql.dml.delete_without_where_count"), 1.0);
    // 8 (drop) + 8 (truncate) + 6 (delete-no-where) + write objects.
    assert!(get(&m, "sql.change_risk_score") >= 22.0);
}

#[test]
fn update_without_where_is_flagged() {
    let with_where = metrics("UPDATE t SET x = 1 WHERE id = 5");
    let without = metrics("UPDATE t SET x = 1");
    assert_eq!(get(&with_where, "sql.dml.update_without_where_count"), 0.0);
    assert_eq!(get(&without, "sql.dml.update_without_where_count"), 1.0);
}

#[test]
fn statement_kind_entropy_low_for_homogeneous() {
    let homogeneous = metrics("CREATE TABLE a (x INT); CREATE TABLE b (y INT);");
    // One distinct kind → entropy 0.
    assert_eq!(get(&homogeneous, "sql.statement.kind_entropy"), 0.0);

    let mixed = metrics("CREATE TABLE a (x INT); INSERT INTO a VALUES (1); DROP TABLE a;");
    assert!(get(&mixed, "sql.statement.kind_entropy") > 0.0);
}

#[test]
fn halstead_volume_grows_with_complexity() {
    let simple = metrics("SELECT a FROM t");
    let complex = metrics(
        "SELECT a, b, c, d FROM t JOIN u ON t.id = u.id \
         WHERE a > 1 AND b < 2 AND c = 3 GROUP BY a, b, c",
    );
    assert!(get(&complex, "sql.halstead.volume") > get(&simple, "sql.halstead.volume"));
    assert!(get(&simple, "sql.halstead.volume") > 0.0);
}

#[test]
fn statement_classification_ctas_merge_insert_select() {
    // CREATE TABLE AS SELECT classifies as create_table_as, not plain create.
    let ctas = metrics("CREATE TABLE s AS SELECT region, SUM(amt) AS t FROM sales GROUP BY region");
    assert_eq!(get(&ctas, "sql.statement.kind_count.create_table_as"), 1.0);
    assert_eq!(get(&ctas, "sql.statement.kind_count.create_table"), 0.0);
    assert_eq!(get(&ctas, "sql.object.write_count"), 1.0);
    // The embedded SELECT's aggregate + GROUP BY are still measured.
    assert_eq!(get(&ctas, "sql.aggregate.function_count"), 1.0);
    assert_eq!(get(&ctas, "sql.group_by.count"), 1.0);

    // INSERT ... SELECT is an insert (a write).
    let insert_select = metrics("INSERT INTO target SELECT * FROM source WHERE active");
    assert_eq!(get(&insert_select, "sql.statement.kind_count.insert"), 1.0);
    assert_eq!(get(&insert_select, "sql.object.write_count"), 1.0);

    // MERGE is its own kind and a write.
    let merge = metrics(
        "MERGE INTO accounts a USING updates u ON a.id = u.id \
         WHEN MATCHED THEN UPDATE SET a.bal = u.bal \
         WHEN NOT MATCHED THEN INSERT (id, bal) VALUES (u.id, u.bal)",
    );
    assert_eq!(get(&merge, "sql.dml.merge_count"), 1.0);
    assert_eq!(get(&merge, "sql.object.write_count"), 1.0);
}

#[test]
fn unqualified_column_ratio_in_multi_relation_scope() {
    // `id` is unqualified in a two-relation scope; `a.x`/`b.y` are qualified.
    let sql = "SELECT a.x, b.y, id FROM a JOIN b ON a.k = b.k";
    let m = metrics(sql);
    let ratio = get(&m, "sql.identifier.unqualified_column_ratio");
    assert!(ratio > 0.0 && ratio < 1.0, "ratio was {ratio}");
}
