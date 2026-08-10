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

    // Fuzzy pass: line-level similarity, best match first; ties break
    // by source then destination path.
    if !remaining_added.is_empty()
        && !remaining_deleted.is_empty()
        && remaining_added.len() * remaining_deleted.len() <= RENAME_FUZZY_LIMIT
    {
        let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
        for (deleted_index, (_, old_oid)) in remaining_deleted.iter().enumerate() {
            let old_data = read_blob_data(repo, old_oid)?;
            let old_lines = count_lines(&old_data);
            for (added_index, (_, new_oid)) in remaining_added.iter().enumerate() {
                let new_data = read_blob_data(repo, new_oid)?;
                let new_lines = count_lines(&new_data);
                let longest = old_lines.max(new_lines);
                if longest == 0 {
                    // Two empty blobs carry no identity signal (git's
                    // rename tracking skips empty files too).
                    continue;
                }
                let (_, removals) = line_diff_counts(&old_data, &new_data);
                let common = old_lines.saturating_sub(removals);
                let similarity = common as f64 / longest as f64;
                if similarity >= RENAME_SIMILARITY {
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
}
