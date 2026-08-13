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
use std::path::{Path, PathBuf};

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
/// underlying git path bytes. `PathBuf`'s own `Ord` may order
/// differently across platforms (Windows `OsStr` ordering is not the
/// byte ordering), which would let equally ranked rename tie-breaks
/// pick different pairings per platform.
fn git_path_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    a.as_os_str()
        .as_encoded_bytes()
        .cmp(b.as_os_str().as_encoded_bytes())
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
/// Windows checkout at all, so callers skip the entry instead of
/// aborting the whole analysis. On Unix the conversion never fails.
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
    path: PathBuf,
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
    let mut modified: Vec<(PathBuf, gix::ObjectId, gix::ObjectId)> = Vec::new();
    let mut changes: Vec<TreeChange> = Vec::new();
    let mut non_blob_changes: usize = 0;

    for change in recorder.records {
        match change {
            Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    let Some(path) = path_from_git(&path) else {
                        continue;
                    };
                    added.push(RenameSide {
                        path,
                        oid,
                        broken: None,
                    });
                } else {
                    non_blob_changes += 1;
                }
            }
            Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    let Some(path) = path_from_git(&path) else {
                        continue;
                    };
                    deleted.push(RenameSide {
                        path,
                        oid,
                        broken: None,
                    });
                } else {
                    non_blob_changes += 1;
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
                let Some(path) = path_from_git(&path) else {
                    continue;
                };
                match (previous_entry_mode.is_blob(), entry_mode.is_blob()) {
                    (true, true) => modified.push((path, previous_oid, oid)),
                    (true, false) => changes.push(TreeChange::Deleted {
                        path,
                        oid: previous_oid,
                    }),
                    (false, true) => changes.push(TreeChange::Added { path, oid }),
                    // e.g. a submodule pointer bump: no analyzable
                    // text, but still a changed path in the changeset.
                    (false, false) => non_blob_changes += 1,
                }
            }
        }
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
    let mut broken_pairs: Vec<(PathBuf, gix::ObjectId, gix::ObjectId)> = Vec::new();
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
    modified.sort_by(|a, b| git_path_order(&a.0, &b.0));
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
            path,
            previous_oid,
            oid,
        });
    }

    detect_renames(repo, &mut changes, added, deleted, &broken_pairs)?;

    Ok(TreeChanges {
        changes,
        non_blob_changes,
    })
}

/// Pair deletions with additions into [`TreeChange::Renamed`] entries:
/// first exact (identical blob), then fuzzy (spanhash similarity
/// ≥ 50%) within the fixed pair budget. Broken modification halves
/// that pair with nothing are reassembled into their original
/// [`TreeChange::Modified`]; every other unpaired entry is appended as
/// a plain addition/deletion. Ordering and tie-breaks are by path so
/// results are stable regardless of walk order.
fn detect_renames(
    repo: &gix::Repository,
    changes: &mut Vec<TreeChange>,
    mut added: Vec<RenameSide>,
    mut deleted: Vec<RenameSide>,
    broken_pairs: &[(PathBuf, gix::ObjectId, gix::ObjectId)],
) -> Result<(), GitError> {
    added.sort_by(|a, b| git_path_order(&a.path, &b.path));
    deleted.sort_by(|a, b| git_path_order(&a.path, &b.path));

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
                path_affinity(&deleted[deleted_index].path, &side.path),
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
        // files churned at once): degrade to positional pairing in
        // path order rather than ranking millions of pairs. Still
        // deterministic; still exact-content renames.
        exact_candidates.clear();
        let mut added_by_oid: HashMap<gix::ObjectId, Vec<usize>> = HashMap::new();
        for (index, side) in added.iter().enumerate() {
            if side.oid != empty_blob {
                added_by_oid.entry(side.oid).or_default().push(index);
            }
        }
        for (oid, added_indices) in added_by_oid {
            let Some(deleted_indices) = deleted_by_oid.get(&oid) else {
                continue;
            };
            for (&deleted_index, &added_index) in deleted_indices.iter().zip(added_indices.iter()) {
                if deleted[deleted_index].broken.is_some()
                    && deleted[deleted_index].broken == added[added_index].broken
                {
                    continue;
                }
                exact_candidates.push((0, deleted_index, added_index));
            }
        }
    }
    exact_candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| git_path_order(&deleted[a.1].path, &deleted[b.1].path))
            .then_with(|| git_path_order(&added[a.2].path, &added[b.2].path))
    });

    let mut deleted_taken = vec![false; deleted.len()];
    let mut added_taken = vec![false; added.len()];
    for (_, deleted_index, added_index) in exact_candidates {
        if deleted_taken[deleted_index] || added_taken[added_index] {
            continue;
        }
        deleted_taken[deleted_index] = true;
        added_taken[added_index] = true;
        changes.push(TreeChange::Renamed {
            path: added[added_index].path.clone(),
            source_path: deleted[deleted_index].path.clone(),
            previous_oid: deleted[deleted_index].oid,
            oid: added[added_index].oid,
        });
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
    // source then destination path. Blobs are materialized at most
    // once each, and only when they clear the per-blob size cap and
    // the total byte budget (checked via object headers first).
    if !remaining_added.is_empty()
        && !remaining_deleted.is_empty()
        && remaining_added.len() * remaining_deleted.len() <= RENAME_FUZZY_LIMIT
    {
        let deleted_blobs = load_fuzzy_blobs(repo, &remaining_deleted, FUZZY_TOTAL_BYTE_BUDGET)?;
        // The added side spends whatever budget the deleted side left,
        // keeping the total bounded even when both sides are large.
        let deleted_bytes: u64 = deleted_blobs
            .iter()
            .flatten()
            .map(|blob| blob.data.len() as u64)
            .sum();
        let added_blobs = load_fuzzy_blobs(
            repo,
            &remaining_added,
            FUZZY_TOTAL_BYTE_BUDGET.saturating_sub(deleted_bytes),
        )?;

        let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
        for (deleted_index, old_blob) in deleted_blobs.iter().enumerate() {
            let Some(old_blob) = old_blob else { continue };
            for (added_index, new_blob) in added_blobs.iter().enumerate() {
                let Some(new_blob) = new_blob else { continue };
                // A broken half must not "rename" onto its own other
                // half — they were just judged dissimilar anyway.
                if remaining_deleted[deleted_index].broken.is_some()
                    && remaining_deleted[deleted_index].broken
                        == remaining_added[added_index].broken
                {
                    continue;
                }
                if let Some(similarity) = blob_similarity(old_blob, new_blob) {
                    candidates.push((similarity, deleted_index, added_index));
                }
            }
        }
        candidates.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| {
                    git_path_order(&remaining_deleted[a.1].path, &remaining_deleted[b.1].path)
                })
                .then_with(|| {
                    git_path_order(&remaining_added[a.2].path, &remaining_added[b.2].path)
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
            changes.push(TreeChange::Renamed {
                path: remaining_added[added_index].path.clone(),
                source_path: remaining_deleted[deleted_index].path.clone(),
                previous_oid: remaining_deleted[deleted_index].oid,
                oid: remaining_added[added_index].oid,
            });
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
                changes.push(TreeChange::Modified {
                    path: path.clone(),
                    previous_oid: *previous_oid,
                    oid: *oid,
                });
                reassembled[pair] = true;
            }
        }
    }

    for side in remaining_added {
        if side.broken.is_some_and(|pair| reassembled[pair]) {
            continue;
        }
        changes.push(TreeChange::Added {
            path: side.path,
            oid: side.oid,
        });
    }
    for side in remaining_deleted {
        if side.broken.is_some_and(|pair| reassembled[pair]) {
            continue;
        }
        changes.push(TreeChange::Deleted {
            path: side.path,
            oid: side.oid,
        });
    }

    Ok(())
}

/// Deterministic path-affinity score for pairing identical blobs:
/// a matching basename dominates, then shared leading directory
/// components, then shared trailing components.
fn path_affinity(a: &Path, b: &Path) -> u64 {
    let mut score = 0u64;
    if a.file_name().is_some() && a.file_name() == b.file_name() {
        score += 1 << 32;
    }
    let prefix = a
        .components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .count() as u64;
    let suffix = a
        .components()
        .rev()
        .zip(b.components().rev())
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
/// rule out a rename. Entries are visited in the caller's
/// (path-sorted) order, keeping budget truncation deterministic.
fn load_fuzzy_blobs(
    repo: &gix::Repository,
    entries: &[RenameSide],
    budget: u64,
) -> Result<Vec<Option<FuzzyBlob>>, GitError> {
    let mut remaining = budget;
    let mut blobs = Vec::with_capacity(entries.len());
    for side in entries {
        let size = blob_size(repo, &side.oid)?;
        if size > FUZZY_MAX_BLOB_BYTES || size > remaining {
            blobs.push(None);
            continue;
        }
        remaining -= size;
        let data = read_blob_data(repo, &side.oid)?;
        blobs.push(Some(FuzzyBlob { data }));
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
