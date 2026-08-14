// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Deterministic tree-to-tree change detection with rename tracking.
//!
//! Built directly on the low-level `gix::diff::tree` walk plus an
//! in-crate similarity pass, deliberately *not* on
//! `Repository::diff_tree_to_tree`. An explicit
//! `Options::track_rewrites(Some(Rewrites { .. }))` can pin the
//! rename-detection *parameters* independent of `diff.renames` /
//! `diff.renameLimit` (see GitoxideLabs/gitoxide#2915), but two gaps
//! remain: the similarity pipeline converts blob content through a
//! resource cache that reads `.gitattributes` from the HEAD index —
//! so rename classification (and therefore churn, ownership, and path
//! identity) could still vary across checkouts — and `Rewrites`
//! models `-M`/`-C` but not git's `-B` break-rewrite, which this
//! module needs to recover renames whose old path was reused within
//! the compared range. Everything here is a pure function of the two
//! trees: histogram line diffs, a fixed 50% similarity threshold
//! (git's `-M50%` default), and a fixed fuzzy-pair budget.
//!
//! Only blob entries are reported: directories, symlinks, and gitlinks
//! (submodules) carry no analyzable text. An entry changing *type*
//! across the trees is reported from the blob side — a file replaced
//! by a submodule is that file's deletion, and vice versa.

use std::collections::HashMap;
use std::path::PathBuf;

use gix::diff::blob::{Algorithm, Diff, InternedInput, sources::byte_lines};
use gix::diff::tree::recorder::Change;

/// Spanhash similarity of two blobs, when both are loadable within
/// the fuzzy budget (equal ids score 1.0; oversized blobs score
/// nothing rather than being loaded).
pub(crate) fn blob_lineage_similarity(
    repo: &gix::Repository,
    a: &gix::ObjectId,
    b: &gix::ObjectId,
) -> Result<Option<f64>, GitError> {
    if a == b {
        return Ok(Some(1.0));
    }
    if blob_size(repo, a)? > FUZZY_MAX_BLOB_BYTES || blob_size(repo, b)? > FUZZY_MAX_BLOB_BYTES {
        return Ok(None);
    }
    let a_data = read_blob_data(repo, a)?;
    let b_data = read_blob_data(repo, b)?;
    Ok(Some(spanhash_similarity(&a_data, &b_data)))
}

/// Whether two blobs plausibly hold the *same file lineage*: equal
/// ids, or spanhash similarity at git's rename threshold. Used by the
/// history walk to tell a lineage-continuing edit apart from an
/// unrelated file re-created at the same path when only trees are
/// available (e.g. qualifying merge-alias scopes). Oversized blobs
/// are conservatively *not* considered the same lineage — never
/// loaded, deterministic.
pub(crate) fn same_blob_lineage(
    repo: &gix::Repository,
    a: &gix::ObjectId,
    b: &gix::ObjectId,
) -> Result<bool, GitError> {
    Ok(blob_lineage_similarity(repo, a, b)?.is_some_and(|s| s >= RENAME_SIMILARITY))
}

/// Canonical, platform-independent ordering for repository paths: the
/// raw git path bytes.
fn git_path_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.cmp(b)
}

/// Convert a git tree path (raw bytes) to a `PathBuf` without lossy
/// UTF-8 replacement: on Unix, `b"x\xff.py"` must stay distinct from
/// a real file literally named `x\u{FFFD}.py` — a lossy conversion
/// would collide the two and merge their changes (and their history
/// accumulators) into one identity.
///
/// Returns `None` when the platform cannot represent the bytes as a
/// native path (Windows requires valid UTF-8; the infallible gix
/// conversion would *panic* there). Such a path cannot exist in a
/// Windows checkout at all, so the diff pipeline keeps the raw bytes
/// through rename detection and converts only when *emitting* a
/// change — an unrepresentable result is then counted as one opaque
/// changed path (coupling cardinality stays platform-independent,
/// including for rename pairs) instead of aborting the analysis. On
/// Unix the conversion never fails.
pub(crate) fn path_from_git(path: &gix::bstr::BString) -> Option<PathBuf> {
    match gix::path::try_from_bstr(path.as_ref() as &gix::bstr::BStr) {
        Ok(p) => Some(p.into_owned()),
        Err(_) => {
            log::warn!(
                "skipping git path not representable on this platform: {}",
                String::from_utf8_lossy(path)
            );
            None
        }
    }
}
use gix::objs::TreeRefIter;

use crate::GitError;

/// Similarity threshold for rename detection (git's `-M50%` default).
pub(crate) const RENAME_SIMILARITY: f64 = 0.5;

/// Upper bound on deletion×addition pairs examined by the fuzzy rename
/// pass within one tree diff (the spirit of git's `diff.renameLimit`).
/// Exact (same-blob) renames are always detected; beyond the budget,
/// inexact renames degrade to a deletion + addition rather than making
/// bulk-restructuring commits quadratically expensive.
const RENAME_FUZZY_LIMIT: usize = 10_000;

/// Blobs larger than this never enter the fuzzy pass — a replaced
/// multi-gigabyte binary must not be materialized and diffed just to
/// rule out a rename. Sizes are checked via object headers before any
/// data is loaded. Exact renames still match at any size.
const FUZZY_MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// Total bytes the fuzzy pass may materialize per tree diff. Applied
/// in sorted path order, so truncation under pressure is deterministic.
const FUZZY_TOTAL_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Total bytes the break-rewrite scan may materialize per tree diff,
/// separate from (and shaped like) the fuzzy budget. Spent in path
/// order, so truncation under pressure is deterministic; modifications
/// beyond the budget simply stay `Modified`.
const BREAK_TOTAL_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Fixed-offset span bound for the similarity chunking (git's
/// spanhash uses the same 64-byte bound).
const SPAN_FIXED_BYTES: usize = 64;

/// Bounds for the *content-defined* similarity chunking: gear-hash
/// cuts average ~64 bytes past the minimum, clamped so pathological
/// content cannot produce degenerate spans.
const SPAN_MIN_BYTES: usize = 16;
const SPAN_MAX_BYTES: usize = 256;
/// Cut when the low six gear bits are all set: 1/64 per byte, ~64-byte
/// spans on average past the minimum.
const SPAN_CUT_MASK: u64 = 0x3F;

/// Binary detection window: git treats content with a NUL byte in the
/// first 8000 bytes as binary.
const BINARY_SNIFF_BYTES: usize = 8000;

/// Commits with at most this many modifications (and no additions or
/// deletions) still get the break-rewrite scan, so *edited* swaps —
/// two files exchanging paths with edits in the same commit, leaving
/// no exact OID cross-match — are recovered as renames. Bulk
/// modifications-only commits above the limit skip the scan entirely.
const SWAP_SCAN_MAX_MODIFICATIONS: usize = 8;

/// One file-level change between two trees, blob entries only.
pub(crate) enum TreeChange {
    Added {
        path: PathBuf,
        oid: gix::ObjectId,
    },
    Deleted {
        path: PathBuf,
        /// The deleted blob (for a blob→non-blob type change, the
        /// pre-change blob).
        oid: gix::ObjectId,
    },
    Modified {
        path: PathBuf,
        previous_oid: gix::ObjectId,
        oid: gix::ObjectId,
    },
    Renamed {
        path: PathBuf,
        source_path: PathBuf,
        previous_oid: gix::ObjectId,
        oid: gix::ObjectId,
    },
}

/// One side of a potential rename pair, tagged with the broken
/// modification it came from (if any) so unpaired halves can be
/// reassembled.
struct RenameSide {
    /// Raw git path bytes — kept unconverted through rename pairing
    /// so unrepresentable (Windows) paths still pair; conversion to a
    /// platform path happens at emission.
    path: gix::bstr::BString,
    oid: gix::ObjectId,
    /// Index into the broken-pairs table when this side came from a
    /// completely rewritten same-path modification (git `-B`).
    broken: Option<usize>,
}

/// The result of a tree-to-tree diff: analyzable blob changes with
/// renames joined, plus a count of changed non-blob leaf entries
/// (symlinks, gitlinks). The count exists for coupling cardinality:
/// a commit's changeset size includes every changed path, even those
/// carrying no analyzable text.
pub(crate) struct TreeChanges {
    pub(crate) changes: Vec<TreeChange>,
    pub(crate) non_blob_changes: usize,
    /// Destinations of renames whose *source* path is not
    /// representable on this platform (Windows, raw non-UTF-8 git
    /// path): the rename was detected but its lineage link cannot be
    /// expressed, so the destination's earlier history is
    /// unreachable here. Consumers publishing per-file history mark
    /// such files unmeasurable rather than reporting the truncated
    /// remainder as if it were the whole history. Always empty on
    /// Unix.
    pub(crate) truncated_lineages: Vec<PathBuf>,
}

/// Blob-to-blob modifications between two trees — paths present in
/// both with different content, as `(path, new_oid)`. No rename
/// detection, no blob loads: a plain tree walk. Used by the merge
/// identity handling to find paths a parent's line changed since its
/// divergence even when the parent's endpoint blob is byte-equal to
/// the merged tree (exact OID equality erases such paths from the
/// parent-to-merge diff entirely).
pub(crate) fn blob_modifications_between_trees(
    repo: &gix::Repository,
    base: &gix::Tree<'_>,
    current: &gix::Tree<'_>,
) -> Result<Vec<(PathBuf, gix::ObjectId)>, GitError> {
    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        TreeRefIter::from_bytes(&base.data, base.id.kind()),
        TreeRefIter::from_bytes(&current.data, current.id.kind()),
        gix::diff::tree::State::default(),
        repo.objects.clone(),
        &mut recorder,
    )
    .map_err(|e| GitError::Internal(e.to_string()))?;

    let mut modifications = Vec::new();
    for change in recorder.records {
        if let Change::Modification {
            previous_entry_mode,
            entry_mode,
            oid,
            path,
            ..
        } = change
            && previous_entry_mode.is_blob()
            && entry_mode.is_blob()
            && let Some(path) = path_from_git(&path)
        {
            modifications.push((path, oid));
        }
    }
    Ok(modifications)
}

/// Diff two trees (blob entries only, renames joined). `parent` of
/// `None` means the empty tree (root commits).
pub(crate) fn changes_between_trees(
    repo: &gix::Repository,
    parent: Option<&gix::Tree<'_>>,
    current: &gix::Tree<'_>,
) -> Result<TreeChanges, GitError> {
    let (parent_data, parent_kind) = match parent {
        Some(tree) => (tree.data.as_slice(), tree.id.kind()),
        None => ([].as_slice(), current.id.kind()),
    };

    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        TreeRefIter::from_bytes(parent_data, parent_kind),
        TreeRefIter::from_bytes(&current.data, current.id.kind()),
        gix::diff::tree::State::default(),
        repo.objects.clone(),
        &mut recorder,
    )
    .map_err(|e| GitError::Internal(e.to_string()))?;

    let mut added: Vec<RenameSide> = Vec::new();
    let mut deleted: Vec<RenameSide> = Vec::new();
    let mut modified: Vec<(gix::bstr::BString, gix::ObjectId, gix::ObjectId)> = Vec::new();
    let mut changes: Vec<TreeChange> = Vec::new();
    let mut non_blob_changes: usize = 0;
    let mut truncated_lineages: Vec<PathBuf> = Vec::new();
    // Non-blob additions/deletions are *paired* before counting: a
    // renamed symlink or gitlink is one changed identity, not two
    // changeset members — counting both records would inflate every
    // other file's coupling and could push a changeset over the
    // noise cutoff. Pairing is exact (same entry kind, same OID),
    // mirroring how such entries actually move; kind/OID multisets
    // suffice since only the *count* feeds coupling cardinality.
    let mut non_blob_added: HashMap<(gix::object::tree::EntryKind, gix::ObjectId), usize> =
        HashMap::new();
    let mut non_blob_deleted: HashMap<(gix::object::tree::EntryKind, gix::ObjectId), usize> =
        HashMap::new();

    for change in recorder.records {
        match change {
            Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    added.push(RenameSide {
                        path,
                        oid,
                        broken: None,
                    });
                } else {
                    *non_blob_added.entry((entry_mode.kind(), oid)).or_insert(0) += 1;
                }
            }
            Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    deleted.push(RenameSide {
                        path,
                        oid,
                        broken: None,
                    });
                } else {
                    *non_blob_deleted
                        .entry((entry_mode.kind(), oid))
                        .or_insert(0) += 1;
                }
            }
            Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
                path,
            } => {
                // A type change is reported from the blob side so
                // downstream blob reads never touch a gitlink OID (the
                // submodule's commit object is not in this
                // repository's odb).
                match (previous_entry_mode.is_blob(), entry_mode.is_blob()) {
                    (true, true) => modified.push((path, previous_oid, oid)),
                    (true, false) => match path_from_git(&path) {
                        Some(path) => changes.push(TreeChange::Deleted {
                            path,
                            oid: previous_oid,
                        }),
                        // Unrepresentable on this platform: still one
                        // opaque changed path for coupling cardinality.
                        None => non_blob_changes += 1,
                    },
                    (false, true) => match path_from_git(&path) {
                        Some(path) => changes.push(TreeChange::Added { path, oid }),
                        None => non_blob_changes += 1,
                    },
                    // e.g. a submodule pointer bump: no analyzable
                    // text, but still a changed path in the changeset.
                    (false, false) => non_blob_changes += 1,
                }
            }
        }
    }

    // Fold the paired non-blob moves: an add/delete pair of the same
    // kind and OID collapses to one changed identity, unpaired
    // records count individually.
    for (key, added_count) in non_blob_added {
        let deleted_count = non_blob_deleted.remove(&key).unwrap_or(0);
        non_blob_changes += added_count.max(deleted_count);
    }
    for (_, deleted_count) in non_blob_deleted {
        non_blob_changes += deleted_count;
    }

    // Break-rewrite pass (git `-B`): a same-path modification whose
    // two sides are completely dissimilar is really "the old file left
    // and something new took its path" — e.g. `a.rs` renamed to `b.rs`
    // and an unrelated new `a.rs` created within the compared range,
    // which the endpoint tree walk collapses into `Modified(a.rs)` +
    // `Added(b.rs)`. Breaking the modification lets rename detection
    // pair the old content with its true destination; unpaired halves
    // are reassembled afterwards. Only worth attempting when there is
    // something to pair with — the common all-modifications diff pays
    // nothing here.
    let mut broken_pairs: Vec<(gix::bstr::BString, gix::ObjectId, gix::ObjectId)> = Vec::new();
    // Two same-commit modifications can also hide a rename: swapping
    // two files through a temporary name leaves only two dissimilar
    // `Modified` entries whose blobs *cross-match exactly*. Detect the
    // exact cross-signal from OIDs alone (no blob loads) so such
    // modifications become break candidates even when the diff has no
    // standalone additions or deletions.
    let modified_previous_oids: std::collections::HashSet<gix::ObjectId> = modified
        .iter()
        .filter(|(_, previous_oid, oid)| previous_oid != oid)
        .map(|(_, previous_oid, _)| *previous_oid)
        .collect();
    let modified_new_oids: std::collections::HashSet<gix::ObjectId> = modified
        .iter()
        .filter(|(_, previous_oid, oid)| previous_oid != oid)
        .map(|(_, _, oid)| *oid)
        .collect();
    let has_loose_ends = !added.is_empty() || !deleted.is_empty();
    // Small all-modifications commits may hide *edited* swaps with no
    // exact OID cross-signal; scan them too (bounded by the count
    // limit and the byte budget). Bulk commits skip the speculative
    // scan unless an exact cross-match or loose end justifies it.
    let small_swap_scan =
        !has_loose_ends && modified.len() >= 2 && modified.len() <= SWAP_SCAN_MAX_MODIFICATIONS;
    // Path order keeps budget truncation deterministic.
    modified.sort_by(|a, b| git_path_order(a.0.as_ref(), b.0.as_ref()));
    let mut break_budget = BREAK_TOTAL_BYTE_BUDGET;
    for (path, previous_oid, oid) in modified {
        // Worth breaking only when something could pair with a half:
        // an unpaired addition/deletion, or another modification whose
        // blob content this one exactly exchanged with, or a small
        // enough all-modifications commit to probe for edited swaps.
        let cross_matched =
            modified_new_oids.contains(&previous_oid) || modified_previous_oids.contains(&oid);
        if (has_loose_ends || cross_matched || small_swap_scan) && previous_oid != oid {
            let old_size = blob_size(repo, &previous_oid)?;
            let new_size = blob_size(repo, &oid)?;
            let within_caps = old_size <= FUZZY_MAX_BLOB_BYTES
                && new_size <= FUZZY_MAX_BLOB_BYTES
                && old_size.saturating_add(new_size) <= break_budget;
            if within_caps {
                break_budget -= old_size + new_size;
                let old_data = read_blob_data(repo, &previous_oid)?;
                let new_data = read_blob_data(repo, &oid)?;
                if spanhash_similarity(&old_data, &new_data) < RENAME_SIMILARITY {
                    let pair = broken_pairs.len();
                    broken_pairs.push((path.clone(), previous_oid, oid));
                    deleted.push(RenameSide {
                        path: path.clone(),
                        oid: previous_oid,
                        broken: Some(pair),
                    });
                    added.push(RenameSide {
                        path,
                        oid,
                        broken: Some(pair),
                    });
                    continue;
                }
            }
        }
        changes.push(TreeChange::Modified {
            path: match path_from_git(&path) {
                Some(path) => path,
                None => {
                    // Unrepresentable on this platform: one opaque
                    // changed path for coupling cardinality.
                    non_blob_changes += 1;
                    continue;
                }
            },
            previous_oid,
            oid,
        });
    }

    detect_renames(
        repo,
        &mut changes,
        added,
        deleted,
        &broken_pairs,
        &mut non_blob_changes,
        &mut truncated_lineages,
    )?;

    Ok(TreeChanges {
        changes,
        non_blob_changes,
        truncated_lineages,
    })
}

/// Pair deletions with additions into [`TreeChange::Renamed`] entries:
/// first exact (identical blob), then fuzzy (spanhash similarity
/// ≥ 50%) within the fixed pair budget. Broken modification halves
/// that pair with nothing are reassembled into their original
/// [`TreeChange::Modified`]; every other unpaired entry is appended as
/// a plain addition/deletion. Ordering and tie-breaks are by path so
/// results are stable regardless of walk order.
///
/// Pairing runs on the raw git path bytes; conversion to platform
/// paths happens at emission. A pair (or single) whose path is
/// unrepresentable on this platform degrades — the representable half
/// of a rename is emitted alone, a fully unrepresentable change bumps
/// `non_blob_changes` — so the changeset *cardinality* every other
/// file's coupling reads is identical across platforms.
#[allow(clippy::too_many_arguments)]
fn detect_renames(
    repo: &gix::Repository,
    changes: &mut Vec<TreeChange>,
    mut added: Vec<RenameSide>,
    mut deleted: Vec<RenameSide>,
    broken_pairs: &[(gix::bstr::BString, gix::ObjectId, gix::ObjectId)],
    non_blob_changes: &mut usize,
    truncated_lineages: &mut Vec<PathBuf>,
) -> Result<(), GitError> {
    added.sort_by(|a, b| git_path_order(a.path.as_ref(), b.path.as_ref()));
    deleted.sort_by(|a, b| git_path_order(a.path.as_ref(), b.path.as_ref()));

    // Exact pass: identical blob content is a certain rename. When an
    // OID has several candidates on either side (e.g. two identical
    // files swapped between directories), all candidate pairs are
    // ranked *globally* by path affinity — matching basenames, then
    // shared directory components — exactly like the fuzzy pass, so an
    // early destination can never consume a source that a later
    // destination matches more strongly. Path tie-breaks keep the
    // outcome deterministic. Empty blobs carry no identity signal and
    // never participate (mirroring the fuzzy pass and git's
    // `track_empty: false` default).
    let empty_blob = gix::ObjectId::empty_blob(repo.object_hash());
    let mut deleted_by_oid: HashMap<gix::ObjectId, Vec<usize>> = HashMap::new();
    for (index, side) in deleted.iter().enumerate() {
        if side.oid != empty_blob {
            deleted_by_oid.entry(side.oid).or_default().push(index);
        }
    }

    let mut exact_candidates: Vec<(u64, usize, usize)> = Vec::new();
    let mut exact_overflow = false;
    for (added_index, side) in added.iter().enumerate() {
        if side.oid == empty_blob {
            continue;
        }
        let Some(candidates) = deleted_by_oid.get(&side.oid) else {
            continue;
        };
        for &deleted_index in candidates {
            // A broken half must not "rename" onto its own other half.
            if deleted[deleted_index].broken.is_some()
                && deleted[deleted_index].broken == side.broken
            {
                continue;
            }
            exact_candidates.push((
                path_affinity(deleted[deleted_index].path.as_ref(), side.path.as_ref()),
                deleted_index,
                added_index,
            ));
            if exact_candidates.len() > RENAME_FUZZY_LIMIT {
                exact_overflow = true;
                break;
            }
        }
        if exact_overflow {
            break;
        }
    }
    if exact_overflow {
        // Pathological same-content fan-out (thousands of identical
        // files churned at once): degrade to bounded pairing in path
        // order rather than ranking millions of pairs. Basename
        // affinity is kept — one hash pass per group — because purely
        // positional pairing can cross-pair a bulk move whose
        // destination directories reorder the path-sorted lists,
        // silently transferring commit history between files whose
        // basenames match unambiguously. Still deterministic; still
        // exact-content renames.
        exact_candidates.clear();
        let mut added_by_oid: HashMap<gix::ObjectId, Vec<usize>> = HashMap::new();
        for (index, side) in added.iter().enumerate() {
            if side.oid != empty_blob {
                added_by_oid.entry(side.oid).or_default().push(index);
            }
        }
        fn basename(path: &gix::bstr::BString) -> &[u8] {
            let bytes: &[u8] = path.as_ref();
            bytes.rsplit(|&c| c == b'/').next().unwrap_or(bytes)
        }
        for (oid, added_indices) in added_by_oid {
            let Some(deleted_indices) = deleted_by_oid.get(&oid) else {
                continue;
            };
            let disallowed = |deleted_index: usize, added_index: usize| {
                deleted[deleted_index].broken.is_some()
                    && deleted[deleted_index].broken == added[added_index].broken
            };
            let mut deleted_by_basename: HashMap<&[u8], std::collections::VecDeque<usize>> =
                HashMap::new();
            for &deleted_index in deleted_indices {
                deleted_by_basename
                    .entry(basename(&deleted[deleted_index].path))
                    .or_default()
                    .push_back(deleted_index);
            }
            let mut taken: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut unpaired_added: Vec<usize> = Vec::new();
            for &added_index in &added_indices {
                let mut chosen: Option<usize> = None;
                if let Some(queue) = deleted_by_basename.get_mut(basename(&added[added_index].path))
                    && let Some(pos) = queue.iter().position(|&d| !disallowed(d, added_index))
                {
                    chosen = queue.remove(pos);
                }
                match chosen {
                    Some(deleted_index) => {
                        taken.insert(deleted_index);
                        exact_candidates.push((0, deleted_index, added_index));
                    }
                    None => unpaired_added.push(added_index),
                }
            }
            // Leftovers pair positionally, in path order.
            let mut remaining_deleted = deleted_indices
                .iter()
                .copied()
                .filter(|deleted_index| !taken.contains(deleted_index));
            for &added_index in &unpaired_added {
                let Some(deleted_index) = remaining_deleted.find(|&d| !disallowed(d, added_index))
                else {
                    break;
                };
                exact_candidates.push((0, deleted_index, added_index));
            }
        }
    }
    exact_candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| git_path_order(deleted[a.1].path.as_ref(), deleted[b.1].path.as_ref()))
            .then_with(|| git_path_order(added[a.2].path.as_ref(), added[b.2].path.as_ref()))
    });

    let mut deleted_taken = vec![false; deleted.len()];
    let mut added_taken = vec![false; added.len()];
    for (_, deleted_index, added_index) in exact_candidates {
        if deleted_taken[deleted_index] || added_taken[added_index] {
            continue;
        }
        deleted_taken[deleted_index] = true;
        added_taken[added_index] = true;
        push_renamed(
            changes,
            non_blob_changes,
            truncated_lineages,
            &deleted[deleted_index],
            &added[added_index],
        );
    }
    let mut remaining_added: Vec<RenameSide> = added
        .into_iter()
        .zip(added_taken)
        .filter(|(_, taken)| !taken)
        .map(|(side, _)| side)
        .collect();
    let mut remaining_deleted: Vec<RenameSide> = deleted
        .into_iter()
        .zip(deleted_taken)
        .filter(|(_, taken)| !taken)
        .map(|(side, _)| side)
        .collect();

    // Fuzzy pass: content similarity, best match first; ties break by
    // source then destination path. The *deleted* side is loaded
    // resident under the byte budget; the *added* side streams one
    // blob at a time (per-blob cap only, each loaded exactly once),
    // so every resident source is compared against **every** eligible
    // destination regardless of path order — a per-side budget split
    // could load two halves that share no real rename pair (e.g.
    // eight edited 8-MiB renames whose destination names reverse the
    // source order) and silently degrade all of them.
    if !remaining_added.is_empty()
        && !remaining_deleted.is_empty()
        // `checked_mul`: on a 32-bit target a pathological diff
        // (tens of thousands of surviving entries per side) would
        // overflow the pair count — a debug panic, or a release wrap
        // to a tiny number that *enables* the nested scan the limit
        // exists to prevent.
        && remaining_added
            .len()
            .checked_mul(remaining_deleted.len())
            .is_some_and(|pairs| pairs <= RENAME_FUZZY_LIMIT)
    {
        let deleted_blobs = load_fuzzy_blobs(repo, &remaining_deleted, FUZZY_TOTAL_BYTE_BUDGET)?;

        let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
        // The streamed side holds one blob at a time but is still
        // I/O-bounded by its own cumulative budget: without it, one
        // deletion against thousands of large additions would read
        // and decompress an unbounded volume despite the documented
        // cap. Like `load_fuzzy_blobs`, the budget is spent in
        // ascending (size, path) order so that both sides sample the
        // *same size region* — real rename pairs have close byte
        // sizes, and a path-ordered prefix on one side against a
        // size-ordered prefix on the other could otherwise be
        // disjoint. Over-budget destinations are skipped (not
        // `break`): smaller later blobs may still fit, keeping the
        // truncation deterministic.
        let mut added_sizes = Vec::with_capacity(remaining_added.len());
        for side in &remaining_added {
            added_sizes.push(blob_size(repo, &side.oid)?);
        }
        let mut added_order: Vec<usize> = (0..remaining_added.len()).collect();
        added_order.sort_by(|&a, &b| {
            added_sizes[a].cmp(&added_sizes[b]).then_with(|| {
                git_path_order(
                    remaining_added[a].path.as_ref(),
                    remaining_added[b].path.as_ref(),
                )
            })
        });
        let mut stream_budget = FUZZY_TOTAL_BYTE_BUDGET;
        for added_index in added_order {
            let added_side = &remaining_added[added_index];
            let size = added_sizes[added_index];
            if size > FUZZY_MAX_BLOB_BYTES || size > stream_budget {
                continue;
            }
            stream_budget -= size;
            let new_blob = FuzzyBlob {
                data: read_blob_data(repo, &added_side.oid)?,
            };
            for (deleted_index, old_blob) in deleted_blobs.iter().enumerate() {
                let Some(old_blob) = old_blob else { continue };
                // A broken half must not "rename" onto its own other
                // half — they were just judged dissimilar anyway.
                if remaining_deleted[deleted_index].broken.is_some()
                    && remaining_deleted[deleted_index].broken == added_side.broken
                {
                    continue;
                }
                if let Some(similarity) = blob_similarity(old_blob, &new_blob) {
                    candidates.push((similarity, deleted_index, added_index));
                }
            }
        }
        candidates.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| {
                    git_path_order(
                        remaining_deleted[a.1].path.as_ref(),
                        remaining_deleted[b.1].path.as_ref(),
                    )
                })
                .then_with(|| {
                    git_path_order(
                        remaining_added[a.2].path.as_ref(),
                        remaining_added[b.2].path.as_ref(),
                    )
                })
        });

        let mut added_taken = vec![false; remaining_added.len()];
        let mut deleted_taken = vec![false; remaining_deleted.len()];
        for (_, deleted_index, added_index) in candidates {
            if added_taken[added_index] || deleted_taken[deleted_index] {
                continue;
            }
            added_taken[added_index] = true;
            deleted_taken[deleted_index] = true;
            push_renamed(
                changes,
                non_blob_changes,
                truncated_lineages,
                &remaining_deleted[deleted_index],
                &remaining_added[added_index],
            );
        }
        remaining_added = remaining_added
            .into_iter()
            .zip(added_taken)
            .filter(|(_, taken)| !taken)
            .map(|(entry, _)| entry)
            .collect();
        remaining_deleted = remaining_deleted
            .into_iter()
            .zip(deleted_taken)
            .filter(|(_, taken)| !taken)
            .map(|(entry, _)| entry)
            .collect();
    }

    // Reassemble broken pairs whose halves both went unpaired — the
    // break was speculative and the entry is still just a (heavily
    // rewritten) modification.
    let mut reassembled = vec![false; broken_pairs.len()];
    {
        let unpaired_deleted: std::collections::HashSet<usize> = remaining_deleted
            .iter()
            .filter_map(|side| side.broken)
            .collect();
        for side in &remaining_added {
            if let Some(pair) = side.broken
                && unpaired_deleted.contains(&pair)
            {
                let (path, previous_oid, oid) = &broken_pairs[pair];
                match path_from_git(path) {
                    Some(path) => changes.push(TreeChange::Modified {
                        path,
                        previous_oid: *previous_oid,
                        oid: *oid,
                    }),
                    // Unrepresentable on this platform: one opaque
                    // changed path for coupling cardinality.
                    None => *non_blob_changes += 1,
                }
                reassembled[pair] = true;
            }
        }
    }

    for side in remaining_added {
        if side.broken.is_some_and(|pair| reassembled[pair]) {
            continue;
        }
        match path_from_git(&side.path) {
            Some(path) => changes.push(TreeChange::Added {
                path,
                oid: side.oid,
            }),
            None => *non_blob_changes += 1,
        }
    }
    for side in remaining_deleted {
        if side.broken.is_some_and(|pair| reassembled[pair]) {
            continue;
        }
        match path_from_git(&side.path) {
            Some(path) => changes.push(TreeChange::Deleted {
                path,
                oid: side.oid,
            }),
            None => *non_blob_changes += 1,
        }
    }

    Ok(())
}

/// Emit a paired rename, degrading when a side's path is not
/// representable on this platform: both sides convert → `Renamed`;
/// destination only → `Added` (the lineage link is unexpressible —
/// its source path cannot exist in a checkout here); source only →
/// `Deleted`; neither → one opaque changed path. Every arm
/// contributes exactly one changed identity, so coupling cardinality
/// matches the platform where both paths convert.
fn push_renamed(
    changes: &mut Vec<TreeChange>,
    non_blob_changes: &mut usize,
    truncated_lineages: &mut Vec<PathBuf>,
    source: &RenameSide,
    dest: &RenameSide,
) {
    match (path_from_git(&dest.path), path_from_git(&source.path)) {
        (Some(path), Some(source_path)) => changes.push(TreeChange::Renamed {
            path,
            source_path,
            previous_oid: source.oid,
            oid: dest.oid,
        }),
        (Some(path), None) => {
            // The lineage link exists but cannot be expressed on this
            // platform: the destination's earlier history is
            // unreachable, and publishing the truncated remainder as
            // if it were the whole history would fabricate a young,
            // single-author file. Record the destination so history
            // consumers mark it unmeasurable instead.
            truncated_lineages.push(path.clone());
            changes.push(TreeChange::Added {
                path,
                oid: dest.oid,
            });
        }
        (None, Some(source_path)) => changes.push(TreeChange::Deleted {
            path: source_path,
            oid: source.oid,
        }),
        (None, None) => *non_blob_changes += 1,
    }
}

/// Deterministic path-affinity score for pairing identical blobs:
/// a matching basename dominates, then shared leading directory
/// components, then shared trailing components. Operates on the raw
/// git path bytes so the score is identical on every platform.
fn path_affinity(a: &[u8], b: &[u8]) -> u64 {
    let mut score = 0u64;
    let base_a = a.rsplit(|&c| c == b'/').next().unwrap_or(a);
    let base_b = b.rsplit(|&c| c == b'/').next().unwrap_or(b);
    if !base_a.is_empty() && base_a == base_b {
        score += 1 << 32;
    }
    let prefix = a
        .split(|&c| c == b'/')
        .zip(b.split(|&c| c == b'/'))
        .take_while(|(x, y)| x == y)
        .count() as u64;
    let suffix = a
        .rsplit(|&c| c == b'/')
        .zip(b.rsplit(|&c| c == b'/'))
        .take_while(|(x, y)| x == y)
        .count() as u64;
    score + prefix * 1024 + suffix
}

/// A blob materialized for the fuzzy pass.
struct FuzzyBlob {
    data: Vec<u8>,
}

/// Materialize each entry's blob for similarity testing, or `None`
/// when the blob exceeds the per-blob cap or the remaining byte
/// budget. Sizes come from object headers, so an oversized blob (a
/// replaced multi-gigabyte binary) is never loaded into memory just to
/// rule out a rename. Budget is spent in ascending (size, path) order:
/// real rename pairs have close byte sizes (≥50% span similarity
/// bounds the ratio), so ordering *both* sides by size keeps
/// corresponding sources and destinations inside the same budget
/// prefix even when their path orders disagree — a path-ordered
/// prefix could load two halves that share no real pair. Still
/// deterministic.
fn load_fuzzy_blobs(
    repo: &gix::Repository,
    entries: &[RenameSide],
    budget: u64,
) -> Result<Vec<Option<FuzzyBlob>>, GitError> {
    let mut sizes = Vec::with_capacity(entries.len());
    for side in entries {
        sizes.push(blob_size(repo, &side.oid)?);
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| {
        sizes[a]
            .cmp(&sizes[b])
            .then_with(|| git_path_order(entries[a].path.as_ref(), entries[b].path.as_ref()))
    });
    let mut remaining = budget;
    let mut blobs: Vec<Option<FuzzyBlob>> = (0..entries.len()).map(|_| None).collect();
    for index in order {
        let size = sizes[index];
        if size > FUZZY_MAX_BLOB_BYTES || size > remaining {
            continue;
        }
        remaining -= size;
        blobs[index] = Some(FuzzyBlob {
            data: read_blob_data(repo, &entries[index].oid)?,
        });
    }
    Ok(blobs)
}

/// A blob's size from its object header, without loading the payload.
pub(crate) fn blob_size(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<u64, GitError> {
    Ok(repo
        .find_header(*oid)
        .map_err(|e| GitError::Internal(e.to_string()))?
        .size())
}

/// Content similarity in `[0, 1]`, or `None` below the rename
/// threshold.
fn blob_similarity(old: &FuzzyBlob, new: &FuzzyBlob) -> Option<f64> {
    if old.data.is_empty() && new.data.is_empty() {
        // Two empty blobs carry no identity signal (git's rename
        // tracking skips empty files too).
        return None;
    }
    let similarity = spanhash_similarity(&old.data, &new.data);
    (similarity >= RENAME_SIMILARITY).then_some(similarity)
}

/// Byte-weighted content similarity following git's spanhash design,
/// scored under two chunkings with the better result taken:
///
/// * spans ending at `\n` or at fixed 64-byte offsets — exact for
///   normal text and for same-length edits of long single-line
///   content (including periodic content, where alignment is the
///   only usable signal);
/// * spans ending at `\n` or at *content-defined* gear-hash cuts —
///   insertion-stable for long single-line content, where a few
///   inserted bytes would shift every fixed offset and collapse the
///   fixed-chunking similarity below the rename threshold.
///
/// Both chunkings are pure functions of the bytes, so the maximum is
/// deterministic. Weighting by *bytes* (not physical lines) means one
/// shared boilerplate line cannot outvote a huge rewritten line.
fn spanhash_similarity(old: &[u8], new: &[u8]) -> f64 {
    let fixed = span_multiset_similarity(old, new, fixed_spans);
    if fixed >= 1.0 {
        return fixed;
    }
    fixed.max(span_multiset_similarity(old, new, gear_spans))
}

/// The byte volume of the multiset span-hash intersection over the
/// larger payload, using `chunk` to split both payloads.
fn span_multiset_similarity(old: &[u8], new: &[u8], chunk: fn(&[u8]) -> Vec<&[u8]>) -> f64 {
    let longest = old.len().max(new.len());
    if longest == 0 {
        return 0.0;
    }
    // Multiset of span hashes, weighted by total span bytes.
    let mut available: HashMap<u64, u64> = HashMap::new();
    for span in chunk(old) {
        *available.entry(fnv1a(span)).or_insert(0) += span.len() as u64;
    }
    let mut common: u64 = 0;
    for span in chunk(new) {
        if let Some(bytes) = available.get_mut(&fnv1a(span)) {
            let take = (span.len() as u64).min(*bytes);
            *bytes -= take;
            common += take;
        }
    }
    common as f64 / longest as f64
}

/// Split a payload into spans ending at `\n` or at fixed 64-byte
/// offsets (git's spanhash bound).
fn fixed_spans(data: &[u8]) -> Vec<&[u8]> {
    let mut rest = data;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let end = match rest.iter().take(SPAN_FIXED_BYTES).position(|&b| b == b'\n') {
            Some(newline) => newline + 1,
            None => SPAN_FIXED_BYTES.min(rest.len()),
        };
        let (span, tail) = rest.split_at(end);
        out.push(span);
        rest = tail;
    }
    out
}

/// Split a payload into spans ending at `\n` or at content-defined
/// gear-hash cuts (low six bits all set: 1/64 per byte, ~64-byte
/// spans past the minimum), bounded to
/// `[SPAN_MIN_BYTES, SPAN_MAX_BYTES]`. Cut positions depend only on
/// nearby content, so an insertion shifts only the spans it touches.
fn gear_spans(data: &[u8]) -> Vec<&[u8]> {
    let mut rest = data;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let mut gear: u64 = 0;
        let mut end = rest.len().min(SPAN_MAX_BYTES);
        for (i, &byte) in rest.iter().take(SPAN_MAX_BYTES).enumerate() {
            if byte == b'\n' {
                end = i + 1;
                break;
            }
            gear = (gear << 1).wrapping_add(GEAR[byte as usize]);
            if i + 1 >= SPAN_MIN_BYTES && gear & SPAN_CUT_MASK == SPAN_CUT_MASK {
                end = i + 1;
                break;
            }
        }
        let (span, tail) = rest.split_at(end);
        out.push(span);
        rest = tail;
    }
    out
}

/// Deterministic gear table for the content-defined span cuts
/// (splitmix64 over a fixed seed — no runtime randomness, identical
/// on every platform and run).
const GEAR: [u64; 256] = build_gear_table();

const fn build_gear_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

/// FNV-1a — a fixed, dependency-free hash so span classification never
/// varies across platforms, Rust releases, or process runs.
fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Whether content is binary by git's heuristic: a NUL byte within the
/// first 8000 bytes.
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// Line-level (added, removed) counts between two blob payloads using
/// the histogram diff over byte lines.
pub(crate) fn line_diff_counts(old_data: &[u8], new_data: &[u8]) -> (u64, u64) {
    let input = InternedInput::new(byte_lines(old_data), byte_lines(new_data));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    (
        u64::from(diff.count_additions()),
        u64::from(diff.count_removals()),
    )
}

pub(crate) fn read_blob_data(
    repo: &gix::Repository,
    oid: &gix::ObjectId,
) -> Result<Vec<u8>, GitError> {
    let object = repo
        .find_object(*oid)
        .map_err(|e| GitError::Internal(e.to_string()))?;
    Ok(object.detach().data)
}

/// Number of lines in a blob (a trailing fragment without `\n` counts
/// as a line).
pub(crate) fn count_lines(data: &[u8]) -> u64 {
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
    fn line_diff_counts_are_line_based_not_byte_based() {
        // One rewritten line must count as exactly +1/−1 regardless of
        // how many bytes changed within it.
        let old = b"fn a() {}\nfn b() {}\n";
        let new = b"fn a_renamed_with_many_bytes() {}\nfn b() {}\n";
        assert_eq!(line_diff_counts(old, new), (1, 1));
    }

    fn fuzzy(data: &[u8]) -> FuzzyBlob {
        FuzzyBlob {
            data: data.to_vec(),
        }
    }

    #[test]
    fn blob_similarity_recognizes_edited_single_line_files() {
        // A long one-line file with a small edit has zero common
        // *physical lines*; the byte-weighted spanhash similarity must
        // still recognize it through its 64-byte sub-spans.
        let old = format!("export const x = [{}];", "1, ".repeat(200));
        let new = old.replace("const x", "const y");
        let similarity = blob_similarity(&fuzzy(old.as_bytes()), &fuzzy(new.as_bytes()))
            .expect("one-line edit should stay above the rename threshold");
        assert!(similarity >= RENAME_SIMILARITY, "got {similarity}");
    }

    #[test]
    fn blob_similarity_rejects_dissimilar_single_line_files() {
        let old = b"export const alpha_configuration_value = 1;";
        let new = b"#!/bin/sh @@ ~~ [[ ]] %% ^^ && || ;; :: ??";
        assert_eq!(blob_similarity(&fuzzy(old), &fuzzy(new)), None);
    }

    #[test]
    fn blob_similarity_ignores_empty_blobs() {
        assert_eq!(blob_similarity(&fuzzy(b""), &fuzzy(b"")), None);
    }

    #[test]
    fn spanhash_similarity_is_byte_weighted_not_line_weighted() {
        // One tiny shared boilerplate line plus a huge fully rewritten
        // line: line-weighted similarity would report 50% (1 of 2
        // lines shared) and join the pair; byte weighting must reject
        // it because nearly all *content* changed.
        let shared = "# generated\n";
        let old = format!("{shared}{}\n", "A".repeat(4000));
        let new = format!("{shared}{}\n", "B".repeat(4000));
        let similarity = spanhash_similarity(old.as_bytes(), new.as_bytes());
        assert!(
            similarity < RENAME_SIMILARITY,
            "a rewritten dominant line must not pass: {similarity}"
        );
    }

    #[test]
    fn is_binary_detects_nul_in_sniff_window() {
        assert!(is_binary(b"PK\x03\x04\x00binary"));
        assert!(!is_binary(b"plain text\nwith lines\n"));
    }
}
