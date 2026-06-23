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
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricSpace,
    ParseDiagnostic, Result, SourceFile, SourceSpan, SpaceId, SpaceKind, byte_offset_clamped,
};
use smol_str::SmolStr;

use sqruff_lib_core::dialects::init::DialectKind;
use sqruff_lib_core::parser::Parser;
use sqruff_lib_core::parser::segments::Tables;
use sqruff_lib_dialects::kind_to_dialect;

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

    fn analyze(&self, source: &SourceFile, _config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let file_span = SourceSpan {
            start_byte: 0,
            end_byte: byte_offset_clamped(source.text.len()),
            start_line: 1,
            end_line: source.line_index.line_count(),
        };

        // Resolve the dialect (no explicit request surfaced in 1.0, so this is
        // inference + ansi fallback). `kind_to_dialect` returns None only when
        // the dialect's feature is off; `ansi` is always compiled, so the
        // fallback can never fail.
        let resolution = dialect::resolve(&source.text, requested_dialect());
        let dialect = kind_to_dialect(&resolution.effective, None)
            .or_else(|| kind_to_dialect(&DialectKind::Ansi, None))
            .expect("ansi dialect is always available");

        let parser = Parser::from(&dialect);
        let tables = Tables::default();
        let (tokens, _lex_errs) = dialect.lexer().lex(&tables, source.text.as_str());
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

        let file_facts = facts::extract(&parsed, &dialect, line_at);
        let statement_spans: Vec<(u32, u32)> = file_facts
            .statements
            .iter()
            .map(|s| (s.start_line, s.end_line))
            .collect();
        let loc_stats = loc::compute(&source.text, &parsed, line_at, &statement_spans);

        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, file_span);
        metrics::publish(&file_facts, &loc_stats, &resolution, &mut root.metrics);
        publish_dialect_labels(&mut root, &resolution);

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

        Ok(LanguageAnalysis {
            language: Language::Sql,
            backend: AnalysisBackend::Sqruff,
            diagnostics,
            root,
            contributions: Vec::new(),
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
    metrics::publish(&facts, &loc, resolution, &mut root.metrics);
    publish_dialect_labels(&mut root, resolution);
    LanguageAnalysis {
        language: Language::Sql,
        backend: AnalysisBackend::Sqruff,
        diagnostics: Vec::new(),
        root,
        contributions: Vec::new(),
    }
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
