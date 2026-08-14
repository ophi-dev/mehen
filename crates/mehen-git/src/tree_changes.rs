// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Deterministic tree-to-tree change detection with rename tracking.
//!
//! The tree walk and rename matcher come from `gix` plumbing. Unlike
//! `Repository::diff_tree_to_tree`, this module supplies an explicit
//! [`gix::diff::Rewrites`] configuration and a raw-object diff platform
//! with no repository attributes, filters, drivers, or diff settings.
//! Rename results therefore depend only on the compared trees.
//! This is the lower-level integration recommended by the `gix`
//! maintainer in GitoxideLabs/gitoxide#2915.
//!
//! Two narrow additions remain in-crate because `gix` does not provide
//! them: a bounded `-B`-style break-rewrite pass for reused paths, and
//! a byte-span fallback for edited one-line files that line-tokenized
//! similarity cannot recognize.
//!
//! Only blob entries are reported: directories, symlinks, and gitlinks
//! (submodules) carry no analyzable text. An entry changing *type*
//! across the trees is reported from the blob side — a file replaced
//! by a submodule is that file's deletion, and vice versa.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;

use gix::diff::blob::{Algorithm, Diff, InternedInput, sources::byte_lines};
use gix::diff::rewrites::tracker::ChangeKind;
use gix::diff::tree::recorder::Change as TreeRecord;
use gix::objs::TreeRefIter;
use gix::objs::tree::EntryMode;

use crate::GitError;

/// Similarity threshold for rename detection (git's `-M50%` default).
pub(crate) const RENAME_SIMILARITY: f64 = 0.5;

/// Upper bound on deletion×addition pairs examined by fuzzy rename
/// tracking within one tree diff. Exact (same-blob) renames are always
/// detected by `gix`; beyond this budget, inexact renames degrade to a
/// deletion + addition.
const RENAME_FUZZY_LIMIT: usize = 10_000;

/// Blobs larger than this never enter a fuzzy similarity pass. Sizes
/// are checked through object headers before data is materialized;
/// exact renames still match at any size.
pub(crate) const FUZZY_MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// Total bytes the byte-span fallback may materialize per tree diff.
/// Applied in sorted path order so truncation is deterministic.
const FUZZY_TOTAL_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Total bytes the break-rewrite scan may materialize per tree diff,
/// separate from the fuzzy fallback budget.
const BREAK_TOTAL_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Fixed-offset span bound for byte-span similarity.
const SPAN_FIXED_BYTES: usize = 64;

/// Bounds for content-defined similarity chunks.
const SPAN_MIN_BYTES: usize = 16;
const SPAN_MAX_BYTES: usize = 256;
const SPAN_CUT_MASK: u64 = 0x3F;

/// Binary detection window: git treats content with a NUL byte in the
/// first 8000 bytes as binary.
const BINARY_SNIFF_BYTES: usize = 8000;

/// Small modifications-only commits still get the break-rewrite scan
/// so edited swaps can be recovered without an add/delete loose end.
const SWAP_SCAN_MAX_MODIFICATIONS: usize = 8;

/// Spanhash similarity of two blobs, when both are loadable within
/// the per-blob cap (equal ids score 1.0).
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

/// Whether two blobs plausibly hold the same file lineage.
pub(crate) fn same_blob_lineage(
    repo: &gix::Repository,
    a: &gix::ObjectId,
    b: &gix::ObjectId,
) -> Result<bool, GitError> {
    Ok(blob_lineage_similarity(repo, a, b)?.is_some_and(|s| s >= RENAME_SIMILARITY))
}

/// Convert a git tree path to a native path without lossy UTF-8
/// replacement. A path that the platform cannot represent is skipped
/// at emission while still contributing one opaque changeset member.
pub(crate) fn path_from_git(path: &gix::bstr::BString) -> Option<PathBuf> {
    match gix::path::try_from_bstr(path.as_ref() as &gix::bstr::BStr) {
        Ok(path) => Some(path.into_owned()),
        Err(_) => {
            log::warn!(
                "skipping git path not representable on this platform: {}",
                String::from_utf8_lossy(path)
            );
            None
        }
    }
}

/// One file-level change between two trees, blob entries only.
pub(crate) enum TreeChange {
    Added {
        path: PathBuf,
        oid: gix::ObjectId,
    },
    Deleted {
        path: PathBuf,
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

/// The result of a tree-to-tree diff: analyzable blob changes with
/// renames joined, plus changed non-blob leaf entries for coupling.
pub(crate) struct TreeChanges {
    pub(crate) changes: Vec<TreeChange>,
    pub(crate) non_blob_changes: usize,
    /// Rename destinations whose source path cannot be represented on
    /// this platform. Their earlier lineage cannot be published safely.
    pub(crate) truncated_lineages: Vec<PathBuf>,
}

#[derive(Clone)]
struct RenameSide {
    path: gix::bstr::BString,
    oid: gix::ObjectId,
    mode: EntryMode,
    /// Index of the same-path modification split by the `-B` pass.
    broken: Option<usize>,
}

struct BlobModification {
    path: gix::bstr::BString,
    previous_oid: gix::ObjectId,
    previous_mode: EntryMode,
    oid: gix::ObjectId,
    mode: EntryMode,
}

struct BrokenPair {
    path: gix::bstr::BString,
    previous_oid: gix::ObjectId,
    oid: gix::ObjectId,
}

#[derive(Clone)]
struct TrackedChange {
    kind: ChangeKind,
    side: RenameSide,
}

struct RewriteMatches {
    renames: Vec<(RenameSide, RenameSide)>,
    added: Vec<RenameSide>,
    deleted: Vec<RenameSide>,
}

impl gix::diff::rewrites::tracker::Change for TrackedChange {
    fn id(&self) -> &gix::oid {
        &self.side.oid
    }

    fn relation(&self) -> Option<gix::diff::tree::visit::Relation> {
        None
    }

    fn kind(&self) -> ChangeKind {
        self.kind
    }

    fn entry_mode(&self) -> EntryMode {
        self.side.mode
    }

    fn id_and_entry_mode(&self) -> (&gix::oid, EntryMode) {
        (&self.side.oid, self.side.mode)
    }
}

/// Blob-to-blob modifications between two trees, with no rename
/// detection or blob loads.
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
        if let TreeRecord::Modification {
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
/// `None` means the empty tree.
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

    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut modified = Vec::new();
    let mut changes = Vec::new();
    let mut non_blob_changes = 0;
    let mut truncated_lineages = Vec::new();
    let mut non_blob_added: HashMap<(gix::objs::tree::EntryKind, gix::ObjectId), usize> =
        HashMap::new();
    let mut non_blob_deleted: HashMap<(gix::objs::tree::EntryKind, gix::ObjectId), usize> =
        HashMap::new();

    for change in recorder.records {
        match change {
            TreeRecord::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    added.push(RenameSide {
                        path,
                        oid,
                        mode: entry_mode,
                        broken: None,
                    });
                } else {
                    *non_blob_added.entry((entry_mode.kind(), oid)).or_insert(0) += 1;
                }
            }
            TreeRecord::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_blob() {
                    deleted.push(RenameSide {
                        path,
                        oid,
                        mode: entry_mode,
                        broken: None,
                    });
                } else {
                    *non_blob_deleted
                        .entry((entry_mode.kind(), oid))
                        .or_insert(0) += 1;
                }
            }
            TreeRecord::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
                path,
            } => match (previous_entry_mode.is_blob(), entry_mode.is_blob()) {
                (true, true) => modified.push(BlobModification {
                    path,
                    previous_oid,
                    previous_mode: previous_entry_mode,
                    oid,
                    mode: entry_mode,
                }),
                (true, false) => match path_from_git(&path) {
                    Some(path) => changes.push(TreeChange::Deleted {
                        path,
                        oid: previous_oid,
                    }),
                    None => non_blob_changes += 1,
                },
                (false, true) => match path_from_git(&path) {
                    Some(path) => changes.push(TreeChange::Added { path, oid }),
                    None => non_blob_changes += 1,
                },
                (false, false) => non_blob_changes += 1,
            },
        }
    }

    // An exact non-blob move is one changed identity, not a deletion
    // plus an addition. Only the count is needed by coupling.
    for (key, added_count) in non_blob_added {
        let deleted_count = non_blob_deleted.remove(&key).unwrap_or(0);
        non_blob_changes += added_count.max(deleted_count);
    }
    non_blob_changes += non_blob_deleted.into_values().sum::<usize>();

    let mut broken_pairs = Vec::new();
    let modified_previous_oids: HashSet<gix::ObjectId> = modified
        .iter()
        .filter(|change| change.previous_oid != change.oid)
        .map(|change| change.previous_oid)
        .collect();
    let modified_new_oids: HashSet<gix::ObjectId> = modified
        .iter()
        .filter(|change| change.previous_oid != change.oid)
        .map(|change| change.oid)
        .collect();
    let has_loose_ends = !added.is_empty() || !deleted.is_empty();
    let small_swap_scan =
        !has_loose_ends && modified.len() >= 2 && modified.len() <= SWAP_SCAN_MAX_MODIFICATIONS;

    modified.sort_by(|a, b| git_path_order(a.path.as_ref(), b.path.as_ref()));
    let mut break_budget = BREAK_TOTAL_BYTE_BUDGET;
    for modification in modified {
        let cross_matched = modified_new_oids.contains(&modification.previous_oid)
            || modified_previous_oids.contains(&modification.oid);
        let should_scan = (has_loose_ends || cross_matched || small_swap_scan)
            && modification.previous_oid != modification.oid;

        if should_scan {
            let old_size = blob_size(repo, &modification.previous_oid)?;
            let new_size = blob_size(repo, &modification.oid)?;
            let bytes = old_size.saturating_add(new_size);
            if old_size <= FUZZY_MAX_BLOB_BYTES
                && new_size <= FUZZY_MAX_BLOB_BYTES
                && bytes <= break_budget
            {
                break_budget -= bytes;
                let old_data = read_blob_data(repo, &modification.previous_oid)?;
                let new_data = read_blob_data(repo, &modification.oid)?;
                if spanhash_similarity(&old_data, &new_data) < RENAME_SIMILARITY {
                    let pair = broken_pairs.len();
                    broken_pairs.push(BrokenPair {
                        path: modification.path.clone(),
                        previous_oid: modification.previous_oid,
                        oid: modification.oid,
                    });
                    deleted.push(RenameSide {
                        path: modification.path.clone(),
                        oid: modification.previous_oid,
                        mode: modification.previous_mode,
                        broken: Some(pair),
                    });
                    added.push(RenameSide {
                        path: modification.path,
                        oid: modification.oid,
                        mode: modification.mode,
                        broken: Some(pair),
                    });
                    continue;
                }
            }
        }

        match path_from_git(&modification.path) {
            Some(path) => changes.push(TreeChange::Modified {
                path,
                previous_oid: modification.previous_oid,
                oid: modification.oid,
            }),
            None => non_blob_changes += 1,
        }
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

/// Match additions and deletions with `gix`'s deterministic rewrite
/// tracker over a raw-object, attribute-free diff platform.
#[allow(clippy::too_many_arguments)]
fn detect_renames(
    repo: &gix::Repository,
    changes: &mut Vec<TreeChange>,
    added: Vec<RenameSide>,
    deleted: Vec<RenameSide>,
    broken_pairs: &[BrokenPair],
    non_blob_changes: &mut usize,
    truncated_lineages: &mut Vec<PathBuf>,
) -> Result<(), GitError> {
    let mut diff_cache = raw_diff_platform(repo);

    // Exact matching is cheap and never materializes blob content, so
    // every candidate participates regardless of size or count.
    let exact = run_rewrite_tracker(repo, &mut diff_cache, added, deleted, None)?;
    for (source, destination) in exact.renames {
        push_renamed(
            changes,
            non_blob_changes,
            truncated_lineages,
            &source,
            &destination,
        );
    }

    // `gix` caches resources for matrix matching. Select a bounded,
    // deterministic subset per side before enabling fuzzy similarity
    // so the cache cannot grow with the repository's total blob volume.
    let (fuzzy_added, mut remaining_added) =
        partition_fuzzy_budget(repo, exact.added, FUZZY_TOTAL_BYTE_BUDGET)?;
    let (fuzzy_deleted, mut remaining_deleted) =
        partition_fuzzy_budget(repo, exact.deleted, FUZZY_TOTAL_BYTE_BUDGET)?;
    let fuzzy = run_rewrite_tracker(
        repo,
        &mut diff_cache,
        fuzzy_added,
        fuzzy_deleted,
        Some(RENAME_SIMILARITY as f32),
    )?;
    for (source, destination) in fuzzy.renames {
        push_renamed(
            changes,
            non_blob_changes,
            truncated_lineages,
            &source,
            &destination,
        );
    }
    remaining_added.extend(fuzzy.added);
    remaining_deleted.extend(fuzzy.deleted);
    drop(diff_cache);

    // `gix` tokenizes similarity by lines. Preserve rename identity
    // for edited one-line/minified files with a bounded raw-byte pass
    // over only the entries it left unmatched.
    match_remaining_by_spanhash(
        repo,
        changes,
        &mut remaining_added,
        &mut remaining_deleted,
        non_blob_changes,
        truncated_lineages,
    )?;

    reassemble_broken_pairs(
        changes,
        remaining_added,
        remaining_deleted,
        broken_pairs,
        non_blob_changes,
    );
    Ok(())
}

fn run_rewrite_tracker(
    repo: &gix::Repository,
    diff_cache: &mut gix::diff::blob::Platform,
    added: Vec<RenameSide>,
    deleted: Vec<RenameSide>,
    percentage: Option<f32>,
) -> Result<RewriteMatches, GitError> {
    let mut tracker = gix::diff::rewrites::Tracker::new(gix::diff::Rewrites {
        copies: None,
        percentage,
        limit: RENAME_FUZZY_LIMIT,
        track_empty: false,
    });

    for (kind, sides) in [
        (ChangeKind::Addition, added),
        (ChangeKind::Deletion, deleted),
    ] {
        for side in sides {
            // The tracker copies the location into its own backing.
            let location = side.path.clone();
            let rejected = tracker.try_push_change(
                TrackedChange { kind, side },
                location.as_ref() as &gix::bstr::BStr,
            );
            debug_assert!(rejected.is_none());
        }
    }

    let mut matches = RewriteMatches {
        renames: Vec::new(),
        added: Vec::new(),
        deleted: Vec::new(),
    };
    tracker
        .emit(
            |destination, source| {
                if let Some(source) = source {
                    let source_side = &source.change.side;
                    let destination_side = &destination.change.side;
                    // A speculative `-B` split must not pair back onto
                    // itself. Keep both halves for the fallback/reassembly.
                    if source_side.broken.is_some() && source_side.broken == destination_side.broken
                    {
                        matches.deleted.push(source_side.clone());
                        matches.added.push(destination_side.clone());
                    } else {
                        matches
                            .renames
                            .push((source_side.clone(), destination_side.clone()));
                    }
                } else {
                    match destination.change.kind {
                        ChangeKind::Addition => {
                            matches.added.push(destination.change.side);
                        }
                        ChangeKind::Deletion => {
                            matches.deleted.push(destination.change.side);
                        }
                        ChangeKind::Modification => {
                            unreachable!("copy tracking is disabled")
                        }
                    }
                }
                std::ops::ControlFlow::Continue(())
            },
            diff_cache,
            &repo.objects,
            |_| Ok::<(), Infallible>(()),
        )
        .map_err(|e| GitError::Internal(e.to_string()))?;

    Ok(matches)
}

/// Build a `gix` blob platform whose similarity input is exactly the
/// object database bytes. The empty attribute stack has no index or
/// worktree mappings, and the pipeline has no drivers or filters.
fn raw_diff_platform(repo: &gix::Repository) -> gix::diff::blob::Platform {
    let attributes = gix::worktree::stack::state::Attributes::new(
        gix::attrs::Search::default(),
        None,
        gix::worktree::stack::state::attributes::Source::IdMapping,
        gix::attrs::search::MetadataCollection::default(),
    );
    let attr_stack = gix::worktree::Stack::new(
        repo.git_dir(),
        gix::worktree::stack::State::AttributesStack(attributes),
        gix::glob::pattern::Case::Sensitive,
        Vec::with_capacity(512),
        Vec::new(),
    );

    let mut worktree_filter = gix::filter::plumbing::Pipeline::default();
    worktree_filter.options_mut().object_hash = repo.object_hash();
    let pipeline = gix::diff::blob::Pipeline::new(
        Default::default(),
        worktree_filter,
        Vec::new(),
        gix::diff::blob::pipeline::Options {
            large_file_threshold_bytes: FUZZY_MAX_BLOB_BYTES,
            fs: Default::default(),
        },
    );

    gix::diff::blob::Platform::new(
        gix::diff::blob::platform::Options {
            algorithm: Some(Algorithm::Histogram),
            skip_internal_diff_if_external_is_configured: false,
        },
        pipeline,
        gix::diff::blob::pipeline::Mode::ToGit,
        attr_stack,
    )
}

fn partition_fuzzy_budget(
    repo: &gix::Repository,
    entries: Vec<RenameSide>,
    budget: u64,
) -> Result<(Vec<RenameSide>, Vec<RenameSide>), GitError> {
    let mut sizes = Vec::with_capacity(entries.len());
    for side in &entries {
        sizes.push(blob_size(repo, &side.oid)?);
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| fuzzy_budget_order(&sizes, &entries, a, b));

    let mut remaining = budget;
    let mut selected = vec![false; entries.len()];
    for index in order {
        let size = sizes[index];
        if size > FUZZY_MAX_BLOB_BYTES || size > remaining {
            continue;
        }
        remaining -= size;
        selected[index] = true;
    }

    let mut within_budget = Vec::new();
    let mut deferred = Vec::new();
    for (entry, selected) in entries.into_iter().zip(selected) {
        if selected {
            within_budget.push(entry);
        } else {
            deferred.push(entry);
        }
    }
    Ok((within_budget, deferred))
}

fn match_remaining_by_spanhash(
    repo: &gix::Repository,
    changes: &mut Vec<TreeChange>,
    added: &mut Vec<RenameSide>,
    deleted: &mut Vec<RenameSide>,
    non_blob_changes: &mut usize,
    truncated_lineages: &mut Vec<PathBuf>,
) -> Result<(), GitError> {
    if added.is_empty()
        || deleted.is_empty()
        || !added
            .len()
            .checked_mul(deleted.len())
            .is_some_and(|pairs| pairs <= RENAME_FUZZY_LIMIT)
    {
        return Ok(());
    }

    added.sort_by(|a, b| git_path_order(a.path.as_ref(), b.path.as_ref()));
    deleted.sort_by(|a, b| git_path_order(a.path.as_ref(), b.path.as_ref()));

    let deleted_blobs = load_fuzzy_blobs(repo, deleted, FUZZY_TOTAL_BYTE_BUDGET)?;
    let mut added_sizes = Vec::with_capacity(added.len());
    for side in added.iter() {
        added_sizes.push(blob_size(repo, &side.oid)?);
    }
    let mut added_order: Vec<usize> = (0..added.len()).collect();
    added_order.sort_by(|&a, &b| fuzzy_budget_order(&added_sizes, added, a, b));

    let mut candidates = Vec::new();
    let mut stream_budget = FUZZY_TOTAL_BYTE_BUDGET;
    for added_index in added_order {
        let size = added_sizes[added_index];
        if size > FUZZY_MAX_BLOB_BYTES || size > stream_budget {
            continue;
        }
        stream_budget -= size;
        let new_blob = read_blob_data(repo, &added[added_index].oid)?;
        for (deleted_index, old_blob) in deleted_blobs.iter().enumerate() {
            let Some(old_blob) = old_blob else { continue };
            if deleted[deleted_index].broken.is_some()
                && deleted[deleted_index].broken == added[added_index].broken
            {
                continue;
            }
            if let Some(similarity) = spanhash_candidate(old_blob, &new_blob) {
                candidates.push((similarity, deleted_index, added_index));
            }
        }
    }

    candidates.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| git_path_order(deleted[a.1].path.as_ref(), deleted[b.1].path.as_ref()))
            .then_with(|| git_path_order(added[a.2].path.as_ref(), added[b.2].path.as_ref()))
    });

    let mut added_taken = vec![false; added.len()];
    let mut deleted_taken = vec![false; deleted.len()];
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
            &deleted[deleted_index],
            &added[added_index],
        );
    }

    *added = std::mem::take(added)
        .into_iter()
        .zip(added_taken)
        .filter_map(|(side, taken)| (!taken).then_some(side))
        .collect();
    *deleted = std::mem::take(deleted)
        .into_iter()
        .zip(deleted_taken)
        .filter_map(|(side, taken)| (!taken).then_some(side))
        .collect();
    Ok(())
}

fn reassemble_broken_pairs(
    changes: &mut Vec<TreeChange>,
    remaining_added: Vec<RenameSide>,
    remaining_deleted: Vec<RenameSide>,
    broken_pairs: &[BrokenPair],
    non_blob_changes: &mut usize,
) {
    let unpaired_deleted: HashSet<usize> = remaining_deleted
        .iter()
        .filter_map(|side| side.broken)
        .collect();
    let mut reassembled = vec![false; broken_pairs.len()];

    for side in &remaining_added {
        if let Some(pair) = side.broken
            && unpaired_deleted.contains(&pair)
        {
            let broken = &broken_pairs[pair];
            match path_from_git(&broken.path) {
                Some(path) => changes.push(TreeChange::Modified {
                    path,
                    previous_oid: broken.previous_oid,
                    oid: broken.oid,
                }),
                None => *non_blob_changes += 1,
            }
            reassembled[pair] = true;
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
}

/// Emit a paired rename, degrading consistently when either path is
/// not representable on this platform.
fn push_renamed(
    changes: &mut Vec<TreeChange>,
    non_blob_changes: &mut usize,
    truncated_lineages: &mut Vec<PathBuf>,
    source: &RenameSide,
    destination: &RenameSide,
) {
    match (
        path_from_git(&destination.path),
        path_from_git(&source.path),
    ) {
        (Some(path), Some(source_path)) => changes.push(TreeChange::Renamed {
            path,
            source_path,
            previous_oid: source.oid,
            oid: destination.oid,
        }),
        (Some(path), None) => {
            truncated_lineages.push(path.clone());
            changes.push(TreeChange::Added {
                path,
                oid: destination.oid,
            });
        }
        (None, Some(source_path)) => changes.push(TreeChange::Deleted {
            path: source_path,
            oid: source.oid,
        }),
        (None, None) => *non_blob_changes += 1,
    }
}

fn git_path_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.cmp(b)
}

fn fuzzy_budget_order(
    sizes: &[u64],
    entries: &[RenameSide],
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    fn basename(path: &gix::bstr::BString) -> &[u8] {
        let bytes: &[u8] = path.as_ref();
        bytes.rsplit(|&c| c == b'/').next().unwrap_or(bytes)
    }

    basename(&entries[a].path)
        .cmp(basename(&entries[b].path))
        .then_with(|| sizes[a].cmp(&sizes[b]))
        .then_with(|| git_path_order(entries[a].path.as_ref(), entries[b].path.as_ref()))
}

fn load_fuzzy_blobs(
    repo: &gix::Repository,
    entries: &[RenameSide],
    budget: u64,
) -> Result<Vec<Option<Vec<u8>>>, GitError> {
    let mut sizes = Vec::with_capacity(entries.len());
    for side in entries {
        sizes.push(blob_size(repo, &side.oid)?);
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| fuzzy_budget_order(&sizes, entries, a, b));

    let mut remaining = budget;
    let mut blobs: Vec<Option<Vec<u8>>> = (0..entries.len()).map(|_| None).collect();
    for index in order {
        let size = sizes[index];
        if size > FUZZY_MAX_BLOB_BYTES || size > remaining {
            continue;
        }
        remaining -= size;
        blobs[index] = Some(read_blob_data(repo, &entries[index].oid)?);
    }
    Ok(blobs)
}

fn spanhash_candidate(old: &[u8], new: &[u8]) -> Option<f64> {
    if old.is_empty() && new.is_empty() {
        return None;
    }
    let similarity = spanhash_similarity(old, new);
    (similarity >= RENAME_SIMILARITY).then_some(similarity)
}

/// Byte-weighted similarity scored under fixed and content-defined
/// chunking, taking the stronger result.
fn spanhash_similarity(old: &[u8], new: &[u8]) -> f64 {
    let fixed = span_multiset_similarity(old, new, fixed_spans);
    if fixed >= 1.0 {
        return fixed;
    }
    fixed.max(span_multiset_similarity(old, new, gear_spans))
}

fn span_multiset_similarity(old: &[u8], new: &[u8], chunk: fn(&[u8]) -> Vec<&[u8]>) -> f64 {
    let longest = old.len().max(new.len());
    if longest == 0 {
        return 0.0;
    }

    let mut available: HashMap<u64, u64> = HashMap::new();
    for span in chunk(old) {
        *available.entry(fnv1a(span)).or_insert(0) += span.len() as u64;
    }
    let mut common = 0;
    for span in chunk(new) {
        if let Some(bytes) = available.get_mut(&fnv1a(span)) {
            let take = (span.len() as u64).min(*bytes);
            *bytes -= take;
            common += take;
        }
    }
    common as f64 / longest as f64
}

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

fn gear_spans(data: &[u8]) -> Vec<&[u8]> {
    let mut rest = data;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let mut gear = 0u64;
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

const GEAR: [u64; 256] = build_gear_table();

const fn build_gear_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
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

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A blob's size from its object header, without loading the payload.
pub(crate) fn blob_size(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<u64, GitError> {
    Ok(repo
        .find_header(*oid)
        .map_err(|e| GitError::Internal(e.to_string()))?
        .size())
}

/// Whether content is binary by git's NUL-sniff heuristic.
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// Line-level (added, removed) counts using histogram diff.
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

/// Number of lines in a blob (a trailing fragment counts as a line).
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
        let old = b"fn a() {}\nfn b() {}\n";
        let new = b"fn a_renamed_with_many_bytes() {}\nfn b() {}\n";
        assert_eq!(line_diff_counts(old, new), (1, 1));
    }

    #[test]
    fn spanhash_recognizes_edited_single_line_files() {
        let old = format!("export const x = [{}];", "1, ".repeat(200));
        let new = old.replace("const x", "const y");
        let similarity = spanhash_candidate(old.as_bytes(), new.as_bytes())
            .expect("one-line edit should stay above the rename threshold");
        assert!(similarity >= RENAME_SIMILARITY, "got {similarity}");
    }

    #[test]
    fn spanhash_rejects_dissimilar_single_line_files() {
        let old = b"export const alpha_configuration_value = 1;";
        let new = b"#!/bin/sh @@ ~~ [[ ]] %% ^^ && || ;; :: ??";
        assert_eq!(spanhash_candidate(old, new), None);
    }

    #[test]
    fn spanhash_ignores_empty_blobs() {
        assert_eq!(spanhash_candidate(b"", b""), None);
    }

    #[test]
    fn spanhash_similarity_is_byte_weighted_not_line_weighted() {
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
