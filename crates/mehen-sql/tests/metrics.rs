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

/// Strict metric lookup: panics if the key is absent. Every `sql.*` key is
/// published unconditionally (with an explicit `0` when not applicable), so a
/// missing key here means a dropped/renamed metric or a typo in the test —
/// failing fast surfaces that instead of silently reading `0.0`.
fn get(m: &mehen_core::MetricSet, key: &str) -> f64 {
    m.get(&MetricKey::new(key))
        .unwrap_or_else(|| panic!("missing metric key: {key}"))
        .as_f64()
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

// ── review-fix regressions ────────────────────────────────────────────

#[test]
fn cast_count_handles_both_forms_once_each() {
    // Shorthand `::` cast is one CastExpression (must not be double-counted via
    // a `::` substring); SQL-standard CAST(...) is a function (must be counted).
    assert_eq!(get(&metrics("SELECT b::int FROM t"), "sql.cast.count"), 1.0);
    assert_eq!(
        get(&metrics("SELECT CAST(a AS int) FROM t"), "sql.cast.count"),
        1.0
    );
    assert_eq!(
        get(
            &metrics("SELECT b::int, CAST(a AS int) FROM t"),
            "sql.cast.count"
        ),
        2.0
    );
}

#[test]
fn read_count_is_per_object_not_per_statement() {
    // A query touching three tables reads three objects, not one.
    let m = metrics("SELECT * FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id");
    assert_eq!(get(&m, "sql.object.read_count"), 3.0);
    assert_eq!(get(&m, "sql.object.touch_count"), 3.0);
}

#[test]
fn touch_count_dedups_read_and_write_of_same_object() {
    // `t` is both read (FROM) and written (UPDATE target) → touched once.
    let m = metrics("UPDATE t SET v = 1 WHERE id IN (SELECT id FROM t WHERE flagged)");
    assert_eq!(get(&m, "sql.object.write_count"), 1.0);
    assert_eq!(get(&m, "sql.object.read_count"), 1.0);
    assert_eq!(get(&m, "sql.object.touch_count"), 1.0);
}

#[test]
fn merge_using_source_is_a_read_not_a_write() {
    let m = metrics(
        "MERGE INTO accounts a USING updates u ON a.id = u.id \
         WHEN MATCHED THEN UPDATE SET a.bal = u.bal",
    );
    assert_eq!(get(&m, "sql.object.write_count"), 1.0); // accounts
    assert_eq!(get(&m, "sql.object.read_count"), 1.0); // updates
}

#[test]
fn table_alias_count_excludes_output_aliases() {
    // `total` is an output alias, not a table alias; `c` is a table alias.
    let m = metrics("SELECT SUM(x) AS total FROM customers c");
    assert_eq!(get(&m, "sql.alias.table_alias_count"), 1.0);
}

#[test]
fn update_without_where_ignores_subquery_where() {
    // No statement-level WHERE — rewrites every row — even though the scalar
    // subquery has its own WHERE (Codex P1).
    let m = metrics("UPDATE t SET v = (SELECT v FROM u WHERE u.id = t.id)");
    assert_eq!(get(&m, "sql.dml.update_without_where_count"), 1.0);
    // With a real statement-level WHERE it is not flagged.
    let guarded = metrics("UPDATE t SET v = (SELECT v FROM u WHERE u.id = t.id) WHERE t.active");
    assert_eq!(get(&guarded, "sql.dml.update_without_where_count"), 0.0);
}

#[test]
fn trivial_cte_is_detected() {
    // `a` just renames a source (trivial); `b` filters (non-trivial).
    let m = metrics(
        "WITH a AS (SELECT * FROM src), \
              b AS (SELECT * FROM other WHERE x > 1) \
         SELECT * FROM a JOIN b ON a.id = b.id",
    );
    assert_eq!(get(&m, "sql.cte.trivial_count"), 1.0);
}

#[test]
fn boolean_depth_not_inflated_by_redundant_parens() {
    let depth = |sql: &str| get(&metrics(sql), "sql.predicate.max_boolean_depth");
    // A flat predicate and a single (redundant) outer bracket are both depth 1.
    assert_eq!(depth("SELECT * FROM t WHERE a AND b"), 1.0);
    assert_eq!(depth("SELECT * FROM t WHERE (a OR b)"), 1.0);
    // One nested boolean group adds a level.
    assert_eq!(depth("SELECT * FROM t WHERE a AND (b OR c)"), 2.0);
    // Two nested levels.
    assert_eq!(depth("SELECT * FROM t WHERE a AND (b OR (c AND d))"), 3.0);
    // Sibling bracketed groups under a top-level AND are depth 2, not 3.
    assert_eq!(depth("SELECT * FROM t WHERE (a OR b) AND (c OR d)"), 2.0);
}

#[test]
fn join_kind_uses_keywords_not_relation_name() {
    // A relation literally named `left_table` in a plain INNER join must not
    // be classified as a LEFT join.
    let m = metrics("SELECT * FROM a JOIN left_table ON a.id = left_table.id");
    assert_eq!(get(&m, "sql.join.kind_count.inner"), 1.0);
    assert_eq!(get(&m, "sql.join.kind_count.left"), 0.0);
    assert_eq!(get(&m, "sql.join.outer_count"), 0.0);
}

#[test]
fn inequality_join_is_non_equi() {
    // A range join (`>=`) has no equality condition → non-equi.
    let range = metrics("SELECT * FROM a JOIN b ON a.ts >= b.start_ts");
    assert_eq!(get(&range, "sql.join.non_equi_count"), 1.0);
    // `!=` likewise.
    let neq = metrics("SELECT * FROM a JOIN b ON a.id != b.id");
    assert_eq!(get(&neq, "sql.join.non_equi_count"), 1.0);
    // A genuine equality join is not flagged.
    let equi = metrics("SELECT * FROM a JOIN b ON a.id = b.id");
    assert_eq!(get(&equi, "sql.join.non_equi_count"), 0.0);
    // USING is inherently equality.
    let using = metrics("SELECT * FROM a JOIN b USING (id)");
    assert_eq!(get(&using, "sql.join.non_equi_count"), 0.0);
}

#[test]
fn block_comment_interior_is_not_code() {
    // The middle line of a multi-line block comment has no marker but is fully
    // inside the comment span — it must count as comment, not code.
    let sql = "/* line one\n   explain why this query exists\n   line three */\nSELECT 1;\n";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.loc.code"), 1.0, "only the SELECT line is code");
    assert_eq!(get(&m, "sql.loc.comment"), 3.0, "all 3 comment lines");
}

#[test]
fn joined_derived_table_is_not_an_in_subquery() {
    // `JOIN (SELECT …)` must not be mis-counted as an `IN (SELECT …)` predicate
    // (the substring `IN (SELECT` appears inside `JOIN (SELECT`).
    let m = metrics("SELECT * FROM a JOIN (SELECT id FROM b) q ON a.id = q.id");
    assert_eq!(get(&m, "sql.subquery.in_count"), 0.0);
    // A genuine IN subquery is still counted.
    let real = metrics("SELECT * FROM a WHERE a.id IN (SELECT id FROM b)");
    assert_eq!(get(&real, "sql.subquery.in_count"), 1.0);
    // EXISTS likewise detected from keyword + bracketed SELECT.
    let exists = metrics("SELECT * FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)");
    assert_eq!(get(&exists, "sql.subquery.exists_count"), 1.0);
}

#[test]
fn null_comparison_risk_counted_once() {
    // `!= NULL` / `<> NULL` must count once each, not also as `= NULL`.
    assert_eq!(
        get(
            &metrics("SELECT * FROM t WHERE x != NULL"),
            "sql.predicate.null_semantics_risk_count"
        ),
        1.0
    );
    assert_eq!(
        get(
            &metrics("SELECT * FROM t WHERE x <> NULL"),
            "sql.predicate.null_semantics_risk_count"
        ),
        1.0
    );
    assert_eq!(
        get(
            &metrics("SELECT * FROM t WHERE x = NULL"),
            "sql.predicate.null_semantics_risk_count"
        ),
        1.0
    );
    // `IS NULL` is the safe form — not a risk.
    assert_eq!(
        get(
            &metrics("SELECT * FROM t WHERE x IS NULL"),
            "sql.predicate.null_semantics_risk_count"
        ),
        0.0
    );
}

#[test]
fn fully_qualified_local_ref_is_not_correlated() {
    // A fully-qualified reference to the subquery's own table (`schema.t.id`)
    // must not be misread as an outer/correlated reference.
    let local = metrics(
        "SELECT * FROM outer_t o \
         WHERE o.id IN (SELECT s.inner_t.id FROM s.inner_t WHERE s.inner_t.active)",
    );
    assert_eq!(get(&local, "sql.subquery.correlated_count"), 0.0);
    // A genuine outer reference (`o.grp`) is still detected as correlated.
    let correlated = metrics(
        "SELECT * FROM outer_t o \
         WHERE o.x > (SELECT AVG(i.x) FROM inner_t i WHERE i.grp = o.grp)",
    );
    assert_eq!(get(&correlated, "sql.subquery.correlated_count"), 1.0);
}

#[test]
fn cte_with_attached_to_ddl_classifies_as_ctas() {
    // A CTE attached to CREATE TABLE AS must classify as create_table_as, not
    // with_select.
    let m = metrics("CREATE TABLE dst AS WITH c AS (SELECT 1 AS x) SELECT x FROM c");
    assert_eq!(get(&m, "sql.statement.kind_count.create_table_as"), 1.0);
    assert_eq!(get(&m, "sql.statement.kind_count.with_select"), 0.0);
    // A standalone WITH … SELECT is still with_select.
    let plain = metrics("WITH c AS (SELECT 1 AS x) SELECT x FROM c");
    assert_eq!(get(&plain, "sql.statement.kind_count.with_select"), 1.0);
}

#[test]
fn outer_apply_is_not_a_missing_condition_join() {
    // CROSS/OUTER APPLY (T-SQL) and LATERAL legitimately omit ON/USING.
    let m = metrics("SELECT * FROM t OUTER APPLY fn(t.id) AS x");
    assert_eq!(get(&m, "sql.join.missing_condition_count"), 0.0);
    assert!(get(&m, "sql.join.kind_count.lateral") >= 1.0);
}

#[test]
fn returning_clause_counts_in_dml() {
    // Postgres/Oracle RETURNING inside an INSERT/UPDATE/DELETE counts. (T-SQL
    // OUTPUT also counts when sqruff parses it into a DML statement — but its
    // exact grammar support varies, so we only assert the reliably-parsed
    // RETURNING form here; the comment-exclusion guard below covers OUTPUT.)
    let insert = metrics("INSERT INTO t (a) VALUES (1) RETURNING a");
    assert_eq!(get(&insert, "sql.dml.returning_count"), 1.0);
    let update = metrics("UPDATE t SET a = 1 WHERE id = 2 RETURNING a");
    assert_eq!(get(&update, "sql.dml.returning_count"), 1.0);
    let delete = metrics("DELETE FROM t WHERE id = 2 RETURNING a");
    assert_eq!(get(&delete, "sql.dml.returning_count"), 1.0);
}

#[test]
fn cte_dependency_dedups_repeated_refs() {
    // `b` references `a` twice; that is one dependency edge, not two.
    let m = metrics(
        "WITH a AS (SELECT 1 AS x), \
              b AS (SELECT a1.x FROM a a1 JOIN a a2 ON a1.x = a2.x) \
         SELECT x FROM b",
    );
    assert_eq!(get(&m, "sql.cte.dependency_edges"), 1.0);
    assert_eq!(get(&m, "sql.cte.max_fan_out"), 1.0);
}

#[test]
fn halstead_treats_qualified_column_as_one_operand() {
    // `t.id` is one operand, not two (`t` + `id`). A query referencing only
    // `t.id` (plus the table) should have a small distinct-operand count.
    let m = metrics("SELECT t.id FROM t");
    // operands: `t.id` (column ref) and `t` (table ref) = 2 distinct.
    assert_eq!(get(&m, "sql.halstead.distinct_operands"), 2.0);
}

#[test]
fn parse_error_still_publishes_full_metric_surface() {
    // A hard parse error must still publish the zeroed sql.* surface so
    // downstream selectors/thresholds find their keys.
    use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
    use mehen_sql::SqlAnalyzer;
    let analysis = SqlAnalyzer::new()
        .analyze(
            &SourceFile::new(
                "broken.sql".into(),
                Language::Sql,
                "SELECT FROM WHERE ;;; garbage (((".to_string(),
            ),
            &AnalysisConfig::production(),
        )
        .expect("analysis returns Ok with diagnostics");
    // Full surface present (e.g. statement count + a composite key).
    assert!(
        analysis
            .root
            .metrics
            .get(&MetricKey::new("sql.statement.count"))
            .is_some()
    );
    assert!(
        analysis
            .root
            .metrics
            .get(&MetricKey::new("sql.structural_complexity"))
            .is_some()
    );
}

#[test]
fn quoted_identifiers_resolve_in_qualifier_and_alias() {
    // Quoted table alias + quoted-qualified column should be measured.
    let m = metrics(r#"SELECT "t".id FROM tbl "t""#);
    // The alias `"t"` is a table alias.
    assert_eq!(get(&m, "sql.alias.table_alias_count"), 1.0);
}

#[test]
fn comment_only_file_counts_loc() {
    // A comment-only file (sqruff returns no parse tree) must still report
    // physical/comment/blank lines, not all zeros.
    let m = metrics("-- migration note\n-- second line\n");
    assert_eq!(get(&m, "sql.loc.physical"), 2.0);
    assert_eq!(get(&m, "sql.loc.comment"), 2.0);
    assert_eq!(get(&m, "sql.loc.code"), 0.0);
}

#[test]
fn returning_matches_across_newlines() {
    // RETURNING on its own line (whitespace, not a literal space, between
    // keyword and expression) must still be counted — the detection works over
    // the DML statement's code-token stream, so token adjacency, not literal
    // spacing, is what matters.
    // `::` cast steers inference to postgres so RETURNING parses into the
    // INSERT (RETURNING is postgres/oracle-specific; ANSI rejects it).
    let pg = metrics("INSERT INTO t (a) VALUES (1::int)\nRETURNING id");
    assert_eq!(get(&pg, "sql.dml.returning_count"), 1.0);
}

#[test]
fn equi_join_requires_column_to_column_equality() {
    // Real equi-join: column = column.
    assert_eq!(
        get(
            &metrics("SELECT * FROM a JOIN b ON a.id = b.id"),
            "sql.join.non_equi_count"
        ),
        0.0
    );
    // Constant/filter equalities are NOT equi-joins.
    assert_eq!(
        get(
            &metrics("SELECT * FROM a JOIN b ON 1 = 1"),
            "sql.join.non_equi_count"
        ),
        1.0
    );
    assert_eq!(
        get(
            &metrics("SELECT * FROM a JOIN b ON a.status = 'active'"),
            "sql.join.non_equi_count"
        ),
        1.0
    );
}

#[test]
fn create_or_replace_matches_across_whitespace() {
    let m = metrics("CREATE\nOR REPLACE VIEW v AS SELECT 1 FROM t");
    assert_eq!(get(&m, "sql.ddl.create_or_replace_count"), 1.0);
}

#[test]
fn returning_in_comment_is_not_counted() {
    // A comment mentioning RETURNING/OUTPUT must not increment the metric.
    let m = metrics("SELECT 1 FROM t; -- RETURNING id\n");
    assert_eq!(get(&m, "sql.dml.returning_count"), 0.0);
}

#[test]
fn column_named_output_is_not_a_returning_clause() {
    // A column/alias named `output` or `returning` in a non-DML query must not
    // be counted as a DML result clause.
    assert_eq!(
        get(&metrics("SELECT output FROM t"), "sql.dml.returning_count"),
        0.0
    );
    assert_eq!(
        get(
            &metrics("SELECT returning FROM t"),
            "sql.dml.returning_count"
        ),
        0.0
    );
    // The real clause inside a DML statement is still counted.
    assert_eq!(
        get(
            &metrics("INSERT INTO t (a) VALUES (1) RETURNING id"),
            "sql.dml.returning_count"
        ),
        1.0
    );
}

#[test]
fn identifier_named_output_in_dml_is_not_a_returning_clause() {
    // A table/column literally named `output`/`returning` inside DML must not
    // count — the clause is a Keyword token, the identifier is not.
    assert_eq!(
        get(
            &metrics("UPDATE t SET output = 1 WHERE id = 1"),
            "sql.dml.returning_count"
        ),
        0.0
    );
    assert_eq!(
        get(
            &metrics("INSERT INTO output (id) VALUES (1)"),
            "sql.dml.returning_count"
        ),
        0.0
    );
}

#[test]
fn null_risk_ignores_comments_and_literals() {
    // Risk text inside a comment or a string literal must not be counted.
    assert_eq!(
        get(
            &metrics("SELECT * FROM t -- avoid x = NULL\n"),
            "sql.predicate.null_semantics_risk_count"
        ),
        0.0
    );
    assert_eq!(
        get(
            &metrics("SELECT 'NOT IN list' AS msg FROM t"),
            "sql.predicate.null_semantics_risk_count"
        ),
        0.0
    );
    // A real `= NULL` predicate is still counted (once).
    assert_eq!(
        get(
            &metrics("SELECT * FROM t WHERE x = NULL"),
            "sql.predicate.null_semantics_risk_count"
        ),
        1.0
    );
    // Real NOT IN.
    assert_eq!(
        get(
            &metrics("SELECT * FROM t WHERE x NOT IN (1, 2)"),
            "sql.predicate.null_semantics_risk_count"
        ),
        1.0
    );
}

#[test]
fn recursive_cte_detected_from_tokens_not_comments() {
    let real = metrics(
        "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 5) \
         SELECT * FROM r",
    );
    assert!(get(&real, "sql.cte.recursive_count") >= 1.0);
    // The phrase inside a comment must not mark a non-recursive CTE recursive.
    let commented = metrics("WITH c AS (SELECT 1 AS x) SELECT * FROM c -- WITH RECURSIVE\n");
    assert_eq!(get(&commented, "sql.cte.recursive_count"), 0.0);
}

#[test]
fn nested_subquery_alias_does_not_mask_outer_correlation() {
    // The inner `coupons c2` alias must not shadow the outer `c` so the
    // `o.customer_id = c.id` correlation is still detected.
    let m = metrics(
        "SELECT * FROM customers c \
         WHERE EXISTS (SELECT 1 FROM orders o \
                       WHERE o.customer_id = c.id \
                         AND EXISTS (SELECT 1 FROM coupons c2 WHERE c2.oid = o.id))",
    );
    assert!(get(&m, "sql.subquery.correlated_count") >= 1.0);
}

#[test]
fn aggregate_distinct_detected_from_keyword_not_substring() {
    assert_eq!(
        get(
            &metrics("SELECT COUNT(DISTINCT id) FROM t"),
            "sql.aggregate.distinct_count"
        ),
        1.0
    );
    // A column whose name merely contains "distinct" must not match.
    assert_eq!(
        get(
            &metrics("SELECT COUNT(distinctive_id) FROM t"),
            "sql.aggregate.distinct_count"
        ),
        0.0
    );
}

#[test]
fn non_self_referential_with_recursive_is_not_recursive() {
    // WITH RECURSIVE but the body never references itself → not a recursive CTE.
    let m = metrics("WITH RECURSIVE c AS (SELECT 1 AS n) SELECT * FROM c");
    assert_eq!(get(&m, "sql.cte.recursive_count"), 0.0);
    // A genuinely self-referential recursive CTE is counted.
    let rec = metrics(
        "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3) \
         SELECT * FROM r",
    );
    assert_eq!(get(&rec, "sql.cte.recursive_count"), 1.0);
}

#[test]
fn cte_names_excluded_from_object_touch() {
    // `base` is a CTE (query-local), not a database object — only `orders` is
    // a real read object.
    let m = metrics(
        "WITH base AS (SELECT id FROM orders) SELECT * FROM base JOIN base b2 ON base.id = b2.id",
    );
    assert_eq!(
        get(&m, "sql.object.read_count"),
        1.0,
        "only `orders` is a real object"
    );
    assert_eq!(get(&m, "sql.object.touch_count"), 1.0);
}

#[test]
fn hard_parse_error_reports_a_diagnostic_count() {
    // A report carrying a `sql.parse_error` must not also say diagnostic_count = 0.
    use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
    use mehen_sql::SqlAnalyzer;
    let analysis = SqlAnalyzer::new()
        .analyze(
            &SourceFile::new(
                "broken.sql".into(),
                Language::Sql,
                "SELECT FROM WHERE ;;; (((".to_string(),
            ),
            &AnalysisConfig::production(),
        )
        .expect("ok with diagnostics");
    let has_error = analysis
        .diagnostics
        .iter()
        .any(|d| d.code == "sql.parse_error");
    let count = analysis
        .root
        .metrics
        .get(&MetricKey::new("sql.parser.diagnostic_count"))
        .map(|v| v.as_f64())
        .unwrap_or(0.0);
    if has_error {
        assert!(
            count >= 1.0,
            "parse error must be reflected in diagnostic_count, got {count}"
        );
        // The composites must also reflect the parse failure (not report a
        // pristine file): unparsable facts are seeded, so maintainability is
        // below its risk-free maximum of 100.
        let unparsable = analysis
            .root
            .metrics
            .get(&MetricKey::new("sql.parser.unparsable_segment_count"))
            .map(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!(unparsable >= 1.0, "unparsable segment count should be >= 1");
        // A totally unparsable file reports a nonzero unparsable ratio (the
        // textual-LOC `code` count is 0, so the ratio falls back to the
        // unparsable-line count as the denominator → 1.0).
        let ratio = analysis
            .root
            .metrics
            .get(&MetricKey::new("sql.parser.unparsable_ratio"))
            .map(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!(
            ratio > 0.0,
            "unparsable_ratio must be > 0 for a hard failure, got {ratio}"
        );
        let mi = analysis
            .root
            .metrics
            .get(&MetricKey::new("sql.maintainability_index"))
            .map(|v| v.as_f64())
            .unwrap_or(100.0);
        assert!(
            mi < 100.0,
            "maintainability must reflect parser risk, got {mi}"
        );
    }
}
