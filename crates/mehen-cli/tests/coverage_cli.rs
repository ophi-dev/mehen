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

// ─── `mehen diff` coverage: head `--coverage` + base `--base-coverage` ───

fn git(path: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(path)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Mehen Test")
        .env("GIT_AUTHOR_EMAIL", "test@mehen.invalid")
        .env("GIT_COMMITTER_NAME", "Mehen Test")
        .env("GIT_COMMITTER_EMAIL", "test@mehen.invalid")
        .output()
        .expect("failed to run git")
}

fn git_ok(path: &std::path::Path, args: &[&str]) {
    let output = git(path, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_repo(path: &std::path::Path) {
    git_ok(path, &["init", "-q", "-b", "main"]);
    git_ok(path, &["config", "commit.gpgsign", "false"]);
}

fn commit_all(path: &std::path::Path, message: &str) {
    git_ok(path, &["add", "-A"]);
    git_ok(path, &["commit", "-q", "-m", message]);
}

/// Run `mehen diff` with the CI env scrubbed so GitHub Actions
/// detection never hijacks ref resolution on a developer machine or in
/// this repo's own CI. `RUST_LOG=warn` surfaces `log::warn!` output
/// (the staleness warning) on stderr — `env_logger`'s default filter
/// is error-only.
fn mehen_diff(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir)
        .arg("diff")
        .args(args)
        .env("RUST_LOG", "warn")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff")
}

/// The `coverage.line` entry of one file's metrics array, if present.
/// Panics when the file has no diff row at all.
fn coverage_line_metric(json: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let files = json["source_code"].as_array().expect("source_code array");
    let file = files
        .iter()
        .find(|f| f["path"].as_str() == Some(path))
        .unwrap_or_else(|| panic!("missing diff row for {path}: {json}"));
    file["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("coverage.line"))
        .cloned()
}

/// Base body: cognitive 1 (one `if`).
const PYTHON_V1: &str = "def hit(flag):\n    if flag:\n        return 1\n    return 2\n";
/// Head body: cognitive 3 (nested `if`) — the statics delta keeps a
/// row alive even when its coverage entry is omitted.
const PYTHON_V2: &str = "def hit(flag):\n    if flag:\n        if flag > 1:\n            return 0\n        return 1\n    return 2\n\n\ndef extra():\n    return 3\n";

/// Base report: `app.py` 1 of 2 instrumented lines hit → 50%.
const BASE_LCOV: &str = "TN:\nSF:app.py\nDA:1,1\nDA:2,0\nend_of_record\n";
/// Head report: `app.py` 3 of 3 instrumented lines hit → 100%.
const HEAD_LCOV: &str = "TN:\nSF:app.py\nDA:1,1\nDA:2,1\nDA:5,1\nend_of_record\n";

#[test]
fn diff_carries_coverage_trend_when_both_sides_have_reports() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    write(dir.path(), "app.py", PYTHON_V1);
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    write(dir.path(), "app.py", PYTHON_V2);
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    // Reports written *after* both commits: fresher than the base
    // commit, so no staleness warning may fire.
    write(dir.path(), "base.info", BASE_LCOV);
    write(dir.path(), "head.info", HEAD_LCOV);

    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--metrics",
            "cognitive,coverage.line",
            "--coverage=head.info",
            "--base-coverage=base.info",
            "--output-format",
            "json",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let json = json_stdout(&output);

    let m = coverage_line_metric(&json, "app.py").expect("coverage.line entry");
    assert_eq!(m["current"], 100.0, "{m}");
    assert_eq!(m["baseline"], 50.0);
    assert_eq!(m["delta"], 50.0);
    // Both sides measured: the unavailable flags are omitted from JSON.
    assert!(m.get("baseline_unavailable").is_none(), "{m}");
    assert!(m.get("current_unavailable").is_none());
    assert!(
        !stderr.contains("predates the base commit"),
        "fresh report must not warn: {stderr}"
    );
}

#[test]
fn diff_defaults_surface_coverage_column_when_reports_resolve() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    write(dir.path(), "app.py", PYTHON_V1);
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    write(dir.path(), "app.py", PYTHON_V2);
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    // A name discovery never picks up (content-sniffed as LCOV on the
    // explicit path): head-side discovery must find only the idiomatic
    // `coverage/lcov.info`, not accidentally ingest the base report.
    write(dir.path(), "base-report.data", BASE_LCOV);
    // The idiomatic discoverable location for the head report.
    write(dir.path(), "coverage/lcov.info", HEAD_LCOV);

    // `--base-coverage` alone implies coverage for the head side: the
    // lazy trigger runs discovery, which finds `coverage/lcov.info`.
    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--base-coverage=base-report.data",
            "--output-format",
            "json",
        ],
    );
    let json = json_stdout(&output);
    let m = coverage_line_metric(&json, "app.py").expect("default coverage column");
    assert_eq!(m["current"], 100.0, "{m}");
    assert_eq!(m["baseline"], 50.0);
    assert_eq!(m["label"], "Coverage");

    // Markdown: the default column set gains a `Coverage` column with
    // a real higher-is-better trend cell.
    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--base-coverage=base-report.data",
        ],
    );
    assert!(output.status.success());
    let markdown = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(markdown.contains("| Coverage |"), "{markdown}");
    assert!(markdown.contains(": 50) \u{1F7E2}"), "{markdown}"); // 🟢 +50pp
    // Without any coverage request, the defaults stay coverage-free.
    let output = mehen_diff(dir.path(), &["--from", "cov-base", "--to", "cov-head"]);
    assert!(output.status.success());
    let markdown = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(!markdown.contains("| Coverage |"), "{markdown}");
}

#[test]
fn diff_renders_one_sided_coverage_as_measurement_change_and_omits_unmeasured() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    for file in ["app.py", "helper.py", "lone.py"] {
        write(dir.path(), file, PYTHON_V1);
    }
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    for file in ["app.py", "helper.py", "lone.py"] {
        write(dir.path(), file, PYTHON_V2);
    }
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    // Head report measures only `app.py`; base report only `helper.py`;
    // `lone.py` is measured on neither side.
    write(dir.path(), "head.info", HEAD_LCOV);
    write(
        dir.path(),
        "base.info",
        "TN:\nSF:helper.py\nDA:1,1\nDA:2,0\nend_of_record\n",
    );

    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--metrics",
            "cognitive,coverage.line",
            "--coverage=head.info",
            "--base-coverage=base.info",
            "--output-format",
            "json",
        ],
    );
    let json = json_stdout(&output);

    // Newly measured: head value real, no fabricated regression — the
    // base side reads *unavailable* (absent ≠ 0, extended to diff).
    let app = coverage_line_metric(&json, "app.py").expect("app.py coverage");
    assert_eq!(app["current"], 100.0, "{app}");
    assert_eq!(app["baseline_unavailable"], true);
    assert_eq!(app["delta"], 0.0, "no direction may be claimed: {app}");

    // Lost measurement: base value real, head side unavailable.
    let helper = coverage_line_metric(&json, "helper.py").expect("helper.py coverage");
    assert_eq!(helper["baseline"], 50.0, "{helper}");
    assert_eq!(helper["current_unavailable"], true);
    assert_eq!(helper["delta"], 0.0);

    // Measured on neither side: the entry is omitted entirely (the
    // column renders `–`), never an `n/a`-forever row or a fabricated
    // `0`.
    assert!(
        coverage_line_metric(&json, "lone.py").is_none(),
        "unmeasured file must omit the coverage entry"
    );

    // The same scope in Markdown pins the `–` cell for the unmeasured
    // file.
    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--metrics",
            "cognitive,coverage.line",
            "--coverage=head.info",
            "--base-coverage=base.info",
        ],
    );
    assert!(output.status.success());
    let markdown = String::from_utf8_lossy(&output.stdout).to_string();
    let lone_row = markdown
        .lines()
        .find(|line| line.starts_with("| lone.py"))
        .expect("lone.py row");
    assert!(lone_row.contains('\u{2013}'), "expected – cell: {lone_row}");
}

#[test]
fn diff_new_and_deleted_files_keep_honest_coverage_cells() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    // Contents are deliberately unrelated: rename tracking must not
    // pair the deletion with the addition.
    write(
        dir.path(),
        "old.py",
        "def legacy(a, b):\n    while a:\n        a -= b\n    return a\n",
    );
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    std::fs::remove_file(dir.path().join("old.py")).unwrap();
    write(
        dir.path(),
        "new.py",
        "class Greeter:\n    def greet(self, name):\n        if name:\n            return f\"hi {name}\"\n        return \"hi\"\n",
    );
    write(
        dir.path(),
        "unmeasured_new.py",
        "VALUES = [1, 2, 3]\n\n\ndef total():\n    return sum(VALUES)\n",
    );
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    write(
        dir.path(),
        "head.info",
        "TN:\nSF:new.py\nDA:1,1\nDA:2,1\nDA:5,1\nend_of_record\n",
    );
    write(
        dir.path(),
        "base.info",
        "TN:\nSF:old.py\nDA:1,1\nDA:2,0\nend_of_record\n",
    );

    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--metrics",
            "coverage.line",
            "--coverage=head.info",
            "--base-coverage=base.info",
            "--output-format",
            "json",
        ],
    );
    let json = json_stdout(&output);

    let new = coverage_line_metric(&json, "new.py").expect("new.py coverage");
    assert_eq!(new["is_new"], true, "{new}");
    assert_eq!(new["current"], 100.0);
    let old = coverage_line_metric(&json, "old.py").expect("old.py coverage");
    assert_eq!(old["is_deleted"], true, "{old}");
    assert_eq!(old["baseline"], 50.0);

    // A new file measured by no report has no coverage entry, and with
    // coverage as the only selected column its row carries no signal
    // at all — it must drop out rather than render an empty row.
    assert!(
        json["source_code"]
            .as_array()
            .expect("source_code array")
            .iter()
            .all(|f| f["path"].as_str() != Some("unmeasured_new.py")),
        "unmeasured new file must not produce a row: {json}"
    );
}

#[test]
fn diff_base_report_predating_base_commit_warns_once_and_is_configurable() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    write(dir.path(), "app.py", PYTHON_V1);
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    write(dir.path(), "app.py", PYTHON_V2);
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    write(dir.path(), "base.info", BASE_LCOV);
    // Backdate the report to 1970: it cannot describe the base commit.
    let report = std::fs::File::options()
        .write(true)
        .open(dir.path().join("base.info"))
        .unwrap();
    report
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .unwrap();
    drop(report);

    let args = [
        "--from",
        "cov-base",
        "--to",
        "cov-head",
        "--metrics",
        "coverage.line",
        "--base-coverage=base.info",
        "--coverage=off",
        "--output-format",
        "json",
    ];
    let output = mehen_diff(dir.path(), &args);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("predates the base commit"),
        "backdated base report must warn: {stderr}"
    );

    // `stale-warning = false` under `[coverage]` silences it.
    write(
        dir.path(),
        "mehen.toml",
        "[coverage]\nstale-warning = false\n",
    );
    let output = mehen_diff(dir.path(), &args);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stderr.contains("predates the base commit"),
        "stale-warning = false must silence the warning: {stderr}"
    );
}

#[test]
fn diff_missing_base_coverage_report_is_a_setup_error() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    write(dir.path(), "app.py", PYTHON_V1);
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    write(dir.path(), "app.py", PYTHON_V2);
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);

    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--base-coverage=nonexistent.info",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("nonexistent.info"),
        "error must name the missing report: {stderr}"
    );

    // The `=` spelling is mandatory, mirroring `--coverage`.
    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--base-coverage",
            "base.info",
        ],
    );
    assert!(
        !output.status.success(),
        "space-separated --base-coverage value must be rejected"
    );
}

#[test]
fn diff_coverage_threshold_gates_head_side_via_lazy_trigger() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    write(dir.path(), "app.py", PYTHON_V1);
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    write(dir.path(), "app.py", PYTHON_V2);
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    // Head report at the idiomatic discoverable location: 1 of 3 → 33%.
    write(
        dir.path(),
        "coverage/lcov.info",
        "TN:\nSF:app.py\nDA:1,1\nDA:2,0\nDA:5,0\nend_of_record\n",
    );
    // The configured minimum is the lazy ingestion trigger — no flag.
    write(
        dir.path(),
        "mehen.toml",
        "[thresholds]\n\"coverage.line\" = 80\n",
    );

    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--output-format",
            "json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "33% line coverage must fail a min-80 gate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("gate-failing diff still emits JSON");
    let violations = json["threshold_violations"]
        .as_array()
        .expect("threshold_violations array");
    assert!(
        violations
            .iter()
            .any(|v| v["metric"].as_str() == Some("coverage.line")),
        "violation must name coverage.line: {json}"
    );
}

#[test]
fn diff_renamed_file_reads_base_coverage_from_old_path() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());
    write(dir.path(), "app.py", PYTHON_V1);
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "cov-base"]);
    // Rename with a one-line edit: similar enough for rename tracking
    // (the diff row appears under the new path with the old path as
    // its baseline), while the coverage trend keeps the row alive.
    git_ok(dir.path(), &["mv", "app.py", "renamed.py"]);
    let mut renamed = PYTHON_V1.to_string();
    renamed.push_str("\n\nLIMIT = 3\n");
    write(dir.path(), "renamed.py", &renamed);
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "cov-head"]);
    // The base report measured the file under its *old* path.
    write(dir.path(), "base.info", BASE_LCOV);
    write(
        dir.path(),
        "head.info",
        "TN:\nSF:renamed.py\nDA:1,1\nDA:2,1\nDA:5,1\nend_of_record\n",
    );

    let output = mehen_diff(
        dir.path(),
        &[
            "--from",
            "cov-base",
            "--to",
            "cov-head",
            "--metrics",
            "cognitive,coverage.line",
            "--coverage=head.info",
            "--base-coverage=base.info",
            "--output-format",
            "json",
        ],
    );
    let json = json_stdout(&output);
    let m = coverage_line_metric(&json, "renamed.py").expect("renamed.py coverage");
    assert_eq!(m["baseline"], 50.0, "old-path base lookup: {m}");
    assert_eq!(m["current"], 100.0);
    assert!(m.get("baseline_unavailable").is_none(), "{m}");
}
