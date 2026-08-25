// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Parser-neutral SQL fact model and the sqruff → facts adapter.
//!
//! This module is the single seam between sqruff's `SyntaxKind` CST and the
//! mehen metric layer (research foundation §2, §5; parser comparison §6
//! "adapter seam"). Every metric reads [`SqlFileFacts`] — owned, `Send`,
//! `'static` data — and never touches an `ErasedSegment`. That keeps sqruff's
//! `Rc`-based (non-`Send`) tree and its `0.x` API surface confined to the one
//! `extract` call, so a sqruff bump can only break this file.
//!
//! The walk is a single recursive descent that classifies nodes by
//! `SyntaxKind` and records facts. The CTE dependency graph is re-derived from
//! the `CommonTableExpression` CST nodes rather than using sqruff's
//! `Query`/`crawl_sources` model, which relies on interior mutability
//! (`Rc<RefCell<…>>`) that would conflict with borrows held across the call
//! (see [`extract_cte_graph`]).

use mehen_core::SourceSpan;
use sqruff_lib_core::dialects::init::DialectKind;
use sqruff_lib_core::dialects::syntax::{SyntaxKind, SyntaxSet};
use sqruff_lib_core::parser::segments::ErasedSegment;

/// Normalized statement kind (research foundation §5.2). Mapped from the
/// concrete sqruff statement node kinds so metrics classify DDL/DML/DCL/TCL
/// without knowing sqruff's per-dialect variant spellings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum StatementKind {
    Select,
    WithSelect,
    Insert,
    Update,
    Delete,
    Merge,
    CreateView,
    CreateTable,
    CreateTableAsSelect,
    CreateOther,
    AlterTable,
    Drop,
    Truncate,
    Grant,
    Revoke,
    TransactionControl,
    Explain,
    Procedural,
    /// `DECLARE … BEGIN … END` anonymous blocks and top-level procedural
    /// scripting statements (T-SQL `IF`/`WHILE`/`BEGIN` batch statements,
    /// BigQuery scripting). Unlike a routine definition, these *execute when
    /// the file is applied*, so their body DML/TCL feeds the object-touch
    /// and change-risk scans (research foundation §5.2 `anonymous_block`).
    AnonymousBlock,
    SetOperation,
    Unknown,
}

impl StatementKind {
    /// Every variant, for catalogue validation of the
    /// `sql.statement.kind_count.<label>` dynamic metric family.
    pub(crate) const ALL: &[StatementKind] = &[
        StatementKind::Select,
        StatementKind::WithSelect,
        StatementKind::Insert,
        StatementKind::Update,
        StatementKind::Delete,
        StatementKind::Merge,
        StatementKind::CreateView,
        StatementKind::CreateTable,
        StatementKind::CreateTableAsSelect,
        StatementKind::CreateOther,
        StatementKind::AlterTable,
        StatementKind::Drop,
        StatementKind::Truncate,
        StatementKind::Grant,
        StatementKind::Revoke,
        StatementKind::TransactionControl,
        StatementKind::Explain,
        StatementKind::Procedural,
        StatementKind::AnonymousBlock,
        StatementKind::SetOperation,
        StatementKind::Unknown,
    ];

    /// Stable label used in `sql.statement.kind_count.<label>` keys.
    pub(crate) fn label(self) -> &'static str {
        match self {
            StatementKind::Select => "select",
            StatementKind::WithSelect => "with_select",
            StatementKind::Insert => "insert",
            StatementKind::Update => "update",
            StatementKind::Delete => "delete",
            StatementKind::Merge => "merge",
            StatementKind::CreateView => "create_view",
            StatementKind::CreateTable => "create_table",
            StatementKind::CreateTableAsSelect => "create_table_as",
            StatementKind::CreateOther => "create_other",
            StatementKind::AlterTable => "alter_table",
            StatementKind::Drop => "drop",
            StatementKind::Truncate => "truncate",
            StatementKind::Grant => "grant",
            StatementKind::Revoke => "revoke",
            StatementKind::TransactionControl => "transaction_control",
            StatementKind::Explain => "explain",
            StatementKind::Procedural => "procedural",
            StatementKind::AnonymousBlock => "anonymous_block",
            StatementKind::SetOperation => "set_operation",
            StatementKind::Unknown => "unknown",
        }
    }
}

/// One join occurrence and its classified kind (research foundation §6.5).
#[derive(Clone, Debug, Default)]
pub(crate) struct JoinFacts {
    pub inner: u32,
    pub left: u32,
    pub right: u32,
    pub full: u32,
    pub cross: u32,
    pub natural: u32,
    pub lateral: u32,
    /// Joins lacking an `ON`/`USING` condition where one is expected.
    pub missing_condition: u32,
    /// Join predicates with no equality comparison between columns.
    pub non_equi: u32,
    pub total: u32,
}

/// Predicate / boolean-logic facts (research foundation §6.7). `IN`/`LIKE`/
/// `BETWEEN` predicates fold into `comparison_count` per §6.7; IN-subqueries
/// are counted separately as `sql.subquery.in_count`.
#[derive(Clone, Debug, Default)]
pub(crate) struct PredicateFacts {
    pub boolean_operator_count: u32,
    pub max_boolean_depth: u32,
    pub not_count: u32,
    /// Byte ranges of the counted `NOT` tokens — one `MetricEvidence` entry
    /// each under `sql.predicate.not_count` (Codex P1). Lines are resolved
    /// at collection time in `lib.rs`.
    pub not_spans: Vec<(u32, u32)>,
    pub comparison_count: u32,
    /// `NOT IN`, `= NULL`, `<> NULL` and similar dialect-risky NULL logic.
    pub null_semantics_risk_count: u32,
}

/// CASE-expression facts (research foundation §6.8).
#[derive(Clone, Debug, Default)]
pub(crate) struct CaseFacts {
    pub count: u32,
    pub max_depth: u32,
    pub when_count: u32,
    pub max_when_count: u32,
    pub missing_else_count: u32,
    /// Sum over each CASE of `max(0, its_when_count - 2)`. Computed per-CASE so
    /// the cognitive "WHEN arms beyond two" term (§8.2 rule 6) is correct: a
    /// global `when_count - 2*count` would let a many-armed CASE be cancelled
    /// by single-armed ones.
    pub surplus_when_arms: u32,
}

/// Aggregation / grouping facts (research foundation §6.9).
#[derive(Clone, Debug, Default)]
pub(crate) struct AggregateFacts {
    pub function_count: u32,
    pub distinct_count: u32,
    pub group_by_count: u32,
    pub rollup_count: u32,
    pub cube_count: u32,
    pub grouping_sets_count: u32,
    pub having_count: u32,
}

/// Window-function facts (research foundation §6.10).
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowFacts {
    pub function_count: u32,
    pub frame_count: u32,
    pub partition_expression_count: u32,
    pub order_expression_count: u32,
}

/// Set-operation facts (research foundation §6.11).
#[derive(Clone, Debug, Default)]
pub(crate) struct SetOpFacts {
    pub count: u32,
    pub union_count: u32,
    pub union_all_count: u32,
    pub intersect_count: u32,
    pub except_count: u32,
}

/// Subquery / derived-table facts (research foundation §6.6).
#[derive(Clone, Debug, Default)]
pub(crate) struct SubqueryFacts {
    pub count: u32,
    pub max_depth: u32,
    pub correlated_count: u32,
    pub scalar_count: u32,
    pub exists_count: u32,
    pub in_count: u32,
    pub derived_table_count: u32,
}

/// Expression / function-call facts (research foundation §6.12).
#[derive(Clone, Debug, Default)]
pub(crate) struct ExpressionFacts {
    pub max_depth: u32,
    pub function_call_count: u32,
    pub distinct_function_count: u32,
    pub max_function_nesting: u32,
    pub cast_count: u32,
}

/// Output-shape and identifier facts (research foundation §6.13).
#[derive(Clone, Debug, Default)]
pub(crate) struct OutputFacts {
    pub star_count: u32,
    pub outer_star_count: u32,
    pub expression_without_alias_count: u32,
    pub derived_expression_count: u32,
    pub aliased_derived_expression_count: u32,
    pub total_column_refs: u32,
    pub unqualified_column_refs: u32,
    pub multi_relation_column_refs: u32,
    pub multi_relation_unqualified_refs: u32,
    pub quoted_identifier_count: u32,
    pub table_alias_count: u32,
}

/// CTE-graph facts derived from the `CommonTableExpression` CST nodes
/// (research foundation §6.4).
#[derive(Clone, Debug, Default)]
pub(crate) struct CteFacts {
    pub count: u32,
    pub recursive_count: u32,
    pub dependency_edges: u32,
    pub max_dependency_depth: u32,
    pub max_fan_out: u32,
    pub unused_count: u32,
    /// CTEs that only rename/select from a single source with no filtering,
    /// aggregation, or join (§6.4) — they add naming overhead without
    /// structural value.
    pub trivial_count: u32,
}

/// DML/DDL/DCL/TCL object-touch facts (research foundation §6.14).
#[derive(Clone, Debug, Default)]
pub(crate) struct ObjectFacts {
    pub insert_count: u32,
    pub update_count: u32,
    pub delete_count: u32,
    pub merge_count: u32,
    pub update_without_where_count: u32,
    pub delete_without_where_count: u32,
    pub create_count: u32,
    pub alter_count: u32,
    pub drop_count: u32,
    pub truncate_count: u32,
    pub create_or_replace_count: u32,
    pub grant_revoke_count: u32,
    pub transaction_control_count: u32,
    pub returning_count: u32,
    /// Distinct objects written (created/altered/dropped/inserted/updated/…).
    pub write_object_count: u32,
    /// Distinct objects read (table references in FROM/JOIN positions).
    pub read_object_count: u32,
    /// Distinct objects touched — `|read ∪ write|` (research foundation §6.14).
    pub touch_count: u32,
}

/// Stable classification for one term in `sql.change_risk_score`.
///
/// The factor owns both its public reason code and score weight so the
/// aggregate formula and emitted evidence cannot drift independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeRiskFactor {
    Drop,
    Truncate,
    Alter,
    DeleteWithoutWhere,
    UpdateWithoutWhere,
    GrantRevoke,
    DynamicSql,
    Merge,
    CreateOrReplace,
    TransactionControl,
    WriteObject,
    ReadObject,
}

impl ChangeRiskFactor {
    pub(crate) fn amount(self) -> f64 {
        match self {
            Self::Drop | Self::Truncate => 8.0,
            Self::Alter | Self::DeleteWithoutWhere | Self::UpdateWithoutWhere => 6.0,
            Self::GrantRevoke | Self::DynamicSql => 5.0,
            Self::Merge | Self::CreateOrReplace => 4.0,
            Self::TransactionControl => 3.0,
            Self::WriteObject => 2.0,
            Self::ReadObject => 1.0,
        }
    }

    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Drop => "sql.change_risk.drop",
            Self::Truncate => "sql.change_risk.truncate",
            Self::Alter => "sql.change_risk.alter",
            Self::DeleteWithoutWhere => "sql.change_risk.delete_without_where",
            Self::UpdateWithoutWhere => "sql.change_risk.update_without_where",
            Self::GrantRevoke => "sql.change_risk.grant_revoke",
            Self::DynamicSql => "sql.change_risk.dynamic_sql",
            Self::Merge => "sql.change_risk.merge",
            Self::CreateOrReplace => "sql.change_risk.create_or_replace",
            Self::TransactionControl => "sql.change_risk.transaction_control",
            Self::WriteObject => "sql.change_risk.write_object",
            Self::ReadObject => "sql.change_risk.read_object",
        }
    }
}

/// One source-resolved term in `sql.change_risk_score`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChangeRiskEvidence {
    pub span: SourceSpan,
    pub factor: ChangeRiskFactor,
}

/// One evidence entry for a raw object-family counter (`sql.dml.*`,
/// `sql.ddl.*`, `sql.dcl.*`, `sql.transaction.*`): the statement or block
/// DML node that moved the metric (Codex P1). `metric == Σ evidence` holds
/// per key because every increment site records one entry.
#[derive(Clone, Debug)]
pub(crate) struct ObjectEvidence {
    pub metric: &'static str,
    pub reason: &'static str,
    pub span: SourceSpan,
}

/// Per-statement facts with source span (research foundation §5.2).
#[derive(Clone, Debug)]
pub(crate) struct StatementFacts {
    pub kind: StatementKind,
    /// 1-based inclusive line span of the statement in the source.
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// One procedural unit — a routine definition with a body: standalone
/// `CREATE FUNCTION`/`PROCEDURE`/`TRIGGER`, a routine nested in a package
/// or type body, or a subprogram declared inside another routine's DECLARE
/// section. Collected in *pre-order* (an enclosing unit precedes the units
/// it contains), so callers can rebuild the nesting from byte containment.
///
/// These become `SpaceKind::Function` spaces: the function-shaped scopes
/// that per-function coverage enrichment (and, later, CRAP) attach to.
#[derive(Clone, Debug)]
pub(crate) struct ProceduralUnitFacts {
    /// Declared name (`betwnstr`, `dbo.do_thing`), when the grammar exposes
    /// one as a direct child of the definition node.
    pub name: Option<String>,
    /// 1-based inclusive line span of the whole definition.
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    /// Per-unit procedural composite tallies (Phase 3): the share of the
    /// file's cyclomatic/cognitive increments whose source position falls
    /// inside this unit (innermost-unit attribution), plus this unit's own
    /// entry path. Zero for units whose bodies the parser lost to a sibling
    /// `Unparsable` run — those increments stay file-level.
    pub cyclomatic_complexity: f64,
    pub cognitive_complexity: f64,
    /// `sql.structural_complexity` of the query constructs embedded in this
    /// unit's subtree (§9.3 `max_embedded_query` feeds from these).
    pub embedded_query_structural: f64,
}

/// Halstead operator/operand tallies (research foundation §7). Operators and
/// operands are deduplicated by their normalized text so `η1`/`η2` are the
/// distinct counts.
#[derive(Clone, Debug, Default)]
pub(crate) struct HalsteadFacts {
    pub distinct_operators: u32,
    pub distinct_operands: u32,
    pub total_operators: u32,
    pub total_operands: u32,
}

/// The complete parser-neutral fact set for one `.sql` file.
#[derive(Clone, Debug, Default)]
pub(crate) struct SqlFileFacts {
    pub statements: Vec<StatementFacts>,
    /// Procedural units in pre-order (see [`ProceduralUnitFacts`]).
    pub procedural_units: Vec<ProceduralUnitFacts>,
    /// Procedural control-flow facts (research foundation §6.17, Phase 3).
    pub procedural: crate::procedural::ProceduralFacts,
    pub query_block_count: u32,
    pub query_block_max_depth: u32,
    pub select_item_total: u32,
    pub select_item_max: u32,
    pub joins: JoinFacts,
    pub subqueries: SubqueryFacts,
    pub predicates: PredicateFacts,
    pub cases: CaseFacts,
    pub aggregates: AggregateFacts,
    pub windows: WindowFacts,
    pub set_ops: SetOpFacts,
    pub expressions: ExpressionFacts,
    pub output: OutputFacts,
    pub ctes: CteFacts,
    pub objects: ObjectFacts,
    pub change_risk_evidence: Vec<ChangeRiskEvidence>,
    /// Evidence for the raw object-family counters (Codex P1).
    pub object_evidence: Vec<ObjectEvidence>,
    pub halstead: HalsteadFacts,
    pub relation_ref_count: u32,
    /// Count of `SyntaxKind::Unparsable` segments (parser-health, §6.16).
    pub unparsable_segments: u32,
    /// Lines touched by unparsable segments.
    pub unparsable_lines: u32,
    /// Count of lexer errors reported by sqruff for malformed tokens. The
    /// current sqruff release always returns an empty lex-error vector
    /// (malformed input becomes `Unparsable` parse segments instead), so this
    /// is 0 in practice today — it is plumbed so a future sqruff version that
    /// does surface lex errors cannot make invalid SQL look clean to the
    /// parser-health metrics (Codex P2).
    pub lex_error_count: u32,
}

// ── SyntaxSet constants ────────────────────────────────────────────────
//
// `children()` requires a `&'static SyntaxSet`, and const construction keeps
// the bitsets out of the hot path.

const SELECT_STATEMENT: SyntaxSet = SyntaxSet::single(SyntaxKind::SelectStatement);

// ── dialect-folding kind sets ──────────────────────────────────────────
//
// sqruff's Oracle dialect emits its own parallel statement/reference kinds
// (`OracleUpdateStatement`, `OracleTableReference`, …) instead of the ANSI
// ones, and T-SQL adds `BulkInsertStatement`. Every scan that matched only
// the ANSI kind silently skipped Oracle DML — top-level `UPDATE`/`INSERT`/
// `DELETE`/`COMMIT` in an Oracle file classified as `unknown` and appeared
// in no `sql.dml.*`, object-touch, or change-risk metric. These sets fold
// the dialect variants so every consumer sees one vocabulary (verified
// against the sqruff v0.40.0 `SyntaxKind` inventory: Oracle is the only
// dialect with parallel DML kinds).

/// `table_reference` in any dialect spelling.
const TABLE_REFERENCES: SyntaxSet =
    SyntaxSet::new(&[SyntaxKind::TableReference, SyntaxKind::OracleTableReference]);

const INSERT_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::InsertStatement,
    SyntaxKind::OracleInsertStatement,
    SyntaxKind::BulkInsertStatement,
]);

const UPDATE_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::UpdateStatement,
    SyntaxKind::OracleUpdateStatement,
]);

const DELETE_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::DeleteStatement,
    SyntaxKind::OracleDeleteStatement,
]);

const TRANSACTION_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::TransactionStatement,
    SyntaxKind::OracleTransactionStatement,
]);

const CREATE_TABLE_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::CreateTableStatement,
    SyntaxKind::OracleCreateTableStatement,
]);

const CREATE_VIEW_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::CreateViewStatement,
    SyntaxKind::CreateMaterializedViewStatement,
    SyntaxKind::OracleCreateViewStatement,
]);

const ALTER_TABLE_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::AlterTableStatement,
    SyntaxKind::OracleAlterTableStatement,
]);

const DROP_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::DropTableStatement,
    SyntaxKind::DropViewStatement,
    SyntaxKind::DropIndexStatement,
    SyntaxKind::DropStatement,
    SyntaxKind::DropFunctionStatement,
    SyntaxKind::DropSchemaStatement,
    SyntaxKind::OracleDropPackageStatement,
    SyntaxKind::OracleDropProcedureStatement,
    SyntaxKind::OracleDropSynonymStatement,
    SyntaxKind::OracleDropDatabaseLinkStatement,
]);

/// Non-table/view CREATE kinds that the per-statement path classifies as
/// `create_other` (by its raw-text CREATE fallback). The node-based
/// anonymous-block scan cannot use raw-text classification, so it mirrors
/// the family with the typed kinds an executing block can realistically
/// contain — `IF … BEGIN CREATE INDEX ix ON t(c); END` is real migration DDL
/// (Codex P2). Routine/trigger definitions are deliberately absent: they are
/// procedural and the `PROCEDURAL_DEFINITIONS` boundary excludes them.
const CREATE_OTHER_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::CreateIndexStatement,
    SyntaxKind::CreateSequenceStatement,
    SyntaxKind::CreateSchemaStatement,
    SyntaxKind::CreateSynonymStatement,
    SyntaxKind::CreateDatabaseStatement,
    SyntaxKind::CreateDomainStatement,
    SyntaxKind::CreateExtensionStatement,
    SyntaxKind::CreateTypeStatement,
    SyntaxKind::CreateUserStatement,
    SyntaxKind::CreateRoleStatement,
]);

/// Build facts for `root` (the parsed `File` segment). `dialect` is the
/// effective dialect: T-SQL's batch model changes statement-ownership
/// semantics (a routine body extends to the next `GO`), and BigQuery's bare
/// scripting `BEGIN`/`END` statements parse as transaction-statement
/// wrappers that must not classify as TCL (Codex P1).
pub(crate) fn extract(
    root: &ErasedSegment,
    line_at: impl Fn(u32) -> u32,
    emit_contributions: bool,
    dialect: DialectKind,
) -> SqlFileFacts {
    let tsql = dialect == DialectKind::Tsql;
    let mysql = dialect == DialectKind::Mysql;
    let oracle = dialect == DialectKind::Oracle;
    let bigquery = dialect == DialectKind::Bigquery;
    let mut facts = SqlFileFacts::default();

    // ── procedural units (function-shaped scopes) ───────────────────
    // Extracted *before* statement classification: BigQuery-style grammars
    // wrap routines in segments without a top-level `Statement` node, so the
    // routine's body statements surface as top-level statements themselves —
    // classification needs the unit ranges to recognize them as
    // routine-owned (Codex P1).
    extract_procedural_units(root, &line_at, tsql, mysql, oracle, &mut facts);

    // ── statements ──────────────────────────────────────────────────
    let unit_ranges: Vec<(u32, u32)> = facts
        .procedural_units
        .iter()
        .map(|u| (u.start_byte, u.end_byte))
        .collect();
    classify_statements(root, &line_at, tsql, bigquery, &unit_ranges, &mut facts);

    // ── procedural control flow (research foundation §6.17) ─────────
    // Needs statement classification and units; contributes dynamic-SQL
    // change-risk evidence alongside `extract_objects`' below.
    crate::procedural::extract(root, &line_at, emit_contributions, dialect, &mut facts);

    // ── unparsable / parser health ──────────────────────────────────
    let unparsables = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Unparsable),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    facts.unparsable_segments = unparsables.len() as u32;
    let mut unparsable_lines = 0u32;
    for seg in &unparsables {
        if let Some(pm) = seg.get_position_marker() {
            let start = line_at(pm.source_slice.start as u32);
            let end = line_at(pm.source_slice.end.saturating_sub(1) as u32);
            unparsable_lines += end.saturating_sub(start) + 1;
        }
    }
    facts.unparsable_lines = unparsable_lines;

    // ── query blocks + depth ────────────────────────────────────────
    let selects = root.recursive_crawl(&SELECT_STATEMENT, true, &SyntaxSet::EMPTY, true);
    facts.query_block_count = selects.len() as u32;
    for sel in &selects {
        let depth = ancestor_select_depth(root, sel);
        facts.query_block_max_depth = facts.query_block_max_depth.max(depth);
        // `SelectClauseElement` nodes live under this SELECT's `select_clause`
        // (a grandchild, not a direct child), so crawl — but stop at nested
        // SELECT nodes so a subquery's projections aren't attributed to the parent.
        let items = sel
            .recursive_crawl(
                &SyntaxSet::single(SyntaxKind::SelectClauseElement),
                true,
                &SELECT_STATEMENT,
                true,
            )
            .len() as u32;
        facts.select_item_total += items;
        facts.select_item_max = facts.select_item_max.max(items);
    }

    // ── joins ───────────────────────────────────────────────────────
    extract_joins(root, &mut facts.joins);

    // ── set operations ──────────────────────────────────────────────
    extract_set_ops(root, &mut facts.set_ops);

    // ── CASE ────────────────────────────────────────────────────────
    extract_cases(root, &mut facts.cases);

    // ── window functions ────────────────────────────────────────────
    extract_windows(root, &mut facts.windows);

    // ── aggregates / grouping ───────────────────────────────────────
    extract_aggregates(root, &mut facts.aggregates);

    // ── predicates / boolean logic ──────────────────────────────────
    extract_predicates(root, &mut facts.predicates);

    // ── subqueries / derived tables ─────────────────────────────────
    extract_subqueries(root, &selects, &mut facts.subqueries);

    // ── expressions / functions ─────────────────────────────────────
    extract_expressions(root, &mut facts.expressions);

    // ── output shape / identifiers ──────────────────────────────────
    extract_output(root, &selects, &mut facts.output);

    // ── relation references ─────────────────────────────────────────
    facts.relation_ref_count =
        count_any(root, &TABLE_REFERENCES) + facts.subqueries.derived_table_count;

    // ── CTE graph (via sqruff Query analysis) ───────────────────────
    extract_cte_graph(root, &mut facts.ctes);

    // ── object-touch / DML-DDL risk ─────────────────────────────────
    extract_objects(root, &line_at, bigquery, &mut facts, emit_contributions);

    // ── Halstead ────────────────────────────────────────────────────
    extract_halstead(root, &mut facts.halstead);

    facts
}

// ── statement classification ───────────────────────────────────────────

fn classify_statements(
    root: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    tsql: bool,
    bigquery: bool,
    unit_ranges: &[(u32, u32)],
    facts: &mut SqlFileFacts,
) {
    // Top-level `Statement` nodes are direct children of `File`; do not
    // recurse into nested statements (a subquery `SELECT` is a query block,
    // not a top-level statement).
    let statements = top_level_statements(root, bigquery);
    // Whether the previous statement was (or continued) a routine
    // definition. sqruff's tsql grammar splits long procedure bodies into
    // sibling statements; T-SQL batch semantics say the body extends to the
    // next `GO`/EOF — `CREATE PROCEDURE` must be alone in its batch — so
    // under the tsql dialect *every* statement after a routine definition is
    // the routine's body until a `GO` separator (Codex P1). Other dialects
    // have real statement terminators, so only *control-shaped* fragments
    // (keyword-led T-SQL shapes, MySQL's per-branch typed statements)
    // reclassify there; a plain `UPDATE` after an Oracle routine is
    // independent. Typed blocks (Oracle `BEGIN…END`) never reclassify —
    // they are genuine anonymous blocks wherever they appear. `unknown`
    // statements (parse debris between spills) keep the chain alive.
    let mut prev_procedural = false;
    for stmt in &statements {
        let (start_byte, end_byte) = stmt
            .get_position_marker()
            .map(|pm| (pm.source_slice.start as u32, pm.source_slice.end as u32))
            .unwrap_or((0, 0));
        let mut kind = classify_statement(stmt, bigquery);
        // A statement whose bytes live *inside* a routine unit is the
        // routine's body — BigQuery-style grammars wrap `CREATE PROCEDURE`
        // in a segment without a top-level `Statement` node, so the body's
        // statements surface as top-level statements themselves. They run
        // when the routine is *called*, not when the file is applied, so
        // they must not publish executing DML, touched objects, or
        // missing-WHERE risk (Codex P1). Strict containment: a definition
        // statement contains its unit, never the reverse.
        let routine_owned = kind != StatementKind::Procedural
            && unit_ranges.iter().any(|&(s, e)| {
                s <= start_byte && end_byte <= e && (s, e) != (start_byte, end_byte)
            });
        if routine_owned {
            kind = StatementKind::Procedural;
        }
        let is_go = kind == StatementKind::Unknown && is_go_separator(stmt);
        if prev_procedural && !is_go && kind != StatementKind::Procedural {
            let continuation = if tsql {
                true
            } else {
                kind == StatementKind::AnonymousBlock
                    && matches!(
                        anonymous_block_shape(stmt),
                        Some(AnonymousBlockShape::KeywordLed | AnonymousBlockShape::TypedControl)
                    )
            };
            if continuation {
                kind = StatementKind::Procedural;
            }
        }
        prev_procedural = match kind {
            _ if is_go => false,
            // Body statements owned by containment never seed a
            // continuation chain — attribution is complete without one.
            StatementKind::Procedural if routine_owned => prev_procedural,
            StatementKind::Procedural => true,
            StatementKind::Unknown => prev_procedural,
            _ => false,
        };
        facts.statements.push(StatementFacts {
            kind,
            start_line: line_at(start_byte),
            end_line: line_at(end_byte.saturating_sub(1)),
            start_byte,
            end_byte,
        });
    }
}

/// Classify a `Statement` node by inspecting which statement-body kind it
/// contains. sqruff produces dialect-specific `Drop*`/`Create*`/Oracle DML
/// variants, so we probe with the dialect-folding `SyntaxSet`s above and map
/// by the first match.
fn classify_statement(stmt: &ErasedSegment, bigquery: bool) -> StatementKind {
    let has_any = |set: &SyntaxSet| {
        !stmt
            .recursive_crawl(set, false, &SyntaxSet::EMPTY, true)
            .is_empty()
    };
    let has = |k: SyntaxKind| has_any(&SyntaxSet::single(k));

    // Anonymous blocks and top-level scripting statements come *before* the
    // routine-definition check: an anonymous block may *declare* a nested
    // procedure in its DECLARE section, and the nested definition must not
    // make the executing outer block look like a routine definition (its
    // UPDATE runs on apply! — Codex P1). The shape walk skips
    // `PROCEDURAL_DEFINITIONS` subtrees, so a routine's *own* body block
    // never marks the routine statement as an anonymous block, and the DML
    // sniffing below stays unreachable for blocks (`BEGIN UPDATE t …; END;`
    // contains an UpdateStatement, but the statement is the block).
    if stmt_is_anonymous_block(stmt, bigquery) {
        return StatementKind::AnonymousBlock;
    }

    // Routine definitions: `CREATE PROCEDURE` / `FUNCTION` / `TRIGGER`
    // bodies commonly contain `INSERT`/`UPDATE`/…, but the top-level
    // statement is the routine definition, not the nested DML — classifying
    // it as DML would also wrongly feed `extract_objects`' DML/no-WHERE risk
    // metrics (Codex P2).
    if stmt_is_procedural(stmt) {
        return StatementKind::Procedural;
    }

    // Order matters: more specific kinds first. The `WithCompoundStatement`
    // (CTE) check is deliberately *after* the DML/DDL checks: a CTE can be
    // attached to another statement form — `CREATE TABLE dst AS WITH c AS (…)
    // SELECT …`, or dialects allowing `WITH … INSERT/UPDATE/DELETE` — and the
    // outer statement kind is the meaningful classification, not `with_select`
    // (Codex P2).
    if has(SyntaxKind::MergeStatement) {
        return StatementKind::Merge;
    }
    if has_any(&INSERT_STATEMENTS) {
        return StatementKind::Insert;
    }
    if has_any(&UPDATE_STATEMENTS) {
        return StatementKind::Update;
    }
    if has_any(&DELETE_STATEMENTS) {
        return StatementKind::Delete;
    }
    if has(SyntaxKind::TruncateStatement) {
        return StatementKind::Truncate;
    }
    if has_any(&ALTER_TABLE_STATEMENTS) {
        return StatementKind::AlterTable;
    }
    // CREATE family: distinguish CTAS, view, table, other.
    if has_any(&CREATE_TABLE_STATEMENTS) {
        // CTAS = CREATE TABLE … AS SELECT — the statement embeds a select.
        if has(SyntaxKind::SelectStatement) || has(SyntaxKind::WithCompoundStatement) {
            return StatementKind::CreateTableAsSelect;
        }
        return StatementKind::CreateTable;
    }
    if has_any(&CREATE_VIEW_STATEMENTS) {
        return StatementKind::CreateView;
    }
    if stmt_contains_create(stmt) {
        return StatementKind::CreateOther;
    }
    if has_any(&DROP_STATEMENTS) {
        return StatementKind::Drop;
    }
    if has(SyntaxKind::AccessStatement) {
        // GRANT / REVOKE both parse as AccessStatement; distinguish by keyword.
        let raw = stmt.raw().to_ascii_uppercase();
        if raw.trim_start().starts_with("REVOKE") {
            return StatementKind::Revoke;
        }
        return StatementKind::Grant;
    }
    if has_any(&TRANSACTION_STATEMENTS) {
        // BigQuery's bare scripting `END;` also parses as a transaction
        // statement — it is the closer of a scripting block, not TCL, so it
        // must not add transaction-control risk (Codex P1). The matching
        // bare `BEGIN;` never reaches here (classified anonymous above).
        // Falls through to `unknown`: a closer is no statement of its own.
        if !(bigquery && bare_scripting_bracket(stmt).is_some()) {
            return StatementKind::TransactionControl;
        }
    }
    if has(SyntaxKind::ExplainStatement) {
        return StatementKind::Explain;
    }
    // Read-query shape: classify by the statement's *top-level* body, not any
    // nested selectable. `WITH c AS (SELECT … UNION …) SELECT …` is a
    // `with_select`, and `SELECT * FROM (SELECT … UNION …) q` is a plain
    // `select` — a nested UNION inside a CTE or derived table must not make
    // either look like a top-level `set_operation` (Codex P2).
    if let Some(kind) = top_level_query_kind(stmt) {
        return kind;
    }
    StatementKind::Unknown
}

/// Classify a statement's outermost query body: the first of
/// `WithCompoundStatement` / `SetExpression` / `SelectStatement` reached while
/// descending the statement's direct structure, *without* entering a
/// `Bracketed` group (a derived table / parenthesized subquery) or a deeper
/// selectable. Returns `None` if the statement has no query body.
fn top_level_query_kind(stmt: &ErasedSegment) -> Option<StatementKind> {
    fn walk(node: &ErasedSegment) -> Option<StatementKind> {
        for child in node.segments() {
            if child.is_type(SyntaxKind::WithCompoundStatement) {
                return Some(StatementKind::WithSelect);
            }
            if child.is_type(SyntaxKind::SetExpression) {
                return Some(StatementKind::SetOperation);
            }
            if child.is_type(SyntaxKind::SelectStatement) {
                return Some(StatementKind::Select);
            }
            // Do not descend into bracketed groups — a nested selectable there
            // (derived table, scalar subquery) is not the statement's body.
            if child.is_type(SyntaxKind::Bracketed) {
                continue;
            }
            if let Some(kind) = walk(child) {
                return Some(kind);
            }
        }
        None
    }
    walk(stmt)
}

fn stmt_contains_create(stmt: &ErasedSegment) -> bool {
    stmt.raw()
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE")
}

fn stmt_is_procedural(stmt: &ErasedSegment) -> bool {
    !stmt
        .recursive_crawl(&PROCEDURAL_DEFINITIONS, false, &SyntaxSet::EMPTY, true)
        .is_empty()
}

/// Typed `BEGIN…END` block node kinds — genuine anonymous blocks wherever
/// they appear (Oracle `DECLARE…BEGIN…END`, T-SQL/ANSI blocks).
const TYPED_BLOCK_KINDS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::OracleBeginEndBlock,
    SyntaxKind::BeginEndBlock,
    SyntaxKind::AtomicBeginEndBlock,
]);

/// Typed scripting/control statement node kinds. At top level these are
/// either genuine scripting (BigQuery) or — far more often — fragments of a
/// routine body that a dialect grammar split into sibling statements (MySQL
/// splits *every branch* of an `IF` into its own `IfThenStatement`
/// statement). The distinction is positional: fragments directly follow a
/// routine definition.
const TYPED_CONTROL_KINDS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::IfStatements,
    SyntaxKind::IfStatement,
    SyntaxKind::IfThenStatement,
    SyntaxKind::WhileStatements,
    SyntaxKind::WhileStatement,
    SyntaxKind::LoopStatements,
    SyntaxKind::LoopStatement,
    SyntaxKind::RepeatStatement,
    SyntaxKind::ForInStatement,
]);

/// How a statement qualifies as an anonymous block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnonymousBlockShape {
    /// A typed `BEGIN…END` block node (Oracle, T-SQL/ANSI) — a genuine
    /// anonymous block wherever it appears.
    TypedBlock,
    /// A typed scripting/control statement (`IfThenStatement`,
    /// `WhileStatement`, …) — genuine scripting when standalone, a routine
    /// body fragment when it directly follows a routine definition.
    TypedControl,
    /// A T-SQL keyword-led control statement (`IF`/`WHILE`/`BEGIN` + nested
    /// statements without a dedicated node kind) — same positional rule as
    /// `TypedControl`.
    KeywordLed,
}

/// Whether a (non-routine) statement is an anonymous block / top-level
/// scripting statement, and which shape it takes.
///
/// Three shapes, per the CST probes (parser comparison §9):
/// - a typed block node reached without crossing a `Bracketed` group (a
///   parenthesized subquery is not the statement's body) — Oracle blocks;
/// - a typed scripting/control statement — BigQuery scripting, MySQL body
///   fragments;
/// - a T-SQL keyword-led statement: the first substantive child is the bare
///   keyword `IF`/`WHILE`/`BEGIN` (sqruff's tsql dialect nests the controlled
///   statements under it without a dedicated node kind). `BEGIN` is checked
///   against `TRANSACTION`/`TRAN`/`WORK`/`DIALOG` so T-SQL transaction
///   control (which also parses keyword-led in fragments) stays TCL.
fn anonymous_block_shape(stmt: &ErasedSegment) -> Option<AnonymousBlockShape> {
    fn contains_kind(node: &ErasedSegment, kinds: &SyntaxSet) -> bool {
        for child in node.segments() {
            if kinds.contains(child.get_type()) {
                return true;
            }
            // A parenthesized subquery is not the statement's body, and a
            // routine definition's own body block belongs to the routine —
            // a `CREATE PROCEDURE` must never look like an anonymous block
            // because of the `BEGIN…END`/scripting nodes inside it
            // (Codex P1).
            if child.is_type(SyntaxKind::Bracketed)
                || PROCEDURAL_DEFINITIONS.contains(child.get_type())
            {
                continue;
            }
            if contains_kind(child, kinds) {
                return true;
            }
        }
        false
    }
    if contains_kind(stmt, &TYPED_BLOCK_KINDS) {
        return Some(AnonymousBlockShape::TypedBlock);
    }
    if contains_kind(stmt, &TYPED_CONTROL_KINDS) {
        return Some(AnonymousBlockShape::TypedControl);
    }
    // T-SQL keyword-led shape: first two substantive children.
    let mut lead = stmt
        .segments()
        .iter()
        .filter(|s| !s.is_whitespace() && !s.is_meta() && !s.is_comment());
    let first = lead.next()?;
    if !first.is_type(SyntaxKind::Keyword) {
        return None;
    }
    let word = first.raw().to_ascii_uppercase();
    match word.as_str() {
        "IF" | "WHILE" => Some(AnonymousBlockShape::KeywordLed),
        "BEGIN" => {
            let next = lead
                .next()
                .map(|s| s.raw().to_ascii_uppercase())
                .unwrap_or_default();
            if matches!(
                next.as_str(),
                "TRANSACTION" | "TRAN" | "WORK" | "DIALOG" | "DISTRIBUTED"
            ) {
                None
            } else {
                Some(AnonymousBlockShape::KeywordLed)
            }
        }
        _ => None,
    }
}

fn stmt_is_anonymous_block(stmt: &ErasedSegment, bigquery: bool) -> bool {
    anonymous_block_shape(stmt).is_some()
        || (bigquery && bare_scripting_bracket(stmt) == Some(ScriptingBracket::Begin))
}

/// Which bare scripting bracket a BigQuery `TransactionStatement`-wrapped
/// statement is, if any. BigQuery's grammar parses the scripting block
/// openers/closers `BEGIN;`/`END;` as transaction statements whose *sole*
/// keyword is the bracket — real transaction control (`BEGIN TRANSACTION`)
/// carries more keywords and stays TCL (Codex P1). Only meaningful under
/// the BigQuery dialect: a bare `BEGIN;`/`END;` in PostgreSQL *is*
/// transaction control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScriptingBracket {
    Begin,
    End,
}

pub(crate) fn bare_scripting_bracket(stmt: &ErasedSegment) -> Option<ScriptingBracket> {
    let wrapper = stmt
        .segments()
        .iter()
        .find(|s| !s.is_whitespace() && !s.is_meta() && !s.is_comment())?;
    if !TRANSACTION_STATEMENTS.contains(wrapper.get_type()) {
        return None;
    }
    transaction_node_bracket(wrapper)
}

/// The sole-keyword bracket shape of a transaction-statement *node*, if any.
pub(crate) fn transaction_node_bracket(node: &ErasedSegment) -> Option<ScriptingBracket> {
    let keywords: Vec<String> = node
        .segments()
        .iter()
        .filter(|s| s.is_type(SyntaxKind::Keyword))
        .map(|s| s.raw().to_ascii_uppercase())
        .collect();
    match keywords.as_slice() {
        [word] if word == "BEGIN" => Some(ScriptingBracket::Begin),
        [word] if word == "END" => Some(ScriptingBracket::End),
        _ => None,
    }
}

/// Whether a statement is a bare T-SQL `GO` batch separator (its only code
/// token is the keyword `GO`).
pub(crate) fn is_go_separator(stmt: &ErasedSegment) -> bool {
    let mut code = stmt
        .segments()
        .iter()
        .filter(|s| !s.is_whitespace() && !s.is_meta() && !s.is_comment());
    let Some(first) = code.next() else {
        return false;
    };
    code.next().is_none()
        && first.is_type(SyntaxKind::Keyword)
        && first.raw().eq_ignore_ascii_case("GO")
}

// ── joins ──────────────────────────────────────────────────────────────

/// Count and classify explicit `JOIN` clauses.
///
/// Known limitation (Phase 1): implicit comma joins (`FROM a, b`) are *not*
/// counted — sqruff models them as sibling `from_expression_element`s, not
/// `JoinClause` nodes, and inferring an implicit cross join from comma
/// separation risks false positives (e.g. `FROM a, LATERAL f(a.x)`). Explicit
/// `CROSS JOIN` is counted; implicit cross-join detection is deferred (research
/// foundation §6.5 lists it as a derive-later item).
pub(crate) fn extract_joins(root: &ErasedSegment, joins: &mut JoinFacts) {
    let clauses = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::JoinClause),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    joins.total = clauses.len() as u32;
    for clause in &clauses {
        // Classify from the join clause's *keyword* tokens (e.g. `LEFT`,
        // `JOIN`), not the whole raw text — otherwise a relation named
        // `left_table` in a plain `JOIN left_table` would be misread as a LEFT
        // join (Codex P2). The keywords precede the joined `from_expression_*`.
        let keywords = join_keywords(clause);
        let has_kw = |w: &str| keywords.iter().any(|k| k == w);
        let kind_word = if has_kw("LEFT") {
            JoinWord::Left
        } else if has_kw("RIGHT") {
            JoinWord::Right
        } else if has_kw("FULL") {
            JoinWord::Full
        } else if has_kw("CROSS") {
            JoinWord::Cross
        } else if has_kw("OUTER") {
            // T-SQL `OUTER APPLY` is a left-outer lateral join (it preserves
            // left rows), so it counts toward `outer_count` like a LEFT join —
            // not as an inner join (Codex P2). `APPLY` also sets the lateral
            // flag below.
            JoinWord::Left
        } else {
            JoinWord::Inner
        };
        match kind_word {
            JoinWord::Left => joins.left += 1,
            JoinWord::Right => joins.right += 1,
            JoinWord::Full => joins.full += 1,
            JoinWord::Cross => joins.cross += 1,
            JoinWord::Inner => joins.inner += 1,
        }
        let natural = has_kw("NATURAL");
        if natural {
            joins.natural += 1;
        }
        // LATERAL / CROSS APPLY / OUTER APPLY are lateral joins; they correlate
        // by position and legitimately have no `ON`/`USING`.
        let lateral = has_kw("LATERAL") || has_kw("APPLY");
        if lateral {
            joins.lateral += 1;
        }
        // `USING` is a keyword child that appears *after* the joined relation,
        // so it is not part of the leading-keyword run — check all keyword
        // children for it.
        let has_using = clause
            .segments()
            .iter()
            .any(|s| s.is_type(SyntaxKind::Keyword) && s.raw().eq_ignore_ascii_case("USING"));
        let has_condition = !clause
            .recursive_crawl(
                &SyntaxSet::single(SyntaxKind::JoinOnCondition),
                true,
                &SyntaxSet::EMPTY,
                true,
            )
            .is_empty()
            || has_using;
        // CROSS / NATURAL / LATERAL (incl. CROSS|OUTER APPLY) joins legitimately
        // omit a condition; only flag the others.
        if !has_condition && !matches!(kind_word, JoinWord::Cross) && !natural && !lateral {
            joins.missing_condition += 1;
        }
        if has_condition && !join_condition_has_equality(clause, has_using) {
            joins.non_equi += 1;
        }
    }
}

enum JoinWord {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// The uppercased `keyword` tokens that lead a join clause (before the joined
/// relation): `["LEFT", "JOIN"]`, `["CROSS", "JOIN"]`, `["NATURAL", "JOIN"]`,
/// etc. Stops at the first non-keyword child so it never picks up identifiers.
fn join_keywords(clause: &ErasedSegment) -> Vec<String> {
    let mut out = Vec::new();
    for child in clause.segments() {
        if child.is_type(SyntaxKind::Keyword) {
            out.push(child.raw().to_ascii_uppercase());
        } else if child.is_whitespace() || child.is_meta() {
            continue;
        } else {
            // First substantive non-keyword node (the joined relation): the
            // leading keyword run is over.
            break;
        }
    }
    out
}

/// Whether a join condition contains a genuine equality (`=`) between columns.
/// A condition with only inequality operators (`>=`, `!=`, `<`, …) is a
/// non-equi join even though those operators' text contains `=` (Codex P2).
fn join_condition_has_equality(clause: &ErasedSegment, has_using: bool) -> bool {
    // `USING (...)` is inherently an equality join.
    if has_using {
        return true;
    }
    // Only *this* join's own `ON` condition counts: stop at any nested
    // `JoinClause` so a subquery's join condition in the `ON` clause
    // (`… AND EXISTS (SELECT … JOIN d ON c.id = d.id)`) isn't read as the
    // outer join's equality (Codex P2).
    let conds = clause.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::JoinOnCondition),
        true,
        &SyntaxSet::single(SyntaxKind::JoinClause),
        false,
    );
    // An equi-join needs an equality (`=`) *between two column references* —
    // `ON a.id = b.id`. A `=` against a literal or constant (`ON 1 = 1`,
    // `ON a.status = 'active'`) is a filter, not a join key, so it must not
    // suppress the non-equi signal (Codex P2).
    for cond in &conds {
        if expression_has_column_equality(cond) {
            return true;
        }
    }
    false
}

/// Whether `node` contains a `=` comparison whose operands on both sides are
/// column references. Walks expression containers and inspects the code
/// siblings flanking each `=` operator.
fn expression_has_column_equality(node: &ErasedSegment) -> bool {
    // Examine the code-token children at this level for the pattern
    // `<col> = <col>`.
    let code: Vec<&ErasedSegment> = node
        .segments()
        .iter()
        .filter(|s| !s.is_whitespace() && !s.is_meta() && !s.is_comment())
        .collect();
    for (i, seg) in code.iter().enumerate() {
        if seg.is_type(SyntaxKind::ComparisonOperator) && seg.raw().trim() == "=" {
            let left_is_col = i
                .checked_sub(1)
                .and_then(|j| code.get(j))
                .is_some_and(|s| operand_is_column_reference(s));
            let right_is_col = code
                .get(i + 1)
                .is_some_and(|s| operand_is_column_reference(s));
            if left_is_col && right_is_col {
                return true;
            }
        }
    }
    // Recurse into nested expression/bracketed groups (e.g. compound ON with
    // AND/OR, or parenthesized conditions) — but NOT into a nested subquery
    // (`EXISTS (SELECT … ON c.id = d.id)`): an equality inside the subquery is
    // that query's join key, not this join's, and must not mark this join as
    // equi (Codex P2).
    node.segments()
        .iter()
        .filter(|child| !child.is_type(SyntaxKind::SelectStatement))
        .any(expression_has_column_equality)
}

/// Whether an `=` operand is (after unwrapping any parentheses/expression
/// wrappers) a single column reference. `ON (a.id) = (b.id)` parses each side
/// as `Bracketed → Expression → ColumnReference`, so a normal equi-join must
/// not be miscounted as non-equi just because its keys are parenthesized
/// (Codex P2). A wrapper around a literal (`(a.status) = ('x')`) unwraps to a
/// `QuotedLiteral`, so it is still correctly rejected as a non-key filter.
fn operand_is_column_reference(seg: &ErasedSegment) -> bool {
    if seg.is_type(SyntaxKind::ColumnReference) {
        return true;
    }
    if seg.is_type(SyntaxKind::Bracketed) || seg.is_type(SyntaxKind::Expression) {
        // Descend through the single meaningful child (skip brackets/trivia).
        let inner: Vec<&ErasedSegment> = seg
            .segments()
            .iter()
            .filter(|s| {
                !s.is_whitespace()
                    && !s.is_meta()
                    && !s.is_comment()
                    && !s.is_type(SyntaxKind::StartBracket)
                    && !s.is_type(SyntaxKind::EndBracket)
            })
            .collect();
        // Only unwrap an unambiguous single-operand group; a multi-token group
        // (e.g. `a + b`) is not a bare column reference.
        if let [only] = inner.as_slice() {
            return operand_is_column_reference(only);
        }
    }
    false
}

// ── set operations ───────────────────────────────────────────────────

pub(crate) fn extract_set_ops(root: &ErasedSegment, set_ops: &mut SetOpFacts) {
    let ops = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::SetOperator),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    set_ops.count = ops.len() as u32;
    for op in &ops {
        let raw = op.raw().to_ascii_uppercase();
        if raw.contains("UNION") {
            if raw.contains("ALL") {
                set_ops.union_all_count += 1;
            } else {
                set_ops.union_count += 1;
            }
        } else if raw.contains("INTERSECT") {
            set_ops.intersect_count += 1;
        } else if raw.contains("EXCEPT") || raw.contains("MINUS") {
            set_ops.except_count += 1;
        }
    }
}

// ── CASE ────────────────────────────────────────────────────────────────

pub(crate) fn extract_cases(root: &ErasedSegment, cases: &mut CaseFacts) {
    let all = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::CaseExpression),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    cases.count = all.len() as u32;
    for case in &all {
        // Depth: nested CASE inside this one (count this node + deepest child
        // chain). `recursive_crawl` with allow_self gives the subtree count;
        // we measure structural nesting separately.
        let depth = case_depth(case);
        cases.max_depth = cases.max_depth.max(depth);
        let whens = count_anywhere_within_case(case, SyntaxKind::WhenClause);
        cases.when_count += whens;
        cases.max_when_count = cases.max_when_count.max(whens);
        cases.surplus_when_arms += whens.saturating_sub(2);
        let has_else = count_direct(case, SyntaxKind::ElseClause) > 0;
        if !has_else {
            cases.missing_else_count += 1;
        }
    }
}

/// Maximum nesting depth of CASE expressions rooted at `case` (1 for a
/// non-nested CASE).
fn case_depth(case: &ErasedSegment) -> u32 {
    let mut max_child = 0u32;
    for child in case.segments() {
        max_child = max_child.max(case_depth_in(child));
    }
    1 + max_child
}

fn case_depth_in(node: &ErasedSegment) -> u32 {
    if node.is_type(SyntaxKind::CaseExpression) {
        return case_depth(node);
    }
    let mut max_child = 0u32;
    for child in node.segments() {
        max_child = max_child.max(case_depth_in(child));
    }
    max_child
}

/// Count `WhenClause` nodes that belong to *this* CASE (not nested ones).
fn count_anywhere_within_case(case: &ErasedSegment, kind: SyntaxKind) -> u32 {
    // WHEN arms are direct children of the CASE expression; nested CASE bodies
    // live deeper. Counting direct children avoids double-counting nested arms.
    count_direct(case, kind)
}

// ── window functions ─────────────────────────────────────────────────

pub(crate) fn extract_windows(root: &ErasedSegment, windows: &mut WindowFacts) {
    let overs = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::OverClause),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    windows.function_count = overs.len() as u32;
    windows.frame_count = count_anywhere(root, SyntaxKind::FrameClause);
    for over in &overs {
        let partitions = over.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::PartitionbyClause),
            true,
            &SyntaxSet::EMPTY,
            true,
        );
        for p in &partitions {
            // A partition clause mixes bare column keys and computed-expression
            // keys (`PARTITION BY a, b + 1`). Sum both categories — taking the
            // max would drop one for mixed clauses (Codex P2).
            windows.partition_expression_count += count_direct(p, SyntaxKind::ColumnReference)
                + count_direct(p, SyntaxKind::Expression);
        }
        let orders = over.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::OrderbyClause),
            true,
            &SyntaxSet::EMPTY,
            true,
        );
        for o in &orders {
            windows.order_expression_count += count_direct(o, SyntaxKind::ColumnReference)
                + count_direct(o, SyntaxKind::Expression);
        }
    }
}

// ── aggregates / grouping ─────────────────────────────────────────────

const AGGREGATE_NAMES: &[&str] = &[
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "ARRAY_AGG",
    "STRING_AGG",
    "GROUP_CONCAT",
    "STDDEV",
    "VARIANCE",
    "VAR_POP",
    "VAR_SAMP",
    "STDDEV_POP",
    "STDDEV_SAMP",
    "MEDIAN",
    "PERCENTILE_CONT",
    "PERCENTILE_DISC",
    "BOOL_AND",
    "BOOL_OR",
    "BIT_AND",
    "BIT_OR",
    "COUNT_BIG",
];

pub(crate) fn extract_aggregates(root: &ErasedSegment, agg: &mut AggregateFacts) {
    let functions = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Function),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for func in &functions {
        if let Some(name) = function_name(func) {
            let upper = name.to_ascii_uppercase();
            if AGGREGATE_NAMES.contains(&upper.as_str()) {
                agg.function_count += 1;
                // `COUNT(DISTINCT …)` — detect the `DISTINCT` *keyword* token,
                // not a substring, so an argument like `COUNT(distinctive_id)`
                // or `COUNT('distinct')` doesn't false-match.
                if count_keyword(func, "DISTINCT") > 0 {
                    agg.distinct_count += 1;
                }
            }
        }
    }
    agg.group_by_count = count_anywhere(root, SyntaxKind::GroupbyClause);
    agg.having_count = count_anywhere(root, SyntaxKind::HavingClause);
    agg.rollup_count = count_anywhere(root, SyntaxKind::CubeRollupClause);
    agg.grouping_sets_count = count_anywhere(root, SyntaxKind::GroupingSetsClause);
    // CubeRollupClause covers both CUBE and ROLLUP in sqruff; split by the
    // clause's `CUBE`/`ROLLUP` *keyword* token, not raw text, so an argument
    // identifier like `ROLLUP(cube_id)` doesn't increment both counters.
    let cube_rollups = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::CubeRollupClause),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut cube = 0u32;
    let mut rollup = 0u32;
    for cr in &cube_rollups {
        // The construct is named by a `FunctionNameIdentifier` (`CUBE`/`ROLLUP`)
        // — not a keyword — so classify by that name, not raw-text substring
        // (a `ROLLUP(cube_id)` argument must not increment the cube counter).
        match function_name(cr).map(|n| n.to_ascii_uppercase()).as_deref() {
            Some("CUBE") => cube += 1,
            Some("ROLLUP") => rollup += 1,
            _ => {}
        }
    }
    agg.cube_count = cube;
    agg.rollup_count = rollup;
}

// ── predicates / boolean logic ────────────────────────────────────────

const PREDICATE_PARENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::WhereClause,
    SyntaxKind::HavingClause,
    SyntaxKind::JoinOnCondition,
]);

pub(crate) fn extract_predicates(root: &ErasedSegment, pred: &mut PredicateFacts) {
    // Boolean operators are `BinaryOperator` nodes whose raw text is AND/OR
    // (the CST does not distinguish boolean from arithmetic binary operators
    // by kind — empirically verified from a parse dump).
    let binops = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::BinaryOperator),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for op in &binops {
        let raw = op.raw().to_ascii_uppercase();
        if raw == "AND" || raw == "OR" {
            pred.boolean_operator_count += 1;
        }
    }
    pred.not_spans = collect_predicate_nots(root);
    pred.not_count = pred.not_spans.len() as u32;
    pred.comparison_count = count_anywhere(root, SyntaxKind::ComparisonOperator);

    // Max boolean nesting depth within predicate-bearing clauses.
    let parents = root.recursive_crawl(&PREDICATE_PARENTS, true, &SyntaxSet::EMPTY, true);
    for parent in &parents {
        pred.max_boolean_depth = pred.max_boolean_depth.max(boolean_depth(parent));
    }

    // NULL-semantics risk: comparing a value to NULL with `=`/`<>`/`!=`
    // (always NULL in standard SQL) and `NOT IN` (NULL-in-list trap). Derived
    // from parsed tokens — a `ComparisonOperator` adjacent to a `NullLiteral`,
    // and adjacent `NOT`+`IN` keyword tokens — so text inside comments or
    // string literals (`-- avoid x = NULL`, `'NOT IN list'`) is never counted.
    pred.null_semantics_risk_count = count_null_semantics_risk(root);
}

/// Collect the source ranges of `NOT` keyword tokens that act as
/// predicate/boolean operators (§6.7) — `not_count` is their count and each
/// range becomes one `MetricEvidence` entry under the metric's own key
/// (Codex P1). Two non-predicate contexts a raw keyword count picks up are
/// excluded:
///
/// - `NOT NULL` column constraints in DDL (`id INT NOT NULL`) — but only
///   with column-definition/constraint ancestry: a boolean `WHERE NOT NULL`
///   predicate is a genuine unary negation and counts (Codex P2). A
///   genuine `IS NOT NULL` predicate always counts (the `IS` before the
///   `NOT` distinguishes it).
/// - `IF NOT EXISTS` guards *inside CREATE/DROP statements* (`CREATE TABLE
///   IF NOT EXISTS`) — but the same token run as a procedural condition
///   (T-SQL `IF NOT EXISTS (SELECT …) BEGIN …`) is a genuine negation and
///   counts, so the exclusion requires DDL ancestry (Codex P2). A `WHERE
///   NOT EXISTS (…)` predicate has no `IF` and always counts.
///
/// Works over sibling code tokens, mirroring `count_null_semantics_risk`.
fn collect_predicate_nots(root: &ErasedSegment) -> Vec<(u32, u32)> {
    /// Statement kinds whose `IF NOT EXISTS` is a DDL guard.
    const DDL_GUARD_CONTEXTS: SyntaxSet = CREATE_TABLE_STATEMENTS
        .union(&CREATE_VIEW_STATEMENTS)
        .union(&CREATE_OTHER_STATEMENTS)
        .union(&ALTER_TABLE_STATEMENTS)
        .union(&DROP_STATEMENTS);
    /// Contexts whose `NOT NULL` is a column constraint, not a predicate.
    const CONSTRAINT_CONTEXTS: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::ColumnDefinition,
        SyntaxKind::ColumnConstraintSegment,
    ]);
    fn walk(node: &ErasedSegment, in_ddl: bool, in_constraint: bool, spans: &mut Vec<(u32, u32)>) {
        let code: Vec<&ErasedSegment> = node
            .segments()
            .iter()
            .filter(|s| !s.is_whitespace() && !s.is_meta() && !s.is_comment())
            .collect();
        for (i, seg) in code.iter().enumerate() {
            if !seg.is_type(SyntaxKind::Keyword) || !seg.raw().eq_ignore_ascii_case("NOT") {
                continue;
            }
            let neighbor = |j: Option<usize>| {
                j.and_then(|k| code.get(k))
                    .map(|s| s.raw().to_ascii_uppercase())
                    .unwrap_or_default()
            };
            let prev = neighbor(i.checked_sub(1));
            let next = neighbor(Some(i + 1));
            let null_constraint = in_constraint && next == "NULL" && prev != "IS";
            let ddl_guard = in_ddl && next == "EXISTS" && prev == "IF";
            if !null_constraint
                && !ddl_guard
                && let Some(pm) = seg.get_position_marker()
            {
                spans.push((pm.source_slice.start as u32, pm.source_slice.end as u32));
            }
        }
        for child in node.segments() {
            let child_in_ddl = in_ddl || DDL_GUARD_CONTEXTS.contains(child.get_type());
            let child_in_constraint =
                in_constraint || CONSTRAINT_CONTEXTS.contains(child.get_type());
            walk(child, child_in_ddl, child_in_constraint, spans);
        }
    }
    let mut spans = Vec::new();
    walk(root, false, false, &mut spans);
    spans
}

/// Count NULL-semantics risks from parsed tokens (comments/literals excluded):
/// a `=`/`<>`/`!=` comparison whose neighboring code operand is a `NullLiteral`
/// (each counted once), plus `NOT IN` keyword pairs.
fn count_null_semantics_risk(root: &ErasedSegment) -> u32 {
    fn walk(node: &ErasedSegment, count: &mut u32) {
        let code: Vec<&ErasedSegment> = node
            .segments()
            .iter()
            .filter(|s| !s.is_whitespace() && !s.is_meta() && !s.is_comment())
            .collect();
        for (i, seg) in code.iter().enumerate() {
            // `<op> NULL` / `NULL <op>` where op is `=`, `<>`, or `!=`.
            if seg.is_type(SyntaxKind::ComparisonOperator) {
                let op = seg.raw().trim();
                if op == "=" || op == "<>" || op == "!=" {
                    let neighbor_is_null = |j: Option<usize>| {
                        j.and_then(|k| code.get(k))
                            .is_some_and(|s| s.is_type(SyntaxKind::NullLiteral))
                    };
                    if neighbor_is_null(i.checked_sub(1)) || neighbor_is_null(Some(i + 1)) {
                        *count += 1;
                    }
                }
            }
            // `NOT IN` — adjacent keyword tokens.
            if seg.is_type(SyntaxKind::Keyword)
                && seg.raw().eq_ignore_ascii_case("NOT")
                && code.get(i + 1).is_some_and(|s| {
                    s.is_type(SyntaxKind::Keyword) && s.raw().eq_ignore_ascii_case("IN")
                })
            {
                *count += 1;
            }
        }
        for child in node.segments() {
            walk(child, count);
        }
    }
    let mut count = 0u32;
    walk(root, &mut count);
    count
}

/// Boolean-expression nesting depth within `node`.
///
/// The outermost boolean operator (wherever it sits — bracketed or not) is
/// level 1; each boolean-bearing `Bracketed` group nested *inside* another
/// boolean expression adds one more level. So:
/// - `a AND b` → 1 (flat),
/// - `(a OR b)` → 1 (a single, possibly redundant, outer group is still one
///   boolean level),
/// - `a AND (b OR c)` → 2 (the `OR` is one level below the `AND`),
/// - `a AND (b OR (c AND d))` → 3.
///
/// Brackets are the nesting proxy because sqruff represents `AND`/`OR` as flat
/// `BinaryOperator` tokens rather than a nested boolean tree. `0` means no
/// boolean operator is present at all.
fn boolean_depth(node: &ErasedSegment) -> u32 {
    fn contains_bool(node: &ErasedSegment) -> bool {
        node.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::BinaryOperator),
            true,
            &SyntaxSet::EMPTY,
            true,
        )
        .iter()
        .any(|o| {
            let r = o.raw().to_ascii_uppercase();
            r == "AND" || r == "OR"
        })
    }
    // `level` is the boolean depth credited to the *current* subtree. It starts
    // at 0 and becomes 1 the first time we descend into a boolean-bearing
    // region (so the outermost boolean — bracketed or not — is level 1). Each
    // *further* boolean-bearing bracket below that adds one.
    fn walk(node: &ErasedSegment, level: u32) -> u32 {
        // Entering a boolean-bearing bracket that is nested inside the current
        // boolean region deepens it by one. The seed level is already 1, so the
        // outermost boolean bracket does not stack an extra level on the base.
        let here = if node.is_type(SyntaxKind::Bracketed) && contains_bool(node) {
            level + 1
        } else {
            level
        };
        let mut max = here;
        for child in node.segments() {
            max = max.max(walk(child, here));
        }
        max
    }
    if !contains_bool(node) {
        return 0;
    }
    // Seed level 1: the clause holds at least one boolean operator at its own
    // level. But a single outer bracket wrapping the whole predicate
    // (`WHERE (a OR b)`) must not count as a deeper level than the equivalent
    // unbracketed `WHERE a OR b`. Descend through any leading non-boolean
    // wrappers, then walk: the first boolean bracket encountered sits at the
    // base level, deeper ones add to it.
    walk(node, 1).saturating_sub(redundant_outer_bracket(node))
}

/// Returns 1 when `node`'s boolean content is entirely wrapped in a single
/// outer bracket (so the seeded walk would over-count it by one), else 0.
/// Handles the `WHERE (a OR b)` redundant-parenthesis case so it scores the
/// same as `WHERE a OR b`.
fn redundant_outer_bracket(node: &ErasedSegment) -> u32 {
    // Find the boolean operators directly at this clause level (not inside any
    // bracket). If there are none — every boolean lives inside brackets — the
    // outermost bracket is redundant and the walk's +1 for it is spurious.
    fn has_unbracketed_bool(node: &ErasedSegment) -> bool {
        for child in node.segments() {
            if child.is_type(SyntaxKind::Bracketed) {
                continue;
            }
            if child.is_type(SyntaxKind::BinaryOperator) {
                let r = child.raw().to_ascii_uppercase();
                if r == "AND" || r == "OR" {
                    return true;
                }
            }
            if has_unbracketed_bool(child) {
                return true;
            }
        }
        false
    }
    if has_unbracketed_bool(node) { 0 } else { 1 }
}

// ── subqueries / derived tables ───────────────────────────────────────

pub(crate) fn extract_subqueries(
    root: &ErasedSegment,
    selects: &[ErasedSegment],
    sub: &mut SubqueryFacts,
) {
    // A subquery is any SELECT that is nested inside another query construct.
    // The outermost SELECT(s) of each top-level statement are not subqueries.
    for sel in selects {
        let depth = ancestor_select_depth(root, sel);
        if depth > 1 {
            sub.count += 1;
            sub.max_depth = sub.max_depth.max(depth - 1);
            if is_correlated(root, sel) {
                sub.correlated_count += 1;
            }
        }
    }
    // Derived tables: SELECT directly inside a FROM/JOIN bracketed expression.
    sub.derived_table_count = count_derived_tables(root);
    // EXISTS / IN subqueries, detected from keyword tokens that are followed by
    // a bracketed SELECT — not substring matching, which would mis-count e.g.
    // `JOIN (SELECT …)` as an `IN (SELECT` predicate (Codex P2).
    let (exists_n, in_n) = count_keyword_subqueries(root);
    sub.exists_count = exists_n;
    sub.in_count = in_n;
    // Scalar subqueries: SELECT inside a select_clause_element expression.
    sub.scalar_count = count_scalar_subqueries(root);
}

/// Count `EXISTS (SELECT …)` and `IN (SELECT …)` subquery predicates by
/// inspecting keyword tokens and their following sibling, so substrings like
/// the `IN (` inside `JOIN (` are not miscounted.
fn count_keyword_subqueries(root: &ErasedSegment) -> (u32, u32) {
    fn following_bracketed_select(siblings: &[ErasedSegment], from: usize) -> bool {
        for sib in siblings.iter().skip(from + 1) {
            // Skip whitespace, meta, and comments — a comment between the
            // keyword and the subquery (`IN /* ids */ (SELECT …)`) is legal
            // SQL and must not hide the predicate (Codex P2).
            if sib.is_whitespace() || sib.is_meta() || sib.is_comment() {
                continue;
            }
            return sib.is_type(SyntaxKind::Bracketed)
                && !sib
                    .recursive_crawl(&SELECT_STATEMENT, false, &SyntaxSet::EMPTY, true)
                    .is_empty();
        }
        false
    }
    fn walk(node: &ErasedSegment, exists: &mut u32, in_count: &mut u32) {
        let children = node.segments();
        for (i, child) in children.iter().enumerate() {
            if child.is_type(SyntaxKind::Keyword) {
                let kw = child.raw().to_ascii_uppercase();
                if kw == "EXISTS" && following_bracketed_select(children, i) {
                    *exists += 1;
                } else if kw == "IN" && following_bracketed_select(children, i) {
                    *in_count += 1;
                }
            }
            walk(child, exists, in_count);
        }
    }
    let mut exists = 0u32;
    let mut in_count = 0u32;
    walk(root, &mut exists, &mut in_count);
    (exists, in_count)
}

/// Number of SELECT ancestors (inclusive) above and including `target`.
/// 1 means top-level; 2 means one level of nesting, etc.
fn ancestor_select_depth(root: &ErasedSegment, target: &ErasedSegment) -> u32 {
    fn walk(node: &ErasedSegment, target: &ErasedSegment, depth: u32) -> Option<u32> {
        let here = if node.is_type(SyntaxKind::SelectStatement) {
            depth + 1
        } else {
            depth
        };
        if node.is(target) {
            return Some(here);
        }
        for child in node.segments() {
            if let Some(found) = walk(child, target, here) {
                return Some(found);
            }
        }
        None
    }
    walk(root, target, 0).unwrap_or(1)
}

/// A subquery is correlated if it references a relation alias defined in an
/// outer scope. Heuristic: the subquery contains a qualified column reference
/// whose qualifier is not a relation defined inside the subquery itself.
fn is_correlated(_root: &ErasedSegment, subquery: &ErasedSegment) -> bool {
    // Relations defined in *this* subquery's own scope (not deeper nested
    // SELECT nodes — their relations belong to their own scope and would otherwise
    // mask a genuine outer reference).
    let inner_relations = relation_names(subquery);
    // Column refs evaluated in this subquery's scope — exclude refs that live
    // inside a more deeply nested SELECT (those are that inner query's to
    // resolve, not this one's).
    let col_refs = subquery.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::ColumnReference),
        true,
        &SELECT_STATEMENT,
        true,
    );
    for col in &col_refs {
        if let Some(qualifier) = column_qualifier(col)
            && !inner_relations
                .iter()
                .any(|r| r.eq_ignore_ascii_case(&qualifier))
        {
            return true;
        }
    }
    false
}

/// Identifier node kinds. Both naked (`t`) and quoted (`"t"`, `` `t` ``,
/// `[t]`) identifiers must be resolved — quoted identifiers are common in
/// Snowflake/BigQuery/Postgres/T-SQL and are a distinct `SyntaxKind`.
const IDENTIFIER_KINDS: SyntaxSet =
    SyntaxSet::new(&[SyntaxKind::NakedIdentifier, SyntaxKind::QuotedIdentifier]);

/// Normalize an identifier's raw text for comparison: strip surrounding quote
/// characters (`"`, `` ` ``, `[ ]`) so `"t"`, `` `t` ``, `[t]`, and `t`
/// compare equal.
fn normalize_identifier(raw: &str) -> String {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        let quoted = (first == b'"' && last == b'"')
            || (first == b'`' && last == b'`')
            || (first == b'[' && last == b']');
        if quoted {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

/// Relation names in scope at `node`: the names of table references in
/// `from_expression_element` positions (read sources) plus their table
/// aliases. Only FROM/JOIN aliases are relation aliases — output-column
/// aliases (`SELECT i.x AS o`) are *not* relations, so including them would
/// mask a genuine outer reference (`o.grp`) in a correlated subquery.
fn relation_names(node: &ErasedSegment) -> Vec<String> {
    let mut names = Vec::new();
    // Only FROM elements in *this* query level — stop at nested SELECT nodes so a
    // derived table's or correlated inner subquery's relations/aliases don't
    // leak into this scope and mask a genuine outer reference.
    let from_elems = node.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::FromExpressionElement),
        true,
        &SELECT_STATEMENT,
        true,
    );
    for elem in &from_elems {
        // Table reference(s) of this FROM element (again, not those inside a
        // derived-table subquery nested in the element).
        for tr in elem.recursive_crawl(&TABLE_REFERENCES, true, &SELECT_STATEMENT, true) {
            names.push(last_identifier(&tr));
        }
        // The element's own (table) alias, if any.
        for a in elem.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::AliasExpression),
            true,
            &SELECT_STATEMENT,
            true,
        ) {
            if let Some(id) = a
                .recursive_crawl(&IDENTIFIER_KINDS, true, &SyntaxSet::EMPTY, true)
                .first()
            {
                names.push(normalize_identifier(id.raw()));
            }
        }
    }
    names
}

/// The relation qualifier of a column reference, if it is qualified — the
/// identifier *immediately before* the column name. For `c.id` that is `c`;
/// for a fully-qualified `schema.t.id` it is `t` (the relation), not `schema`.
/// Using the relation component keeps it consistent with [`relation_names`]
/// (which records table refs by their last identifier), so a local
/// fully-qualified reference is not misread as an outer/correlated reference.
/// Quoted identifiers (`"t".id`) are handled by [`normalize_identifier`].
fn column_qualifier(col: &ErasedSegment) -> Option<String> {
    let idents = col.recursive_crawl(&IDENTIFIER_KINDS, true, &SyntaxSet::EMPTY, true);
    // A qualified ref has at least two identifier parts separated by a dot.
    // The last part is the column; the part before it is the relation.
    if col.raw().contains('.') && idents.len() >= 2 {
        Some(normalize_identifier(idents[idents.len() - 2].raw()))
    } else {
        None
    }
}

fn last_identifier(node: &ErasedSegment) -> String {
    let idents = node.recursive_crawl(&IDENTIFIER_KINDS, true, &SyntaxSet::EMPTY, true);
    idents
        .last()
        .map(|i| normalize_identifier(i.raw()))
        .unwrap_or_else(|| normalize_identifier(node.raw()))
}

/// Count SELECT nodes that sit directly inside a FROM/JOIN bracketed expression.
fn count_derived_tables(root: &ErasedSegment) -> u32 {
    let from_elems = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::FromExpressionElement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut count = 0u32;
    for elem in &from_elems {
        // A derived table wraps a SELECT/SetExpression in a bracketed
        // table_expression.
        let nested = elem.recursive_crawl(
            &SyntaxSet::new(&[SyntaxKind::SelectStatement, SyntaxKind::SetExpression]),
            false,
            &SyntaxSet::EMPTY,
            false,
        );
        if !nested.is_empty() {
            count += 1;
        }
    }
    count
}

/// Count scalar subqueries: SELECT inside a select_clause_element expression.
fn count_scalar_subqueries(root: &ErasedSegment) -> u32 {
    let elems = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::SelectClauseElement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut count = 0u32;
    for elem in &elems {
        let nested = elem.recursive_crawl(&SELECT_STATEMENT, false, &SyntaxSet::EMPTY, false);
        count += nested.len() as u32;
    }
    count
}

// ── expressions / functions ───────────────────────────────────────────

pub(crate) fn extract_expressions(root: &ErasedSegment, expr: &mut ExpressionFacts) {
    // Max expression AST depth across all Expression nodes.
    let expressions = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Expression),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for e in &expressions {
        expr.max_depth = expr.max_depth.max(expression_depth(e));
    }
    // Function calls + distinct names + nesting.
    let functions = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Function),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    expr.function_call_count = functions.len() as u32;
    let mut names = std::collections::BTreeSet::new();
    for f in &functions {
        if let Some(n) = function_name(f) {
            names.insert(n.to_ascii_uppercase());
        }
        expr.max_function_nesting = expr.max_function_nesting.max(function_nesting(f));
    }
    expr.distinct_function_count = names.len() as u32;
    // Casts come in two forms: the shorthand `x::int` parses to exactly one
    // `CastExpression` node (whose raw already contains `::`, so a substring
    // count would double-count it), and the SQL-standard `CAST(x AS int)`
    // parses as a `Function` named CAST (no `CastExpression`, no `::`). Count
    // each form once: CastExpression nodes + CAST(...) function calls.
    let standard_casts = functions
        .iter()
        .filter(|f| function_name(f).is_some_and(|n| n.eq_ignore_ascii_case("CAST")))
        .count() as u32;
    expr.cast_count = count_anywhere(root, SyntaxKind::CastExpression) + standard_casts;
}

/// Depth of an expression's operator/operand tree (1 for a leaf expression).
fn expression_depth(node: &ErasedSegment) -> u32 {
    let mut max_child = 0u32;
    for child in node.segments() {
        if child.is_type(SyntaxKind::Expression)
            || child.is_type(SyntaxKind::Function)
            || child.is_type(SyntaxKind::CaseExpression)
            || child.is_type(SyntaxKind::Bracketed)
        {
            max_child = max_child.max(expression_depth(child));
        } else {
            max_child = max_child.max(expression_depth_in(child));
        }
    }
    1 + max_child
}

fn expression_depth_in(node: &ErasedSegment) -> u32 {
    let mut max_child = 0u32;
    for child in node.segments() {
        if child.is_type(SyntaxKind::Expression) {
            max_child = max_child.max(expression_depth(child));
        } else {
            max_child = max_child.max(expression_depth_in(child));
        }
    }
    max_child
}

/// Maximum nested function-call depth rooted at `func`.
fn function_nesting(func: &ErasedSegment) -> u32 {
    let mut max_child = 0u32;
    for child in func.segments() {
        max_child = max_child.max(function_nesting_in(child));
    }
    1 + max_child
}

fn function_nesting_in(node: &ErasedSegment) -> u32 {
    if node.is_type(SyntaxKind::Function) {
        return function_nesting(node);
    }
    let mut max_child = 0u32;
    for child in node.segments() {
        max_child = max_child.max(function_nesting_in(child));
    }
    max_child
}

fn function_name(func: &ErasedSegment) -> Option<String> {
    func.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::FunctionNameIdentifier),
        true,
        &SyntaxSet::EMPTY,
        true,
    )
    .first()
    .map(|n| n.raw().to_string())
}

// ── output shape / identifiers ────────────────────────────────────────

fn extract_output(root: &ErasedSegment, selects: &[ErasedSegment], out: &mut OutputFacts) {
    // Wildcards.
    let wildcards = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::WildcardExpression),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    out.star_count = wildcards.len() as u32;
    // Outer wildcards: those whose nearest enclosing SELECT is at depth 1.
    for w in &wildcards {
        if nearest_select_depth(root, w) <= 1 {
            out.outer_star_count += 1;
        }
    }

    // Derived SELECT expressions without alias.
    let elems = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::SelectClauseElement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for elem in &elems {
        // A "derived expression" is a select element that is not a bare column
        // reference or wildcard — i.e. it computes something.
        let is_expr = !elem
            .recursive_crawl(
                &SyntaxSet::new(&[
                    SyntaxKind::Expression,
                    SyntaxKind::Function,
                    SyntaxKind::CaseExpression,
                    SyntaxKind::CastExpression,
                ]),
                false,
                &SyntaxSet::EMPTY,
                false,
            )
            .is_empty();
        if is_expr {
            out.derived_expression_count += 1;
            let has_alias = !elem
                .recursive_crawl(
                    &SyntaxSet::single(SyntaxKind::AliasExpression),
                    true,
                    &SyntaxSet::EMPTY,
                    true,
                )
                .is_empty();
            if has_alias {
                out.aliased_derived_expression_count += 1;
            } else {
                out.expression_without_alias_count += 1;
            }
        }
    }

    // Column references + qualification (per query block, since "multi-relation
    // scope" depends on how many relations the enclosing SELECT touches).
    for sel in selects {
        let relation_count = count_direct_relations(sel);
        let cols = sel.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::ColumnReference),
            true,
            // Don't descend into nested SELECT nodes — they have their own scope.
            &SELECT_STATEMENT,
            true,
        );
        for col in &cols {
            out.total_column_refs += 1;
            let qualified = column_qualifier(col).is_some();
            if relation_count > 1 {
                out.multi_relation_column_refs += 1;
                if !qualified {
                    out.multi_relation_unqualified_refs += 1;
                }
            }
            if !qualified {
                out.unqualified_column_refs += 1;
            }
        }
    }

    out.quoted_identifier_count = count_anywhere(root, SyntaxKind::QuotedIdentifier);
    // Only FROM/JOIN aliases are *table* aliases. `AliasExpression` also covers
    // output-column aliases (`SELECT SUM(x) AS total`), so counting every alias
    // would inflate the table-alias metric. Count aliases that are direct
    // children of a `from_expression_element`.
    let from_elems = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::FromExpressionElement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    out.table_alias_count = from_elems
        .iter()
        .map(|e| count_direct(e, SyntaxKind::AliasExpression))
        .sum();
}

/// Count the relations in a SELECT's own FROM/JOIN scope (not nested
/// subqueries). Each `from_expression_element` is one relation — a base table
/// *or* a derived table (`(SELECT …) q`) — so derived-table joins are counted
/// (a `TableReference`-only count would miss `q` and make a 2-relation scope
/// look single-relation, skewing `unqualified_column_ratio`).
fn count_direct_relations(sel: &ErasedSegment) -> u32 {
    sel.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::FromExpressionElement),
        true,
        &SELECT_STATEMENT,
        true,
    )
    .len() as u32
}

fn nearest_select_depth(root: &ErasedSegment, target: &ErasedSegment) -> u32 {
    ancestor_select_depth(root, target)
}

// ── CTE graph (via sqruff Query analysis) ─────────────────────────────

pub(crate) fn extract_cte_graph(root: &ErasedSegment, ctes: &mut CteFacts) {
    // The CTE dependency graph is derived directly from `CommonTableExpression`
    // CST nodes: each carries a name identifier and a body whose
    // `TableReference`s name its dependencies. We deliberately avoid sqruff's
    // `Query`/`crawl_sources` analysis here — it uses interior mutability
    // (`Rc<RefCell<…>>`) and re-borrows while crawling, which would conflict
    // with any borrow held across the call. The CST is the stable, verified
    // shape (parser comparison §2.1).
    //
    // CTE names are scoped to their owning `WITH` block, so the graph is built
    // per `WithCompoundStatement`: a `b` table reference in `WITH a AS (SELECT
    // * FROM b) …` only counts as a CTE dependency when `b` is a CTE *of the
    // same WITH block*. A file-wide name set would forge cross-statement edges
    // — `WITH b AS (…) SELECT …; WITH a AS (SELECT * FROM b) …` would falsely
    // make the second statement's real table `b` a dependency on the first
    // statement's CTE (Codex P2).
    use std::collections::{BTreeMap, BTreeSet};

    let with_blocks = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::WithCompoundStatement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );

    let mut total = 0u32;
    let mut trivial = 0u32;

    for with in &with_blocks {
        // The CTEs *directly* owned by this WITH block. A nested WITH (in a CTE
        // body or subquery) is its own scope and is handled when the crawl
        // reaches it, so restrict to this block's own definitions.
        let cte_nodes: Vec<ErasedSegment> = with
            .recursive_crawl(
                &SyntaxSet::single(SyntaxKind::CommonTableExpression),
                true,
                &SyntaxSet::single(SyntaxKind::WithCompoundStatement),
                false,
            )
            .into_iter()
            .filter(|c| owning_with_block(root, c).is_some_and(|w| w.is(with)))
            .collect();
        if cte_nodes.is_empty() {
            continue;
        }
        total += cte_nodes.len() as u32;
        trivial += cte_nodes.iter().filter(|c| is_trivial_cte(c)).count() as u32;

        // Names defined in *this* block — the only names that can be intra-block
        // dependencies.
        let cte_names: Vec<String> = cte_nodes.iter().map(cte_name).collect();

        let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut fan_out: BTreeMap<String, u32> = BTreeMap::new();

        for (idx, cte) in cte_nodes.iter().enumerate() {
            let name_up = cte_names[idx].to_ascii_uppercase();
            for dep in cte_body_dependencies(cte, &cte_names) {
                let dep_up = dep.to_ascii_uppercase();
                if dep_up == name_up {
                    // A recursive CTE references *itself*. `WITH RECURSIVE`
                    // merely permits recursion; the count reflects an actual
                    // self-reference, not the keyword.
                    ctes.recursive_count += 1;
                    continue;
                }
                ctes.dependency_edges += 1;
                edges
                    .entry(name_up.clone())
                    .or_default()
                    .push(dep_up.clone());
                *fan_out.entry(dep_up).or_default() += 1;
            }
        }
        ctes.max_fan_out = ctes
            .max_fan_out
            .max(fan_out.values().copied().max().unwrap_or(0));
        ctes.max_dependency_depth = ctes
            .max_dependency_depth
            .max(longest_chain(&edges, &cte_names));

        // Unused CTEs: defined but referenced by neither another CTE body nor
        // the block's main query. A name counts as referenced only when it
        // appears as a table reference *within this WITH block* and *outside*
        // its own definition body.
        let block_refs = with.recursive_crawl(
            &TABLE_REFERENCES,
            true,
            &SyntaxSet::single(SyntaxKind::WithCompoundStatement),
            false,
        );
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        for (idx, cte) in cte_nodes.iter().enumerate() {
            let self_name = cte_names[idx].to_ascii_uppercase();
            for r in &block_refs {
                let name = r.raw().to_ascii_uppercase();
                if name == self_name
                    && !is_within(cte, r)
                    && owning_with_block(root, r).is_some_and(|w| w.is(with))
                {
                    referenced.insert(name);
                }
            }
        }
        ctes.unused_count += cte_names
            .iter()
            .filter(|n| !referenced.contains(&n.to_ascii_uppercase()))
            .count() as u32;
    }

    ctes.count = total;
    ctes.trivial_count = trivial;
}

/// The innermost `WithCompoundStatement` that contains `node`, if any. Used to
/// attribute a CTE definition or table reference to the WITH block whose scope
/// it actually belongs to (so a nested WITH's names don't leak outward, and a
/// reference is matched against the right block's CTE names).
fn owning_with_block(root: &ErasedSegment, node: &ErasedSegment) -> Option<ErasedSegment> {
    fn walk(
        current: &ErasedSegment,
        node: &ErasedSegment,
        nearest_with: Option<&ErasedSegment>,
    ) -> Option<ErasedSegment> {
        if current.is(node) {
            return nearest_with.cloned();
        }
        let next = if current.is_type(SyntaxKind::WithCompoundStatement) {
            Some(current)
        } else {
            nearest_with
        };
        for child in current.segments() {
            if let Some(found) = walk(child, node, next) {
                return Some(found);
            }
        }
        None
    }
    walk(root, node, None)
}

/// A trivial CTE selects from a single source with no filtering, aggregation,
/// grouping, or join — it only renames a relation (§6.4). Such CTEs add naming
/// overhead without structural value and dock the modularity-health score.
fn is_trivial_cte(cte: &ErasedSegment) -> bool {
    // The CTE body is the bracketed SELECT after `AS`. A trivial body has
    // exactly one table reference and none of the structure-adding clauses.
    let table_refs = cte
        .recursive_crawl(&TABLE_REFERENCES, true, &SyntaxSet::EMPTY, true)
        .len();
    if table_refs != 1 {
        return false;
    }
    const STRUCTURE: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::WhereClause,
        SyntaxKind::JoinClause,
        SyntaxKind::GroupbyClause,
        SyntaxKind::HavingClause,
        SyntaxKind::SetExpression,
        SyntaxKind::CaseExpression,
        SyntaxKind::OverClause,
    ]);
    cte.recursive_crawl(&STRUCTURE, true, &SyntaxSet::EMPTY, true)
        .is_empty()
}

/// The defined name of a CTE (the identifier before its `AS (`).
fn cte_name(cte: &ErasedSegment) -> String {
    // The first identifier child is the CTE name; the body's identifiers come
    // later and deeper. Taking the first naked/quoted identifier is reliable
    // for the verified `common_table_expression > naked_identifier "AS" …`
    // shape.
    for child in cte.segments() {
        if child.is_type(SyntaxKind::NakedIdentifier) || child.is_type(SyntaxKind::QuotedIdentifier)
        {
            return child.raw().to_string();
        }
    }
    String::new()
}

/// Distinct CTE names referenced inside `cte`'s body (its dependencies). A CTE
/// referenced multiple times in one body is a single dependency edge — so the
/// result is deduplicated to avoid inflating `dependency_edges`/`max_fan_out`
/// (and thus the modularity score).
///
/// CTE names are lexically scoped: a nested `WITH` inside the body can *see*
/// the enclosing block's CTE names, so `WITH a AS (…), b AS (WITH x AS (SELECT
/// * FROM a) …) …` is a real `b -> a` edge and the crawl must keep descending
/// into nested WITH bodies (CodeRabbit). But a nested CTE *shadows* an outer
/// name: `WITH b AS (…), a AS (WITH b AS (…) SELECT * FROM b) …` reads the
/// inner `b`, not the outer one, so that reference is NOT an `a -> b` edge
/// (Codex P2). We therefore crawl the whole body and drop any reference that a
/// nested `CommonTableExpression` between it and `cte` redefines.
fn cte_body_dependencies(
    cte: &ErasedSegment,
    cte_names: &[String],
) -> std::collections::BTreeSet<String> {
    let refs = cte.recursive_crawl(&TABLE_REFERENCES, true, &SyntaxSet::EMPTY, true);
    // Nested `WITH` blocks inside this body. A reference is *shadowed* when it
    // sits inside a nested block that defines the same name — it then resolves
    // to that inner CTE, not the enclosing block's. The shadowing scope is the
    // whole `WithCompoundStatement` (its CTE bodies *and* its main query), not
    // just the `CommonTableExpression` definition node.
    let nested_with_blocks = cte.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::WithCompoundStatement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut deps = std::collections::BTreeSet::new();
    for r in &refs {
        let name = r.raw().to_ascii_uppercase();
        if !cte_names.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let shadowed = nested_with_blocks.iter().any(|w| {
            is_within(w, r)
                && w.segments()
                    .iter()
                    .filter(|c| c.is_type(SyntaxKind::CommonTableExpression))
                    .any(|c| cte_name(c).eq_ignore_ascii_case(&name))
        });
        if !shadowed {
            deps.insert(name);
        }
    }
    deps
}

/// Whether `needle` is the same node as, or a descendant of, `haystack`.
fn is_within(haystack: &ErasedSegment, needle: &ErasedSegment) -> bool {
    if haystack.is(needle) {
        return true;
    }
    haystack.segments().iter().any(|c| is_within(c, needle))
}

/// CTE names referenced by the final (non-CTE) query body.
/// Longest dependency chain length through the CTE edge map.
fn longest_chain(edges: &std::collections::BTreeMap<String, Vec<String>>, nodes: &[String]) -> u32 {
    use std::collections::BTreeMap;
    let mut memo: BTreeMap<String, u32> = BTreeMap::new();
    fn depth(
        node: &str,
        edges: &BTreeMap<String, Vec<String>>,
        memo: &mut BTreeMap<String, u32>,
        stack: &mut Vec<String>,
    ) -> u32 {
        if let Some(d) = memo.get(node) {
            return *d;
        }
        if stack.iter().any(|s| s == node) {
            // Cycle (recursive CTE) — stop to avoid infinite recursion.
            return 1;
        }
        stack.push(node.to_string());
        let mut best = 1u32;
        if let Some(deps) = edges.get(node) {
            for dep in deps {
                best = best.max(1 + depth(dep, edges, memo, stack));
            }
        }
        stack.pop();
        memo.insert(node.to_string(), best);
        best
    }
    let mut max = 0u32;
    let mut stack = Vec::new();
    for n in nodes {
        let up = n.to_ascii_uppercase();
        max = max.max(depth(&up, edges, &mut memo, &mut stack));
    }
    max
}

// ── object-touch / DML-DDL risk ───────────────────────────────────────

fn extract_objects(
    root: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    bigquery: bool,
    facts: &mut SqlFileFacts,
    emit_contributions: bool,
) {
    let fallback_span = if emit_contributions {
        segment_span(root, line_at).unwrap_or_else(SourceSpan::empty)
    } else {
        SourceSpan::empty()
    };
    let obj = &mut facts.objects;
    let evidence = &mut facts.change_risk_evidence;
    let obj_evidence = &mut facts.object_evidence;
    // Per-statement-kind counters (used by the DML/DDL/TCL metric keys).
    for stmt in &facts.statements {
        match stmt.kind {
            StatementKind::Insert => {
                obj.insert_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.dml.insert_count",
                    "sql.dml.insert",
                    statement_span(stmt),
                );
            }
            StatementKind::Update => {
                obj.update_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.dml.update_count",
                    "sql.dml.update",
                    statement_span(stmt),
                );
            }
            StatementKind::Delete => {
                obj.delete_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.dml.delete_count",
                    "sql.dml.delete",
                    statement_span(stmt),
                );
            }
            StatementKind::Merge => {
                obj.merge_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.dml.merge_count",
                    "sql.dml.merge",
                    statement_span(stmt),
                );
                record_change_risk(
                    evidence,
                    emit_contributions,
                    statement_span(stmt),
                    ChangeRiskFactor::Merge,
                );
            }
            StatementKind::CreateTable
            | StatementKind::CreateTableAsSelect
            | StatementKind::CreateView
            | StatementKind::CreateOther => {
                obj.create_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.ddl.create_count",
                    "sql.ddl.create",
                    statement_span(stmt),
                );
            }
            StatementKind::AlterTable => {
                obj.alter_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.ddl.alter_count",
                    "sql.ddl.alter",
                    statement_span(stmt),
                );
                record_change_risk(
                    evidence,
                    emit_contributions,
                    statement_span(stmt),
                    ChangeRiskFactor::Alter,
                );
            }
            StatementKind::Drop => {
                obj.drop_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.ddl.drop_count",
                    "sql.ddl.drop",
                    statement_span(stmt),
                );
                record_change_risk(
                    evidence,
                    emit_contributions,
                    statement_span(stmt),
                    ChangeRiskFactor::Drop,
                );
            }
            StatementKind::Truncate => {
                obj.truncate_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.ddl.truncate_count",
                    "sql.ddl.truncate",
                    statement_span(stmt),
                );
                record_change_risk(
                    evidence,
                    emit_contributions,
                    statement_span(stmt),
                    ChangeRiskFactor::Truncate,
                );
            }
            StatementKind::Grant | StatementKind::Revoke => {
                obj.grant_revoke_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.dcl.grant_revoke_count",
                    "sql.dcl.grant_revoke",
                    statement_span(stmt),
                );
                record_change_risk(
                    evidence,
                    emit_contributions,
                    statement_span(stmt),
                    ChangeRiskFactor::GrantRevoke,
                );
            }
            StatementKind::TransactionControl => {
                obj.transaction_control_count += 1;
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.transaction.control_count",
                    "sql.transaction.control",
                    statement_span(stmt),
                );
                record_change_risk(
                    evidence,
                    emit_contributions,
                    statement_span(stmt),
                    ChangeRiskFactor::TransactionControl,
                );
            }
            _ => {}
        }
    }

    // Anonymous blocks *execute when the file is applied* (unlike routine
    // definitions, whose bodies only run when called), so DML/DDL/DCL/TCL
    // inside them is real migration risk. The per-statement-kind loop above
    // cannot see it — the statement classifies as `anonymous_block` — so the
    // block bodies are scanned node-based here. Nested routine definitions
    // (subprograms declared in a block's DECLARE section) stay excluded via
    // the `PROCEDURAL_DEFINITIONS` crawl boundary, mirroring every other
    // object scan. Statement nodes are paired with their classification by
    // index — `top_level_statements` is the same crawl `classify_statements`
    // consumed (CodeRabbit).
    let statements = top_level_statements(root, bigquery);
    debug_assert_eq!(statements.len(), facts.statements.len());
    for (node, stmt) in statements.iter().zip(facts.statements.iter()) {
        if stmt.kind == StatementKind::AnonymousBlock {
            scan_block_body_dml(
                node,
                line_at,
                obj,
                evidence,
                obj_evidence,
                emit_contributions,
            );
        }
    }

    // Distinct read/write/touch object counts (research foundation §6.14:
    // "distinct objects read/written/touched"). Counting objects rather than
    // statements means a 10-table SELECT contributes 10 reads, and an object
    // both read and written is touched once. Read objects are table references
    // in FROM/JOIN positions; write objects are the statement-level targets of
    // write statements. Names are uppercased so case variants collapse.
    let (read_objects, write_objects) =
        collect_touched_objects(&statements, &facts.statements, line_at, emit_contributions);
    obj.read_object_count = read_objects.len() as u32;
    obj.write_object_count = write_objects.len() as u32;
    obj.touch_count = (read_objects.len()
        + write_objects
            .keys()
            .filter(|name| !read_objects.contains_key(*name))
            .count()) as u32;
    if emit_contributions {
        for span in write_objects.values() {
            record_change_risk(
                evidence,
                true,
                span.unwrap_or(fallback_span),
                ChangeRiskFactor::WriteObject,
            );
        }
        for span in read_objects.values() {
            record_change_risk(
                evidence,
                true,
                span.unwrap_or(fallback_span),
                ChangeRiskFactor::ReadObject,
            );
        }
    }

    // UPDATE/DELETE without WHERE. Two scoping rules:
    // - the WHERE crawl stops at nested SELECT nodes: `UPDATE t SET v =
    //   (SELECT v FROM u WHERE u.id = t.id)` has no *statement-level* WHERE —
    //   it still rewrites every row — but a naive recursive crawl would find
    //   the subquery's WHERE and wrongly clear the no-WHERE flag (Codex P1);
    // - only non-`procedural` statements are scanned: a routine definition's
    //   body DML runs when called, not when the file is applied. The
    //   `PROCEDURAL_DEFINITIONS` crawl boundary covers well-formed routine
    //   nodes; skipping `procedural`-classified statements additionally
    //   covers T-SQL body fragments that sqruff splits into sibling
    //   statements (Codex P1).
    for (node, stmt) in statements.iter().zip(facts.statements.iter()) {
        if stmt.kind == StatementKind::Procedural {
            continue;
        }
        let updates = node.recursive_crawl(&UPDATE_STATEMENTS, true, &PROCEDURAL_DEFINITIONS, true);
        for u in &updates {
            if !has_own_where_clause(u) {
                obj.update_without_where_count += 1;
                if emit_contributions {
                    let span = segment_span(u, line_at).unwrap_or(fallback_span);
                    record_object(
                        obj_evidence,
                        true,
                        "sql.dml.update_without_where_count",
                        "sql.dml.update_without_where",
                        span,
                    );
                    record_change_risk(evidence, true, span, ChangeRiskFactor::UpdateWithoutWhere);
                }
            }
        }
        let deletes = node.recursive_crawl(&DELETE_STATEMENTS, true, &PROCEDURAL_DEFINITIONS, true);
        for d in &deletes {
            if !has_own_where_clause(d) {
                obj.delete_without_where_count += 1;
                if emit_contributions {
                    let span = segment_span(d, line_at).unwrap_or(fallback_span);
                    record_object(
                        obj_evidence,
                        true,
                        "sql.dml.delete_without_where_count",
                        "sql.dml.delete_without_where",
                        span,
                    );
                    record_change_risk(evidence, true, span, ChangeRiskFactor::DeleteWithoutWhere);
                }
            }
        }
    }

    // The remaining clause detections work over the *code* token stream
    // (comments/whitespace excluded) so a comment like `-- RETURNING id` or
    // `/* CREATE OR REPLACE */` never trips them, and arbitrary whitespace
    // between keywords is irrelevant (we look at adjacency in the token list).
    let code_tokens = code_tokens(root, line_at, emit_contributions);

    // CREATE OR REPLACE — three consecutive code tokens, handling
    // `CREATE\nOR REPLACE` / `CREATE  OR  REPLACE` (whitespace-insensitive).
    let create_or_replace = code_tokens
        .windows(3)
        .filter(|w| w[0].word == "CREATE" && w[1].word == "OR" && w[2].word == "REPLACE")
        .collect::<Vec<_>>();
    obj.create_or_replace_count = create_or_replace.len() as u32;
    if emit_contributions {
        for tokens in create_or_replace {
            let span = match (tokens[0].span, tokens[2].span) {
                (Some(start), Some(end)) => SourceSpan::new(
                    start.start_byte,
                    end.end_byte,
                    start.start_line,
                    end.end_line,
                ),
                _ => fallback_span,
            };
            record_change_risk(evidence, true, span, ChangeRiskFactor::CreateOrReplace);
        }
    }

    // RETURNING (Postgres/Oracle) / OUTPUT (T-SQL) DML result clauses, counted
    // from `Keyword` tokens inside DML statements (INSERT/UPDATE/DELETE/MERGE).
    // The clause word is lexed as a `Keyword`, whereas a column or table named
    // `output`/`returning` is a `NakedIdentifier` — so this never fires on
    // `UPDATE t SET output = 1` or `INSERT INTO output (…)`. `procedural`
    // statements (routine definitions and their split body fragments) are
    // skipped like every other object scan.
    const DML_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::InsertStatement,
        SyntaxKind::OracleInsertStatement,
        SyntaxKind::BulkInsertStatement,
        SyntaxKind::UpdateStatement,
        SyntaxKind::OracleUpdateStatement,
        SyntaxKind::DeleteStatement,
        SyntaxKind::OracleDeleteStatement,
        SyntaxKind::MergeStatement,
    ]);
    for (node, _) in statements
        .iter()
        .zip(facts.statements.iter())
        .filter(|(_, stmt)| stmt.kind != StatementKind::Procedural)
    {
        for dml in node.recursive_crawl(&DML_STATEMENTS, true, &PROCEDURAL_DEFINITIONS, true) {
            for kw in keyword_tokens(&dml, "RETURNING")
                .into_iter()
                .chain(keyword_tokens(&dml, "OUTPUT"))
            {
                obj.returning_count += 1;
                // Each counted clause is one evidence entry (Codex P1).
                record_object(
                    obj_evidence,
                    emit_contributions,
                    "sql.dml.returning_count",
                    "sql.dml.returning",
                    segment_span(&kw, line_at).unwrap_or(fallback_span),
                );
            }
        }
    }
}

/// Node-based DML/DDL/DCL/TCL tally for one anonymous block's body — the
/// statement-kind counters (`sql.dml.*`, `sql.ddl.*`,
/// `sql.dcl.grant_revoke_count`, `sql.transaction.control_count`) and their
/// change-risk terms, mirroring the per-statement loop in `extract_objects`
/// arm for arm (Codex P1: `IF … DROP TABLE t; END IF` executes the drop when
/// the file is applied). Only statement kinds are counted here; object
/// touches and the without-WHERE risks are covered by the per-statement node
/// scans, which do not stop at anonymous blocks.
///
/// Every crawl passes `recurse_into = false` so only the *outermost* match
/// on each path counts — sqruff double-wraps some kinds (an Oracle GRANT
/// parses as `AccessStatement > AccessStatement`), and a descend-into-match
/// crawl would count both layers.
fn scan_block_body_dml(
    block: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    obj: &mut ObjectFacts,
    evidence: &mut Vec<ChangeRiskEvidence>,
    obj_evidence: &mut Vec<ObjectEvidence>,
    emit_contributions: bool,
) {
    let span_of =
        |seg: &ErasedSegment| segment_span(seg, line_at).unwrap_or_else(SourceSpan::empty);
    for seg in block.recursive_crawl(&INSERT_STATEMENTS, false, &PROCEDURAL_DEFINITIONS, false) {
        obj.insert_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.dml.insert_count",
            "sql.dml.insert",
            span_of(&seg),
        );
    }
    for seg in block.recursive_crawl(&UPDATE_STATEMENTS, false, &PROCEDURAL_DEFINITIONS, false) {
        obj.update_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.dml.update_count",
            "sql.dml.update",
            span_of(&seg),
        );
    }
    for seg in block.recursive_crawl(&DELETE_STATEMENTS, false, &PROCEDURAL_DEFINITIONS, false) {
        obj.delete_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.dml.delete_count",
            "sql.dml.delete",
            span_of(&seg),
        );
    }
    for set in [
        &CREATE_TABLE_STATEMENTS,
        &CREATE_VIEW_STATEMENTS,
        &CREATE_OTHER_STATEMENTS,
    ] {
        for seg in block.recursive_crawl(set, false, &PROCEDURAL_DEFINITIONS, false) {
            obj.create_count += 1;
            record_object(
                obj_evidence,
                emit_contributions,
                "sql.ddl.create_count",
                "sql.ddl.create",
                span_of(&seg),
            );
        }
    }
    for seg in block.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::MergeStatement),
        false,
        &PROCEDURAL_DEFINITIONS,
        false,
    ) {
        obj.merge_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.dml.merge_count",
            "sql.dml.merge",
            span_of(&seg),
        );
        record_change_risk(
            evidence,
            emit_contributions,
            span_of(&seg),
            ChangeRiskFactor::Merge,
        );
    }
    for seg in block.recursive_crawl(&DROP_STATEMENTS, false, &PROCEDURAL_DEFINITIONS, false) {
        obj.drop_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.ddl.drop_count",
            "sql.ddl.drop",
            span_of(&seg),
        );
        record_change_risk(
            evidence,
            emit_contributions,
            span_of(&seg),
            ChangeRiskFactor::Drop,
        );
    }
    for seg in block.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::TruncateStatement),
        false,
        &PROCEDURAL_DEFINITIONS,
        false,
    ) {
        obj.truncate_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.ddl.truncate_count",
            "sql.ddl.truncate",
            span_of(&seg),
        );
        record_change_risk(
            evidence,
            emit_contributions,
            span_of(&seg),
            ChangeRiskFactor::Truncate,
        );
    }
    for seg in block.recursive_crawl(
        &ALTER_TABLE_STATEMENTS,
        false,
        &PROCEDURAL_DEFINITIONS,
        false,
    ) {
        obj.alter_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.ddl.alter_count",
            "sql.ddl.alter",
            span_of(&seg),
        );
        record_change_risk(
            evidence,
            emit_contributions,
            span_of(&seg),
            ChangeRiskFactor::Alter,
        );
    }
    for seg in block.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::AccessStatement),
        false,
        &PROCEDURAL_DEFINITIONS,
        false,
    ) {
        obj.grant_revoke_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.dcl.grant_revoke_count",
            "sql.dcl.grant_revoke",
            span_of(&seg),
        );
        record_change_risk(
            evidence,
            emit_contributions,
            span_of(&seg),
            ChangeRiskFactor::GrantRevoke,
        );
    }
    for seg in block.recursive_crawl(
        &TRANSACTION_STATEMENTS,
        false,
        &PROCEDURAL_DEFINITIONS,
        false,
    ) {
        // Sole-keyword `BEGIN`/`END` wrappers are scripting-block brackets,
        // not TCL (BigQuery — Codex P1). Real transaction control inside a
        // block (`BEGIN TRANSACTION`, `COMMIT`, `ROLLBACK`) carries other
        // keywords; the only dialects lexing bare-`BEGIN` TCL (PostgreSQL,
        // MySQL) never surface it inside a typed block statement — MySQL
        // blocks live in routines and PostgreSQL DO bodies are opaque
        // dollar-quoted literals.
        if transaction_node_bracket(&seg).is_some() {
            continue;
        }
        obj.transaction_control_count += 1;
        record_object(
            obj_evidence,
            emit_contributions,
            "sql.transaction.control_count",
            "sql.transaction.control",
            span_of(&seg),
        );
        record_change_risk(
            evidence,
            emit_contributions,
            span_of(&seg),
            ChangeRiskFactor::TransactionControl,
        );
    }
}

/// The uppercased text of every *code* leaf token in `node` (comments,
/// whitespace, and meta tokens excluded), in source order. Used for
/// adjacency/word checks that must ignore comments and be whitespace-agnostic.
struct CodeToken {
    word: String,
    span: Option<SourceSpan>,
}

fn code_tokens(
    node: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    emit_contributions: bool,
) -> Vec<CodeToken> {
    fn walk(
        node: &ErasedSegment,
        line_at: &impl Fn(u32) -> u32,
        emit_contributions: bool,
        out: &mut Vec<CodeToken>,
    ) {
        let children = node.segments();
        if children.is_empty() {
            if node.is_comment() || node.is_whitespace() || node.is_meta() {
                return;
            }
            let raw = node.raw().trim();
            if !raw.is_empty() {
                out.push(CodeToken {
                    word: raw.to_ascii_uppercase(),
                    span: if emit_contributions {
                        segment_span(node, line_at)
                    } else {
                        None
                    },
                });
            }
            return;
        }
        for child in children {
            walk(child, line_at, emit_contributions, out);
        }
    }
    let mut out = Vec::new();
    walk(node, line_at, emit_contributions, &mut out);
    out
}

/// Whether `stmt` has a `WHERE` clause at its own statement level — i.e. one
/// not nested inside a subquery. The crawl stops descending at nested
/// `SelectStatement`s so a scalar subquery's `WHERE` doesn't mask a missing
/// statement-level predicate on an `UPDATE`/`DELETE`.
fn has_own_where_clause(stmt: &ErasedSegment) -> bool {
    !stmt
        .recursive_crawl(
            &SyntaxSet::single(SyntaxKind::WhereClause),
            true,
            &SELECT_STATEMENT,
            true,
        )
        .is_empty()
}

/// Write-statement kinds whose statement-level `table_reference` targets are
/// the objects they mutate. Includes the Oracle parallel kinds (see the
/// dialect-folding sets above).
const WRITE_STATEMENTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::InsertStatement,
    SyntaxKind::OracleInsertStatement,
    SyntaxKind::BulkInsertStatement,
    SyntaxKind::UpdateStatement,
    SyntaxKind::OracleUpdateStatement,
    SyntaxKind::DeleteStatement,
    SyntaxKind::OracleDeleteStatement,
    SyntaxKind::MergeStatement,
    SyntaxKind::TruncateStatement,
    SyntaxKind::AlterTableStatement,
    SyntaxKind::OracleAlterTableStatement,
    SyntaxKind::CreateTableStatement,
    SyntaxKind::OracleCreateTableStatement,
    SyntaxKind::CreateViewStatement,
    SyntaxKind::OracleCreateViewStatement,
    SyntaxKind::CreateMaterializedViewStatement,
    SyntaxKind::CreateIndexStatement,
    SyntaxKind::DropTableStatement,
    SyntaxKind::DropViewStatement,
    SyntaxKind::DropIndexStatement,
    SyntaxKind::DropFunctionStatement,
    SyntaxKind::DropSchemaStatement,
    SyntaxKind::DropStatement,
    SyntaxKind::OracleDropPackageStatement,
    SyntaxKind::OracleDropProcedureStatement,
    SyntaxKind::OracleDropSynonymStatement,
    SyntaxKind::OracleDropDatabaseLinkStatement,
]);

/// Procedural-definition statement kinds. DML/object scans pass this as their
/// `no_recursive_types` so statements *inside* a stored routine/trigger body
/// are not attributed to the top-level statement (which is `procedural`) —
/// otherwise `CREATE PROCEDURE … INSERT INTO t …` would still add `t` to the
/// object-touch sets and inflate `change_risk_score` (Codex P2). Phase 1 does
/// not analyze routine bodies; Phase 3 will.
///
/// The Oracle dialect emits its own `OracleCreate*Statement` kinds (not the
/// ANSI ones), so those are listed alongside — without them an Oracle
/// routine classifies as `create_other` and its body DML leaks into the
/// object-touch scans. Package and type bodies are containers of routines
/// and count as procedural for the same reasons.
const PROCEDURAL_DEFINITIONS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::CreateProcedureStatement,
    SyntaxKind::CreateFunctionStatement,
    SyntaxKind::CreateTriggerStatement,
    SyntaxKind::CreateTrigger,
    SyntaxKind::OracleCreateProcedureStatement,
    SyntaxKind::OracleCreateFunctionStatement,
    SyntaxKind::OracleCreateTriggerStatement,
    SyntaxKind::OracleCreatePackageStatement,
    SyntaxKind::OracleCreateTypeBodyStatement,
]);

/// Definition kinds that are themselves one routine — the granularity of a
/// `SpaceKind::Function` space. Deliberately *excludes* the package/type-body
/// containers in [`PROCEDURAL_DEFINITIONS`]: a package body is a module, and
/// its routines are the function-shaped units inside it.
pub(crate) const PROCEDURAL_UNITS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::CreateProcedureStatement,
    SyntaxKind::CreateFunctionStatement,
    SyntaxKind::CreateTriggerStatement,
    SyntaxKind::CreateTrigger,
    SyntaxKind::OracleCreateProcedureStatement,
    SyntaxKind::OracleCreateFunctionStatement,
    SyntaxKind::OracleCreateTriggerStatement,
]);

/// Name-bearing nodes that appear as *direct* children of a routine
/// definition. Direct children only: a body contains call sites whose
/// `function_name`/`object_reference` nodes belong to the *called*
/// routine, and Oracle's optional `END <name>` repeats the name inside
/// the begin/end block — crawling would pick those up.
const UNIT_NAME_KINDS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::OracleFunctionName,
    SyntaxKind::FunctionName,
    SyntaxKind::ObjectReference,
    SyntaxKind::TriggerReference,
]);

/// The routine-definition CST nodes in the same pre-order as
/// [`extract_procedural_units`] collects `ProceduralUnitFacts` — callers zip
/// the two by index (e.g. for per-unit embedded-query scoring). Two filters
/// keep the sequence honest:
/// - nodes without a position marker are dropped so both sequences stay
///   index-aligned with the facts (which cannot represent a span-less unit)
///   — otherwise every unit after a skipped node would take its neighbor's
///   embedded score (CodeRabbit);
/// - Oracle member *prototypes* are dropped: a package/type specification
///   declares `PROCEDURE p;` with the same `OracleCreateProcedureStatement`
///   kind as an implementation, but has no `BEGIN…END` body — a prototype
///   is not an executable routine and must not grow an entry path or a
///   coverage space (Codex P2). The prototype test is *contextual*, not a
///   blanket body requirement: Oracle also has executable definitions
///   without an `OracleBeginEndBlock` — Java/C call-specs (`AS LANGUAGE
///   JAVA …`) and triggers whose body is a `CALL` clause — so only bodyless
///   routines sitting inside a package/type *specification* are dropped
///   (Codex P2). A bodyless forward declaration inside a package *body* is
///   kept: it is indistinguishable from a call-spec without deeper clause
///   parsing, and over-counting a declared routine is the safer error for a
///   coverage denominator. Non-Oracle kinds keep body-less shapes: a
///   PostgreSQL `$$`-quoted body is an opaque literal, not a block node.
pub(crate) fn procedural_unit_nodes(root: &ErasedSegment) -> Vec<ErasedSegment> {
    const ORACLE_ROUTINE_KINDS: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::OracleCreateProcedureStatement,
        SyntaxKind::OracleCreateFunctionStatement,
        SyntaxKind::OracleCreateTriggerStatement,
    ]);
    // Package/type *specification* byte ranges. sqruff parses `CREATE
    // PACKAGE` and `CREATE PACKAGE BODY` as the same node kind with an
    // optional `BODY` keyword child, so the keyword picks the spec form.
    const ORACLE_SPEC_CONTAINERS: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::OracleCreatePackageStatement,
        SyntaxKind::OracleCreateTypeStatement,
    ]);
    let spec_ranges: Vec<(u32, u32)> = root
        .recursive_crawl(&ORACLE_SPEC_CONTAINERS, true, &SyntaxSet::EMPTY, false)
        .into_iter()
        .filter(|node| {
            !node.is_type(SyntaxKind::OracleCreatePackageStatement)
                || !node.segments().iter().any(|child| {
                    child.is_type(SyntaxKind::Keyword) && child.raw().eq_ignore_ascii_case("body")
                })
        })
        .filter_map(|node| {
            let pm = node.get_position_marker()?;
            Some((pm.source_slice.start as u32, pm.source_slice.end as u32))
        })
        .collect();
    root.recursive_crawl(&PROCEDURAL_UNITS, true, &SyntaxSet::EMPTY, false)
        .into_iter()
        .filter(|unit| unit.get_position_marker().is_some())
        .filter(|unit| {
            if !ORACLE_ROUTINE_KINDS.contains(unit.get_type()) {
                return true;
            }
            let has_body = !unit
                .recursive_crawl(
                    &SyntaxSet::single(SyntaxKind::OracleBeginEndBlock),
                    true,
                    &SyntaxSet::EMPTY,
                    true,
                )
                .is_empty();
            if has_body {
                return true;
            }
            // Bodyless: a prototype only in a spec context.
            let Some(pm) = unit.get_position_marker() else {
                return false;
            };
            let (start, end) = (pm.source_slice.start as u32, pm.source_slice.end as u32);
            !spec_ranges
                .iter()
                .any(|&(s, e)| s <= start && end <= e && (s, e) != (start, end))
        })
        .collect()
}

/// Collect procedural units (routine definitions) in pre-order.
///
/// `recurse_into = true` descends into matched definitions, so routines
/// nested in package bodies and subprograms declared inside another
/// routine's DECLARE section are collected after their container —
/// [`ProceduralUnitFacts`]'s ordering contract.
fn extract_procedural_units(
    root: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    tsql: bool,
    mysql: bool,
    oracle: bool,
    facts: &mut SqlFileFacts,
) {
    let units = procedural_unit_nodes(root);
    for unit in &units {
        let Some(pm) = unit.get_position_marker() else {
            continue;
        };
        let (start_byte, end_byte) = (pm.source_slice.start as u32, pm.source_slice.end as u32);
        let name = unit
            .segments()
            .iter()
            .find(|child| UNIT_NAME_KINDS.contains(child.get_type()))
            .map(|child| child.raw().trim().to_string())
            .filter(|name| !name.is_empty());
        facts.procedural_units.push(ProceduralUnitFacts {
            name,
            start_line: line_at(start_byte),
            end_line: line_at(end_byte.saturating_sub(1)),
            start_byte,
            end_byte,
            cyclomatic_complexity: 0.0,
            cognitive_complexity: 0.0,
            embedded_query_structural: 0.0,
        });
    }
    // sqruff 0.40's overridden T-SQL statement grammar leaves whole valid
    // definitions (`CREATE FUNCTION dbo.f(@x int) RETURNS int AS BEGIN …`,
    // `CREATE TRIGGER trg ON t FOR INSERT AS …`, standalone `ALTER
    // FUNCTION|TRIGGER … AS …` — Codex P1 ×2) in root `Unparsable` nodes —
    // no `PROCEDURAL_UNITS` kind ever forms. MySQL single-statement bodies
    // (`CREATE PROCEDURE p() SIGNAL SQLSTATE '45000';`) share the fate
    // (Codex P1). Recover units from the unambiguous header token shape at
    // the start of a root run: `CREATE [OR ALTER|REPLACE]
    // [DEFINER = <user>] FUNCTION|PROC[EDURE]|TRIGGER <name>` or `ALTER
    // <kind> <name>`. Scoped to the T-SQL/MySQL dialects and to *leading*
    // headers so broken declarative SQL in other dialects stays
    // unrecognized; the unit spans the whole run — the batch/delimiter
    // semantics that already govern body ownership in those dialects.
    if tsql || mysql {
        for run in root.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::Unparsable),
            true,
            &SyntaxSet::EMPTY,
            true,
        ) {
            let Some(pm) = run.get_position_marker() else {
                continue;
            };
            let (start_byte, end_byte) = (pm.source_slice.start as u32, pm.source_slice.end as u32);
            // Runs inside an already-recognized unit belong to it.
            if facts
                .procedural_units
                .iter()
                .any(|u| u.start_byte <= start_byte && end_byte <= u.end_byte)
            {
                continue;
            }
            let headers = recovered_definition_headers(&run, tsql);
            if headers.is_empty() {
                continue;
            }
            // Each header starts its own unit; a unit runs to the next
            // header or the run's end. sqruff's error recovery often
            // swallows *several* definitions (and even parseable statements
            // between them) into one run — splitting at header boundaries
            // keeps sibling definitions independent (Codex P2).
            for (k, (header_start, name)) in headers.iter().enumerate() {
                let unit_end = headers
                    .get(k + 1)
                    .map(|&(next_start, _)| next_start)
                    .unwrap_or(end_byte);
                facts.procedural_units.push(ProceduralUnitFacts {
                    name: Some(name.clone()),
                    start_line: line_at(*header_start),
                    end_line: line_at(unit_end.saturating_sub(1)),
                    start_byte: *header_start,
                    end_byte: unit_end,
                    cyclomatic_complexity: 0.0,
                    cognitive_complexity: 0.0,
                    embedded_query_structural: 0.0,
                });
            }
        }
        // Keep the pre-order contract (parents before children, source
        // order): synthetic units interleave with typed ones by position.
        facts
            .procedural_units
            .sort_by_key(|u| (u.start_byte, std::cmp::Reverse(u.end_byte)));
    }
    // Oracle package *bodies* fall into the same parse gap: sqruff 0.40
    // emits `CREATE PACKAGE BODY pkg AS PROCEDURE p IS BEGIN … END p; …`
    // wholly as a root `Unparsable` run (Codex P1). Recover the member
    // routines: `PROCEDURE|FUNCTION <name>` headers at `;` boundaries
    // inside a run *leading* with a package-body header. A member's span
    // ends at its named `END <name>` when present (the PL/SQL convention),
    // else at the next member or the run's end — so an initialization
    // section stays file-level.
    if oracle {
        for run in root.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::Unparsable),
            true,
            &SyntaxSet::EMPTY,
            true,
        ) {
            let Some(pm) = run.get_position_marker() else {
                continue;
            };
            let (start_byte, end_byte) = (pm.source_slice.start as u32, pm.source_slice.end as u32);
            if facts
                .procedural_units
                .iter()
                .any(|u| u.start_byte <= start_byte && end_byte <= u.end_byte)
            {
                continue;
            }
            let members = recovered_package_body_members(&run, end_byte);
            for (member_start, member_end, name) in members {
                facts.procedural_units.push(ProceduralUnitFacts {
                    name: Some(name),
                    start_line: line_at(member_start),
                    end_line: line_at(member_end.saturating_sub(1)),
                    start_byte: member_start,
                    end_byte: member_end,
                    cyclomatic_complexity: 0.0,
                    cognitive_complexity: 0.0,
                    embedded_query_structural: 0.0,
                });
            }
        }
        facts
            .procedural_units
            .sort_by_key(|u| (u.start_byte, std::cmp::Reverse(u.end_byte)));
    }
}

/// The member routines recovered from an Oracle package-body parse gap:
/// `(start, end, name)` per `PROCEDURE|FUNCTION <name>` header at a `;`
/// boundary in a run that *leads* with `CREATE [OR REPLACE]
/// [EDITIONABLE|NONEDITIONABLE] PACKAGE BODY <name>`. Package
/// *specifications* (no BODY keyword) declare prototypes, not members —
/// they recover nothing.
fn recovered_package_body_members(run: &ErasedSegment, run_end: u32) -> Vec<(u32, u32, String)> {
    let mut tokens = Vec::new();
    leaf_tokens(run, &mut tokens);
    let word = |j: usize| tokens.get(j).map(|(_, w)| w.as_str()).unwrap_or("");
    // Leading package-body header check.
    let mut i = 0usize;
    if !word(i).eq_ignore_ascii_case("CREATE") {
        return Vec::new();
    }
    i += 1;
    if word(i).eq_ignore_ascii_case("OR") && word(i + 1).eq_ignore_ascii_case("REPLACE") {
        i += 2;
    }
    if word(i).eq_ignore_ascii_case("EDITIONABLE") || word(i).eq_ignore_ascii_case("NONEDITIONABLE")
    {
        i += 1;
    }
    if !(word(i).eq_ignore_ascii_case("PACKAGE") && word(i + 1).eq_ignore_ascii_case("BODY")) {
        return Vec::new();
    }
    // Member headers at `;` boundaries (or right after the package's IS/AS).
    let mut headers: Vec<(usize, u32, String)> = Vec::new();
    for (j, (token_start, _)) in tokens.iter().enumerate() {
        let kind = word(j);
        if !(kind.eq_ignore_ascii_case("PROCEDURE") || kind.eq_ignore_ascii_case("FUNCTION")) {
            continue;
        }
        let prev = if j == 0 { "" } else { word(j - 1) };
        let boundary =
            prev == ";" || prev.eq_ignore_ascii_case("IS") || prev.eq_ignore_ascii_case("AS");
        if !boundary {
            continue;
        }
        let name = word(j + 1);
        if name.is_empty()
            || !name
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"')
        {
            continue;
        }
        headers.push((j, *token_start, name.to_string()));
    }
    // Spans: to the END that balances the member's body — `BEGIN`/`CASE`
    // open, bare/named `END …;` and `END CASE` close, `END IF/LOOP/WHILE/
    // REPEAT` close their own constructs without touching block depth —
    // so the common unnamed `END;` terminator is recognized too, and an
    // initialization section after the last member stays file-level
    // (Codex P2). Fallback: the next member's header or the run's end.
    let mut members = Vec::new();
    for (k, (token_idx, member_start, name)) in headers.iter().enumerate() {
        let fallback_end = headers
            .get(k + 1)
            .map(|&(_, next_start, _)| next_start)
            .unwrap_or(run_end);
        let mut member_end = fallback_end;
        let mut depth = 0u32;
        let mut opened = false;
        for j in *token_idx..tokens.len() {
            // Stop searching past the next member.
            if tokens[j].0 >= fallback_end {
                break;
            }
            let w = word(j);
            if w.eq_ignore_ascii_case("BEGIN") || w.eq_ignore_ascii_case("CASE") {
                depth += 1;
                opened = true;
            } else if w.eq_ignore_ascii_case("END") {
                let next = word(j + 1);
                if next.eq_ignore_ascii_case("IF")
                    || next.eq_ignore_ascii_case("LOOP")
                    || next.eq_ignore_ascii_case("WHILE")
                    || next.eq_ignore_ascii_case("REPEAT")
                {
                    continue;
                }
                depth = depth.saturating_sub(1);
                if opened && depth == 0 {
                    // Through the optional name and terminator.
                    let mut t = j + 1;
                    if word(t).eq_ignore_ascii_case(name) {
                        t += 1;
                    }
                    if word(t) == ";" {
                        t += 1;
                    }
                    member_end = tokens
                        .get(t - 1)
                        .map(|(b, tok)| b + tok.len() as u32)
                        .unwrap_or(fallback_end);
                    break;
                }
            }
        }
        members.push((*member_start, member_end.min(fallback_end), name.clone()));
    }
    members
}

/// The recovered definition headers in a token run: each `(start byte,
/// name)` where a `CREATE [OR ALTER|REPLACE] [DEFINER = <account>]
/// FUNCTION|PROC[EDURE]|TRIGGER <name>` header sits at a statement
/// boundary — the run's start, or right after `;`, `GO`, or a MySQL custom
/// delimiter (`//`, `$$` — punctuation-only tokens, Codex P1). sqruff's
/// error recovery can swallow several definitions into one run, so a run
/// yields one unit *per header* (Codex P2). Standalone `ALTER <kind>`
/// headers count only when `allow_alter` (T-SQL redefinitions) — MySQL's
/// `ALTER PROCEDURE p COMMENT …` alters metadata, not the body (Codex P2).
/// Names keep their original case; dotted qualification split by the lexer
/// is re-joined.
fn recovered_definition_headers(run: &ErasedSegment, allow_alter: bool) -> Vec<(u32, String)> {
    fn header_at(tokens: &[(u32, String)], mut i: usize, allow_alter: bool) -> Option<String> {
        let word = |j: usize| tokens.get(j).map(|(_, w)| w.as_str()).unwrap_or("");
        let leading_alter = word(i).eq_ignore_ascii_case("ALTER");
        if !(word(i).eq_ignore_ascii_case("CREATE") || (leading_alter && allow_alter)) {
            return None;
        }
        i += 1;
        // `CREATE OR ALTER` (T-SQL) / `CREATE OR REPLACE` (MariaDB).
        if word(i).eq_ignore_ascii_case("OR")
            && (word(i + 1).eq_ignore_ascii_case("ALTER")
                || word(i + 1).eq_ignore_ascii_case("REPLACE"))
        {
            i += 2;
        }
        // MySQL `DEFINER = <account>` between CREATE and the kind keyword:
        // the account is a multi-token expression (`` `root`@`localhost` ``,
        // `CURRENT_USER()`), so consume until the kind keyword appears
        // (bounded — Codex P1).
        if word(i).eq_ignore_ascii_case("DEFINER") {
            let limit = i + 8;
            i += 1;
            while i <= limit {
                let w = word(i);
                if w.eq_ignore_ascii_case("FUNCTION")
                    || w.eq_ignore_ascii_case("PROCEDURE")
                    || w.eq_ignore_ascii_case("PROC")
                    || w.eq_ignore_ascii_case("TRIGGER")
                    || w.is_empty()
                {
                    break;
                }
                i += 1;
            }
        }
        let kind = word(i);
        if !(kind.eq_ignore_ascii_case("FUNCTION")
            || kind.eq_ignore_ascii_case("PROCEDURE")
            || kind.eq_ignore_ascii_case("PROC")
            || kind.eq_ignore_ascii_case("TRIGGER"))
        {
            return None;
        }
        i += 1;
        let mut name = tokens.get(i)?.1.clone();
        // Identifier sanity: a name never starts with punctuation — quoted
        // identifiers (`[bracketed]`, `"double"`, MySQL backticks —
        // Codex P2) are names too.
        if !name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '[' || c == '"' || c == '`')
        {
            return None;
        }
        // Re-join dotted qualification the lexer split (`dbo` `.` `f`).
        while tokens.get(i + 1).is_some_and(|(_, w)| w == ".") {
            if let Some((_, part)) = tokens.get(i + 2) {
                name = format!("{name}.{part}");
                i += 2;
            } else {
                break;
            }
        }
        Some(name)
    }
    let mut tokens = Vec::new();
    leaf_tokens(run, &mut tokens);
    let mut headers = Vec::new();
    for j in 0..tokens.len() {
        let boundary = j == 0 || {
            let prev = tokens[j - 1].1.as_str();
            prev == ";" || prev.eq_ignore_ascii_case("GO") || is_custom_delimiter(prev)
        };
        if !boundary {
            continue;
        }
        if let Some(name) = header_at(&tokens, j, allow_alter) {
            headers.push((tokens[j].0, name));
        }
    }
    headers
}

/// Whether a token looks like a MySQL client custom delimiter (`//`, `$$`,
/// `;;`): punctuation-only, drawn from the characters delimiters are made
/// of. Identifiers, quoted names, and operators with operands never match.
fn is_custom_delimiter(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| matches!(c, '/' | '$' | ';' | '|' | '!'))
}

/// The leaf code tokens of a node with their start bytes, in source order.
fn leaf_tokens(node: &ErasedSegment, out: &mut Vec<(u32, String)>) {
    let children = node.segments();
    if children.is_empty() {
        if !(node.is_comment() || node.is_whitespace() || node.is_meta()) {
            let raw = node.raw().trim();
            if !raw.is_empty()
                && let Some(pm) = node.get_position_marker()
            {
                out.push((pm.source_slice.start as u32, raw.to_string()));
            }
        }
        return;
    }
    for child in children {
        leaf_tokens(child, out);
    }
}

/// Collect the distinct read and write object names touched by the file.
///
/// Read objects are `table_reference`s in FROM/JOIN read positions
/// (`from_expression_element`). Write objects are the statement-level
/// `table_reference` targets of write statements — the `table_reference`
/// children that are *not* inside a FROM/JOIN element (e.g. the `accounts`
/// in `UPDATE accounts …`, the `target` in `INSERT INTO target …`). Names are
/// uppercased so case variants collapse to one object.
///
/// `procedural` statements are skipped entirely: a routine body's objects
/// are touched when the routine is *called*, not when the file is applied.
/// The skip covers both well-formed definitions (whose bodies the
/// `PROCEDURAL_DEFINITIONS` crawl boundary would exclude anyway) and T-SQL
/// body fragments that sqruff splits into sibling statements (Codex P1).
fn collect_touched_objects(
    statements: &[ErasedSegment],
    kinds: &[StatementFacts],
    line_at: &impl Fn(u32) -> u32,
    emit_contributions: bool,
) -> (
    std::collections::BTreeMap<String, Option<SourceSpan>>,
    std::collections::BTreeMap<String, Option<SourceSpan>>,
) {
    use std::collections::BTreeMap;
    let mut read = BTreeMap::new();
    let mut write = BTreeMap::new();

    // CTE names are query-local aliases scoped to their *owning query block*,
    // not database objects. Scope matters at two levels:
    //   - per statement: a CTE `tmp` in `WITH tmp AS (…) SELECT … FROM tmp;`
    //     must not suppress a real `tmp` read in a *later* statement;
    //   - per nested query block: a CTE `tmp` defined inside a subquery
    //     (`SELECT … FROM tmp WHERE EXISTS (WITH tmp AS (…) SELECT … FROM tmp)`)
    //     must not suppress the *outer* real `tmp` read (CodeRabbit).
    // So a reference is treated as CTE-local only when an *ancestor*
    // `WithCompoundStatement` defines its name. We resolve that by node
    // identity (`cte_local_refs`), not by a flat name set.
    for (stmt, facts) in statements.iter().zip(kinds.iter()) {
        if facts.kind == StatementKind::Procedural {
            continue;
        }
        let cte_local = cte_local_refs(stmt);
        collect_statement_objects(
            stmt,
            &cte_local,
            &mut read,
            &mut write,
            line_at,
            emit_contributions,
        );
    }

    (read, write)
}

/// Top-level `Statement` nodes (one per DML/DDL/… statement in the file),
/// not descending into nested statements (a procedural body or a subquery's
/// inner statement is handled within its owner's scope). The single crawl
/// definition every statement-indexed consumer shares (`classify_statements`,
/// `extract_objects`, `procedural::extract`), so their zips stay aligned.
///
/// Under BigQuery, the bare `END;` scripting bracket is filtered out here —
/// it is a syntactic closer, not a statement, and counting it would inflate
/// `sql.statement.count`, the `unknown` kind, and statement-kind entropy
/// (Codex P2). Filtering at the shared crawl keeps every consumer's
/// index-zip aligned. The matching `BEGIN;` stays: it *is* the anonymous
/// scripting block.
pub(crate) fn top_level_statements(root: &ErasedSegment, bigquery: bool) -> Vec<ErasedSegment> {
    root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Statement),
        false,
        &SyntaxSet::EMPTY,
        true,
    )
    .into_iter()
    .filter(|stmt| !bigquery || bare_scripting_bracket(stmt) != Some(ScriptingBracket::End))
    .collect()
}

/// The `TableReference` nodes within `stmt` that resolve to a CTE alias visible
/// in their own query-block scope. A reference is CTE-local iff some ancestor
/// `WithCompoundStatement` defines a CTE with that name. Returned as node
/// identities so the read/write passes exclude exactly those references (and
/// not a same-named real table read in a sibling/outer scope).
fn cte_local_refs(stmt: &ErasedSegment) -> Vec<ErasedSegment> {
    let mut local = Vec::new();
    let with_blocks = stmt.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::WithCompoundStatement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for w in &with_blocks {
        // Names this WITH defines: its *direct* `CommonTableExpression`
        // children (a nested WITH's CTEs belong to that nested scope, not this
        // one, and are covered when this loop reaches the nested `w`).
        let names: std::collections::BTreeSet<String> = w
            .segments()
            .iter()
            .filter(|c| c.is_type(SyntaxKind::CommonTableExpression))
            .map(|c| cte_name(c).to_ascii_uppercase())
            .collect();
        if names.is_empty() {
            continue;
        }
        // Every table reference in this WITH's subtree (the CTE bodies and the
        // main query) is in-scope for these names; mark the ones whose name
        // matches as CTE-local.
        for tr in w.recursive_crawl(&TABLE_REFERENCES, true, &SyntaxSet::EMPTY, true) {
            if names.contains(&tr.raw().to_ascii_uppercase()) {
                local.push(tr);
            }
        }
    }
    local
}

/// Collect read/write objects for a single top-level statement. `cte_local`
/// holds the node identities of table references that resolve to an in-scope
/// CTE alias and must therefore be excluded from object-touch counts.
fn collect_statement_objects(
    stmt: &ErasedSegment,
    cte_local: &[ErasedSegment],
    read: &mut std::collections::BTreeMap<String, Option<SourceSpan>>,
    write: &mut std::collections::BTreeMap<String, Option<SourceSpan>>,
    line_at: &impl Fn(u32) -> u32,
    emit_contributions: bool,
) {
    let is_cte_local = |tr: &ErasedSegment| cte_local.iter().any(|c| c.is(tr));
    // The objects a write statement targets are not always `TableReference`s:
    // `DROP FUNCTION foo` targets a `FunctionName`, `CREATE INDEX idx …` a
    // `DatabaseReference`, etc. Match those outer reference kinds. The crawl
    // stops at nested SELECT nodes (subquery sources are not targets); it does
    // NOT recurse *into* a matched reference, so a `TableReference`'s inner
    // `ObjectReference` is not double-counted — `recursive_crawl` with
    // `recurse_into = false` returns the outermost match on each path.
    const TARGET_REFS: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::TableReference,
        SyntaxKind::OracleTableReference,
        SyntaxKind::FunctionName,
        SyntaxKind::DatabaseReference,
    ]);
    // Oracle routine/package/synonym drops name their target through kinds
    // that are far too generic to scan on every write statement (a
    // `seq.nextval` in an INSERT is an `ObjectReference` too): `DROP
    // PROCEDURE p` → `OracleFunctionName`, `DROP PACKAGE`/`DROP SYNONYM` →
    // `ObjectReference`. Without them the statement counts in
    // `sql.ddl.drop_count` but its target is missing from the write objects
    // (Codex P2), so the extended set applies to the drop family only.
    const DROP_TARGET_REFS: SyntaxSet = TARGET_REFS.union(&SyntaxSet::new(&[
        SyntaxKind::ObjectReference,
        SyntaxKind::OracleFunctionName,
    ]));

    // Writes: the mutated target of each write statement is its *first*
    // statement-level reference in document order (not inside a nested SELECT).
    // Every *other* statement-level reference is a read source — `UPDATE dst …
    // FROM src`, `DELETE dst USING src`, `MERGE INTO dst USING src`, `INSERT
    // INTO dst SELECT … FROM src`. This first-target rule is robust across
    // dialect shapes (ANSI wraps the target in a `from_expression_element`;
    // Postgres keeps it a bare child). Procedural bodies are skipped so a
    // routine's DML isn't counted as a top-level write (Phase 1). The
    // write-target *node* is tracked by identity so the read pass can skip
    // exactly that node (and not a same-named table genuinely read in a
    // subquery).
    let mut write_target_nodes: Vec<ErasedSegment> = Vec::new();
    // Write-statement node(s) inside this top-level statement, outside any
    // procedural body. Empty for read-only statements (the loop is then a
    // no-op and only the read pass below runs).
    let write_stmts = stmt.recursive_crawl(&WRITE_STATEMENTS, true, &PROCEDURAL_DEFINITIONS, true);
    for ws in &write_stmts {
        // The Oracle drop family names its target through generic reference
        // kinds; every other write statement uses the narrow set (see
        // `DROP_TARGET_REFS`).
        let refs = if matches!(
            ws.get_type(),
            SyntaxKind::OracleDropPackageStatement
                | SyntaxKind::OracleDropProcedureStatement
                | SyntaxKind::OracleDropSynonymStatement
                | SyntaxKind::OracleDropDatabaseLinkStatement
        ) {
            &DROP_TARGET_REFS
        } else {
            &TARGET_REFS
        };
        let stmt_tables = ws.recursive_crawl(refs, false, &SELECT_STATEMENT, true);
        // Multi-target DDL mutates *every* statement-level reference (`DROP
        // TABLE a, b`, `TRUNCATE a, b`), and so do INSERT statements — a plain
        // `INSERT INTO t SELECT … FROM s` keeps `s` inside the SELECT (the
        // crawl stops there), while Oracle `INSERT ALL INTO a … INTO b …`
        // legitimately lists several statement-level targets, all of them
        // written (Codex P2). Host-object shapes mutate only their first
        // reference (the target); later references are reads:
        //   - DML: `UPDATE dst … FROM src`, `MERGE INTO dst USING src` — `dst`
        //     is written, sources are read.
        //   - `CREATE INDEX idx ON t` / `DROP INDEX idx ON t` — `idx` is the
        //     written object, the host table `t` is only a read.
        //   - `CREATE TABLE child (… REFERENCES parent(id))` and
        //     `ALTER TABLE … ADD CONSTRAINT … REFERENCES parent` mutate only
        //     their subject; the referenced table is read, not written
        //     (Codex P2).
        let first_target_only = matches!(
            ws.get_type(),
            SyntaxKind::UpdateStatement
                | SyntaxKind::OracleUpdateStatement
                | SyntaxKind::DeleteStatement
                | SyntaxKind::OracleDeleteStatement
                | SyntaxKind::MergeStatement
                | SyntaxKind::CreateIndexStatement
                | SyntaxKind::DropIndexStatement
                | SyntaxKind::CreateTableStatement
                | SyntaxKind::OracleCreateTableStatement
                | SyntaxKind::AlterTableStatement
                | SyntaxKind::OracleAlterTableStatement
        );
        let all_targets = !first_target_only;
        for (i, tr) in stmt_tables.iter().enumerate() {
            if is_cte_local(tr) {
                continue; // a CTE used as a DML/MERGE target/source is query-local
            }
            let name = tr.raw().to_ascii_uppercase();
            if i == 0 || all_targets {
                record_object_occurrence(write, name, tr, line_at, emit_contributions);
                write_target_nodes.push(tr.clone());
            } else {
                // FROM/USING source tables of a DML statement are reads.
                record_object_occurrence(read, name, tr, line_at, emit_contributions);
            }
        }
    }

    // Reads: table references in FROM/JOIN positions that are not CTE
    // references and are not the write-target node itself (excluded by node
    // identity, not name — so a `DELETE FROM u` target that sits in a FROM
    // element under some dialects is not double-counted as a read, while a
    // table genuinely read in a subquery still counts). Stop at procedural
    // definitions so a routine body's FROM clauses aren't attributed to this
    // top-level statement.
    let from_elems = stmt.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::FromExpressionElement),
        true,
        &PROCEDURAL_DEFINITIONS,
        true,
    );
    for elem in &from_elems {
        for tr in elem.recursive_crawl(&TABLE_REFERENCES, true, &SyntaxSet::EMPTY, true) {
            let is_write_target = write_target_nodes.iter().any(|w| w.is(&tr));
            if !is_cte_local(&tr) && !is_write_target {
                record_object_occurrence(
                    read,
                    tr.raw().to_ascii_uppercase(),
                    &tr,
                    line_at,
                    emit_contributions,
                );
            }
        }
    }
}

fn record_object_occurrence(
    objects: &mut std::collections::BTreeMap<String, Option<SourceSpan>>,
    name: String,
    segment: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    emit_contributions: bool,
) {
    let candidate = if emit_contributions {
        segment_span(segment, line_at)
    } else {
        None
    };
    objects
        .entry(name)
        .and_modify(|current| match (*current, candidate) {
            (None, Some(_)) => *current = candidate,
            (Some(existing), Some(new)) if new.start_byte < existing.start_byte => {
                *current = candidate;
            }
            _ => {}
        })
        .or_insert(candidate);
}

pub(crate) fn statement_span(statement: &StatementFacts) -> SourceSpan {
    SourceSpan::new(
        statement.start_byte,
        statement.end_byte,
        statement.start_line,
        statement.end_line,
    )
}

fn segment_span(segment: &ErasedSegment, line_at: &impl Fn(u32) -> u32) -> Option<SourceSpan> {
    let marker = segment.get_position_marker()?;
    let start_byte = marker.source_slice.start as u32;
    let end_byte = marker.source_slice.end as u32;
    Some(SourceSpan::new(
        start_byte,
        end_byte,
        line_at(start_byte),
        line_at(end_byte.saturating_sub(1)),
    ))
}

fn record_change_risk(
    evidence: &mut Vec<ChangeRiskEvidence>,
    enabled: bool,
    span: SourceSpan,
    factor: ChangeRiskFactor,
) {
    if enabled {
        evidence.push(ChangeRiskEvidence { span, factor });
    }
}

fn record_object(
    evidence: &mut Vec<ObjectEvidence>,
    enabled: bool,
    metric: &'static str,
    reason: &'static str,
    span: SourceSpan,
) {
    if enabled {
        evidence.push(ObjectEvidence {
            metric,
            reason,
            span,
        });
    }
}

// ── Halstead ────────────────────────────────────────────────────────────

fn extract_halstead(root: &ErasedSegment, h: &mut HalsteadFacts) {
    // Operator/operand taxonomy (research foundation §7). We treat keywords
    // (statement verbs, clause/join/set/predicate keywords), operators, and
    // function names as operators; identifiers and literals as operands.
    use std::collections::BTreeMap;
    let mut operators: BTreeMap<String, u32> = BTreeMap::new();
    let mut operands: BTreeMap<String, u32> = BTreeMap::new();

    fn walk(
        node: &ErasedSegment,
        operators: &mut BTreeMap<String, u32>,
        operands: &mut BTreeMap<String, u32>,
    ) {
        // A column/table/object reference is a single operand even though it is
        // a *node* (`t`, `.`, `id`): treat its full text (`t.id`) as one
        // operand and do not recurse, otherwise `t.id` would split into two
        // operands `t` and `id`, distorting the Halstead operand counts (§7).
        if node.is_type(SyntaxKind::ColumnReference)
            || node.is_type(SyntaxKind::TableReference)
            || node.is_type(SyntaxKind::ObjectReference)
        {
            let raw = node.raw().trim();
            if !raw.is_empty() {
                *operands.entry(raw.to_ascii_lowercase()).or_default() += 1;
            }
            return;
        }
        let children = node.segments();
        if children.is_empty() {
            // Leaf token: classify.
            if node.is_comment() || node.is_whitespace() || node.is_meta() {
                return;
            }
            let kind = node.get_type();
            let raw = node.raw().trim();
            if raw.is_empty() {
                return;
            }
            match kind {
                // Operands: identifiers, literals, parameters.
                SyntaxKind::NakedIdentifier
                | SyntaxKind::QuotedIdentifier
                | SyntaxKind::NumericLiteral
                | SyntaxKind::QuotedLiteral
                | SyntaxKind::BooleanLiteral
                | SyntaxKind::NullLiteral
                | SyntaxKind::ColumnReference
                | SyntaxKind::Parameter => {
                    *operands.entry(raw.to_ascii_lowercase()).or_default() += 1;
                }
                // Punctuation that isn't semantically an operator.
                SyntaxKind::Comma
                | SyntaxKind::Dot
                | SyntaxKind::StartBracket
                | SyntaxKind::EndBracket
                | SyntaxKind::StatementTerminator
                | SyntaxKind::Semicolon => {}
                // Everything else that is code (keywords, operators, function
                // name identifiers, stars) counts as an operator.
                _ => {
                    *operators.entry(raw.to_ascii_uppercase()).or_default() += 1;
                }
            }
            return;
        }
        for child in children {
            walk(child, operators, operands);
        }
    }
    walk(root, &mut operators, &mut operands);

    h.distinct_operators = operators.len() as u32;
    h.distinct_operands = operands.len() as u32;
    h.total_operators = operators.values().sum();
    h.total_operands = operands.values().sum();
}

// ── generic helpers ──────────────────────────────────────────────────

/// Count direct children of `node` with the given kind.
fn count_direct(node: &ErasedSegment, kind: SyntaxKind) -> u32 {
    node.segments().iter().filter(|c| c.is_type(kind)).count() as u32
}

/// Count occurrences of `kind` anywhere in the subtree (including `node`).
fn count_anywhere(node: &ErasedSegment, kind: SyntaxKind) -> u32 {
    node.recursive_crawl(&SyntaxSet::single(kind), true, &SyntaxSet::EMPTY, true)
        .len() as u32
}

/// Count occurrences of any kind in `set` anywhere in the subtree.
fn count_any(node: &ErasedSegment, set: &SyntaxSet) -> u32 {
    node.recursive_crawl(set, true, &SyntaxSet::EMPTY, true)
        .len() as u32
}

/// The keyword tokens whose raw text equals `word` (case-insensitive).
fn keyword_tokens(node: &ErasedSegment, word: &str) -> Vec<ErasedSegment> {
    node.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Keyword),
        true,
        &SyntaxSet::EMPTY,
        true,
    )
    .into_iter()
    .filter(|k| k.raw().eq_ignore_ascii_case(word))
    .collect()
}

/// Count keyword tokens whose raw text equals `word` (case-insensitive).
fn count_keyword(node: &ErasedSegment, word: &str) -> u32 {
    let kws = node.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Keyword),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    kws.iter()
        .filter(|k| k.raw().eq_ignore_ascii_case(word))
        .count() as u32
}
