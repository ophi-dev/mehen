// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::path::PathBuf;

use mehen_git::{ChangeStatus, ChangedFile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiProvider {
    GitHubActions,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CiContext {
    pub provider: CiProvider,
    pub event_name: String,
    pub base_ref: Option<String>,
    pub head_sha: Option<String>,
    /// For `push` events, the payload's `before` revision — the tip of
    /// the branch before the push. This is the correct diff baseline
    /// for a multi-commit push (`HEAD~1` would only cover the final
    /// commit). `None` when absent or all-zeros (branch creation).
    pub before_sha: Option<String>,
    /// For `push` events, the SHA of the *first* pushed commit. Its
    /// parent is the analysis baseline for branch-creation pushes,
    /// where the payload carries no usable `before` revision.
    pub first_commit_sha: Option<String>,
    /// Files changed by the CI event, with the change status folded
    /// across the commits in that event. For GitHub `push` events the
    /// per-commit `added` / `modified` / `removed` arrays are walked in
    /// order to derive the *final* per-path status (e.g. a file added
    /// in one commit and removed in a later one is dropped entirely;
    /// a file modified then removed is `Deleted`). Without that fold
    /// the per-file diff downstream loses the new/deleted semantics.
    pub changed_files: Option<Vec<ChangedFile>>,
    pub pr_number: Option<u64>,
    pub repository: Option<String>,
}

pub fn detect() -> Option<CiContext> {
    detect_github_actions()
}

fn detect_github_actions() -> Option<CiContext> {
    if std::env::var("GITHUB_ACTIONS").ok()?.as_str() != "true" {
        return None;
    }

    let event_name = std::env::var("GITHUB_EVENT_NAME").unwrap_or_default();
    let head_sha = std::env::var("GITHUB_SHA").ok();
    let repository = std::env::var("GITHUB_REPOSITORY").ok();

    let mut base_ref = std::env::var("GITHUB_BASE_REF")
        .ok()
        .filter(|s| !s.is_empty());
    let mut changed_files = None;
    let mut pr_number = None;
    let mut before_sha = None;
    let mut first_commit_sha = None;

    if let Ok(event_path) = std::env::var("GITHUB_EVENT_PATH")
        && let Ok(data) = std::fs::read_to_string(&event_path)
        && let Ok(payload) = serde_json::from_str::<serde_json::Value>(&data)
    {
        match event_name.as_str() {
            "push" => {
                changed_files = extract_push_changed_files(&payload);
                // The all-zeros SHA marks a branch creation — there is
                // no pre-push tip; `first_commit_sha`'s parent becomes
                // the baseline instead.
                before_sha = payload
                    .get("before")
                    .and_then(|b| b.as_str())
                    .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '0'))
                    .map(str::to_string);
                first_commit_sha = payload
                    .get("commits")
                    .and_then(|c| c.as_array())
                    .and_then(|commits| commits.first())
                    .and_then(|commit| commit.get("id"))
                    .and_then(|id| id.as_str())
                    .map(str::to_string);
            }
            "pull_request" => {
                if let Some(pr) = payload.get("pull_request") {
                    if base_ref.is_none() {
                        base_ref = pr
                            .get("base")
                            .and_then(|b| b.get("ref"))
                            .and_then(|r| r.as_str())
                            .map(|s| s.to_string());
                    }
                    pr_number = payload.get("number").and_then(|n| n.as_u64());
                }
            }
            "merge_group" => {
                if let Some(mg) = payload.get("merge_group")
                    && base_ref.is_none()
                {
                    base_ref = mg
                        .get("base_ref")
                        .and_then(|r| r.as_str())
                        .map(|s| s.to_string());
                }
            }
            _ => {}
        }
    }

    Some(CiContext {
        provider: CiProvider::GitHubActions,
        event_name,
        base_ref,
        head_sha,
        before_sha,
        first_commit_sha,
        changed_files,
        pr_number,
        repository,
    })
}

fn extract_push_changed_files(payload: &serde_json::Value) -> Option<Vec<ChangedFile>> {
    let commits = payload.get("commits")?.as_array()?;
    let mut by_path: std::collections::HashMap<PathBuf, ChangeStatus> =
        std::collections::HashMap::new();

    for commit in commits {
        if let Some(arr) = commit.get("added").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(path) = item.as_str() {
                    // A re-added file (was removed earlier in the
                    // push, now added again) becomes `Modified` in
                    // the final state — it existed before the push
                    // and exists after, just changed.
                    let key = PathBuf::from(path);
                    let status = match by_path.get(&key) {
                        Some(ChangeStatus::Deleted) => ChangeStatus::Modified,
                        _ => ChangeStatus::Added,
                    };
                    by_path.insert(key, status);
                }
            }
        }
        if let Some(arr) = commit.get("modified").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(path) = item.as_str() {
                    // A modify after add keeps the path as `Added`
                    // (the file is new in this push). Otherwise the
                    // path is `Modified`. A modify after delete is
                    // illegal in real GitHub payloads but we treat
                    // it as `Modified` for safety.
                    let key = PathBuf::from(path);
                    let status = match by_path.get(&key) {
                        Some(ChangeStatus::Added) => ChangeStatus::Added,
                        _ => ChangeStatus::Modified,
                    };
                    by_path.insert(key, status);
                }
            }
        }
        if let Some(arr) = commit.get("removed").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(path) = item.as_str() {
                    let key = PathBuf::from(path);
                    // A file that was added inside this push and then
                    // removed in a later commit is a no-op — it never
                    // existed at the head of the push, so drop it.
                    if matches!(by_path.get(&key), Some(ChangeStatus::Added)) {
                        by_path.remove(&key);
                    } else {
                        by_path.insert(key, ChangeStatus::Deleted);
                    }
                }
            }
        }
    }

    // An *empty* fold is meaningful and distinct from an unavailable
    // payload (`commits` missing entirely → `None` above): a push
    // whose commits add a file and then remove it changes nothing, and
    // callers must not fall back to a ref-range diff that would
    // misreport the final commit's deletion.
    let mut sorted: Vec<ChangedFile> = by_path
        .into_iter()
        .map(|(path, status)| ChangedFile {
            path,
            status,
            // GitHub push payloads carry no rename information.
            source_path: None,
        })
        .collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    Some(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_with_status(files: &[ChangedFile]) -> Vec<(PathBuf, ChangeStatus)> {
        files.iter().map(|f| (f.path.clone(), f.status)).collect()
    }

    #[test]
    fn test_extract_push_preserves_per_path_status() {
        let payload = serde_json::json!({
            "commits": [
                {
                    "added": ["src/new.rs"],
                    "modified": ["src/main.rs"],
                    "removed": ["src/old.rs"]
                },
                {
                    "added": [],
                    "modified": ["src/main.rs", "src/lib.rs"],
                    "removed": []
                }
            ]
        });

        let files = extract_push_changed_files(&payload).unwrap();
        assert_eq!(
            paths_with_status(&files),
            vec![
                (PathBuf::from("src/lib.rs"), ChangeStatus::Modified),
                (PathBuf::from("src/main.rs"), ChangeStatus::Modified),
                (PathBuf::from("src/new.rs"), ChangeStatus::Added),
                (PathBuf::from("src/old.rs"), ChangeStatus::Deleted),
            ]
        );
    }

    /// A file added in one commit and modified in a later commit is
    /// new at the head of the push, so its final status is `Added`
    /// (not `Modified` — the file did not exist before the push).
    #[test]
    fn test_extract_push_add_then_modify_is_added() {
        let payload = serde_json::json!({
            "commits": [
                {"added": ["src/new.rs"], "modified": [], "removed": []},
                {"added": [], "modified": ["src/new.rs"], "removed": []}
            ]
        });
        let files = extract_push_changed_files(&payload).unwrap();
        assert_eq!(
            paths_with_status(&files),
            vec![(PathBuf::from("src/new.rs"), ChangeStatus::Added)]
        );
    }

    /// A file added then removed in the same push is a no-op against
    /// the base — the fold is *empty* (`Some(vec![])`), which callers
    /// must honor rather than falling back to a ref-range diff that
    /// would misreport the final commit's deletion.
    #[test]
    fn test_extract_push_add_then_remove_folds_to_empty() {
        let payload = serde_json::json!({
            "commits": [
                {"added": ["src/scratch.rs"], "modified": [], "removed": []},
                {"added": [], "modified": [], "removed": ["src/scratch.rs"]}
            ]
        });
        let files = extract_push_changed_files(&payload).expect("payload is available");
        assert!(files.is_empty(), "add-then-remove folds to nothing");
    }

    /// A file modified then removed across the push is `Deleted` at
    /// the head — it existed before the push and doesn't anymore.
    #[test]
    fn test_extract_push_modify_then_remove_is_deleted() {
        let payload = serde_json::json!({
            "commits": [
                {"added": [], "modified": ["src/main.rs"], "removed": []},
                {"added": [], "modified": [], "removed": ["src/main.rs"]}
            ]
        });
        let files = extract_push_changed_files(&payload).unwrap();
        assert_eq!(
            paths_with_status(&files),
            vec![(PathBuf::from("src/main.rs"), ChangeStatus::Deleted)]
        );
    }

    /// A file removed then re-added across the push is `Modified` —
    /// it existed before, exists after, but its content changed.
    #[test]
    fn test_extract_push_remove_then_add_is_modified() {
        let payload = serde_json::json!({
            "commits": [
                {"added": [], "modified": [], "removed": ["src/main.rs"]},
                {"added": ["src/main.rs"], "modified": [], "removed": []}
            ]
        });
        let files = extract_push_changed_files(&payload).unwrap();
        assert_eq!(
            paths_with_status(&files),
            vec![(PathBuf::from("src/main.rs"), ChangeStatus::Modified)]
        );
    }

    #[test]
    fn test_extract_push_no_commits() {
        let payload = serde_json::json!({});
        assert!(extract_push_changed_files(&payload).is_none());
    }

    #[test]
    fn test_extract_push_empty_commits() {
        // An empty commits array is still an *available* payload with
        // nothing changed — distinct from a payload with no `commits`
        // key at all.
        let payload = serde_json::json!({
            "commits": []
        });
        let files = extract_push_changed_files(&payload).expect("payload is available");
        assert!(files.is_empty());
    }

    #[test]
    fn test_detect_not_github() {
        // Ensure GITHUB_ACTIONS is not set for this test
        // SAFETY: single-threaded test context; no other thread reads this var concurrently
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("GITHUB_ACTIONS");
        }
        assert!(detect().is_none());
    }
}
