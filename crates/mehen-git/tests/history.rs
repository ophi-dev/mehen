// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Integration tests for the history-walk subsystem against a real,
//! fully deterministic fixture repository (pinned author identities
//! and commit timestamps).

use std::path::Path;

use mehen_git::collect_history;

// Fixture timestamps (seconds since epoch, first-of-month UTC 2026).
const T_JAN: i64 = 1_767_225_600;
const T_FEB: i64 = 1_769_904_000;
const T_MAR: i64 = 1_772_323_200;
const T_APR: i64 = 1_775_001_600;
const T_MAY: i64 = 1_777_593_600;
const T_JUN: i64 = 1_780_272_000;

/// Average Gregorian month in seconds — must match
/// `mehen_git::history::SECONDS_PER_MONTH`.
const SECONDS_PER_MONTH: f64 = 2_629_746.0;

fn git(repo: &Path, args: &[&str], author: (&str, &str), seconds: i64) {
    let date = format!("{seconds} +0000");
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_COMMITTER_NAME", author.0)
        .env("GIT_COMMITTER_EMAIL", author.1)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const ALICE: (&str, &str) = ("Alice", "alice@mehen.invalid");
const BOB: (&str, &str) = ("Bob", "bob@mehen.invalid");
const CAROL: (&str, &str) = ("Carol", "carol@mehen.invalid");

/// Time-Weighted Risk term for one bug-fixing commit at `seconds`,
/// normalized over `[first, head]` (Lewis et al. logistic, ω = 12).
fn twr_term(seconds: i64, first: i64, head: i64) -> f64 {
    let t = (seconds - first) as f64 / (head - first) as f64;
    1.0 / (1.0 + (-12.0 * t + 12.0).exp())
}

/// Fixture:
///   Jan (alice): add a.rs (3 lines) + b.rs (2 lines)   "initial import"
///   Feb (alice): a.rs +2 lines                         "feat: expand a"
///   Mar (bob):   a.rs rewrite 1 line (+1/−1)           "fix: bug in a"
///   Apr (alice): b.rs +1 line                          "feat: more b"
///   May (carol): add c.rs (2 lines) on branch topic    "fix typo"
///   Jun (alice): merge topic --no-ff                   "merge topic branch"
fn build_fixture(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(dir, &["config", "commit.gpgsign", "false"], ALICE, T_JAN);

    std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn x() {}\nfn y() {}\n").unwrap();
    git(dir, &["add", "-A"], ALICE, T_JAN);
    git(dir, &["commit", "-q", "-m", "initial import"], ALICE, T_JAN);

    std::fs::write(
        dir.join("a.rs"),
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n",
    )
    .unwrap();
    git(
        dir,
        &["commit", "-q", "-am", "feat: expand a"],
        ALICE,
        T_FEB,
    );

    std::fs::write(
        dir.join("a.rs"),
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e_fixed() {}\n",
    )
    .unwrap();
    git(dir, &["commit", "-q", "-am", "fix: bug in a"], BOB, T_MAR);

    std::fs::write(dir.join("b.rs"), "fn x() {}\nfn y() {}\nfn z() {}\n").unwrap();
    git(dir, &["commit", "-q", "-am", "feat: more b"], ALICE, T_APR);

    git(dir, &["checkout", "-q", "-b", "topic"], CAROL, T_MAY);
    std::fs::write(dir.join("c.rs"), "fn m() {}\nfn n() {}\n").unwrap();
    git(dir, &["add", "c.rs"], CAROL, T_MAY);
    git(dir, &["commit", "-q", "-m", "fix typo"], CAROL, T_MAY);

    git(dir, &["checkout", "-q", "main"], ALICE, T_JUN);
    git(
        dir,
        &[
            "merge",
            "-q",
            "--no-ff",
            "--no-edit",
            "-m",
            "merge topic branch",
            "topic",
        ],
        ALICE,
        T_JUN,
    );
}

#[test]
fn collect_history_computes_all_per_file_statistics() {
    let dir = tempfile::tempdir().unwrap();
    build_fixture(dir.path());
    let repo = gix::discover(dir.path()).unwrap();

    let history = collect_history(&repo, "HEAD").unwrap();

    // "Now" is the head (merge) commit's committer time, not wall clock.
    assert_eq!(history.head_seconds, T_JUN);
    // a.rs, b.rs, c.rs — the merge commit itself contributes nothing.
    assert_eq!(history.len(), 3);

    let a = history.file(Path::new("a.rs")).expect("a.rs history");
    assert_eq!(a.commit_frequency, 3);
    assert_eq!(a.churn_added, 6); // 3 (add) + 2 (expand) + 1 (fix)
    assert_eq!(a.churn_removed, 1); // 1 (fix)
    assert_eq!(a.churn_abs(), 7);
    assert_eq!(a.authors, 2); // alice, bob
    // alice churned 5 of 7 lines (71%), bob 2 of 7 (29%): no minors.
    assert_eq!(a.minor_contributors, 0);
    assert!((a.ownership - 5.0 / 7.0).abs() < 1e-9);
    assert_eq!(a.last_change_seconds, T_MAR);
    // Only the initial 2-file commit couples a.rs with another file.
    assert_eq!(a.sum_of_coupling, 1);
    assert_eq!(a.bugfix_commits, 1); // "fix: bug in a"
    let expected_twr = twr_term(T_MAR, T_JAN, T_JUN);
    assert!((a.twr - expected_twr).abs() < 1e-12);
    let expected_age = (T_JUN - T_MAR) as f64 / SECONDS_PER_MONTH;
    assert!((a.age_months(history.head_seconds) - expected_age).abs() < 1e-9);

    let b = history.file(Path::new("b.rs")).expect("b.rs history");
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 3); // 2 (add) + 1 (more b)
    assert_eq!(b.churn_removed, 0);
    assert_eq!(b.authors, 1);
    assert_eq!(b.minor_contributors, 0);
    assert!((b.ownership - 1.0).abs() < 1e-9);
    assert_eq!(b.last_change_seconds, T_APR);
    assert_eq!(b.sum_of_coupling, 1);
    assert_eq!(b.bugfix_commits, 0);
    assert_eq!(b.twr, 0.0);

    let c = history.file(Path::new("c.rs")).expect("c.rs history");
    assert_eq!(c.commit_frequency, 1);
    assert_eq!(c.churn_added, 2);
    assert_eq!(c.churn_removed, 0);
    assert_eq!(c.authors, 1);
    assert!((c.ownership - 1.0).abs() < 1e-9);
    assert_eq!(c.last_change_seconds, T_MAY);
    assert_eq!(c.sum_of_coupling, 0);
    assert_eq!(c.bugfix_commits, 1); // "fix typo"
    let expected_twr = twr_term(T_MAY, T_JAN, T_JUN);
    assert!((c.twr - expected_twr).abs() < 1e-12);
}

#[test]
fn collect_history_is_rev_scoped_and_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    build_fixture(dir.path());
    let repo = gix::discover(dir.path()).unwrap();

    // Walking an older rev must only see history up to that rev.
    let at_march = collect_history(&repo, "HEAD~1^").unwrap();
    // HEAD~1 is the Apr commit (first parent of the merge); its parent
    // is the Mar fix. c.rs does not exist there and b.rs has one commit.
    assert_eq!(at_march.head_seconds, T_MAR);
    assert!(at_march.file(Path::new("c.rs")).is_none());
    let b = at_march.file(Path::new("b.rs")).expect("b.rs history");
    assert_eq!(b.commit_frequency, 1);

    // Two walks of the same rev produce identical values.
    let one = collect_history(&repo, "HEAD").unwrap();
    let two = collect_history(&repo, "HEAD").unwrap();
    for path in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(one.file(Path::new(path)), two.file(Path::new(path)));
    }
}

#[test]
fn oversized_changesets_do_not_contribute_to_coupling() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    // Commit 1: hub.rs plus 31 filler files — a 32-file changeset,
    // above the 30-file coupling noise threshold.
    std::fs::write(dir.path().join("hub.rs"), "fn hub() {}\n").unwrap();
    for i in 0..31 {
        std::fs::write(dir.path().join(format!("filler{i:02}.rs")), "fn f() {}\n").unwrap();
    }
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(
        dir.path(),
        &["commit", "-q", "-m", "bulk import"],
        ALICE,
        T_JAN,
    );

    // Commit 2: hub.rs plus one filler — a qualifying 2-file changeset.
    std::fs::write(dir.path().join("hub.rs"), "fn hub() {}\nfn spoke() {}\n").unwrap();
    std::fs::write(dir.path().join("filler00.rs"), "fn f() {}\nfn g() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "grow hub"],
        ALICE,
        T_FEB,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let hub = history.file(Path::new("hub.rs")).expect("hub history");
    // The 32-file bulk import is ignored for coupling; only the 2-file
    // commit counts (1 other file). Churn still counts both commits.
    assert_eq!(hub.sum_of_coupling, 1);
    assert_eq!(hub.commit_frequency, 2);
    assert_eq!(hub.churn_added, 2);
}

#[test]
fn blob_to_gitlink_type_change_does_not_fail_the_walk() {
    // A checked-in file replaced by a submodule produces a
    // `Modification` whose new mode is a gitlink (commit) pointing at
    // an object that only exists in the submodule. Reading it as a
    // blob would fail the whole walk; it must count as the old blob's
    // deletion instead.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(dir.path().join("dep"), "line one\nline two\n").unwrap();
    git(dir.path(), &["add", "dep"], ALICE, T_JAN);
    git(
        dir.path(),
        &["commit", "-q", "-m", "vendor dep"],
        ALICE,
        T_JAN,
    );

    // Replace the blob with a gitlink to a commit absent from this
    // repository's object database (the normal submodule situation).
    std::fs::remove_file(dir.path().join("dep")).unwrap();
    git(dir.path(), &["rm", "-q", "--cached", "dep"], ALICE, T_FEB);
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,dep",
        ],
        ALICE,
        T_FEB,
    );
    git(
        dir.path(),
        &["commit", "-q", "-m", "switch dep to submodule"],
        ALICE,
        T_FEB,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").expect("walk must survive gitlink type change");

    let dep = history.file(Path::new("dep")).expect("dep history");
    assert_eq!(dep.commit_frequency, 2);
    assert_eq!(dep.churn_added, 2); // initial blob
    assert_eq!(dep.churn_removed, 2); // blob → gitlink counts as its deletion
    assert_eq!(dep.last_change_seconds, T_FEB);
}
