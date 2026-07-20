// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

const EXCLUDED_ATTRIBUTES: [&str; 3] = ["linguist-generated", "linguist-vendored", "binary"];

/// Matches repository-relative paths against attributes that identify files
/// which should not be analyzed by default.
#[derive(Clone)]
pub(crate) struct GitAttributeFilter {
    worktree: Option<PathBuf>,
    attrs: gix::worktree::Stack,
    objects: gix::OdbHandle,
    outcome: gix::attrs::search::Outcome,
}

impl GitAttributeFilter {
    pub(crate) fn new(repo: &gix::Repository) -> Result<Self, Box<dyn std::error::Error>> {
        let worktree = repo
            .workdir()
            .map(|path| std::fs::canonicalize(path).or_else(|_| std::path::absolute(path)))
            .transpose()?;
        let index = repo.index_or_empty()?;
        let source = gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping
            .adjust_for_bare(repo.is_bare());
        let attrs = repo.attributes_only(&index, source)?;
        let outcome = attrs.selected_attribute_matches(EXCLUDED_ATTRIBUTES);

        Ok(Self {
            worktree,
            attrs: attrs.detach(),
            objects: repo.objects.clone(),
            outcome,
        })
    }

    pub(crate) fn from_revision(
        repo: &gix::Repository,
        revision: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let tree_id = repo
            .rev_parse_single(revision)?
            .object()?
            .peel_to_commit()?
            .tree_id()?;
        let index = repo.index_from_tree(&tree_id)?;
        // `Repository::attributes_only` also injects info and configured
        // global attributes. Build the virtual stack directly so historical
        // reports depend only on committed files and Git's built-in macros.
        let mut buffer = Vec::with_capacity(512);
        let mut collection = gix::attrs::search::MetadataCollection::default();
        let globals = gix::attrs::Search::new_globals(
            std::iter::empty::<PathBuf>(),
            &mut buffer,
            &mut collection,
        )?;
        let attributes = gix::worktree::stack::state::Attributes::new(
            globals,
            None,
            gix::worktree::stack::state::attributes::Source::IdMapping,
            collection,
        );
        let attrs = gix::worktree::Stack::from_state_and_ignore_case(
            repo.workdir().unwrap_or(repo.git_dir()),
            repo.config_snapshot()
                .boolean("core.ignoreCase")
                .unwrap_or(false),
            gix::worktree::stack::State::AttributesStack(attributes),
            &index,
            index.path_backing(),
        );
        let outcome = attrs.selected_attribute_matches(EXCLUDED_ATTRIBUTES);

        Ok(Self {
            worktree: None,
            attrs,
            objects: repo.objects.clone(),
            outcome,
        })
    }

    pub(crate) fn excludes_relative_path(&mut self, path: &Path) -> std::io::Result<bool> {
        self.attrs
            .at_path(path, None, &self.objects)?
            .matching_attributes(&mut self.outcome);
        Ok(self
            .outcome
            .iter_selected()
            .any(|matched| is_excluded_state(matched.assignment.state)))
    }
}

/// Repository roots found before or during a walk.
///
/// The registry contains only normalized paths, so it is cheap to share
/// between traversal workers. Each worker still owns its mutable gix
/// attribute stacks.
#[derive(Default)]
struct GitRepositoryRegistryInner {
    worktrees: RwLock<Vec<PathBuf>>,
    generation: AtomicUsize,
}

#[derive(Clone, Default)]
pub(crate) struct GitRepositoryRegistry {
    inner: Arc<GitRepositoryRegistryInner>,
}

impl GitRepositoryRegistry {
    fn for_walk_paths(paths: &[PathBuf]) -> Self {
        let registry = Self::default();
        for path in paths.iter().filter(|path| path.is_dir()) {
            registry.register_repository(path, false);
        }
        registry
    }

    pub(crate) fn discover_nested_repository(&self, directory: &Path) {
        if std::fs::symlink_metadata(directory.join(".git")).is_ok() {
            self.register_repository(directory, true);
        }
    }

    fn register_repository(&self, path: &Path, require_exact_root: bool) {
        let Ok(repo) = gix::discover(path) else {
            return;
        };
        let Some(worktree) = repo.workdir() else {
            return;
        };
        let Ok(worktree) =
            std::fs::canonicalize(worktree).or_else(|_| std::path::absolute(worktree))
        else {
            return;
        };
        if require_exact_root && worktree != path {
            return;
        }

        let mut worktrees = self
            .inner
            .worktrees
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if !worktrees.contains(&worktree) {
            worktrees.push(worktree);
            self.inner.generation.fetch_add(1, Ordering::Release);
        }
    }

    fn snapshot_if_changed(&self, previous_generation: usize) -> Option<(usize, Vec<PathBuf>)> {
        if self.inner.generation.load(Ordering::Acquire) == previous_generation {
            return None;
        }
        let worktrees = self
            .inner
            .worktrees
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let generation = self.inner.generation.load(Ordering::Relaxed);
        Some((generation, worktrees.clone()))
    }
}

/// Per-walker attribute filters. Each parallel traversal worker clones this
/// value so its mutable gix attribute stacks remain thread-local.
#[derive(Clone, Default)]
pub(crate) struct GitAttributeFilterSet {
    repositories: GitRepositoryRegistry,
    filters: Vec<GitAttributeFilter>,
    repository_generation: usize,
    loaded_worktrees: HashSet<PathBuf>,
    explicit_files: HashSet<PathBuf>,
}

impl GitAttributeFilterSet {
    pub(crate) fn for_walk_paths(paths: &[PathBuf]) -> Self {
        let explicit_files = paths
            .iter()
            .filter(|path| path.is_file())
            .cloned()
            .collect();
        let mut filters = Self {
            repositories: GitRepositoryRegistry::for_walk_paths(paths),
            filters: Vec::new(),
            repository_generation: 0,
            loaded_worktrees: HashSet::new(),
            explicit_files,
        };
        filters.sync_filters();
        filters
    }

    pub(crate) fn repository_registry(&self) -> GitRepositoryRegistry {
        self.repositories.clone()
    }

    pub(crate) fn excludes_path(&mut self, path: &Path) -> std::io::Result<bool> {
        if self.explicit_files.contains(path) {
            return Ok(false);
        }

        self.sync_filters();
        for filter in &mut self.filters {
            let Some(worktree) = &filter.worktree else {
                continue;
            };
            if let Ok(relative) = path.strip_prefix(worktree) {
                return filter.excludes_relative_path(relative);
            }
        }
        Ok(false)
    }

    fn sync_filters(&mut self) {
        let Some((generation, worktrees)) = self
            .repositories
            .snapshot_if_changed(self.repository_generation)
        else {
            return;
        };
        self.repository_generation = generation;
        let mut added = false;
        for worktree in worktrees {
            if !self.loaded_worktrees.insert(worktree.clone()) {
                continue;
            }
            let Ok(repo) = gix::discover(&worktree) else {
                log::warn!(
                    "Failed to discover Git repository at {}",
                    worktree.display()
                );
                continue;
            };
            match GitAttributeFilter::new(&repo) {
                Ok(filter) => {
                    self.filters.push(filter);
                    added = true;
                }
                Err(error) => log::warn!(
                    "Failed to configure Git attribute filtering for {}: {error}",
                    worktree.display()
                ),
            }
        }
        if added {
            // Prefer the innermost repository when worktrees are nested.
            self.filters.sort_by(|a, b| {
                b.worktree
                    .as_ref()
                    .map_or(0, |path| path.components().count())
                    .cmp(
                        &a.worktree
                            .as_ref()
                            .map_or(0, |path| path.components().count()),
                    )
            });
        }
    }
}

fn is_excluded_state(state: gix::attrs::StateRef<'_>) -> bool {
    match state {
        gix::attrs::StateRef::Set => true,
        gix::attrs::StateRef::Value(value) => {
            let value: &[u8] = value.as_bstr().as_ref();
            value.eq_ignore_ascii_case(b"true")
        }
        gix::attrs::StateRef::Unset | gix::attrs::StateRef::Unspecified => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_generated_vendored_and_binary_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
*.generated linguist-generated
*.vendored linguist-vendored=true
*.bin binary
*.false linguist-generated=false
*.unset -linguist-generated
*.unspecified !linguist-generated
*.upper linguist-generated=TRUE
*.py linguist-generated
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/.gitattributes"),
            "manual.py -linguist-generated -linguist-vendored -binary\n",
        )
        .unwrap();

        let mut filter = GitAttributeFilter::new(&repo).unwrap();

        for path in [
            "file.generated",
            "file.vendored",
            "file.bin",
            "file.upper",
            "src/generated.py",
        ] {
            assert!(
                filter.excludes_relative_path(Path::new(path)).unwrap(),
                "{path} should be excluded"
            );
        }
        for path in [
            "file.false",
            "file.unset",
            "file.unspecified",
            "src/manual.py",
            "file.txt",
        ] {
            assert!(
                !filter.excludes_relative_path(Path::new(path)).unwrap(),
                "{path} should be retained"
            );
        }
    }

    #[test]
    fn filter_reads_repository_info_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(".git/info/attributes"),
            "local.py linguist-vendored\n",
        )
        .unwrap();

        let mut filter = GitAttributeFilter::new(&repo).unwrap();

        assert!(
            filter
                .excludes_relative_path(Path::new("local.py"))
                .unwrap()
        );
    }

    #[test]
    fn relative_filtering_remains_available_in_bare_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        std::fs::write(dir.path().join("info/attributes"), "archive.py binary\n").unwrap();

        let mut filter = GitAttributeFilter::new(&repo).unwrap();

        assert!(
            filter
                .excludes_relative_path(Path::new("archive.py"))
                .unwrap()
        );
    }

    #[test]
    fn walk_filter_set_supports_multiple_repositories_and_explicit_files() {
        let first = tempfile::tempdir().unwrap();
        gix::init(first.path()).unwrap();
        std::fs::write(
            first.path().join(".gitattributes"),
            "*.py linguist-generated\n",
        )
        .unwrap();
        let first_generated = first.path().join("generated.py");
        std::fs::write(&first_generated, "x = 1\n").unwrap();

        let second = tempfile::tempdir().unwrap();
        gix::init(second.path()).unwrap();
        std::fs::write(
            second.path().join(".gitattributes"),
            "*.py linguist-vendored\n",
        )
        .unwrap();
        let second_vendored = second.path().join("vendored.py");
        std::fs::write(&second_vendored, "x = 1\n").unwrap();

        let first_root = std::fs::canonicalize(first.path()).unwrap();
        let first_generated = std::fs::canonicalize(first_generated).unwrap();
        let second_root = std::fs::canonicalize(second.path()).unwrap();
        let second_vendored = std::fs::canonicalize(second_vendored).unwrap();
        let mut filters = GitAttributeFilterSet::for_walk_paths(&[first_root.clone(), second_root]);
        assert!(filters.excludes_path(&first_generated).unwrap());
        assert!(filters.excludes_path(&second_vendored).unwrap());

        let mut explicit =
            GitAttributeFilterSet::for_walk_paths(&[first_root, first_generated.clone()]);
        assert!(!explicit.excludes_path(&first_generated).unwrap());
    }

    #[test]
    fn revision_filter_uses_only_attributes_from_the_requested_commit() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "generated.py linguist-generated\n",
        )
        .unwrap();
        for name in ["generated.py", "info-only.py", "global-only.py"] {
            std::fs::write(dir.path().join(name), "x = 1\n").unwrap();
        }
        let status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "-A"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args([
                "-c",
                "user.name=Mehen Test",
                "-c",
                "user.email=test@mehen.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "attributes",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        std::fs::write(
            dir.path().join(".gitattributes"),
            "generated.py -linguist-generated\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".git/info/attributes"),
            "\
generated.py -linguist-generated
info-only.py linguist-vendored
",
        )
        .unwrap();
        let global_dir = tempfile::tempdir().unwrap();
        let global_attributes = global_dir.path().join("attributes");
        std::fs::write(
            &global_attributes,
            "\
generated.py -linguist-generated
global-only.py binary
",
        )
        .unwrap();
        let status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["config", "core.attributesFile"])
            .arg(&global_attributes)
            .status()
            .unwrap();
        assert!(status.success());

        let repo = gix::discover(dir.path()).unwrap();
        let mut filter = GitAttributeFilter::from_revision(&repo, "HEAD").unwrap();

        assert!(
            filter
                .excludes_relative_path(Path::new("generated.py"))
                .unwrap()
        );
        for path in ["info-only.py", "global-only.py"] {
            assert!(
                !filter.excludes_relative_path(Path::new(path)).unwrap(),
                "{path} must not inherit checkout-local attributes"
            );
        }
    }
}
