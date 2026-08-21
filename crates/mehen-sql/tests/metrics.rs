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
fn cte_dependency_graph_is_scoped_per_with_block() {
    // CTE names are scoped to their owning WITH block. The second statement's
    // `FROM b` reads a real table `b`, NOT the first statement's CTE `b`, so it
    // must not forge a cross-statement dependency edge (Codex P2).
    let sql = "WITH b AS (SELECT 1 AS x) SELECT * FROM b; \
               WITH a AS (SELECT * FROM b) SELECT * FROM a;";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.cte.count"), 2.0);
    assert_eq!(get(&m, "sql.cte.dependency_edges"), 0.0);
    assert_eq!(get(&m, "sql.cte.max_dependency_depth"), 1.0);
    // Both CTEs are used within their own statement, so neither is unused.
    assert_eq!(get(&m, "sql.cte.unused_count"), 0.0);

    // A genuine intra-block chain a -> b -> c is still detected.
    let chain = metrics(
        "WITH a AS (SELECT 1 AS x), \
              b AS (SELECT x FROM a), \
              c AS (SELECT x FROM b) \
         SELECT x FROM c",
    );
    assert_eq!(get(&chain, "sql.cte.dependency_edges"), 2.0);
    assert_eq!(get(&chain, "sql.cte.max_dependency_depth"), 3.0);
}

#[test]
fn nested_with_shadowing_and_visibility() {
    // Shadowing: `a`'s body has its own `WITH b`, so `FROM b` reads the *inner*
    // `b`, not the outer sibling CTE `b` — no phantom `a -> b` edge (Codex P2).
    let shadow = metrics(
        "WITH b AS (SELECT 1 AS x), \
              a AS (WITH b AS (SELECT 2 AS y) SELECT * FROM b) \
         SELECT * FROM a",
    );
    assert_eq!(get(&shadow, "sql.cte.dependency_edges"), 0.0);
    assert_eq!(get(&shadow, "sql.cte.max_dependency_depth"), 1.0);

    // Visibility: an outer CTE `a` referenced inside `b`'s nested WITH body is
    // a real `b -> a` dependency — the scan must keep traversing nested WITH
    // bodies, only dropping *shadowed* names (CodeRabbit).
    let visible = metrics(
        "WITH a AS (SELECT 1 AS x), \
              b AS (WITH y AS (SELECT * FROM a) SELECT * FROM y) \
         SELECT * FROM b",
    );
    assert_eq!(get(&visible, "sql.cte.dependency_edges"), 1.0);
}

#[test]
fn in_subquery_counted_through_intervening_comment() {
    // A comment between `IN`/`EXISTS` and the subquery parentheses is legal
    // SQL and must not hide the predicate (Codex P2).
    let in_q = metrics("SELECT * FROM t WHERE id IN /* ids */ (SELECT id FROM u)");
    assert_eq!(get(&in_q, "sql.subquery.in_count"), 1.0);
    let exists_q =
        metrics("SELECT * FROM t WHERE EXISTS /* chk */ (SELECT 1 FROM u WHERE u.id = t.id)");
    assert_eq!(get(&exists_q, "sql.subquery.exists_count"), 1.0);
}

#[test]
fn dialect_inference_ignores_comments_and_literals() {
    // A dialect hint that appears only in a comment or a string literal must
    // not flip the effective parser dialect (Codex P2). These files are ANSI.
    let comment = metrics("-- TODO: QUALIFY this later\nSELECT a FROM t");
    assert_eq!(get(&comment, "sql.dialect.is_ansi"), 1.0);
    let literal = metrics("SELECT 'mentions NVARCHAR here' AS note FROM t");
    assert_eq!(get(&literal, "sql.dialect.is_ansi"), 1.0);
    // A genuine code hint is still detected.
    let real = metrics("SELECT a::text FROM t");
    assert_eq!(get(&real, "sql.dialect.is_postgres"), 1.0);
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
fn outer_apply_is_an_outer_lateral_join() {
    // T-SQL `OUTER APPLY` is a left-outer lateral join: it counts toward
    // outer_count and lateral, not inner (Codex P2).
    let outer = metrics("SELECT * FROM t OUTER APPLY dbo.fn(t.id) AS x");
    assert_eq!(get(&outer, "sql.join.outer_count"), 1.0);
    assert_eq!(get(&outer, "sql.join.kind_count.lateral"), 1.0);
    assert_eq!(get(&outer, "sql.join.kind_count.inner"), 0.0);
    // `CROSS APPLY` is lateral but not outer.
    let cross = metrics("SELECT * FROM t CROSS APPLY dbo.fn(t.id) AS x");
    assert_eq!(get(&cross, "sql.join.kind_count.lateral"), 1.0);
    assert_eq!(get(&cross, "sql.join.outer_count"), 0.0);
}

#[test]
fn non_equi_join_ignores_nested_subquery_equalities() {
    // A range/non-equi outer join whose ON clause contains a subquery with its
    // own `col = col` must still count as non-equi: the nested equality is the
    // subquery's join key, not the outer join's (Codex P2).
    let m = metrics(
        "SELECT * FROM a JOIN b ON a.ts > b.ts \
         AND EXISTS (SELECT 1 FROM c JOIN d ON c.id = d.id)",
    );
    assert_eq!(get(&m, "sql.join.non_equi_count"), 1.0);
    // A genuine equi join is still not flagged.
    let equi = metrics("SELECT * FROM a JOIN b ON a.id = b.id");
    assert_eq!(get(&equi, "sql.join.non_equi_count"), 0.0);
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
fn parenthesized_join_keys_are_equi() {
    // `ON (a.id) = (b.id)` wraps each key in `Bracketed → Expression`, so the
    // operands flanking `=` are not immediate `ColumnReference`s. Unwrapping
    // single-operand parens must still recognise the equality join (Codex P2).
    let paren = metrics("SELECT * FROM a JOIN b ON (a.id) = (b.id)");
    assert_eq!(get(&paren, "sql.join.non_equi_count"), 0.0);
    // A parenthesized comparison against a literal is *not* an equi-join: the
    // right side unwraps to a literal, not a column.
    let lit = metrics("SELECT * FROM a JOIN b ON (a.status) = ('x')");
    assert_eq!(get(&lit, "sql.join.non_equi_count"), 1.0);
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
    // Assert the precondition first: if the parser stops emitting
    // `sql.parse_error` for this malformed input, the test must *fail*, not
    // silently pass with all the body assertions skipped (CodeRabbit).
    assert!(
        has_error,
        "expected sql.parse_error diagnostic for malformed SQL"
    );
    let count = analysis
        .root
        .metrics
        .get(&MetricKey::new("sql.parser.diagnostic_count"))
        .map(|v| v.as_f64())
        .unwrap_or(0.0);
    {
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

#[test]
fn nested_set_expression_does_not_make_statement_a_set_operation() {
    // A UNION inside a CTE → the statement is a with_select, not set_operation.
    let cte = metrics("WITH c AS (SELECT 1 AS x UNION ALL SELECT 2) SELECT * FROM c");
    assert_eq!(get(&cte, "sql.statement.kind_count.with_select"), 1.0);
    assert_eq!(get(&cte, "sql.statement.kind_count.set_operation"), 0.0);
    // A UNION inside a derived table → plain select.
    let derived = metrics("SELECT * FROM (SELECT 1 AS x UNION ALL SELECT 2) q");
    assert_eq!(get(&derived, "sql.statement.kind_count.select"), 1.0);
    assert_eq!(get(&derived, "sql.statement.kind_count.set_operation"), 0.0);
    // A genuine top-level UNION is still a set_operation.
    let top = metrics("SELECT a FROM x UNION ALL SELECT a FROM y");
    assert_eq!(get(&top, "sql.statement.kind_count.set_operation"), 1.0);
}

#[test]
fn mixed_window_keys_are_summed_not_maxed() {
    // `PARTITION BY a, b + 1` has 1 column key + 1 expression key = 2.
    let m =
        metrics("SELECT ROW_NUMBER() OVER (PARTITION BY a, b + 1 ORDER BY c, d * 2) AS rn FROM t");
    assert_eq!(get(&m, "sql.window.partition_expression_count"), 2.0);
    assert_eq!(get(&m, "sql.window.order_expression_count"), 2.0);
}

#[test]
fn procedure_body_dml_is_not_top_level_dml() {
    // A CREATE PROCEDURE whose body contains INSERT must not be classified as
    // an insert (and must not inflate the DML risk counters). When sqruff
    // parses the routine under a procedural dialect it is `procedural`;
    // otherwise it is `unknown` — either way it is never `insert`.
    let m = metrics("CREATE PROCEDURE p () BEGIN INSERT INTO t (a) VALUES (1); END");
    assert_eq!(get(&m, "sql.statement.kind_count.insert"), 0.0);
    assert_eq!(get(&m, "sql.dml.insert_count"), 0.0);
}

#[test]
fn delete_and_update_targets_are_writes() {
    // The table a DELETE/UPDATE governs is a write object (it mutates rows),
    // even when a dialect places the target after `FROM`.
    let del = metrics("DELETE FROM u WHERE id = 1");
    assert_eq!(get(&del, "sql.object.write_count"), 1.0);
    assert_eq!(get(&del, "sql.object.read_count"), 0.0);

    let upd = metrics("UPDATE accounts SET bal = 0 WHERE id = 1");
    assert_eq!(get(&upd, "sql.object.write_count"), 1.0);
    assert_eq!(get(&upd, "sql.object.read_count"), 0.0);

    // INSERT … SELECT writes the target and reads the source.
    let ins = metrics("INSERT INTO target SELECT * FROM source");
    assert_eq!(get(&ins, "sql.object.write_count"), 1.0);
    assert_eq!(get(&ins, "sql.object.read_count"), 1.0);
}

#[test]
fn update_from_and_merge_sources_are_reads() {
    // `UPDATE dst … FROM src`: dst is written, src is read.
    let upd = metrics("UPDATE dst SET x = src.y FROM src WHERE dst.id = src.id");
    assert_eq!(get(&upd, "sql.object.write_count"), 1.0);
    assert_eq!(get(&upd, "sql.object.read_count"), 1.0);
    // MERGE: INTO target is written, USING source is read.
    let merge = metrics(
        "MERGE INTO accounts a USING updates u ON a.id = u.id \
         WHEN MATCHED THEN UPDATE SET a.bal = u.bal",
    );
    assert_eq!(get(&merge, "sql.object.write_count"), 1.0);
    assert_eq!(get(&merge, "sql.object.read_count"), 1.0);
}

#[test]
fn multi_target_ddl_counts_all_as_writes() {
    // `DROP TABLE a, b` mutates both — both are writes, neither a read.
    let m = metrics("DROP TABLE a, b");
    assert_eq!(get(&m, "sql.object.write_count"), 2.0);
    assert_eq!(get(&m, "sql.object.read_count"), 0.0);
}

#[test]
fn cte_names_are_scoped_to_their_owning_statement() {
    // A CTE named `tmp` in the first statement must not suppress a *real* `tmp`
    // table read in a later statement: CTE name scope is per-statement, not
    // file-wide (CodeRabbit). The second `SELECT * FROM tmp` reads the real
    // table `tmp`, so the file touches one read object.
    let m = metrics("WITH tmp AS (SELECT 1 AS x) SELECT * FROM tmp; SELECT * FROM tmp;");
    assert_eq!(get(&m, "sql.object.read_count"), 1.0);
    assert_eq!(get(&m, "sql.object.touch_count"), 1.0);
    // Within the first statement the CTE `tmp` is still query-local and is not
    // counted as a read on its own.
    let single = metrics("WITH tmp AS (SELECT 1 AS x) SELECT * FROM tmp");
    assert_eq!(get(&single, "sql.object.read_count"), 0.0);
}

#[test]
fn cte_names_are_scoped_by_query_block_not_whole_statement() {
    // A CTE `tmp` defined inside a *subquery* must not suppress the outer
    // real `tmp` table read: CTE scope is per query block (CodeRabbit). The
    // outer `FROM tmp` reads a real table; the inner `WITH tmp … FROM tmp` is
    // query-local to the EXISTS subquery.
    let m =
        metrics("SELECT * FROM tmp WHERE EXISTS (WITH tmp AS (SELECT 1 AS x) SELECT 1 FROM tmp)");
    assert_eq!(get(&m, "sql.object.read_count"), 1.0);
    assert_eq!(get(&m, "sql.object.touch_count"), 1.0);
}

#[test]
fn cube_rollup_classified_by_function_name() {
    // `ROLLUP(cube_id)` is a rollup, not a cube (the arg name must not match).
    let r = metrics("SELECT a FROM t GROUP BY ROLLUP(cube_id)");
    assert_eq!(get(&r, "sql.group_by.rollup_count"), 1.0);
    assert_eq!(get(&r, "sql.group_by.cube_count"), 0.0);
    let c = metrics("SELECT a FROM t GROUP BY CUBE(rollup_id)");
    assert_eq!(get(&c, "sql.group_by.cube_count"), 1.0);
    assert_eq!(get(&c, "sql.group_by.rollup_count"), 0.0);
}

#[test]
fn derived_table_join_is_a_multi_relation_scope() {
    // The derived table `q` and base `b` are two relations, so the unqualified
    // `id` is detected in a multi-relation scope.
    let m = metrics("SELECT id FROM (SELECT id FROM a) q JOIN b ON q.id = b.id");
    assert!(get(&m, "sql.identifier.unqualified_column_ratio") > 0.0);
}

#[test]
fn non_table_ddl_targets_count_as_writes() {
    // DROP FUNCTION / DROP SCHEMA target non-table object references but still
    // mutate a schema object — they must count toward write/touch and risk.
    let func = metrics("DROP FUNCTION foo");
    assert_eq!(get(&func, "sql.object.write_count"), 1.0);
    assert!(get(&func, "sql.change_risk_score") >= 8.0);
    let schema = metrics("DROP SCHEMA s");
    assert_eq!(get(&schema, "sql.object.write_count"), 1.0);
}

#[test]
fn create_index_writes_the_index_and_reads_the_host_table() {
    // `CREATE INDEX idx ON t` mutates the index object (`idx`), but the host
    // table `t` is only a dependency — referenced, not written. So the index
    // is the single write target and the table is a read.
    let create = metrics("CREATE INDEX idx ON t (a)");
    assert_eq!(get(&create, "sql.object.write_count"), 1.0);
    assert_eq!(get(&create, "sql.object.read_count"), 1.0);
    // `DROP INDEX idx ON t` (dialects that name the host table) is the same
    // shape: the index is dropped, the table is only referenced.
    let drop = metrics("DROP INDEX idx ON t");
    assert_eq!(get(&drop, "sql.object.write_count"), 1.0);
}

// ── in-file dialect directive (`-- sqlfluff:dialect:<name>`) ──────────────

/// Full analysis (metrics + diagnostics) for directive integration tests.
fn analyze_full(sql: &str) -> mehen_core::LanguageAnalysis {
    SqlAnalyzer::new()
        .analyze(
            &SourceFile::new("dir.sql".into(), Language::Sql, sql.to_string()),
            &AnalysisConfig::production(),
        )
        .expect("analysis ok")
}

fn diag_codes(a: &mehen_core::LanguageAnalysis) -> Vec<String> {
    a.diagnostics.iter().map(|d| d.code.clone()).collect()
}

#[test]
fn directive_pins_dialect_and_sets_confidence() {
    let m = metrics("-- sqlfluff:dialect:postgres\nSELECT 1");
    assert_eq!(get(&m, "sql.dialect.is_postgres"), 1.0);
    assert_eq!(get(&m, "sql.dialect.confidence"), 1.0);
    assert_eq!(get(&m, "sql.dialect.directive_present"), 1.0);
}

#[test]
fn directive_unknown_dialect_emits_warning_and_falls_back() {
    let a = analyze_full("-- sqlfluff:dialect:nope\nSELECT 1");
    assert!(diag_codes(&a).contains(&"sql.dialect.unknown".to_string()));
    // Falls back to ansi inference; directive still recorded as present.
    let m = &a.root.metrics;
    assert_eq!(get(m, "sql.dialect.is_ansi"), 1.0);
    assert_eq!(get(m, "sql.dialect.directive_present"), 1.0);
    assert!(get(m, "sql.dialect.confidence") < 1.0);
}

#[test]
fn directive_uncompiled_dialect_warns_and_does_not_panic() {
    // `duckdb` is a real sqruff dialect but not compiled into this build. It
    // must degrade to inference with a warning — never panic.
    let a = analyze_full("-- sqlfluff:dialect:duckdb\nSELECT 1");
    assert!(diag_codes(&a).contains(&"sql.dialect.unsupported".to_string()));
    assert_eq!(get(&a.root.metrics, "sql.dialect.is_ansi"), 1.0);
}

#[test]
fn directive_present_is_zero_without_a_directive() {
    let m = metrics("SELECT 1");
    assert_eq!(get(&m, "sql.dialect.directive_present"), 0.0);
}

#[test]
fn directive_surfaces_on_comment_only_file() {
    // No statements parse, but the bad directive warning must still surface.
    let a = analyze_full("-- sqlfluff:dialect:nope\n-- just a note");
    assert!(diag_codes(&a).contains(&"sql.dialect.unknown".to_string()));
    assert_eq!(get(&a.root.metrics, "sql.dialect.directive_present"), 1.0);
}

// ── procedural SQL (research foundation §6.17, Phase 3) ───────────────

/// PL/SQL routine with the full §6.17 construct set — every count below is
/// hand-traced against the fixture (see the cyclomatic/cognitive breakdowns
/// inline). The fixture parses fully under the Oracle dialect, so this
/// exercises the typed-CST token path.
#[test]
fn plsql_procedural_family_counts() {
    let m = metrics(include_str!("fixtures/plsql_procedure_control_flow.sql"));
    assert_eq!(get(&m, "sql.procedural.routine_count"), 1.0);
    assert_eq!(get(&m, "sql.procedural.block_count"), 1.0);
    assert_eq!(get(&m, "sql.procedural.max_block_depth"), 1.0);
    // IF + one ELSIF.
    assert_eq!(get(&m, "sql.procedural.if_count"), 2.0);
    // WHILE … LOOP + numeric FOR … LOOP (each counted once, not twice for
    // their body-opening LOOP keyword).
    assert_eq!(get(&m, "sql.procedural.loop_count"), 2.0);
    assert_eq!(get(&m, "sql.procedural.case_statement_count"), 0.0);
    // EXCEPTION WHEN no_data_found / WHEN others.
    assert_eq!(get(&m, "sql.procedural.exception_handler_count"), 2.0);
    assert_eq!(get(&m, "sql.procedural.return_count"), 1.0);
    // raise_application_error + two bare RAISE.
    assert_eq!(get(&m, "sql.procedural.raise_throw_count"), 3.0);
    // EXECUTE IMMEDIATE.
    assert_eq!(get(&m, "sql.procedural.dynamic_sql_count"), 1.0);
    // Cyclomatic (Sonar PL/SQL model): entry 1 + IF 1 + ELSIF 1 + AND 1
    // + RAISE×3 + loops×2 + EXIT WHEN 1 + handlers×2 = 12.
    assert_eq!(get(&m, "sql.procedural.cyclomatic_complexity"), 12.0);
    // Cognitive: IF 1 + ELSIF 1 + ELSE 1 + boolean sequence 1 + WHILE 1
    // + EXIT WHEN 1 + FOR 1 + handlers×2 = 9 (flat: nothing is nested).
    assert_eq!(get(&m, "sql.procedural.cognitive_complexity"), 9.0);
    // Change risk: CREATE OR REPLACE (4) + dynamic SQL (5). The routine
    // body's UPDATE is *not* file-level risk (it runs when called, not when
    // the file is applied).
    assert_eq!(get(&m, "sql.change_risk_score"), 9.0);
    assert_eq!(get(&m, "sql.object.write_count"), 0.0);
    // The embedded UPDATE…WHERE gives the routine a small query-structural
    // score, surfaced file-level as the max over routines.
    assert!(get(&m, "sql.structural_complexity.max_embedded_query") > 0.0);
}

/// T-SQL routine exercising the token fallback path: sqruff parses the
/// header and keyword-led IF statements but spills the WHILE/TRY-CATCH tail
/// into top-level `Unparsable` runs (parser comparison §9). The procedural
/// counts must survive that degradation — and the split body must NOT be
/// reported as independently-executing batch DML (Codex P1): T-SQL batch
/// semantics say the body extends to the next GO/EOF, so the keyword-led IF
/// that sqruff splits off is a routine *continuation*, not migration risk.
#[test]
fn tsql_procedural_family_counts_through_unparsable_spill() {
    let m = metrics(include_str!("fixtures/tsql_procedure_control_flow.sql"));
    // The parse loses statement structure but not the token stream.
    assert!(get(&m, "sql.parser.unparsable_segment_count") > 0.0);
    assert_eq!(get(&m, "sql.procedural.routine_count"), 1.0);
    // IF @batch, IF @count = 5, IF error_number() = 208.
    assert_eq!(get(&m, "sql.procedural.if_count"), 3.0);
    assert_eq!(get(&m, "sql.procedural.loop_count"), 1.0);
    // BEGIN CATCH.
    assert_eq!(get(&m, "sql.procedural.exception_handler_count"), 1.0);
    // THROW.
    assert_eq!(get(&m, "sql.procedural.raise_throw_count"), 1.0);
    // EXEC sp_executesql.
    assert_eq!(get(&m, "sql.procedural.dynamic_sql_count"), 1.0);
    assert_eq!(get(&m, "sql.procedural.return_count"), 2.0);
    // Proc body BEGIN + IF/ELSE blocks + WHILE block + TRY + CATCH.
    assert_eq!(get(&m, "sql.procedural.block_count"), 6.0);
    // Entry: the routine only — the keyword-led IF that sqruff splits into a
    // sibling statement is reclassified as the routine's continuation, so it
    // earns no separate anonymous-block entry. 1 entry + 3 IF + 1 WHILE
    // + 1 CATCH + 1 THROW = 7.
    assert_eq!(get(&m, "sql.procedural.cyclomatic_complexity"), 7.0);
    assert_eq!(get(&m, "sql.statement.kind_count.anonymous_block"), 0.0);
    assert_eq!(get(&m, "sql.statement.kind_count.procedural"), 2.0);
    // The body's UPDATE runs when the procedure is *called*, not when the
    // file is applied — no file-level DML or object-touch risk (Codex P1).
    assert_eq!(get(&m, "sql.dml.update_count"), 0.0);
    assert_eq!(get(&m, "sql.object.write_count"), 0.0);
    // Change risk: dynamic SQL only.
    assert_eq!(get(&m, "sql.change_risk_score"), 5.0);
}

/// An Oracle anonymous block *executes when the file is applied*, so its
/// body DML/TCL feeds the DML counters, object touches, and change risk —
/// unlike a routine definition's body (probed: `Statement >
/// OracleBeginEndBlock`).
#[test]
fn anonymous_block_body_dml_counts_as_migration_risk() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               begin\n\
                 update accounts set bal = 0;\n\
                 commit;\n\
               end;\n\
               /\n";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.statement.kind_count.anonymous_block"), 1.0);
    assert_eq!(get(&m, "sql.dml.update_count"), 1.0);
    assert_eq!(get(&m, "sql.dml.update_without_where_count"), 1.0);
    assert_eq!(get(&m, "sql.transaction.control_count"), 1.0);
    assert_eq!(get(&m, "sql.object.write_count"), 1.0);
    // Procedural entry for the block itself.
    assert_eq!(get(&m, "sql.procedural.block_count"), 1.0);
    assert!(get(&m, "sql.procedural.cyclomatic_complexity") >= 1.0);
}

/// Regression (dialect folding): sqruff's Oracle dialect emits its own
/// `OracleUpdateStatement`/`OracleTableReference`/… kinds. Before the folding
/// sets, top-level Oracle DML classified as `unknown` and appeared in no
/// `sql.dml.*` / object-touch / change-risk metric.
#[test]
fn oracle_dml_classifies_and_feeds_object_touch() {
    let sql = "-- sqlfluff:dialect:oracle\n\
               update orders set status = 'X' where id = 1;\n\
               insert into audit_log (id) values (1);\n\
               delete from stale_rows;\n\
               commit;\n";
    let m = metrics(sql);
    assert_eq!(get(&m, "sql.statement.kind_count.update"), 1.0);
    assert_eq!(get(&m, "sql.statement.kind_count.insert"), 1.0);
    assert_eq!(get(&m, "sql.statement.kind_count.delete"), 1.0);
    assert_eq!(get(&m, "sql.statement.kind_count.transaction_control"), 1.0);
    assert_eq!(get(&m, "sql.statement.kind_count.unknown"), 0.0);
    assert_eq!(get(&m, "sql.dml.update_count"), 1.0);
    assert_eq!(get(&m, "sql.dml.delete_without_where_count"), 1.0);
    // orders + audit_log + stale_rows are written objects.
    assert_eq!(get(&m, "sql.object.write_count"), 3.0);
}

// ── predicate keyword fixes ─────────────────────────────────────────────

/// `NOT NULL` column constraints and `IF NOT EXISTS` guards are DDL, not
/// predicate logic; `IS NOT NULL`, `NOT IN`, and `NOT EXISTS` predicates
/// still count.
#[test]
fn not_count_excludes_ddl_contexts() {
    let ddl = metrics("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)");
    assert_eq!(get(&ddl, "sql.predicate.not_count"), 0.0);

    let guard = metrics("-- sqlfluff:dialect:postgres\nCREATE TABLE IF NOT EXISTS t (id INT)");
    assert_eq!(get(&guard, "sql.predicate.not_count"), 0.0);

    let is_not_null = metrics("SELECT a FROM t WHERE a IS NOT NULL");
    assert_eq!(get(&is_not_null, "sql.predicate.not_count"), 1.0);

    let not_in = metrics("SELECT a FROM t WHERE a NOT IN (1, 2)");
    assert_eq!(get(&not_in, "sql.predicate.not_count"), 1.0);

    let not_exists = metrics("SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM u)");
    assert_eq!(get(&not_exists, "sql.predicate.not_count"), 1.0);
}

/// `sql.subquery.in_count` counts IN-subqueries, not raw `IN` keywords —
/// a `FOR i IN 1..10 LOOP` header or a parameter direction never counts
/// (only `IN` followed by a bracketed SELECT does).
#[test]
fn in_subquery_count_ignores_procedural_in_keywords() {
    let m = metrics(include_str!("fixtures/plsql_procedure_control_flow.sql"));
    // The fixture has a FOR … IN loop and no IN-subqueries.
    assert_eq!(get(&m, "sql.subquery.in_count"), 0.0);

    let predicate = metrics("SELECT a FROM t WHERE a IN (SELECT id FROM u)");
    assert_eq!(get(&predicate, "sql.subquery.in_count"), 1.0);
}

// ── PR #257 review regressions (procedural state machine) ──────────────

/// Homogeneous boolean chains cost one cognitive *sequence*, not one per
/// operator — operands must not break the run (Codex P2, PR #257).
#[test]
fn boolean_sequences_charge_per_run_not_per_operator() {
    let homogeneous = metrics(
        "-- sqlfluff:dialect:oracle\n\
         begin\n\
           if a = 1 and b = 2 and c = 3 then\n\
             null;\n\
           end if;\n\
         end;\n\
         /\n",
    );
    // Cyclomatic: entry 1 + if 1 + two ANDs = 4.
    assert_eq!(
        get(&homogeneous, "sql.procedural.cyclomatic_complexity"),
        4.0
    );
    // Cognitive: if 1 + ONE sequence for the AND-run = 2.
    assert_eq!(
        get(&homogeneous, "sql.procedural.cognitive_complexity"),
        2.0
    );

    let mixed = metrics(
        "-- sqlfluff:dialect:oracle\n\
         begin\n\
           if a = 1 and b = 2 or c = 3 then\n\
             null;\n\
           end if;\n\
         end;\n\
         /\n",
    );
    // Cognitive: if 1 + AND-run 1 + OR-run 1 = 3 (operator change re-charges).
    assert_eq!(get(&mixed, "sql.procedural.cognitive_complexity"), 3.0);
}

/// `WHEN NOT <cond>` in a procedural CASE is a real branch; only the MERGE
/// `WHEN [NOT] MATCHED` token shape is declaratively excluded (Codex P2,
/// PR #257).
#[test]
fn case_when_not_condition_still_counts() {
    // The Oracle grammar sends procedural CASE to Unparsable inside the
    // block region — the token machine must still see both WHEN arms.
    let m = metrics(
        "-- sqlfluff:dialect:oracle\n\
         begin\n\
           case when not done then null; when ready then null; end case;\n\
         end;\n\
         /\n",
    );
    assert_eq!(get(&m, "sql.procedural.case_statement_count"), 1.0);
    // Cyclomatic: two WHEN arms. The Oracle grammar cannot parse procedural
    // CASE, so the whole block degrades to an Unparsable fragment — no typed
    // block node forms, the statement is not classified `anonymous_block`,
    // and no entry path is credited (only classified regions earn entries).
    assert_eq!(get(&m, "sql.procedural.cyclomatic_complexity"), 2.0);
    // MERGE keeps its clauses out of the procedural family.
    let merge = metrics(
        "MERGE INTO t USING s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET c = 2 \
         WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id)",
    );
    assert_eq!(get(&merge, "sql.procedural.case_statement_count"), 0.0);
    assert_eq!(get(&merge, "sql.procedural.cyclomatic_complexity"), 0.0);
}

/// Parenthesized conditions are ordinary statements (`IF (@x > 0)`), not the
/// scalar `IF(…)` function — the discriminator is the parsed function-name
/// shape, not the following `(` (Codex P2, PR #257).
#[test]
fn parenthesized_if_condition_counts_as_control_flow() {
    let tsql = metrics(
        "-- sqlfluff:dialect:tsql\n\
         if (@batch > 0)\n\
         begin\n\
           select 1;\n\
         end\n",
    );
    assert_eq!(get(&tsql, "sql.procedural.if_count"), 1.0);

    // The MySQL scalar IF() *function* in parsed SQL stays declarative.
    let scalar = metrics("-- sqlfluff:dialect:mysql\nSELECT IF(x > 0, 1, 2) FROM t;\n");
    assert_eq!(get(&scalar, "sql.procedural.if_count"), 0.0);
}

/// A T-SQL `WHILE … BEGIN … END` body carries the loop's nesting: the IF
/// inside costs 1 + 1, exactly like its PL/SQL `WHILE … LOOP` equivalent
/// (Codex P2, PR #257).
#[test]
fn tsql_while_begin_body_nests_its_contents() {
    let m = metrics(
        "-- sqlfluff:dialect:tsql\n\
         while @x > 0\n\
         begin\n\
           if @x = 5 break;\n\
           set @x = @x - 1;\n\
         end\n",
    );
    assert_eq!(get(&m, "sql.procedural.loop_count"), 1.0);
    assert_eq!(get(&m, "sql.procedural.if_count"), 1.0);
    // Cognitive: while 1 + if (1 + 1 nesting) = 3.
    assert_eq!(get(&m, "sql.procedural.cognitive_complexity"), 3.0);
}

/// `DBMS_SQL.PARSE(…)` is dynamic SQL even though the parsed package
/// qualifier lexes as a `NakedIdentifier` (Codex P2, PR #257).
#[test]
fn dbms_sql_package_reference_counts_as_dynamic_sql() {
    let m = metrics(
        "-- sqlfluff:dialect:oracle\n\
         begin\n\
           dbms_sql.parse(c, 'drop table scratch', 1);\n\
         end;\n\
         /\n",
    );
    assert_eq!(get(&m, "sql.procedural.dynamic_sql_count"), 1.0);
    assert!(get(&m, "sql.change_risk_score") >= 5.0);
}

/// A genuinely top-level scripting block (no preceding routine definition)
/// executes on apply — DDL inside it is migration risk (Codex P1, PR #257):
/// `IF … THEN DROP TABLE t; END IF` must report the drop and its +8 risk
/// term. BigQuery scripting parses block DDL into typed nodes; the T-SQL
/// grammar loses `IF … BEGIN DROP …` bodies to `Unparsable` entirely, so
/// there this remains parser-bound (flagged by `sql.parser.*`, never
/// mis-counted).
#[test]
fn top_level_batch_block_ddl_counts_as_migration_risk() {
    let m = metrics(
        "-- sqlfluff:dialect:bigquery\n\
         if cleanup then\n\
           drop table stale_data;\n\
           truncate table audit_log;\n\
         end if;\n",
    );
    // BigQuery top-level scripting parses as a `MultiStatementSegment` whose
    // *inner* statements are the file's top-level statements — the DDL
    // classifies and risk-scores through the normal per-statement path…
    assert_eq!(get(&m, "sql.ddl.drop_count"), 1.0);
    assert_eq!(get(&m, "sql.ddl.truncate_count"), 1.0);
    // Drop 8 + truncate 8, plus write objects.
    assert!(get(&m, "sql.change_risk_score") >= 16.0);
    // …while the scripting control flow around them is measured as a
    // procedural region (entry + IF).
    assert_eq!(get(&m, "sql.procedural.if_count"), 1.0);
    assert_eq!(get(&m, "sql.procedural.cyclomatic_complexity"), 2.0);
}

/// Oracle `INSERT ALL` lists several statement-level `INTO` targets — every
/// one is written, none is a read (Codex P2, PR #257). A plain
/// `INSERT INTO … SELECT` keeps its source inside the SELECT, so widening
/// inserts to all-targets cannot misclassify sources as writes.
#[test]
fn insert_all_destinations_are_all_write_targets() {
    let m = metrics(
        "-- sqlfluff:dialect:oracle\n\
         insert all\n\
           into orders_archive (id) values (id)\n\
           into orders_audit (id) values (id)\n\
         select id from orders;\n",
    );
    assert_eq!(get(&m, "sql.object.write_count"), 2.0);
    assert_eq!(get(&m, "sql.object.read_count"), 1.0);

    let plain = metrics("INSERT INTO dst SELECT id FROM src");
    assert_eq!(get(&plain, "sql.object.write_count"), 1.0);
    assert_eq!(get(&plain, "sql.object.read_count"), 1.0);
}

/// DCL inside a typed anonymous block (Oracle `BEGIN … END`) counts — the
/// grant parses as an `AccessStatement` node there, unlike inside T-SQL
/// keyword-led blocks where the tsql grammar loses it to `Unparsable`
/// (Codex P1, PR #257).
#[test]
fn anonymous_block_dcl_counts_as_migration_risk() {
    let m = metrics(
        "-- sqlfluff:dialect:oracle\n\
         begin\n\
           grant select on t to reporting;\n\
         end;\n\
         /\n",
    );
    assert_eq!(get(&m, "sql.statement.kind_count.anonymous_block"), 1.0);
    assert_eq!(get(&m, "sql.dcl.grant_revoke_count"), 1.0);
    assert!(get(&m, "sql.change_risk_score") >= 5.0);
}

/// MySQL routine exercising the fragment path: the mysql grammar splits the
/// body into per-branch typed statements (`IfThenStatement` ×4,
/// `WhileStatement` ×2, `RepeatStatement` ×2) plus an `Unparsable` CASE run.
/// All fragments reclassify as routine continuations — body DML is not
/// migration-time DML — while the token machine counts control flow across
/// them (CodeRabbit, PR #257).
#[test]
fn mysql_procedural_family_counts_across_fragments() {
    let m = metrics(include_str!("fixtures/mysql_procedure_control_flow.sql"));
    assert_eq!(get(&m, "sql.procedural.routine_count"), 1.0);
    // IF + ELSEIF.
    assert_eq!(get(&m, "sql.procedural.if_count"), 2.0);
    // WHILE … DO + REPEAT … END REPEAT.
    assert_eq!(get(&m, "sql.procedural.loop_count"), 2.0);
    // CASE … END CASE (from the Unparsable run).
    assert_eq!(get(&m, "sql.procedural.case_statement_count"), 1.0);
    // SIGNAL.
    assert_eq!(get(&m, "sql.procedural.raise_throw_count"), 1.0);
    // PREPARE … FROM (the paired EXECUTE stmt does not double-count).
    assert_eq!(get(&m, "sql.procedural.dynamic_sql_count"), 1.0);
    // Entry 1 + IF 1 + ELSEIF 1 + WHILE 1 + REPEAT 1 + CASE WHEN 1
    // + SIGNAL 1 = 7.
    assert_eq!(get(&m, "sql.procedural.cyclomatic_complexity"), 7.0);
    // IF 1 + ELSEIF 1 + ELSE 1 + WHILE 1 + REPEAT 1 + CASE statement 1 = 6.
    assert_eq!(get(&m, "sql.procedural.cognitive_complexity"), 6.0);
    // Body fragments are routine continuations, not executing batches.
    assert_eq!(get(&m, "sql.statement.kind_count.anonymous_block"), 0.0);
    assert_eq!(get(&m, "sql.dml.update_count"), 0.0);
    // Change risk: dynamic SQL only.
    assert_eq!(get(&m, "sql.change_risk_score"), 5.0);
}

/// BigQuery top-level scripting: `MultiStatementSegment`s directly under
/// `File` are procedural regions (each with an entry path), while their
/// inner statements stay the file's top-level statements — the UPDATE
/// executes on apply and *does* count as migration DML, unlike a routine
/// body's (CodeRabbit, PR #257).
#[test]
fn bigquery_scripting_family_counts() {
    let m = metrics(include_str!("fixtures/bigquery_scripting.sql"));
    assert_eq!(get(&m, "sql.procedural.routine_count"), 0.0);
    // IF + ELSEIF at top level, plus the IF nested in the WHILE.
    assert_eq!(get(&m, "sql.procedural.if_count"), 3.0);
    // WHILE … END WHILE + FOR … IN … DO … END FOR.
    assert_eq!(get(&m, "sql.procedural.loop_count"), 2.0);
    // EXCEPTION WHEN ERROR THEN (inside the begin/exception block, which the
    // bigquery grammar partially loses to Unparsable — the handler tokens
    // survive).
    assert_eq!(get(&m, "sql.procedural.exception_handler_count"), 1.0);
    // RAISE USING MESSAGE.
    assert_eq!(get(&m, "sql.procedural.raise_throw_count"), 1.0);
    // EXECUTE IMMEDIATE.
    assert_eq!(get(&m, "sql.procedural.dynamic_sql_count"), 1.0);
    // Entries: if/while/for scripting regions = 3; + IF 3 + loops 2
    // + handler 1 + raise 1 = 10.
    assert_eq!(get(&m, "sql.procedural.cyclomatic_complexity"), 10.0);
    // IF 1 + ELSEIF 1 + ELSE 1 + WHILE 1 + nested IF 2 + FOR 1
    // + handler 1 = 8.
    assert_eq!(get(&m, "sql.procedural.cognitive_complexity"), 8.0);
    // Top-level scripting DML executes on apply.
    assert_eq!(get(&m, "sql.dml.update_count"), 1.0);
}
