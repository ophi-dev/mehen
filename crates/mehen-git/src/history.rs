// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Git history-walk subsystem backing the `history.*` metric family
//! (post-classical metrics research foundation §6).
//!
//! [`collect_history`] performs a single revision walk from a given rev
//! and accumulates deterministic per-file process metrics: churn, code
//! age, authorship/ownership, commit frequency, sum of coupling,
//! bug-fix commit count, and Google's Time-Weighted Risk.
//!
//! Determinism contract: every value is a pure function of the
//! repository state at the walked rev. "Now" is the walked commit's
//! own committer time (research foundation §6.2), never wall-clock
//! time, so results are reproducible across runs and machines.
//!
//! Walk semantics match the common reference implementations
//! (code-maat, PyDriller, `git log --no-merges --numstat`):
//! merge commits are skipped and every other commit is diffed against
//! its first parent (or the empty tree for root commits). Renames are
//! not detected — a rename counts as a deletion plus an addition,
//! consistent with [`crate::changed_files`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::diff::tree::recorder::Change;
use gix::objs::TreeRefIter;

use crate::GitError;

/// Average Gregorian month in seconds (30.436875 days), used to express
/// code age in months without depending on calendar arithmetic.
const SECONDS_PER_MONTH: f64 = 2_629_746.0;

/// Commits touching more than this many files are ignored for the
/// sum-of-coupling signal (code-maat / CodeScene changeset noise
/// threshold, research foundation §6.4). Such commits — bulk renames,
/// reformat sweeps, vendored imports — carry no architectural coupling
/// signal. All other metrics still count them.
const MAX_COUPLING_CHANGESET: usize = 30;

/// An author contributing less than this share of a file's churned
/// lines is a "minor contributor" (PyDriller's fixed 5% threshold,
/// research foundation §6.3).
const MINOR_CONTRIBUTOR_SHARE: f64 = 0.05;

/// Steepness / decay-window constants of the Time-Weighted Risk
/// logistic: `1 / (1 + e^(-12t + 12))` (Lewis et al., ICSE 2013, with
/// ω hard-coded to 12 as in the paper's deployed variant).
const TWR_STEEPNESS: f64 = 12.0;
const TWR_OMEGA: f64 = 12.0;

/// Deterministic per-file history statistics, finalized over the full
/// walk. Raw counts are exposed alongside derived ratios so callers can
/// build composites (e.g. hotspot = complexity × [`Self::commit_frequency`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FileHistory {
    /// Number of non-merge commits that touched the file
    /// (`history.commit_frequency`).
    pub commit_frequency: u64,
    /// Total lines added across those commits.
    pub churn_added: u64,
    /// Total lines removed across those commits.
    pub churn_removed: u64,
    /// Number of distinct authors (`history.authors`), identified by
    /// lower-cased author email (falling back to the author name when
    /// the email is empty).
    pub authors: u64,
    /// Authors contributing less than 5% of the file's churned lines
    /// (`history.minor_contributors`).
    pub minor_contributors: u64,
    /// Share of churned lines contributed by the single top author
    /// (`history.ownership`), in `[0, 1]`. `0` when no lines were
    /// churned at all.
    pub ownership: f64,
    /// Committer timestamp (seconds since epoch) of the last commit
    /// touching the file.
    pub last_change_seconds: i64,
    /// Sum over qualifying commits of the number of *other* files
    /// changed alongside this one (`history.sum_of_coupling`).
    pub sum_of_coupling: u64,
    /// Commits whose message matches the bug-fix heuristic
    /// (`history.bugfix_commits`).
    pub bugfix_commits: u64,
    /// Time-Weighted Risk over the bug-fixing commits
    /// (`history.twr`).
    pub twr: f64,
}

impl FileHistory {
    /// Absolute churn: lines added + lines removed
    /// (`history.churn.abs`, code-maat's `abs-churn` definition).
    pub fn churn_abs(&self) -> u64 {
        self.churn_added + self.churn_removed
    }

    /// Months since the file's last change, relative to `head_seconds`
    /// (`history.age_months`). Clamped at zero for robustness against
    /// clock-skewed commit metadata.
    pub fn age_months(&self, head_seconds: i64) -> f64 {
        let delta = head_seconds.saturating_sub(self.last_change_seconds);
        (delta.max(0) as f64) / SECONDS_PER_MONTH
    }
}

/// Per-file history statistics for an entire repository at a fixed rev.
#[derive(Debug)]
pub struct RepositoryHistory {
    /// Committer timestamp (seconds since epoch) of the walked rev —
    /// the deterministic "now" for age computations.
    pub head_seconds: i64,
    files: HashMap<PathBuf, FileHistory>,
}

impl RepositoryHistory {
    /// History stats for a repository-relative path, if any walked
    /// commit touched it.
    pub fn file(&self, path: &Path) -> Option<&FileHistory> {
        self.files.get(path)
    }

    /// Number of files with recorded history.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the walk recorded no file history at all.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Per-file accumulator filled during the walk, finalized into
/// [`FileHistory`] once repository-wide bounds (first/head commit
/// times) are known.
#[derive(Debug, Default)]
struct FileAccumulator {
    commit_frequency: u64,
    churn_added: u64,
    churn_removed: u64,
    /// Churned lines per author identity.
    author_lines: HashMap<String, u64>,
    last_change_seconds: i64,
    sum_of_coupling: u64,
    /// Committer timestamps of bug-fixing commits, for TWR.
    bugfix_seconds: Vec<i64>,
}

/// Walk the full history reachable from `rev` (first-parent diffs,
/// merges skipped) and accumulate per-file process statistics.
///
/// The cost is one tree diff per non-merge commit plus one line diff
/// per modified blob; results depend only on the repository state at
/// `rev`.
pub fn collect_history(repo: &gix::Repository, rev: &str) -> Result<RepositoryHistory, GitError> {
    let head_id = repo
        .rev_parse_single(rev)
        .map_err(|_| GitError::RefNotFound(rev.to_string()))?;
    let head_commit = head_id
        .object()
        .map_err(|e| GitError::Internal(e.to_string()))?
        .peel_to_commit()
        .map_err(|e| GitError::Internal(e.to_string()))?;
    let head_seconds = commit_seconds(&head_commit)?;

    let mut files: HashMap<PathBuf, FileAccumulator> = HashMap::new();
    let mut first_commit_seconds = head_seconds;

    let walk = repo
        .rev_walk([head_commit.id])
        .all()
        .map_err(|e| GitError::Internal(e.to_string()))?;

    for info in walk {
        let info = info.map_err(|e| GitError::Internal(e.to_string()))?;
        // Merge commits are skipped: their first-parent diff would
        // double-count every line already attributed to the merged
        // commits (matching `git log --no-merges` / code-maat).
        if info.parent_ids.len() > 1 {
            continue;
        }

        let commit = info
            .object()
            .map_err(|e| GitError::Internal(e.to_string()))?;
        let seconds = commit_seconds(&commit)?;
        first_commit_seconds = first_commit_seconds.min(seconds);
        let author = author_identity(&commit)?;
        let is_bugfix = is_bugfix_message(commit.message_raw_sloppy());

        let changes = diff_against_first_parent(repo, &commit)?;
        let coupling_eligible = changes.len() <= MAX_COUPLING_CHANGESET;
        // The "other files in this commit" count is the same for every
        // file in the changeset.
        let coupled_others = changes.len().saturating_sub(1) as u64;

        for change in &changes {
            let acc = files.entry(change.path.clone()).or_default();
            acc.commit_frequency += 1;
            acc.churn_added += change.added;
            acc.churn_removed += change.removed;
            *acc.author_lines.entry(author.clone()).or_insert(0) += change.added + change.removed;
            acc.last_change_seconds = acc.last_change_seconds.max(seconds);
            if coupling_eligible {
                acc.sum_of_coupling += coupled_others;
            }
            if is_bugfix {
                acc.bugfix_seconds.push(seconds);
            }
        }
    }

    let files = files
        .into_iter()
        .map(|(path, acc)| (path, finalize_file(acc, first_commit_seconds, head_seconds)))
        .collect();

    Ok(RepositoryHistory {
        head_seconds,
        files,
    })
}

/// Fold a walk-time accumulator into the public [`FileHistory`].
fn finalize_file(acc: FileAccumulator, first_seconds: i64, head_seconds: i64) -> FileHistory {
    let authors = acc.author_lines.len() as u64;
    let total_lines: u64 = acc.author_lines.values().sum();
    let (minor_contributors, ownership) = if total_lines == 0 {
        // A history of pure renames/mode changes churns no lines;
        // ownership is undefined — report zero rather than dividing
        // by zero.
        (0, 0.0)
    } else {
        let total = total_lines as f64;
        let minor = acc
            .author_lines
            .values()
            .filter(|&&lines| (lines as f64) / total < MINOR_CONTRIBUTOR_SHARE)
            .count() as u64;
        let top = acc.author_lines.values().copied().max().unwrap_or(0);
        (minor, (top as f64) / total)
    };

    FileHistory {
        commit_frequency: acc.commit_frequency,
        churn_added: acc.churn_added,
        churn_removed: acc.churn_removed,
        authors,
        minor_contributors,
        ownership,
        last_change_seconds: acc.last_change_seconds,
        sum_of_coupling: acc.sum_of_coupling,
        bugfix_commits: acc.bugfix_seconds.len() as u64,
        twr: time_weighted_risk(&acc.bugfix_seconds, first_seconds, head_seconds),
    }
}

/// Google Time-Weighted Risk (Lewis et al., ICSE 2013):
/// `Σᵢ 1 / (1 + e^(−12·tᵢ + 12))` where `tᵢ` is the bug-fixing
/// commit's time normalized to `[0, 1]` over the walked history
/// (0 = oldest walked commit, 1 = head).
fn time_weighted_risk(bugfix_seconds: &[i64], first_seconds: i64, head_seconds: i64) -> f64 {
    if bugfix_seconds.is_empty() {
        return 0.0;
    }
    let span = (head_seconds - first_seconds).max(0) as f64;
    // Sort so the float summation order is independent of the walk
    // order (cross-platform determinism contract).
    let mut times: Vec<i64> = bugfix_seconds.to_vec();
    times.sort_unstable();
    times
        .iter()
        .map(|&s| {
            let t = if span == 0.0 {
                // Single-commit histories: the fix is "now".
                1.0
            } else {
                (((s - first_seconds).max(0) as f64) / span).clamp(0.0, 1.0)
            };
            1.0 / (1.0 + (-TWR_STEEPNESS * t + TWR_OMEGA).exp())
        })
        .sum()
}

/// A single file's change within one commit, with line-level churn.
struct CommitFileChange {
    path: PathBuf,
    added: u64,
    removed: u64,
}

/// Diff `commit` against its first parent (or the empty tree for root
/// commits) and compute line-level churn per changed blob.
fn diff_against_first_parent(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
) -> Result<Vec<CommitFileChange>, GitError> {
    let internal = |e: &dyn std::error::Error| GitError::Internal(e.to_string());

    let to_tree = commit.tree().map_err(|e| internal(&e))?;
    let parent_tree = match commit.parent_ids().next() {
        Some(parent_id) => {
            let parent = parent_id
                .object()
                .map_err(|e| internal(&e))?
                .peel_to_commit()
                .map_err(|e| internal(&e))?;
            Some(parent.tree().map_err(|e| internal(&e))?)
        }
        None => None,
    };
    let (parent_data, parent_kind) = match &parent_tree {
        Some(tree) => (tree.data.as_slice(), tree.id.kind()),
        // Root commit: diff against the empty tree.
        None => ([].as_slice(), to_tree.id.kind()),
    };

    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        TreeRefIter::from_bytes(parent_data, parent_kind),
        TreeRefIter::from_bytes(&to_tree.data, to_tree.id.kind()),
        gix::diff::tree::State::default(),
        repo.objects.clone(),
        &mut recorder,
    )
    .map_err(|e| internal(&e))?;

    let mut changes = Vec::with_capacity(recorder.records.len());
    for change in recorder.records {
        let file_change = match change {
            Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if !entry_mode.is_blob() {
                    continue;
                }
                CommitFileChange {
                    path: PathBuf::from(path.to_string()),
                    added: blob_line_count(repo, &oid)?,
                    removed: 0,
                }
            }
            Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if !entry_mode.is_blob() {
                    continue;
                }
                CommitFileChange {
                    path: PathBuf::from(path.to_string()),
                    added: 0,
                    removed: blob_line_count(repo, &oid)?,
                }
            }
            Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
                path,
            } => {
                // Mode-only or non-blob changes (e.g. submodule bumps)
                // churn no lines.
                if !entry_mode.is_blob() && !previous_entry_mode.is_blob() {
                    continue;
                }
                let (added, removed) = blob_line_diff(repo, &previous_oid, &oid)?;
                CommitFileChange {
                    path: PathBuf::from(path.to_string()),
                    added,
                    removed,
                }
            }
        };
        changes.push(file_change);
    }

    Ok(changes)
}

/// Number of lines in a blob (a trailing fragment without `\n` counts
/// as a line).
fn blob_line_count(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<u64, GitError> {
    let data = read_blob_data(repo, oid)?;
    Ok(count_lines(&data))
}

/// Line-level (added, removed) counts between two blob versions using
/// the histogram diff algorithm.
fn blob_line_diff(
    repo: &gix::Repository,
    old: &gix::ObjectId,
    new: &gix::ObjectId,
) -> Result<(u64, u64), GitError> {
    let old_data = read_blob_data(repo, old)?;
    let new_data = read_blob_data(repo, new)?;
    let input = InternedInput::new(old_data.as_slice(), new_data.as_slice());
    let diff = Diff::compute(Algorithm::Histogram, &input);
    Ok((
        u64::from(diff.count_additions()),
        u64::from(diff.count_removals()),
    ))
}

fn read_blob_data(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<Vec<u8>, GitError> {
    let object = repo
        .find_object(*oid)
        .map_err(|e| GitError::Internal(e.to_string()))?;
    Ok(object.detach().data)
}

fn count_lines(data: &[u8]) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let newlines = data.iter().filter(|&&b| b == b'\n').count() as u64;
    if data.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

/// Committer timestamp in seconds since the Unix epoch.
fn commit_seconds(commit: &gix::Commit<'_>) -> Result<i64, GitError> {
    Ok(commit
        .time()
        .map_err(|e| GitError::Internal(e.to_string()))?
        .seconds)
}

/// Author identity for ownership metrics: lower-cased email, falling
/// back to the author name for commits without one.
fn author_identity(commit: &gix::Commit<'_>) -> Result<String, GitError> {
    let author = commit
        .author()
        .map_err(|e| GitError::Internal(e.to_string()))?;
    let email = author.email.to_string();
    if email.trim().is_empty() {
        Ok(author.name.to_string().to_lowercase())
    } else {
        Ok(email.to_lowercase())
    }
}

/// Bug-fix commit heuristic (Lewis et al. use a message classifier; the
/// transparent variant here matches whole words from a fixed list).
///
/// Word-boundary matching avoids classics like "prefix" or "debug"
/// counting as fixes. Issue references (`#123`) are deliberately *not*
/// treated as bug-fix markers: on GitHub-style squash merges every PR
/// commit carries one.
fn is_bugfix_message(message: &[u8]) -> bool {
    const BUGFIX_WORDS: &[&str] = &[
        "fix", "fixes", "fixed", "fixing", "fixup", "hotfix", "bugfix", "bug", "bugs",
    ];
    let lowered = String::from_utf8_lossy(message).to_lowercase();
    lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| BUGFIX_WORDS.contains(&word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_lines_handles_trailing_newline_variants() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"one\n"), 1);
        assert_eq!(count_lines(b"one\ntwo\n"), 2);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b"no newline"), 1);
    }

    #[test]
    fn bugfix_heuristic_matches_whole_words_only() {
        assert!(is_bugfix_message(b"fix: broken parser"));
        assert!(is_bugfix_message(b"Fixed the flaky test"));
        assert!(is_bugfix_message(b"hotfix for release"));
        assert!(is_bugfix_message(b"chore: fixup review comments"));
        assert!(is_bugfix_message(b"resolve BUG in walker"));
        // Substrings must not match.
        assert!(!is_bugfix_message(b"add prefix support"));
        assert!(!is_bugfix_message(b"improve debugging output"));
        assert!(!is_bugfix_message(b"feat: add suffix trees"));
        // Issue references alone are not bug-fix markers.
        assert!(!is_bugfix_message(b"feat: add pagination (#123)"));
    }

    #[test]
    fn twr_of_no_bugfixes_is_zero() {
        assert_eq!(time_weighted_risk(&[], 0, 100), 0.0);
    }

    #[test]
    fn twr_weights_recent_fixes_higher() {
        // One fix at the very start vs one at head: the recent fix
        // scores ~0.5, the old one ~e^-12.
        let old = time_weighted_risk(&[0], 0, 1_000_000);
        let recent = time_weighted_risk(&[1_000_000], 0, 1_000_000);
        assert!(old < 1e-4, "old fix should decay to ~0, got {old}");
        assert!(
            (recent - 0.5).abs() < 1e-9,
            "fix at head should score 0.5, got {recent}"
        );
        assert!(recent > old);
    }

    #[test]
    fn twr_zero_span_treats_fix_as_now() {
        let v = time_weighted_risk(&[42], 42, 42);
        assert!((v - 0.5).abs() < 1e-9);
    }

    #[test]
    fn twr_is_order_independent() {
        let a = time_weighted_risk(&[10, 500_000, 999_999], 0, 1_000_000);
        let b = time_weighted_risk(&[999_999, 10, 500_000], 0, 1_000_000);
        assert_eq!(a, b);
    }

    #[test]
    fn finalize_ownership_and_minor_contributors() {
        let mut acc = FileAccumulator::default();
        // 100 churned lines total: alice 90, bob 7, carol 3.
        acc.author_lines.insert("alice@x".into(), 90);
        acc.author_lines.insert("bob@x".into(), 7);
        acc.author_lines.insert("carol@x".into(), 3);
        let fh = finalize_file(acc, 0, 0);
        assert_eq!(fh.authors, 3);
        // carol (3%) is minor; bob (7%) is not.
        assert_eq!(fh.minor_contributors, 1);
        assert!((fh.ownership - 0.9).abs() < 1e-9);
    }

    #[test]
    fn finalize_zero_churn_has_defined_ownership() {
        let acc = FileAccumulator::default();
        let fh = finalize_file(acc, 0, 0);
        assert_eq!(fh.minor_contributors, 0);
        assert_eq!(fh.ownership, 0.0);
        assert_eq!(fh.churn_abs(), 0);
    }

    #[test]
    fn age_months_is_head_relative_and_clamped() {
        let fh = FileHistory {
            commit_frequency: 1,
            churn_added: 1,
            churn_removed: 0,
            authors: 1,
            minor_contributors: 0,
            ownership: 1.0,
            last_change_seconds: 0,
            sum_of_coupling: 0,
            bugfix_commits: 0,
            twr: 0.0,
        };
        // One average month after the last change.
        assert!((fh.age_months(2_629_746) - 1.0).abs() < 1e-9);
        // Clock skew (last change "after" head) clamps to zero.
        assert_eq!(fh.age_months(-100), 0.0);
    }
}
