// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

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

/// Per-walker attribute filters. Each parallel traversal worker clones this
/// value so its mutable gix attribute stacks remain thread-local.
#[derive(Clone, Default)]
pub(crate) struct GitAttributeFilterSet {
    filters: Vec<GitAttributeFilter>,
    explicit_files: HashSet<PathBuf>,
}

impl GitAttributeFilterSet {
    pub(crate) fn for_walk_paths(paths: &[PathBuf]) -> Self {
        let explicit_files = paths
            .iter()
            .filter(|path| path.is_file())
            .flat_map(|path| {
                [
                    std::path::absolute(path).ok(),
                    std::fs::canonicalize(path).ok(),
                ]
                .into_iter()
                .flatten()
            })
            .collect();

        let mut seen_worktrees = HashSet::new();
        let mut filters = Vec::new();
        for path in paths.iter().filter(|path| path.is_dir()) {
            let Ok(repo) = gix::discover(path) else {
                continue;
            };
            let Some(worktree) = repo.workdir() else {
                continue;
            };
            let Ok(worktree) =
                std::fs::canonicalize(worktree).or_else(|_| std::path::absolute(worktree))
            else {
                continue;
            };
            if !seen_worktrees.insert(worktree) {
                continue;
            }
            match GitAttributeFilter::new(&repo) {
                Ok(filter) => filters.push(filter),
                Err(error) => log::warn!(
                    "Failed to configure Git attribute filtering for {}: {error}",
                    path.display()
                ),
            }
        }

        // Prefer the innermost repository if callers provide roots from
        // nested worktrees.
        filters.sort_by(|a, b| {
            b.worktree
                .as_ref()
                .map_or(0, |path| path.components().count())
                .cmp(
                    &a.worktree
                        .as_ref()
                        .map_or(0, |path| path.components().count()),
                )
        });

        Self {
            filters,
            explicit_files,
        }
    }

    pub(crate) fn excludes_path(&mut self, path: &Path) -> std::io::Result<bool> {
        let absolute = std::path::absolute(path)?;
        if let Some(excluded) = self.excludes_absolute_path(&absolute)? {
            return Ok(excluded);
        }

        if let Ok(canonical) = std::fs::canonicalize(path)
            && canonical != absolute
            && let Some(excluded) = self.excludes_absolute_path(&canonical)?
        {
            return Ok(excluded);
        }
        Ok(false)
    }

    fn excludes_absolute_path(&mut self, path: &Path) -> std::io::Result<Option<bool>> {
        if self.explicit_files.contains(path) {
            return Ok(Some(false));
        }
        for filter in &mut self.filters {
            let Some(worktree) = &filter.worktree else {
                continue;
            };
            if let Ok(relative) = path.strip_prefix(worktree)
                && !relative
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                return filter.excludes_relative_path(relative).map(Some);
            }
        }
        Ok(None)
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

        let mut filters = GitAttributeFilterSet::for_walk_paths(&[
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]);
        assert!(filters.excludes_path(&first_generated).unwrap());
        assert!(filters.excludes_path(&second_vendored).unwrap());

        let mut explicit = GitAttributeFilterSet::for_walk_paths(&[
            first.path().to_path_buf(),
            first_generated.clone(),
        ]);
        assert!(!explicit.excludes_path(&first_generated).unwrap());
    }

    #[test]
    fn walk_filter_set_normalizes_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        gix::init(&root).unwrap();
        std::fs::write(
            root.join(".gitattributes"),
            "generated.py linguist-generated\n",
        )
        .unwrap();
        std::fs::write(root.join("generated.py"), "x = 1\n").unwrap();

        let non_normalized_root = root.join("..").join("repo");
        let non_normalized_file = non_normalized_root.join("generated.py");
        let mut filters = GitAttributeFilterSet::for_walk_paths(&[non_normalized_root]);

        assert!(filters.excludes_path(&non_normalized_file).unwrap());
    }
}
