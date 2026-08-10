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
//! detected (git-style `-M50%` similarity tracking, implemented
//! in-crate so it never depends on machine configuration): a renamed
//! file keeps its accumulated history under its head-relative path,
//! and a pure rename churns no lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;

use crate::GitError;
use crate::tree_changes::{
    TreeChange, blob_size, changes_between_trees, count_lines, is_binary, line_diff_counts,
    read_blob_data,
};

/// Average Gregorian month in seconds (30.436875 days), used to express
/// code age in months without depending on calendar arithmetic.
const SECONDS_PER_MONTH: f64 = 2_629_746.0;

/// Commits touching more than this many files are ignored for the
/// sum-of-coupling signal (code-maat / CodeScene changeset noise
/// threshold, research foundation §6.4). Such commits — bulk renames,
/// reformat sweeps, vendored imports — carry no architectural coupling
/// signal. All other metrics still count them.
const MAX_COUPLING_CHANGESET: usize = 30;

/// An author contributing less than this share of a file's added
/// lines is a "minor contributor" (PyDriller's fixed 5% threshold,
/// research foundation §6.3).
const MINOR_CONTRIBUTOR_SHARE: f64 = 0.05;

/// Steepness / decay-window constants of the Time-Weighted Risk
/// logistic: `1 / (1 + e^(-12t + 12))` (Lewis et al., ICSE 2013, with
/// ω hard-coded to 12 as in the paper's deployed variant).
const TWR_STEEPNESS: f64 = 12.0;
const TWR_OMEGA: f64 = 12.0;

/// Blobs larger than this are never materialized for churn counting;
/// they contribute zero lines, mirroring `git log --numstat`, which
/// reports `-` (no line counts) for binary files. Sizes are checked
/// via object headers, so a historically modified multi-gigabyte
/// binary costs two header reads per touching commit instead of
/// exhausting memory.
const MAX_CHURN_BLOB_BYTES: u64 = 8 * 1024 * 1024;

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
    /// Authors who wrote less than 5% of the file's added lines
    /// (`history.minor_contributors`, PyDriller's fixed threshold).
    pub minor_contributors: u64,
    /// Share of the file's added lines written by the single top
    /// author (`history.ownership`), in `[0, 1]`. Only *added* lines
    /// count as authorship — deleting someone else's code is not
    /// writing code. `0` when no lines were ever added.
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
    /// Everyone who touched the file with any change, for
    /// `history.authors`.
    authors: std::collections::HashSet<String>,
    /// Added lines per author — the *authorship* signal driving
    /// ownership and minor-contributor classification. Deletion-only /
    /// rename-only touches deliberately never appear here: a zero
    /// entry would classify the toucher as a sub-5% minor contributor
    /// despite having written nothing.
    author_lines: HashMap<String, u64>,
    last_change_seconds: i64,
    sum_of_coupling: u64,
    /// Committer timestamps of bug-fixing commits, for TWR.
    bugfix_seconds: Vec<i64>,
    /// Whether the *newest walked* change to this path removed the
    /// file. A stranded accumulator whose lineage ended in deletion is
    /// a dead post-rename path reuse, not a parallel branch's edits to
    /// the renamed lineage — it must not be folded into the rename
    /// destination.
    newest_is_deletion: bool,
    /// Whether any accumulated change *created* the file. Required for
    /// the delete-then-recreate identity split: a true re-creation
    /// begins with an addition, while parallel-branch edits walked
    /// before an unrelated branch's deletion are modifications only.
    has_addition: bool,
}

impl FileAccumulator {
    /// Fold another accumulator into this one — used when a rename is
    /// discovered *after* the walk already accumulated changes under
    /// the source path (a parallel branch's later-timestamp commit can
    /// precede the rename in the newest-first order).
    fn merge(&mut self, other: FileAccumulator) {
        self.commit_frequency += other.commit_frequency;
        self.churn_added += other.churn_added;
        self.churn_removed += other.churn_removed;
        self.authors.extend(other.authors);
        for (author, lines) in other.author_lines {
            *self.author_lines.entry(author).or_insert(0) += lines;
        }
        self.last_change_seconds = self.last_change_seconds.max(other.last_change_seconds);
        self.sum_of_coupling += other.sum_of_coupling;
        self.bugfix_seconds.extend(other.bugfix_seconds);
        self.has_addition |= other.has_addition;
    }
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
    // The walked rev's tree, used to tell a rename's *source lineage*
    // apart from a distinct file later re-created at the same path.
    let head_tree = head_commit
        .tree()
        .map_err(|e| GitError::Internal(e.to_string()))?;

    let mut files: HashMap<PathBuf, FileAccumulator> = HashMap::new();
    let mut first_commit_seconds = head_seconds;
    // Rename identity: maps a historical path to the head-relative
    // path it eventually became, so a renamed file accumulates one
    // history entry instead of losing everything before the rename.
    // The newest-first walk sees a rename before the older commits
    // that touched its source path; values stored in the map are
    // always fully resolved, and `resolve_alias` follows chains for
    // multi-rename histories. Rename destinations also get a
    // *boundary*: older direct changes to the destination path belong
    // to a dead prior occupant of that path, and are redirected to a
    // per-boundary tombstone identity so the surviving file never
    // inherits them.
    let mut aliases: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut tombstones: usize = 0;

    let walk = repo
        .rev_walk([head_commit.id])
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
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

        // ── Phase 1: resolve every change against the *pre-commit*
        // alias map, so same-commit rename cycles (an a↔b swap) don't
        // resolve through each other's just-installed aliases.
        let mut targets: Vec<PathBuf> = changes
            .iter()
            .map(|change| resolve_alias(&aliases, &change.path))
            .collect();
        // Paths that are rename *sources* in this commit: a swap's
        // destination is simultaneously a source, and its older
        // changes are a lineage this commit moves elsewhere — not a
        // dead prior occupant to fence off.
        let commit_sources: std::collections::HashSet<&PathBuf> = changes
            .iter()
            .filter_map(|change| change.source_path.as_ref())
            .collect();

        // ── Phase 2: a deletion *older* than an already-accumulated
        // *re-creation* of the same (unaliased) path cuts the lineage:
        // the newer changes belong to a file re-created at that path,
        // and this deletion plus everything older belongs to the dead
        // prior occupant. The newer accumulation must contain an
        // actual addition — a parallel branch's *edits* walked before
        // an unrelated branch's deletion (merge retained the file) are
        // modifications only and must not split the identity.
        for (change, target) in changes.iter().zip(targets.iter_mut()) {
            if change.is_deletion
                && !aliases.contains_key(&change.path)
                && files.get(&change.path).is_some_and(|acc| acc.has_addition)
            {
                tombstones += 1;
                let tombstone = tombstone_path(tombstones);
                aliases.insert(change.path.clone(), tombstone.clone());
                *target = tombstone;
            }
        }

        // ── Phase 3: install rename aliases, boundaries, and stranded
        // merges (all against the phase-1 resolutions).
        for (change, target) in changes.iter().zip(targets.iter()) {
            let Some(source) = &change.source_path else {
                continue;
            };
            if source == target {
                // A rename returning to its own identity (a→b→a):
                // reconnect the lineage by dropping a stale destination
                // boundary, so pre-rename commits flow to the survivor
                // again instead of a tombstone.
                if aliases.get(source).is_some_and(|t| is_tombstone(t)) {
                    aliases.remove(source);
                }
                continue;
            }
            // First-visited (newest) rename wins when parallel branches
            // renamed the same source differently — deterministic,
            // though the losing lineage keeps only its own direct
            // commits. A *tombstone* alias is reclaimable, though: this
            // rename explains where the fenced-off occupant actually
            // went (it was renamed away before the newer occupant
            // arrived).
            if aliases.get(source).is_some_and(|t| !is_tombstone(t)) {
                continue;
            }
            // The alias redirects the *older* commits that are walked
            // after this rename (the pre-rename lineage).
            aliases.insert(source.clone(), target.clone());
            // Anything already accumulated under the source path is
            // *newer* than the rename. Fold it into the surviving
            // identity only when it belongs to the renamed lineage (a
            // parallel branch's edits): a source path that still exists
            // at the walked rev — or whose newer accumulation ended in
            // a deletion — is a distinct file re-created after the
            // rename, and its history must stay its own.
            let stranded_is_lineage = !path_exists_at_head(&head_tree, source)?
                && files.get(source).is_some_and(|acc| !acc.newest_is_deletion);
            if stranded_is_lineage && let Some(stranded) = files.remove(source) {
                files.entry(target.clone()).or_default().merge(stranded);
            }
            // Destination identity boundary: older direct changes to
            // the destination path belong to a dead prior occupant —
            // unless the destination is itself a rename source in this
            // same commit (a swap), in which case its older changes
            // are a live lineage this commit moves elsewhere.
            if !commit_sources.contains(&change.path) {
                tombstones += 1;
                aliases.insert(change.path.clone(), tombstone_path(tombstones));
            }
        }

        // ── Phase 4: accumulate.
        for (change, target) in changes.iter().zip(targets.iter()) {
            let newly_tracked = !files.contains_key(target);
            let acc = files.entry(target.clone()).or_default();
            if newly_tracked {
                // First (newest-walked) change for this path.
                acc.newest_is_deletion = change.is_deletion;
            }
            acc.has_addition |= change.is_addition;
            acc.commit_frequency += 1;
            acc.churn_added += change.added;
            acc.churn_removed += change.removed;
            acc.authors.insert(author.clone());
            // Authorship = added lines only: deleting someone else's
            // code (or renaming a file) must not count as writing
            // code, and a zero-line entry would misclassify the
            // toucher as a minor contributor.
            if change.added > 0 {
                *acc.author_lines.entry(author.clone()).or_insert(0) += change.added;
            }
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
    let authors = acc.authors.len() as u64;
    let total_lines: u64 = acc.author_lines.values().sum();
    let (minor_contributors, ownership) = if total_lines == 0 {
        // A history of pure renames/mode changes/deletions adds no
        // lines; ownership is undefined — report zero rather than
        // dividing by zero.
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
///
/// The result is quantized to 1e-9 before publication: `f64::exp` has
/// no cross-platform bit-exactness guarantee, and TWR is both
/// serialized raw and used as a ranking key. Each summand lies in
/// `[0, 1]` with sub-ULP libm variance, so absorbing everything below
/// a nanounit keeps identical repositories identical across platforms.
fn time_weighted_risk(bugfix_seconds: &[i64], first_seconds: i64, head_seconds: i64) -> f64 {
    if bugfix_seconds.is_empty() {
        return 0.0;
    }
    let span = (head_seconds - first_seconds).max(0) as f64;
    // Sort so the float summation order is independent of the walk
    // order (cross-platform determinism contract).
    let mut times: Vec<i64> = bugfix_seconds.to_vec();
    times.sort_unstable();
    let sum: f64 = times
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
        .sum();
    (sum * 1e9).round() / 1e9
}

/// A single file's change within one commit, with line-level churn.
struct CommitFileChange {
    path: PathBuf,
    /// The pre-rename path when this change is a rename.
    source_path: Option<PathBuf>,
    added: u64,
    removed: u64,
    /// Whether this change removed the file (used to tell a dead
    /// post-rename path reuse apart from parallel-branch lineage
    /// edits — see the stranded-accumulator merge).
    is_deletion: bool,
    /// Whether this change *created* the file (a tree-diff addition).
    /// A delete-then-recreate boundary requires the newer occupant to
    /// have actually been created after the deletion; parallel-branch
    /// edits are modifications and must not trigger a split.
    is_addition: bool,
}

/// Whether `path` names an entry in the walked rev's tree.
fn path_exists_at_head(head_tree: &gix::Tree<'_>, path: &Path) -> Result<bool, GitError> {
    Ok(head_tree
        .lookup_entry_by_path(path)
        .map_err(|e| GitError::Internal(e.to_string()))?
        .is_some())
}

/// A synthetic identity for a dead prior occupant of a rename
/// destination. Starts with a control byte so it can never collide
/// with a real repository path; entries accumulated under it are
/// simply never looked up.
fn tombstone_path(counter: usize) -> PathBuf {
    PathBuf::from(format!("\u{1}tombstone\u{1}{counter}"))
}

/// Whether an alias target is a tombstone (see [`tombstone_path`]).
fn is_tombstone(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with('\u{1}')
}

/// Resolve a historical path to its head-relative identity.
///
/// A *single* lookup, deliberately not chain-following: every value in
/// the map is stored fully resolved at insertion time, and rename
/// destinations later gain terminal *boundary* entries (path →
/// tombstone) that cut off a prior occupant's older changes — chasing
/// chains through such a boundary would misroute a lineage into a
/// tombstone.
fn resolve_alias(aliases: &HashMap<PathBuf, PathBuf>, path: &Path) -> PathBuf {
    aliases
        .get(path)
        .cloned()
        .unwrap_or_else(|| path.to_path_buf())
}

/// Diff `commit` against its first parent (or the empty tree for root
/// commits) with in-crate rename tracking, and compute line-level
/// churn per changed blob.
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
        // Root commit: diff against the empty tree.
        None => None,
    };

    let records = changes_between_trees(repo, parent_tree.as_ref(), &to_tree)?;
    let mut changes = Vec::with_capacity(records.len());
    for change in records {
        let file_change = match change {
            TreeChange::Added { path, oid } => CommitFileChange {
                path,
                source_path: None,
                added: blob_line_count(repo, &oid)?,
                removed: 0,
                is_deletion: false,
                is_addition: true,
            },
            TreeChange::Deleted { path, oid } => CommitFileChange {
                path,
                source_path: None,
                added: 0,
                removed: blob_line_count(repo, &oid)?,
                is_deletion: true,
                is_addition: false,
            },
            TreeChange::Modified {
                path,
                previous_oid,
                oid,
            } => {
                // Mode-only changes keep the same blob and churn no
                // lines — skip the two blob reads.
                let (added, removed) = if previous_oid == oid {
                    (0, 0)
                } else {
                    blob_line_diff(repo, &previous_oid, &oid)?
                };
                CommitFileChange {
                    path,
                    source_path: None,
                    added,
                    removed,
                    is_deletion: false,
                    is_addition: false,
                }
            }
            TreeChange::Renamed {
                path,
                source_path,
                previous_oid,
                oid,
            } => {
                // A perfect rename keeps the blob — zero churn.
                let (added, removed) = if previous_oid == oid {
                    (0, 0)
                } else {
                    blob_line_diff(repo, &previous_oid, &oid)?
                };
                CommitFileChange {
                    path,
                    source_path: Some(source_path),
                    added,
                    removed,
                    is_deletion: false,
                    is_addition: false,
                }
            }
        };
        changes.push(file_change);
    }

    Ok(changes)
}

/// Number of lines in a blob (a trailing fragment without `\n` counts
/// as a line). Oversized and binary blobs count zero lines
/// (numstat-style binary handling); oversized ones are never loaded.
fn blob_line_count(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<u64, GitError> {
    if blob_size(repo, oid)? > MAX_CHURN_BLOB_BYTES {
        return Ok(0);
    }
    let data = read_blob_data(repo, oid)?;
    if is_binary(&data) {
        return Ok(0);
    }
    Ok(count_lines(&data))
}

/// Line-level (added, removed) counts between two blob versions. Pairs
/// with an oversized or binary side churn zero lines — mirroring
/// `git log --numstat`, which reports `-` for binary files, so e.g. a
/// NUL-containing generated revision doesn't count its bytes as
/// "source lines" churned. Oversized blobs are never loaded.
fn blob_line_diff(
    repo: &gix::Repository,
    old: &gix::ObjectId,
    new: &gix::ObjectId,
) -> Result<(u64, u64), GitError> {
    if blob_size(repo, old)? > MAX_CHURN_BLOB_BYTES || blob_size(repo, new)? > MAX_CHURN_BLOB_BYTES
    {
        return Ok((0, 0));
    }
    let old_data = read_blob_data(repo, old)?;
    let new_data = read_blob_data(repo, new)?;
    if is_binary(&old_data) || is_binary(&new_data) {
        return Ok((0, 0));
    }
    Ok(line_diff_counts(&old_data, &new_data))
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
        // 100 added lines total: alice 90, bob 7, carol 3.
        for name in ["alice@x", "bob@x", "carol@x"] {
            acc.authors.insert(name.into());
        }
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
    fn deletion_only_authors_count_as_authors_but_not_minor_contributors() {
        // dave touched the file (pure deletion) — he is an author, but
        // a zero-added entry must not be classified as a sub-5% minor
        // contributor: he wrote nothing, minor or otherwise.
        let mut acc = FileAccumulator::default();
        acc.authors.insert("alice@x".into());
        acc.authors.insert("dave@x".into());
        acc.author_lines.insert("alice@x".into(), 100);
        let fh = finalize_file(acc, 0, 0);
        assert_eq!(fh.authors, 2);
        assert_eq!(fh.minor_contributors, 0);
        assert!((fh.ownership - 1.0).abs() < 1e-9);
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
