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

#[test]
fn dead_path_reuse_after_rename_stays_out_of_the_lineage() {
    // a.rs is renamed to b.rs; later an unrelated a.rs is created and
    // deleted again before head. The temporary file's history must not
    // be folded into b.rs even though a.rs is absent from the head
    // tree.
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

    // Temporary unrelated reuse of the a.rs path, dead before head.
    std::fs::write(dir.path().join("a.rs"), "fn temporary() {}\n").unwrap();
    git(dir.path(), &["add", "a.rs"], BOB, T_MAR);
    git(dir.path(), &["commit", "-q", "-m", "temp a"], BOB, T_MAR);
    git(dir.path(), &["rm", "-q", "a.rs"], BOB, T_APR);
    git(
        dir.path(),
        &["commit", "-q", "-m", "remove temp a"],
        BOB,
        T_APR,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // b.rs carries only its own lineage: creation + rename by alice.
    let b = history.file(Path::new("b.rs")).expect("b.rs history");
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 3);
    assert_eq!(b.churn_removed, 0);
    assert_eq!(b.authors, 1, "bob's dead temp file must not leak in");
    assert_eq!(b.last_change_seconds, T_FEB);
}

#[test]
fn empty_blob_additions_and_deletions_do_not_pair_as_renames() {
    // Deleting an empty old/a.rs while independently adding an empty
    // new/b.rs must stay a deletion + addition: an empty blob carries
    // no identity signal, and pairing would hand the new path the old
    // path's baseline and history.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::create_dir_all(dir.path().join("old")).unwrap();
    std::fs::write(dir.path().join("old/a.rs"), "").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "empty-base"], ALICE, T_JAN);

    std::fs::remove_file(dir.path().join("old/a.rs")).unwrap();
    std::fs::create_dir_all(dir.path().join("new")).unwrap();
    std::fs::write(dir.path().join("new/b.rs"), "").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(dir.path(), &["commit", "-q", "-m", "shuffle"], ALICE, T_FEB);
    git(dir.path(), &["tag", "empty-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "empty-base", "empty-head").unwrap();

    let mut summary: Vec<(String, mehen_git::ChangeStatus)> = changed
        .iter()
        .map(|cf| (cf.path.display().to_string(), cf.status))
        .collect();
    summary.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        summary,
        vec![
            ("new/b.rs".to_string(), mehen_git::ChangeStatus::Added),
            ("old/a.rs".to_string(), mehen_git::ChangeStatus::Deleted),
        ],
        "empty blobs must not pair as renames"
    );
    assert!(changed.iter().all(|cf| cf.source_path.is_none()));
}

#[test]
fn exact_rename_pairs_use_global_affinity_ranking() {
    // Identical blobs: deletions at src/a/foo.rs + tests/b/foo.rs,
    // additions at new/foo.rs + src/c/foo.rs. A greedy per-destination
    // match in lexical order would give new/foo.rs the src/a source;
    // global ranking must assign the strongly prefix-matching
    // src/a/foo.rs → src/c/foo.rs pair first.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let same_content = "fn identical_everywhere() {}\n";
    for parent in ["src/a", "tests/b"] {
        std::fs::create_dir_all(dir.path().join(parent)).unwrap();
        std::fs::write(dir.path().join(parent).join("foo.rs"), same_content).unwrap();
    }
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "affinity-base"], ALICE, T_JAN);

    for parent in ["src/a", "tests/b"] {
        std::fs::remove_file(dir.path().join(parent).join("foo.rs")).unwrap();
    }
    for parent in ["new", "src/c"] {
        std::fs::create_dir_all(dir.path().join(parent)).unwrap();
        std::fs::write(dir.path().join(parent).join("foo.rs"), same_content).unwrap();
    }
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "reshuffle"],
        ALICE,
        T_FEB,
    );
    git(dir.path(), &["tag", "affinity-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "affinity-base", "affinity-head").unwrap();

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
    assert_eq!(source_of("src/c/foo.rs"), "src/a/foo.rs");
    assert_eq!(source_of("new/foo.rs"), "tests/b/foo.rs");
}

#[test]
fn prior_occupants_of_a_rename_destination_stay_out_of_the_lineage() {
    // An old b.rs existed and was deleted; later an unrelated a.rs is
    // renamed onto the b.rs path. The current b.rs must carry only the
    // a.rs lineage — not the dead prior occupant's commits.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    // Prior occupant of b.rs, by bob, dead by T_FEB.
    std::fs::write(dir.path().join("b.rs"), "fn prior() {}\nfn occupant() {}\n").unwrap();
    git(dir.path(), &["add", "b.rs"], BOB, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "old b"], BOB, T_JAN);
    git(dir.path(), &["rm", "-q", "b.rs"], BOB, T_FEB);
    git(
        dir.path(),
        &["commit", "-q", "-m", "drop old b"],
        BOB,
        T_FEB,
    );

    // Unrelated a.rs lineage by alice, renamed onto the b.rs path.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "a.rs"], ALICE, T_MAR);
    git(dir.path(), &["commit", "-q", "-m", "add a"], ALICE, T_MAR);
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, T_APR);
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename a onto b"],
        ALICE,
        T_APR,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let b = history.file(Path::new("b.rs")).expect("b.rs history");
    // Only the a.rs lineage: creation + rename, alice alone; bob's
    // dead prior occupant (2 commits, 2 added + 2 removed lines) must
    // not leak into the surviving file.
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 3);
    assert_eq!(b.churn_removed, 0);
    assert_eq!(b.authors, 1, "prior occupant's author must not leak in");
    assert_eq!(b.last_change_seconds, T_APR);
}

#[test]
fn same_commit_content_swaps_are_reported_as_renames() {
    // Swapping two files through a temporary name leaves the endpoint
    // tree with two dissimilar Modified entries and no additions or
    // deletions; the exact cross-match of their blobs must still be
    // recovered as a pair of renames.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let first = "fn first_impl() {}\nfn first_helper() {}\n";
    let second = "const SECOND: &str = \"completely different\";\n";
    std::fs::write(dir.path().join("first.rs"), first).unwrap();
    std::fs::write(dir.path().join("second.rs"), second).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "content-swap-base"], ALICE, T_JAN);

    // Swap the contents (as `git mv` through a temp name would).
    std::fs::write(dir.path().join("first.rs"), second).unwrap();
    std::fs::write(dir.path().join("second.rs"), first).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_FEB);
    git(dir.path(), &["commit", "-q", "-m", "swap"], ALICE, T_FEB);
    git(dir.path(), &["tag", "content-swap-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed =
        mehen_git::changed_files(&repo, "content-swap-base", "content-swap-head").unwrap();

    let source_of = |dest: &str| -> String {
        changed
            .iter()
            .find(|cf| cf.path == Path::new(dest))
            .unwrap_or_else(|| panic!("missing {dest} in {changed:?}"))
            .source_path
            .as_ref()
            .unwrap_or_else(|| panic!("{dest} must be a rename in {changed:?}"))
            .display()
            .to_string()
    };
    assert_eq!(source_of("first.rs"), "second.rs");
    assert_eq!(source_of("second.rs"), "first.rs");
}

#[test]
fn delete_then_recreate_without_rename_splits_the_lineage() {
    // x.rs is written and edited by bob, deleted, then an unrelated
    // x.rs is created by alice. The current file must not inherit the
    // dead prior occupant's churn, authors, or commit count.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(dir.path().join("x.rs"), "fn old() {}\n").unwrap();
    git(dir.path(), &["add", "x.rs"], BOB, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "old x"], BOB, T_JAN);
    std::fs::write(dir.path().join("x.rs"), "fn old() {}\nfn more() {}\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow old x"],
        BOB,
        T_FEB,
    );
    git(dir.path(), &["rm", "-q", "x.rs"], BOB, T_MAR);
    git(
        dir.path(),
        &["commit", "-q", "-m", "drop old x"],
        BOB,
        T_MAR,
    );

    std::fs::write(dir.path().join("x.rs"), "const NEW_WORLD: u8 = 1;\n").unwrap();
    git(dir.path(), &["add", "x.rs"], ALICE, T_APR);
    git(dir.path(), &["commit", "-q", "-m", "new x"], ALICE, T_APR);

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();
    let x = history.file(Path::new("x.rs")).expect("x.rs history");
    assert_eq!(x.commit_frequency, 1);
    assert_eq!(x.churn_added, 1);
    assert_eq!(x.churn_removed, 0);
    assert_eq!(x.authors, 1, "bob's dead occupant must not leak in");
    assert_eq!(x.last_change_seconds, T_APR);
}

#[test]
fn same_commit_swaps_keep_each_lineage_with_its_content() {
    // first.rs and second.rs exchange contents in one commit. Each
    // current path must carry the history of the content now living
    // there — and neither may end up empty or double-counted.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    // first.rs: 2 commits by alice; second.rs: 1 commit by bob.
    std::fs::write(dir.path().join("first.rs"), "fn first() {}\n").unwrap();
    std::fs::write(
        dir.path().join("second.rs"),
        "const SECOND: &str = \"other\";\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    std::fs::write(
        dir.path().join("first.rs"),
        "fn first() {}\nfn first_more() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow first"],
        ALICE,
        T_FEB,
    );

    // Swap contents in one commit (as `git mv` via a temp name would).
    let first_content = std::fs::read(dir.path().join("first.rs")).unwrap();
    let second_content = std::fs::read(dir.path().join("second.rs")).unwrap();
    std::fs::write(dir.path().join("first.rs"), &second_content).unwrap();
    std::fs::write(dir.path().join("second.rs"), &first_content).unwrap();
    git(dir.path(), &["commit", "-q", "-am", "swap"], BOB, T_MAR);

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // second.rs now hosts the old first.rs content: 2 pre-swap commits
    // + the swap = 3, with both authors.
    let second = history.file(Path::new("second.rs")).expect("second");
    assert_eq!(second.commit_frequency, 3);
    assert_eq!(second.authors, 2);
    // first.rs now hosts the old second.rs content: 1 pre-swap commit
    // + the swap = 2.
    let first = history.file(Path::new("first.rs")).expect("first");
    assert_eq!(first.commit_frequency, 2);
    assert_eq!(first.authors, 2);
}

#[test]
fn renaming_back_to_an_old_path_reconnects_the_lineage() {
    // a.rs → b.rs → a.rs: the file returns to its original path. The
    // destination boundary installed by the return rename must not
    // fence off the file's own pre-rename history.
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
    git(dir.path(), &["commit", "-q", "-m", "to b"], ALICE, T_FEB);

    git(dir.path(), &["mv", "b.rs", "a.rs"], BOB, T_MAR);
    git(dir.path(), &["commit", "-q", "-m", "back to a"], BOB, T_MAR);

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let a = history.file(Path::new("a.rs")).expect("a.rs history");
    // Full lineage: creation + both renames.
    assert_eq!(a.commit_frequency, 3);
    assert_eq!(a.churn_added, 3, "the original creation must survive");
    assert_eq!(a.authors, 2);
    assert_eq!(a.last_change_seconds, T_MAR);
    assert!(history.file(Path::new("b.rs")).is_none());
}

#[test]
fn edited_swaps_are_recovered_as_renames() {
    // Two files exchange paths *and* each picks up a small edit in the
    // same commit — no exact OID cross-match exists, but each new blob
    // is far more similar to the other path's baseline than to its
    // own. Both must be reported as renames.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    let alpha: Vec<String> = (0..10).map(|i| format!("fn alpha_{i}() {{}}")).collect();
    let omega: Vec<String> = (0..10)
        .map(|i| format!("const OMEGA_{i}: u8 = {i};"))
        .collect();
    std::fs::write(dir.path().join("alpha.rs"), alpha.join("\n") + "\n").unwrap();
    std::fs::write(dir.path().join("omega.rs"), omega.join("\n") + "\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, T_JAN);
    git(dir.path(), &["tag", "edited-swap-base"], ALICE, T_JAN);

    // Swap the contents and edit one line on each side.
    let mut alpha_edited = alpha.clone();
    alpha_edited[0] = "fn alpha_0_edited() {}".to_string();
    let mut omega_edited = omega.clone();
    omega_edited[0] = "const OMEGA_0_EDITED: u8 = 0;".to_string();
    std::fs::write(dir.path().join("alpha.rs"), omega_edited.join("\n") + "\n").unwrap();
    std::fs::write(dir.path().join("omega.rs"), alpha_edited.join("\n") + "\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "edited swap"],
        ALICE,
        T_FEB,
    );
    git(dir.path(), &["tag", "edited-swap-head"], ALICE, T_FEB);

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "edited-swap-base", "edited-swap-head").unwrap();

    let source_of = |dest: &str| -> String {
        changed
            .iter()
            .find(|cf| cf.path == Path::new(dest))
            .unwrap_or_else(|| panic!("missing {dest} in {changed:?}"))
            .source_path
            .as_ref()
            .unwrap_or_else(|| panic!("{dest} must be a rename in {changed:?}"))
            .display()
            .to_string()
    };
    assert_eq!(source_of("alpha.rs"), "omega.rs");
    assert_eq!(source_of("omega.rs"), "alpha.rs");
}

#[test]
fn open_repo_at_reports_repo_not_found_only_outside_repositories() {
    let dir = tempfile::tempdir().unwrap();
    match mehen_git::open_repo_at(dir.path()) {
        Err(mehen_git::GitError::RepoNotFound) => {}
        other => panic!("expected RepoNotFound outside a repository, got {other:?}"),
    }
}

#[test]
fn parallel_branch_deletion_does_not_split_a_surviving_lineage() {
    // One branch edits x.rs (later timestamp) while another deletes it
    // (earlier timestamp); the merge keeps the file. The newest-first
    // walk sees the edit before the deletion — that deletion must not
    // be mistaken for a delete-then-recreate boundary, or the shared
    // creation and older edits would be fenced off the survivor.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, T_JAN);
    git(
        dir.path(),
        &["config", "commit.gpgsign", "false"],
        ALICE,
        T_JAN,
    );

    std::fs::write(
        dir.path().join("x.rs"),
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "x.rs"], ALICE, T_JAN);
    git(dir.path(), &["commit", "-q", "-m", "add x"], ALICE, T_JAN);

    // Side branch deletes x.rs at T_FEB (earlier timestamp).
    git(dir.path(), &["checkout", "-q", "-b", "side"], BOB, T_FEB);
    git(dir.path(), &["rm", "-q", "x.rs"], BOB, T_FEB);
    git(dir.path(), &["commit", "-q", "-m", "drop x"], BOB, T_FEB);

    // Main edits x.rs at T_MAR (later timestamp, walked first).
    git(dir.path(), &["checkout", "-q", "main"], ALICE, T_MAR);
    std::fs::write(
        dir.path().join("x.rs"),
        "fn one_edited() {}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "edit x"], ALICE, T_MAR);

    // Merge keeps main's edited file (resolve the delete/modify
    // conflict in favor of the surviving file).
    let merge = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "--no-commit", "side"])
        .env("GIT_AUTHOR_NAME", ALICE.0)
        .env("GIT_AUTHOR_EMAIL", ALICE.1)
        .env("GIT_COMMITTER_NAME", ALICE.0)
        .env("GIT_COMMITTER_EMAIL", ALICE.1)
        .env("GIT_AUTHOR_DATE", format!("{T_APR} +0000"))
        .env("GIT_COMMITTER_DATE", format!("{T_APR} +0000"))
        .output()
        .expect("failed to run git merge");
    // The delete/modify conflict is expected; keep the modified file.
    drop(merge);
    git(
        dir.path(),
        &["checkout", "HEAD", "--", "x.rs"],
        ALICE,
        T_APR,
    );
    git(dir.path(), &["add", "x.rs"], ALICE, T_APR);
    git(
        dir.path(),
        &["commit", "-q", "-m", "merge side keeping x"],
        ALICE,
        T_APR,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let x = history.file(Path::new("x.rs")).expect("x.rs history");
    // The survivor keeps its creation and edit; the parallel-branch
    // deletion touch is also attributed here (it is lineage, not a
    // boundary). Crucially the creation's 3 added lines survive.
    assert!(
        x.churn_added >= 4,
        "creation (3) + edit (1) must survive, got {}",
        x.churn_added
    );
    assert!(x.commit_frequency >= 3);
    assert_eq!(x.last_change_seconds, T_MAR);
}

/// Tombstone identities live outside the path namespace: a real
/// repository file whose name matches the old in-namespace sentinel
/// (`\x01tombstone\x011`) must keep its own history even when the walk
/// fences off a dead prior occupant with tombstone #1.
#[test]
#[cfg(unix)]
fn tombstones_cannot_collide_with_real_control_byte_paths() {
    let dir = tempfile::tempdir().unwrap();
    let weird = "\u{1}tombstone\u{1}1";
    git(
        dir.path(),
        &["init", "-q", "-b", "main"],
        ALICE,
        1_700_000_000,
    );

    // c1: the control-byte-named file, plus a prior occupant of the
    // future rename destination, plus the future rename source.
    std::fs::write(dir.path().join(weird), "fn w0() {}\nfn w1() {}\n").unwrap();
    std::fs::write(
        dir.path().join("dest.rs"),
        "fn d0() {}\nfn d1() {}\nfn d2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("old.rs"), "fn o0() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, 1_700_000_000);
    git(
        dir.path(),
        &["commit", "-q", "-m", "init"],
        ALICE,
        1_700_000_000,
    );

    // c2: edit the control-byte file; remove the prior occupant.
    std::fs::write(
        dir.path().join(weird),
        "fn w0() {}\nfn w1() {}\nfn w2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["rm", "-q", "dest.rs"], BOB, 1_700_100_000);
    git(dir.path(), &["add", "-A"], BOB, 1_700_100_000);
    git(
        dir.path(),
        &["commit", "-q", "-m", "edit and drop"],
        BOB,
        1_700_100_000,
    );

    // c3: rename onto the freed path — installs a destination
    // boundary, which allocates tombstone #1.
    git(
        dir.path(),
        &["mv", "old.rs", "dest.rs"],
        ALICE,
        1_700_200_000,
    );
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename"],
        ALICE,
        1_700_200_000,
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // The control-byte file saw exactly its own two commits; with the
    // old sentinel scheme the fenced-off `dest.rs` occupant (creation
    // + deletion) would have been merged into it.
    let weird_history = history.file(Path::new(weird)).unwrap();
    assert_eq!(weird_history.commit_frequency, 2);
    assert_eq!(weird_history.churn_added, 3);
    assert_eq!(weird_history.churn_removed, 0);

    // The survivor carries the rename-source lineage only.
    let dest = history.file(Path::new("dest.rs")).unwrap();
    assert_eq!(dest.commit_frequency, 2);
    assert_eq!(dest.churn_added, 1);
}

/// A source path reused and renamed *again*: `a.rs → b.rs`, then an
/// unrelated `a.rs` is created and renamed to `c.rs`. The newest
/// rename's alias is consumed once the walk accumulates the reused
/// file's creation, so the older `a.rs → b.rs` rename must take the
/// alias over — the original lineage belongs to `b.rs`, not `c.rs`.
#[test]
fn older_rename_reclaims_a_source_path_reused_by_a_newer_rename() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1: the original file.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // c2: edit the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));

    // c3: the original moves to b.rs.
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to b"],
        ALICE,
        t(2),
    );

    // c4: an unrelated file reuses the a.rs path.
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "new a"], CAROL, t(3));

    // c5: the reuse moves to c.rs.
    git(dir.path(), &["mv", "a.rs", "c.rs"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to c"],
        CAROL,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // b.rs carries the original lineage: creation, edit, rename.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 3);
    assert_eq!(b.churn_added, 4);

    // c.rs carries only the reuse: creation and rename.
    let c = history.file(Path::new("c.rs")).unwrap();
    assert_eq!(c.commit_frequency, 2);
    assert_eq!(c.churn_added, 1);
}

/// Delete, recreate, rename: the deletion (and everything older) at
/// the reused path belongs to a dead prior occupant and must not leak
/// into the rename survivor through the (already consumed) alias.
#[test]
fn deleted_occupant_of_a_reused_then_renamed_path_stays_fenced_off() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1 + c2: a prior occupant lives and grows at a.rs.
    std::fs::write(dir.path().join("a.rs"), "fn old0() {}\nfn old1() {}\n").unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn old0() {}\nfn old1() {}\nfn old2() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow old a"],
        BOB,
        t(1),
    );

    // c3: the occupant dies.
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], ALICE, t(2));

    // c4: an unrelated file reuses the path.
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "new a"], CAROL, t(3));

    // c5: the reuse moves to c.rs.
    git(dir.path(), &["mv", "a.rs", "c.rs"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to c"],
        CAROL,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // The survivor sees only the reuse's creation and rename — not
    // the dead occupant's creation, edit, or deletion.
    let c = history.file(Path::new("c.rs")).unwrap();
    assert_eq!(c.commit_frequency, 2);
    assert_eq!(c.churn_added, 1);
    assert_eq!(c.churn_removed, 0);
    assert_eq!(c.authors, 1);

    // The dead occupant's history is fenced off behind a tombstone,
    // not reported under the vacated path.
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// Run git and capture trimmed stdout (for plumbing that returns ids).
fn git_out(repo: &Path, args: &[&str], author: (&str, &str), seconds: i64) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// One branch renames `a.rs → b.rs` and re-creates an unrelated
/// `a.rs`; a parallel branch edits the *original* `a.rs` before the
/// merge. Date order can walk both the re-creation and the parallel
/// edit before the rename — ancestry must split them: the re-creation
/// (a descendant of the rename) stays at `a.rs`, the concurrent edit
/// belongs to the renamed lineage at `b.rs`.
#[test]
fn parallel_edits_are_split_from_a_reused_rename_source() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1 (main): the original file.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));
    let c1 = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    // feature branch: rename a.rs → b.rs, then reuse the a.rs path.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "feature"],
        ALICE,
        t(1),
    );
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to b"],
        ALICE,
        t(1),
    );
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(dir.path(), &["commit", "-q", "-m", "reuse a"], CAROL, t(4));
    let feature = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // main (parallel): edit the original a.rs.
    git(dir.path(), &["checkout", "-q", "main"], BOB, t(3));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\nfn a4() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(3));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(3));
    let _ = c1;

    // Merged tree, built directly to sidestep the rename/edit
    // conflict: b.rs absorbs main's edit, the reused a.rs survives.
    git(dir.path(), &["checkout", "-q", "feature"], ALICE, t(5));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\nfn a4() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &feature,
            "-p",
            &main,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = original creation (4 lines) + parallel edit (1 line) +
    // the rename itself. The parallel edit must not stick to the
    // reused a.rs path.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 3);
    assert_eq!(b.churn_added, 5);
    assert_eq!(b.authors, 2, "bob's parallel edit belongs to b.rs");

    // a.rs = only the unrelated re-creation.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 1);
    assert_eq!(a.authors, 1);
}

/// A rename performed *by the merge commit itself* (conflict
/// resolution commits `a.rs` — present in both parents — as `b.rs`)
/// must install identity: without it the older commits accumulate
/// under the vacated `a.rs` while `b.rs` reads an empty history. The
/// merge still contributes no churn of its own.
#[test]
fn merge_commit_renames_establish_file_identity() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1: common ancestor.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // main: edit a.rs.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow on main"],
        BOB,
        t(1),
    );
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // side branch from c1: a different edit to a.rs.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "side", "HEAD~1"],
        CAROL,
        t(2),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn side() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow on side"],
        CAROL,
        t(2),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(2));

    // The merge resolves the conflict by committing the file as b.rs
    // (main's blob, so the tree diff sees an exact rename); a.rs is
    // gone from the merged tree though both parents contain it.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(3));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(3));
    git(dir.path(), &["add", "-A"], ALICE, t(3));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(3));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(3),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs carries the full a.rs lineage: creation + both edits. The
    // merge itself adds no commit and no churn.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 3);
    assert_eq!(b.churn_added, 5);
    assert_eq!(b.authors, 3);

    // Nothing is reported under the vacated path.
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// A commit that renames one file *over* another (`git mv -f a.rs
/// b.rs`) emits `Renamed(a → b)` plus `Deleted(b)` for the old
/// occupant. The deletion must land behind the destination boundary —
/// not in the surviving lineage, where its removed lines, author, and
/// commit would pollute the new `b.rs` history.
#[test]
fn rename_over_an_existing_file_fences_the_old_occupant() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1: two unrelated files.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn alpha0() {}\nfn alpha1() {}\nfn alpha2() {}\nfn alpha3() {}\nfn alpha4() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.rs"),
        "fn beta0() {}\nfn beta1() {}\nfn beta2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // c2: a.rs replaces b.rs.
    git(dir.path(), &["mv", "-f", "a.rs", "b.rs"], BOB, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "replace b with a"],
        BOB,
        t(1),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // b.rs = the a.rs lineage: creation (5 lines) + the rename. The
    // old occupant's deletion (3 removed lines) is fenced off.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 5);
    assert_eq!(b.churn_removed, 0, "old occupant's deletion leaked in");
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// One branch renames `a.rs → b.rs`; a parallel branch deletes the
/// original `a.rs` and re-creates an unrelated file at that path; the
/// merge keeps both. Whatever the walk order, the surviving `a.rs`
/// must keep (only) its own history and the original lineage must
/// flow to `b.rs`. This variant walks the parallel branch *before*
/// the rename (its timestamps are newer).
#[test]
fn concurrent_delete_and_recreate_walked_before_the_rename() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // feature: the rename, with the *oldest* post-branch timestamp.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "feature"],
        ALICE,
        t(1),
    );
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to b"],
        ALICE,
        t(1),
    );
    let feature = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(1));

    // main (parallel): delete the original, then reuse the path.
    git(dir.path(), &["checkout", "-q", "main"], BOB, t(2));
    git(dir.path(), &["rm", "-q", "a.rs"], BOB, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], BOB, t(2));
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "reuse a"], CAROL, t(3));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // Merged tree keeps both files.
    git(dir.path(), &["checkout", "-q", "feature"], ALICE, t(4));
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(4));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &feature,
            "-p",
            &main,
            "-m",
            "merge",
        ],
        ALICE,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The surviving a.rs is only the parallel re-creation.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 1);
    assert_eq!(a.authors, 1);

    // b.rs owns the original lineage: creation, the parallel branch's
    // deletion of the original (reclaimed from its fence when the
    // rename explained where the file went), and the rename.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.churn_added, 3);
    assert_eq!(b.commit_frequency, 3);
}

/// Same topology as above, but the rename carries the *newest*
/// timestamp and is walked first — the parallel branch's re-creation
/// must then bypass the already-installed alias (it is concurrent
/// with the rename, not part of its pre-rename lineage).
#[test]
fn concurrent_delete_and_recreate_walked_after_the_rename() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // main (parallel): delete the original, then reuse the path —
    // with *older* timestamps than the rename.
    git(dir.path(), &["rm", "-q", "a.rs"], BOB, t(1));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], BOB, t(1));
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(2));
    git(dir.path(), &["commit", "-q", "-m", "reuse a"], CAROL, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(2));

    // feature from the root commit: the rename, newest timestamp.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "feature", "HEAD~2"],
        ALICE,
        t(3),
    );
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to b"],
        ALICE,
        t(3),
    );
    let feature = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(3));

    // Merged tree keeps both files.
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(4));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &feature,
            "-p",
            &main,
            "-m",
            "merge",
        ],
        ALICE,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The surviving a.rs is only the parallel re-creation — the
    // alias installed by the earlier-walked rename must not swallow
    // a concurrent addition.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 1);
    assert_eq!(a.authors, 1);

    // b.rs owns the original lineage: creation + rename. The
    // concurrent deletion of the original stays fenced in this
    // ordering (the fence postdates the already-walked rename).
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.churn_added, 3);
    assert_eq!(b.commit_frequency, 2);
}

/// A merge-introduced rename whose source exists only in a
/// *non-first* parent: the first-parent diff sees the destination as
/// a plain addition, so the merge diff must be taken against every
/// parent to pair the rename and install identity.
#[test]
fn merge_renames_of_non_first_parent_files_establish_identity() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1: common ancestor without the file.
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    // side branch: create and grow a.rs — main never has it.
    git(dir.path(), &["checkout", "-q", "-b", "side"], BOB, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(1));
    git(dir.path(), &["commit", "-q", "-m", "add a"], BOB, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], CAROL, t(2));
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(2));

    // The merge (first parent = main) commits side's file as b.rs;
    // neither parent contains b.rs.
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(3));
    git(dir.path(), &["add", "-A"], ALICE, t(3));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(3));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(3),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs carries side's full a.rs lineage.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.authors, 2);
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// One commit renames `a → b` *and* creates a replacement `a`; the
/// replacement is later deleted and the path re-created. The
/// replacement's creation resolves into the later deletion's fence in
/// the same commit as the rename — that in-use fence must not be
/// reclaimed into `b`, and the fresh `a → b` alias must not be
/// mistaken for the entry the replacement consumed.
#[test]
fn same_commit_replacement_does_not_corrupt_the_rename_alias() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1: the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // c2: rename to b.rs and create a replacement a.rs, one commit.
    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(1));
    std::fs::write(dir.path().join("a.rs"), "fn repl0() {}\nfn repl1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "split a into b and new a"],
        BOB,
        t(1),
    );

    // c3: the replacement dies.
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "drop replacement"],
        ALICE,
        t(2),
    );

    // c4: an unrelated file re-creates the path.
    std::fs::write(dir.path().join("a.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "reuse a"], CAROL, t(3));

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // b.rs = the original lineage only: creation + rename. Neither
    // the replacement's churn nor its deletion may leak in.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 3);
    assert_eq!(b.churn_removed, 0);

    // The final a.rs is only the last re-creation.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 1);
    assert_eq!(a.authors, 1);
}

/// Two branches rename the same source to different destinations and
/// the merge keeps both: each branch's pre-rename edits must route to
/// that branch's survivor (ancestry-scoped aliases), with the shared
/// pre-branch lineage counted once toward the first-walked rename.
#[test]
fn concurrent_renames_to_different_destinations_both_keep_their_edits() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c1: common ancestor.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // branch one: edit, then rename to b.rs.
    git(dir.path(), &["checkout", "-q", "-b", "one"], BOB, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn b_edit() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "edit on one"],
        BOB,
        t(1),
    );
    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(2));
    git(dir.path(), &["commit", "-q", "-m", "move to b"], BOB, t(2));
    let one = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // branch two: a different edit, then rename to c.rs — with newer
    // timestamps, so its rename is walked first.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "two", "main"],
        CAROL,
        t(3),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn c_edit0() {}\nfn c_edit1() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "edit on two"],
        CAROL,
        t(3),
    );
    git(dir.path(), &["mv", "a.rs", "c.rs"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to c"],
        CAROL,
        t(4),
    );
    let two = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // Merge keeps both survivors.
    git(dir.path(), &["checkout", "-q", "one"], ALICE, t(5));
    std::fs::write(
        dir.path().join("c.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn c_edit0() {}\nfn c_edit1() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &["commit-tree", &tree, "-p", &one, "-p", &two, "-m", "merge"],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // First-walked rename (two → c.rs, newest timestamps) gets the
    // shared pre-branch lineage plus its own edit.
    let c = history.file(Path::new("c.rs")).unwrap();
    assert_eq!(c.commit_frequency, 3);
    assert_eq!(c.churn_added, 5);

    // The other survivor still owns its branch's pre-rename edit —
    // previously the second-visited rename was rejected outright and
    // this edit stayed keyed to the obsolete a.rs.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 1);

    assert!(history.file(Path::new("a.rs")).is_none());
}

/// Non-UTF-8 paths keep their raw bytes: `x\xff.py` must not collide
/// with a real file literally named `x\u{FFFD}.py` (the lossy
/// replacement of the invalid byte).
#[test]
#[cfg(unix)]
fn non_utf8_paths_do_not_collide_with_replacement_character_paths() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));
    git(
        dir.path(),
        &["config", "core.quotePath", "false"],
        ALICE,
        t(0),
    );

    let weird = std::ffi::OsStr::from_bytes(b"x\xff.py");
    let lookalike = "x\u{FFFD}.py";

    // Not every Unix filesystem accepts non-UTF-8 names (macOS APFS
    // rejects the invalid byte with EILSEQ): the collision scenario
    // cannot exist there, so there is nothing to test.
    if std::fs::write(dir.path().join(weird), "w0 = 1\nw1 = 2\n").is_err() {
        return;
    }
    std::fs::write(dir.path().join(lookalike), "l0 = 1\nl1 = 2\nl2 = 3\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    std::fs::write(
        dir.path().join(lookalike),
        "l0 = 1\nl1 = 2\nl2 = 3\nl3 = 4\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow lookalike"],
        BOB,
        t(1),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // Each file reports exactly its own history — with lossy path
    // conversion both would fold into one identity.
    let w = history.file(Path::new(weird)).unwrap();
    assert_eq!(w.commit_frequency, 1);
    assert_eq!(w.churn_added, 2);

    let l = history.file(Path::new(lookalike)).unwrap();
    assert_eq!(l.commit_frequency, 2);
    assert_eq!(l.churn_added, 4);
}

/// A merge that *creates* a file at a path absent from every parent
/// (conflict resolution) establishes a fresh identity: a dead prior
/// occupant of that path must stay fenced off instead of donating its
/// creation, edits, and deletion to the merge-created file.
#[test]
fn merge_created_additions_fence_dead_prior_occupants() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // The prior occupant lives and dies on main.
    std::fs::write(
        dir.path().join("victim.rs"),
        "fn v0() {}\nfn v1() {}\nfn v2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));
    std::fs::write(
        dir.path().join("victim.rs"),
        "fn v0() {}\nfn v1() {}\nfn v2() {}\nfn v3() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow victim"],
        BOB,
        t(1),
    );
    git(dir.path(), &["rm", "-q", "victim.rs"], ALICE, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "drop victim"],
        ALICE,
        t(2),
    );

    // A side branch, so the merge has two parents.
    git(dir.path(), &["checkout", "-q", "-b", "side"], CAROL, t(3));
    std::fs::write(dir.path().join("side.rs"), "fn side() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "side work"],
        CAROL,
        t(3),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(4));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow keep"],
        ALICE,
        t(4),
    );
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(4));

    // The merge re-creates victim.rs — absent from both parents.
    std::fs::write(dir.path().join("side.rs"), "fn side() {}\n").unwrap();
    std::fs::write(
        dir.path().join("victim.rs"),
        "fn reborn0() {}\nfn reborn1() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The dead occupant's creation, edit, and deletion must not be
    // attributed to the merge-created file (which itself accumulates
    // nothing — merge churn stays excluded).
    assert!(history.file(Path::new("victim.rs")).is_none());

    // Sanity: unrelated files keep their history.
    assert_eq!(
        history.file(Path::new("side.rs")).unwrap().commit_frequency,
        1
    );
}

/// Author identities preserve raw bytes: two emails differing only in
/// an invalid UTF-8 byte must stay two distinct authors — a lossy
/// conversion would collapse both to the same `U+FFFD` string.
#[test]
#[cfg(unix)]
fn non_utf8_author_emails_stay_distinct() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    let commit = |file_content: &str, msg: &str, email: &[u8], seconds: i64| {
        std::fs::write(dir.path().join("a.py"), file_content).unwrap();
        let date = format!("{seconds} +0000");
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", msg]] {
            let output = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(&args)
                .env("GIT_AUTHOR_NAME", "Weird")
                .env("GIT_AUTHOR_EMAIL", OsStr::from_bytes(email))
                .env("GIT_COMMITTER_NAME", "Weird")
                .env("GIT_COMMITTER_EMAIL", OsStr::from_bytes(email))
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
    };

    // Two identities differing only in the invalid byte: lossy
    // conversion maps both to "a\u{FFFD}x@example.com".
    commit("x = 1\n", "one", b"a\xffx@example.com", t(0));
    commit("x = 1\ny = 2\n", "two", b"a\xfex@example.com", t(1));

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let a = history.file(Path::new("a.py")).unwrap();
    assert_eq!(a.commit_frequency, 2);
    assert_eq!(a.authors, 2, "distinct non-UTF-8 emails collapsed");
    // Each contributed half the added lines: no 100% owner.
    assert!(
        (a.ownership - 0.5).abs() < 1e-9,
        "ownership {}",
        a.ownership
    );
}

/// A merge that resolves a path by *deleting* it must still let the
/// delete-then-recreate boundary fire: when a later commit creates an
/// unrelated file at that path, the pre-merge occupant's history must
/// not leak into it.
#[test]
fn merge_performed_deletions_fence_recreated_paths() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // The occupant lives on main.
    std::fs::write(
        dir.path().join("p.rs"),
        "fn p0() {}\nfn p1() {}\nfn p2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));
    std::fs::write(
        dir.path().join("p.rs"),
        "fn p0() {}\nfn p1() {}\nfn p2() {}\nfn p3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow p"], BOB, t(1));

    // A side branch, so the merge has two parents.
    git(dir.path(), &["checkout", "-q", "-b", "side"], CAROL, t(2));
    std::fs::write(dir.path().join("side.rs"), "fn side() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "side work"],
        CAROL,
        t(2),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(2));

    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(3));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow keep"],
        ALICE,
        t(3),
    );
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(3));

    // The merge resolves p.rs away: present in both parents, absent
    // from the merged tree.
    git(dir.path(), &["rm", "-q", "p.rs"], ALICE, t(4));
    std::fs::write(dir.path().join("side.rs"), "fn side() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(4));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(4),
    );
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &merge],
        ALICE,
        t(4),
    );
    git(dir.path(), &["checkout", "-q", "-f", "main"], ALICE, t(4));

    // A later commit reuses the path for an unrelated file.
    std::fs::write(dir.path().join("p.rs"), "fn unrelated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(5));
    git(dir.path(), &["commit", "-q", "-m", "reuse p"], CAROL, t(5));

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // Only the re-creation: the pre-merge occupant's creation and
    // edit stay fenced behind the merge-performed deletion.
    let p = history.file(Path::new("p.rs")).unwrap();
    assert_eq!(p.commit_frequency, 1);
    assert_eq!(p.churn_added, 1);
    assert_eq!(p.authors, 1);
}

/// A merge-introduced rename must be scoped to the parents whose
/// trees contain the source: with the merge commit as the scope, an
/// unrelated file that lived and died at the same path on *another*
/// parent's line would resolve through the alias and corrupt the
/// survivor's history.
#[test]
fn merge_rename_aliases_are_scoped_to_the_supplying_parent() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: common ancestor without a.rs.
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: the real file is born and grows (older timestamps, so
    // this line is walked *after* the side branch).
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(dir.path(), &["commit", "-q", "-m", "create a"], ALICE, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\nfn orig3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // side (from the root): an unrelated a.rs lives and dies — with
    // *newer* timestamps, so it is walked before the merge's alias
    // would find the real lineage.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "side", "HEAD~2"],
        CAROL,
        t(3),
    );
    std::fs::write(dir.path().join("a.rs"), "fn other0() {}\nfn other1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "unrelated a"],
        CAROL,
        t(4),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(5));
    git(
        dir.path(),
        &["commit", "-q", "-m", "drop unrelated a"],
        CAROL,
        t(5),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(5));

    // The merge resolves main's a.rs as b.rs (absent from both
    // parents); a.rs is gone from the merged tree.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(6));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(6));
    git(dir.path(), &["add", "-A"], ALICE, t(6));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(6));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(6),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = main's lineage only: creation + growth. The side
    // branch's unrelated file (2 commits, 2 added, 2 removed, carol)
    // must not leak in nor consume the alias.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.churn_removed, 0);
    assert_eq!(b.authors, 2, "only alice and bob touched the lineage");
}

/// Coupling cardinality counts every changed leaf path — a commit
/// touching one source file plus a symlink couples them, even though
/// the symlink carries no analyzable text.
#[test]
#[cfg(unix)]
fn coupling_counts_non_blob_changeset_members() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(dir.path().join("code.rs"), "fn a() {}\n").unwrap();
    std::os::unix::fs::symlink("code.rs", dir.path().join("link")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));

    // Change both the file and the symlink target in one commit.
    std::fs::write(dir.path().join("code.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::remove_file(dir.path().join("link")).unwrap();
    std::os::unix::fs::symlink("other", dir.path().join("link")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "grow both"],
        ALICE,
        t(1),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // Each commit's changeset is {code.rs, link}: one coupled other
    // per commit — previously the symlink was invisible and soc was 0.
    let code = history.file(Path::new("code.rs")).unwrap();
    assert_eq!(code.sum_of_coupling, 2);
}

/// A merge retains one parent's *independently created* `a.rs` while
/// moving the other parent's original to `b.rs`. The rename alias
/// must be scoped by lineage, not bare path existence: the retained
/// occupant's commits belong to the surviving `a.rs`, not to `b.rs`.
#[test]
fn merge_rename_scopes_exclude_independently_created_occupants() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: common ancestor without a.rs.
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: the original file (older timestamps — walked last).
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(dir.path(), &["commit", "-q", "-m", "create a"], ALICE, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\nfn orig3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // indep (from the root, which never had a.rs): its own a.rs —
    // newer timestamps, walked before main's line.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "indep", "HEAD~2"],
        CAROL,
        t(3),
    );
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(dir.path(), &["commit", "-q", "-m", "own a"], CAROL, t(4));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn own0() {}\nfn own1() {}\nfn own2() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow own a"],
        CAROL,
        t(5),
    );
    let indep = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(5));

    // The merge keeps indep's a.rs and moves main's original to b.rs.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(6));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(6));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn own0() {}\nfn own1() {}\nfn own2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(6));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(6));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &indep,
            "-m",
            "merge",
        ],
        ALICE,
        t(6),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = the original lineage only.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.authors, 2, "only alice and bob wrote the original");

    // The surviving a.rs keeps the independent creator's history.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 2);
    assert_eq!(a.churn_added, 3);
    assert_eq!(a.authors, 1, "carol's file stays carol's");
}

/// A transient blob life inside the range must not promote a path
/// that is a symlink at both endpoints: there is no analyzable text
/// to hang a diff row on.
#[test]
#[cfg(unix)]
fn range_touched_files_require_blob_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(dir.path().join("real.py"), "x = 1\n").unwrap();
    std::os::unix::fs::symlink("real.py", dir.path().join("alias.py")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, t(0));
    git(dir.path(), &["tag", "sym-base"], ALICE, t(0));

    // The symlink briefly becomes a regular file…
    std::fs::remove_file(dir.path().join("alias.py")).unwrap();
    std::fs::write(dir.path().join("alias.py"), "y = 2\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "materialize"],
        ALICE,
        t(1),
    );

    // …and is restored.
    std::fs::remove_file(dir.path().join("alias.py")).unwrap();
    std::os::unix::fs::symlink("real.py", dir.path().join("alias.py")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "restore"], ALICE, t(2));
    git(dir.path(), &["tag", "sym-head"], ALICE, t(2));

    let repo = gix::discover(dir.path()).unwrap();
    let touched = mehen_git::range_touched_files(&repo, "sym-base", "sym-head").unwrap();
    assert!(
        touched.is_empty(),
        "symlink-at-both-endpoints paths must not surface: {touched:?}"
    );
}

/// A parent that deleted and *recreated* the source path after the
/// merge base must be excluded from a merge rename's scopes when the
/// merge retains its recreated version at the path: the retained
/// occupant survives where it is, and its commits must not resolve
/// into the rename target nor consume the alias.
#[test]
fn merge_rename_scopes_exclude_retained_delete_and_recreate_occupants() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original a.rs exists at the (future) merge base.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: grow the original (older timestamps — walked last).
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\nfn orig3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // dr branch (from the base): delete the original, recreate an
    // unrelated file at the path — newer timestamps, walked first.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "dr", "HEAD~1"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], CAROL, t(3));
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a"],
        CAROL,
        t(4),
    );
    let dr = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge keeps dr's a.rs and moves main's original to b.rs.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(5));
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &["commit-tree", &tree, "-p", &main, "-p", &dr, "-m", "merge"],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = the original lineage: root creation + main's growth.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.churn_removed, 0);

    // The surviving a.rs is only carol's recreation; her deletion of
    // the original stays fenced.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 2);
    assert_eq!(a.authors, 1);
}

/// A merge replacing a symlink with a regular blob at the same path
/// creates a *new* file identity: an older regular file that occupied
/// the path before it became a symlink stays fenced instead of
/// donating its history to the merge-created blob.
#[test]
#[cfg(unix)]
fn merge_created_blob_over_symlink_fences_the_old_occupant() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // The old regular occupant lives and grows…
    std::fs::write(dir.path().join("alias.py"), "old0 = 1\nold1 = 2\n").unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "init"], ALICE, t(0));
    std::fs::write(
        dir.path().join("alias.py"),
        "old0 = 1\nold1 = 2\nold2 = 3\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow"], BOB, t(1));

    // …then the path becomes a symlink.
    std::fs::remove_file(dir.path().join("alias.py")).unwrap();
    std::os::unix::fs::symlink("keep.rs", dir.path().join("alias.py")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "symlinkify"],
        ALICE,
        t(2),
    );

    // A side branch so the merge has two parents (both hold the
    // symlink).
    git(dir.path(), &["checkout", "-q", "-b", "side"], CAROL, t(3));
    std::fs::write(dir.path().join("side.rs"), "fn side() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "side"], CAROL, t(3));
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(4));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "grow keep"],
        ALICE,
        t(4),
    );
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(4));

    // The merge replaces the symlink with a brand-new regular blob.
    std::fs::remove_file(dir.path().join("alias.py")).unwrap();
    std::fs::write(dir.path().join("alias.py"), "reborn = 1\n").unwrap();
    std::fs::write(dir.path().join("side.rs"), "fn side() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The merge-created blob inherits nothing from the pre-symlink
    // occupant (its creation, growth, and blob-side deletion at the
    // symlinkify commit all stay fenced).
    assert!(history.file(Path::new("alias.py")).is_none());
    assert_eq!(
        history.file(Path::new("side.rs")).unwrap().commit_frequency,
        1
    );
}

/// A parent whose delete-and-recreated `a.rs` is retained by the
/// merge *with conflict-resolution edits* (so its blob differs from
/// the parent's) must still be excluded from the rename scopes: a
/// surviving blob at the source path means the moved lineage is the
/// supplier's alone.
#[test]
fn merge_rename_scopes_exclude_edited_retained_occupants() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original a.rs exists at the (future) merge base.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: grow the original (older timestamps — walked last).
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\nfn orig3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // dr branch: delete + recreate an unrelated file at the path.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "dr", "HEAD~1"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], CAROL, t(3));
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a"],
        CAROL,
        t(4),
    );
    let dr = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge keeps dr's recreation *with an extra edit* (blob no
    // longer byte-identical to dr's) and moves main's original.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(5));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn own0() {}\nfn own1() {}\nfn merged_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &["commit-tree", &tree, "-p", &main, "-p", &dr, "-m", "merge"],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = the original lineage only.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.churn_removed, 0);

    // The surviving a.rs keeps carol's recreation.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 2);
    assert_eq!(a.authors, 1);
}

/// Two branches rename the shared base file differently and the merge
/// commits the survivor under a third name: both intermediate-path
/// lineages must converge on the survivor instead of the second
/// pairing being dropped.
#[test]
fn converging_merge_renames_preserve_every_parent_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the shared original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: rename to x.rs and edit it.
    git(dir.path(), &["mv", "a.rs", "x.rs"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to x"],
        ALICE,
        t(1),
    );
    std::fs::write(
        dir.path().join("x.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn x_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow x"], BOB, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // side: rename to y.rs and edit it differently.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "side", "HEAD~2"],
        CAROL,
        t(3),
    );
    git(dir.path(), &["mv", "a.rs", "y.rs"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "move to y"],
        CAROL,
        t(3),
    );
    std::fs::write(
        dir.path().join("y.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn y_edit0() {}\nfn y_edit1() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow y"], CAROL, t(4));
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge commits the survivor as z.rs (main's blob, exact for
    // the x-side pairing; y pairs by similarity).
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "x.rs", "z.rs"], ALICE, t(5));
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // z.rs carries the shared creation, both renames, and both
    // branches' edits — the y-side lineage must not be stranded.
    let z = history.file(Path::new("z.rs")).unwrap();
    assert_eq!(z.commit_frequency, 5);
    assert_eq!(z.churn_added, 6);
    assert_eq!(z.authors, 3);
    assert!(history.file(Path::new("x.rs")).is_none());
    assert!(history.file(Path::new("y.rs")).is_none());
}

/// A merge rename onto a destination another parent already owns:
/// conflict resolution carries the `a.rs` lineage into the merged
/// `b.rs` (content from `a.rs`, not the retained parent blob). The
/// alias must install — and without a destination boundary, so the
/// owning parent's legitimate `b.rs` history keeps converging.
#[test]
fn merge_renames_onto_parent_owned_destinations_install_identity() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: common ancestor.
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: a.rs is born and grows.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(dir.path(), &["commit", "-q", "-m", "create a"], ALICE, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // side: an unrelated b.rs is born.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "side", "HEAD~2"],
        CAROL,
        t(3),
    );
    std::fs::write(dir.path().join("b.rs"), "fn b_own() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(dir.path(), &["commit", "-q", "-m", "create b"], CAROL, t(4));
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge resolves a.rs *into* b.rs: the merged b.rs holds
    // main's a.rs content (≠ side's b.rs blob), and a.rs is gone.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(5));
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs absorbs both lineages: a.rs's creation + growth (via the
    // alias) and side's own b.rs creation (no boundary fences it).
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 3);
    assert_eq!(b.churn_added, 5);
    assert_eq!(b.authors, 3);
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// A parent that deleted the base file and re-created an unrelated
/// one — whose re-creation the merge then *discards* — must not enter
/// the rename scopes: path existence at the merge base is not
/// lineage continuity.
#[test]
fn merge_rename_scopes_exclude_discarded_recreated_sources() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original a.rs exists at the (future) merge base.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: grow the original (older timestamps — walked last).
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\nfn orig3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // dr branch: delete + recreate an unrelated file (newer
    // timestamps — walked first).
    git(
        dir.path(),
        &["checkout", "-q", "-b", "dr", "HEAD~1"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], CAROL, t(3));
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a"],
        CAROL,
        t(4),
    );
    let dr = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge discards the recreation entirely and moves main's
    // original to b.rs — no blob survives at a.rs.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(5));
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &["commit-tree", &tree, "-p", &main, "-p", &dr, "-m", "merge"],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = the original lineage only: the discarded recreation's
    // commits must neither route into it nor consume the alias.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.churn_removed, 0);
    assert_eq!(b.authors, 2);
}

/// A branch that deletes the base file and re-creates it with
/// byte-identical contents still crossed an identity boundary:
/// endpoint blobs cannot see the interruption, so the range walk
/// must — the parent stays out of the rename scopes.
#[test]
fn merge_rename_scopes_detect_exact_recreations_via_range_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    let base_content = "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\n";
    std::fs::write(dir.path().join("a.rs"), base_content).unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: grow the original (older timestamps — walked last).
    std::fs::write(
        dir.path().join("a.rs"),
        "fn orig0() {}\nfn orig1() {}\nfn orig2() {}\nfn orig3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // dr branch: delete, then recreate with the *exact base bytes*.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "dr", "HEAD~1"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(3));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], CAROL, t(3));
    std::fs::write(dir.path().join("a.rs"), base_content).unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a exactly"],
        CAROL,
        t(4),
    );
    let dr = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge discards the recreation and moves main's original.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(5));
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &["commit-tree", &tree, "-p", &main, "-p", &dr, "-m", "merge"],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = the original lineage only; the byte-identical recreation
    // neither routes into it nor consumes the alias (which would
    // strand the shared pre-branch history).
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
    assert_eq!(b.authors, 2);
}

/// A destination the merge keeps *and edits*: the merged blob is no
/// longer byte-identical to the owning parent's, but it continues
/// that parent's file — a similar discarded source must not be
/// declared the winner and merged into it.
#[test]
fn merge_retention_recognizes_edited_destinations() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: common ancestor.
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: b.rs is born and grows.
    std::fs::write(
        dir.path().join("b.rs"),
        "fn s0() {}\nfn s1() {}\nfn s2() {}\nfn s3() {}\nfn s4() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(dir.path(), &["commit", "-q", "-m", "create b"], ALICE, t(1));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn s0() {}\nfn s1() {}\nfn s2() {}\nfn s3() {}\nfn s4() {}\nfn s5() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow b"], BOB, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // side: a similar a.rs (shares the five s-lines).
    git(
        dir.path(),
        &["checkout", "-q", "-b", "side", "HEAD~2"],
        CAROL,
        t(3),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        "fn s0() {}\nfn s1() {}\nfn s2() {}\nfn s3() {}\nfn s4() {}\nfn a_extra() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "create similar a"],
        CAROL,
        t(4),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge keeps main's b.rs with a conflict-resolution edit and
    // discards a.rs entirely.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn s0() {}\nfn s1() {}\nfn s2() {}\nfn s3() {}\nfn s4() {}\nfn s5() {}\nfn merged() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs = its own two commits; the discarded similar a.rs must not
    // be merged into it.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.authors, 2, "the discarded a.rs leaked into b.rs");

    // The discarded file keeps its own record under its own path.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
}

/// A merge that renames one parent's `a.rs` *and* deletes another
/// parent's unrelated occupant of the same path: the unscoped
/// parent's deletion still needs its merge-time fence, or a
/// post-merge recreation inherits the dead occupant's history.
#[test]
fn merge_deletions_on_unscoped_parents_still_fence() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: common ancestor without a.rs.
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // main: the real a.rs.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(dir.path(), &["commit", "-q", "-m", "create a"], ALICE, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(2));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(2));

    // side (from the root): an unrelated occupant of the same path.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "side", "HEAD~2"],
        CAROL,
        t(3),
    );
    std::fs::write(dir.path().join("a.rs"), "fn other0() {}\nfn other1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "unrelated a"],
        CAROL,
        t(4),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // The merge moves main's a.rs to b.rs and drops side's occupant.
    git(dir.path(), &["checkout", "-q", "main"], ALICE, t(5));
    git(dir.path(), &["mv", "a.rs", "b.rs"], ALICE, t(5));
    git(dir.path(), &["add", "-A"], ALICE, t(5));
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
        ALICE,
        t(5),
    );
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &merge],
        ALICE,
        t(5),
    );
    git(dir.path(), &["checkout", "-q", "-f", "main"], ALICE, t(5));

    // A later commit reuses the path.
    std::fs::write(dir.path().join("a.rs"), "fn reborn() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(6));
    git(dir.path(), &["commit", "-q", "-m", "reuse a"], ALICE, t(6));

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // The recreated a.rs owns only its own commit — side's dead
    // occupant stays fenced behind the merge deletion.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.commit_frequency, 1);
    assert_eq!(a.churn_added, 1);
    assert_eq!(a.authors, 1);

    // b.rs still owns main's lineage.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.commit_frequency, 2);
    assert_eq!(b.churn_added, 4);
}

/// A candidate parent whose merge *kept* its own uninterrupted copy
/// while a merged-in side branch deleted the path: the side deletion
/// is not the candidate's identity boundary, and the candidate's
/// parallel edits must still follow the rename.
#[test]
fn side_branch_deletions_do_not_disqualify_uninterrupted_parents() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the shared original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Candidate branch P: edit a.rs, then merge a side branch that
    // deleted it — resolving to keep P's own copy.
    git(dir.path(), &["checkout", "-q", "-b", "p"], CAROL, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "edit on p"],
        CAROL,
        t(1),
    );
    let p_edit = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(1));

    git(
        dir.path(),
        &["checkout", "-q", "-b", "s", "main"],
        ALICE,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "side deletes a"],
        ALICE,
        t(2),
    );
    let side_del = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(2));

    // Merge s into p, keeping p's a.rs (first parent = p's edit).
    git(dir.path(), &["checkout", "-q", "p"], CAROL, t(3));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    let p_tree = git_out(dir.path(), &["write-tree"], CAROL, t(3));
    let p_tip = git_out(
        dir.path(),
        &[
            "commit-tree",
            &p_tree,
            "-p",
            &p_edit,
            "-p",
            &side_del,
            "-m",
            "keep p's a",
        ],
        CAROL,
        t(3),
    );

    // main (supplier): unrelated work with newer timestamps; the
    // rename itself happens in the final merge (conflict resolution).
    git(dir.path(), &["checkout", "-q", "main"], BOB, t(4));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(4));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(4));

    // Final merge: conflict resolution commits the file as b.rs
    // (p's edited content, so the parallel edit visibly survives) and
    // vacates a.rs.
    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(5));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(5));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &p_tip,
            "-m",
            "merge",
        ],
        BOB,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // Carol's parallel edit belongs to the surviving b.rs: the side
    // branch's deletion (kept out by p's merge) must not disqualify
    // p from the rename scopes. b.rs = shared creation (3 lines,
    // alice) + carol's edit (1 line) + the side deletion touch.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.churn_added, 4, "carol's parallel edit must reach b.rs");
    assert_eq!(b.authors, 2, "alice and carol touched the lineage");
}

/// A non-supplier parent that is itself a merge: its first-parent
/// line keeps `a.rs` uninterrupted, but a side branch merged into it
/// deleted and recreated the path. The recreation is an ancestor of
/// the scoped parent, yet it must not resolve through the rename
/// alias (the addition floor keeps additions pre-divergence).
#[test]
fn recreations_on_merged_side_branches_do_not_consume_merge_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the shared original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Candidate branch P: keeps a.rs on its own line but merges a
    // side branch that deleted and recreated it (P's merge keeps P's
    // copy).
    git(dir.path(), &["checkout", "-q", "-b", "p"], CAROL, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "edit on p"],
        CAROL,
        t(1),
    );
    let p_edit = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(1));

    git(
        dir.path(),
        &["checkout", "-q", "-b", "s", "main"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "s drops a"],
        CAROL,
        t(2),
    );
    std::fs::write(dir.path().join("a.rs"), "fn recreated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "s recreates a"],
        CAROL,
        t(3),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // P merges s, keeping P's own copy.
    git(dir.path(), &["checkout", "-q", "p"], CAROL, t(4));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    let p_tree = git_out(dir.path(), &["write-tree"], CAROL, t(4));
    let p_tip = git_out(
        dir.path(),
        &[
            "commit-tree",
            &p_tree,
            "-p",
            &p_edit,
            "-p",
            &side,
            "-m",
            "keep p's a",
        ],
        CAROL,
        t(4),
    );

    // main: unrelated work; the rename happens in the final merge.
    git(dir.path(), &["checkout", "-q", "main"], BOB, t(5));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(5));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(5));

    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(6));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(6));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(6));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &p_tip,
            "-m",
            "merge",
        ],
        BOB,
        t(6),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs: shared creation (3) + p's edit (1). The side branch's
    // recreated file must neither add its churn nor consume the alias
    // (which would strand the shared creation under a.rs). The side's
    // *deletion* of the original is a lineage touch and legitimately
    // counts toward the survivor's removed churn.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.churn_added, 4, "recreation leaked or alias consumed");
    assert_eq!(b.churn_removed, 3);

    // The recreation is a discarded occupant, fenced at the inner
    // merge rather than reported under the vacated path.
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// The recreated occupant on a merged side branch was also *edited*
/// before being discarded: those edits are walked before the
/// recreation and initially route through the alias — discovering the
/// floor-gated birth must pull them back to the occupant's identity.
#[test]
fn recreated_occupant_edits_are_pulled_back_from_merge_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the shared original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Candidate branch P: edits a.rs on its own line.
    git(dir.path(), &["checkout", "-q", "-b", "p"], CAROL, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "edit on p"],
        CAROL,
        t(1),
    );
    let p_edit = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(1));

    // Side branch: delete, recreate, then *edit* the recreation.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "s", "main"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "s drops a"],
        CAROL,
        t(2),
    );
    std::fs::write(dir.path().join("a.rs"), "fn recreated() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "s recreates a"],
        CAROL,
        t(3),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        "fn recreated() {}\nfn recreated_more() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "s grows recreation"],
        CAROL,
        t(4),
    );
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(4));

    // P merges s, keeping P's own copy.
    git(dir.path(), &["checkout", "-q", "p"], CAROL, t(5));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(5));
    let p_tree = git_out(dir.path(), &["write-tree"], CAROL, t(5));
    let p_tip = git_out(
        dir.path(),
        &[
            "commit-tree",
            &p_tree,
            "-p",
            &p_edit,
            "-p",
            &side,
            "-m",
            "keep p's a",
        ],
        CAROL,
        t(5),
    );

    // main: unrelated work; the rename happens in the final merge.
    git(dir.path(), &["checkout", "-q", "main"], BOB, t(6));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(6));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(6));

    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(7));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(7));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(7));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &p_tip,
            "-m",
            "merge",
        ],
        BOB,
        t(7),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // b.rs: shared creation (3) + p's edit (1). Neither the recreated
    // occupant's birth nor its later edit may stick to the survivor.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(
        b.churn_added, 4,
        "the recreated occupant's edits leaked into the survivor"
    );

    // The discarded occupant's whole lineage (recreation + edit) is
    // fenced at the inner merge rather than reported under the
    // vacated path.
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// A few bytes inserted near the start of a renamed long single-line
/// file: content-defined similarity chunking must keep the rename
/// joined (fixed-offset chunking alone would shift every span
/// boundary and collapse the similarity).
#[test]
fn single_line_renames_survive_small_insertions() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // ~900 bytes of varied single-line content (no newline until the
    // end), so gear cuts fire at content-defined positions.
    let body: String = (0..100).map(|i| format!("tok{i:04}x")).collect();
    std::fs::write(dir.path().join("bundle.min.js"), format!("{body}\n")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "base"], ALICE, t(0));
    git(dir.path(), &["tag", "ins-base"], ALICE, t(0));

    // Rename + insert a few bytes near the beginning.
    git(
        dir.path(),
        &["mv", "bundle.min.js", "bundle.v2.min.js"],
        BOB,
        t(1),
    );
    std::fs::write(
        dir.path().join("bundle.v2.min.js"),
        format!("INSERTED;{body}\n"),
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "rename + insert"],
        BOB,
        t(1),
    );
    git(dir.path(), &["tag", "ins-head"], BOB, t(1));

    let repo = gix::discover(dir.path()).unwrap();
    let changed = mehen_git::changed_files(&repo, "ins-base", "ins-head").unwrap();
    assert_eq!(changed.len(), 1, "rename must stay joined: {changed:?}");
    assert_eq!(
        changed[0].path,
        std::path::PathBuf::from("bundle.v2.min.js")
    );
    assert_eq!(
        changed[0].source_path.as_deref(),
        Some(Path::new("bundle.min.js"))
    );

    // And the history walk keeps one lineage across the rename.
    let history = collect_history(&repo, "ins-head").unwrap();
    let fh = history.file(Path::new("bundle.v2.min.js")).unwrap();
    assert_eq!(fh.commit_frequency, 2);
}

/// A candidate parent that is itself a merge which kept its *second*
/// parent's retained copy while its first-parent line deleted the
/// path: the boundary scan must follow the lineage that supplies the
/// candidate's blob, not blindly the first parent.
#[test]
fn boundary_scan_follows_the_blob_supplying_parent() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the shared original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // p1: deletes a.rs (will be the candidate merge's *first* parent).
    git(dir.path(), &["checkout", "-q", "-b", "p1"], CAROL, t(1));
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "p1 drops a"],
        CAROL,
        t(1),
    );
    let p1 = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(1));

    // p2: retains and edits a.rs.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "p2", "main"],
        CAROL,
        t(2),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p2_edit() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "p2 edits a"],
        CAROL,
        t(2),
    );
    let p2 = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(2));

    // The candidate merge keeps p2's blob (first parent = p1!).
    git(dir.path(), &["checkout", "-q", "p2"], CAROL, t(3));
    let p_tree = git_out(dir.path(), &["write-tree"], CAROL, t(3));
    let p_tip = git_out(
        dir.path(),
        &[
            "commit-tree",
            &p_tree,
            "-p",
            &p1,
            "-p",
            &p2,
            "-m",
            "keep p2's a",
        ],
        CAROL,
        t(3),
    );

    // main: unrelated work; the rename happens in the final merge.
    git(dir.path(), &["checkout", "-q", "main"], BOB, t(4));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(4));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(4));

    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(5));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p2_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(5));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(5));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &p_tip,
            "-m",
            "merge",
        ],
        BOB,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // Carol's retained-lineage edit must follow the rename: the first
    // parent's deletion is not the supplying lineage's boundary.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(
        b.churn_added, 4,
        "the retained second-parent lineage was disqualified"
    );
}

/// The candidate merge *edits* the blob its second parent supplied
/// (no parent matches exactly), while its first-parent line deleted
/// and re-created the path with unrelated content: the boundary scan
/// must follow the similarity-continuing parent, not the first
/// parent that happens to hold any blob.
#[test]
fn boundary_scan_follows_edited_blobs_by_similarity() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the shared original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // p1 (the candidate merge's *first* parent): delete, then
    // recreate with unrelated content.
    git(dir.path(), &["checkout", "-q", "-b", "p1"], CAROL, t(1));
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "p1 drops a"],
        CAROL,
        t(1),
    );
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "p1 recreates a"],
        CAROL,
        t(2),
    );
    let p1 = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(2));

    // p2: retains and edits the original.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "p2", "main"],
        CAROL,
        t(3),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p2_edit() {}\n",
    )
    .unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "p2 edits a"],
        CAROL,
        t(3),
    );
    let p2 = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // The candidate merge keeps p2's lineage *with an extra edit*
    // (matching no parent blob exactly); first parent is p1.
    git(dir.path(), &["checkout", "-q", "p2"], CAROL, t(4));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p2_edit() {}\nfn merge_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(4));
    let p_tree = git_out(dir.path(), &["write-tree"], CAROL, t(4));
    let p_tip = git_out(
        dir.path(),
        &[
            "commit-tree",
            &p_tree,
            "-p",
            &p1,
            "-p",
            &p2,
            "-m",
            "keep p2's a, edited",
        ],
        CAROL,
        t(4),
    );

    // main: unrelated work; the rename happens in the final merge.
    git(dir.path(), &["checkout", "-q", "-f", "main"], BOB, t(5));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn more() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(5));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(5));

    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(6));
    std::fs::write(
        dir.path().join("b.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn p2_edit() {}\nfn merge_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(6));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(6));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &p_tip,
            "-m",
            "merge",
        ],
        BOB,
        t(6),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // Carol's retained-lineage edit follows the rename: neither p1's
    // unrelated recreation nor its deletion flip may disqualify the
    // candidate whose blob continues through p2.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(
        b.churn_added, 4,
        "the edited merge blob was traced through the wrong parent"
    );
}

/// A merge keeps one parent's original `a.rs` while another parent's
/// line deleted and recreated an unrelated `a.rs` that the merge
/// discards — with no introduced rename or addition. The discarded
/// occupant still needs a fence: its recreation must not accumulate
/// under the live path, and its deletion must not fence the shared
/// pre-branch creation away from the survivor.
#[test]
fn discarded_same_path_recreations_are_fenced_at_merges() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Retaining branch: grows the original.
    git(dir.path(), &["checkout", "-q", "-b", "retain"], BOB, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn kept_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let retain = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // Discard branch: delete, then recreate unrelated content —
    // *newer* timestamps, so its commits walk before the retainer's.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "discard", "main"],
        ALICE,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], ALICE, t(2));
    std::fs::write(dir.path().join("a.rs"), "fn own0() {}\nfn own1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a"],
        CAROL,
        t(3),
    );
    let discard = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // The merge keeps the retaining parent's blob at a.rs.
    git(dir.path(), &["checkout", "-q", "-f", "retain"], BOB, t(4));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &retain,
            "-p",
            &discard,
            "-m",
            "keep the original",
        ],
        BOB,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor keeps its full lineage — creation, growth, and the
    // discard branch's deletion touch — and nothing of the discarded
    // recreation (whose author carol must not appear).
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 4, "the shared creation was fenced away");
    assert_eq!(a.commit_frequency, 3);
    assert_eq!(a.authors, 2, "the discarded occupant leaked in");
}

/// The discarded branch's recreation *resembles* the survivor
/// (≥ 50% similar): endpoint similarity alone would classify it as a
/// continuation, but the deletion on that branch's line is an
/// identity boundary and the fence must still install.
#[test]
fn similar_discarded_recreations_are_still_fenced_at_merges() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Retaining branch: grows the original.
    git(dir.path(), &["checkout", "-q", "-b", "retain"], BOB, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\nfn kept_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let retain = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // Discard branch: delete, then recreate with *similar* content
    // (three of four original lines survive — well above the rename
    // threshold). Newer timestamps: walked before the retainer.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "discard", "main"],
        ALICE,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], ALICE, t(2));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn imposter() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate similar a"],
        CAROL,
        t(3),
    );
    let discard = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // The merge keeps the retaining parent's blob.
    git(dir.path(), &["checkout", "-q", "-f", "retain"], BOB, t(4));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &retain,
            "-p",
            &discard,
            "-m",
            "keep the original",
        ],
        BOB,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor keeps its full lineage; carol's similar-but-
    // recreated file stays fenced (its creation must neither appear
    // here nor fence away the shared root creation).
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 5, "the shared creation was fenced away");
    assert_eq!(a.authors, 2, "the similar recreation leaked in");
}

/// The discarded branch's recreation is *byte-identical* to the blob
/// the merge keeps: neither parent-to-merge diff mentions the path at
/// all (exact OID equality removes it from both diffs), so no
/// `Modified` entry exists to hang a fence on. The recreated
/// occupant's line still ends at the deletion boundary — its
/// recreation must not accumulate under the survivor, and its
/// deletion must not fence the shared root creation away.
#[test]
fn exact_content_discarded_recreations_are_fenced_at_merges() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Retaining branch: grows the original.
    git(dir.path(), &["checkout", "-q", "-b", "retain"], BOB, t(1));
    let grown = "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\nfn kept_edit() {}\n";
    std::fs::write(dir.path().join("a.rs"), grown).unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let retain = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // Discard branch: delete, then recreate with content byte-equal
    // to the retainer's grown blob — the exact OID match erases the
    // path from both parent-to-merge diffs. Newer timestamps: walked
    // before the retainer.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "discard", "main"],
        ALICE,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], ALICE, t(2));
    std::fs::write(dir.path().join("a.rs"), grown).unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a byte-identical"],
        CAROL,
        t(3),
    );
    let discard = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // The merge keeps the retaining parent's blob.
    git(dir.path(), &["checkout", "-q", "-f", "retain"], BOB, t(4));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &retain,
            "-p",
            &discard,
            "-m",
            "keep the original",
        ],
        BOB,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor keeps its full lineage; carol's byte-identical
    // recreation stays on its own dead line (its creation must
    // neither appear here nor fence away the shared root creation).
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 5, "the shared creation was fenced away");
    assert_eq!(a.authors, 2, "the identical recreation leaked in");
}

/// The hardest discarded-recreation shape: the surviving parent left
/// the path *untouched* since the divergence, and the discarded
/// branch recreated it byte-identical to that original — base, both
/// parent endpoints, and the merged tree all hold one OID, so no tree
/// pair anywhere can see the recreation. Only the walk knows: the
/// deletion is bypassed by a merge whose other parent carried the
/// blob over an uninterrupted line, so the recreation belongs to a
/// dead occupant and the shared creation stays with the survivor.
#[test]
fn revert_style_discarded_recreations_are_fenced_at_merges() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original.
    let original = "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n";
    std::fs::write(dir.path().join("a.rs"), original).unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Retaining branch: unrelated work only — a.rs stays untouched.
    git(dir.path(), &["checkout", "-q", "-b", "retain"], BOB, t(1));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn keep2() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(1));
    let retain = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // Discard branch: delete, then restore the original bytes —
    // invisible to every endpoint diff. Newer timestamps: walked
    // before the retainer.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "discard", "main"],
        ALICE,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], ALICE, t(2));
    std::fs::write(dir.path().join("a.rs"), original).unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "restore a verbatim"],
        CAROL,
        t(3),
    );
    let discard = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // The merge keeps the retaining parent's tree.
    git(dir.path(), &["checkout", "-q", "-f", "retain"], BOB, t(4));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &retain,
            "-p",
            &discard,
            "-m",
            "keep the original",
        ],
        BOB,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor keeps the shared creation and the discard branch's
    // deletion touch; carol's verbatim restoration stays on its dead
    // line.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 4, "the shared creation was fenced away");
    assert_eq!(a.authors, 1, "the verbatim restoration leaked in");
    assert_eq!(a.commit_frequency, 2, "creation + deletion touch");
}

/// A merge rename whose *supplier* deleted and recreated the source
/// after the divergence, while another parent retained the original:
/// the alias must stay supplier-only. Widening to the retaining
/// parent (whose endpoint blob does continue the base) would set an
/// addition floor that rejects the supplier's actual recreation —
/// stranding its edits under the vanished source — and route the
/// retained original into the rename target.
#[test]
fn merge_rename_scopes_stay_supplier_only_when_the_supplier_recreated_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Retaining branch: unrelated work; a.rs keeps the original.
    git(dir.path(), &["checkout", "-q", "-b", "retain"], BOB, t(1));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn keep2() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow keep"], BOB, t(1));
    let retain = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // Mover branch: delete the original, recreate something unrelated
    // at the same path. Newer timestamps: walked before the retainer.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "mover", "main"],
        ALICE,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], ALICE, t(2));
    git(dir.path(), &["commit", "-q", "-m", "drop a"], ALICE, t(2));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn n0() {}\nfn n1() {}\nfn n2() {}\nfn n3() {}\nfn n4() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "recreate a"],
        CAROL,
        t(3),
    );
    let mover = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // The merge renames the *recreation* to b.rs (the merged tree has
    // no a.rs): the mover's diff pairs `a.rs → b.rs` exactly, so the
    // mover is the supplier.
    git(dir.path(), &["checkout", "-q", "-f", "mover"], BOB, t(4));
    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(4));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(4));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &retain,
            "-p",
            &mover,
            "-m",
            "rename the recreation",
        ],
        BOB,
        t(4),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor is carol's recreation, moved: exactly its own
    // churn (5 lines) — not the 4-line root creation the retaining
    // parent held.
    let b = history.file(Path::new("b.rs")).unwrap();
    assert_eq!(b.churn_added, 5, "the recreation's edits were stranded");
    assert_eq!(b.authors, 1, "the retained original leaked into b.rs");
    assert_eq!(b.commit_frequency, 1);
    assert!(history.file(Path::new("a.rs")).is_none());
}

/// `--allow-unrelated-histories`: the merged parents have no merge
/// base, so two similar same-path blobs cannot share a lineage —
/// endpoint similarity proves nothing. The discarded parent's
/// independently created occupant must be fenced (with no floor:
/// there is no shared pre-branch history to protect).
#[test]
fn unrelated_history_merges_fence_similar_discarded_occupants() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // Main line: the survivor.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    // Orphan line: an unrelated root whose a.rs happens to be ≥50%
    // similar. Newer timestamps: walked before the main line.
    git(
        dir.path(),
        &["checkout", "-q", "--orphan", "other"],
        CAROL,
        t(1),
    );
    git(dir.path(), &["rm", "-rfq", "."], CAROL, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn imposter() {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "unrelated root"],
        CAROL,
        t(1),
    );
    let other = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(1));

    // The merge keeps the main parent's tree.
    git(dir.path(), &["checkout", "-q", "-f", "main"], BOB, t(2));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(2));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &other,
            "-m",
            "merge unrelated histories",
        ],
        BOB,
        t(2),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor keeps only its own line; carol's independent
    // occupant stays fenced despite the endpoint similarity.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 4, "the unrelated occupant leaked in");
    assert_eq!(a.authors, 1, "the unrelated occupant's author leaked in");
    assert_eq!(a.commit_frequency, 1);
}

/// Unrelated histories whose roots hold *byte-identical* same-path
/// blobs: exact OID equality erases the path from every
/// parent-to-merge diff, and there is no merge base to diff a parent
/// against — only the merged-tree enumeration pass can see the
/// duplicate. Without its fence both root additions would accumulate
/// under the surviving path, doubling churn and frequency and merging
/// unrelated authorship.
#[test]
fn unrelated_history_merges_fence_byte_identical_occupants() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // Main line: the survivor.
    let content = "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn a3() {}\n";
    std::fs::write(dir.path().join("a.rs"), content).unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    // Orphan line: an unrelated root with the exact same bytes at the
    // same path. Newer timestamp: walked before the main line.
    git(
        dir.path(),
        &["checkout", "-q", "--orphan", "other"],
        CAROL,
        t(1),
    );
    git(dir.path(), &["rm", "-rfq", "."], CAROL, t(1));
    std::fs::write(dir.path().join("a.rs"), content).unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "unrelated identical root"],
        CAROL,
        t(1),
    );
    let other = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(1));

    // The merge keeps the main parent's tree.
    git(dir.path(), &["checkout", "-q", "-f", "main"], BOB, t(2));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(2));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &other,
            "-m",
            "merge unrelated histories",
        ],
        BOB,
        t(2),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // One creation, one author, one commit — not two of each.
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 4, "the identical unrelated root leaked in");
    assert_eq!(a.authors, 1, "unrelated authorship was merged");
    assert_eq!(a.commit_frequency, 1, "commit frequency was doubled");
}

/// A renamed symlink is one changed identity, not two changeset
/// members: the tree diff reports it as a non-blob deletion plus a
/// non-blob addition, and counting both would inflate every other
/// file's coupling in the same commit.
#[test]
#[cfg(unix)]
fn symlink_renames_count_once_in_coupling_cardinality() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
    std::os::unix::fs::symlink("a.py", dir.path().join("alias.py")).unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // One commit: edit a.py and rename the symlink. The changeset has
    // two identities (the file, the moved link) — a.py's coupling
    // must read 1, not 2.
    git(dir.path(), &["mv", "alias.py", "alias2.py"], ALICE, t(1));
    std::fs::write(dir.path().join("a.py"), "x = 1\ny = 2\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "edit + move link"],
        ALICE,
        t(1),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let a = history.file(Path::new("a.py")).unwrap();
    // Root commit: a.py + symlink = 1 other; edit commit: a.py +
    // moved symlink = 1 other. A double-counted rename would read 3.
    assert_eq!(
        a.sum_of_coupling, 2,
        "the symlink rename was counted as two changeset members"
    );
}

/// A blob created purely by merge conflict resolution — never touched
/// by any walked non-merge commit — is *tracked with an all-zero
/// history*, not unmeasurable: `tracked_file` must return a zero
/// entry (rankable as legitimately calm) rather than `None` (which
/// reads as an untracked file and renders every history column
/// `n/a`).
#[test]
fn merge_created_untouched_blobs_read_zero_history_not_none() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));
    let main = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    git(dir.path(), &["checkout", "-q", "-b", "side"], BOB, t(1));
    std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(1));
    git(dir.path(), &["commit", "-q", "-m", "side"], BOB, t(1));
    let side = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // The merge tree carries a file absent from both parents —
    // conflict-resolution-created.
    std::fs::write(dir.path().join("merge_only.rs"), "fn m() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(2));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(2));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge with new file",
        ],
        BOB,
        t(2),
    );

    // Advance HEAD past the merge without touching the merge-created
    // blob: its age must measure from the *creating merge*, not read
    // an eternal zero pinned to whatever HEAD is now.
    git(dir.path(), &["checkout", "-q", &merge], ALICE, t(3));
    git(dir.path(), &["checkout", "-q", "-b", "after"], ALICE, t(3));
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn a2() {}\n").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "-am", "later work"],
        ALICE,
        t(3),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // No non-merge commit touched it: no accumulator...
    assert!(history.file(Path::new("merge_only.rs")).is_none());
    // ...but the blob is tracked, so its history is a measured zero.
    let fh = history
        .tracked_file(Path::new("merge_only.rs"))
        .expect("tracked blob must be history-available");
    assert_eq!(fh.commit_frequency, 0);
    assert_eq!(fh.churn_abs(), 0);
    assert_eq!(fh.authors, 0);
    // Age counts from the creating merge (t2) to HEAD (t3): 100 000
    // seconds, not zero.
    let expected_months = 100_000.0 / (30.436875 * 86_400.0);
    assert!(
        (fh.age_months(history.head_seconds) - expected_months).abs() < 1e-9,
        "age must measure from the creating merge, got {}",
        fh.age_months(history.head_seconds)
    );
}

/// Two parallel merges each conflict-create the same path; a later
/// merge keeps one version and fences the other. The surviving
/// zero-touch blob's synthesized age must come from *its* creating
/// merge, not from the discarded occupant's — which the date-order
/// walk visits first here (newer timestamps).
#[test]
fn discarded_parallel_merge_creations_do_not_misdate_the_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));
    let root = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    // A tiny two-branch diamond whose merge tree conflict-creates
    // `a.rs` with the given content.
    let creating_merge = |branch: &str, filler: &str, content: &str, n: i64| -> String {
        git(
            dir.path(),
            &["checkout", "-q", "-b", branch, &root],
            ALICE,
            t(n),
        );
        std::fs::write(dir.path().join(filler), "fn f() {}\n").unwrap();
        git(dir.path(), &["add", "-A"], ALICE, t(n));
        git(dir.path(), &["commit", "-q", "-m", "filler"], ALICE, t(n));
        let side = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(n));
        std::fs::write(dir.path().join("a.rs"), content).unwrap();
        git(dir.path(), &["add", "-A"], BOB, t(n + 1));
        let tree = git_out(dir.path(), &["write-tree"], BOB, t(n + 1));
        let merge = git_out(
            dir.path(),
            &[
                "commit-tree",
                &tree,
                "-p",
                &root,
                "-p",
                &side,
                "-m",
                "conflict-create a.rs",
            ],
            BOB,
            t(n + 1),
        );
        std::fs::remove_file(dir.path().join("a.rs")).unwrap();
        git(dir.path(), &["checkout", "-q", "-f", &root], ALICE, t(n));
        merge
    };

    // Survivor created at t(2); discarded occupant created at t(4)
    // (newer — walked first).
    let survivor_merge = creating_merge("one", "f1.rs", "fn kept() {}\n", 1);
    let discarded_merge = creating_merge("two", "f2.rs", "fn discarded_occupant_content() {}\n", 3);

    // The outer merge keeps the survivor's blob.
    git(
        dir.path(),
        &["checkout", "-q", "-f", &survivor_merge],
        ALICE,
        t(5),
    );
    let tree = git_out(dir.path(), &["write-tree"], ALICE, t(5));
    let outer = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &survivor_merge,
            "-p",
            &discarded_merge,
            "-m",
            "keep one version",
        ],
        ALICE,
        t(5),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &outer).unwrap();

    let fh = history
        .tracked_file(Path::new("a.rs"))
        .expect("tracked blob must be history-available");
    assert_eq!(
        fh.commit_frequency, 0,
        "merge-only blob accumulates nothing"
    );
    // head = t(5), surviving creation = t(2): 300 000 seconds. The
    // discarded occupant's creation (t(4), walked first) must not
    // shrink this to 100 000.
    let expected_months = 300_000.0 / (30.436875 * 86_400.0);
    assert!(
        (fh.age_months(history.head_seconds) - expected_months).abs() < 1e-9,
        "age must come from the surviving creation, got {} months",
        fh.age_months(history.head_seconds)
    );
}

/// A merge conflict-creates `a.rs`; a later merge's identity-only
/// change renames it to `b.rs`. The creation timestamp must be keyed
/// by the *resolved* live path (`b.rs`) — keying by the addition's
/// own path would leave the zero-touch `b.rs` with no entry and an
/// age fabricated from HEAD.
#[test]
fn merge_created_then_merge_renamed_blobs_keep_their_creation_age() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));
    let root = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(0));

    // First diamond: the merge conflict-creates a.rs at t(2).
    git(dir.path(), &["checkout", "-q", "-b", "one"], ALICE, t(1));
    std::fs::write(dir.path().join("f1.rs"), "fn f1() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(dir.path(), &["commit", "-q", "-m", "side one"], ALICE, t(1));
    let side1 = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(1));
    std::fs::write(dir.path().join("a.rs"), "fn created_by_merge() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], BOB, t(2));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(2));
    let m1 = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &root,
            "-p",
            &side1,
            "-m",
            "conflict-create a.rs",
        ],
        BOB,
        t(2),
    );

    // Second diamond off m1: the merge renames a.rs → b.rs at t(4).
    git(dir.path(), &["checkout", "-q", &m1], ALICE, t(3));
    git(dir.path(), &["checkout", "-q", "-b", "two"], ALICE, t(3));
    std::fs::write(dir.path().join("f2.rs"), "fn f2() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(3));
    git(dir.path(), &["commit", "-q", "-m", "side two"], ALICE, t(3));
    let side2 = git_out(dir.path(), &["rev-parse", "HEAD"], ALICE, t(3));
    git(dir.path(), &["mv", "a.rs", "b.rs"], BOB, t(4));
    git(dir.path(), &["add", "-A"], BOB, t(4));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(4));
    let m2 = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &m1,
            "-p",
            &side2,
            "-m",
            "rename a.rs to b.rs",
        ],
        BOB,
        t(4),
    );

    // Advance HEAD one commit past the renaming merge.
    git(dir.path(), &["checkout", "-q", &m2], ALICE, t(5));
    git(dir.path(), &["checkout", "-q", "-b", "after"], ALICE, t(5));
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\nfn k2() {}\n").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "later"], ALICE, t(5));

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    let fh = history
        .tracked_file(Path::new("b.rs"))
        .expect("tracked blob must be history-available");
    // head = t(5), creation = t(2): 300 000 seconds — not zero.
    let expected_months = 300_000.0 / (30.436875 * 86_400.0);
    assert!(
        (fh.age_months(history.head_seconds) - expected_months).abs() < 1e-9,
        "age must survive the merge rename, got {} months",
        fh.age_months(history.head_seconds)
    );
}

/// The exact-rename overflow fallback (> 10 000 same-content pairs)
/// must keep basename affinity: a bulk move of identical stubs whose
/// destination directories reverse the path-sorted order would
/// otherwise be paired positionally, silently transferring commit
/// history between files whose basenames match unambiguously.
#[test]
fn exact_rename_overflow_fallback_pairs_by_basename() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // One stub is seeded by a bug-fix commit — the marker that must
    // follow its lineage through the move. `f000` sorts first on the
    // source side while its destination (`d100/f000.rs`) sorts last,
    // so positional pairing is maximally wrong for it.
    let stub = "fn stub() {}\n";
    std::fs::create_dir(dir.path().join("s")).unwrap();
    std::fs::write(dir.path().join("s/f000.rs"), stub).unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(0));
    git(
        dir.path(),
        &["commit", "-q", "-m", "fix: seed f000"],
        CAROL,
        t(0),
    );

    // 100 more byte-identical stubs: 101 deletions × 101 additions
    // in the move commit exceeds the 10 000-pair ranking budget.
    for i in 1..101 {
        std::fs::write(dir.path().join(format!("s/f{i:03}.rs")), stub).unwrap();
    }
    git(dir.path(), &["add", "-A"], ALICE, t(1));
    git(
        dir.path(),
        &["commit", "-q", "-m", "bulk stubs"],
        ALICE,
        t(1),
    );

    // Move every stub into its own directory, numbered so the sorted
    // destination order *reverses* the source order.
    for i in 0..101 {
        let dest_dir = dir.path().join(format!("d{:03}", 100 - i));
        std::fs::create_dir(&dest_dir).unwrap();
        std::fs::rename(
            dir.path().join(format!("s/f{i:03}.rs")),
            dest_dir.join(format!("f{i:03}.rs")),
        )
        .unwrap();
    }
    git(dir.path(), &["add", "-A"], ALICE, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "bulk move"],
        ALICE,
        t(2),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, "HEAD").unwrap();

    // f000's bug-fix creation must have followed *its* basename to
    // d100/f000.rs — positional pairing would hand it to d000/f100.rs
    // (the first destination in sorted order).
    let bugfix_carrier = history.file(Path::new("d100/f000.rs")).unwrap();
    assert_eq!(
        bugfix_carrier.bugfix_commits, 1,
        "the bug-fix lineage was paired onto the wrong destination"
    );
    let other = history.file(Path::new("d000/f100.rs")).unwrap();
    assert_eq!(other.bugfix_commits, 0);
}

/// An octopus merge discarding *two* parents' independent occupants
/// of the same path: each discarded parent needs its own fence — a
/// per-path dedup would leave the second occupant unfenced.
#[test]
fn octopus_merges_fence_every_discarded_occupant() {
    let dir = tempfile::tempdir().unwrap();
    let t = |n: i64| 1_700_000_000 + n * 100_000;
    git(dir.path(), &["init", "-q", "-b", "main"], ALICE, t(0));

    // c0: the original.
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], ALICE, t(0));
    git(dir.path(), &["commit", "-q", "-m", "root"], ALICE, t(0));

    // Retaining branch.
    git(dir.path(), &["checkout", "-q", "-b", "retain"], BOB, t(1));
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a0() {}\nfn a1() {}\nfn a2() {}\nfn kept_edit() {}\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "grow a"], BOB, t(1));
    let retain = git_out(dir.path(), &["rev-parse", "HEAD"], BOB, t(1));

    // First discarded branch: delete + recreate (newest timestamps).
    git(
        dir.path(),
        &["checkout", "-q", "-b", "d1", "main"],
        CAROL,
        t(2),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(2));
    git(
        dir.path(),
        &["commit", "-q", "-m", "d1 drops a"],
        CAROL,
        t(2),
    );
    std::fs::write(dir.path().join("a.rs"), "fn d1_own() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(3));
    git(
        dir.path(),
        &["commit", "-q", "-m", "d1 recreates a"],
        CAROL,
        t(3),
    );
    let d1 = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(3));

    // Second discarded branch: another independent occupant.
    git(
        dir.path(),
        &["checkout", "-q", "-b", "d2", "main"],
        CAROL,
        t(4),
    );
    git(dir.path(), &["rm", "-q", "a.rs"], CAROL, t(4));
    git(
        dir.path(),
        &["commit", "-q", "-m", "d2 drops a"],
        CAROL,
        t(4),
    );
    std::fs::write(dir.path().join("a.rs"), "fn d2_own() {}\nfn d2_more() {}\n").unwrap();
    git(dir.path(), &["add", "-A"], CAROL, t(5));
    git(
        dir.path(),
        &["commit", "-q", "-m", "d2 recreates a"],
        CAROL,
        t(5),
    );
    let d2 = git_out(dir.path(), &["rev-parse", "HEAD"], CAROL, t(5));

    // The octopus merge keeps the retaining parent's blob.
    git(dir.path(), &["checkout", "-q", "-f", "retain"], BOB, t(6));
    let tree = git_out(dir.path(), &["write-tree"], BOB, t(6));
    let merge = git_out(
        dir.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &retain,
            "-p",
            &d1,
            "-p",
            &d2,
            "-m",
            "octopus keep",
        ],
        BOB,
        t(6),
    );

    let repo = gix::discover(dir.path()).unwrap();
    let history = collect_history(&repo, &merge).unwrap();

    // The survivor keeps exactly its own lineage plus the two
    // deletion touches; neither recreated occupant's lines leak in.
    // (carol appears as an author through her deletion touches only —
    // zero added lines.)
    let a = history.file(Path::new("a.rs")).unwrap();
    assert_eq!(a.churn_added, 4, "a discarded occupant leaked in");
    assert_eq!(a.commit_frequency, 4);
    assert_eq!(a.authors, 3);
    assert!((a.ownership - 0.75).abs() < 1e-9, "got {}", a.ownership);
}
