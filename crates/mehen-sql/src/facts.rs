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
//! `SyntaxKind` and records facts. Where sqruff already ships higher-level
//! analysis (the `Query`/CTE model in `utils::analysis::query`), the adapter
//! uses it directly rather than re-deriving the CTE dependency graph.

use sqruff_lib_core::dialects::Dialect;
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
    SetOperation,
    Unknown,
}

impl StatementKind {
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

/// Predicate / boolean-logic facts (research foundation §6.7).
#[derive(Clone, Debug, Default)]
pub(crate) struct PredicateFacts {
    pub boolean_operator_count: u32,
    pub max_boolean_depth: u32,
    pub not_count: u32,
    pub comparison_count: u32,
    pub in_count: u32,
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

/// CTE-graph facts derived from sqruff's `Query` analysis (research
/// foundation §6.4).
#[derive(Clone, Debug, Default)]
pub(crate) struct CteFacts {
    pub count: u32,
    pub recursive_count: u32,
    pub dependency_edges: u32,
    pub max_dependency_depth: u32,
    pub max_fan_out: u32,
    pub unused_count: u32,
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
    pub write_object_count: u32,
    pub read_object_count: u32,
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
    pub halstead: HalsteadFacts,
    pub relation_ref_count: u32,
    /// Count of `SyntaxKind::Unparsable` segments (parser-health, §6.16).
    pub unparsable_segments: u32,
    /// Lines touched by unparsable segments.
    pub unparsable_lines: u32,
}

// ── SyntaxSet constants ────────────────────────────────────────────────
//
// `children()` requires a `&'static SyntaxSet`, and const construction keeps
// the bitsets out of the hot path.

const SELECT_STATEMENT: SyntaxSet = SyntaxSet::single(SyntaxKind::SelectStatement);

/// Build facts for `root` (the parsed `File` segment) under `dialect`.
pub(crate) fn extract(
    root: &ErasedSegment,
    dialect: &Dialect,
    line_at: impl Fn(u32) -> u32,
) -> SqlFileFacts {
    let mut facts = SqlFileFacts::default();

    // ── statements ──────────────────────────────────────────────────
    classify_statements(root, &line_at, &mut facts);

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
        count_anywhere(root, SyntaxKind::TableReference) + facts.subqueries.derived_table_count;

    // ── CTE graph (via sqruff Query analysis) ───────────────────────
    extract_cte_graph(root, dialect, &mut facts.ctes);

    // ── object-touch / DML-DDL risk ─────────────────────────────────
    extract_objects(root, &line_at, &mut facts);

    // ── Halstead ────────────────────────────────────────────────────
    extract_halstead(root, &mut facts.halstead);

    facts
}

// ── statement classification ───────────────────────────────────────────

fn classify_statements(
    root: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    facts: &mut SqlFileFacts,
) {
    // Top-level `Statement` nodes are direct children of `File`; do not
    // recurse into nested statements (a subquery `SELECT` is a query block,
    // not a top-level statement).
    let statements = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::Statement),
        false,
        &SyntaxSet::EMPTY,
        false,
    );
    for stmt in &statements {
        let kind = classify_statement(stmt);
        let (start_byte, end_byte) = stmt
            .get_position_marker()
            .map(|pm| (pm.source_slice.start as u32, pm.source_slice.end as u32))
            .unwrap_or((0, 0));
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
/// contains. sqruff produces dialect-specific `Drop*`/`Create*` variants, so
/// we probe with a broad `SyntaxSet` and map by the first match.
fn classify_statement(stmt: &ErasedSegment) -> StatementKind {
    let has = |k: SyntaxKind| {
        !stmt
            .recursive_crawl(&SyntaxSet::single(k), false, &SyntaxSet::EMPTY, true)
            .is_empty()
    };

    // Order matters: more specific kinds first.
    if has(SyntaxKind::WithCompoundStatement) {
        return StatementKind::WithSelect;
    }
    if has(SyntaxKind::MergeStatement) {
        return StatementKind::Merge;
    }
    if has(SyntaxKind::InsertStatement) {
        return StatementKind::Insert;
    }
    if has(SyntaxKind::UpdateStatement) {
        return StatementKind::Update;
    }
    if has(SyntaxKind::DeleteStatement) {
        return StatementKind::Delete;
    }
    if has(SyntaxKind::TruncateStatement) {
        return StatementKind::Truncate;
    }
    if has(SyntaxKind::AlterTableStatement) {
        return StatementKind::AlterTable;
    }
    // CREATE family: distinguish CTAS, view, table, other.
    if has(SyntaxKind::CreateTableStatement) {
        // CTAS = CREATE TABLE … AS SELECT — the statement embeds a select.
        if has(SyntaxKind::SelectStatement) || has(SyntaxKind::WithCompoundStatement) {
            return StatementKind::CreateTableAsSelect;
        }
        return StatementKind::CreateTable;
    }
    if has(SyntaxKind::CreateViewStatement) || has(SyntaxKind::CreateMaterializedViewStatement) {
        return StatementKind::CreateView;
    }
    if stmt_contains_create(stmt) {
        return StatementKind::CreateOther;
    }
    if has(SyntaxKind::DropTableStatement)
        || has(SyntaxKind::DropViewStatement)
        || has(SyntaxKind::DropIndexStatement)
        || has(SyntaxKind::DropStatement)
        || has(SyntaxKind::DropFunctionStatement)
        || has(SyntaxKind::DropSchemaStatement)
    {
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
    if has(SyntaxKind::TransactionStatement) {
        return StatementKind::TransactionControl;
    }
    if has(SyntaxKind::ExplainStatement) {
        return StatementKind::Explain;
    }
    if has(SyntaxKind::SetExpression) {
        return StatementKind::SetOperation;
    }
    if has(SyntaxKind::SelectStatement) {
        return StatementKind::Select;
    }
    if stmt_is_procedural(stmt) {
        return StatementKind::Procedural;
    }
    StatementKind::Unknown
}

fn stmt_contains_create(stmt: &ErasedSegment) -> bool {
    stmt.raw()
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE")
}

fn stmt_is_procedural(stmt: &ErasedSegment) -> bool {
    const PROCEDURAL: SyntaxSet = SyntaxSet::new(&[
        SyntaxKind::CreateProcedureStatement,
        SyntaxKind::CreateFunctionStatement,
        SyntaxKind::CreateTriggerStatement,
    ]);
    !stmt
        .recursive_crawl(&PROCEDURAL, false, &SyntaxSet::EMPTY, true)
        .is_empty()
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
fn extract_joins(root: &ErasedSegment, joins: &mut JoinFacts) {
    let clauses = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::JoinClause),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    joins.total = clauses.len() as u32;
    for clause in &clauses {
        let raw = clause.raw().to_ascii_uppercase();
        // Classify by leading keywords (the join clause text begins with the
        // join keywords before the joined relation).
        let kind_word = join_kind(&raw);
        match kind_word {
            JoinWord::Left => joins.left += 1,
            JoinWord::Right => joins.right += 1,
            JoinWord::Full => joins.full += 1,
            JoinWord::Cross => joins.cross += 1,
            JoinWord::Inner => joins.inner += 1,
        }
        if raw.contains("NATURAL") {
            joins.natural += 1;
        }
        if raw.contains("LATERAL") || raw.contains("APPLY") {
            joins.lateral += 1;
        }
        let has_condition = !clause
            .recursive_crawl(
                &SyntaxSet::single(SyntaxKind::JoinOnCondition),
                true,
                &SyntaxSet::EMPTY,
                true,
            )
            .is_empty()
            || raw.contains(" USING");
        // CROSS / NATURAL joins legitimately omit a condition; only flag the
        // others.
        if !has_condition && !matches!(kind_word, JoinWord::Cross) && !raw.contains("NATURAL") {
            joins.missing_condition += 1;
        }
        if has_condition && !join_condition_has_equality(clause) {
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

fn join_kind(raw: &str) -> JoinWord {
    if raw.contains("LEFT") {
        JoinWord::Left
    } else if raw.contains("RIGHT") {
        JoinWord::Right
    } else if raw.contains("FULL") {
        JoinWord::Full
    } else if raw.contains("CROSS") {
        JoinWord::Cross
    } else {
        JoinWord::Inner
    }
}

fn join_condition_has_equality(clause: &ErasedSegment) -> bool {
    let conds = clause.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::JoinOnCondition),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for cond in &conds {
        let comparisons = cond.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::ComparisonOperator),
            true,
            &SyntaxSet::EMPTY,
            true,
        );
        for c in &comparisons {
            if c.raw().contains('=') {
                return true;
            }
        }
    }
    // `USING` is inherently equality.
    clause.raw().to_ascii_uppercase().contains(" USING")
}

// ── set operations ───────────────────────────────────────────────────

fn extract_set_ops(root: &ErasedSegment, set_ops: &mut SetOpFacts) {
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

fn extract_cases(root: &ErasedSegment, cases: &mut CaseFacts) {
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
        let has_else = !case
            .recursive_crawl(
                &SyntaxSet::single(SyntaxKind::ElseClause),
                true,
                &SyntaxSet::EMPTY,
                true,
            )
            .is_empty();
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

fn extract_windows(root: &ErasedSegment, windows: &mut WindowFacts) {
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
            windows.partition_expression_count += count_direct(p, SyntaxKind::Expression).max(
                // Some partition keys are bare column_references, not wrapped
                // in Expression.
                count_direct(p, SyntaxKind::ColumnReference),
            );
        }
        let orders = over.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::OrderbyClause),
            true,
            &SyntaxSet::EMPTY,
            true,
        );
        for o in &orders {
            windows.order_expression_count += count_direct(o, SyntaxKind::ColumnReference)
                .max(count_direct(o, SyntaxKind::Expression));
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

fn extract_aggregates(root: &ErasedSegment, agg: &mut AggregateFacts) {
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
                if func.raw().to_ascii_uppercase().contains("DISTINCT") {
                    agg.distinct_count += 1;
                }
            }
        }
    }
    agg.group_by_count = count_anywhere(root, SyntaxKind::GroupbyClause);
    agg.having_count = count_anywhere(root, SyntaxKind::HavingClause);
    agg.rollup_count = count_anywhere(root, SyntaxKind::CubeRollupClause);
    agg.grouping_sets_count = count_anywhere(root, SyntaxKind::GroupingSetsClause);
    // CubeRollupClause covers both CUBE and ROLLUP in sqruff; split by keyword.
    let cube_rollups = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::CubeRollupClause),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut cube = 0u32;
    let mut rollup = 0u32;
    for cr in &cube_rollups {
        let raw = cr.raw().to_ascii_uppercase();
        if raw.contains("CUBE") {
            cube += 1;
        }
        if raw.contains("ROLLUP") {
            rollup += 1;
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

fn extract_predicates(root: &ErasedSegment, pred: &mut PredicateFacts) {
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
    pred.not_count = count_keyword(root, "NOT");
    pred.comparison_count = count_anywhere(root, SyntaxKind::ComparisonOperator);
    // `IN (...)` predicates.
    pred.in_count = count_keyword(root, "IN");

    // Max boolean nesting depth within predicate-bearing clauses.
    let parents = root.recursive_crawl(&PREDICATE_PARENTS, true, &SyntaxSet::EMPTY, true);
    for parent in &parents {
        pred.max_boolean_depth = pred.max_boolean_depth.max(boolean_depth(parent));
    }

    // NULL-semantics risk: `NOT IN`, `= NULL`, `<> NULL`, `!= NULL`.
    let upper = root.raw().to_ascii_uppercase();
    pred.null_semantics_risk_count = count_substr(&upper, "NOT IN")
        + count_substr(&upper, "= NULL")
        + count_substr(&upper, "<> NULL")
        + count_substr(&upper, "!= NULL");
}

/// Approximate boolean-expression nesting depth: the deepest chain of
/// bracketed groups containing AND/OR within `node`.
fn boolean_depth(node: &ErasedSegment) -> u32 {
    fn walk(node: &ErasedSegment, depth: u32) -> u32 {
        let mut max = depth;
        let mut next = depth;
        if node.is_type(SyntaxKind::Bracketed) {
            // A bracketed group that contains a boolean operator adds a level.
            let contains_bool = node
                .recursive_crawl(
                    &SyntaxSet::single(SyntaxKind::BinaryOperator),
                    true,
                    &SyntaxSet::EMPTY,
                    true,
                )
                .iter()
                .any(|o| {
                    let r = o.raw().to_ascii_uppercase();
                    r == "AND" || r == "OR"
                });
            if contains_bool {
                next = depth + 1;
                max = max.max(next);
            }
        }
        for child in node.segments() {
            max = max.max(walk(child, next));
        }
        max
    }
    // Base level is 1 if any boolean operator is present at all.
    let has_bool = node
        .recursive_crawl(
            &SyntaxSet::single(SyntaxKind::BinaryOperator),
            true,
            &SyntaxSet::EMPTY,
            true,
        )
        .iter()
        .any(|o| {
            let r = o.raw().to_ascii_uppercase();
            r == "AND" || r == "OR"
        });
    if !has_bool {
        return 0;
    }
    walk(node, 1)
}

// ── subqueries / derived tables ───────────────────────────────────────

fn extract_subqueries(root: &ErasedSegment, selects: &[ErasedSegment], sub: &mut SubqueryFacts) {
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
    // EXISTS / IN subqueries by keyword adjacency.
    let upper = root.raw().to_ascii_uppercase();
    sub.exists_count = count_substr(&upper, "EXISTS (") + count_substr(&upper, "EXISTS(");
    sub.in_count = count_substr(&upper, "IN (SELECT") + count_substr(&upper, "IN(SELECT");
    // Scalar subqueries: SELECT inside a select_clause_element expression.
    sub.scalar_count = count_scalar_subqueries(root);
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
    // Collect aliases/tables defined inside the subquery.
    let inner_relations = relation_names(subquery);
    // Qualified column refs inside the subquery.
    let col_refs = subquery.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::ColumnReference),
        true,
        &SyntaxSet::EMPTY,
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

fn relation_names(node: &ErasedSegment) -> Vec<String> {
    let mut names = Vec::new();
    // Table references and their aliases define the in-scope relation names.
    let table_refs = node.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::TableReference),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for tr in &table_refs {
        names.push(last_identifier(tr));
    }
    let aliases = node.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::AliasExpression),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for a in &aliases {
        if let Some(id) = a
            .recursive_crawl(
                &SyntaxSet::single(SyntaxKind::NakedIdentifier),
                true,
                &SyntaxSet::EMPTY,
                true,
            )
            .first()
        {
            names.push(id.raw().to_string());
        }
    }
    names
}

/// The qualifier of a column reference (`c` in `c.id`), if it is qualified.
fn column_qualifier(col: &ErasedSegment) -> Option<String> {
    let idents = col.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::NakedIdentifier),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    // A qualified ref has at least two identifier parts separated by a dot.
    if col.raw().contains('.') && idents.len() >= 2 {
        Some(idents[0].raw().to_string())
    } else {
        None
    }
}

fn last_identifier(node: &ErasedSegment) -> String {
    let idents = node.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::NakedIdentifier),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    idents
        .last()
        .map(|i| i.raw().to_string())
        .unwrap_or_else(|| node.raw().to_string())
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

fn extract_expressions(root: &ErasedSegment, expr: &mut ExpressionFacts) {
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
    expr.cast_count = count_anywhere(root, SyntaxKind::CastExpression)
        + count_substr(&root.raw().to_ascii_uppercase(), "::");
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
        let is_expr = elem
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
            .is_empty()
            .not_eq();
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
    out.table_alias_count = count_anywhere(root, SyntaxKind::AliasExpression);
}

/// Count the relations referenced *directly* by a SELECT's own FROM/JOIN
/// (not nested subqueries).
fn count_direct_relations(sel: &ErasedSegment) -> u32 {
    sel.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::TableReference),
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

fn extract_cte_graph(root: &ErasedSegment, _dialect: &Dialect, ctes: &mut CteFacts) {
    // The CTE dependency graph is derived directly from `CommonTableExpression`
    // CST nodes: each carries a name identifier and a body whose
    // `TableReference`s name its dependencies. We deliberately avoid sqruff's
    // `Query`/`crawl_sources` analysis here — it uses interior mutability
    // (`Rc<RefCell<…>>`) and re-borrows while crawling, which would conflict
    // with any borrow held across the call. The CST is the stable, verified
    // shape (parser comparison §2.1).
    let cte_nodes = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::CommonTableExpression),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    ctes.count = cte_nodes.len() as u32;
    if ctes.count == 0 {
        return;
    }

    // CTE names (the leading identifier of each CTE definition).
    let cte_names: Vec<String> = cte_nodes.iter().map(cte_name).collect();

    let recursive_keyword = root.raw().to_ascii_uppercase().contains("WITH RECURSIVE");

    // Build the dependency graph: edge cte_a -> cte_b when a's body reads b.
    use std::collections::{BTreeMap, BTreeSet};
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fan_out: BTreeMap<String, u32> = BTreeMap::new();
    let mut self_recursive = 0u32;

    for (idx, cte) in cte_nodes.iter().enumerate() {
        let name_up = cte_names[idx].to_ascii_uppercase();
        for dep in cte_body_dependencies(cte, &cte_names) {
            let dep_up = dep.to_ascii_uppercase();
            if dep_up == name_up {
                self_recursive += 1;
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
    ctes.recursive_count = if recursive_keyword {
        self_recursive.max(1)
    } else {
        self_recursive
    };
    ctes.max_fan_out = fan_out.values().copied().max().unwrap_or(0);
    ctes.max_dependency_depth = longest_chain(&edges, &cte_names);

    // Unused CTEs: defined but referenced by neither another CTE body nor the
    // final query. Build the full set of referenced CTE names from every
    // table reference in the file that matches a CTE name, then subtract.
    let all_refs = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::TableReference),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    // A CTE name only counts as "referenced" when it appears as a table
    // reference *outside* its own definition body.
    for (idx, cte) in cte_nodes.iter().enumerate() {
        let self_name = cte_names[idx].to_ascii_uppercase();
        for r in &all_refs {
            let name = r.raw().to_ascii_uppercase();
            if name == self_name && !is_within(cte, r) {
                referenced.insert(name);
            }
        }
    }
    ctes.unused_count = cte_names
        .iter()
        .filter(|n| !referenced.contains(&n.to_ascii_uppercase()))
        .count() as u32;
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

/// CTE names referenced inside `cte`'s body (its dependencies).
fn cte_body_dependencies(cte: &ErasedSegment, cte_names: &[String]) -> Vec<String> {
    let refs = cte.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::TableReference),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    let mut deps = Vec::new();
    for r in &refs {
        let name = r.raw().to_ascii_uppercase();
        if cte_names.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
            deps.push(name);
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

fn extract_objects(root: &ErasedSegment, line_at: &impl Fn(u32) -> u32, facts: &mut SqlFileFacts) {
    let _ = line_at;
    let obj = &mut facts.objects;
    for stmt in &facts.statements {
        match stmt.kind {
            StatementKind::Insert => {
                obj.insert_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::Update => {
                obj.update_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::Delete => {
                obj.delete_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::Merge => {
                obj.merge_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::CreateTable
            | StatementKind::CreateTableAsSelect
            | StatementKind::CreateView
            | StatementKind::CreateOther => {
                obj.create_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::AlterTable => {
                obj.alter_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::Drop => {
                obj.drop_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::Truncate => {
                obj.truncate_count += 1;
                obj.write_object_count += 1;
            }
            StatementKind::Grant | StatementKind::Revoke => obj.grant_revoke_count += 1,
            StatementKind::TransactionControl => obj.transaction_control_count += 1,
            StatementKind::Select | StatementKind::WithSelect | StatementKind::SetOperation => {
                obj.read_object_count += 1
            }
            _ => {}
        }
    }

    // UPDATE/DELETE without WHERE.
    let updates = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::UpdateStatement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for u in &updates {
        if u.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::WhereClause),
            true,
            &SyntaxSet::EMPTY,
            true,
        )
        .is_empty()
        {
            obj.update_without_where_count += 1;
        }
    }
    let deletes = root.recursive_crawl(
        &SyntaxSet::single(SyntaxKind::DeleteStatement),
        true,
        &SyntaxSet::EMPTY,
        true,
    );
    for d in &deletes {
        if d.recursive_crawl(
            &SyntaxSet::single(SyntaxKind::WhereClause),
            true,
            &SyntaxSet::EMPTY,
            true,
        )
        .is_empty()
        {
            obj.delete_without_where_count += 1;
        }
    }

    // CREATE OR REPLACE.
    obj.create_or_replace_count =
        count_substr(&root.raw().to_ascii_uppercase(), "CREATE OR REPLACE");

    // RETURNING / OUTPUT clauses.
    let upper = root.raw().to_ascii_uppercase();
    obj.returning_count = count_substr(&upper, "RETURNING ");
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

fn count_substr(haystack: &str, needle: &str) -> u32 {
    haystack.matches(needle).count() as u32
}

trait BoolExt {
    fn not_eq(self) -> bool;
}
impl BoolExt for bool {
    fn not_eq(self) -> bool {
        !self
    }
}
