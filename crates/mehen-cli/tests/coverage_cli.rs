// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! End-to-end CLI tests for the `coverage.*` category: `--coverage`
//! flag semantics on `mehen metrics` and `mehen top-offenders`,
//! auto-discovery inside gitignored directories, `[coverage]` config
//! handling, and coverage thresholds.

use std::process::Command;

const PYTHON_BODY: &str = "def hit(flag):\n    if flag:\n        return 1\n    return 2\n\n\ndef missed():\n    return 3\n";

/// LCOV describing `app.py`: `hit` executed (lines 1–4, line 3 missed),
/// `missed` never executed. 4 of 6 instrumented lines hit; one of two
/// branch arms taken; one of two functions executed.
const LCOV: &str = "TN:\nSF:app.py\nFN:1,hit\nFN:7,missed\nFNDA:5,hit\nFNDA:0,missed\nDA:1,5\nDA:2,5\nDA:3,0\nDA:4,5\nDA:7,1\nDA:8,0\nBRDA:2,0,0,5\nBRDA:2,0,1,0\nend_of_record\n";

fn write(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn mehen(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run mehen")
}

fn json_stdout(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "mehen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is JSON")
}

#[test]
fn metrics_with_explicit_coverage_report_publishes_the_family() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.py", PYTHON_BODY);
    write(dir.path(), "lcov.info", LCOV);

    let output = mehen(dir.path(), &["metrics", "app.py", "--coverage=lcov.info"]);
    let json = json_stdout(&output);

    let coverage = &json["metrics"]["coverage"];
    assert_eq!(coverage["line"]["covered"], 4, "{coverage}");
    assert_eq!(coverage["line"]["total"], 6);
    assert!((coverage["line"]["percent"].as_f64().unwrap() - 400.0 / 6.0).abs() < 1e-9);
    assert_eq!(coverage["branch"]["covered"], 1);
    assert_eq!(coverage["branch"]["total"], 2);
    assert_eq!(coverage["function"]["covered"], 1);
    assert_eq!(coverage["function"]["total"], 2);

    // Per-function injection: the root space tree carries span-scoped
    // line coverage on each function space (the CRAP input).
    let spaces = json["root"]["spaces"].as_array().expect("spaces");
    let function_coverage: Vec<(String, f64)> = spaces
        .iter()
        .filter(|s| s["kind"] == "function")
        .map(|s| {
            (
                s["name"].as_str().unwrap_or_default().to_string(),
                s["metrics"]["coverage.line"].as_f64().unwrap_or(-1.0),
            )
        })
        .collect();
    assert!(
        function_coverage.contains(&("hit".to_string(), 75.0)),
        "hit spans lines 1..=4: 3 of 4 hit → 75%; got {function_coverage:?}"
    );
    assert!(
        function_coverage.contains(&("missed".to_string(), 50.0)),
        "missed spans lines 7..=8: the `def` line executes at import \
         (DA:7,1) while the body never runs (DA:8,0) → 50%; got {function_coverage:?}"
    );
}

#[test]
fn metrics_without_coverage_omits_the_family() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.py", PYTHON_BODY);
    write(dir.path(), "lcov.info", LCOV);

    // No flag, no config, no threshold: coverage must not load — and
    // the JSON must omit the family entirely (absent ≠ 0).
    let output = mehen(dir.path(), &["metrics", "app.py"]);
    let json = json_stdout(&output);
    assert!(
        json["metrics"].get("coverage").is_none(),
        "coverage family must be absent: {}",
        json["metrics"]
    );

    // `--coverage off` beats an opting-in config section.
    write(dir.path(), "mehen.toml", "[coverage]\ndiscover = true\n");
    let output = mehen(dir.path(), &["metrics", "app.py", "--coverage=off"]);
    let json = json_stdout(&output);
    assert!(json["metrics"].get("coverage").is_none());
}

#[test]
fn metrics_auto_discovers_reports_in_gitignored_directories() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.py", PYTHON_BODY);
    // The idiomatic layout: report inside a gitignored coverage/ dir.
    write(dir.path(), ".gitignore", "coverage/\n");
    write(dir.path(), "coverage/lcov.info", LCOV);

    let output = mehen(dir.path(), &["metrics", "app.py", "--coverage"]);
    let json = json_stdout(&output);
    assert_eq!(json["metrics"]["coverage"]["line"]["covered"], 4);
}

#[test]
fn config_coverage_section_opts_in_and_extra_patterns_extend_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.py", PYTHON_BODY);
    // A location no built-in pattern covers…
    write(dir.path(), "qa/run.lcovdata", LCOV);
    write(
        dir.path(),
        "mehen.toml",
        "[coverage]\ndiscover = true\nextra-patterns = [\"qa/*.lcovdata\"]\n",
    );

    // No CLI flag: the [coverage] section opts the run in.
    let output = mehen(dir.path(), &["metrics", "app.py"]);
    let json = json_stdout(&output);
    assert_eq!(json["metrics"]["coverage"]["line"]["covered"], 4);
}

#[test]
fn coverage_threshold_gates_the_metrics_command() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.py", PYTHON_BODY);
    write(dir.path(), "lcov.info", LCOV);
    // Higher-is-better: the configured limit is a minimum. 66.7% < 80.
    write(
        dir.path(),
        "mehen.toml",
        "[thresholds]\n\"coverage.line\" = 80\n",
    );

    // The threshold itself is the lazy trigger — no flag needed.
    let output = mehen(dir.path(), &["metrics", "app.py"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "66.7% line coverage must fail a min-80 gate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("coverage.line"),
        "violation must name the metric: {stderr}"
    );

    // A permissive limit passes.
    write(
        dir.path(),
        "mehen.toml",
        "[thresholds]\n\"coverage.line\" = 50\n",
    );
    let output = mehen(dir.path(), &["metrics", "app.py"]);
    assert!(
        output.status.success(),
        "66.7% must pass a min-50 gate: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A file absent from the report is unmeasured: the gate must be
    // skipped, not fired against a fabricated 0%.
    write(dir.path(), "other.py", "a = 1\n");
    write(
        dir.path(),
        "mehen.toml",
        "[thresholds]\n\"coverage.line\" = 80\n",
    );
    let output = mehen(dir.path(), &["metrics", "other.py"]);
    assert!(
        output.status.success(),
        "unmeasured file must not fail a coverage gate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn metrics_with_missing_explicit_report_is_a_setup_error() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.py", PYTHON_BODY);

    let output = mehen(
        dir.path(),
        &["metrics", "app.py", "--coverage=nonexistent.info"],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nonexistent.info"),
        "error must name the missing report: {stderr}"
    );

    // Conflicting flag values are a usage error too.
    let output = mehen(
        dir.path(),
        &["metrics", "app.py", "--coverage=auto", "--coverage=off"],
    );
    assert_eq!(output.status.code(), Some(1));

    // Explicit report paths require the `=` spelling: a space-separated
    // value would otherwise swallow a positional path
    // (`top-offenders --coverage src/`), so clap rejects it outright.
    let output = mehen(
        dir.path(),
        &["metrics", "app.py", "--coverage", "lcov.info"],
    );
    assert!(
        !output.status.success(),
        "space-separated --coverage value must be rejected"
    );
}

#[test]
fn bare_coverage_flag_does_not_swallow_positional_paths() {
    // Regression: with `require_equals`, bare `--coverage` never
    // consumes the following positional argument as its value.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "covered.py", PYTHON_BODY);
    write(dir.path(), "lcov.info", LCOV);

    let output = mehen(
        dir.path(),
        &[
            "top-offenders",
            "--metric",
            "coverage.line",
            "--coverage",
            ".",
        ],
    );
    let json = json_stdout(&mehen(
        dir.path(),
        &[
            "top-offenders",
            "--metric",
            "coverage.line",
            "--coverage",
            "-O",
            "json",
            ".",
        ],
    ));
    assert!(
        output.status.success(),
        "paths after bare --coverage must survive: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        json.as_array().is_some_and(|rows| !rows.is_empty()),
        "the positional path must be walked: {json}"
    );
}

#[test]
fn top_offenders_ranks_by_coverage_ascending_risk() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "covered.py", PYTHON_BODY);
    write(dir.path(), "uncovered.py", PYTHON_BODY);
    write(
        dir.path(),
        "lcov.info",
        "TN:\nSF:covered.py\nDA:1,5\nDA:2,5\nend_of_record\nSF:uncovered.py\nDA:1,0\nDA:2,0\nend_of_record\n",
    );

    let output = mehen(
        dir.path(),
        &[
            "top-offenders",
            "--metric",
            "coverage.line",
            "--coverage=lcov.info",
            "-O",
            "json",
            ".",
        ],
    );
    let json = json_stdout(&output);
    let rows = json.as_array().expect("offender array");
    // Suffix-match on the full component — `./uncovered.py` also ends
    // with the bytes `covered.py`, and Windows walk output spells the
    // separator as `\`.
    let value_for = |name: &str| {
        rows.iter()
            .find(|r| {
                let path = r["path"].as_str().unwrap_or_default();
                path.ends_with(&format!("/{name}")) || path.ends_with(&format!("\\{name}"))
            })
            .map(|r| r["metrics"][0]["value"].clone())
    };
    assert_eq!(value_for("uncovered.py"), Some(serde_json::json!(0.0)));
    assert_eq!(value_for("covered.py"), Some(serde_json::json!(100.0)));
    // Higher-is-better polarity: the least-covered file is the worst
    // offender and sorts first.
    assert!(
        rows[0]["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("uncovered.py"),
        "least covered first: {json}"
    );
}

/// SQL routines are function-shaped scopes too: mehen-sql nests
/// `SpaceKind::Function` spaces (one per routine) under their
/// `sql.statement` space, and the engine's coverage recursion annotates
/// them through the statement layer. An Oracle package body with one
/// covered and one uncovered routine must publish per-routine
/// `coverage.line` — the utPLSQL-style lines-only report also pins the
/// absent-dimension rule at routine granularity (no branch/function
/// keys on the spaces).
#[test]
fn sql_package_routines_receive_per_function_coverage() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "pkg_demo.sql",
        "-- sqlfluff:dialect:oracle\ncreate or replace package body pkg_demo is\n  function get_a return number is\n  begin\n    return 1;\n  end get_a;\n\n  procedure set_b(p number) is\n  begin\n    null;\n  end set_b;\nend pkg_demo;\n/\n",
    );
    // Lines-only Cobertura, the shape utPLSQL emits with -source_path
    // file mapping: get_a's body lines hit, set_b's never executed.
    write(
        dir.path(),
        "cobertura.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<coverage version="1">
<packages><package name="PKG_DEMO">
<class name="PKG_DEMO" filename="pkg_demo.sql" line-rate="0.5">
<lines>
<line number="3" hits="4" branch="false"/>
<line number="5" hits="4" branch="false"/>
<line number="8" hits="0" branch="false"/>
<line number="10" hits="0" branch="false"/>
</lines>
</class>
</package></packages>
</coverage>
"#,
    );

    let output = mehen(
        dir.path(),
        &["metrics", "pkg_demo.sql", "--coverage=cobertura.xml"],
    );
    let json = json_stdout(&output);

    // Root: 2 of 4 measured lines covered. SQL publishes its flat
    // metric map verbatim (no families pivot), so the keys sit in
    // root.metrics.
    let root_metrics = &json["root"]["metrics"];
    assert_eq!(root_metrics["coverage.line.covered"], 2, "{root_metrics}");
    assert_eq!(root_metrics["coverage.line.total"], 4);

    // The statement space carries the two routine spaces; each gets
    // span-scoped line coverage (get_a: lines 3..=6 → 2/2 hit; set_b:
    // lines 8..=11 → 0/2), and no branch/function keys — the report
    // has no such dimensions (absent, not zero).
    let statement = &json["root"]["spaces"][0];
    assert_eq!(statement["kind"]["custom"], "sql.statement", "{statement}");
    let routines = statement["spaces"].as_array().expect("routine spaces");
    let coverage_of = |name: &str| {
        let space = routines
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("missing routine space {name}: {routines:?}"));
        assert_eq!(space["kind"], "function");
        assert!(
            space["metrics"].get("coverage.branch").is_none()
                && space["metrics"].get("coverage.function").is_none(),
            "unmeasured dimensions must stay absent: {}",
            space["metrics"]
        );
        (
            space["metrics"]["coverage.line"].clone(),
            space["metrics"]["coverage.line.covered"].clone(),
            space["metrics"]["coverage.line.total"].clone(),
        )
    };
    assert_eq!(
        coverage_of("get_a"),
        (
            serde_json::json!(100.0),
            serde_json::json!(2),
            serde_json::json!(2)
        )
    );
    assert_eq!(
        coverage_of("set_b"),
        (
            serde_json::json!(0.0),
            serde_json::json!(0),
            serde_json::json!(2)
        )
    );
}
