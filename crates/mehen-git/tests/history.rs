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
    // alice wrote 5 of 6 added lines (83%), bob 1 of 6 (17%): no minors.
    // (Authorship counts added lines only — deletions aren't writing.)
    assert_eq!(a.minor_contributors, 0);
    assert!((a.ownership - 5.0 / 6.0).abs() < 1e-9);
    assert_eq!(a.last_change_seconds, T_MAR);
    // Only the initial 2-file commit couples a.rs with another file.
    assert_eq!(a.sum_of_coupling, 1);
    assert_eq!(a.bugfix_commits, 1); // "fix: bug in a"
    let expected_twr = twr_term(T_MAR, T_JAN, T_JUN);
    // TWR is quantized to 1e-9 before publication.
    assert!((a.twr - expected_twr).abs() < 1e-9);
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
    assert!((c.twr - expected_twr).abs() < 1e-9);
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

#[test]
fn renames_preserve_file_identity_and_history() {
    // A rename must not split the file's history: the head-relative
    // path keeps the pre-rename commits/churn/authors, a pure rename
    // churns nothing, and a rename-with-edit churns only the edit.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    // pure.rs: 4 lines by alice, then renamed untouched by bob.
    std::fs::write(
        dir.path().join("pure.rs"),
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "pure.rs"], ALICE, T_JAN);
    git(
        dir.path(),
        &["commit", "-q", "-m", "add pure"],
        ALICE,
        T_JAN,
    );
    git(
        dir.path(),
        &["mv", "pure.rs", "renamed_pure.rs"],
        BOB,
        T_FEB,
    );
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename pure"],
        BOB,
        T_FEB,
    );

    // edited.rs: 10 lines by alice, then renamed *and* edited (2 lines
    // rewritten) by bob — 80% similar, above the 50% threshold.
    let lines: Vec<String> = (0..10).map(|i| format!("fn f{i}() {{}}")).collect();
    std::fs::write(dir.path().join("edited.rs"), lines.join("\n") + "\n").unwrap();
    git(dir.path(), &["add", "edited.rs"], ALICE, T_MAR);
    git(
        dir.path(),
        &["commit", "-q", "-m", "add edited"],
        ALICE,
        T_MAR,
    );
    let mut edited: Vec<String> = lines.clone();
    edited[0] = "fn f0_changed() {}".to_string();
    edited[9] = "fn f9_changed() {}".to_string();
    std::fs::remove_file(dir.path().join("edited.rs")).unwrap();
    std::fs::write(
        dir.path().join("renamed_edited.rs"),
        edited.join("\n") + "\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, T_APR);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename and tweak edited"],
        BOB,
        T_APR,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // No stale entries under the pre-rename paths.
    assert!(history.file(Path::new("pure.rs")).is_none());
    assert!(history.file(Path::new("edited.rs")).is_none());

    let pure = history
        .file(Path::new("renamed_pure.rs"))
        .expect("renamed_pure history");
    // Pre-rename history is carried over; the pure rename churns nothing.
    assert_eq!(pure.commit_frequency, 2);
    assert_eq!(pure.churn_added, 4);
    assert_eq!(pure.churn_removed, 0);
    assert_eq!(pure.authors, 2); // alice (4 lines) + bob (0 lines)
    assert!((pure.ownership - 1.0).abs() < 1e-9);
    assert_eq!(pure.last_change_seconds, T_FEB);

    let edited = history
        .file(Path::new("renamed_edited.rs"))
        .expect("renamed_edited history");
    // Creation (10 lines) plus only the two rewritten lines.
    assert_eq!(edited.commit_frequency, 2);
    assert_eq!(edited.churn_added, 12);
    assert_eq!(edited.churn_removed, 2);
    assert_eq!(edited.last_change_seconds, T_APR);
}

#[test]
fn changed_files_joins_rename_pairs() {
    // A rename between `from` and `to` must surface as one `Modified`
    // entry carrying `source_path`, not a deletion + addition pair —
    // otherwise diff consumers lose the baseline for both metric and
    // history comparison.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(
        dir.path().join("before.rs"),
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "rename-base"], ALICE, T_JAN);

    git(dir.path(), &["mv", "before.rs", "after.rs"], ALICE, T_FEB);
    git(dir.path(), &["commit", "-q", "-m", "rename"], ALICE, T_FEB);
    git(dir.path(), &["tag", "rename-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "rename-base", "rename-head").unwrap();

    assert_eq!(
        changed.len(),
        1,
        "rename must be a single entry: {changed:?}"
    );
    let cf = &changed[0];
    assert_eq!(cf.path, Path::new("after.rs"));
    assert_eq!(cf.status, mehen_git::ChangeStatus::Modified);
    assert_eq!(cf.source_path.as_deref(), Some(Path::new("before.rs")));
}

#[test]
fn changed_files_reports_type_changes_from_the_blob_side() {
    // A blob replaced by a gitlink must surface as the blob's
    // *deletion*, not a `Modified` row whose baseline/head reads would
    // hit a gitlink OID absent from the superproject odb.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(dir.path().join("dep.py"), "d = 1\nd2 = 2\n").unwrap();
    git(dir.path(), &["add", "dep.py"], ALICE, T_JAN);
    git(
        dir.path(),
        &["commit", "-q", "-m", "vendor dep"],
        ALICE,
        T_JAN,
    );
    git(dir.path(), &["tag", "type-base"], ALICE, T_JAN);

    std::fs::remove_file(dir.path().join("dep.py")).unwrap();
    git(
        dir.path(),
        &["rm", "-q", "--cached", "dep.py"],
        ALICE,
        T_FEB,
    );
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,dep.py",
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
    git(dir.path(), &["tag", "type-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "type-base", "type-head").unwrap();

    assert_eq!(changed.len(), 1, "one blob-side entry: {changed:?}");
    assert_eq!(changed[0].path, Path::new("dep.py"));
    assert_eq!(changed[0].status, mehen_git::ChangeStatus::Deleted);
    assert!(changed[0].source_path.is_none());
}

#[test]
fn parallel_branch_edits_before_a_rename_are_merged_into_the_survivor() {
    // Branches diverge after a.rs is created; the side branch edits
    // a.rs with a *later* timestamp than main's rename to b.rs. The
    // newest-first walk therefore accumulates the edit under a.rs
    // before it learns about the rename — that stranded accumulator
    // must be folded into b.rs, not left behind.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "a.rs"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "add a"], ALICE, T_JAN);

    // Side branch: edit a.rs at T_MAR (after main's rename time).
    git(dir.path(), &["checkout", "-q", "-b", "side"], BOB, T_MAR);
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0_edited() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "edit a"], BOB, T_MAR);

    // Main: rename a.rs -> b.rs at T_FEB (before the side edit's time).
    git(dir.path(), &["checkout", "-q", "main"], ALICE, T_FEB);
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename a"],
        ALICE,
        T_FEB,
    );

    // Merge (git's own rename detection applies the edit to b.rs).
    git(
        dir.path(),
        &[
            "merge",
            "-q",
            "--no-edit",
            "-m",
            "merge side branch",
            "side",
        ],
        ALICE,
        T_APR,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // No stranded entry under the pre-rename path.
    assert!(history.file(Path::new("a.rs")).is_none());

    let b = history.file(Path::new("b.rs")).expect("b.rs history");
    // Creation (4 lines, alice) + side edit (+1/−1, bob) + rename (0).
    assert_eq!(b.commit_frequency, 3);
    assert_eq!(b.churn_added, 5);
    assert_eq!(b.churn_removed, 1);
    assert_eq!(b.authors, 2);
    assert_eq!(b.last_change_seconds, T_MAR);
}

#[test]
fn changed_files_joins_renames_that_also_change_content() {
    // Rename tracking must hold at the pinned 50% similarity
    // threshold, not just for identical blobs: a rename that also
    // edits a minority of lines stays a joined `Modified` row.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let lines: Vec<String> = (0..10).map(|i| format!("fn f{i}() {{}}")).collect();
    std::fs::write(dir.path().join("orig.rs"), lines.join("\n") + "\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "edit-rename-base"], ALICE, T_JAN);

    // Rename plus a 2-of-10-line edit — 80% similar, above 50%.
    let mut edited = lines.clone();
    edited[0] = "fn f0_changed() {}".to_string();
    edited[9] = "fn f9_changed() {}".to_string();
    std::fs::remove_file(dir.path().join("orig.rs")).unwrap();
    std::fs::write(dir.path().join("moved.rs"), edited.join("\n") + "\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename and edit"],
        ALICE,
        T_FEB,
    );
    git(dir.path(), &["tag", "edit-rename-head"], ALICE, T_FEB);

    // A rewrite below 50% similarity must NOT pair: replace a second
    // file wholesale under a new name.
    std::fs::write(dir.path().join("old_impl.rs"), "fn tiny() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_MAR);
    git(dir.path(), &["commit", "-q", "-m", "tiny"], ALICE, T_MAR);
    git(dir.path(), &["tag", "dissimilar-base"], ALICE, T_MAR);
    std::fs::remove_file(dir.path().join("old_impl.rs")).unwrap();
    std::fs::write(
        dir.path().join("new_impl.rs"),
        "fn completely() {}\nfn different() {}\nfn content() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_APR);
    git(dir.path(), &["commit", "-q", "-m", "replace"], ALICE, T_APR);
    git(dir.path(), &["tag", "dissimilar-head"], ALICE, T_APR);

    let repo = gix::discover(dir.path()).unwrap();

    let changed = mehen_git::changed_files(&repo, "edit-rename-base", "edit-rename-head").unwrap();
    assert_eq!(changed.len(), 1, "80%-similar rename joins: {changed:?}");
    assert_eq!(changed[0].path, Path::new("moved.rs"));
    assert_eq!(changed[0].status, mehen_git::ChangeStatus::Modified);
    assert_eq!(
        changed[0].source_path.as_deref(),
        Some(Path::new("orig.rs"))
    );

    let changed = mehen_git::changed_files(&repo, "dissimilar-base", "dissimilar-head").unwrap();
    let mut statuses: Vec<(String, mehen_git::ChangeStatus)> = changed
        .iter()
        .map(|cf| (cf.path.display().to_string(), cf.status))
        .collect();
    statuses.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        statuses,
        vec![
            ("new_impl.rs".to_string(), mehen_git::ChangeStatus::Added),
            ("old_impl.rs".to_string(), mehen_git::ChangeStatus::Deleted),
        ],
        "below-threshold rewrite must stay a deletion + addition"
    );
}

#[test]
fn reused_source_path_keeps_its_own_history_after_a_rename() {
    // Commit 1 adds a.rs, commit 2 renames it to b.rs, commit 3 adds a
    // brand-new unrelated a.rs. The new a.rs must keep its own history
    // instead of being folded into b.rs by the rename alias.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(
        dir.path().join("a.rs"),
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "a.rs"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "add a"], ALICE, T_JAN);

    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename a"],
        ALICE,
        T_FEB,
    );

    std::fs::write(dir.path().join("a.rs"), "fn brand_new() {}\n").unwrap();
    git(dir.path(), &["add", "a.rs"], BOB, T_MAR);
    git(dir.path(), &["commit", "-q", "-m", "new a"], BOB, T_MAR);

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // The re-created a.rs keeps its own single-commit history.
    let a = history.file(Path::new("a.rs")).expect("new a.rs history");
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 1);
    assert_eq!(a.authors, 1);
    assert_eq!(a.last_change_seconds, T_MAR);

    // b.rs carries the renamed lineage: the original creation (walked
    // after the rename, redirected by the alias) plus the rename.
    let b = history.file(Path::new("b.rs")).expect("b.rs history");
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 3);
    assert_eq!(b.last_change_seconds, T_FEB);
}

#[test]
fn single_line_file_renames_with_edits_stay_joined() {
    // A one-line file has zero common *lines* after any edit; the
    // byte-level similarity fallback must still join the rename so the
    // diff keeps its baseline and history keeps its lineage.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let one_liner = format!("export const table = [{}];", "1, ".repeat(100));
    std::fs::write(dir.path().join("bundle.js"), &one_liner).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "oneline-base"], ALICE, T_JAN);

    // Rename plus a small in-line edit.
    std::fs::remove_file(dir.path().join("bundle.js")).unwrap();
    std::fs::write(
        dir.path().join("bundle.min.js"),
        one_liner.replace("const table", "const lookup"),
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename bundle"],
        ALICE,
        T_FEB,
    );
    git(dir.path(), &["tag", "oneline-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "oneline-base", "oneline-head").unwrap();
    assert_eq!(changed.len(), 1, "one-line rename joins: {changed:?}");
    assert_eq!(changed[0].path, Path::new("bundle.min.js"));
    assert_eq!(changed[0].status, mehen_git::ChangeStatus::Modified);
    assert_eq!(
        changed[0].source_path.as_deref(),
        Some(Path::new("bundle.js"))
    );
}

#[test]
fn deletion_only_commits_do_not_create_minor_contributors() {
    // bob's only touch is deleting lines: he counts as an author but
    // must not appear as a sub-5% minor contributor, and ownership
    // stays with the writer.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let lines: Vec<String> = (0..10).map(|i| format!("fn f{i}() {{}}")).collect();
    std::fs::write(dir.path().join("code.rs"), lines.join("\n") + "\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(
        dir.path(),
        &["commit", "-q", "-m", "write it"],
        ALICE,
        T_JAN,
    );

    // bob deletes the last four functions, adds nothing.
    std::fs::write(dir.path().join("code.rs"), lines[..6].join("\n") + "\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "prune dead code"],
        BOB,
        T_FEB,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();
    let code = history.file(Path::new("code.rs")).expect("code history");
    assert_eq!(code.authors, 2); // alice wrote, bob touched
    assert_eq!(code.minor_contributors, 0); // bob wrote nothing — not "minor"
    assert!((code.ownership - 1.0).abs() < 1e-9); // alice owns all added lines
    assert_eq!(code.churn_added, 10);
    assert_eq!(code.churn_removed, 4);
}

#[test]
fn changed_files_recovers_renames_hidden_behind_path_reuse() {
    // Between base and head, a.rs was renamed to b.rs and an unrelated
    // new a.rs was created. The endpoint tree diff sees Modified(a.rs)
    // + Added(b.rs); break-rewrite detection must recover the real
    // shape: b.rs is the rename of the old a.rs, the new a.rs is an
    // addition.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let original: Vec<String> = (0..8).map(|i| format!("fn original_{i}() {{}}")).collect();
    std::fs::write(dir.path().join("a.rs"), original.join("\n") + "\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "add a"], ALICE, T_JAN);
    git(dir.path(), &["tag", "reuse-base"], ALICE, T_JAN);

    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename a"],
        ALICE,
        T_FEB,
    );

    std::fs::write(
        dir.path().join("a.rs"),
        "const REPLACEMENT: &str = \"totally unrelated\";\n",
    )
    .unwrap();
    git(dir.path(), &["add", "a.rs"], BOB, T_MAR);
    git(dir.path(), &["commit", "-q", "-m", "new a"], BOB, T_MAR);
    git(dir.path(), &["tag", "reuse-head"], ALICE, T_MAR);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "reuse-base", "reuse-head").unwrap();

    let mut summary: Vec<(String, mehen_git::ChangeStatus, Option<String>)> = changed
        .iter()
        .map(|cf| {
            (
                cf.path.display().to_string(),
                cf.status,
                cf.source_path.as_ref().map(|p| p.display().to_string()),
            )
        })
        .collect();
    summary.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        summary,
        vec![
            (
                "a.rs".to_string(),
                mehen_git::ChangeStatus::Added,
                None // the new a.rs is genuinely new content
            ),
            (
                "b.rs".to_string(),
                mehen_git::ChangeStatus::Modified,
                Some("a.rs".to_string()) // carries the old lineage
            ),
        ]
    );
}

#[test]
fn heavily_rewritten_files_stay_modified_when_nothing_pairs() {
    // A same-path full rewrite with no rename candidates around must
    // stay a single Modified row (the speculative break-rewrite is
    // reassembled), not degrade into a deletion + addition.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(dir.path().join("config.rs"), "fn old_world() {}\n").unwrap();
    // An unrelated addition so the break pass is actually exercised
    // (it is skipped entirely when there is nothing to pair with).
    std::fs::write(dir.path().join("unrelated.txt"), "notes\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "rewrite-base"], ALICE, T_JAN);

    std::fs::write(
        dir.path().join("config.rs"),
        "const COMPLETELY_DIFFERENT: u32 = 42;\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("second.txt"), "more notes\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(dir.path(), &["commit", "-q", "-m", "rewrite"], ALICE, T_FEB);
    git(dir.path(), &["tag", "rewrite-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "rewrite-base", "rewrite-head").unwrap();

    let config: Vec<_> = changed
        .iter()
        .filter(|cf| cf.path == Path::new("config.rs"))
        .collect();
    assert_eq!(config.len(), 1, "one row for config.rs: {changed:?}");
    assert_eq!(config[0].status, mehen_git::ChangeStatus::Modified);
    assert!(config[0].source_path.is_none());
}

#[test]
fn identical_blob_renames_pair_by_path_affinity() {
    // Two identical files swapped between directories: src/foo.rs →
    // tests/foo.rs and tests/bar.rs → src/bar.rs. Pairing by
    // lexicographic order would cross the lineages; path affinity
    // (matching basenames) must keep each file with its own history.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let same_content = "fn shared() {}\nfn helper() {}\n";
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();
    std::fs::write(dir.path().join("src/foo.rs"), same_content).unwrap();
    std::fs::write(dir.path().join("tests/bar.rs"), same_content).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "swap-base"], ALICE, T_JAN);

    git(
        dir.path(),
        &["mv", "src/foo.rs", "tests/foo.rs"],
        ALICE,
        T_FEB,
    );
    git(
        dir.path(),
        &["mv", "tests/bar.rs", "src/bar.rs"],
        ALICE,
        T_FEB,
    );
    git(dir.path(), &["commit", "-q", "-m", "swap"], ALICE, T_FEB);
    git(dir.path(), &["tag", "swap-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "swap-base", "swap-head").unwrap();

    let source_of = |dest: &str| -> String {
        changed
            .iter()
            .find(|cf| cf.path == Path::new(dest))
            .unwrap_or_else(|| panic!("missing {dest} in {changed:?}"))
            .source_path
            .as_ref()
            .expect("rename must carry a source")
            .display()
            .to_string()
    };
    assert_eq!(source_of("tests/foo.rs"), "src/foo.rs");
    assert_eq!(source_of("src/bar.rs"), "tests/bar.rs");
}

#[test]
fn binary_revisions_of_source_paths_churn_zero_lines() {
    // A sub-cap binary revision (NUL bytes) of a tracked source path
    // must not count its bytes as added/removed source lines.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(dir.path().join("gen.rs"), "fn text() {}\nfn more() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "text"], ALICE, T_JAN);

    // Binary interlude: NUL-containing generated revision.
    std::fs::write(dir.path().join("gen.rs"), b"\x00\x01\x02binary\ngarbage\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "binary blob"],
        ALICE,
        T_FEB,
    );

    // Back to parseable text.
    std::fs::write(dir.path().join("gen.rs"), "fn text() {}\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "text again"],
        ALICE,
        T_MAR,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();
    let generated = history.file(Path::new("gen.rs")).expect("gen history");
    assert_eq!(generated.commit_frequency, 3);
    // Only the initial text creation (2 lines) counts as added source;
    // both binary-involving diffs churn zero (numstat-style).
    assert_eq!(generated.churn_added, 2);
    assert_eq!(generated.churn_removed, 0);
}
