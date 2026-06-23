// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Dialect selection and conservative inference.
//!
//! A `.sql` suffix is ambiguous (research foundation §4.3): SonarQube treats
//! `.sql` as PL/SQL and `.tsql` as T-SQL, which silently misclassifies most
//! files. mehen instead resolves a dialect explicitly and exposes a
//! confidence so callers can tell a guessed dialect from a configured one.
//!
//! Resolution priority (research foundation §11.1):
//! 1. an explicit request (CLI/config) — not yet surfaced in 1.0 since
//!    `AnalysisConfig` carries no SQL options, so this is reserved;
//! 2. syntax-hint inference from the source text;
//! 3. conservative fallback to `ansi` with low confidence.
//!
//! Inference is intentionally cheap and advisory: a few high-signal token
//! probes, never a full pre-parse. When two dialect families both match we
//! lower confidence and record the conflict rather than pick arbitrarily.

use sqruff_lib_core::dialects::init::DialectKind;

/// The outcome of dialect resolution for one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DialectResolution {
    /// Dialect explicitly requested by the caller, if any.
    pub requested: Option<DialectKind>,
    /// Best dialect inferred from syntax hints (always set — falls back to
    /// `Ansi`).
    pub inferred: DialectKind,
    /// The dialect actually used for parsing (`requested` if present, else
    /// `inferred`).
    pub effective: DialectKind,
    /// Confidence in `inferred`, scaled 0..100 (stored as an integer so the
    /// metric value is bit-exact across platforms). A requested dialect is
    /// authoritative and reports 100.
    pub confidence: u8,
    /// Distinct dialect families whose hints fired. >1 signals ambiguity.
    pub conflict_count: u8,
}

/// Resolve the dialect for `source`, honoring an optional explicit request.
pub(crate) fn resolve(source: &str, requested: Option<DialectKind>) -> DialectResolution {
    let inference = infer(source);
    let effective = requested.unwrap_or(inference.kind);
    let confidence = if requested.is_some() {
        100
    } else {
        inference.confidence
    };
    DialectResolution {
        requested,
        inferred: inference.kind,
        effective,
        confidence,
        conflict_count: inference.conflict_count,
    }
}

struct Inference {
    kind: DialectKind,
    confidence: u8,
    conflict_count: u8,
}

/// Map of dialect family → a count of how many of its hint tokens fired.
/// Inference picks the family with the most hits; ties or a single weak hit
/// keep confidence low and fall back to `ansi`.
fn infer(source: &str) -> Inference {
    let upper = source.to_ascii_uppercase();
    // Word-boundary-ish containment: wrap the haystack in spaces and search
    // for ` TOKEN ` / ` TOKEN(` is overkill for advisory hints, so we use
    // plain `contains` on the uppercased text. False positives inside string
    // literals or identifiers only nudge confidence; they never override an
    // explicit request, and the cost of being wrong is a low-confidence
    // advisory number, not a misparse (we still parse with the resolved
    // dialect grammar).
    let has = |needle: &str| upper.contains(needle);

    // Each family lists high-signal hints (research foundation §11.2).
    let tsql = count(&[
        has("\nGO\n") || upper.starts_with("GO\n") || upper.ends_with("\nGO"),
        has("CROSS APPLY") || has("OUTER APPLY"),
        has("[") && has("]"), // bracket-quoted identifiers
        has("ISNULL(") || has("NVARCHAR") || has("DATETIME2"),
        has("SELECT TOP ") || has("SELECT TOP("),
    ]);
    let snowflake = count(&[
        has("QUALIFY "),
        has("IFF("),
        has("COPY INTO "),
        has(":: VARIANT") || has("::VARIANT") || has(" VARIANT"),
        has("LATERAL FLATTEN"),
    ]);
    let postgres = count(&[
        has("::"),
        has("DISTINCT ON"),
        has(" ILIKE "),
        has("RETURNING "),
        has("ARRAY["),
    ]);
    let bigquery = count(&[
        has("`"), // backtick identifiers
        has(" STRUCT<") || has(" STRUCT("),
        has("UNNEST("),
        has("ARRAY<"),
        has("SAFE_CAST("),
    ]);
    let oracle = count(&[
        has("CONNECT BY"),
        has(" MINUS "),
        has("NVL(") || has("NVL2("),
        has(" DUAL"),
        has("VARCHAR2"),
    ]);
    let mysql = count(&[
        has("ENGINE=") || has("ENGINE ="),
        has("AUTO_INCREMENT"),
        has("`") && has("ENGINE"),
        has("UNSIGNED"),
        has("LIMIT ") && has("OFFSET "),
    ]);

    let scored = [
        (DialectKind::Tsql, tsql),
        (DialectKind::Snowflake, snowflake),
        (DialectKind::Postgres, postgres),
        (DialectKind::Bigquery, bigquery),
        (DialectKind::Oracle, oracle),
        (DialectKind::Mysql, mysql),
    ];

    let conflict_count = scored.iter().filter(|(_, n)| *n > 0).count() as u8;
    let best = scored
        .iter()
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n);

    match best {
        // A single dominant family with ≥2 hits is a confident guess. One hit
        // is weak; we still pick the family but cap confidence low so callers
        // treat it as a hint, not a decision.
        Some((kind, hits)) => {
            let runner_up_tie = scored
                .iter()
                .filter(|(k, n)| *k != *kind && *n == *hits)
                .count()
                > 0;
            let confidence = if runner_up_tie {
                40
            } else if *hits >= 3 {
                90
            } else if *hits == 2 {
                70
            } else {
                45
            };
            Inference {
                kind: *kind,
                confidence,
                conflict_count,
            }
        }
        // No hints at all: ANSI is the safe, dialect-neutral default. Confidence
        // is deliberately low because "looks like generic SQL" is itself a weak
        // signal, not a guarantee the file is ANSI-only.
        None => Inference {
            kind: DialectKind::Ansi,
            confidence: 30,
            conflict_count: 0,
        },
    }
}

fn count(flags: &[bool]) -> u32 {
    flags.iter().filter(|b| **b).count() as u32
}

/// Stable lowercase label for a dialect (matches sqruff's snake_case names).
pub(crate) fn dialect_label(kind: DialectKind) -> &'static str {
    match kind {
        DialectKind::Ansi => "ansi",
        DialectKind::Athena => "athena",
        DialectKind::Bigquery => "bigquery",
        DialectKind::Clickhouse => "clickhouse",
        DialectKind::Databricks => "databricks",
        DialectKind::Db2 => "db2",
        DialectKind::Duckdb => "duckdb",
        DialectKind::Mysql => "mysql",
        DialectKind::Oracle => "oracle",
        DialectKind::Postgres => "postgres",
        DialectKind::Redshift => "redshift",
        DialectKind::Snowflake => "snowflake",
        DialectKind::Sparksql => "sparksql",
        DialectKind::Sqlite => "sqlite",
        DialectKind::Trino => "trino",
        DialectKind::Tsql => "tsql",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_tsql_from_apply_and_brackets() {
        let r = resolve(
            "SELECT TOP 10 * FROM [dbo].[t] CROSS APPLY fn(t.id) AS x",
            None,
        );
        assert_eq!(r.inferred, DialectKind::Tsql);
        assert!(r.confidence >= 70, "confidence was {}", r.confidence);
    }

    #[test]
    fn infers_snowflake_from_qualify() {
        let r = resolve(
            "SELECT a, IFF(b > 0, 1, 0) FROM t QUALIFY ROW_NUMBER() OVER (ORDER BY a) = 1",
            None,
        );
        assert_eq!(r.inferred, DialectKind::Snowflake);
    }

    #[test]
    fn requested_dialect_is_authoritative() {
        let r = resolve("SELECT 1", Some(DialectKind::Postgres));
        assert_eq!(r.effective, DialectKind::Postgres);
        assert_eq!(r.confidence, 100);
    }

    #[test]
    fn plain_sql_falls_back_to_ansi_low_confidence() {
        let r = resolve("SELECT a, b FROM t WHERE a = 1", None);
        assert_eq!(r.inferred, DialectKind::Ansi);
        assert!(r.confidence <= 45);
    }
}
