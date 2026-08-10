// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-git` — git/repository operations and changed-file detection.
//!
//! Per rewrite plan §8.1, this is the home of the pre-1.0 `src/git.rs`
//! helpers. Phase-6+ may introduce a `Utf8PathBuf`-based API per plan
//! §4.8; for now the API surface matches the pre-1.0 shape (`PathBuf`)
//! so the still-in-place `src/diff.rs` keeps compiling unchanged.

#![deny(unsafe_code)]

mod history;

pub use history::{FileHistory, RepositoryHistory, collect_history};

use std::fmt;
use std::path::{Path, PathBuf};

/// Collapses any trailing run of `\n` / `\r` into a single `\n`.
///
/// When a blob has *no* trailing newline (or is empty), the buffer is
/// left unchanged — appending a synthetic `\n` would mutate repository
/// content and create spurious metric deltas between revisions.
///
/// Inlined from the pre-1.0 `src/tools.rs` so this crate has no
/// dependency on the legacy `mehen` library.
fn remove_blank_lines(data: &mut Vec<u8>) {
    let count_trailing = data
        .iter()
        .rev()
        .take_while(|&c| *c == b'\n' || *c == b'\r')
        .count();
    if count_trailing == 0 {
        return;
    }
    data.truncate(data.len() - count_trailing);
    data.push(b'\n');
}

#[derive(Debug)]
pub enum GitError {
    RepoNotFound,
    ShallowClone {
        hint: String,
    },
    RefNotFound(String),
    #[allow(dead_code)]
    BlobNotFound {
        rev: String,
        path: PathBuf,
    },
    Internal(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepoNotFound => write!(f, "Not a git repository."),
            Self::ShallowClone { hint } => write!(f, "Shallow clone detected. {hint}"),
            Self::RefNotFound(r) => write!(f, "Could not resolve ref '{r}'."),
            Self::BlobNotFound { rev, path } => {
                write!(f, "Could not find '{}' at rev '{rev}'.", path.display())
            }
            Self::Internal(msg) => write!(f, "Git error: {msg}"),
        }
    }
}

impl std::error::Error for GitError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub status: ChangeStatus,
    /// The pre-rename path when this change is a rename detected
    /// between the two revisions (`status` is then `Modified`).
    /// Callers should read the baseline side from this path.
    pub source_path: Option<PathBuf>,
}

/// Discover a git repository from the current working directory.
/// Fails fast on shallow clones.
pub fn open_repo() -> Result<gix::Repository, GitError> {
    open_repo_at(Path::new("."))
}

/// Discover the git repository containing `path` (walking up from it,
/// like `gix::discover`). Fails fast on shallow clones — history-based
/// features need the full commit graph.
pub fn open_repo_at(path: &Path) -> Result<gix::Repository, GitError> {
    let repo = gix::discover(path).map_err(|_| GitError::RepoNotFound)?;

    if repo.is_shallow() {
        return Err(GitError::ShallowClone {
            hint: "Use 'actions/checkout' with 'fetch-depth: 0' for full history.".to_string(),
        });
    }

    Ok(repo)
}

/// List files changed between two revisions via tree-to-tree diff with
/// git-style rename tracking (`-M50%`, pinned for determinism). A
/// renamed file is reported once as `Modified` under its new path with
/// [`ChangedFile::source_path`] set, instead of a deletion + addition
/// pair with full-value metric deltas.
pub fn changed_files(
    repo: &gix::Repository,
    from: &str,
    to: &str,
) -> Result<Vec<ChangedFile>, GitError> {
    let from_tree = resolve_tree(repo, from)?;
    let to_tree = resolve_tree(repo, to)?;

    let options = gix::diff::Options::default().with_rewrites(Some(history::rewrite_tracking()));
    let records = repo
        .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), options)
        .map_err(|e| GitError::Internal(e.to_string()))?;

    let files = records
        .into_iter()
        .map(|change| {
            use gix::object::tree::diff::ChangeDetached;
            match change {
                ChangeDetached::Addition { location, .. } => ChangedFile {
                    path: PathBuf::from(location.to_string()),
                    status: ChangeStatus::Added,
                    source_path: None,
                },
                ChangeDetached::Deletion { location, .. } => ChangedFile {
                    path: PathBuf::from(location.to_string()),
                    status: ChangeStatus::Deleted,
                    source_path: None,
                },
                ChangeDetached::Modification { location, .. } => ChangedFile {
                    path: PathBuf::from(location.to_string()),
                    status: ChangeStatus::Modified,
                    source_path: None,
                },
                ChangeDetached::Rewrite {
                    source_location,
                    location,
                    copy,
                    ..
                } => ChangedFile {
                    path: PathBuf::from(location.to_string()),
                    status: if copy {
                        // A copy's source still exists unchanged; only
                        // the destination is new content to review.
                        ChangeStatus::Added
                    } else {
                        ChangeStatus::Modified
                    },
                    source_path: (!copy).then(|| PathBuf::from(source_location.to_string())),
                },
            }
        })
        .collect();

    Ok(files)
}

/// Read file content at a specific revision. Returns `None` if the path
/// doesn't exist at that revision (e.g. newly added file with no baseline).
pub fn read_blob(
    repo: &gix::Repository,
    rev: &str,
    path: &Path,
) -> Result<Option<Vec<u8>>, GitError> {
    let tree = resolve_tree(repo, rev)?;

    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|e| GitError::Internal(e.to_string()))?;

    let Some(entry) = entry else {
        return Ok(None);
    };

    let object = entry
        .object()
        .map_err(|e| GitError::Internal(e.to_string()))?;

    let mut data = object.detach().data;
    remove_blank_lines(&mut data);
    Ok(Some(data))
}

/// Try to resolve a rev string to a friendly symbolic branch name.
///
/// Resolves `rev` to a commit OID, then scans local and remote branches for
/// one that points at the same commit.  Returns the short branch name
/// (e.g. `"main"`) on a match, or falls back to `rev` unchanged.
pub fn friendly_ref_label(repo: &gix::Repository, rev: &str) -> String {
    let friendly_name = (|| {
        let id = repo.rev_parse_single(rev).ok()?;
        let commit = id.object().ok()?.peel_to_commit().ok()?;
        let refs = repo.references().ok()?;

        find_branch_for_commit(&refs, commit.id, true)
            .or_else(|| find_branch_for_commit(&refs, commit.id, false))
    })();

    friendly_name.unwrap_or_else(|| rev.to_string())
}

fn find_branch_for_commit(
    refs: &gix::reference::iter::Platform<'_>,
    commit_id: gix::ObjectId,
    local: bool,
) -> Option<String> {
    let iter = if local {
        refs.local_branches().ok()?
    } else {
        refs.remote_branches().ok()?
    };
    let peeled = iter.peeled().ok()?;
    for reference in peeled.flatten() {
        if reference.id() == commit_id {
            let full = reference.name().as_bstr().to_string();
            return Some(shorten_ref_name(&full).to_string());
        }
    }
    None
}

/// Strip standard ref prefixes to produce a short branch name.
fn shorten_ref_name(full: &str) -> &str {
    full.strip_prefix("refs/heads/")
        .or_else(|| full.strip_prefix("refs/remotes/origin/"))
        .or_else(|| {
            full.strip_prefix("refs/remotes/")
                .and_then(|s: &str| s.split_once('/').map(|(_, branch)| branch))
        })
        .unwrap_or(full)
}

fn resolve_tree<'a>(repo: &'a gix::Repository, rev: &str) -> Result<gix::Tree<'a>, GitError> {
    let id = repo
        .rev_parse_single(rev)
        .map_err(|_| GitError::RefNotFound(rev.to_string()))?;

    let object = id.object().map_err(|e| GitError::Internal(e.to_string()))?;

    let commit = object
        .peel_to_commit()
        .map_err(|e| GitError::Internal(e.to_string()))?;

    commit.tree().map_err(|e| GitError::Internal(e.to_string()))
}
