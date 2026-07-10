// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use mehen_core::{
    AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, MetricKey, SourceFile,
};
use mehen_sql::SqlAnalyzer;

fn analyze(sql: &str, config: &AnalysisConfig) -> LanguageAnalysis {
    SqlAnalyzer::new()
        .analyze(
            &SourceFile::new("risk.sql".into(), Language::Sql, sql.to_string()),
            config,
        )
        .expect("SQL analysis succeeds")
}

fn metric(analysis: &LanguageAnalysis, key: &str) -> f64 {
    analysis
        .root
        .metrics
        .get(&MetricKey::new(key))
        .unwrap_or_else(|| panic!("missing metric {key}"))
        .as_f64()
}

fn assert_risk_sum(analysis: &LanguageAnalysis) {
    let sum: f64 = analysis
        .contributions
        .iter()
        .filter(|item| item.metric.as_str() == "sql.change_risk_score")
        .map(|item| item.amount)
        .sum();
    assert_eq!(sum, metric(analysis, "sql.change_risk_score"));
}

#[test]
fn change_risk_contributions_are_weighted_spanned_and_complete() {
    let sql = "DROP TABLE t;\nTRUNCATE TABLE s;\nDELETE FROM u;";
    let analysis = analyze(sql, &AnalysisConfig::production());
    let contributions = &analysis.contributions;

    assert!(!contributions.is_empty());
    assert!(contributions.iter().all(|item| {
        item.metric.as_str() == "sql.change_risk_score"
            && item.span.start_byte <= item.span.end_byte
            && item.span.end_byte as usize <= sql.len()
            && item.span.start_line >= 1
            && item.span.start_line <= item.span.end_line
    }));

    assert_risk_sum(&analysis);

    let reasons: Vec<_> = contributions
        .iter()
        .map(|item| item.reason.as_str())
        .collect();
    assert!(reasons.contains(&"sql.change_risk.drop"));
    assert!(reasons.contains(&"sql.change_risk.truncate"));
    assert!(reasons.contains(&"sql.change_risk.delete_without_where"));
    assert_eq!(
        reasons
            .iter()
            .filter(|reason| **reason == "sql.change_risk.write_object")
            .count(),
        3
    );

    // The collector's public ordering contract is source order.
    assert!(contributions.windows(2).all(|pair| {
        (pair[0].span.start_byte, pair[0].span.end_byte)
            <= (pair[1].span.start_byte, pair[1].span.end_byte)
    }));
}

#[test]
fn distinct_object_evidence_uses_the_first_occurrence() {
    let sql = "SELECT * FROM t;\nSELECT id FROM t;";
    let analysis = analyze(sql, &AnalysisConfig::production());
    let reads: Vec<_> = analysis
        .contributions
        .iter()
        .filter(|item| item.reason.as_str() == "sql.change_risk.read_object")
        .collect();

    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].amount, 1.0);
    assert_eq!(reads[0].span.start_line, 1);
    assert_eq!(metric(&analysis, "sql.object.read_count"), 1.0);
    assert_eq!(metric(&analysis, "sql.change_risk_score"), 1.0);
}

#[test]
fn benchmark_profile_skips_evidence_without_changing_metrics() {
    let sql = "UPDATE accounts SET enabled = FALSE";
    let production = analyze(sql, &AnalysisConfig::production());
    let benchmark = analyze(sql, &AnalysisConfig::benchmark());

    assert!(!production.contributions.is_empty());
    assert!(benchmark.contributions.is_empty());
    assert_eq!(
        metric(&production, "sql.change_risk_score"),
        metric(&benchmark, "sql.change_risk_score")
    );
}

#[test]
fn every_implemented_change_risk_term_has_a_stable_reason() {
    let cases = [
        (
            "ALTER TABLE customers ADD COLUMN active BOOLEAN",
            "sql.change_risk.alter",
        ),
        (
            "UPDATE accounts SET enabled = FALSE",
            "sql.change_risk.update_without_where",
        ),
        (
            "GRANT SELECT ON customers TO reporting_role",
            "sql.change_risk.grant_revoke",
        ),
        (
            "MERGE INTO accounts a USING updates u ON a.id = u.id WHEN MATCHED THEN UPDATE SET a.balance = u.balance",
            "sql.change_risk.merge",
        ),
        (
            "CREATE OR REPLACE VIEW active_accounts AS SELECT * FROM accounts",
            "sql.change_risk.create_or_replace",
        ),
        ("BEGIN; COMMIT;", "sql.change_risk.transaction_control"),
    ];

    for (sql, expected_reason) in cases {
        let analysis = analyze(sql, &AnalysisConfig::production());
        assert!(
            analysis
                .contributions
                .iter()
                .any(|item| item.reason.as_str() == expected_reason),
            "missing {expected_reason} for {sql:?}: {:?}",
            analysis.contributions
        );
        assert_risk_sum(&analysis);
    }
}
