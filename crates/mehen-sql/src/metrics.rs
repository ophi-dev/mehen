// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Raw + composite metric publishing.
//!
//! Maps [`crate::facts::SqlFileFacts`] (and LOC/dialect inputs) into the flat
//! `sql.*` `MetricSet` keys the shared report schema serializes. Metric keys
//! follow the catalogue in `docs/metrics/sql/overview.mdx` and the research
//! foundation §6/§7/§8. Composite scores are computed here from the raw
//! values so they stay explainable (every weight is visible in one place).

use mehen_core::{MetricKey, MetricSet};

use crate::dialect::{DialectResolution, dialect_label};
use crate::facts::{SqlFileFacts, StatementKind};
use crate::loc::SqlLoc;

/// All distinct statement kinds, so `kind_count.<kind>` keys are emitted with
/// an explicit `0` when absent (grepability over silent omission).
const ALL_STATEMENT_KINDS: &[StatementKind] = &[
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
    StatementKind::SetOperation,
    StatementKind::Unknown,
];

const JOIN_KINDS: &[&str] = &[
    "inner", "left", "right", "full", "cross", "natural", "lateral",
];
const SET_OP_KINDS: &[&str] = &["union", "union_all", "intersect", "except"];

/// Publish every `sql.*` metric for one file into `target`.
pub(crate) fn publish(
    facts: &SqlFileFacts,
    loc: &SqlLoc,
    dialect: &DialectResolution,
    target: &mut MetricSet,
) {
    publish_loc(loc, target);
    publish_statements(facts, target);
    publish_query_blocks(facts, target);
    publish_cte(facts, target);
    publish_joins(facts, target);
    publish_subqueries(facts, target);
    publish_predicates(facts, target);
    publish_case(facts, target);
    publish_aggregates(facts, target);
    publish_windows(facts, target);
    publish_set_ops(facts, target);
    publish_expressions(facts, target);
    publish_output(facts, target);
    publish_objects(facts, target);
    publish_dialect(facts, loc, dialect, target);
    publish_parser(facts, loc, target);
    publish_halstead(facts, target);
    publish_composites(facts, loc, dialect, target);
}

fn set(target: &mut MetricSet, key: &str, value: impl Into<mehen_core::MetricValue>) {
    target.insert(MetricKey::new(key), value);
}

fn publish_loc(loc: &SqlLoc, target: &mut MetricSet) {
    set(target, "sql.loc.physical", loc.physical);
    set(target, "sql.loc.code", loc.code);
    set(target, "sql.loc.comment", loc.comment);
    set(target, "sql.loc.blank", loc.blank);
    set(target, "sql.loc.logical", loc.logical);
    set(target, "sql.loc.comment_density", loc.comment_density());
    set(
        target,
        "sql.loc.max_statement_lines",
        loc.max_statement_lines,
    );
    set(
        target,
        "sql.loc.avg_statement_lines",
        loc.avg_statement_lines,
    );
}

fn publish_statements(facts: &SqlFileFacts, target: &mut MetricSet) {
    set(target, "sql.statement.count", facts.statements.len());
    // kind_count.<kind>
    for kind in ALL_STATEMENT_KINDS {
        let n = facts.statements.iter().filter(|s| s.kind == *kind).count();
        set(
            target,
            &format!("sql.statement.kind_count.{}", kind.label()),
            n,
        );
    }
    let distinct: std::collections::BTreeSet<_> = facts.statements.iter().map(|s| s.kind).collect();
    set(target, "sql.statement.kind_distinct", distinct.len());
    set(target, "sql.statement.kind_entropy", kind_entropy(facts));
    let unparsed = facts
        .statements
        .iter()
        .filter(|s| s.kind == StatementKind::Unknown)
        .count();
    set(target, "sql.statement.unparsed_count", unparsed);
}

/// Normalized Shannon entropy over statement kinds (research foundation §6.2).
fn kind_entropy(facts: &SqlFileFacts) -> f64 {
    use std::collections::BTreeMap;
    let total = facts.statements.len();
    if total == 0 {
        return 0.0;
    }
    let mut counts: BTreeMap<StatementKind, usize> = BTreeMap::new();
    for s in &facts.statements {
        *counts.entry(s.kind).or_default() += 1;
    }
    let distinct = counts.len();
    if distinct <= 1 {
        return 0.0;
    }
    let total_f = total as f64;
    let h: f64 = counts
        .values()
        .map(|&c| {
            let p = c as f64 / total_f;
            -p * p.log2()
        })
        .sum();
    h / (distinct.max(2) as f64).log2()
}

fn publish_query_blocks(facts: &SqlFileFacts, target: &mut MetricSet) {
    set(target, "sql.query_block.count", facts.query_block_count);
    set(
        target,
        "sql.query_block.max_depth",
        facts.query_block_max_depth,
    );
    let avg = if facts.query_block_count > 0 {
        facts.select_item_total as f64 / facts.query_block_count as f64
    } else {
        0.0
    };
    set(target, "sql.query_block.avg_select_items", avg);
    set(
        target,
        "sql.query_block.max_select_items",
        facts.select_item_max,
    );
}

fn publish_cte(facts: &SqlFileFacts, target: &mut MetricSet) {
    let c = &facts.ctes;
    set(target, "sql.cte.count", c.count);
    set(target, "sql.cte.recursive_count", c.recursive_count);
    set(target, "sql.cte.dependency_edges", c.dependency_edges);
    set(
        target,
        "sql.cte.max_dependency_depth",
        c.max_dependency_depth,
    );
    set(target, "sql.cte.max_fan_out", c.max_fan_out);
    set(target, "sql.cte.unused_count", c.unused_count);
    set(target, "sql.cte.trivial_count", c.trivial_count);
}

fn publish_joins(facts: &SqlFileFacts, target: &mut MetricSet) {
    let j = &facts.joins;
    set(target, "sql.join.count", j.total);
    set(target, "sql.join.kind_count.inner", j.inner);
    set(target, "sql.join.kind_count.left", j.left);
    set(target, "sql.join.kind_count.right", j.right);
    set(target, "sql.join.kind_count.full", j.full);
    set(target, "sql.join.kind_count.cross", j.cross);
    set(target, "sql.join.kind_count.natural", j.natural);
    set(target, "sql.join.kind_count.lateral", j.lateral);
    let _ = JOIN_KINDS; // documented kind list (kept for parity with docs)
    set(target, "sql.join.outer_count", j.left + j.right + j.full);
    set(target, "sql.join.cross_count", j.cross);
    set(target, "sql.join.natural_count", j.natural);
    set(target, "sql.join.non_equi_count", j.non_equi);
    set(
        target,
        "sql.join.missing_condition_count",
        j.missing_condition,
    );
}

fn publish_subqueries(facts: &SqlFileFacts, target: &mut MetricSet) {
    let s = &facts.subqueries;
    set(target, "sql.subquery.count", s.count);
    set(target, "sql.subquery.max_depth", s.max_depth);
    set(target, "sql.subquery.correlated_count", s.correlated_count);
    set(target, "sql.subquery.scalar_count", s.scalar_count);
    set(target, "sql.subquery.exists_count", s.exists_count);
    set(target, "sql.subquery.in_count", s.in_count);
    set(target, "sql.derived_table.count", s.derived_table_count);
}

fn publish_predicates(facts: &SqlFileFacts, target: &mut MetricSet) {
    let p = &facts.predicates;
    set(
        target,
        "sql.predicate.boolean_operator_count",
        p.boolean_operator_count,
    );
    set(
        target,
        "sql.predicate.max_boolean_depth",
        p.max_boolean_depth,
    );
    set(target, "sql.predicate.not_count", p.not_count);
    set(target, "sql.predicate.comparison_count", p.comparison_count);
    set(
        target,
        "sql.predicate.null_semantics_risk_count",
        p.null_semantics_risk_count,
    );
}

fn publish_case(facts: &SqlFileFacts, target: &mut MetricSet) {
    let c = &facts.cases;
    set(target, "sql.case.count", c.count);
    set(target, "sql.case.max_depth", c.max_depth);
    set(target, "sql.case.when_count", c.when_count);
    set(target, "sql.case.max_when_count", c.max_when_count);
    set(target, "sql.case.missing_else_count", c.missing_else_count);
}

fn publish_aggregates(facts: &SqlFileFacts, target: &mut MetricSet) {
    let a = &facts.aggregates;
    set(target, "sql.aggregate.function_count", a.function_count);
    set(target, "sql.aggregate.distinct_count", a.distinct_count);
    set(target, "sql.group_by.count", a.group_by_count);
    set(target, "sql.group_by.rollup_count", a.rollup_count);
    set(target, "sql.group_by.cube_count", a.cube_count);
    set(
        target,
        "sql.group_by.grouping_sets_count",
        a.grouping_sets_count,
    );
    set(target, "sql.having.count", a.having_count);
}

fn publish_windows(facts: &SqlFileFacts, target: &mut MetricSet) {
    let w = &facts.windows;
    set(target, "sql.window.function_count", w.function_count);
    set(target, "sql.window.frame_count", w.frame_count);
    set(
        target,
        "sql.window.partition_expression_count",
        w.partition_expression_count,
    );
    set(
        target,
        "sql.window.order_expression_count",
        w.order_expression_count,
    );
}

fn publish_set_ops(facts: &SqlFileFacts, target: &mut MetricSet) {
    let s = &facts.set_ops;
    set(target, "sql.set_op.count", s.count);
    set(target, "sql.set_op.kind_count.union", s.union_count);
    set(target, "sql.set_op.kind_count.union_all", s.union_all_count);
    set(target, "sql.set_op.kind_count.intersect", s.intersect_count);
    set(target, "sql.set_op.kind_count.except", s.except_count);
    let _ = SET_OP_KINDS;
    let union_total = s.union_count + s.union_all_count;
    let ratio = if union_total > 0 {
        s.union_all_count as f64 / union_total as f64
    } else {
        0.0
    };
    set(target, "sql.set_op.union_all_ratio", ratio);
}

fn publish_expressions(facts: &SqlFileFacts, target: &mut MetricSet) {
    let e = &facts.expressions;
    set(target, "sql.expression.max_depth", e.max_depth);
    set(target, "sql.function.call_count", e.function_call_count);
    set(
        target,
        "sql.function.distinct_count",
        e.distinct_function_count,
    );
    set(
        target,
        "sql.function.nested_call_depth",
        e.max_function_nesting,
    );
    set(target, "sql.cast.count", e.cast_count);
}

fn publish_output(facts: &SqlFileFacts, target: &mut MetricSet) {
    let o = &facts.output;
    set(target, "sql.select.star_count", o.star_count);
    set(target, "sql.select.outer_star_count", o.outer_star_count);
    set(
        target,
        "sql.select.expression_without_alias_count",
        o.expression_without_alias_count,
    );
    let coverage = if o.derived_expression_count > 0 {
        o.aliased_derived_expression_count as f64 / o.derived_expression_count as f64
    } else {
        1.0
    };
    set(target, "sql.select.output_alias_coverage", coverage);
    let unqualified_ratio = if o.multi_relation_column_refs > 0 {
        o.multi_relation_unqualified_refs as f64 / o.multi_relation_column_refs as f64
    } else {
        0.0
    };
    set(
        target,
        "sql.identifier.unqualified_column_ratio",
        unqualified_ratio,
    );
    set(
        target,
        "sql.identifier.quoted_count",
        o.quoted_identifier_count,
    );
    set(target, "sql.alias.table_alias_count", o.table_alias_count);
    set(target, "sql.relation.ref_count", facts.relation_ref_count);
}

fn publish_objects(facts: &SqlFileFacts, target: &mut MetricSet) {
    let o = &facts.objects;
    set(target, "sql.object.read_count", o.read_object_count);
    set(target, "sql.object.write_count", o.write_object_count);
    set(target, "sql.object.touch_count", o.touch_count);
    set(target, "sql.dml.insert_count", o.insert_count);
    set(target, "sql.dml.update_count", o.update_count);
    set(target, "sql.dml.delete_count", o.delete_count);
    set(target, "sql.dml.merge_count", o.merge_count);
    set(
        target,
        "sql.dml.update_without_where_count",
        o.update_without_where_count,
    );
    set(
        target,
        "sql.dml.delete_without_where_count",
        o.delete_without_where_count,
    );
    set(target, "sql.dml.returning_count", o.returning_count);
    set(target, "sql.ddl.create_count", o.create_count);
    set(target, "sql.ddl.alter_count", o.alter_count);
    set(target, "sql.ddl.drop_count", o.drop_count);
    set(target, "sql.ddl.truncate_count", o.truncate_count);
    set(
        target,
        "sql.ddl.create_or_replace_count",
        o.create_or_replace_count,
    );
    set(target, "sql.dcl.grant_revoke_count", o.grant_revoke_count);
    set(
        target,
        "sql.transaction.control_count",
        o.transaction_control_count,
    );
}

fn publish_dialect(
    facts: &SqlFileFacts,
    _loc: &SqlLoc,
    dialect: &DialectResolution,
    target: &mut MetricSet,
) {
    let _ = facts;
    // Dialect identity is stringly-typed elsewhere; here we publish numeric
    // confidence + conflict counts that fit the f64 MetricValue shape, and a
    // 1/0 flag for whether a dialect was requested. The string labels live in
    // the analyzer's `SqlReport`/diagnostics so JSON consumers can read them.
    set(
        target,
        "sql.dialect.confidence",
        dialect.confidence as f64 / 100.0,
    );
    set(
        target,
        "sql.dialect.conflict_count",
        u32::from(dialect.conflict_count),
    );
    set(
        target,
        "sql.dialect.requested",
        dialect.requested.is_some() as i64,
    );
    // 1 when an in-file `-- sqlfluff:dialect:<name>` directive was present
    // (regardless of whether it resolved to a compiled dialect). The effective
    // dialect itself is recoverable from the one-hot `sql.dialect.is_<name>`
    // key published by `publish_dialect_labels`.
    set(
        target,
        "sql.dialect.directive_present",
        dialect.directive.is_some() as i64,
    );
    let _ = dialect_label(dialect.effective);
}

fn publish_parser(facts: &SqlFileFacts, loc: &SqlLoc, target: &mut MetricSet) {
    set(
        target,
        "sql.parser.unparsable_segment_count",
        facts.unparsable_segments,
    );
    set(
        target,
        "sql.parser.unparsable_line_count",
        facts.unparsable_lines,
    );
    // Ratio of unparsable lines to code lines. When a hard parse failure
    // leaves `loc.code == 0` (textual LOC counts non-blank lines as comments),
    // fall back to the unparsable-line count as the denominator so a totally
    // unparsable file reports a ratio of 1.0 rather than 0.0 — otherwise
    // parser-health thresholds would miss exactly the failures this surfaces.
    let denom = loc.code.max(facts.unparsable_lines);
    let ratio = if denom > 0 {
        facts.unparsable_lines as f64 / denom as f64
    } else {
        0.0
    };
    set(target, "sql.parser.unparsable_ratio", ratio.min(1.0));
    // Diagnostics surfaced by the parser: unparsable segments plus any lexer
    // errors. Lexer errors are 0 in the current sqruff release (malformed
    // input becomes unparsable segments) but are folded in so a future version
    // that emits them cannot leave `diagnostic_count` at 0 for invalid SQL.
    set(
        target,
        "sql.parser.diagnostic_count",
        facts.unparsable_segments + facts.lex_error_count,
    );
}

fn publish_halstead(facts: &SqlFileFacts, target: &mut MetricSet) {
    let h = &facts.halstead;
    let n1 = h.distinct_operators as f64;
    let n2 = h.distinct_operands as f64;
    let big_n1 = h.total_operators as f64;
    let big_n2 = h.total_operands as f64;
    let vocabulary = n1 + n2;
    let length = big_n1 + big_n2;
    let volume = if vocabulary > 0.0 {
        length * vocabulary.log2()
    } else {
        0.0
    };
    let difficulty = if n2 > 0.0 {
        (n1 / 2.0) * (big_n2 / n2)
    } else {
        0.0
    };
    let effort = difficulty * volume;
    set(
        target,
        "sql.halstead.distinct_operators",
        h.distinct_operators,
    );
    set(
        target,
        "sql.halstead.distinct_operands",
        h.distinct_operands,
    );
    set(target, "sql.halstead.total_operators", h.total_operators);
    set(target, "sql.halstead.total_operands", h.total_operands);
    set(target, "sql.halstead.vocabulary", vocabulary);
    set(target, "sql.halstead.length", length);
    set(target, "sql.halstead.volume", volume);
    set(target, "sql.halstead.difficulty", difficulty);
    set(target, "sql.halstead.effort", effort);
}

// ── composite scores (research foundation §8) ─────────────────────────

fn publish_composites(
    facts: &SqlFileFacts,
    loc: &SqlLoc,
    dialect: &DialectResolution,
    target: &mut MetricSet,
) {
    let scores = crate::composite::compute(facts, loc, dialect);
    set(
        target,
        "sql.structural_complexity",
        scores.structural_complexity,
    );
    set(
        target,
        "sql.cognitive_complexity",
        scores.cognitive_complexity,
    );
    set(target, "sql.change_risk_score", scores.change_risk_score);
    set(
        target,
        "sql.review_burden_index",
        scores.review_burden_index,
    );
    set(
        target,
        "sql.maintainability_index",
        scores.maintainability_index,
    );
    set(target, "sql.modularity_health", scores.modularity_health);
}
