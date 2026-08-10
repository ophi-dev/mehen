// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Deterministic tree-to-tree change detection with rename tracking.
//!
//! Built directly on the low-level `gix::diff::tree` walk plus an
//! in-crate similarity pass, deliberately *not* on
//! `Repository::diff_tree_to_tree`: the high-level API builds its
//! rewrite-tracking resource cache from repository/user configuration
//! (`diff.algorithm`, attribute-driven conversions from the current
//! checkout's index), which would make rename classification — and
//! therefore churn, ownership, and path identity — vary across
//! machines and checkouts. Everything here is a pure function of the
//! two trees: histogram line diffs, a fixed 50% similarity threshold
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
use gix::objs::TreeRefIter;

use crate::GitError;

/// Similarity threshold for rename detection (git's `-M50%` default).
const RENAME_SIMILARITY: f64 = 0.5;

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

/// Maximum span length for the similarity chunking (git's spanhash
/// uses the same bound): spans end at a newline or at 64 bytes,
/// whichever comes first, so similarity is byte-weighted, robust to
/// line insertions, and meaningful for single-line files.
const SIMILARITY_SPAN_BYTES: usize = 64;

/// Binary detection window: git treats content with a NUL byte in the
/// first 8000 bytes as binary.
const BINARY_SNIFF_BYTES: usize = 8000;

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

/// Diff two trees (blob entries only, renames joined). `parent` of
/// `None` means the empty tree (root commits).
pub(crate) fn changes_between_trees(
    repo: &gix::Repository,
    parent: Option<&gix::Tree<'_>>,
    current: &gix::Tree<'_>,
) -> Result<Vec<TreeChange>, GitError> {
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
                        path: PathBuf::from(path.to_string()),
                        oid,
                        broken: None,
                    });
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
                        path: PathBuf::from(path.to_string()),
                        oid,
                        broken: None,
                    });
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
                let path = PathBuf::from(path.to_string());
                match (previous_entry_mode.is_blob(), entry_mode.is_blob()) {
                    (true, true) => modified.push((path, previous_oid, oid)),
                    (true, false) => changes.push(TreeChange::Deleted {
                        path,
                        oid: previous_oid,
                    }),
                    (false, true) => changes.push(TreeChange::Added { path, oid }),
                    (false, false) => {}
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
    let consider_breaking = !added.is_empty() || !deleted.is_empty();
    for (path, previous_oid, oid) in modified {
        if consider_breaking
            && previous_oid != oid
            && blob_size(repo, &previous_oid)? <= FUZZY_MAX_BLOB_BYTES
            && blob_size(repo, &oid)? <= FUZZY_MAX_BLOB_BYTES
        {
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
        changes.push(TreeChange::Modified {
            path,
            previous_oid,
            oid,
        });
    }

    detect_renames(repo, &mut changes, added, deleted, &broken_pairs)?;

    Ok(changes)
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
    added.sort_by(|a, b| a.path.cmp(&b.path));
    deleted.sort_by(|a, b| a.path.cmp(&b.path));

    // Exact pass: identical blob content is a certain rename. Sorted
    // path order on both sides keeps repeated content deterministic.
    let mut deleted_by_oid: HashMap<gix::ObjectId, Vec<usize>> = HashMap::new();
    for (index, side) in deleted.iter().enumerate() {
        deleted_by_oid.entry(side.oid).or_default().push(index);
    }
    // Indices into `deleted` grow with path order; take from the front.
    for candidates in deleted_by_oid.values_mut() {
        candidates.reverse();
    }

    let mut deleted_taken = vec![false; deleted.len()];
    let mut remaining_added: Vec<RenameSide> = Vec::new();
    for side in added {
        // A broken half must not "rename" onto its own other half.
        let paired = deleted_by_oid.get_mut(&side.oid).and_then(|candidates| {
            let position = candidates.iter().rposition(|&index| {
                deleted[index].broken.is_none() || deleted[index].broken != side.broken
            })?;
            Some(candidates.remove(position))
        });
        match paired {
            Some(deleted_index) => {
                deleted_taken[deleted_index] = true;
                changes.push(TreeChange::Renamed {
                    path: side.path,
                    source_path: deleted[deleted_index].path.clone(),
                    previous_oid: deleted[deleted_index].oid,
                    oid: side.oid,
                });
            }
            None => remaining_added.push(side),
        }
    }
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
                    remaining_deleted[a.1]
                        .path
                        .cmp(&remaining_deleted[b.1].path)
                })
                .then_with(|| remaining_added[a.2].path.cmp(&remaining_added[b.2].path))
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

/// Byte-weighted content similarity following git's spanhash design:
/// both payloads are split into spans terminated by `\n` or at 64
/// bytes, and similarity is the byte volume of the multiset span
/// intersection over the larger payload. Weighting by *bytes* (not
/// physical lines) means one shared boilerplate line cannot outvote a
/// huge rewritten line, and long single-line files still compare
/// meaningfully through their 64-byte sub-spans.
fn spanhash_similarity(old: &[u8], new: &[u8]) -> f64 {
    let longest = old.len().max(new.len());
    if longest == 0 {
        return 0.0;
    }
    // Multiset of span hashes, weighted by total span bytes.
    let mut available: HashMap<u64, u64> = HashMap::new();
    for span in spans(old) {
        *available.entry(fnv1a(span)).or_insert(0) += span.len() as u64;
    }
    let mut common: u64 = 0;
    for span in spans(new) {
        if let Some(bytes) = available.get_mut(&fnv1a(span)) {
            let take = (span.len() as u64).min(*bytes);
            *bytes -= take;
            common += take;
        }
    }
    common as f64 / longest as f64
}

/// Split a payload into spans ending at `\n` or [`SIMILARITY_SPAN_BYTES`].
fn spans(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = match rest
            .iter()
            .take(SIMILARITY_SPAN_BYTES)
            .position(|&b| b == b'\n')
        {
            Some(newline) => newline + 1,
            None => SIMILARITY_SPAN_BYTES.min(rest.len()),
        };
        let (span, tail) = rest.split_at(end);
        rest = tail;
        Some(span)
    })
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
