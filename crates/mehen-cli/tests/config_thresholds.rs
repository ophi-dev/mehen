// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Integration tests for `mehen.toml` metric thresholds.
//!
//! The configuration contract: per-metric limits in `[thresholds]`,
//! per-language overrides in `[languages.<lang>.thresholds]`; every
//! command that reports a configured metric exits 1 when a limit is
//! crossed, after a grouped violation report on stderr.

use std::process::Command;

/// A Python body with cognitive complexity 3 and `loc.lloc` 4.
const COMPLEX_PY: &str =
    "def foo(x):\n    if x:\n        if x > 1:\n            return 1\n    return 2\n";
/// A Python body with cognitive complexity 0 and `loc.lloc` 1.
const SIMPLE_PY: &str = "def foo():\n    return 1\n";

fn write_file(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write test file");
    path
}

fn mehen() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mehen"));
    // Diff derives refs from CI context when present; tests must not
    // inherit a real Actions environment.
    command
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY");
    command
}

fn git_ok(path: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Mehen Test")
        .env("GIT_AUTHOR_EMAIL", "test@mehen.invalid")
        .env("GIT_COMMITTER_NAME", "Mehen Test")
        .env("GIT_COMMITTER_EMAIL", "test@mehen.invalid")
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn metrics_exits_one_on_threshold_violation_with_grouped_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        dir.path(),
        "mehen.toml",
        "[thresholds]\ncognitive = 2\nloc.lloc = 3\n",
    );
    let file = write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("2 metric threshold violations"),
        "stderr must summarize the violation count: {stderr}"
    );
    assert!(
        stderr.contains("cognitive = 3 — exceeds max 2  [thresholds]"),
        "stderr must name value, limit, and config table: {stderr}"
    );
    assert!(
        stderr.contains("loc.lloc = 4 — exceeds max 3  [thresholds]"),
        "stderr must report every crossed metric: {stderr}"
    );
    assert!(
        stderr.contains("help:"),
        "stderr must carry the actionable help line: {stderr}"
    );
    // The JSON report still lands on stdout before the gate fails.
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must remain valid JSON");
    assert_eq!(parsed["language"].as_str(), Some("python"));
}

#[test]
fn metrics_succeeds_when_within_thresholds() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        dir.path(),
        "mehen.toml",
        "[thresholds]\ncognitive = 50\nloc.lloc = 50\n",
    );
    let file = write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert!(
        output.status.success(),
        "within-limit run must pass: {}",
        stderr_of(&output)
    );
}

#[test]
fn metrics_language_override_wins_and_is_named_in_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Global limit passes (50); the Python override (1) is crossed.
    write_file(
        dir.path(),
        "mehen.toml",
        "[thresholds]\ncognitive = 50\n\n[languages.python.thresholds]\ncognitive = 1\n",
    );
    let file = write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("cognitive = 3 — exceeds max 1  [languages.python.thresholds]"),
        "the override limit and its exact config table must be reported: {stderr}"
    );
}

#[test]
fn missing_explicit_config_fails_with_path_in_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_file(dir.path(), "sample.py", SIMPLE_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["--config", "absent.toml", "metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("config file not found") && stderr.contains("absent.toml"),
        "an explicit --config must fail loudly: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "a missing requested config must fail before any analysis output"
    );
}

#[test]
fn metrics_language_override_does_not_affect_other_languages() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Only Python is limited; an equally complex Rust file passes.
    write_file(
        dir.path(),
        "mehen.toml",
        "[languages.python.thresholds]\ncognitive = 1\n",
    );
    let file = write_file(
        dir.path(),
        "sample.rs",
        "fn foo(x: i32) -> i32 {\n    if x > 0 {\n        if x > 1 {\n            return 1;\n        }\n    }\n    2\n}\n",
    );

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert!(
        output.status.success(),
        "a python-only override must not gate rust files: {}",
        stderr_of(&output)
    );
}

#[test]
fn metrics_without_config_is_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert!(output.status.success());
    assert!(
        !stderr_of(&output).contains("threshold"),
        "no config means no threshold machinery in the output"
    );
}

#[test]
fn explicit_config_flag_bypasses_discovery() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The discoverable config passes; the explicit one fails.
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 50\n");
    let strict = write_file(dir.path(), "strict.toml", "[thresholds]\ncognitive = 1\n");
    let file = write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args([
            "--config",
            strict.to_str().unwrap(),
            "metrics",
            file.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains("strict.toml"),
        "the report must point at the explicitly selected config"
    );
}

#[test]
fn config_is_discovered_from_parent_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Discovery walks upward within the enclosing git repository.
    git_ok(dir.path(), &["init", "-q", "-b", "main"]);
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 1\n");
    let nested = dir.path().join("src/deep");
    std::fs::create_dir_all(&nested).expect("mkdirs");
    let file = write_file(&nested, "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(&nested)
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a repo-root mehen.toml must apply: {}",
        stderr_of(&output)
    );
}

#[test]
fn config_above_the_repository_root_is_ignored() {
    // outer/mehen.toml sits above the repository at outer/repo — it
    // cannot belong to the project, so the run stays ungated.
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 1\n");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdirs");
    git_ok(&repo, &["init", "-q", "-b", "main"]);
    let file = write_file(&repo, "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(&repo)
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert!(
        output.status.success(),
        "a config outside the repository must not gate the run: {}",
        stderr_of(&output)
    );
}

#[test]
fn malformed_config_fails_with_actionable_suggestion() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitve = 5\n");
    let file = write_file(dir.path(), "sample.py", SIMPLE_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("unknown metric `cognitve`"),
        "typos must fail at load time: {stderr}"
    );
    assert!(
        stderr.contains("did you mean `cognitive`?"),
        "typos should carry a suggestion: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "a broken config must fail before any analysis output"
    );
}

#[test]
fn invalid_config_syntax_fails_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(dir.path(), "mehen.toml", "[thresholds\ncognitive = 5\n");
    let file = write_file(dir.path(), "sample.py", SIMPLE_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["metrics", file.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_of(&output).contains("invalid TOML"),
        "syntax errors must name the file and the problem: {}",
        stderr_of(&output)
    );
}

#[test]
fn top_offenders_reports_violations_beyond_max_results_and_exits_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 1\n");
    write_file(dir.path(), "aaa.py", COMPLEX_PY);
    write_file(dir.path(), "bbb.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args([
            "top-offenders",
            "-M",
            "cognitive",
            "--max-results",
            "1",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The ranking table respects --max-results…
    assert!(stdout.contains("aaa.py"));
    assert!(!stdout.contains("bbb.py"));
    // …but the violation report covers every analyzed file, so a
    // breach cannot hide below the cut.
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("aaa.py") && stderr.contains("bbb.py"),
        "all violating files must be reported: {stderr}"
    );
}

#[test]
fn top_offenders_ignores_thresholds_for_unselected_metrics() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The limit targets loc.lloc, but the ranking only reports
    // cognitive — thresholds gate the metrics a command outputs.
    write_file(dir.path(), "mehen.toml", "[thresholds]\n\"loc.lloc\" = 1\n");
    write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["top-offenders", "-M", "cognitive", "."])
        .output()
        .expect("failed to run mehen top-offenders");

    assert!(
        output.status.success(),
        "unselected metrics must not gate the run: {}",
        stderr_of(&output)
    );
}

#[test]
fn top_offenders_gates_published_aggregate_selectors() {
    let dir = tempfile::tempdir().expect("tempdir");
    // `cognitive.max` is a published aggregate key: configurable as a
    // threshold and selectable as a ranking column, so the gate fires.
    write_file(
        dir.path(),
        "mehen.toml",
        "[thresholds]\n\"cognitive.max\" = 1\n",
    );
    write_file(dir.path(), "sample.py", COMPLEX_PY);

    let output = mehen()
        .current_dir(dir.path())
        .args(["top-offenders", "-M", "cognitive.max", "."])
        .output()
        .expect("failed to run mehen top-offenders");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("cognitive.max = 3 — exceeds max 1"),
        "aggregate selectors must be gated: {stderr}"
    );
}

#[test]
fn diff_exits_one_when_head_side_crosses_threshold() {
    let dir = tempfile::tempdir().expect("tempdir");
    git_ok(dir.path(), &["init", "-q", "-b", "main"]);
    git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
    write_file(dir.path(), "sample.py", SIMPLE_PY);
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
    write_file(dir.path(), "sample.py", COMPLEX_PY);
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "head"]);
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 1\n");

    let output = mehen()
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "HEAD~1",
            "--to",
            "HEAD",
            "--metrics",
            "cognitive",
        ])
        .output()
        .expect("failed to run mehen diff");

    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("sample.py") && stderr.contains("exceeds max 1"),
        "diff must report the head-side breach: {stderr}"
    );
    // The summary table still prints before the gate fails.
    assert!(String::from_utf8_lossy(&output.stdout).contains("sample.py"));
}

#[test]
fn diff_passes_when_head_within_thresholds() {
    let dir = tempfile::tempdir().expect("tempdir");
    git_ok(dir.path(), &["init", "-q", "-b", "main"]);
    git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
    write_file(dir.path(), "sample.py", SIMPLE_PY);
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
    write_file(dir.path(), "sample.py", COMPLEX_PY);
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "head"]);
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 100\n");

    let output = mehen()
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "HEAD~1",
            "--to",
            "HEAD",
            "--metrics",
            "cognitive",
        ])
        .output()
        .expect("failed to run mehen diff");

    assert!(
        output.status.success(),
        "within-limit diff must pass: {}",
        stderr_of(&output)
    );
}

#[test]
fn diff_json_output_still_emitted_before_threshold_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    git_ok(dir.path(), &["init", "-q", "-b", "main"]);
    git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
    write_file(dir.path(), "sample.py", SIMPLE_PY);
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
    write_file(dir.path(), "sample.py", COMPLEX_PY);
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "head"]);
    write_file(dir.path(), "mehen.toml", "[thresholds]\ncognitive = 1\n");

    let output = mehen()
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "HEAD~1",
            "--to",
            "HEAD",
            "--metrics",
            "cognitive",
            "--output-format",
            "json",
        ])
        .output()
        .expect("failed to run mehen diff");

    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("machine output must stay parseable when the gate fails");
    assert!(value["source_code"].is_array());
    // The explicit gate signal machine consumers (e.g. the GitHub
    // Action) use to distinguish a quality-gate exit from an analysis
    // failure, which also exits 1 but without this key.
    let violations = value["threshold_violations"]
        .as_array()
        .expect("gate failures must carry threshold_violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["metric"].as_str(), Some("cognitive"));
    assert_eq!(violations[0]["path"].as_str(), Some("sample.py"));
    assert_eq!(violations[0]["limit"].as_f64(), Some(1.0));
}
