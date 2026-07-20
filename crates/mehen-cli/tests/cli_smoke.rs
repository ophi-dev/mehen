// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Smoke tests for the 1.0 `mehen` CLI.
//!
//! Replaces the pre-1.0 `tests/cli_smoke.rs`. The pre-1.0 commands
//! `--dump`, `--find`, `--count`, `--function`, root-level `-m -p` are
//! dropped per the rewrite plan §2.1; the new surface is `metrics`,
//! `diff`, and `top-offenders`.
use std::io::Write;
use std::process::Command;

fn write_python(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).expect("create py file");
    f.write_all(body.as_bytes()).expect("write py file");
    path
}

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

#[test]
fn version_prints_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .arg("--version")
        .output()
        .expect("failed to run mehen --version");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("mehen"));
}

#[test]
fn help_succeeds() {
    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .arg("--help")
        .output()
        .expect("failed to run mehen --help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("metrics"), "expected `metrics` in help");
    assert!(stdout.contains("diff"), "expected `diff` in help");
    assert!(
        stdout.contains("top-offenders"),
        "expected `top-offenders` in help"
    );
}

#[test]
fn metrics_emits_json_for_python_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_python(
        dir.path(),
        "sample.py",
        "def foo(x):\n    if x:\n        return 1\n    return 2\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .args(["metrics", path.to_str().unwrap(), "--pretty"])
        .output()
        .expect("failed to run mehen metrics");
    assert!(
        output.status.success(),
        "mehen metrics failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("metrics output must be valid JSON");
    assert_eq!(parsed["language"].as_str(), Some("python"));
    // Phase 6 (Ruff migration): Python now reports the `python-ruff`
    // backend label. See `docs/python-ruff-spec.md`.
    assert_eq!(parsed["analysis_backend"].as_str(), Some("python-ruff"));
    let spaces = parsed["root"]["spaces"]
        .as_array()
        .expect("root must have spaces array");
    assert!(!spaces.is_empty(), "expected one function space");
    assert_eq!(spaces[0]["kind"].as_str(), Some("function"));
    assert_eq!(spaces[0]["name"].as_str(), Some("foo"));
}

#[test]
fn metrics_rejects_unknown_language() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_python(dir.path(), "sample.unknown", "def f(): pass\n");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .args(["metrics", path.to_str().unwrap()])
        .output()
        .expect("failed to run mehen metrics");
    assert!(
        !output.status.success(),
        "unknown language must fail; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn top_offenders_requires_paths() {
    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .args(["top-offenders"])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        !output.status.success(),
        "top-offenders without paths must fail"
    );
}

#[test]
fn top_offenders_respects_default_ignores_and_no_ignore_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());
    std::fs::create_dir(dir.path().join("node_modules")).expect("create ignored directory");
    std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").expect("write gitignore");
    std::fs::write(
        dir.path().join(".gitattributes"),
        "\
* -linguist-generated -linguist-vendored -binary
attributed.py linguist-generated
vendored.py linguist-vendored
binary.py binary
",
    )
    .expect("write gitattributes");
    write_python(dir.path(), "kept.py", "x = 1\n");
    write_python(dir.path(), "attributed.py", "x = 1\n");
    write_python(dir.path(), "vendored.py", "x = 1\n");
    write_python(dir.path(), "binary.py", "x = 1\n");
    write_python(
        &dir.path().join("node_modules"),
        "generated.py",
        "def generated():\n    return 1\n",
    );

    let run = |no_ignore: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mehen"));
        command.current_dir(dir.path()).args([
            "top-offenders",
            "--metric",
            "loc.lloc",
            "--output-format",
            "json",
        ]);
        if no_ignore {
            command.arg("--no-ignore");
        }
        command
            .arg(".")
            .output()
            .expect("failed to run mehen top-offenders")
    };

    let ignored = run(false);
    assert!(
        ignored.status.success(),
        "default run failed: stderr={}",
        String::from_utf8_lossy(&ignored.stderr)
    );
    let ignored: serde_json::Value =
        serde_json::from_slice(&ignored.stdout).expect("default output must be JSON");
    let ignored = ignored.as_array().expect("default output must be an array");
    assert_eq!(ignored.len(), 1);
    assert!(
        ignored[0]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("kept.py"))
    );

    let unfiltered = run(true);
    assert!(
        unfiltered.status.success(),
        "--no-ignore run failed: stderr={}",
        String::from_utf8_lossy(&unfiltered.stderr)
    );
    let unfiltered: serde_json::Value =
        serde_json::from_slice(&unfiltered.stdout).expect("--no-ignore output must be JSON");
    let mut names: Vec<&str> = unfiltered
        .as_array()
        .expect("--no-ignore output must be an array")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .filter_map(|path| std::path::Path::new(path).file_name()?.to_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "attributed.py",
            "binary.py",
            "generated.py",
            "kept.py",
            "vendored.py"
        ]
    );

    let explicit = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "top-offenders",
            "--metric",
            "loc.lloc",
            "--output-format",
            "json",
            "attributed.py",
        ])
        .output()
        .expect("failed to run mehen top-offenders for explicit file");
    assert!(
        explicit.status.success(),
        "explicit file run failed: stderr={}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let explicit: serde_json::Value =
        serde_json::from_slice(&explicit.stdout).expect("explicit output must be JSON");
    assert_eq!(explicit.as_array().map(Vec::len), Some(1));
    assert!(
        explicit[0]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("attributed.py"))
    );
}

#[test]
fn diff_respects_git_attribute_defaults_and_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());
    std::fs::write(
        dir.path().join(".gitattributes"),
        "\
* -linguist-generated -linguist-vendored -binary
generated.py linguist-generated
vendored.py linguist-vendored
binary.py binary
",
    )
    .expect("write gitattributes");
    for name in ["kept.py", "generated.py", "vendored.py", "binary.py"] {
        write_python(dir.path(), name, "x = 1\n");
    }
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "attribute-base"]);

    for name in ["kept.py", "generated.py", "vendored.py", "binary.py"] {
        write_python(dir.path(), name, "x = 2\n");
    }
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "attribute-head"]);

    std::fs::write(
        dir.path().join(".gitattributes"),
        "* -linguist-generated -linguist-vendored -binary\n",
    )
    .expect("replace checkout gitattributes");
    commit_all(dir.path(), "checkout");

    let run = |override_flag: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mehen"));
        command.current_dir(dir.path()).args([
            "diff",
            "--from",
            "attribute-base",
            "--to",
            "attribute-head",
            "--metrics",
            "loc.lloc",
            "--show-unchanged",
            "--output-format",
            "json",
        ]);
        if let Some(flag) = override_flag {
            command.arg(flag);
        }
        command
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_EVENT_NAME")
            .env_remove("GITHUB_BASE_REF")
            .env_remove("GITHUB_SHA")
            .env_remove("GITHUB_REPOSITORY")
            .output()
            .expect("failed to run mehen diff")
    };

    let paths = |output: &std::process::Output| {
        assert!(
            output.status.success(),
            "diff failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
        let mut paths: Vec<String> = value["source_code"]
            .as_array()
            .expect("source_code must be an array")
            .iter()
            .filter_map(|entry| entry["path"].as_str().map(str::to_owned))
            .collect();
        paths.sort_unstable();
        paths
    };

    assert_eq!(paths(&run(None)), vec!["kept.py"]);
    assert_eq!(
        paths(&run(Some("--ignore-git-attributes=false"))),
        vec!["binary.py", "generated.py", "kept.py", "vendored.py"]
    );
}
