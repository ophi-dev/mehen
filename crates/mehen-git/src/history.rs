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

use crate::GitError;
use crate::tree_changes::{
    RENAME_SIMILARITY, TreeChange, blob_lineage_similarity, blob_modifications_between_trees,
    blob_size, changes_between_trees, count_lines, is_binary, line_diff_counts, read_blob_data,
    same_blob_lineage,
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

    /// A tracked blob no walked (non-merge) commit ever touched —
    /// e.g. one created purely by merge conflict resolution. Every
    /// count reads a legitimate zero; the last change is the blob's
    /// creation time (the introducing merge's timestamp, or the
    /// walked rev when unknown), so `age_months` measures the time it
    /// has sat untouched.
    fn untouched(creation_seconds: i64) -> Self {
        Self {
            commit_frequency: 0,
            churn_added: 0,
            churn_removed: 0,
            authors: 0,
            minor_contributors: 0,
            ownership: 0.0,
            last_change_seconds: creation_seconds,
            sum_of_coupling: 0,
            bugfix_commits: 0,
            twr: 0.0,
        }
    }
}

/// Per-file history statistics for an entire repository at a fixed rev.
#[derive(Debug)]
pub struct RepositoryHistory {
    /// Committer timestamp (seconds since epoch) of the walked rev —
    /// the deterministic "now" for age computations.
    pub head_seconds: i64,
    files: HashMap<PathBuf, FileHistory>,
    /// Blob paths present in the walked rev's tree, for
    /// [`tracked_file`](Self::tracked_file).
    head_blobs: std::collections::HashSet<PathBuf>,
    /// Creation timestamps of conflict-resolution-created blobs
    /// (merge-introduced additions, which never accumulate
    /// contributions) — the basis for a synthesized zero entry's age.
    merge_creation_seconds: HashMap<PathBuf, i64>,
    /// Files whose lineage crosses a path this platform cannot
    /// represent (Windows, raw non-UTF-8 rename source): their
    /// earlier commits, churn, and authorship are unreachable here,
    /// so the per-file lookups report them unmeasurable rather than
    /// publishing the truncated remainder as if it were the whole
    /// history. Always empty on Unix.
    truncated_lineages: std::collections::HashSet<PathBuf>,
}

impl RepositoryHistory {
    /// History stats for a repository-relative path, if any walked
    /// commit touched it — including files *deleted* at the walked
    /// rev, whose history diff baselines still read.
    pub fn file(&self, path: &Path) -> Option<&FileHistory> {
        if self.truncated_lineages.contains(path) {
            // The lineage crosses a platform-unrepresentable path:
            // whatever accumulated is a truncated fabrication, not
            // this file's history.
            return None;
        }
        self.files.get(path)
    }

    /// Like [`file`](Self::file), but only for paths that exist as
    /// blobs at the walked rev. Workspace-oriented consumers (ranking
    /// files found on disk) must use this: an untracked file — or a
    /// symlink — occupying a path whose tracked blob HEAD deleted has
    /// no history of its own, and returning the dead occupant's would
    /// assign it someone else's commits, churn, and authors.
    ///
    /// A tracked blob *without* an accumulator (created purely by
    /// merge conflict resolution, never touched by a walked non-merge
    /// commit) reads a legitimate all-zero history rather than `None`
    /// — it is measured (nothing ever happened to it), not
    /// unmeasurable like an untracked path.
    pub fn tracked_file(&self, path: &Path) -> Option<FileHistory> {
        if !self.head_blobs.contains(path) || self.truncated_lineages.contains(path) {
            return None;
        }
        Some(self.files.get(path).cloned().unwrap_or_else(|| {
            // Age measures from the creating merge — pinning it to
            // the walked rev would make an old, untouched blob read
            // as newly changed forever.
            FileHistory::untouched(
                self.merge_creation_seconds
                    .get(path)
                    .copied()
                    .unwrap_or(self.head_seconds),
            )
        }))
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

/// One walked change contributing to a file identity. Contributions
/// stay un-folded until the walk finishes: a rename discovered later
/// (walk order is newest-first) may have to split everything
/// accumulated under its source path between the renamed lineage and
/// a newer occupant of the vacated path, and that partition is by
/// commit ancestry — folded counters could not be taken apart again.
#[derive(Debug, Clone)]
struct Contribution {
    commit: gix::ObjectId,
    /// Committer timestamp in seconds.
    seconds: i64,
    author: std::sync::Arc<[u8]>,
    added: u64,
    removed: u64,
    /// Other files changed in the same commit (`history.coupling`).
    coupled_others: u64,
    /// Whether the changeset was small enough to count for coupling.
    coupling_eligible: bool,
    is_bugfix: bool,
    /// Whether this change *created* the file (a tree-diff addition).
    is_addition: bool,
}

/// Per-file accumulator filled during the walk, finalized into
/// [`FileHistory`] once repository-wide bounds (first/head commit
/// times) are known.
#[derive(Debug, Default)]
struct FileAccumulator {
    contributions: Vec<Contribution>,
    /// Whether any contribution created the file — cached because the
    /// delete-then-recreate boundary check runs once per deletion.
    has_addition: bool,
}

impl FileAccumulator {
    fn push(&mut self, contribution: Contribution) {
        self.has_addition |= contribution.is_addition;
        self.contributions.push(contribution);
    }

    /// Fold another accumulator into this one — used when a rename is
    /// discovered *after* the walk already accumulated changes under
    /// the source path (a parallel branch's later-timestamp commit can
    /// precede the rename in the newest-first order).
    fn merge(&mut self, other: FileAccumulator) {
        self.has_addition |= other.has_addition;
        self.contributions.extend(other.contributions);
    }

    fn is_empty(&self) -> bool {
        self.contributions.is_empty()
    }
}

/// Walk the full history reachable from `rev` (first-parent diffs;
/// merges contribute no churn but may install rename identity) and
/// accumulate per-file process statistics.
///
/// The cost is one tree diff per commit plus one line diff per
/// modified blob; results depend only on the repository state at
/// `rev`.
pub fn collect_history(repo: &gix::Repository, rev: &str) -> Result<RepositoryHistory, GitError> {
    // Accelerate the walk's many commit/blob lookups (gix maintainer
    // guidance): an in-memory object cache for repeated reads, and one
    // reusable revision graph for all ancestry queries.
    let mut repo = repo.clone();
    repo.object_cache_size_if_unset(4 * 1024 * 1024);
    let repo = &repo;
    let commit_graph_cache = repo
        .commit_graph_if_enabled()
        .map_err(|e| GitError::Internal(e.to_string()))?;
    let mut ancestry: AncestryGraph<'_, '_> = repo.revision_graph(commit_graph_cache.as_ref());

    let head_id = repo
        .rev_parse_single(rev)
        .map_err(|_| GitError::RefNotFound(rev.to_string()))?;
    let head_commit = head_id
        .object()
        .map_err(|e| GitError::Internal(e.to_string()))?
        .peel_to_commit()
        .map_err(|e| GitError::Internal(e.to_string()))?;
    let head_seconds = commit_seconds(&head_commit)?;
    let head_blobs = tree_blob_paths(&head_commit)?;

    let mut files: HashMap<FileIdentity, FileAccumulator> = HashMap::new();
    let mut first_commit_seconds = head_seconds;
    // Rename identity: maps a historical path to entries describing
    // what it became (a head-relative path, or a tombstone standing in
    // for a dead prior occupant), each scoped by the commit that
    // installed it — see `resolve_alias` for the ancestry gate and
    // preference order. Values are stored fully resolved; resolution
    // is a single lookup, deliberately not chain-following (chasing
    // chains through a later destination boundary would misroute a
    // lineage into a tombstone). Rename destinations get a *boundary*
    // entry: older direct changes to the destination path belong to a
    // dead prior occupant, and are redirected to a per-boundary
    // tombstone so the surviving file never inherits them.
    let mut aliases: HashMap<PathBuf, Vec<AliasEntry>> = HashMap::new();
    let mut tombstones: usize = 0;
    // Conflict-resolution-created blobs (merge-introduced additions)
    // never accumulate contributions, but their *creation time* is
    // real history: `tracked_file` synthesizes their zero entry from
    // it so `history.age_months` measures time since the creating
    // merge instead of reading an eternal 0. Newest-first walk: the
    // first-seen merge addition for a path is the one that created
    // the blob HEAD still carries.
    let mut merge_creation_seconds: HashMap<PathBuf, i64> = HashMap::new();
    // Files whose lineage crosses a path this platform cannot
    // represent (Windows, raw non-UTF-8 rename source): their earlier
    // history is unreachable here, and publishing the truncated
    // remainder would fabricate a young, single-author file — the
    // per-file lookups report them unmeasurable instead. Always empty
    // on Unix.
    let mut truncated_lineages: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    // Every walked merge `(id, parents)` — including ones that
    // introduced no identity changes. The phase-2 delete-then-recreate
    // cut consults this to recognize a *bypassed* deletion: one whose
    // path survived around it through another parent of a downstream
    // merge (see below). Ids only; no tree work is done here.
    let mut walked_merges: Vec<(gix::ObjectId, Vec<gix::ObjectId>)> = Vec::new();

    // Date-order traversal (`git rev-list --date-order`): commits come
    // newest-first by timestamp, but crucially *no parent is emitted
    // before all of its children* — the rename-alias machinery depends
    // on seeing every descendant (and its renames) before an ancestor,
    // which plain commit-time ordering cannot guarantee when clock
    // skew makes an ancestor's timestamp newer than a descendant's.
    let walk = gix::traverse::commit::topo::Builder::from_iters(
        repo.objects.clone(),
        [head_commit.id],
        None::<Vec<gix::ObjectId>>,
    )
    .sorting(gix::traverse::commit::topo::Sorting::DateOrder)
    .build()
    .map_err(|e| GitError::Internal(e.to_string()))?;

    for info in walk {
        let info = info.map_err(|e| GitError::Internal(e.to_string()))?;
        let is_merge = info.parent_ids.len() > 1;

        let commit = repo
            .find_object(info.id)
            .map_err(|e| GitError::Internal(e.to_string()))?
            .peel_to_commit()
            .map_err(|e| GitError::Internal(e.to_string()))?;

        // Merge commits contribute no churn: their first-parent diff
        // would double-count every line already attributed to the
        // merged commits (matching `git log --no-merges` / code-maat).
        // But a merge can still *create identity*: conflict resolution
        // may commit a tree that renames a file present in a parent
        // (`a.rs` in the parents, `b.rs` in the merged tree) or
        // creates a file at a brand-new path. Such merge-introduced
        // changes — destination absent from every parent tree — must
        // install aliases and boundaries like any other commit, or
        // older commits accumulate under vacated paths while the
        // surviving files read an empty (or worse, a dead prior
        // occupant's) history.
        let (changes, non_blob_changes, commit_truncated) = if is_merge {
            walked_merges.push((info.id, info.parent_ids.iter().copied().collect()));
            let mut merge_truncated: Vec<PathBuf> = Vec::new();
            let introduced = merge_introduced_changes(repo, &commit, &mut merge_truncated)?;
            if introduced.is_empty() && merge_truncated.is_empty() {
                continue;
            }
            // Identity only — merges never reach the coupling math.
            (introduced, 0, merge_truncated)
        } else {
            diff_against_first_parent(repo, &commit)?
        };
        // Truncated-lineage markers are keyed by the *live* identity:
        // the marker's path is the rename destination as of this
        // commit, but a newer (already-walked) rename may have moved
        // the file on — resolving through the pre-commit alias map
        // lands the marker on the path `tracked_file` will actually
        // be asked about. A tombstone resolution means the truncated
        // line is already fenced off as dead: nothing to mark.
        for path in commit_truncated {
            if let (FileIdentity::Path(live), _, _) =
                resolve_alias(repo, &mut ancestry, &aliases, &path, info.id, false)?
            {
                truncated_lineages.insert(live);
            }
        }
        if is_merge && changes.is_empty() {
            continue;
        }

        let seconds = commit_seconds(&commit)?;
        if !is_merge {
            // Merges never accumulate, so they don't bound the TWR
            // normalization window either (as before this walk saw
            // identity-bearing merges at all).
            first_commit_seconds = first_commit_seconds.min(seconds);
        }
        let author: std::sync::Arc<[u8]> = std::sync::Arc::from(author_identity(&commit)?);
        let is_bugfix = is_bugfix_message(commit.message_raw_sloppy());

        // ── Phase 1: resolve every change against the *pre-commit*
        // alias map, so same-commit rename cycles (an a↔b swap) don't
        // resolve through each other's just-installed aliases. Each
        // resolution remembers which entry applied (if any) so the
        // consumed flag lands on the right one.
        let mut targets: Vec<FileIdentity> = Vec::with_capacity(changes.len());
        let mut used_entries: Vec<Option<usize>> = Vec::with_capacity(changes.len());
        let mut floor_gated_entries: Vec<Option<usize>> = Vec::with_capacity(changes.len());
        for change in &changes {
            let (target, used, floor_gated) = resolve_alias(
                repo,
                &mut ancestry,
                &aliases,
                &change.path,
                info.id,
                change.is_addition,
            )?;
            targets.push(target);
            used_entries.push(used);
            floor_gated_entries.push(floor_gated);
        }
        // Paths that are rename *sources* in this commit: a swap's
        // destination is simultaneously a source, and its older
        // changes are a lineage this commit moves elsewhere — not a
        // dead prior occupant to fence off.
        let commit_sources: std::collections::HashSet<&PathBuf> = changes
            .iter()
            .filter_map(|change| change.source_path.as_ref())
            .collect();

        // ── Phase 2: a deletion *older* than a re-creation of the
        // same path cuts the lineage: the re-creation (and everything
        // on its descendants) belongs to a new file, and this deletion
        // plus everything older belongs to the dead prior occupant.
        // The proof must be ancestry-precise: either an accumulated
        // *addition* at this path from a descendant of the deletion,
        // or a consumed alias entry installed on the deletion's own
        // descendant line (the recreation was redirected through it).
        // A parallel branch's *edits* walked earlier are modifications
        // only and must not split the identity.
        for ((change, target), used) in changes.iter().zip(targets.iter_mut()).zip(&used_entries) {
            if !change.is_deletion || used.is_some() {
                continue;
            }
            let mut recreation_seen = false;
            if let Some(acc) = files.get(&FileIdentity::Path(change.path.clone())) {
                for c in &acc.contributions {
                    if c.is_addition && is_descendant_of(repo, &mut ancestry, info.id, c.commit)? {
                        recreation_seen = true;
                        break;
                    }
                }
            }
            if !recreation_seen && let Some(entries) = aliases.get(&change.path) {
                for entry in entries {
                    if entry.consumed
                        && !entry.from_discarded_occupant
                        && entry.applies_to(repo, &mut ancestry, info.id, false)?
                    {
                        recreation_seen = true;
                        break;
                    }
                }
            }
            if !recreation_seen {
                continue;
            }
            // A recreation-cut deletion may still be *bypassed*: an
            // already-walked merge kept the path alive through another
            // parent whose line never dropped it, discarding this
            // deletion's branch. Then the recreation on this line is a
            // dead occupant — not the live file's birth — and cutting
            // here would tombstone the shared pre-branch creation away
            // from the survivor. When the discarded recreation's blob
            // differs from some endpoint the merge-time fences catch
            // it; a recreation byte-identical to the surviving blob is
            // invisible to every tree diff and only this walk-level
            // check can see it. The bypass requires exact
            // continuation: a merge parent that does not descend from
            // this deletion, holds the very blob the merged tree
            // keeps, and carries it over an uninterrupted line.
            let mut bypass: Option<(gix::ObjectId, Option<gix::ObjectId>)> = None;
            'merges: for (merge_id, parents) in &walked_merges {
                let Some(merge_oid) = blob_oid_in_commit(repo, *merge_id, &change.path)? else {
                    continue;
                };
                for q in parents {
                    // The parent that carried this deletion into the
                    // merge.
                    if !is_descendant_of(repo, &mut ancestry, info.id, *q)? {
                        continue;
                    }
                    for s in parents {
                        if s == q
                            || is_descendant_of(repo, &mut ancestry, info.id, *s)?
                            || blob_oid_in_commit(repo, *s, &change.path)? != Some(merge_oid)
                        {
                            continue;
                        }
                        let floor = match repo.merge_base(*s, *q) {
                            Ok(base) => Some(base.detach()),
                            Err(gix::repository::merge_base::Error::NotFound { .. }) => None,
                            Err(e) => return Err(GitError::Internal(e.to_string())),
                        };
                        if let Some(floor) = floor
                            && path_deleted_in_range(repo, *s, floor, &change.path)?
                        {
                            continue;
                        }
                        bypass = Some((*q, floor));
                        break 'merges;
                    }
                }
            }
            tombstones += 1;
            let tombstone = FileIdentity::Tombstone(tombstones);
            if let Some((scope, floor)) = bypass {
                // Install the fence the merge would have installed had
                // the recreation been visible to its diffs, and move
                // the already-accumulated dead-line contributions (the
                // recreation and its descendants up to the discarded
                // parent) behind it. The deletion itself stays a touch
                // on the surviving path, exactly like a merge-time
                // fence leaves it.
                let path_id = FileIdentity::Path(change.path.clone());
                if let Some(acc) = files.get_mut(&path_id) {
                    let contributions = std::mem::take(&mut acc.contributions);
                    let mut kept = Vec::with_capacity(contributions.len());
                    let mut moved: Vec<Contribution> = Vec::new();
                    for c in contributions {
                        if is_descendant_of(repo, &mut ancestry, info.id, c.commit)?
                            && is_descendant_of(repo, &mut ancestry, c.commit, scope)?
                        {
                            moved.push(c);
                        } else {
                            kept.push(c);
                        }
                    }
                    acc.has_addition = kept.iter().any(|c| c.is_addition);
                    acc.contributions = kept;
                    if acc.is_empty() {
                        files.remove(&path_id);
                    }
                    if !moved.is_empty() {
                        let dead = files.entry(tombstone.clone()).or_default();
                        for c in moved {
                            dead.push(c);
                        }
                    }
                }
                let mut entry = AliasEntry::new(tombstone, vec![scope]);
                entry.floor = floor;
                entry.from_discarded_occupant = true;
                aliases.entry(change.path.clone()).or_default().push(entry);
            } else {
                aliases
                    .entry(change.path.clone())
                    .or_default()
                    .push(AliasEntry::new(tombstone.clone(), vec![info.id]));
                *target = tombstone;
            }
        }

        // ── Phase 3: install rename aliases, boundaries, and stranded
        // merges (all against the phase-1 resolutions). Entry vectors
        // only ever grow or mark entries consumed here — removing
        // entries would invalidate the indices phase 4 uses to mark
        // consumption.
        let mut new_boundaries: Vec<(PathBuf, FileIdentity)> = Vec::new();
        // Entries a change of this very commit resolved through: such
        // an entry is *in use* — e.g. a same-commit replacement's
        // creation resolving into a later deletion's fence — and must
        // not be reclaimed or retired by this commit's renames.
        let mut in_use: HashMap<&PathBuf, std::collections::HashSet<usize>> = HashMap::new();
        for (change, used) in changes.iter().zip(&used_entries) {
            if let Some(idx) = used {
                in_use.entry(&change.path).or_default().insert(*idx);
            }
        }
        for (change, target) in changes.iter().zip(targets.iter()) {
            let Some(source) = &change.source_path else {
                continue;
            };
            if matches!(target, FileIdentity::Path(p) if p == source) {
                // A rename returning to its own identity (a→b→a):
                // reconnect the lineage by retiring stale destination
                // boundaries, so pre-rename commits flow to the
                // survivor again instead of a tombstone.
                if let Some(entries) = aliases.get_mut(source) {
                    for (idx, entry) in entries.iter_mut().enumerate() {
                        if matches!(entry.target, FileIdentity::Tombstone(_))
                            && !in_use.get(source).is_some_and(|s| s.contains(&idx))
                        {
                            entry.consumed = true;
                        }
                    }
                }
                continue;
            }
            // Install alongside any existing entries: ancestry scoping
            // disambiguates at resolution time. When parallel branches
            // renamed the same source differently and the merge kept
            // both, each branch's pre-rename edits are ancestors of
            // only their own rename and route to the right survivor;
            // for shared ancestors (the common pre-branch lineage) the
            // first-visited entry wins — deterministic under the
            // deterministic walk order, and the shared history is
            // counted once rather than duplicated into both survivors.
            // A *tombstone* entry is reclaimable outright — unless a
            // change of this same commit resolved through it (the
            // fence is in active use), or this very commit installed
            // it (a same-commit deletion boundary, e.g. a merge that
            // both moves one parent's file and fences another
            // parent's dead occupant of the same path) — because this
            // rename explains where the fenced-off occupant actually
            // went (it was renamed away, not merely deleted): its
            // fenced contributions move to the rename target and the
            // fence retires.
            let entries = aliases.entry(source.clone()).or_default();
            let mut reclaimed = FileAccumulator::default();
            for (idx, entry) in entries.iter_mut().enumerate() {
                if entry.consumed
                    || !matches!(entry.target, FileIdentity::Tombstone(_))
                    || in_use.get(source).is_some_and(|s| s.contains(&idx))
                    || entry.scopes.contains(&info.id)
                {
                    continue;
                }
                if let Some(fenced) = files.remove(&entry.target) {
                    reclaimed.merge(fenced);
                }
                entry.consumed = true;
            }
            // The alias redirects the *older* commits that are walked
            // after this rename (the pre-rename lineage). A
            // merge-introduced rename carries the scopes of the
            // parents that supplied the source (and an addition floor
            // when they span several parents); everything else is
            // scoped to this commit.
            let mut alias_entry = AliasEntry::new(
                target.clone(),
                change.alias_scopes.clone().unwrap_or_else(|| vec![info.id]),
            );
            alias_entry.addition_floor = change.alias_addition_floor;
            entries.push(alias_entry);
            if !reclaimed.is_empty() {
                files.entry(target.clone()).or_default().merge(reclaimed);
            }
            // Anything already accumulated under the source path is
            // *newer in walk order* than this rename — but it can mix
            // two occupants: concurrent branches' edits belong to the
            // renamed lineage (they edited the file that moved away),
            // while a re-creation of the vacated path (and the edits
            // in its descendants) is a distinct newer file. Commit
            // ancestry separates the two: descendants of this rename
            // postdate it on its own line, and a concurrent *addition*
            // is a parallel branch's re-creation (an addition cannot
            // edit the moved file) — both belong to the new occupant,
            // as do edits descending from such a re-creation. Anything
            // else concurrent edited the renamed lineage.
            let source_id = FileIdentity::Path(source.clone());
            if let Some(stranded) = files.remove(&source_id) {
                let mut occupant = FileAccumulator::default();
                let mut recreations: Vec<gix::ObjectId> = Vec::new();
                let mut pending: Vec<Contribution> = Vec::new();
                for contribution in stranded.contributions {
                    if contribution.is_addition
                        && !is_descendant_of(repo, &mut ancestry, info.id, contribution.commit)?
                    {
                        recreations.push(contribution.commit);
                        occupant.push(contribution);
                    } else if is_descendant_of(repo, &mut ancestry, info.id, contribution.commit)? {
                        occupant.push(contribution);
                    } else {
                        pending.push(contribution);
                    }
                }
                let mut lineage = FileAccumulator::default();
                'pending: for contribution in pending {
                    for recreation in &recreations {
                        if is_descendant_of(repo, &mut ancestry, *recreation, contribution.commit)?
                        {
                            occupant.push(contribution);
                            continue 'pending;
                        }
                    }
                    lineage.push(contribution);
                }
                if !occupant.is_empty() {
                    files.insert(source_id, occupant);
                }
                if !lineage.is_empty() {
                    files.entry(target.clone()).or_default().merge(lineage);
                }
            }
            // Destination identity boundary: older direct changes to
            // the destination path belong to a dead prior occupant —
            // unless the destination is itself a rename source in this
            // same commit (a swap), in which case its older changes
            // are a live lineage this commit moves elsewhere, or the
            // change opts out (a merge rename converging onto a
            // parent-owned destination whose older history is real).
            if change.install_destination_boundary && !commit_sources.contains(&change.path) {
                tombstones += 1;
                let boundary = FileIdentity::Tombstone(tombstones);
                aliases
                    .entry(change.path.clone())
                    .or_default()
                    .push(AliasEntry::new(boundary.clone(), vec![info.id]));
                new_boundaries.push((change.path.clone(), boundary));
            }
        }
        // A commit that renames one file *over* another emits both
        // `Renamed(a → b)` and `Deleted(b)`; the deletion is the old
        // occupant of `b` dying, and belongs behind the destination
        // boundary just installed — not in the surviving lineage,
        // where its removed lines, author, and commit would pollute
        // the new `b`'s history.
        for ((change, target), used) in changes.iter().zip(targets.iter_mut()).zip(&used_entries) {
            if !change.is_deletion || used.is_some() {
                continue;
            }
            if let Some((_, boundary)) =
                new_boundaries.iter().find(|(path, _)| *path == change.path)
            {
                *target = boundary.clone();
            }
        }

        // ── Phase 4: accumulate. Merge commits install identity only —
        // their churn is deliberately excluded (see above).
        if is_merge {
            // A merge-introduced *addition* (conflict resolution
            // creating a file at a path absent from every parent)
            // establishes a fresh identity: older commits touching
            // the path belong to a dead prior occupant. Merges never
            // accumulate, so the usual delete-then-recreate fence
            // (which needs an accumulated creation as proof) can
            // never fire for them — install the boundary eagerly.
            for (change, target) in changes.iter().zip(targets.iter()) {
                if let Some((scope, floor)) = change.discarded_occupant_fence {
                    tombstones += 1;
                    let mut entry =
                        AliasEntry::new(FileIdentity::Tombstone(tombstones), vec![scope]);
                    entry.floor = floor;
                    entry.from_discarded_occupant = true;
                    aliases.entry(change.path.clone()).or_default().push(entry);
                }
                if change.is_addition {
                    // Record the creation time only when the addition
                    // resolves to a live path — and key it by that
                    // *resolved* path: a parallel merge's discarded
                    // occupant resolves through its fence to a
                    // tombstone (skipped), and a merge-created file
                    // later renamed by another merge resolves to its
                    // final name, which is the key `tracked_file`
                    // will look up.
                    if let FileIdentity::Path(live) = target {
                        merge_creation_seconds
                            .entry(live.clone())
                            .or_insert(seconds);
                    }
                    tombstones += 1;
                    aliases
                        .entry(change.path.clone())
                        .or_default()
                        .push(AliasEntry::new(
                            FileIdentity::Tombstone(tombstones),
                            vec![info.id],
                        ));
                }
            }
            continue;
        }
        // The changeset size for coupling includes every changed leaf
        // path — symlinks and submodule pointer bumps co-change like
        // any other file even though they carry no analyzable text —
        // so both the noise threshold and the "other files in this
        // commit" count use the full cardinality.
        let coupling_paths = changes.len() + non_blob_changes;
        let coupling_eligible = coupling_paths <= MAX_COUPLING_CHANGESET;
        // The "other files in this commit" count is the same for every
        // file in the changeset.
        let coupled_others = coupling_paths.saturating_sub(1) as u64;
        for (index, ((change, target), used)) in changes
            .iter()
            .zip(targets.iter())
            .zip(&used_entries)
            .enumerate()
        {
            // An addition that resolved *through* an alias entry is
            // the redirected occupant's birth: every deletion or
            // rename of this path walked from here on is older than
            // that birth and belongs to a previous occupant, so mark
            // the applied entry consumed (see `AliasEntry`).
            if change.is_addition
                && let Some(idx) = used
                && let Some(entries) = aliases.get_mut(&change.path)
            {
                entries[*idx].consumed = true;
            }
            // A *floor-gated* addition is a recreated occupant's birth
            // on a scoped line (see `AliasEntry::addition_floor`). Its
            // edits — descendants of this addition, inside the entry's
            // scopes — were walked earlier and routed through the
            // alias into the rename target; pull them back to the
            // occupant's own identity now that its birth proves they
            // belong to it. Genuine parallel edits of the moved file
            // are concurrent with (not descendants of) the recreation
            // and stay put, as do the rename target's own commits
            // (outside the entry's scopes).
            if change.is_addition
                && let Some(idx) = floor_gated_entries[index]
            {
                let (entry_target, entry_scopes) = {
                    let entry = &aliases[&change.path][idx];
                    (entry.target.clone(), entry.scopes.clone())
                };
                let mut moved: Vec<Contribution> = Vec::new();
                if let Some(acc) = files.get_mut(&entry_target) {
                    let contributions = std::mem::take(&mut acc.contributions);
                    let mut kept = Vec::with_capacity(contributions.len());
                    for c in contributions {
                        let mut is_occupant_edit =
                            is_descendant_of(repo, &mut ancestry, info.id, c.commit)?;
                        if is_occupant_edit {
                            let mut in_scope = false;
                            for scope in &entry_scopes {
                                if is_descendant_of(repo, &mut ancestry, c.commit, *scope)? {
                                    in_scope = true;
                                    break;
                                }
                            }
                            is_occupant_edit = in_scope;
                        }
                        if is_occupant_edit {
                            moved.push(c);
                        } else {
                            kept.push(c);
                        }
                    }
                    acc.has_addition = kept.iter().any(|c| c.is_addition);
                    acc.contributions = kept;
                    if acc.is_empty() {
                        files.remove(&entry_target);
                    }
                }
                if !moved.is_empty() {
                    let occupant = files.entry(target.clone()).or_default();
                    for c in moved {
                        occupant.push(c);
                    }
                }
            }
            files.entry(target.clone()).or_default().push(Contribution {
                commit: info.id,
                seconds,
                author: author.clone(),
                added: change.added,
                removed: change.removed,
                coupled_others,
                coupling_eligible,
                is_bugfix,
                is_addition: change.is_addition,
            });
        }
    }

    let files = files
        .into_iter()
        .filter_map(|(identity, acc)| match identity {
            FileIdentity::Path(path) => {
                Some((path, finalize_file(acc, first_commit_seconds, head_seconds)))
            }
            // Dead prior occupants: their fenced-off accumulations
            // exist only so live lineages don't inherit them.
            FileIdentity::Tombstone(_) => None,
        })
        .collect();

    Ok(RepositoryHistory {
        head_seconds,
        files,
        head_blobs,
        merge_creation_seconds,
        truncated_lineages,
    })
}

/// Blob paths in a commit's tree (recursive).
fn tree_blob_paths(
    commit: &gix::Commit<'_>,
) -> Result<std::collections::HashSet<PathBuf>, GitError> {
    let internal = |e: &dyn std::error::Error| GitError::Internal(e.to_string());
    let tree = commit.tree().map_err(|e| internal(&e))?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(|e| internal(&e))?;
    Ok(recorder
        .records
        .into_iter()
        .filter(|entry| entry.mode.is_blob())
        .filter_map(|entry| crate::tree_changes::path_from_git(&entry.filepath))
        .collect())
}

/// Fold a walk-time accumulator into the public [`FileHistory`].
fn finalize_file(acc: FileAccumulator, first_seconds: i64, head_seconds: i64) -> FileHistory {
    let mut churn_added = 0u64;
    let mut churn_removed = 0u64;
    let mut last_change_seconds = i64::MIN;
    let mut sum_of_coupling = 0u64;
    let mut bugfix_seconds: Vec<i64> = Vec::new();
    let mut authors: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    // Added lines per author — the *authorship* signal driving
    // ownership and minor-contributor classification. Deletion-only /
    // rename-only touches deliberately never appear here: a zero
    // entry would classify the toucher as a sub-5% minor contributor
    // despite having written nothing.
    let mut author_lines: HashMap<&[u8], u64> = HashMap::new();
    for c in &acc.contributions {
        churn_added += c.added;
        churn_removed += c.removed;
        last_change_seconds = last_change_seconds.max(c.seconds);
        if c.coupling_eligible {
            sum_of_coupling += c.coupled_others;
        }
        if c.is_bugfix {
            bugfix_seconds.push(c.seconds);
        }
        authors.insert(&c.author);
        if c.added > 0 {
            *author_lines.entry(&c.author).or_insert(0) += c.added;
        }
    }

    let authors = authors.len() as u64;
    let total_lines: u64 = author_lines.values().sum();
    let (minor_contributors, ownership) = if total_lines == 0 {
        // A history of pure renames/mode changes/deletions adds no
        // lines; ownership is undefined — report zero rather than
        // dividing by zero.
        (0, 0.0)
    } else {
        let total = total_lines as f64;
        let minor = author_lines
            .values()
            .filter(|&&lines| (lines as f64) / total < MINOR_CONTRIBUTOR_SHARE)
            .count() as u64;
        let top = author_lines.values().copied().max().unwrap_or(0);
        (minor, (top as f64) / total)
    };

    FileHistory {
        commit_frequency: acc.contributions.len() as u64,
        churn_added,
        churn_removed,
        authors,
        minor_contributors,
        ownership,
        // Signed: valid raw commit metadata can be pre-epoch, and
        // clamping the timestamp itself would misreport `age_months`
        // for a pre-epoch analyzed revision (the elapsed difference
        // is clamped where it is computed instead). `i64::MIN` only
        // survives an accumulator with no contributions; pin it to
        // the walked rev so the degenerate age reads 0.
        last_change_seconds: if last_change_seconds == i64::MIN {
            head_seconds
        } else {
            last_change_seconds
        },
        sum_of_coupling,
        bugfix_commits: bugfix_seconds.len() as u64,
        twr: time_weighted_risk(&bugfix_seconds, first_seconds, head_seconds),
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
    // Saturating: raw commit metadata can carry arbitrary i64
    // timestamps (git objects can be written directly), and a
    // repository spanning extreme negative and positive values would
    // overflow a plain subtraction — a debug-build panic mid-walk, a
    // silently wrapped TWR in release.
    let span = head_seconds.saturating_sub(first_seconds).max(0) as f64;
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
                ((s.saturating_sub(first_seconds).max(0) as f64) / span).clamp(0.0, 1.0)
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
    /// For merge-introduced renames: the parent commits whose lineage
    /// the rename describes (parents whose trees contain the source).
    /// `None` for ordinary changes — the walked commit itself scopes
    /// the alias.
    alias_scopes: Option<Vec<gix::ObjectId>>,
    /// Whether the rename installs a destination identity boundary.
    /// True everywhere except a merge rename onto a path some parent
    /// already owns: that parent's older history at the destination
    /// is legitimate lineage converging into the merged file, not a
    /// dead prior occupant to fence off.
    install_destination_boundary: bool,
    /// See [`AliasEntry::addition_floor`] — set for merge renames
    /// whose scopes span several parents.
    alias_addition_floor: Option<gix::ObjectId>,
    /// A same-path occupant fence a merge must install: `(scope,
    /// floor)` for a parent whose version of this path the merge
    /// discarded in favor of another parent's continuation. Older
    /// commits on that parent's post-divergence line describe the
    /// discarded occupant, not the survivor.
    discarded_occupant_fence: Option<(gix::ObjectId, Option<gix::ObjectId>)>,
}

/// A reusable revision graph for merge-base queries, per the gix
/// maintainer's guidance (GitoxideLabs/gitoxide#2914): reusing one
/// graph across queries amortizes commit lookups (and leverages the
/// commit-graph file when present) instead of re-walking from scratch
/// on every ancestry check.
type AncestryGraph<'repo, 'cache> = gix::revwalk::Graph<
    'repo,
    'cache,
    gix::revwalk::graph::Commit<gix::revision::plumbing::merge_base::Flags>,
>;

/// Whether `commit` is a descendant of `ancestor` (equal ids count).
///
/// Used to partition an accumulator stranded at a rename source: a
/// contribution from a descendant of the rename commit postdates the
/// rename on its own line (the path was re-created there), while a
/// concurrent contribution edited the file that moved away. Rename
/// events with stranded contributions are rare, and the shared graph
/// caches commit lookups across queries, so the cost stays negligible
/// next to the per-commit tree diffs.
fn is_descendant_of(
    repo: &gix::Repository,
    graph: &mut AncestryGraph<'_, '_>,
    ancestor: gix::ObjectId,
    commit: gix::ObjectId,
) -> Result<bool, GitError> {
    if ancestor == commit {
        return Ok(true);
    }
    match repo.merge_base_with_graph(ancestor, commit, graph) {
        Ok(base) => Ok(base.detach() == ancestor),
        // Disjoint histories (e.g. an orphan branch): not an ancestor.
        Err(gix::repository::merge_base_with_graph::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(GitError::Internal(e.to_string())),
    }
}

/// Whether `path` was deleted on `tip`'s *first-parent chain* between
/// `base` and `tip`: a present→absent flip along the candidate
/// parent's own line is an identity boundary, even when the path is
/// later re-created with byte-identical contents — endpoint blobs
/// alone cannot see the interruption. Deliberately not a full range
/// walk: a side branch merged into the candidate may have deleted the
/// path while the candidate's own copy survived uninterrupted (the
/// merge kept it), and that side deletion says nothing about the
/// candidate's lineage. The chain reads commit headers and per-commit
/// tree lookups only, and runs just for the rare multi-parent-source
/// merge rename.
/// The blob OID at `path` in `commit`'s tree (`None`: absent or not a
/// blob).
fn blob_oid_in_commit(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    path: &Path,
) -> Result<Option<gix::ObjectId>, GitError> {
    let internal = |e: &dyn std::error::Error| GitError::Internal(e.to_string());
    Ok(repo
        .find_object(commit)
        .map_err(|e| internal(&e))?
        .peel_to_commit()
        .map_err(|e| internal(&e))?
        .tree()
        .map_err(|e| internal(&e))?
        .lookup_entry_by_path(path)
        .map_err(|e| internal(&e))?
        .filter(|entry| entry.mode().is_blob())
        .map(|entry| entry.oid().to_owned()))
}

fn path_deleted_in_range(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    base: gix::ObjectId,
    path: &Path,
) -> Result<bool, GitError> {
    let internal = |e: &dyn std::error::Error| GitError::Internal(e.to_string());
    let blob_oid = |id: gix::ObjectId| -> Result<Option<gix::ObjectId>, GitError> {
        Ok(repo
            .find_object(id)
            .map_err(|e| internal(&e))?
            .peel_to_commit()
            .map_err(|e| internal(&e))?
            .tree()
            .map_err(|e| internal(&e))?
            .lookup_entry_by_path(path)
            .map_err(|e| internal(&e))?
            .filter(|entry| entry.mode().is_blob())
            .map(|entry| entry.oid().to_owned()))
    };

    let mut current = tip;
    let mut current_oid = blob_oid(current)?;
    while current != base {
        let commit = repo
            .find_object(current)
            .map_err(|e| internal(&e))?
            .peel_to_commit()
            .map_err(|e| internal(&e))?;
        let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
        let Some(&first_parent) = parents.first() else {
            break;
        };
        // At a merge, follow the parent that actually *supplied* the
        // blob the lineage carries: exact blob-oid match first; when
        // conflict resolution edited the merged blob (no parent
        // matches exactly), the parent whose blob *continues* it at
        // rename similarity — falling back to the first parent
        // holding any blob would happily pick an unrelated
        // delete-and-recreate occupant on that line. Last resorts:
        // any blob-holding parent, then the first parent.
        let mut next = first_parent;
        if parents.len() > 1
            && let Some(oid) = current_oid
        {
            let mut exact: Option<gix::ObjectId> = None;
            // The *strongest* threshold-passing continuation wins: a
            // weaker passing parent may be an unrelated recreation
            // while a later parent holds the closer lineage.
            let mut best_continuing: Option<(f64, gix::ObjectId)> = None;
            let mut any_blob: Option<gix::ObjectId> = None;
            for &parent in &parents {
                let Some(parent_oid) = blob_oid(parent)? else {
                    continue;
                };
                if parent_oid == oid {
                    exact = Some(parent);
                    break;
                }
                if let Some(similarity) = blob_lineage_similarity(repo, &parent_oid, &oid)?
                    && similarity >= RENAME_SIMILARITY
                    && best_continuing.is_none_or(|(best, _)| similarity > best)
                {
                    best_continuing = Some((similarity, parent));
                }
                if any_blob.is_none() {
                    any_blob = Some(parent);
                }
            }
            next = exact
                .or(best_continuing.map(|(_, parent)| parent))
                .or(any_blob)
                .unwrap_or(first_parent);
        }
        let next_oid = blob_oid(next)?;
        // A presence flip in either direction along the followed line
        // is an identity boundary: absent→present downward means the
        // path was deleted here; present→absent downward means the
        // tip's file was *created* inside the range — and since the
        // qualification already verified the path exists at the base,
        // a prior occupant must have been deleted below.
        if current_oid.is_some() != next_oid.is_some() {
            return Ok(true);
        }
        current = next;
        current_oid = next_oid;
    }
    Ok(false)
}

/// Tree changes a merge commit itself introduced: renames and
/// additions whose destination path exists in *no* parent tree —
/// conflict resolution that committed a file under a brand-new path.
/// The merge tree is compared against **every** parent: a rename
/// whose source lives only in a non-first parent is invisible to the
/// first-parent diff (the destination looks like a plain addition).
/// Rename pairings win over plain additions for the same destination,
/// and the first parent's pairing wins ties — deterministic parent
/// order. Everything else in a merge's diffs is either a parent's own
/// changes replayed (their commits are walked separately) or churn
/// that `--no-merges` semantics deliberately exclude; accordingly the
/// returned changes carry no churn (identity only, never accumulated).
fn merge_introduced_changes(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    truncated: &mut Vec<PathBuf>,
) -> Result<Vec<CommitFileChange>, GitError> {
    let internal = |e: &dyn std::error::Error| GitError::Internal(e.to_string());
    let to_tree = commit.tree().map_err(|e| internal(&e))?;
    let mut parent_trees = Vec::new();
    let mut parent_ids: Vec<gix::ObjectId> = Vec::new();
    for parent_id in commit.parent_ids() {
        parent_ids.push(parent_id.detach());
        let parent = parent_id
            .object()
            .map_err(|e| internal(&e))?
            .peel_to_commit()
            .map_err(|e| internal(&e))?;
        parent_trees.push(parent.tree().map_err(|e| internal(&e))?);
    }
    let mut diffs: Vec<Vec<TreeChange>> = Vec::with_capacity(parent_trees.len());
    for base in &parent_trees {
        let tc = changes_between_trees(repo, Some(base), &to_tree)?;
        truncated.extend(tc.truncated_lineages);
        diffs.push(tc.changes);
    }

    // A destination is merge-introduced only when no parent has a
    // *blob* there: a parent holding a symlink or gitlink at the path
    // is a different identity entirely (conflict resolution replacing
    // a symlink with a real file still creates that file), and a
    // parent holding a blob performed (or already contained) the
    // change itself — walking its commits handles identity.
    let blob_in = |tree: &gix::Tree<'_>, path: &Path| -> Result<bool, GitError> {
        Ok(tree
            .lookup_entry_by_path(path)
            .map_err(|e| internal(&e))?
            .is_some_and(|entry| entry.mode().is_blob()))
    };
    let in_any_parent = |path: &Path| -> Result<bool, GitError> {
        for tree in &parent_trees {
            if blob_in(tree, path)? {
                return Ok(true);
            }
        }
        Ok(false)
    };

    let blob_oid_at =
        |tree: &gix::Tree<'_>, path: &Path| -> Result<Option<gix::ObjectId>, GitError> {
            Ok(tree
                .lookup_entry_by_path(path)
                .map_err(|e| internal(&e))?
                .filter(|entry| entry.mode().is_blob())
                .map(|entry| entry.oid().to_owned()))
        };

    let mut introduced: Vec<CommitFileChange> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Renames first, across all parent diffs. Distinct sources may
    // converge on one destination (two branches renamed the shared
    // file differently and conflict resolution committed a third
    // name): every pairing installs its own alias, deduplicated per
    // (destination, source) pair, so no branch's intermediate-path
    // lineage is stranded.
    let mut seen_pairs: std::collections::HashSet<(PathBuf, PathBuf)> =
        std::collections::HashSet::new();
    for (supplier_idx, diff) in diffs.iter().enumerate() {
        for change in diff {
            let TreeChange::Renamed {
                path, source_path, ..
            } = change
            else {
                continue;
            };
            if seen_pairs.contains(&(path.clone(), source_path.clone())) {
                continue;
            }
            // A destination some parent already owns is still a merge
            // rename when the merged content does *not continue* that
            // parent's version — conflict resolution carried the
            // source lineage into the existing path, and both files'
            // histories converge there. Continuation is judged like
            // rename similarity (equal blob, or ≥50% similar): an
            // edited-but-kept destination is retention, and treating
            // every unequal blob as proof that the source lineage won
            // would merge a discarded source into a surviving file.
            // Such convergent renames skip the destination boundary:
            // the owning parent's older history at the path is real
            // lineage, not a dead occupant.
            let dest_in_parent = in_any_parent(path)?;
            if dest_in_parent {
                let merged_oid = blob_oid_at(&to_tree, path)?;
                let mut retained = merged_oid.is_none();
                if let Some(merged_oid) = merged_oid {
                    for tree in &parent_trees {
                        if let Some(parent_oid) = blob_oid_at(tree, path)?
                            && same_blob_lineage(repo, &parent_oid, &merged_oid)?
                        {
                            retained = true;
                            break;
                        }
                    }
                }
                if retained {
                    continue;
                }
            }
            // Scope the alias to the parents whose lineage the rename
            // actually describes. The supplying parent (whose diff
            // paired the rename) always qualifies. When the merged
            // tree *retains a blob at the source path*, every other
            // parent is excluded outright: whatever survives there —
            // a delete-and-recreate resolved in that branch's favor,
            // even one edited during conflict resolution — is the
            // occupant those parents' commits describe, not the moved
            // lineage. Otherwise another parent holding a blob at the
            // source qualifies only when the path predates the
            // branches' divergence (their merge base has it): an
            // occupant independently created on that line is an
            // unrelated file, and admitting its commits would route
            // them into the rename target and let its creation
            // consume the alias before the real lineage is walked.
            let supplier = parent_ids[supplier_idx];
            let mut scopes = vec![supplier];
            let source_retained = blob_in(&to_tree, source_path)?;
            let mut addition_floor: Option<gix::ObjectId> = None;
            for (idx, (parent_id, tree)) in parent_ids.iter().zip(&parent_trees).enumerate() {
                if source_retained || idx == supplier_idx || !blob_in(tree, source_path)? {
                    continue;
                }
                let shares_lineage = match repo.merge_base(supplier, *parent_id) {
                    Ok(base) => {
                        let base_commit = base
                            .object()
                            .map_err(|e| internal(&e))?
                            .peel_to_commit()
                            .map_err(|e| internal(&e))?;
                        let base_tree = base_commit.tree().map_err(|e| internal(&e))?;
                        // The path merely *existing* at the base is
                        // not enough: a parent that deleted the base
                        // file and re-created an unrelated one at the
                        // same path crossed an identity boundary, and
                        // its commits describe the discarded
                        // re-creation, not the moved lineage. Require
                        // the parent's blob to plausibly continue the
                        // base's (equal, or similar at git's rename
                        // threshold) — and require no delete/recreate
                        // boundary inside the range, which endpoint
                        // blobs cannot see when the re-creation is
                        // byte-identical.
                        match (
                            blob_oid_at(&base_tree, source_path)?,
                            blob_oid_at(tree, source_path)?,
                        ) {
                            (Some(base_oid), Some(parent_oid)) => {
                                let shares = same_blob_lineage(repo, &base_oid, &parent_oid)?
                                    && !path_deleted_in_range(
                                        repo,
                                        *parent_id,
                                        base_commit.id,
                                        source_path,
                                    )?
                                    // The *supplier* must continue the
                                    // base lineage too: a supplier
                                    // whose own line deleted and
                                    // recreated the source after the
                                    // base renamed its *recreation* —
                                    // the other parent's retained
                                    // original is a different
                                    // (discarded) occupant. Widening
                                    // would route that original into
                                    // the rename target while the
                                    // addition floor rejects the
                                    // supplier's actual recreation,
                                    // stranding its edits; keep the
                                    // alias supplier-only instead.
                                    && !path_deleted_in_range(
                                        repo,
                                        supplier,
                                        base_commit.id,
                                        source_path,
                                    )?;
                                if shares && addition_floor.is_none() {
                                    // Additions through a multi-parent
                                    // scope must predate the
                                    // divergence: an addition on just
                                    // one line is a delete-and-
                                    // recreate inside its unchecked
                                    // sub-branches, not the moved
                                    // file's creation.
                                    addition_floor = Some(base_commit.id);
                                }
                                shares
                            }
                            _ => false,
                        }
                    }
                    // Disjoint histories cannot share the file.
                    Err(gix::repository::merge_base::Error::NotFound { .. }) => false,
                    Err(e) => return Err(GitError::Internal(e.to_string())),
                };
                if shares_lineage {
                    scopes.push(*parent_id);
                }
            }
            seen_pairs.insert((path.clone(), source_path.clone()));
            seen.insert(path.clone());
            introduced.push(CommitFileChange {
                path: path.clone(),
                source_path: Some(source_path.clone()),
                added: 0,
                removed: 0,
                is_deletion: false,
                is_addition: false,
                alias_scopes: Some(scopes),
                install_destination_boundary: !dest_in_parent,
                alias_addition_floor: addition_floor,
                discarded_occupant_fence: None,
            });
        }
    }
    // Then plain additions: a merge-created file at a brand-new path
    // establishes a fresh identity, fencing off any dead prior
    // occupant of that path (see the boundary install in the walk).
    for diff in &diffs {
        for change in diff {
            let TreeChange::Added { path, .. } = change else {
                continue;
            };
            if seen.contains(path) || in_any_parent(path)? {
                continue;
            }
            seen.insert(path.clone());
            introduced.push(CommitFileChange {
                path: path.clone(),
                source_path: None,
                added: 0,
                removed: 0,
                is_deletion: false,
                is_addition: true,
                alias_scopes: None,
                install_destination_boundary: true,
                alias_addition_floor: None,
                discarded_occupant_fence: None,
            });
        }
    }
    // Finally merge-performed deletions: a path present in a parent
    // but absent from the merged tree was resolved away by the merge
    // itself. Passing the deletion through lets the walk's
    // delete-then-recreate boundary fence the dead occupant when a
    // newer commit reuses the path — without it, the pre-merge
    // occupant's history would leak into the unrelated new file. A
    // deletion is suppressed only for parent lineages the rename
    // alias actually covers: a merge can move one parent's `a.rs`
    // *and* delete another parent's unrelated occupant of the same
    // path, and the latter still needs its fence. Like every merge
    // change, deletions carry no churn and are never accumulated.
    let mut rename_source_scopes: HashMap<&PathBuf, Vec<gix::ObjectId>> = HashMap::new();
    for change in &introduced {
        if let (Some(source), Some(scopes)) =
            (change.source_path.as_ref(), change.alias_scopes.as_ref())
        {
            rename_source_scopes
                .entry(source)
                .or_default()
                .extend(scopes.iter().copied());
        }
    }
    let mut deletions: Vec<CommitFileChange> = Vec::new();
    for (parent_idx, diff) in diffs.iter().enumerate() {
        for change in diff {
            let TreeChange::Deleted { path, .. } = change else {
                continue;
            };
            if seen.contains(path) || blob_in(&to_tree, path)? {
                continue;
            }
            if rename_source_scopes
                .get(path)
                .is_some_and(|scopes| scopes.contains(&parent_ids[parent_idx]))
            {
                // This parent's lineage moved with the rename — its
                // "deletion" is the move itself.
                continue;
            }
            seen.insert(path.clone());
            deletions.push(CommitFileChange {
                path: path.clone(),
                source_path: None,
                added: 0,
                removed: 0,
                is_deletion: true,
                is_addition: false,
                alias_scopes: None,
                install_destination_boundary: true,
                alias_addition_floor: None,
                discarded_occupant_fence: None,
            });
        }
    }
    introduced.extend(deletions);
    // Discarded same-path occupants: the merged tree keeps a blob at
    // a path, but some parent's blob there does *not* continue it —
    // that parent's post-divergence line held a different occupant
    // (e.g. a delete-and-recreate) which the merge resolved away.
    // Without a fence, the discarded occupant's recreation would
    // accumulate under the live path and its deletion could fence the
    // survivor's own pre-branch history. The fence is scoped to the
    // discarding parent and floored at its divergence from the
    // supplying parent, so shared ancestors stay with the survivor.
    // One fence per (path, discarding parent): an octopus merge can
    // discard several parents' independent occupants of one path, and
    // each needs its own scoped fence.
    let mut fenced: std::collections::HashSet<(PathBuf, gix::ObjectId)> =
        std::collections::HashSet::new();
    for (q_idx, diff) in diffs.iter().enumerate() {
        for change in diff {
            let TreeChange::Modified {
                path,
                previous_oid,
                oid,
            } = change
            else {
                continue;
            };
            if seen.contains(path) || fenced.contains(&(path.clone(), parent_ids[q_idx])) {
                continue;
            }
            // Find the parent that supplied the merged blob (exact,
            // then similarity) — the survivor's lineage.
            let mut supplier: Option<gix::ObjectId> = None;
            let mut best = 0.0_f64;
            for (idx, (parent_id, tree)) in parent_ids.iter().zip(&parent_trees).enumerate() {
                if idx == q_idx {
                    continue;
                }
                let Some(parent_oid) = blob_oid_at(tree, path)? else {
                    continue;
                };
                if parent_oid == *oid {
                    supplier = Some(*parent_id);
                    break;
                }
                if let Some(similarity) = blob_lineage_similarity(repo, &parent_oid, oid)?
                    && similarity >= RENAME_SIMILARITY
                    && similarity > best
                {
                    supplier = Some(*parent_id);
                    best = similarity;
                }
            }
            let Some(supplier) = supplier else {
                // No parent continues the merged blob (conflict
                // resolution rewrote it): ambiguous — leave identity
                // handling to the ordinary walk.
                continue;
            };
            let floor = match repo.merge_base(supplier, parent_ids[q_idx]) {
                Ok(base) => Some(base.detach()),
                Err(gix::repository::merge_base::Error::NotFound { .. }) => None,
                Err(e) => return Err(GitError::Internal(e.to_string())),
            };
            // Endpoint similarity alone cannot prove continuation:
            // a recreation may resemble the survivor (or be judged
            // against an edited merge blob). The parent's own line is
            // consulted for a delete/recreate boundary since the
            // divergence — only an uninterrupted, similar version is
            // a genuine continuation needing no fence.
            if same_blob_lineage(repo, previous_oid, oid)?
                && match floor {
                    Some(base) => !path_deleted_in_range(repo, parent_ids[q_idx], base, path)?,
                    // No merge base (`--allow-unrelated-histories`):
                    // the parents cannot share a lineage, so endpoint
                    // similarity between two independently created
                    // files proves nothing — fence the discarded
                    // occupant (with no floor: there is no shared
                    // pre-branch history to protect).
                    None => false,
                }
            {
                continue;
            }
            fenced.insert((path.clone(), parent_ids[q_idx]));
            introduced.push(CommitFileChange {
                path: path.clone(),
                source_path: None,
                added: 0,
                removed: 0,
                is_deletion: false,
                is_addition: false,
                alias_scopes: None,
                install_destination_boundary: true,
                alias_addition_floor: None,
                discarded_occupant_fence: Some((parent_ids[q_idx], floor)),
            });
        }
    }
    // Byte-identical recreated occupants: a parent whose line deleted
    // and recreated the path with content byte-equal to the blob the
    // merge keeps leaves *no entry at all* in its parent-to-merge diff
    // (exact OID equality erases the path), so the Modified-based pass
    // above never sees a candidate — yet the recreation still ends at
    // its deletion boundary. Without a fence it would accumulate under
    // the survivor and its deletion would tombstone the shared
    // pre-branch creation. Candidates are recovered by diffing each
    // parent against its divergence base — the recreation is visible
    // *there* whenever its content differs from the base (a recreation
    // byte-equal to the base as well is a pure revert and reads as
    // lineage continuation). Because both blobs are byte-equal, line
    // continuity — not content — is the only survivor signal: the
    // fence installs only when another parent carries the same blob
    // through an uninterrupted line. If no parent does, the recreation
    // is itself the survivor and the ordinary walk fence handles its
    // older history.
    for (q_idx, q_id) in parent_ids.iter().enumerate() {
        if parent_ids.len() < 2 {
            break;
        }
        // Divergence base for candidate detection; per-path floors
        // are still derived from the chosen supplier below. For an
        // octopus merge this approximates the divergence with the
        // first other parent — a deeper true base only widens the
        // candidate diff, and the boundary scan filters the excess.
        let other = parent_ids[usize::from(q_idx == 0)];
        let base = match repo.merge_base(other, *q_id) {
            Ok(base) => base.detach(),
            Err(gix::repository::merge_base::Error::NotFound { .. }) => continue,
            Err(e) => return Err(GitError::Internal(e.to_string())),
        };
        let base_tree = repo
            .find_object(base)
            .map_err(|e| internal(&e))?
            .peel_to_commit()
            .map_err(|e| internal(&e))?
            .tree()
            .map_err(|e| internal(&e))?;
        for (path, q_oid) in
            blob_modifications_between_trees(repo, &base_tree, &parent_trees[q_idx])?
        {
            if seen.contains(&path) || fenced.contains(&(path.clone(), *q_id)) {
                continue;
            }
            // Only the invisible case: the parent's endpoint blob is
            // byte-equal to the merged blob. Anything else produced a
            // parent-to-merge diff entry and was handled above.
            if blob_oid_at(&to_tree, &path)? != Some(q_oid) {
                continue;
            }
            // A supplier must carry the identical blob through an
            // uninterrupted post-divergence line.
            let mut supplier: Option<gix::ObjectId> = None;
            for (idx, (parent_id, tree)) in parent_ids.iter().zip(&parent_trees).enumerate() {
                if idx == q_idx || blob_oid_at(tree, &path)? != Some(q_oid) {
                    continue;
                }
                let s_base = match repo.merge_base(*parent_id, *q_id) {
                    Ok(base) => base.detach(),
                    Err(gix::repository::merge_base::Error::NotFound { .. }) => continue,
                    Err(e) => return Err(GitError::Internal(e.to_string())),
                };
                if path_deleted_in_range(repo, *parent_id, s_base, &path)? {
                    continue;
                }
                supplier = Some(*parent_id);
                break;
            }
            let Some(supplier) = supplier else {
                continue;
            };
            let floor = match repo.merge_base(supplier, *q_id) {
                Ok(base) => base.detach(),
                Err(gix::repository::merge_base::Error::NotFound { .. }) => continue,
                Err(e) => return Err(GitError::Internal(e.to_string())),
            };
            // The fence needs proof, not endpoint similarity: only a
            // deletion on this parent's own post-divergence line makes
            // the byte-equal blob a *recreation* rather than the
            // surviving file itself.
            if !path_deleted_in_range(repo, *q_id, floor, &path)? {
                continue;
            }
            fenced.insert((path.clone(), *q_id));
            introduced.push(CommitFileChange {
                path: path.clone(),
                source_path: None,
                added: 0,
                removed: 0,
                is_deletion: false,
                is_addition: false,
                alias_scopes: None,
                install_destination_boundary: true,
                alias_addition_floor: None,
                discarded_occupant_fence: Some((*q_id, Some(floor))),
            });
        }
    }
    // Unrelated parents (`--allow-unrelated-histories`) can hold
    // *byte-identical* same-path roots: exact OID equality erases the
    // path from every parent-to-merge diff, and the base-relative
    // recovery pass above has no base to diff against — yet without a
    // fence both independent root additions accumulate under the
    // surviving path, doubling churn/frequency and merging unrelated
    // authorship. The shape is rare, so the merged tree is enumerated
    // only when some parent pair actually lacks a merge base: the
    // first parent holding the merged blob supplies the survivor, and
    // every *unrelated* other parent holding the identical blob gets
    // an unfloored fence (no shared pre-branch history exists).
    let related = |a: gix::ObjectId, b: gix::ObjectId| -> Result<bool, GitError> {
        match repo.merge_base(a, b) {
            Ok(_) => Ok(true),
            Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(GitError::Internal(e.to_string())),
        }
    };
    let mut any_unrelated = false;
    'pairs: for (i, a) in parent_ids.iter().enumerate() {
        for b in &parent_ids[i + 1..] {
            if !related(*a, *b)? {
                any_unrelated = true;
                break 'pairs;
            }
        }
    }
    if any_unrelated {
        let mut recorder = gix::traverse::tree::Recorder::default();
        to_tree
            .traverse()
            .breadthfirst(&mut recorder)
            .map_err(|e| internal(&e))?;
        for entry in recorder.records {
            if !entry.mode.is_blob() {
                continue;
            }
            let Some(path) = crate::tree_changes::path_from_git(&entry.filepath) else {
                continue;
            };
            if seen.contains(&path) {
                continue;
            }
            let mut supplier: Option<gix::ObjectId> = None;
            for (parent_id, tree) in parent_ids.iter().zip(&parent_trees) {
                if blob_oid_at(tree, &path)? == Some(entry.oid) {
                    supplier = Some(*parent_id);
                    break;
                }
            }
            let Some(supplier) = supplier else {
                continue;
            };
            for (parent_id, tree) in parent_ids.iter().zip(&parent_trees) {
                if *parent_id == supplier
                    || fenced.contains(&(path.clone(), *parent_id))
                    || blob_oid_at(tree, &path)? != Some(entry.oid)
                    || related(supplier, *parent_id)?
                {
                    continue;
                }
                fenced.insert((path.clone(), *parent_id));
                introduced.push(CommitFileChange {
                    path: path.clone(),
                    source_path: None,
                    added: 0,
                    removed: 0,
                    is_deletion: false,
                    is_addition: false,
                    alias_scopes: None,
                    install_destination_boundary: true,
                    alias_addition_floor: None,
                    discarded_occupant_fence: Some((*parent_id, None)),
                });
            }
        }
    }
    Ok(introduced)
}

/// The paths whose *identity* a merge commit changes — conflict-
/// resolution creations, merge-only renames and deletions (with their
/// sources), and discarded-occupant fences. `range_touched_files`
/// consults this: such changes alter `history.*` metrics even when
/// the endpoint trees are byte-identical and no non-merge commit
/// touched the path.
pub(crate) fn merge_identity_paths(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
) -> Result<Vec<PathBuf>, GitError> {
    let mut truncated: Vec<PathBuf> = Vec::new();
    let mut paths: Vec<PathBuf> = merge_introduced_changes(repo, commit, &mut truncated)?
        .into_iter()
        .flat_map(|change| std::iter::once(change.path).chain(change.source_path))
        .collect();
    // A lineage truncated at this merge changed identity too.
    paths.extend(truncated);
    Ok(paths)
}

/// The identity a historical change accumulates under: a real
/// head-relative path, or a synthetic tombstone standing in for a dead
/// prior occupant of a rename destination (or of a delete-then-
/// recreate boundary). A dedicated variant rather than a sentinel
/// `PathBuf`: Git permits arbitrary bytes in filenames on some
/// platforms, so no in-namespace sentinel can be collision-free.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FileIdentity {
    Path(PathBuf),
    Tombstone(usize),
}

/// One rename-identity redirection. `scopes` are the commits whose
/// *ancestors* the entry applies to: an alias only describes the
/// lineage it redirected, so *concurrent* changes (a parallel branch
/// re-creating the path) must not resolve through it. For an ordinary
/// rename the scope is the renaming commit itself; for a
/// merge-introduced rename it is the parents whose trees contain the
/// source — scoping to the merge commit would wrongly capture every
/// parent's line, including one where an unrelated file lived and died
/// at the same path. `consumed` retires an entry from resolution —
/// either because the walk accumulated the *creation* of the aliased
/// path through it (the redirected occupant's birth has been found, so
/// anything older at that path belongs to a previous occupant), or
/// because an older rename reclaimed the fence after explaining where
/// the fenced occupant went. An older rename may install alongside a
/// consumed entry, and an older deletion fences history off behind a
/// fresh tombstone (matching the delete-then-recreate boundary).
/// Entries are never removed: phase 4 marks consumption by index, so
/// indices must stay stable.
#[derive(Clone, Debug)]
struct AliasEntry {
    target: FileIdentity,
    scopes: Vec<gix::ObjectId>,
    /// For merge-installed aliases whose scopes span several parents:
    /// an *addition* resolves through the entry only when it is an
    /// ancestor of this floor (the parents' merge base). The moved
    /// file's true creation predates the divergence; an addition on
    /// just one scoped line is a delete-and-recreate inside that
    /// line's sub-branches — a different identity that must neither
    /// route into the rename target nor consume the alias. `None`
    /// (ordinary renames, single-scope merges) leaves additions
    /// gated by the scopes alone.
    addition_floor: Option<gix::ObjectId>,
    /// A floor for *every* change: the entry applies only to commits
    /// that are **not** ancestors of it. Used by discarded-occupant
    /// fences, whose events all postdate the divergence — the shared
    /// pre-branch history belongs to the surviving file, not behind
    /// the fence.
    floor: Option<gix::ObjectId>,
    /// A fence for a same-path occupant a merge discarded (see the
    /// merge handling in the walk). Excluded from the
    /// delete-then-recreate "consumed entry" proof: the path stayed
    /// continuously occupied by the survivor, so a consumed fence
    /// says nothing about *its* older history.
    from_discarded_occupant: bool,
    consumed: bool,
}

impl AliasEntry {
    fn new(target: FileIdentity, scopes: Vec<gix::ObjectId>) -> Self {
        Self {
            target,
            scopes,
            addition_floor: None,
            floor: None,
            from_discarded_occupant: false,
            consumed: false,
        }
    }

    /// Whether the entry applies to a change made by `commit`: the
    /// change must be an ancestor of (or equal to) one of the scopes,
    /// must postdate the all-change floor when one is set, and an
    /// addition must additionally pass the addition floor.
    fn applies_to(
        &self,
        repo: &gix::Repository,
        graph: &mut AncestryGraph<'_, '_>,
        commit: gix::ObjectId,
        is_addition: bool,
    ) -> Result<bool, GitError> {
        if let Some(floor) = self.floor
            && is_descendant_of(repo, graph, commit, floor)?
        {
            // Pre-divergence commits are the surviving lineage's.
            return Ok(false);
        }
        if is_addition
            && let Some(floor) = self.addition_floor
            && !is_descendant_of(repo, graph, commit, floor)?
        {
            return Ok(false);
        }
        for scope in &self.scopes {
            if is_descendant_of(repo, graph, commit, *scope)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Resolve a historical path to its identity for a change made by
/// `commit`, returning the applied entry's index when an alias was
/// used (so the caller can mark it consumed).
///
/// An entry applies only when `commit` is an *ancestor* of the commit
/// that installed it: aliases redirect the pre-rename lineage on the
/// installer's own line, and a concurrent change (a parallel branch
/// re-creating or deleting the path) knows nothing of that rename.
/// Consumed entries never apply — the occupant they redirected has
/// been fully walked, so anything older belongs to someone else.
/// Among applicable entries, real-path targets win over tombstones
/// (a rename explains where the file went; a deletion boundary is
/// only a fence), first-installed first — deterministic under the
/// deterministic walk order.
fn resolve_alias(
    repo: &gix::Repository,
    graph: &mut AncestryGraph<'_, '_>,
    aliases: &HashMap<PathBuf, Vec<AliasEntry>>,
    path: &Path,
    commit: gix::ObjectId,
    is_addition: bool,
) -> Result<(FileIdentity, Option<usize>, Option<usize>), GitError> {
    let Some(entries) = aliases.get(path) else {
        return Ok((FileIdentity::Path(path.to_path_buf()), None, None));
    };
    let mut tombstone: Option<(FileIdentity, usize)> = None;
    // The first entry whose scopes admit this addition but whose
    // addition floor rejects it: the caller uses it to recognize a
    // recreated occupant's birth and pull the occupant's already-
    // routed edits back out of the alias target (see phase 4).
    let mut floor_gated: Option<usize> = None;
    for (idx, entry) in entries.iter().enumerate() {
        if entry.consumed {
            continue;
        }
        if is_addition
            && entry.addition_floor.is_some()
            && entry.applies_to(repo, graph, commit, false)?
            && !entry.applies_to(repo, graph, commit, true)?
        {
            if floor_gated.is_none() {
                floor_gated = Some(idx);
            }
            continue;
        }
        if !entry.applies_to(repo, graph, commit, is_addition)? {
            continue;
        }
        match &entry.target {
            FileIdentity::Path(_) => return Ok((entry.target.clone(), Some(idx), floor_gated)),
            FileIdentity::Tombstone(_) => {
                if tombstone.is_none() {
                    tombstone = Some((entry.target.clone(), idx));
                }
            }
        }
    }
    Ok(match tombstone {
        Some((target, idx)) => (target, Some(idx), floor_gated),
        None => (FileIdentity::Path(path.to_path_buf()), None, floor_gated),
    })
}

/// Diff `commit` against its first parent (or the empty tree for root
/// commits) with in-crate rename tracking, and compute line-level
/// churn per changed blob. Also returns the count of changed non-blob
/// leaf paths (symlinks, gitlinks) — they carry no analyzable text but
/// still belong to the commit's changeset for coupling cardinality.
fn diff_against_first_parent(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
) -> Result<(Vec<CommitFileChange>, usize, Vec<PathBuf>), GitError> {
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
    let non_blob_changes = records.non_blob_changes;
    let truncated_lineages = records.truncated_lineages;
    let records = records.changes;
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
                alias_scopes: None,
                install_destination_boundary: true,
                alias_addition_floor: None,
                discarded_occupant_fence: None,
            },
            TreeChange::Deleted { path, oid } => CommitFileChange {
                path,
                source_path: None,
                added: 0,
                removed: blob_line_count(repo, &oid)?,
                is_deletion: true,
                is_addition: false,
                alias_scopes: None,
                install_destination_boundary: true,
                alias_addition_floor: None,
                discarded_occupant_fence: None,
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
                    alias_scopes: None,
                    install_destination_boundary: true,
                    alias_addition_floor: None,
                    discarded_occupant_fence: None,
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
                    alias_scopes: None,
                    install_destination_boundary: true,
                    alias_addition_floor: None,
                    discarded_occupant_fence: None,
                }
            }
        };
        changes.push(file_change);
    }

    Ok((changes, non_blob_changes, truncated_lineages))
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

/// Author identity for ownership metrics: the author's raw email
/// bytes (ASCII-lowercased), falling back to the name for commits
/// without one. Byte-preserving deliberately — a lossy UTF-8
/// conversion would replace every invalid sequence with U+FFFD and
/// collapse distinct identities that differ only in such bytes,
/// undercounting `history.authors` and skewing ownership shares.
fn author_identity(commit: &gix::Commit<'_>) -> Result<Vec<u8>, GitError> {
    let author = commit
        .author()
        .map_err(|e| GitError::Internal(e.to_string()))?;
    let email: &[u8] = author.email.as_ref();
    let bytes: &[u8] = if email.iter().all(u8::is_ascii_whitespace) {
        author.name.as_ref()
    } else {
        email
    };
    Ok(bytes.to_ascii_lowercase())
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

    /// A minimal contribution for finalize-focused tests.
    fn contribution(author: &str, added: u64) -> Contribution {
        Contribution {
            commit: gix::ObjectId::null(gix::hash::Kind::Sha1),
            seconds: 0,
            author: author.as_bytes().into(),
            added,
            removed: 0,
            coupled_others: 0,
            coupling_eligible: true,
            is_bugfix: false,
            is_addition: false,
        }
    }

    #[test]
    fn finalize_ownership_and_minor_contributors() {
        let mut acc = FileAccumulator::default();
        // 100 added lines total: alice 90, bob 7, carol 3.
        acc.push(contribution("alice@x", 90));
        acc.push(contribution("bob@x", 7));
        acc.push(contribution("carol@x", 3));
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
        acc.push(contribution("alice@x", 100));
        acc.push(contribution("dave@x", 0));
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
