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
fn antlr_syntax_errors_are_structured_without_stderr_output() {
    let dir = tempfile::tempdir().expect("tempdir");

    for (name, language, source, diagnostic_code) in [
        (
            "invalid.java",
            "java",
            "package %name.namespace%;\npublic class Broken {\n",
            "java.syntax_error",
        ),
        (
            "lexer-error.java",
            "java",
            "public class A # {}\n",
            "java.syntax_error",
        ),
        (
            "invalid.kt",
            "kotlin",
            "fun broken( {\n",
            "kotlin.syntax_error",
        ),
        (
            "lexer-error.kt",
            "kotlin",
            "class A # {}\n",
            "kotlin.syntax_error",
        ),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, source).expect("write invalid source file");

        let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
            .args([
                "metrics",
                path.to_str().expect("UTF-8 test path"),
                "--language",
                language,
            ])
            .output()
            .expect("failed to run mehen metrics");

        assert!(
            !output.status.success(),
            "{language} syntax errors must produce a non-zero exit"
        );
        assert!(
            output.stderr.is_empty(),
            "{language} syntax errors leaked raw ANTLR diagnostics: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("metrics output must be valid JSON");
        assert!(
            report["diagnostics"]
                .as_array()
                .is_some_and(|diagnostics| diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic["code"] == diagnostic_code)),
            "{language} structured diagnostics must contain {diagnostic_code}"
        );
    }
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
deleted.py linguist-generated
",
    )
    .expect("write gitattributes");
    for name in [
        "kept.py",
        "generated.py",
        "vendored.py",
        "binary.py",
        "deleted.py",
        "info-only.py",
        "global-only.py",
    ] {
        write_python(dir.path(), name, "x = 1\n");
    }
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "attribute-base"]);

    for name in [
        "kept.py",
        "generated.py",
        "vendored.py",
        "binary.py",
        "info-only.py",
        "global-only.py",
    ] {
        write_python(dir.path(), name, "x = 2\n");
    }
    std::fs::remove_file(dir.path().join("deleted.py")).expect("remove generated file");
    std::fs::write(
        dir.path().join(".gitattributes"),
        "\
* -linguist-generated -linguist-vendored -binary
generated.py linguist-generated
vendored.py linguist-vendored
binary.py binary
",
    )
    .expect("remove deleted file attribute");
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "attribute-head"]);

    std::fs::write(
        dir.path().join(".gitattributes"),
        "* -linguist-generated -linguist-vendored -binary\n",
    )
    .expect("replace checkout gitattributes");
    commit_all(dir.path(), "checkout");
    std::fs::write(
        dir.path().join(".git/info/attributes"),
        "\
generated.py -linguist-generated
info-only.py linguist-generated
",
    )
    .expect("write local attributes");
    let global_attributes = dir.path().join(".global-attributes");
    std::fs::write(
        &global_attributes,
        "\
generated.py -linguist-generated
global-only.py linguist-vendored
",
    )
    .expect("write configured attributes");
    git_ok(
        dir.path(),
        &[
            "config",
            "core.attributesFile",
            global_attributes.to_str().expect("UTF-8 temp path"),
        ],
    );

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

    assert_eq!(
        paths(&run(None)),
        vec!["global-only.py", "info-only.py", "kept.py"]
    );
    assert_eq!(
        paths(&run(Some("--ignore-git-attributes=false"))),
        vec![
            "binary.py",
            "deleted.py",
            "generated.py",
            "global-only.py",
            "info-only.py",
            "kept.py",
            "vendored.py"
        ]
    );
}

#[test]
fn diff_reports_history_metrics_for_both_sides() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "sample.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "history-base"]);

    write_python(dir.path(), "sample.py", "x = 1\ny = 2\nz = 3\nw = 4\n");
    commit_all(dir.path(), "fix: append two lines");
    git_ok(dir.path(), &["tag", "history-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "history-base",
            "--to",
            "history-head",
            "--metrics",
            "history.commit_frequency,history.churn.abs,history.bugfix_commits",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"].as_str(), Some("sample.py"));

    let metric = |name: &str| -> (f64, f64) {
        let m = files[0]["metrics"]
            .as_array()
            .expect("metrics array")
            .iter()
            .find(|m| m["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("missing metric {name}"));
        (
            m["current"].as_f64().expect("current"),
            m["baseline"].as_f64().expect("baseline"),
        )
    };

    // Head history: 2 commits, 2+2 lines added; base history: 1 commit,
    // 2 lines. Only the head commit message matches the bug-fix
    // heuristic.
    assert_eq!(metric("history.commit_frequency"), (2.0, 1.0));
    assert_eq!(metric("history.churn.abs"), (4.0, 2.0));
    assert_eq!(metric("history.bugfix_commits"), (1.0, 0.0));
}

#[test]
fn top_offenders_ranks_by_history_metrics() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    // busy.py is touched by three commits, calm.py by one.
    write_python(dir.path(), "busy.py", "a = 1\n");
    write_python(dir.path(), "calm.py", "b = 1\n");
    commit_all(dir.path(), "initial");
    write_python(dir.path(), "busy.py", "a = 1\na2 = 2\n");
    commit_all(dir.path(), "grow busy");
    write_python(dir.path(), "busy.py", "a = 1\na2 = 2\na3 = 3\n");
    commit_all(dir.path(), "grow busy more");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    assert_eq!(offenders.len(), 2);
    // Worst first: busy.py with 3 commits, then calm.py with 1.
    assert!(
        offenders[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("busy.py")
    );
    assert_eq!(offenders[0]["metrics"][0]["value"].as_f64(), Some(3.0));
    assert!(
        offenders[1]["path"]
            .as_str()
            .expect("path")
            .ends_with("calm.py")
    );
    assert_eq!(offenders[1]["metrics"][0]["value"].as_f64(), Some(1.0));
}

#[test]
fn diff_default_columns_include_history_hotspot_and_churn() {
    // Research foundation §9.4: the default PR-comment set is
    // Cognitive, ABC, MI, Hotspot, Churn — the last two computed from
    // the git history walk without any explicit `--metrics`.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(
        dir.path(),
        "sample.py",
        "def foo(x):\n    if x:\n        return 1\n    return 2\n",
    );
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "default-base"]);

    write_python(
        dir.path(),
        "sample.py",
        "def foo(x):\n    if x:\n        return 1\n    if x > 2:\n        return 3\n    return 2\n",
    );
    commit_all(dir.path(), "head");
    git_ok(dir.path(), &["tag", "default-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "default-base",
            "--to",
            "default-head",
            "--output-format",
            "markdown",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(
        stdout.contains("| File | Cognitive | ABC | MI | Hotspot | Churn |"),
        "expected the §9.4 default column header, got:\n{stdout}"
    );
    assert!(stdout.contains("sample.py"), "row missing:\n{stdout}");
}

#[test]
fn top_offenders_discovers_history_from_the_analyzed_paths() {
    // `mehen top-offenders -M history.… /path/to/repo` must load
    // *that* repository's history even when the process CWD is not
    // inside it (or is inside a different repository).
    let repo_dir = tempfile::tempdir().expect("repo tempdir");
    init_git_repo(repo_dir.path());
    write_python(repo_dir.path(), "tracked.py", "a = 1\n");
    commit_all(repo_dir.path(), "one");
    write_python(repo_dir.path(), "tracked.py", "a = 1\nb = 2\n");
    commit_all(repo_dir.path(), "two");

    let elsewhere = tempfile::tempdir().expect("non-repo tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(elsewhere.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            repo_dir.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    assert_eq!(offenders.len(), 1);
    assert!(
        offenders[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("tracked.py")
    );
    assert_eq!(offenders[0]["metrics"][0]["value"].as_f64(), Some(2.0));
}

#[test]
fn diff_joins_rename_pairs_and_carries_baseline_history() {
    // A renamed file must appear once, compared against its old path's
    // metrics and history — not as a deleted row plus a 🆕 row whose
    // entire accumulated history shows up as a fresh delta.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "before.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "rename-base"]);

    git_ok(dir.path(), &["mv", "before.py", "after.py"]);
    write_python(dir.path(), "after.py", "x = 1\ny = 2\nz = 3\n");
    commit_all(dir.path(), "rename and extend");
    git_ok(dir.path(), &["tag", "rename-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "rename-base",
            "--to",
            "rename-head",
            "--metrics",
            "loc.lloc,history.commit_frequency",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    // One joined row for the rename — not before.py deleted + after.py new.
    assert_eq!(files.len(), 1, "expected one joined rename row: {files:?}");
    assert_eq!(files[0]["path"].as_str(), Some("after.py"));
    assert_eq!(files[0]["is_new"].as_bool(), Some(false));
    assert_eq!(files[0]["is_deleted"].as_bool(), Some(false));

    let metric = |name: &str| -> (f64, f64) {
        let m = files[0]["metrics"]
            .as_array()
            .expect("metrics array")
            .iter()
            .find(|m| m["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("missing metric {name}"));
        (
            m["current"].as_f64().expect("current"),
            m["baseline"].as_f64().expect("baseline"),
        )
    };
    // Static baseline comes from the old path (2 lines → 3 lines).
    assert_eq!(metric("loc.lloc"), (3.0, 2.0));
    // History baseline is the old path's history (1 commit → 2 commits,
    // with the rename walk carrying identity across the rename).
    assert_eq!(metric("history.commit_frequency"), (2.0, 1.0));
}

#[test]
fn top_offenders_loads_history_for_every_repository_root() {
    // Input roots spanning two repositories must each read their own
    // repository's history rather than the first root's.
    let repo_a = tempfile::tempdir().expect("repo a");
    init_git_repo(repo_a.path());
    write_python(repo_a.path(), "a.py", "a = 1\n");
    commit_all(repo_a.path(), "one");
    write_python(repo_a.path(), "a.py", "a = 1\nb = 2\n");
    commit_all(repo_a.path(), "two");
    write_python(repo_a.path(), "a.py", "a = 1\nb = 2\nc = 3\n");
    commit_all(repo_a.path(), "three");

    let repo_b = tempfile::tempdir().expect("repo b");
    init_git_repo(repo_b.path());
    write_python(repo_b.path(), "b.py", "b = 1\n");
    commit_all(repo_b.path(), "only");

    let elsewhere = tempfile::tempdir().expect("non-repo tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(elsewhere.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            repo_a.path().to_str().expect("UTF-8 temp path"),
            repo_b.path().to_str().expect("UTF-8 temp path"),
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    assert_eq!(offenders.len(), 2);
    // a.py (3 commits in repo A) ranks above b.py (1 commit in repo B) —
    // and b.py reads its own repo's history, not zero.
    assert!(
        offenders[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("a.py")
    );
    assert_eq!(offenders[0]["metrics"][0]["value"].as_f64(), Some(3.0));
    assert!(
        offenders[1]["path"]
            .as_str()
            .expect("path")
            .ends_with("b.py")
    );
    assert_eq!(offenders[1]["metrics"][0]["value"].as_f64(), Some(1.0));
}

#[cfg(unix)]
#[test]
fn top_offenders_does_not_borrow_history_through_symlinks() {
    // A tracked symlink `alias.py -> real.py` must keep its own
    // (empty) history: canonicalizing the full path would resolve the
    // final component and enrich the alias row with the target file's
    // churn and commit count.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());
    write_python(dir.path(), "real.py", "r = 1\n");
    commit_all(dir.path(), "one");
    write_python(dir.path(), "real.py", "r = 1\ns = 2\n");
    commit_all(dir.path(), "two");
    std::os::unix::fs::symlink("real.py", dir.path().join("alias.py")).expect("symlink");
    commit_all(dir.path(), "add alias symlink");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    let by_name = |suffix: &str| -> f64 {
        offenders
            .iter()
            .find(|o| o["path"].as_str().expect("path").ends_with(suffix))
            .unwrap_or_else(|| panic!("missing {suffix} in {offenders:?}"))["metrics"][0]["value"]
            .as_f64()
            .expect("value")
    };
    // real.py has two content commits; the alias symlink has none of
    // them (symlinks are non-blob entries in the history walk).
    assert_eq!(by_name("real.py"), 2.0);
    assert_eq!(by_name("alias.py"), 0.0);
}

#[test]
fn top_offenders_loads_history_for_nested_repositories() {
    // A nested repository discovered *during traversal* (not passed as
    // its own root) must read its own history, not zeros from the
    // outer repository.
    let outer = tempfile::tempdir().expect("outer repo");
    init_git_repo(outer.path());
    write_python(outer.path(), "outer.py", "o = 1\n");
    commit_all(outer.path(), "outer one");

    let nested_dir = outer.path().join("vendor").join("inner");
    std::fs::create_dir_all(&nested_dir).expect("nested dir");
    init_git_repo(&nested_dir);
    write_python(&nested_dir, "inner.py", "i = 1\n");
    commit_all(&nested_dir, "inner one");
    write_python(&nested_dir, "inner.py", "i = 1\nj = 2\n");
    commit_all(&nested_dir, "inner two");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(outer.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    let by_name = |suffix: &str| -> f64 {
        offenders
            .iter()
            .find(|o| o["path"].as_str().expect("path").ends_with(suffix))
            .unwrap_or_else(|| panic!("missing {suffix} in {offenders:?}"))["metrics"][0]["value"]
            .as_f64()
            .expect("value")
    };
    // inner.py reads the nested repository's 2 commits (outer sees it
    // as untracked); outer.py reads its own single commit.
    assert_eq!(by_name("inner.py"), 2.0);
    assert_eq!(by_name("outer.py"), 1.0);
}

#[cfg(unix)]
#[test]
fn top_offenders_follows_directory_symlink_roots() {
    // `mehen top-offenders -M history.… /outside/link-to-repo` where
    // the link's parent is not a repository must discover the target
    // repository instead of failing with RepoNotFound.
    let repo_dir = tempfile::tempdir().expect("repo tempdir");
    init_git_repo(repo_dir.path());
    write_python(repo_dir.path(), "linked.py", "a = 1\n");
    commit_all(repo_dir.path(), "one");
    write_python(repo_dir.path(), "linked.py", "a = 1\nb = 2\n");
    commit_all(repo_dir.path(), "two");

    let outside = tempfile::tempdir().expect("non-repo tempdir");
    let link = outside.path().join("link-to-repo");
    std::os::unix::fs::symlink(repo_dir.path(), &link).expect("dir symlink");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(outside.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            link.to_str().expect("UTF-8 temp path"),
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    assert_eq!(offenders.len(), 1);
    assert_eq!(offenders[0]["metrics"][0]["value"].as_f64(), Some(2.0));
}

#[test]
fn cross_language_rename_to_markdown_keeps_deletion_history() {
    // `a.py → a.md` splits into a deletion + addition, and the
    // Markdown destination is routed to the documentation pipeline,
    // which carries no history columns. The Python deletion row must
    // therefore keep its baseline history — suppressing it too (as
    // for a split whose destination stays in the source-code
    // pipeline) would erase the lineage from the output entirely.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "base");
    write_python(dir.path(), "a.py", "x = 1\ny = 2\nz = 3\n");
    commit_all(dir.path(), "grow");
    git_ok(dir.path(), &["tag", "md-base"]);

    git_ok(dir.path(), &["mv", "a.py", "a.md"]);
    commit_all(dir.path(), "convert to markdown");
    git_ok(dir.path(), &["tag", "md-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "md-base",
            "--to",
            "md-head",
            "--metrics",
            "history.commit_frequency",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .unwrap_or_else(|| panic!("a.py deletion row must survive: {files:?}"));
    assert_eq!(row["is_deleted"].as_bool(), Some(true));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.commit_frequency"))
        .expect("history.commit_frequency must be present");
    // Two commits touched a.py before the conversion.
    assert_eq!(metric["baseline"].as_f64(), Some(2.0));
    assert_eq!(metric["current"].as_f64(), Some(0.0));
}

#[test]
fn cross_language_rename_to_sql_keeps_deletion_history_under_defaults() {
    // `a.py → a.sql` splits, and the SQL destination *stays* in the
    // source-code pipeline — but under default metrics SQL's
    // selectors are history-free, so the destination reads no history
    // columns. The Python deletion row must keep its baseline history
    // or the lineage vanishes from the default report.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "base");
    write_python(dir.path(), "a.py", "x = 1\ny = 2\nz = 3\n");
    commit_all(dir.path(), "grow");
    git_ok(dir.path(), &["tag", "sql-base"]);

    git_ok(dir.path(), &["mv", "a.py", "a.sql"]);
    commit_all(dir.path(), "convert to sql");
    git_ok(dir.path(), &["tag", "sql-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "sql-base",
            "--to",
            "sql-head",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .unwrap_or_else(|| panic!("a.py deletion row must survive: {files:?}"));
    assert_eq!(row["is_deleted"].as_bool(), Some(true));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.churn.relative"))
        .unwrap_or_else(|| panic!("history.churn.relative must be present: {row:?}"));
    // a.py churned lines across two commits before the conversion —
    // the baseline history must not be suppressed.
    let baseline = metric["baseline"].as_f64().expect("baseline");
    assert!(baseline > 0.0, "baseline history suppressed: {metric:?}");
}

#[test]
fn modified_then_reverted_files_report_history_deltas() {
    // The endpoint trees are identical for a.py (modified in one
    // range commit, reverted in the next), so the endpoint diff has
    // no row — but the head history gained two commits and churn.
    // With history selectors active the file must appear.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "revert-base"]);

    write_python(dir.path(), "a.py", "x = 1\ny = 2\nz = 3\n");
    commit_all(dir.path(), "grow");
    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "revert");
    git_ok(dir.path(), &["tag", "revert-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "revert-base",
            "--to",
            "revert-head",
            "--metrics",
            "history.commit_frequency",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .unwrap_or_else(|| panic!("reverted file must appear: {files:?}"));
    assert_eq!(row["is_new"].as_bool(), Some(false));
    assert_eq!(row["is_deleted"].as_bool(), Some(false));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.commit_frequency"))
        .expect("history.commit_frequency must be present");
    assert_eq!(metric["baseline"].as_f64(), Some(1.0));
    assert_eq!(metric["current"].as_f64(), Some(3.0));
}

#[test]
fn non_utf8_content_still_reports_history_metrics() {
    // Static analysis rejects non-UTF-8 (non-binary) content, but
    // history metrics don't depend on decoding the blob: an explicit
    // history selector must read real values, not zeros.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    // Latin-1 content: 0xE9 is invalid UTF-8 but contains no NUL.
    std::fs::write(dir.path().join("a.py"), b"# caf\xe9\nx = 1\n").unwrap();
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "latin-base"]);

    std::fs::write(dir.path().join("a.py"), b"# caf\xe9\nx = 1\ny = 2\n").unwrap();
    commit_all(dir.path(), "grow");
    git_ok(dir.path(), &["tag", "latin-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "latin-base",
            "--to",
            "latin-head",
            "--metrics",
            "history.commit_frequency",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .unwrap_or_else(|| panic!("non-UTF-8 file must appear: {files:?}"));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.commit_frequency"))
        .expect("history.commit_frequency must be present");
    assert_eq!(metric["baseline"].as_f64(), Some(1.0));
    assert_eq!(metric["current"].as_f64(), Some(2.0));
}

#[test]
fn authoritative_empty_push_payloads_suppress_history_augmentation() {
    // A branch created at an existing commit: the payload's commit
    // fold is authoritatively empty, but resolve_refs falls back to
    // HEAD~1..HEAD. The history range augmentation must not
    // repopulate the report with the tip's previous commit.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "old.py", "x = 1\n");
    commit_all(dir.path(), "one");
    write_python(dir.path(), "newer.py", "y = 2\n");
    commit_all(dir.path(), "two");

    let event = dir.path().join("event.json");
    std::fs::write(
        &event,
        serde_json::json!({
            "before": "0000000000000000000000000000000000000000",
            "size": 0,
            "commits": []
        })
        .to_string(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args(["diff", "--output-format", "json"])
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_EVENT_NAME", "push")
        .env("GITHUB_EVENT_PATH", &event)
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    assert!(
        files.is_empty(),
        "an authoritatively-empty push must stay empty: {files:?}"
    );
}

#[test]
fn history_diffs_support_annotated_tags() {
    // Annotated tag objects must be peeled before the range walk —
    // endpoint diffing and the history walks already peel them.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "a.py", "x = 1\n");
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "-a", "-m", "release base", "ann-base"]);
    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "grow");
    git_ok(dir.path(), &["tag", "-a", "-m", "release head", "ann-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "ann-base",
            "--to",
            "ann-head",
            "--metrics",
            "history.commit_frequency",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .unwrap_or_else(|| panic!("a.py must appear: {files:?}"));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.commit_frequency"))
        .expect("history.commit_frequency must be present");
    assert_eq!(metric["baseline"].as_f64(), Some(1.0));
    assert_eq!(metric["current"].as_f64(), Some(2.0));
}

#[test]
fn reversed_ranges_report_history_decreases() {
    // The touched-path augmentation must walk both sides of the
    // range: comparing back from a tip whose extra commits modified
    // and restored a file leaves identical endpoint trees, but the
    // *baseline* history is richer and the decrease must be visible.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "rev-old"]);

    write_python(dir.path(), "a.py", "x = 1\ny = 2\nz = 3\n");
    commit_all(dir.path(), "grow");
    write_python(dir.path(), "a.py", "x = 1\ny = 2\n");
    commit_all(dir.path(), "revert");
    git_ok(dir.path(), &["tag", "rev-new"]);

    // Reversed: from the newer tag back to the older one.
    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "rev-new",
            "--to",
            "rev-old",
            "--metrics",
            "history.commit_frequency",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .unwrap_or_else(|| panic!("from-side-only history must surface: {files:?}"));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.commit_frequency"))
        .expect("history.commit_frequency must be present");
    assert_eq!(metric["baseline"].as_f64(), Some(3.0));
    assert_eq!(metric["current"].as_f64(), Some(1.0));
}

#[test]
fn restored_markdown_stays_out_of_history_augmentation() {
    // Markdown routes to the documentation pipeline, which reads no
    // history selectors and applies no unchanged-row filter: a
    // modified-then-restored README must not be resurrected by the
    // history augmentation under default metrics.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    std::fs::write(dir.path().join("README.md"), "# Title\n\nStable body.\n").unwrap();
    write_python(dir.path(), "code.py", "x = 1\n");
    commit_all(dir.path(), "base");
    git_ok(dir.path(), &["tag", "md-rev-base"]);

    std::fs::write(dir.path().join("README.md"), "# Title\n\nTemporary body.\n").unwrap();
    commit_all(dir.path(), "touch readme");
    std::fs::write(dir.path().join("README.md"), "# Title\n\nStable body.\n").unwrap();
    commit_all(dir.path(), "restore readme");
    git_ok(dir.path(), &["tag", "md-rev-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "md-rev-base",
            "--to",
            "md-rev-head",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");
    assert!(
        output.status.success(),
        "diff failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    if let Some(docs) = value.get("markdown").and_then(|d| d.as_array()) {
        assert!(
            !docs.iter().any(|f| f["path"].as_str() == Some("README.md")),
            "an unchanged document must not be reported: {docs:?}"
        );
    }
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    assert!(
        !files
            .iter()
            .any(|f| f["path"].as_str() == Some("README.md")),
        "markdown must not enter the source-code table: {files:?}"
    );
}

#[test]
fn top_offenders_rank_non_utf8_files_on_history() {
    // Static analysis cannot decode the Latin-1 file, but its
    // repository history is real — a history selector must rank it
    // instead of silently dropping it.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    std::fs::write(dir.path().join("latin.py"), b"# caf\xe9\nx = 1\n").unwrap();
    write_python(dir.path(), "plain.py", "y = 1\n");
    commit_all(dir.path(), "base");
    std::fs::write(dir.path().join("latin.py"), b"# caf\xe9\nx = 1\nz = 2\n").unwrap();
    commit_all(dir.path(), "grow latin");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    // latin.py leads with 2 commits; plain.py has 1.
    assert_eq!(offenders.len(), 2, "non-UTF-8 file dropped: {offenders:?}");
    assert!(
        offenders[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("latin.py")
    );
    assert_eq!(offenders[0]["metrics"][0]["value"].as_f64(), Some(2.0));
}

#[test]
fn split_rename_history_composites_use_source_static_inputs() {
    // Cross-language rename: the destination row's baseline hotspot
    // must be cognitive.sum(source) × commit_frequency(source), not
    // zero — otherwise the whole current hotspot masquerades as new.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    // Python with cognitive complexity 1 (one `if`).
    let py = "def f(x):\n    if x:\n        return 1\n    return 0\n";
    write_python(dir.path(), "a.py", py);
    commit_all(dir.path(), "base");
    write_python(
        dir.path(),
        "a.py",
        "def f(x):\n    if x:\n        return 1\n    return 0\n\n\ndef g():\n    return 2\n",
    );
    commit_all(dir.path(), "grow");
    git_ok(dir.path(), &["tag", "split-base"]);

    git_ok(dir.path(), &["mv", "a.py", "a.rs"]);
    commit_all(dir.path(), "cross-language move");
    git_ok(dir.path(), &["tag", "split-head"]);

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "diff",
            "--from",
            "split-base",
            "--to",
            "split-head",
            "--metrics",
            "cognitive,history.hotspot",
            "--output-format",
            "json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("GITHUB_SHA")
        .env_remove("GITHUB_REPOSITORY")
        .output()
        .expect("failed to run mehen diff");

    // The Rust analyzer may reject the moved Python text (that is the
    // point of a cross-language split) — assert on the report itself,
    // not the exit code.
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff output must be JSON");
    let files = value["source_code"]
        .as_array()
        .expect("source_code must be an array");
    let row = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.rs"))
        .unwrap_or_else(|| panic!("split destination row must exist: {files:?}"));
    let metric = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("history.hotspot"))
        .expect("history.hotspot must be present");
    // Source lineage at split-base: cognitive.sum = 1, two commits.
    assert_eq!(
        metric["baseline"].as_f64(),
        Some(2.0),
        "baseline hotspot must use the source's static inputs: {metric:?}"
    );
    // The staged composite inputs must not leak into displayed static
    // selectors: the new row's cognitive baseline stays 0 (the paired
    // deletion row already carries the source's static baseline, and
    // leaking here would double-count it).
    let cognitive = row["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("cognitive"))
        .expect("cognitive must be present");
    assert_eq!(
        cognitive["baseline"].as_f64(),
        Some(0.0),
        "composite inputs leaked into displayed selectors: {cognitive:?}"
    );
    let deletion = files
        .iter()
        .find(|f| f["path"].as_str() == Some("a.py"))
        .expect("deletion row must exist");
    let deletion_cognitive = deletion["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"].as_str() == Some("cognitive"))
        .expect("cognitive must be present");
    assert_eq!(deletion_cognitive["baseline"].as_f64(), Some(1.0));
}

#[test]
fn top_offenders_history_supports_container_roots() {
    // The root itself is not inside Git but contains a repository:
    // per-file lazy discovery must resolve the nested repository
    // instead of the eager root check failing the whole run.
    let outer = tempfile::tempdir().expect("tempdir");
    let proj = outer.path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    init_git_repo(&proj);

    write_python(&proj, "tracked.py", "x = 1\n");
    commit_all(&proj, "one");
    write_python(&proj, "tracked.py", "x = 1\ny = 2\n");
    commit_all(&proj, "two");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(outer.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "container root must not fail: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    assert_eq!(offenders.len(), 1, "{offenders:?}");
    assert!(
        offenders[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("tracked.py")
    );
    assert_eq!(offenders[0]["metrics"][0]["value"].as_f64(), Some(2.0));
}

#[test]
fn untracked_files_do_not_inherit_dead_occupant_history() {
    // HEAD deleted the tracked file; an untracked workspace file now
    // occupies the path. Ranking must not assign it the dead
    // occupant's commits.
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo(dir.path());

    write_python(dir.path(), "ghost.py", "x = 1\n");
    write_python(dir.path(), "alive.py", "y = 1\n");
    commit_all(dir.path(), "base");
    write_python(dir.path(), "ghost.py", "x = 1\nx2 = 2\n");
    commit_all(dir.path(), "grow ghost");
    git_ok(dir.path(), &["rm", "-q", "ghost.py"]);
    commit_all(dir.path(), "drop ghost");

    // An untracked file re-occupies the path in the workspace only.
    write_python(dir.path(), "ghost.py", "unrelated = 1\n");

    let output = Command::new(env!("CARGO_BIN_EXE_mehen"))
        .current_dir(dir.path())
        .args([
            "top-offenders",
            "-M",
            "history.commit_frequency",
            "--output-format",
            "json",
            ".",
        ])
        .output()
        .expect("failed to run mehen top-offenders");
    assert!(
        output.status.success(),
        "top-offenders failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("top-offenders output must be JSON");
    let offenders = value.as_array().expect("offender array");
    let ghost = offenders
        .iter()
        .find(|o| o["path"].as_str().expect("path").ends_with("ghost.py"))
        .expect("untracked file is still ranked");
    assert_eq!(
        ghost["metrics"][0]["value"].as_f64(),
        Some(0.0),
        "the dead occupant's history leaked into the untracked file: {ghost:?}"
    );
}
