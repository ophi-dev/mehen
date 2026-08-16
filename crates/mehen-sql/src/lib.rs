// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-sql` — SQL language analyzer.
//!
//! SQL gets its own `sql.*` metric family rather than being squeezed into the
//! function/class-centric source-code model: the dominant complexity
//! mechanism in standalone `.sql` files is relational/dataflow structure, not
//! imperative control flow (research foundation §1). The analyzer follows the
//! same "shared output contract, language-owned semantics" model as
//! `mehen-markdown`: it parses with a dedicated backend (sqruff), runs a
//! metric pipeline, and flattens the headline numbers into the shared
//! `MetricSet` flat-key shape so `mehen metrics x.sql` returns real values.
//!
//! Architecture (mirrors `design-docs/mehen_sql_metrics_research_foundation.md`
//! §2):
//! - [`dialect`]    — dialect selection + conservative inference;
//! - [`facts`]      — the sqruff→facts adapter (the only module that touches
//!   `ErasedSegment`/`SyntaxKind`);
//! - [`loc`]        — comment-aware LOC family;
//! - [`metrics`]    — raw `sql.*` key publishing;
//! - [`composite`]  — explainable weighted scores.
//!
//! The sqruff CST is `Rc`-based and not `Send`; per the `LanguageAnalyzer`
//! contract, parsing and fact extraction both happen inside one `analyze`
//! call and only owned `MetricSet`/`MetricSpace` data escapes.

#![forbid(unsafe_code)]

mod composite;
mod dialect;
mod facts;
mod loc;
mod metrics;

use mehen_core::{
    AnalysisBackend, AnalysisConfig, ContributionCollector, Language, LanguageAnalysis,
    LanguageAnalyzer, MetricSpace, ParseDiagnostic, Result, SourceFile, SourceSpan, SpaceId,
    SpaceKind, byte_offset_clamped,
};
use smol_str::SmolStr;

use sqruff_lib_core::dialects::init::DialectKind;
use sqruff_lib_core::parser::Parser;
use sqruff_lib_core::parser::segments::Tables;

use crate::dialect::dialect_label;

/// sqruff-backed SQL analyzer for the engine registry.
pub struct SqlAnalyzer;

impl SqlAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SqlAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for SqlAnalyzer {
    fn language(&self) -> Language {
        Language::Sql
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::Sqruff
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let file_span = SourceSpan {
            start_byte: 0,
            end_byte: byte_offset_clamped(source.text.len()),
            start_line: 1,
            end_line: source.line_index.line_count(),
        };

        // Resolve the dialect and build its grammar in one step. An in-file
        // `-- sqlfluff:dialect:<name>` directive (SQLFluff parity) wins, then a
        // caller request (reserved in 1.0), then syntax inference; an
        // unknown/uncompiled pin degrades to inference. The grammar is built
        // exactly once here — `resolve_with_dialect` reuses the same build for
        // its compiled-in check, so directive files don't pay for it twice.
        let (resolution, dialect) =
            dialect::resolve_with_dialect(&source.text, requested_dialect());

        let parser = Parser::from(&dialect);
        let tables = Tables::default();
        let (tokens, lex_errs) = dialect.lexer().lex(&tables, source.text.as_str());
        let parsed = match parser.parse(&tables, &tokens) {
            Ok(Some(tree)) => tree,
            Ok(None) => {
                // Empty input (only whitespace/comments) — emit a valid,
                // mostly-empty analysis rather than failing. LOC is still
                // counted from the text so comment-only files report real
                // physical/comment/blank lines.
                return Ok(empty_sql_analysis(
                    file_span,
                    &source.text,
                    &resolution,
                    /* parse_failed */ false,
                    lex_errs.len() as u32,
                ));
            }
            Err(e) => {
                // Publish the full `sql.*` surface so the output contract is
                // identical to the empty/success paths — a hard parse error
                // must not silently drop every metric key for downstream
                // selectors/thresholds. `parse_failed = true` seeds the
                // parser-health facts so the parser-health metrics *and* the
                // composites that consume them reflect the failure. The error
                // diagnostic marks the analysis incomplete (engine treats it
                // as blocking).
                let mut analysis = empty_sql_analysis(
                    file_span,
                    &source.text,
                    &resolution,
                    /* parse_failed */ true,
                    lex_errs.len() as u32,
                );
                analysis.diagnostics.push(ParseDiagnostic::error(
                    "sql.parse_error",
                    format!("sqruff failed to parse: {}", e.description),
                ));
                return Ok(analysis);
            }
        };

        let line_index = &source.line_index;
        let line_at = |byte: u32| line_index.line_at(byte);

        let mut file_facts = facts::extract(&parsed, &dialect, line_at, config.emit_contributions);
        // Lexer errors (malformed tokens) are distinct from unparsable parse
        // segments. The current sqruff release never populates this vector, but
        // surface them into parser-health so a future version cannot make
        // invalid SQL look clean (Codex P2).
        file_facts.lex_error_count = lex_errs.len() as u32;
        let statement_spans: Vec<(u32, u32)> = file_facts
            .statements
            .iter()
            .map(|s| (s.start_line, s.end_line))
            .collect();
        let loc_stats = loc::compute(&source.text, &parsed, line_at, &statement_spans);

        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, file_span);
        metrics::publish(&file_facts, &loc_stats, &resolution, &mut root.metrics);
        publish_dialect_labels(&mut root, &resolution);

        let mut contribution_collector = ContributionCollector::new(config.emit_contributions);
        for item in &file_facts.change_risk_evidence {
            contribution_collector.record(
                "sql.change_risk_score",
                item.span,
                item.factor.amount(),
                item.factor.reason(),
            );
        }

        // Per-statement spaces so top-offenders / nested reporting can attribute
        // metrics to a statement's line range (research foundation §4.4).
        // Space ids start at 1 (the root unit is 0).
        for (next_id, stmt) in (1u32..).zip(file_facts.statements.iter()) {
            let span = SourceSpan {
                start_byte: stmt.start_byte,
                end_byte: stmt.end_byte,
                start_line: stmt.start_line,
                end_line: stmt.end_line,
            };
            let mut space = MetricSpace::new(
                SpaceId(next_id),
                SpaceKind::Custom(SmolStr::new("sql.statement")),
                span,
            );
            space.name = Some(stmt.kind.label().to_string());
            space.metrics.insert(
                "sql.statement.lines",
                stmt.end_line.saturating_sub(stmt.start_line) + 1,
            );
            root.spaces.push(space);
        }

        // Unparsable segments → non-fatal diagnostics (research foundation
        // §6.16). They lower confidence but don't block the report.
        let mut diagnostics = Vec::new();
        if file_facts.unparsable_segments > 0 {
            diagnostics.push(ParseDiagnostic::warning(
                "sql.unparsable",
                format!(
                    "{} unparsable SQL segment(s); some metrics may be incomplete",
                    file_facts.unparsable_segments
                ),
            ));
        }
        // Surface any lexer errors as warnings (none in the current sqruff
        // release, but plumbed so malformed tokens can never look clean).
        for err in &lex_errs {
            diagnostics.push(ParseDiagnostic::warning(
                "sql.lex_error",
                format!("SQL lexer error: {err}"),
            ));
        }
        // An in-file `-- sqlfluff:dialect:<name>` directive that names an
        // unknown or uncompiled dialect degrades to inference; warn so the
        // author sees why their pin had no effect.
        if let Some(diag) = directive_diagnostic(&resolution) {
            diagnostics.push(diag);
        }

        Ok(LanguageAnalysis {
            language: Language::Sql,
            backend: AnalysisBackend::Sqruff,
            diagnostics,
            root,
            contributions: contribution_collector.finish(),
        })
    }
}

/// Reserved hook for an explicit dialect request. 1.0 carries no SQL options
/// on `AnalysisConfig`, so this always returns `None` and inference drives the
/// choice. Kept as a single seam so wiring a CLI `--sql-dialect` flag later
/// touches one place.
fn requested_dialect() -> Option<DialectKind> {
    None
}

/// Publish the human-readable dialect labels as string-shaped metrics are not
/// representable in `MetricValue` (Int/Float only); instead we surface the
/// inferred/effective dialect via dedicated zero-or-one feature flags so the
/// numeric metric set still records *which* dialect ran. The canonical string
/// is also stamped on the diagnostics-free report through the metric keys
/// `sql.dialect.is_<name>`.
fn publish_dialect_labels(root: &mut MetricSpace, resolution: &dialect::DialectResolution) {
    // One-hot the effective dialect so JSON consumers can recover it from the
    // numeric metric map without a separate string channel.
    let effective = dialect_label(resolution.effective);
    root.metrics.insert(
        mehen_core::MetricKey::new(format!("sql.dialect.is_{effective}")),
        1i64,
    );
}

/// Translate a directive that named an unknown or uncompiled dialect into a
/// non-blocking warning. An *active* directive (recognized + compiled) drives
/// the dialect silently; only the degraded cases warn. Returns `None` when
/// there is no directive or the directive is active.
fn directive_diagnostic(resolution: &dialect::DialectResolution) -> Option<ParseDiagnostic> {
    use dialect::DirectiveStatus;
    let directive = resolution.directive.as_ref()?;
    match directive.status {
        DirectiveStatus::Active(_) => None,
        DirectiveStatus::Unsupported => Some(ParseDiagnostic::warning(
            "sql.dialect.unsupported",
            format!(
                "in-file directive requested dialect '{}', which is not compiled \
                 into this build; falling back to inferred dialect '{}'",
                directive.name,
                dialect_label(resolution.inferred),
            ),
        )),
        DirectiveStatus::Unknown => Some(ParseDiagnostic::warning(
            "sql.dialect.unknown",
            format!(
                "in-file directive requested unknown dialect '{}'; falling back \
                 to inferred dialect '{}'",
                directive.name,
                dialect_label(resolution.inferred),
            ),
        )),
    }
}

/// Build an analysis with no parse tree (empty/comment-only input, or a hard
/// parse error). Publishes the full zeroed `sql.*` surface, but LOC is still
/// counted from `text` so a comment-only file (`-- migration note`) reports
/// real physical/comment/blank lines rather than all zeros.
///
/// When `parse_failed` is true (a hard `Err` from the parser), the facts are
/// seeded with parser-health risk (unparsable segment/lines) so the
/// *composite* scores reflect the failure too — otherwise a totally
/// unparsable file would report a healthy maintainability index alongside its
/// `sql.parse_error` diagnostic.
fn empty_sql_analysis(
    file_span: SourceSpan,
    text: &str,
    resolution: &dialect::DialectResolution,
    parse_failed: bool,
    lex_error_count: u32,
) -> LanguageAnalysis {
    let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, file_span);
    // No parse tree → no statement spans and no code tokens; LOC is derived
    // from the text alone (every non-blank line is a comment line).
    let loc = loc::compute_textual(text);
    let mut facts = facts::SqlFileFacts::default();
    if parse_failed {
        // The whole file is unparsable; mark it so parser-health metrics and
        // the composites that consume them (review burden, maintainability)
        // reflect the parse failure rather than reporting a clean file.
        facts.unparsable_segments = 1;
        facts.unparsable_lines = loc.code.max(loc.physical).max(1);
    }
    // Carry any lexer errors into parser-health so they are reflected even
    // when there is no parse tree (current sqruff never sets this).
    facts.lex_error_count = lex_error_count;
    metrics::publish(&facts, &loc, resolution, &mut root.metrics);
    publish_dialect_labels(&mut root, resolution);
    let mut diagnostics = Vec::new();
    if lex_error_count > 0 {
        diagnostics.push(ParseDiagnostic::warning(
            "sql.lex_error",
            format!("{lex_error_count} SQL lexer error(s)"),
        ));
    }
    // A bad dialect directive must surface even when there is no parse tree
    // (comment-only file, or a hard parse failure), so the author still sees
    // why their pin had no effect.
    if let Some(diag) = directive_diagnostic(resolution) {
        diagnostics.push(diag);
    }
    LanguageAnalysis {
        language: Language::Sql,
        backend: AnalysisBackend::Sqruff,
        diagnostics,
        root,
        contributions: Vec::new(),
    }
}

/// Every fixed scalar metric key the SQL analyzer can publish, for
/// configuration validation and typo suggestions. Two dynamic,
/// enum-backed families are validated separately by
/// [`is_published_metric_key`]: `sql.statement.kind_count.<kind>` and
/// `sql.dialect.is_<dialect>`.
///
/// Kept honest by `published_key_catalogue_is_in_sync` in the tests
/// below, which analyzes feature-rich SQL and asserts every published
/// key validates.
pub const PUBLISHED_METRIC_KEYS: &[&str] = &[
    "sql.aggregate.distinct_count",
    "sql.aggregate.function_count",
    "sql.alias.table_alias_count",
    "sql.case.count",
    "sql.case.max_depth",
    "sql.case.max_when_count",
    "sql.case.missing_else_count",
    "sql.case.when_count",
    "sql.cast.count",
    "sql.change_risk_score",
    "sql.cognitive_complexity",
    "sql.cte.count",
    "sql.cte.dependency_edges",
    "sql.cte.max_dependency_depth",
    "sql.cte.max_fan_out",
    "sql.cte.recursive_count",
    "sql.cte.trivial_count",
    "sql.cte.unused_count",
    "sql.dcl.grant_revoke_count",
    "sql.ddl.alter_count",
    "sql.ddl.create_count",
    "sql.ddl.create_or_replace_count",
    "sql.ddl.drop_count",
    "sql.ddl.truncate_count",
    "sql.derived_table.count",
    "sql.dialect.confidence",
    "sql.dialect.conflict_count",
    "sql.dialect.directive_present",
    "sql.dialect.requested",
    "sql.dml.delete_count",
    "sql.dml.delete_without_where_count",
    "sql.dml.insert_count",
    "sql.dml.merge_count",
    "sql.dml.returning_count",
    "sql.dml.update_count",
    "sql.dml.update_without_where_count",
    "sql.expression.max_depth",
    "sql.function.call_count",
    "sql.function.distinct_count",
    "sql.function.nested_call_depth",
    "sql.group_by.count",
    "sql.group_by.cube_count",
    "sql.group_by.grouping_sets_count",
    "sql.group_by.rollup_count",
    "sql.halstead.difficulty",
    "sql.halstead.distinct_operands",
    "sql.halstead.distinct_operators",
    "sql.halstead.effort",
    "sql.halstead.length",
    "sql.halstead.total_operands",
    "sql.halstead.total_operators",
    "sql.halstead.vocabulary",
    "sql.halstead.volume",
    "sql.having.count",
    "sql.identifier.quoted_count",
    "sql.identifier.unqualified_column_ratio",
    "sql.join.count",
    "sql.join.cross_count",
    "sql.join.kind_count.cross",
    "sql.join.kind_count.full",
    "sql.join.kind_count.inner",
    "sql.join.kind_count.lateral",
    "sql.join.kind_count.left",
    "sql.join.kind_count.natural",
    "sql.join.kind_count.right",
    "sql.join.missing_condition_count",
    "sql.join.natural_count",
    "sql.join.non_equi_count",
    "sql.join.outer_count",
    "sql.loc.avg_statement_lines",
    "sql.loc.blank",
    "sql.loc.code",
    "sql.loc.comment",
    "sql.loc.comment_density",
    "sql.loc.logical",
    "sql.loc.max_statement_lines",
    "sql.loc.physical",
    "sql.maintainability_index",
    "sql.modularity_health",
    "sql.object.read_count",
    "sql.object.touch_count",
    "sql.object.write_count",
    "sql.parser.diagnostic_count",
    "sql.parser.unparsable_line_count",
    "sql.parser.unparsable_ratio",
    "sql.parser.unparsable_segment_count",
    "sql.predicate.boolean_operator_count",
    "sql.predicate.comparison_count",
    "sql.predicate.max_boolean_depth",
    "sql.predicate.not_count",
    "sql.predicate.null_semantics_risk_count",
    "sql.query_block.avg_select_items",
    "sql.query_block.count",
    "sql.query_block.max_depth",
    "sql.query_block.max_select_items",
    "sql.relation.ref_count",
    "sql.review_burden_index",
    "sql.select.expression_without_alias_count",
    "sql.select.outer_star_count",
    "sql.select.output_alias_coverage",
    "sql.select.star_count",
    "sql.set_op.count",
    "sql.set_op.kind_count.except",
    "sql.set_op.kind_count.intersect",
    "sql.set_op.kind_count.union",
    "sql.set_op.kind_count.union_all",
    "sql.set_op.union_all_ratio",
    "sql.statement.count",
    "sql.statement.kind_distinct",
    "sql.statement.kind_entropy",
    "sql.statement.lines",
    "sql.statement.unparsed_count",
    "sql.structural_complexity",
    "sql.subquery.correlated_count",
    "sql.subquery.count",
    "sql.subquery.exists_count",
    "sql.subquery.in_count",
    "sql.subquery.max_depth",
    "sql.subquery.scalar_count",
    "sql.transaction.control_count",
    "sql.window.frame_count",
    "sql.window.function_count",
    "sql.window.order_expression_count",
    "sql.window.partition_expression_count",
];

/// Whether the SQL analyzer can publish `name` onto a `MetricSpace` —
/// the fixed catalogue plus the enum-backed dynamic families. Used by
/// `mehen.toml` threshold validation so a typo like
/// `sql.modularit_health` is rejected at load time instead of
/// becoming a gate that can never fire.
pub fn is_published_metric_key(name: &str) -> bool {
    use core::str::FromStr;
    if PUBLISHED_METRIC_KEYS.contains(&name) {
        return true;
    }
    if let Some(label) = name.strip_prefix("sql.statement.kind_count.") {
        return facts::StatementKind::ALL
            .iter()
            .any(|kind| kind.label() == label);
    }
    if let Some(dialect) = name.strip_prefix("sql.dialect.is_") {
        return DialectKind::from_str(dialect).is_ok();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, MetricKey, SourceFile};

    fn analyze(sql: &str) -> LanguageAnalysis {
        SqlAnalyzer::new()
            .analyze(
                &SourceFile::new("q.sql".into(), Language::Sql, sql.to_string()),
                &AnalysisConfig::production(),
            )
            .expect("analysis ok")
    }

    fn metric(a: &LanguageAnalysis, key: &str) -> f64 {
        a.root
            .metrics
            .get(&MetricKey::new(key))
            .map(|v| v.as_f64())
            .unwrap_or(0.0)
    }

    /// Every key the analyzer publishes — root and statement spaces
    /// alike — must validate through `is_published_metric_key`, or
    /// `mehen.toml` threshold validation would reject a real metric.
    #[test]
    fn published_key_catalogue_is_in_sync() {
        let sql = "WITH history AS (SELECT 1 AS x),\n\
                   unused AS (SELECT 2 AS y)\n\
                   SELECT a.x,\n\
                          COUNT(DISTINCT b.y) OVER (PARTITION BY a.x ORDER BY a.x \
                          ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS w,\n\
                          CASE WHEN a.x > 0 THEN 1 ELSE 0 END AS c,\n\
                          CAST(a.x AS INT) AS casted,\n\
                          (SELECT MAX(z) FROM v WHERE v.id = a.x) AS scalar\n\
                   FROM history a\n\
                   LEFT JOIN t b ON a.x = b.x\n\
                   CROSS JOIN u\n\
                   JOIN (SELECT 1 AS d) derived ON derived.d = a.x\n\
                   WHERE EXISTS (SELECT 1 FROM v WHERE v.id = a.x)\n\
                     AND a.x IN (SELECT z FROM w2)\n\
                     AND NOT (a.x = 1 OR b.y IS NULL)\n\
                   GROUP BY ROLLUP(a.x)\n\
                   HAVING COUNT(*) > 1\n\
                   UNION ALL SELECT 9;\n\
                   INSERT INTO t VALUES (1);\n\
                   UPDATE t SET c = 1;\n\
                   DELETE FROM t;\n\
                   MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET c = 2;\n\
                   CREATE TABLE fresh (id INT);\n\
                   ALTER TABLE fresh ADD COLUMN c INT;\n\
                   DROP TABLE old_table;\n\
                   TRUNCATE TABLE t;\n\
                   GRANT SELECT ON t TO PUBLIC;\n\
                   COMMIT;\n";
        let a = analyze(sql);
        let mut stack = vec![&a.root];
        let mut seen = 0usize;
        while let Some(space) = stack.pop() {
            for (key, _) in space.metrics.iter() {
                seen += 1;
                assert!(
                    is_published_metric_key(key.as_str()),
                    "published key `{key}` is missing from the catalogue"
                );
            }
            stack.extend(space.spaces.iter());
        }
        assert!(
            seen > 80,
            "fixture must exercise a rich key set, saw {seen}"
        );
        // The dynamic families validate by their enums, including
        // members the fixture may not exercise…
        assert!(is_published_metric_key(
            "sql.statement.kind_count.create_view"
        ));
        assert!(is_published_metric_key("sql.dialect.is_snowflake"));
        // …and near-misses stay invalid.
        assert!(!is_published_metric_key("sql.modularit_health"));
        assert!(!is_published_metric_key("sql.statement.kind_count.selec"));
        assert!(!is_published_metric_key("sql.dialect.is_klingon"));
    }

    #[test]
    fn counts_statements_and_kinds() {
        let a = analyze("SELECT 1; INSERT INTO t VALUES (1); DROP TABLE t;");
        assert_eq!(metric(&a, "sql.statement.count"), 3.0);
        assert_eq!(metric(&a, "sql.statement.kind_count.insert"), 1.0);
        assert_eq!(metric(&a, "sql.ddl.drop_count"), 1.0);
    }

    #[test]
    fn counts_joins_and_ctes() {
        let sql = "WITH a AS (SELECT 1 AS x) \
                   SELECT t.x FROM a t LEFT JOIN a u ON t.x = u.x";
        let a = analyze(sql);
        assert_eq!(metric(&a, "sql.cte.count"), 1.0);
        assert_eq!(metric(&a, "sql.join.count"), 1.0);
        assert_eq!(metric(&a, "sql.join.outer_count"), 1.0);
    }

    #[test]
    fn flags_destructive_dml() {
        let a = analyze("DELETE FROM big_table;");
        assert_eq!(metric(&a, "sql.dml.delete_without_where_count"), 1.0);
        assert!(metric(&a, "sql.change_risk_score") > 0.0);
    }

    #[test]
    fn publishes_halstead_volume() {
        let a = analyze("SELECT a, b, c FROM t WHERE a > 1 AND b < 2");
        assert!(metric(&a, "sql.halstead.volume") > 0.0);
    }

    #[test]
    fn empty_input_is_clean() {
        let a = analyze("-- just a comment\n");
        assert_eq!(metric(&a, "sql.statement.count"), 0.0);
        assert!(a.diagnostics.is_empty());
    }
}
