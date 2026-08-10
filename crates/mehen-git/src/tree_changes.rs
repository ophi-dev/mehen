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
/// multi-gigabyte binary must not be materialized and line-diffed just
/// to rule out a rename. Sizes are checked via object headers before
/// any data is loaded. Exact renames still match at any size.
const FUZZY_MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// Total bytes the fuzzy pass may materialize per tree diff. Applied
/// in sorted path order, so truncation under pressure is deterministic.
const FUZZY_TOTAL_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Blobs at or below this size get a byte-level similarity fallback
/// when line-level similarity misses: a *one-line* file with any edit
/// has zero common lines regardless of how many bytes survived, so
/// line granularity alone would break renames of single-line sources
/// (minified bundles, one-liner configs).
const BYTE_SIMILARITY_MAX_BLOB_BYTES: usize = 256 * 1024;

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

    let mut added: Vec<(PathBuf, gix::ObjectId)> = Vec::new();
    let mut deleted: Vec<(PathBuf, gix::ObjectId)> = Vec::new();
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
                    added.push((PathBuf::from(path.to_string()), oid));
                }
            }
            Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    deleted.push((PathBuf::from(path.to_string()), oid));
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
                    (true, true) => changes.push(TreeChange::Modified {
                        path,
                        previous_oid,
                        oid,
                    }),
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

    detect_renames(repo, &mut changes, added, deleted)?;

    Ok(changes)
}

/// Pair deletions with additions into [`TreeChange::Renamed`] entries:
/// first exact (identical blob), then fuzzy (line similarity ≥ 50%)
/// within the fixed pair budget. Unpaired entries are appended as
/// plain additions/deletions. Ordering and tie-breaks are by path so
/// results are stable regardless of walk order.
fn detect_renames(
    repo: &gix::Repository,
    changes: &mut Vec<TreeChange>,
    mut added: Vec<(PathBuf, gix::ObjectId)>,
    mut deleted: Vec<(PathBuf, gix::ObjectId)>,
) -> Result<(), GitError> {
    added.sort_by(|a, b| a.0.cmp(&b.0));
    deleted.sort_by(|a, b| a.0.cmp(&b.0));

    // Exact pass: identical blob content is a certain rename. Sorted
    // path order on both sides keeps repeated content deterministic.
    let mut deleted_by_oid: HashMap<gix::ObjectId, Vec<usize>> = HashMap::new();
    for (index, (_, oid)) in deleted.iter().enumerate() {
        deleted_by_oid.entry(*oid).or_default().push(index);
    }
    // Indices into `deleted` grow with path order; take from the front.
    for candidates in deleted_by_oid.values_mut() {
        candidates.reverse();
    }

    let mut deleted_taken = vec![false; deleted.len()];
    let mut remaining_added: Vec<(PathBuf, gix::ObjectId)> = Vec::new();
    for (path, oid) in added {
        let paired = deleted_by_oid.get_mut(&oid).and_then(Vec::pop);
        match paired {
            Some(deleted_index) => {
                deleted_taken[deleted_index] = true;
                changes.push(TreeChange::Renamed {
                    path,
                    source_path: deleted[deleted_index].0.clone(),
                    previous_oid: deleted[deleted_index].1,
                    oid,
                });
            }
            None => remaining_added.push((path, oid)),
        }
    }
    let mut remaining_deleted: Vec<(PathBuf, gix::ObjectId)> = deleted
        .iter()
        .zip(deleted_taken.iter())
        .filter(|(_, taken)| !**taken)
        .map(|((path, oid), _)| (path.clone(), *oid))
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
                if let Some(similarity) = blob_similarity(old_blob, new_blob) {
                    candidates.push((similarity, deleted_index, added_index));
                }
            }
        }
        candidates.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| remaining_deleted[a.1].0.cmp(&remaining_deleted[b.1].0))
                .then_with(|| remaining_added[a.2].0.cmp(&remaining_added[b.2].0))
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
                path: remaining_added[added_index].0.clone(),
                source_path: remaining_deleted[deleted_index].0.clone(),
                previous_oid: remaining_deleted[deleted_index].1,
                oid: remaining_added[added_index].1,
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

    for (path, oid) in remaining_added {
        changes.push(TreeChange::Added { path, oid });
    }
    for (path, oid) in remaining_deleted {
        changes.push(TreeChange::Deleted { path, oid });
    }

    Ok(())
}

/// A blob materialized for the fuzzy pass, with its line count.
struct FuzzyBlob {
    data: Vec<u8>,
    lines: u64,
}

/// Materialize each entry's blob for similarity testing, or `None`
/// when the blob exceeds the per-blob cap or the remaining byte
/// budget. Sizes come from object headers, so an oversized blob (a
/// replaced multi-gigabyte binary) is never loaded into memory just to
/// rule out a rename. Entries are visited in the caller's
/// (path-sorted) order, keeping budget truncation deterministic.
fn load_fuzzy_blobs(
    repo: &gix::Repository,
    entries: &[(PathBuf, gix::ObjectId)],
    budget: u64,
) -> Result<Vec<Option<FuzzyBlob>>, GitError> {
    let mut remaining = budget;
    let mut blobs = Vec::with_capacity(entries.len());
    for (_, oid) in entries {
        let size = repo
            .find_header(*oid)
            .map_err(|e| GitError::Internal(e.to_string()))?
            .size();
        if size > FUZZY_MAX_BLOB_BYTES || size > remaining {
            blobs.push(None);
            continue;
        }
        remaining -= size;
        let data = read_blob_data(repo, oid)?;
        let lines = count_lines(&data);
        blobs.push(Some(FuzzyBlob { data, lines }));
    }
    Ok(blobs)
}

/// Content similarity in `[0, 1]`, or `None` below the rename
/// threshold.
///
/// Primary measure: surviving lines over the longer side. When that
/// misses and both blobs are small enough, a byte-level diff decides
/// instead — a *one-line* file with any edit has zero common lines, so
/// line granularity alone would never recognize renames of single-line
/// sources (minified bundles, one-liner configs).
fn blob_similarity(old: &FuzzyBlob, new: &FuzzyBlob) -> Option<f64> {
    let longest_lines = old.lines.max(new.lines);
    if longest_lines == 0 {
        // Two empty blobs carry no identity signal (git's rename
        // tracking skips empty files too).
        return None;
    }
    let (_, removals) = line_diff_counts(&old.data, &new.data);
    let common = old.lines.saturating_sub(removals);
    let line_similarity = common as f64 / longest_lines as f64;
    if line_similarity >= RENAME_SIMILARITY {
        return Some(line_similarity);
    }

    let longest_bytes = old.data.len().max(new.data.len());
    if longest_bytes > 0 && longest_bytes <= BYTE_SIMILARITY_MAX_BLOB_BYTES {
        let input = InternedInput::new(ByteTokens(&old.data), ByteTokens(&new.data));
        let diff = Diff::compute(Algorithm::Histogram, &input);
        let common_bytes = (old.data.len() as u64).saturating_sub(u64::from(diff.count_removals()));
        let byte_similarity = common_bytes as f64 / longest_bytes as f64;
        if byte_similarity >= RENAME_SIMILARITY {
            return Some(byte_similarity);
        }
    }

    None
}

/// Byte-granularity token source for the similarity fallback (the
/// vendored imara-diff tokenizes `&[u8]` by *lines*, which is exactly
/// what the fallback must not do).
struct ByteTokens<'a>(&'a [u8]);

impl<'a> gix::diff::blob::TokenSource for ByteTokens<'a> {
    type Token = u8;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, u8>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
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
            lines: count_lines(data),
        }
    }

    #[test]
    fn blob_similarity_falls_back_to_bytes_for_single_line_files() {
        // A long one-line file with a small edit has zero common
        // *lines*; the byte-level fallback must still recognize it.
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
}
