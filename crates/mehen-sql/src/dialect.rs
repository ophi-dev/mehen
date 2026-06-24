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
//! 1. an in-file `-- sqlfluff:dialect:<name>` directive (SQLFluff parity —
//!    see [`parse_dialect_directive`]);
//! 2. an explicit request (CLI/config) — not yet surfaced in 1.0 since
//!    `AnalysisConfig` carries no SQL options, so this is reserved;
//! 3. syntax-hint inference from the source text;
//! 4. conservative fallback to `ansi` with low confidence.
//!
//! Inference is intentionally cheap and advisory: a few high-signal token
//! probes, never a full pre-parse. When two dialect families both match we
//! lower confidence and record the conflict rather than pick arbitrarily.

use std::str::FromStr;

use sqruff_lib_core::dialects::init::DialectKind;

/// An in-file `-- sqlfluff:dialect:<name>` directive parsed from the source.
///
/// SQLFluff lets a file pin its own dialect with a comment directive; sqruff
/// itself does not consume in-file config (and its config layer panics on
/// some inline forms), so mehen parses the directive manually and never feeds
/// it to sqruff's config path. See [`parse_dialect_directive`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DialectDirective {
    /// The raw dialect name as written after the final colon (trimmed, verbatim
    /// case). Kept for the diagnostic message.
    pub name: String,
    /// Resolution status of `name`.
    pub status: DirectiveStatus,
}

/// What happened when a directive's dialect name was resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectiveStatus {
    /// `name` resolved to a dialect that is compiled into this build; it drives
    /// `effective` with full confidence. Carries the resolved kind.
    Active(DialectKind),
    /// `name` is a real sqruff dialect (its grammar is not compiled into this
    /// build — e.g. `databricks`/`duckdb`/`trino`). Falls back to inference;
    /// surfaced as a `sql.dialect.unsupported` warning.
    Unsupported,
    /// `name` is not a recognized sqruff dialect at all. Falls back to
    /// inference; surfaced as a `sql.dialect.unknown` warning.
    Unknown,
}

/// The outcome of dialect resolution for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DialectResolution {
    /// Dialect explicitly requested by the caller (CLI/config), if any. 1.0
    /// surfaces no CLI option, so this is reserved; the in-file `directive`
    /// is a separate, higher-priority source.
    pub requested: Option<DialectKind>,
    /// The in-file `-- sqlfluff:dialect:<name>` directive, if one was present.
    pub directive: Option<DialectDirective>,
    /// Best dialect inferred from syntax hints (always set — falls back to
    /// `Ansi`).
    pub inferred: DialectKind,
    /// The dialect actually used for parsing. Priority: an active directive,
    /// then `requested`, then `inferred`.
    pub effective: DialectKind,
    /// Confidence in `effective`, scaled 0..100 (stored as an integer so the
    /// metric value is bit-exact across platforms). An active directive or a
    /// caller request is authoritative and reports 100; a directive that names
    /// an unknown/uncompiled dialect falls back to inference confidence.
    pub confidence: u8,
    /// Distinct dialect families whose hints fired. >1 signals ambiguity.
    pub conflict_count: u8,
}

/// Pure resolution without building/returning the grammar. `analyze` uses
/// [`resolve_with_dialect`] (which reuses the single grammar build); this thin
/// wrapper is convenient for tests and any caller that only needs the metric
/// surface.
#[cfg(test)]
pub(crate) fn resolve(source: &str, requested: Option<DialectKind>) -> DialectResolution {
    resolve_with_dialect(source, requested).0
}

/// Resolve the dialect for `source` and build its grammar in one step.
///
/// Priority (research foundation §11.1; SQLFluff in-file directive parity):
/// 1. an *active* in-file `-- sqlfluff:dialect:<name>` directive;
/// 2. an explicit caller request (CLI/config — reserved in 1.0);
/// 3. syntax-hint inference;
/// 4. conservative `ansi` fallback (inside `infer`).
///
/// The effective grammar is built **exactly once** here so callers (`analyze`)
/// don't rebuild it. The build doubles as the compiled-in check: an uncompiled
/// directive/request grammar comes back `None`, which downgrades a directive to
/// [`DirectiveStatus::Unsupported`] and falls the effective dialect back to
/// inference (`ansi` is always compiled, so the returned `Dialect` is never
/// absent).
pub(crate) fn resolve_with_dialect(
    source: &str,
    requested: Option<DialectKind>,
) -> (DialectResolution, sqruff_lib_core::dialects::Dialect) {
    let inference = infer(source);
    let mut directive = parse_dialect_directive(source);

    // Candidate priority: an active directive, then a caller request, then
    // inference. We build the candidate grammar once; if it is not compiled in,
    // we downgrade and retry with the next source. `ansi` (the inference floor)
    // is always compiled, so the loop terminates with a real `Dialect`.
    let directive_kind = match &directive {
        Some(DialectDirective {
            status: DirectiveStatus::Active(kind),
            ..
        }) => Some(*kind),
        _ => None,
    };

    let mut effective = directive_kind.or(requested).unwrap_or(inference.kind);
    let dialect = loop {
        if let Some(d) = dialect_for_kind(effective) {
            break d;
        }
        // `effective` is not compiled in. If it came from the directive, the
        // directive is unsupported; record that and fall back.
        if directive_kind == Some(effective)
            && let Some(dir) = directive.as_mut()
        {
            dir.status = DirectiveStatus::Unsupported;
        }
        // Fall back to a caller request if it differs and is compiled, else to
        // inference.
        effective = match requested {
            Some(req) if req != effective => req,
            _ => inference.kind,
        };
    };

    // Authority is recomputed from the *final* effective source: a pin is
    // authoritative only if `effective` still equals the (compiled) directive
    // or caller request. This correctly keeps confidence 100 when an
    // unsupported directive falls through to a compiled `requested` dialect,
    // and drops it to inference confidence when both pins were dropped.
    let directive_authoritative = directive_kind == Some(effective);
    let request_authoritative = requested == Some(effective);
    let confidence = if directive_authoritative || request_authoritative {
        100
    } else {
        inference.confidence
    };

    let resolution = DialectResolution {
        requested,
        directive,
        inferred: inference.kind,
        effective,
        confidence,
        conflict_count: inference.conflict_count,
    };
    (resolution, dialect)
}

/// Parse an in-file `-- sqlfluff:dialect:<name>` directive from `source`,
/// mirroring SQLFluff's `process_raw_file_for_config` /
/// `process_inline_config`.
///
/// Fidelity notes (verified against SQLFluff `config.rs`):
/// * The gate is on the **raw** line: it must start with `-- sqlfluff` or
///   `--sqlfluff` (no leading-whitespace trim — an indented directive is
///   ignored, exactly as SQLFluff ignores it). A leading UTF-8 BOM on the
///   first line is stripped first (Rust's `trim`/`lines` keep it otherwise).
/// * After the gate, strip the leading `--`, trim, require a `sqlfluff:`
///   prefix, then take the remainder (`dialect:<name>`). The config *key path*
///   must be exactly `dialect` (one segment): SQLFluff's
///   `split_colon_separated_string` parses `dialect:postgres:x` into the
///   two-segment key path `("dialect","postgres")` with value `x`, which is
///   **not** a dialect set — so a value that itself contains a further colon
///   (`dialect:postgres:x`, or the degenerate `dialect::`) is treated as
///   absent, not as an unknown dialect.
/// * The value is trimmed but **not** lowercased: SQLFluff dialect names are
///   case-sensitive, so `PostgreSQL` is reported as unknown (not silently
///   coerced). An empty value (`-- sqlfluff:dialect:`) is treated as no
///   directive, not an unknown dialect named "".
/// * Multiple directives → **last wins** (SQLFluff applies them in order).
/// * Only `--` line comments are honored; block comments
///   (`/* sqlfluff:... */`) and `#` comments are not (SQLFluff ignores them).
/// * Like SQLFluff, this is a raw-line scan, so a `-- sqlfluff:` pattern inside
///   a multi-line string literal would also match; this matches SQLFluff's own
///   behavior and is acceptable for an advisory directive.
pub(crate) fn parse_dialect_directive(source: &str) -> Option<DialectDirective> {
    let mut last: Option<String> = None;
    for (i, raw_line) in source.lines().enumerate() {
        // Strip a leading BOM only on the first physical line.
        let line = if i == 0 {
            raw_line.trim_start_matches('\u{feff}')
        } else {
            raw_line
        };
        // Gate on the raw (un-left-trimmed) line, like SQLFluff.
        if !(line.starts_with("-- sqlfluff") || line.starts_with("--sqlfluff")) {
            continue;
        }
        // Strip the `--`, trim, require the `sqlfluff:` prefix.
        let after_dashes = line[2..].trim();
        let Some(rest) = after_dashes.strip_prefix("sqlfluff:") else {
            continue;
        };
        // `rest` is e.g. `dialect:postgres`. Split on the FIRST colon into
        // (key, value); only a single-segment `dialect` key is a dialect set.
        let rest = rest.trim();
        let Some((key, value)) = rest.split_once(':') else {
            continue; // `-- sqlfluff:dialect` (no value) — not a dialect set.
        };
        if key.trim() != "dialect" {
            continue;
        }
        let value = value.trim();
        // A further colon means SQLFluff would parse a multi-segment key path
        // (`dialect:postgres:x` → key `("dialect","postgres")`), which is not a
        // dialect set. The degenerate `dialect::` (value `":"`) lands here too.
        if value.is_empty() || value.contains(':') {
            continue;
        }
        last = Some(value.to_string());
    }

    last.map(|name| {
        // Case-sensitive, matching SQLFluff: `DialectKind` derives strum
        // `EnumString` with snake_case, so `from_str` accepts every dialect
        // name verbatim. Compiled-vs-uncompiled (`Active` vs `Unsupported`) is
        // decided later from the single grammar build, so this does not build.
        let status = match DialectKind::from_str(&name) {
            Ok(kind) => DirectiveStatus::Active(kind),
            Err(_) => DirectiveStatus::Unknown,
        };
        DialectDirective { name, status }
    })
}

/// Build the sqruff grammar for `kind`, or `None` if that dialect is not
/// compiled into this build (`kind_to_dialect` returns `None` for an
/// uncompiled dialect — it never panics). This is the authoritative
/// compiled-in check **and** the single grammar build per effective dialect.
pub(crate) fn dialect_for_kind(kind: DialectKind) -> Option<sqruff_lib_core::dialects::Dialect> {
    sqruff_lib_dialects::kind_to_dialect(&kind, None)
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
        // `RETURNING` followed by any whitespace (so `RETURNING\nid` is matched,
        // not just `RETURNING id`).
        has("RETURNING ") || has("RETURNING\n") || has("RETURNING\t") || has("RETURNING\r"),
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

    // ── in-file directive (`-- sqlfluff:dialect:<name>`) ──────────────────

    fn directive(source: &str) -> Option<DialectDirective> {
        parse_dialect_directive(source)
    }

    #[test]
    fn directive_active_compiled_dialect_drives_effective() {
        let r = resolve("-- sqlfluff:dialect:postgres\nSELECT 1", None);
        assert_eq!(r.effective, DialectKind::Postgres);
        assert_eq!(r.confidence, 100);
        assert!(matches!(
            r.directive.unwrap().status,
            DirectiveStatus::Active(DialectKind::Postgres)
        ));
    }

    #[test]
    fn directive_accepts_both_dash_forms_and_colon_whitespace() {
        // `--sqlfluff` (no space) and whitespace around the value both work.
        assert!(matches!(
            directive("--sqlfluff:dialect:tsql\nSELECT 1")
                .unwrap()
                .status,
            DirectiveStatus::Active(DialectKind::Tsql)
        ));
        assert!(matches!(
            directive("-- sqlfluff:dialect:  mysql  \nSELECT 1")
                .unwrap()
                .status,
            DirectiveStatus::Active(DialectKind::Mysql)
        ));
    }

    #[test]
    fn directive_recognized_but_uncompiled_is_unsupported_and_does_not_pin() {
        // `duckdb` is a real sqruff dialect but not compiled into this build.
        let r = resolve("-- sqlfluff:dialect:duckdb\nSELECT 1", None);
        assert_eq!(
            r.directive.as_ref().unwrap().status,
            DirectiveStatus::Unsupported
        );
        // Falls back to inference, NOT confidence 100.
        assert_eq!(r.effective, r.inferred);
        assert_ne!(r.confidence, 100);
    }

    #[test]
    fn directive_unknown_name_is_unknown_and_falls_back() {
        let r = resolve("-- sqlfluff:dialect:nope\nSELECT 1", None);
        assert_eq!(
            r.directive.as_ref().unwrap().status,
            DirectiveStatus::Unknown
        );
        assert_eq!(r.effective, r.inferred);
        assert_ne!(r.confidence, 100);
    }

    #[test]
    fn directive_is_case_sensitive_like_sqlfluff() {
        // SQLFluff dialect names are case-sensitive; `Postgres` is unknown.
        let r = resolve("-- sqlfluff:dialect:Postgres\nSELECT 1", None);
        assert_eq!(r.directive.unwrap().status, DirectiveStatus::Unknown);
    }

    #[test]
    fn directive_last_one_wins() {
        let r = resolve(
            "-- sqlfluff:dialect:mysql\n-- sqlfluff:dialect:postgres\nSELECT 1",
            None,
        );
        assert_eq!(r.effective, DialectKind::Postgres);
    }

    #[test]
    fn directive_ignores_block_comments_and_hash_comments() {
        assert!(directive("/* sqlfluff:dialect:postgres */\nSELECT 1").is_none());
        assert!(directive("# sqlfluff:dialect:postgres\nSELECT 1").is_none());
    }

    #[test]
    fn directive_ignores_indented_and_empty_and_keyless_forms() {
        // Indented directive: SQLFluff gates on the raw (un-trimmed) line.
        assert!(directive("    -- sqlfluff:dialect:postgres\nSELECT 1").is_none());
        // Empty value is treated as no directive, not unknown-dialect "".
        assert!(directive("-- sqlfluff:dialect:\nSELECT 1").is_none());
        // Missing the value colon entirely.
        assert!(directive("-- sqlfluff:dialect\nSELECT 1").is_none());
        // A non-dialect sqlfluff key is not a dialect directive.
        assert!(directive("-- sqlfluff:rules:LT01\nSELECT 1").is_none());
    }

    #[test]
    fn directive_multi_segment_key_is_not_a_dialect_set() {
        // SQLFluff parses `dialect:postgres:x` into the two-segment key path
        // ("dialect","postgres") with value "x" — NOT a dialect set. mehen must
        // treat it as absent (no spurious `unknown` warning), not pin/reject.
        assert!(directive("-- sqlfluff:dialect:postgres:x\nSELECT 1").is_none());
        // The degenerate double-trailing-colon is likewise absent, not a
        // directive named ":".
        assert!(directive("-- sqlfluff:dialect::\nSELECT 1").is_none());
    }

    #[test]
    fn requested_uncompiled_dialect_is_not_authoritative() {
        // A caller request for an uncompiled dialect must not report confidence
        // 100 while parsing silently falls back — mirror the directive gate.
        // (`Databricks` is recognized by sqruff but not compiled into mehen.)
        let r = resolve("SELECT 1", Some(DialectKind::Databricks));
        assert_ne!(r.effective, DialectKind::Databricks);
        assert_ne!(r.confidence, 100);
        // A compiled request stays authoritative.
        let ok = resolve("SELECT 1", Some(DialectKind::Postgres));
        assert_eq!(ok.effective, DialectKind::Postgres);
        assert_eq!(ok.confidence, 100);
    }

    #[test]
    fn unsupported_directive_falls_through_to_compiled_request_authoritatively() {
        // An unsupported directive (`duckdb`) falling through to a *compiled*
        // caller request must keep confidence 100 — authority is recomputed
        // from the final effective source, not cleared on the first fallback.
        let r = resolve(
            "-- sqlfluff:dialect:duckdb\nSELECT 1",
            Some(DialectKind::Postgres),
        );
        assert_eq!(r.effective, DialectKind::Postgres);
        assert_eq!(r.confidence, 100);
        assert_eq!(r.directive.unwrap().status, DirectiveStatus::Unsupported);
    }

    #[test]
    fn directive_trailing_content_is_part_of_the_value_and_unknown() {
        // SQLFluff does NOT strip a trailing inline comment; the value becomes
        // `postgres -- x`, which is not a known dialect.
        let r = resolve("-- sqlfluff:dialect:postgres -- x\nSELECT 1", None);
        assert_eq!(r.directive.unwrap().status, DirectiveStatus::Unknown);
    }

    #[test]
    fn directive_handles_crlf_and_leading_bom() {
        // CRLF: `.lines()` strips the trailing `\r`, so the value is clean.
        let r = resolve("-- sqlfluff:dialect:postgres\r\nSELECT 1\r\n", None);
        assert_eq!(r.effective, DialectKind::Postgres);
        // UTF-8 BOM on the first line must not block the prefix gate.
        let r = resolve("\u{feff}-- sqlfluff:dialect:sqlite\nSELECT 1", None);
        assert_eq!(r.effective, DialectKind::Sqlite);
    }

    #[test]
    fn directive_every_recognized_name_round_trips_via_from_str() {
        // Each compiled dialect's snake_case label is a valid directive value
        // that resolves to Active(kind) — guards the from_str/label contract.
        for (name, kind) in [
            ("ansi", DialectKind::Ansi),
            ("postgres", DialectKind::Postgres),
            ("tsql", DialectKind::Tsql),
            ("snowflake", DialectKind::Snowflake),
            ("bigquery", DialectKind::Bigquery),
            ("mysql", DialectKind::Mysql),
            ("sqlite", DialectKind::Sqlite),
            ("oracle", DialectKind::Oracle),
            ("clickhouse", DialectKind::Clickhouse),
            ("redshift", DialectKind::Redshift),
            ("sparksql", DialectKind::Sparksql),
            ("athena", DialectKind::Athena),
            ("db2", DialectKind::Db2),
        ] {
            let src = format!("-- sqlfluff:dialect:{name}\nSELECT 1");
            let r = resolve(&src, None);
            assert_eq!(
                r.directive.unwrap().status,
                DirectiveStatus::Active(kind),
                "name {name} should be active {kind:?}",
            );
        }
    }
}
