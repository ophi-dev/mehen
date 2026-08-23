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
mod procedural;

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

        let mut file_facts = facts::extract(
            &parsed,
            line_at,
            config.emit_contributions,
            resolution.effective,
        );
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
        // Procedural composites are evidence-backed too: the published value
        // equals the sum of its contributions by construction (§4.7).
        for item in &file_facts.procedural.evidence {
            let metric = match item.metric {
                procedural::ProceduralMetric::Cyclomatic => "sql.procedural.cyclomatic_complexity",
                procedural::ProceduralMetric::Cognitive => "sql.procedural.cognitive_complexity",
                procedural::ProceduralMetric::EmbeddedQueryMax => {
                    "sql.structural_complexity.max_embedded_query"
                }
                procedural::ProceduralMetric::BlockCount => "sql.procedural.block_count",
                procedural::ProceduralMetric::RoutineCount => "sql.procedural.routine_count",
                procedural::ProceduralMetric::LoopCount => "sql.procedural.loop_count",
                procedural::ProceduralMetric::IfCount => "sql.procedural.if_count",
                procedural::ProceduralMetric::CaseStatementCount => {
                    "sql.procedural.case_statement_count"
                }
                procedural::ProceduralMetric::ExceptionHandlerCount => {
                    "sql.procedural.exception_handler_count"
                }
                procedural::ProceduralMetric::ReturnCount => "sql.procedural.return_count",
                procedural::ProceduralMetric::RaiseThrowCount => "sql.procedural.raise_throw_count",
                procedural::ProceduralMetric::DynamicSqlCount => "sql.procedural.dynamic_sql_count",
                procedural::ProceduralMetric::MaxBlockDepth => "sql.procedural.max_block_depth",
            };
            contribution_collector.record(metric, item.span, item.amount, item.reason);
        }
        // Predicate NOTs are evidence-backed too — the improved
        // `sql.predicate.not_count` explains each counted negation with its
        // token span (Codex P1).
        for &(start, end) in &file_facts.predicates.not_spans {
            contribution_collector.record(
                "sql.predicate.not_count",
                SourceSpan {
                    start_byte: start,
                    end_byte: end,
                    start_line: line_at(start),
                    end_line: line_at(end.saturating_sub(1)),
                },
                1.0,
                "sql.predicate.not",
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

        // Procedural units become `SpaceKind::Function` spaces nested under
        // the statement that contains them (and under each other for
        // subprograms declared inside a routine or package body). These are
        // the function-shaped scopes per-function coverage enrichment — and,
        // later, CRAP — attach to.
        attach_procedural_unit_spaces(
            &mut root,
            &file_facts.procedural_units,
            file_facts.statements.len() as u32 + 1,
        );

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

/// Nest procedural-unit spaces by byte containment and attach them to the
/// space tree.
///
/// `units` is pre-order ([`facts::ProceduralUnitFacts`]'s contract): a
/// container precedes its contents, so a stack suffices to rebuild the
/// nesting — routines inside package bodies and subprograms declared in a
/// routine's DECLARE section become children of their container's space.
/// Each top-level unit attaches to the `sql.statement` space whose span
/// contains it (the normal case — a routine definition *is* a statement),
/// falling back to the root defensively.
///
/// The spaces carry `SpaceKind::Function`: the engine's coverage
/// enrichment annotates exactly Function/Closure-kind spaces (recursing
/// through the statement layer), which is what gives SQL routines
/// per-function `coverage.*` keys — and, later, a CRAP denominator.
fn attach_procedural_unit_spaces(
    root: &mut MetricSpace,
    units: &[facts::ProceduralUnitFacts],
    first_id: u32,
) {
    /// Pop the innermost open unit and hand it to its parent (the new
    /// stack top) or to the top-level list.
    fn close_one(
        stack: &mut Vec<(MetricSpace, u32, u32)>,
        top_level: &mut Vec<(MetricSpace, u32, u32)>,
    ) {
        let done = stack.pop().expect("close_one on empty stack");
        match stack.last_mut() {
            Some((parent, _, _)) => parent.spaces.push(done.0),
            None => top_level.push(done),
        }
    }

    let mut stack: Vec<(MetricSpace, u32, u32)> = Vec::new();
    let mut top_level: Vec<(MetricSpace, u32, u32)> = Vec::new();

    for (next_id, unit) in (first_id..).zip(units.iter()) {
        let mut space = MetricSpace::new(
            SpaceId(next_id),
            SpaceKind::Function,
            SourceSpan {
                start_byte: unit.start_byte,
                end_byte: unit.end_byte,
                start_line: unit.start_line,
                end_line: unit.end_line,
            },
        );
        space.name = unit.name.clone();
        // Per-routine procedural composites (Phase 3): the same keys as the
        // file-level aggregates, scoped to this routine — the numbers
        // `mehen top-offenders` shows next to a function name, and the
        // complexity denominator CRAP will use.
        space.metrics.insert(
            "sql.procedural.cyclomatic_complexity",
            unit.cyclomatic_complexity,
        );
        space.metrics.insert(
            "sql.procedural.cognitive_complexity",
            unit.cognitive_complexity,
        );
        space
            .metrics
            .insert("sql.structural_complexity", unit.embedded_query_structural);
        while let Some((_, _, open_end)) = stack.last() {
            if unit.start_byte >= *open_end {
                close_one(&mut stack, &mut top_level);
            } else {
                break;
            }
        }
        stack.push((space, unit.start_byte, unit.end_byte));
    }
    while !stack.is_empty() {
        close_one(&mut stack, &mut top_level);
    }

    for (space, start_byte, end_byte) in top_level {
        let host = root.spaces.iter_mut().find(|s| {
            matches!(s.kind, SpaceKind::Custom(_))
                && s.span.start_byte <= start_byte
                && end_byte <= s.span.end_byte
        });
        match host {
            Some(statement_space) => statement_space.spaces.push(space),
            None => root.spaces.push(space),
        }
    }
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

/// Every fixed scalar metric key the SQL analyzer can publish onto the
/// *root* `MetricSpace` — the space `mehen.toml` thresholds and CLI
/// selectors read — for configuration validation and typo suggestions.
/// Child-only keys (`sql.statement.lines` on per-statement spaces) are
/// deliberately excluded: validating them would accept a gate that can
/// never fire. Two dynamic, enum-backed families are validated
/// separately by [`is_published_metric_key`]:
/// `sql.statement.kind_count.<kind>` and `sql.dialect.is_<dialect>`
/// (compiled dialects only).
///
/// Kept honest by `published_key_catalogue_is_in_sync` in the tests
/// below, which analyzes feature-rich SQL and asserts every published
/// root key validates.
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
    "sql.procedural.block_count",
    "sql.procedural.case_statement_count",
    "sql.procedural.cognitive_complexity",
    "sql.procedural.cyclomatic_complexity",
    "sql.procedural.dynamic_sql_count",
    "sql.procedural.exception_handler_count",
    "sql.procedural.if_count",
    "sql.procedural.loop_count",
    "sql.procedural.max_block_depth",
    "sql.procedural.raise_throw_count",
    "sql.procedural.return_count",
    "sql.procedural.routine_count",
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
    "sql.statement.unparsed_count",
    "sql.structural_complexity",
    "sql.structural_complexity.max_embedded_query",
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
    if let Some(name) = name.strip_prefix("sql.dialect.is_") {
        // Only *compiled* dialects can become the effective dialect
        // and publish their one-hot key; a recognized-but-uncompiled
        // name (e.g. `duckdb`) would be a gate that can never fire.
        return DialectKind::from_str(name)
            .is_ok_and(|kind| dialect::dialect_for_kind(kind).is_some());
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

    /// Every compiled-in dialect must survive a full analyze of a file
    /// containing function calls. sqruff resolves grammar `Ref`s lazily
    /// during matching, so a dialect grammar that references an
    /// unregistered segment panics at *parse* time, not at grammar
    /// construction — sqruff v0.39.0's oracle grammar did exactly that
    /// (`JSONObjectContentSegment`, fixed upstream in v0.40.0; tracked
    /// as ophi-dev/mehen#247). Parse *quality* is irrelevant here: a
    /// dialect may find the file unparsable, but analysis must return,
    /// never panic. The directive pins each dialect in turn; the
    /// `is_<dialect>` assert proves the pin took effect instead of
    /// silently falling back to inference.
    #[test]
    fn every_compiled_dialect_survives_function_call_parse() {
        use strum::IntoEnumIterator;

        use sqruff_lib_core::dialects::init::DialectKind;

        for kind in DialectKind::iter() {
            if dialect::dialect_for_kind(kind).is_none() {
                continue; // not compiled into this build
            }
            let label = dialect_label(kind);
            let sql = format!(
                "-- sqlfluff:dialect:{label}\nselect coalesce(a, 1) from t where nullif(b, 0) > 1;\n"
            );
            let analysis = analyze(&sql);
            assert_eq!(
                metric(&analysis, &format!("sql.dialect.is_{label}")),
                1.0,
                "{label}: directive-pinned dialect must drive the parse"
            );
        }
    }

    /// Every key the analyzer publishes onto the *root* space — the
    /// space thresholds and selectors read — must validate through
    /// `is_published_metric_key`, or `mehen.toml` threshold validation
    /// would reject a real metric. Child-only keys must NOT validate:
    /// a threshold on them could never fire.
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
        let mut seen = 0usize;
        for (key, _) in a.root.metrics.iter() {
            seen += 1;
            assert!(
                is_published_metric_key(key.as_str()),
                "published root key `{key}` is missing from the catalogue"
            );
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
        // Recognized-but-uncompiled dialects can never publish their
        // one-hot key; accepting them would create a dead gate.
        assert!(!is_published_metric_key("sql.dialect.is_duckdb"));
        // Child-only keys (statement spaces) are not root-configurable.
        assert!(!is_published_metric_key("sql.statement.lines"));
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
