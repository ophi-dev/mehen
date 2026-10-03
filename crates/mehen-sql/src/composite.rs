// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Composite scores (research foundation §8).
//!
//! Each score is a weighted, *explainable* combination of raw metrics. The
//! weights here mirror the research foundation's published formulas
//! (§8.1–§8.6) so the numbers can be cross-checked against the design doc.
//! Composites are review-prioritization signals, not absolute quality
//! judgments — the analyzer publishes the raw metrics first and these second.

use crate::dialect::DialectResolution;
use crate::facts::{ChangeRiskFactor, SqlFileFacts};
use crate::loc::SqlLoc;

/// The six composite scores published per file.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompositeScores {
    pub structural_complexity: f64,
    pub cognitive_complexity: f64,
    pub change_risk_score: f64,
    pub review_burden_index: f64,
    pub maintainability_index: f64,
    pub modularity_health: f64,
}

/// `norm(x, t) = x / (x + t)` — the bounded normalizer used by the index
/// formulas (research foundation §8.3). Saturates toward 1 as `x` grows.
fn norm(x: f64, t: f64) -> f64 {
    if x <= 0.0 { 0.0 } else { x / (x + t) }
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

pub(crate) fn compute(
    facts: &SqlFileFacts,
    loc: &SqlLoc,
    dialect: &DialectResolution,
) -> CompositeScores {
    let structural_complexity = structural(facts);
    let cognitive_complexity = cognitive(facts);
    let change_risk_score = change_risk(facts);
    let halstead_volume = halstead_volume(facts);
    // The review-burden / maintainability formulas (§8.3/§8.5) weight a
    // `sql.dialect.portability_risk_count` (count of features outside the
    // ANSI/core profile). Phase 1 does not enumerate dialect-specific
    // features, so we use the dialect-inference conflict count — how many
    // dialect families' syntax hints fired — as a conservative proxy: a file
    // that trips several dialects' markers is, by construction, less portable.
    // Both terms carry only a 0.05 weight, so the approximation is bounded.
    let portability_risk = dialect.conflict_count as f64;

    let review_burden_index = review_burden(
        cognitive_complexity,
        structural_complexity,
        facts,
        change_risk_score,
        halstead_volume,
        portability_risk,
        loc,
    );
    let maintainability_index = maintainability(
        halstead_volume,
        cognitive_complexity,
        structural_complexity,
        facts,
        portability_risk,
    );
    let modularity_health = modularity(facts);

    CompositeScores {
        structural_complexity,
        cognitive_complexity,
        change_risk_score,
        review_burden_index,
        maintainability_index,
        modularity_health,
    }
}

/// SQL Structural Complexity (research foundation §8.1). Also reused by the
/// procedural module to score the query constructs embedded in one routine
/// (`sql.structural_complexity.max_embedded_query`, §9.3).
pub(crate) fn structural(f: &SqlFileFacts) -> f64 {
    1.00 * f.query_block_count as f64
        + 0.80 * f.ctes.count as f64
        + 1.20 * f.ctes.max_dependency_depth as f64
        + 1.00 * f.joins.total as f64
        + 0.80 * (f.joins.left + f.joins.right + f.joins.full) as f64
        + 2.00 * f.joins.cross as f64
        + 1.50 * f.subqueries.count as f64
        + 1.25 * f.subqueries.max_depth as f64
        + 2.00 * f.subqueries.correlated_count as f64
        + 0.35 * f.predicates.boolean_operator_count as f64
        + 1.00 * f.predicates.max_boolean_depth as f64
        + 0.80 * f.cases.count as f64
        + 0.80 * f.cases.max_depth as f64
        + 0.60 * f.windows.function_count as f64
        + 0.35 * f.aggregates.function_count as f64
        + 1.00 * f.set_ops.count as f64
        + 0.50 * f.expressions.max_depth as f64
        + 0.50 * f.subqueries.derived_table_count as f64
}

/// SQL Cognitive Complexity (research foundation §8.2).
///
/// Mirrors the spirit of code cognitive complexity but uses SQL-specific
/// mental contexts: each query block, nested-scope penalty, correlated
/// subquery, CTE edge, join, CASE level, boolean nesting, window, set op,
/// unaliased expression, and outer wildcard — minus a small modularization
/// credit for shallow, well-used CTEs.
fn cognitive(f: &SqlFileFacts) -> f64 {
    let mut score = 0.0f64;
    // 1. query blocks
    score += f.query_block_count as f64;
    // 2. subquery nesting depth
    score += f.subqueries.max_depth as f64;
    // 3. correlated subqueries
    score += 2.0 * f.subqueries.correlated_count as f64;
    // 4. CTEs + dependency edges beyond the first
    score += f.ctes.count as f64;
    score += f.ctes.dependency_edges.saturating_sub(1) as f64;
    // 5. joins + extra weight for outer/cross/natural/lateral
    score += f.joins.total as f64;
    score += (f.joins.left
        + f.joins.right
        + f.joins.full
        + f.joins.cross
        + f.joins.natural
        + f.joins.lateral) as f64;
    // 6. CASE: +1 each, +1 per nesting level, +0.25 per WHEN beyond 2. The
    //    surplus-arm count is computed per-CASE in `extract_cases` so a
    //    many-armed CASE is not cancelled by single-armed ones.
    score += f.cases.count as f64;
    score += f.cases.max_depth.saturating_sub(1) as f64;
    score += 0.25 * f.cases.surplus_when_arms as f64;
    // 7. boolean operators + nesting beyond 2.
    //    §8.2 rule 7 also adds "+1 for mixed AND/OR chains without explicit
    //    grouping"; that term is omitted here because
    //    `sql.predicate.mixed_and_or_without_grouping_count` is not tracked in
    //    Phase 1 (would require precedence-aware predicate analysis).
    score += 0.25 * f.predicates.boolean_operator_count as f64;
    score += f.predicates.max_boolean_depth.saturating_sub(2) as f64;
    // 8. window functions + frames
    score += 0.5 * f.windows.function_count as f64;
    score += f.windows.frame_count as f64;
    // 9. set operations.
    //    §8.2 rule 9 also adds "+1 for nested set expressions"; that term is
    //    omitted because nested set-expression depth is not tracked in Phase 1.
    score += 0.5 * f.set_ops.count as f64;
    // 10. unaliased derived expressions
    score += 0.25 * f.output.expression_without_alias_count as f64;
    // 11. outer wildcards
    score += 0.5 * f.output.outer_star_count as f64;
    // 12. modularization credit, capped at -5, for shallow well-used CTEs
    let credit = modularization_credit(f);
    (score - credit).max(0.0)
}

/// Small credit for CTEs that reduce nesting and have shallow dependency
/// depth (research foundation §8.2 rule 12).
fn modularization_credit(f: &SqlFileFacts) -> f64 {
    if f.ctes.count == 0 {
        return 0.0;
    }
    let used = f.ctes.count.saturating_sub(f.ctes.unused_count);
    let shallow = f.ctes.max_dependency_depth <= 3;
    let credit = if shallow {
        used as f64 * 0.75
    } else {
        used as f64 * 0.25
    };
    credit.min(5.0)
}

/// SQL Change Risk Score (research foundation §8.4).
///
/// Every term matches the spec weights exactly, including the
/// `+ 5 × dynamic_sql_count` term (Phase 3 — `EXECUTE IMMEDIATE`,
/// `sp_executesql`, `EXEC('…')`, `DBMS_SQL`).
fn change_risk(f: &SqlFileFacts) -> f64 {
    let o = &f.objects;
    ChangeRiskFactor::Drop.amount() * o.drop_count as f64
        + ChangeRiskFactor::Truncate.amount() * o.truncate_count as f64
        + ChangeRiskFactor::Alter.amount() * o.alter_count as f64
        + ChangeRiskFactor::DeleteWithoutWhere.amount() * o.delete_without_where_count as f64
        + ChangeRiskFactor::UpdateWithoutWhere.amount() * o.update_without_where_count as f64
        + ChangeRiskFactor::GrantRevoke.amount() * o.grant_revoke_count as f64
        + ChangeRiskFactor::DynamicSql.amount() * f.procedural.dynamic_sql_count as f64
        + ChangeRiskFactor::Merge.amount() * o.merge_count as f64
        + ChangeRiskFactor::CreateOrReplace.amount() * o.create_or_replace_count as f64
        + ChangeRiskFactor::TransactionControl.amount() * o.transaction_control_count as f64
        + ChangeRiskFactor::WriteObject.amount() * o.write_object_count as f64
        + ChangeRiskFactor::ReadObject.amount() * o.read_object_count as f64
}

/// SQL Review Burden Index (research foundation §8.3), 0..100.
#[allow(clippy::too_many_arguments)]
fn review_burden(
    cognitive: f64,
    structural: f64,
    f: &SqlFileFacts,
    change_risk: f64,
    halstead_volume: f64,
    portability_risk: f64,
    loc: &SqlLoc,
) -> f64 {
    // §8.3 weights `norm(sql.object.touch_count, 20)` — the count of *distinct*
    // objects touched (read ∪ write), not the sum of the two counters (which
    // would double-count an object that is both read and written).
    let touch = f.objects.touch_count as f64;
    let raw = 0.30 * norm(cognitive, 60.0)
        + 0.18 * norm(structural, 80.0)
        + 0.14 * norm(touch, 20.0)
        + 0.12 * norm(change_risk, 25.0)
        + 0.10 * norm(halstead_volume, 1500.0)
        + 0.08 * norm(f.unparsable_segments as f64, 5.0)
        + 0.05 * norm(portability_risk, 20.0)
        + 0.05 * norm(loc.code as f64, 300.0)
        - 0.02 * clamp01(loc.comment_density() / 0.20);
    100.0 * clamp01(raw)
}

/// SQL Maintainability Index (research foundation §8.5), 0..100, higher is
/// better.
fn maintainability(
    halstead_volume: f64,
    cognitive: f64,
    structural: f64,
    f: &SqlFileFacts,
    portability_risk: f64,
) -> f64 {
    let risk = 0.22 * norm(halstead_volume, 1500.0)
        + 0.22 * norm(cognitive, 60.0)
        + 0.16 * norm(structural, 80.0)
        + 0.12 * norm(f.predicates.boolean_operator_count as f64, 30.0)
        + 0.10 * norm(f.ctes.max_dependency_depth as f64, 6.0)
        + 0.08 * norm(f.subqueries.max_depth as f64, 4.0)
        + 0.05 * norm(f.unparsable_segments as f64, 5.0)
        + 0.05 * norm(portability_risk, 20.0);
    100.0 * clamp01(1.0 - risk)
}

/// SQL Modularity Health (research foundation §8.6), 0..100. Only meaningful
/// for query-like files with CTEs/subqueries; returns 0 when no CTEs exist
/// (callers should treat that as N/A).
fn modularity(f: &SqlFileFacts) -> f64 {
    if f.ctes.count == 0 {
        return 0.0;
    }
    let cte_count = f.ctes.count as f64;
    let used = f.ctes.count.saturating_sub(f.ctes.unused_count) as f64;
    let cte_use_ratio = used / cte_count.max(1.0);
    let cte_shallow_score = 1.0 - norm(f.ctes.max_dependency_depth as f64, 6.0);
    let cte_fanout_score = 1.0 - norm(f.ctes.max_fan_out as f64, 8.0);
    let derived_table_penalty = norm(f.subqueries.derived_table_count as f64, 5.0);
    let trivial_cte_penalty = f.ctes.trivial_count as f64 / cte_count.max(1.0);

    let health = 0.35 * cte_use_ratio
        + 0.25 * cte_shallow_score
        + 0.15 * cte_fanout_score
        + 0.15 * (1.0 - derived_table_penalty)
        + 0.10 * (1.0 - trivial_cte_penalty);
    100.0 * clamp01(health)
}

/// Recompute Halstead volume from facts (kept here so composites don't depend
/// on metric publishing order). Mirrors `metrics::publish_halstead`.
fn halstead_volume(f: &SqlFileFacts) -> f64 {
    let h = &f.halstead;
    let vocabulary = (h.distinct_operators + h.distinct_operands) as f64;
    let length = (h.total_operators + h.total_operands) as f64;
    if vocabulary > 0.0 {
        length * vocabulary.log2()
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_saturates() {
        assert_eq!(norm(0.0, 10.0), 0.0);
        assert!((norm(10.0, 10.0) - 0.5).abs() < 1e-9);
        assert!(norm(1000.0, 10.0) > 0.9);
    }

    #[test]
    fn empty_facts_score_zero() {
        let f = SqlFileFacts::default();
        let loc = SqlLoc::default();
        let d = DialectResolution {
            requested: None,
            directive: None,
            inferred: sqruff_lib_core::dialects::init::DialectKind::Ansi,
            effective: sqruff_lib_core::dialects::init::DialectKind::Ansi,
            confidence: 30,
            conflict_count: 0,
        };
        let s = compute(&f, &loc, &d);
        assert_eq!(s.structural_complexity, 0.0);
        assert_eq!(s.cognitive_complexity, 0.0);
        assert_eq!(s.change_risk_score, 0.0);
        // No CTEs → modularity is N/A (0).
        assert_eq!(s.modularity_health, 0.0);
        // No risk → maintainability is at its max.
        assert_eq!(s.maintainability_index, 100.0);
    }
}
