// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};

use crate::git_attributes::GitAttributeFilterSet;

/// Build a `GlobSet` from a list of glob strings, ignoring empty and invalid
/// entries.
///
/// Used by both the `diff` and `top-offenders` orchestrators to turn the
/// user's `--include` / `--exclude` flags into a usable matcher.
pub(crate) fn mk_globset<I, S>(elems: I) -> GlobSet
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut globset = GlobSetBuilder::new();
    for elem in elems {
        let elem = elem.as_ref();
        if !elem.is_empty()
            && let Ok(glob) = Glob::new(elem)
        {
            globset.add(glob);
        }
    }
    globset.build().unwrap_or_else(|_| GlobSet::empty())
}

fn is_file(entry: &DirEntry) -> bool {
    entry
        .file_type()
        .is_some_and(|file_type| file_type.is_file())
        || (entry.path_is_symlink() && entry.path().is_file())
}

fn path_matches(path: &Path, include: &GlobSet, exclude: &GlobSet) -> bool {
    (include.is_empty() || include.is_match(path))
        && (exclude.is_empty() || !exclude.is_match(path))
}

fn walk_builder(files_data: &FilesData) -> WalkBuilder {
    let mut builder = WalkBuilder::empty();
    for path in &files_data.paths {
        if path.exists() {
            builder.add(path);
        } else {
            log::warn!("File doesn't exist: {path:?}");
        }
    }

    // Keep the policy explicit: hidden entries, .ignore, .gitignore, the
    // repository-local exclude file, parent rules, and the global Git ignore
    // are all honored. `ignore` prunes ignored directories before they can
    // enqueue files for analysis.
    builder.standard_filters(true);
    if !files_data.respect_ignores {
        // `--no-ignore` disables ignore files without changing the established
        // behavior of omitting hidden children.
        builder
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    builder
}

fn log_ignore_error(entry: &DirEntry) {
    if let Some(error) = entry.error() {
        log::warn!(
            "Failed to apply an ignore rule while walking {}: {error}",
            entry.path().display()
        );
    }
}

/// Walk all configured roots serially and return matching files.
///
/// The public `rank_top_offenders` API uses this path. Analysis remains
/// serial there, but traversal follows exactly the same ignore policy as the
/// parallel CLI runner.
pub(crate) fn walk_files(files_data: &FilesData) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut attribute_filters = if files_data.respect_ignores {
        GitAttributeFilterSet::for_walk_paths(&files_data.paths)
    } else {
        GitAttributeFilterSet::default()
    };
    for result in walk_builder(files_data).build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("Failed to walk an input path: {error}");
                continue;
            }
        };
        log_ignore_error(&entry);
        if is_file(&entry)
            && path_matches(entry.path(), &files_data.include, &files_data.exclude)
            && !is_excluded_by_attributes(&mut attribute_filters, entry.path())
        {
            paths.push(entry.into_path());
        }
    }
    paths
}

type ProcFilesFunction<Config> = dyn Fn(PathBuf, &Config) -> std::io::Result<()> + Send + Sync;

/// An error encountered while walking files concurrently.
#[derive(Debug)]
pub(crate) enum ConcurrentErrors {
    /// Filesystem traversal failed.
    Walk(String),
    /// A worker panicked while traversing or processing a file.
    Worker(String),
}

impl std::fmt::Display for ConcurrentErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Walk(msg) => write!(f, "walk error: {msg}"),
            Self::Worker(msg) => write!(f, "worker error: {msg}"),
        }
    }
}

impl std::error::Error for ConcurrentErrors {}

/// Data related to files.
#[derive(Debug)]
pub(crate) struct FilesData {
    /// Kind of files included in a search.
    pub include: GlobSet,
    /// Kind of files excluded from a search.
    pub exclude: GlobSet,
    /// List of file paths.
    pub paths: Vec<PathBuf>,
    /// Whether standard ignore files and Git attributes should be respected.
    pub respect_ignores: bool,
}

/// A runner that traverses and processes files concurrently.
pub(crate) struct ConcurrentRunner<Config> {
    proc_files: Box<ProcFilesFunction<Config>>,
    num_jobs: usize,
}

impl<Config> std::fmt::Debug for ConcurrentRunner<Config> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrentRunner")
            .field("num_jobs", &self.num_jobs)
            .finish_non_exhaustive()
    }
}

impl<Config: 'static + Send + Sync> ConcurrentRunner<Config> {
    /// Creates a new `ConcurrentRunner`.
    ///
    /// * `num_jobs` - Number of jobs utilized to process files concurrently.
    /// * `proc_files` - Function that processes each file found during
    ///   the search.
    pub(crate) fn new<ProcFiles>(num_jobs: usize, proc_files: ProcFiles) -> Self
    where
        ProcFiles: 'static + Fn(PathBuf, &Config) -> std::io::Result<()> + Send + Sync,
    {
        Self {
            proc_files: Box::new(proc_files),
            num_jobs: num_jobs.max(1),
        }
    }

    /// Walk the configured roots and process each matching file in a traversal
    /// worker.
    ///
    /// `ignore::WalkParallel` schedules directories with work stealing and
    /// invokes `proc_files` directly. There is no producer channel that can
    /// accumulate one job per file while parsers are busy.
    pub(crate) fn run(self, config: Config, files_data: FilesData) -> Result<(), ConcurrentErrors> {
        let config = Arc::new(config);
        let proc_files: Arc<ProcFilesFunction<Config>> = Arc::from(self.proc_files);
        let include = Arc::new(files_data.include.clone());
        let exclude = Arc::new(files_data.exclude.clone());
        let walk_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let attribute_filters = if files_data.respect_ignores {
            GitAttributeFilterSet::for_walk_paths(&files_data.paths)
        } else {
            GitAttributeFilterSet::default()
        };

        let mut builder = walk_builder(&files_data);
        builder.threads(self.num_jobs);
        let walker = builder.build_parallel();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            walker.run(|| {
                let config = Arc::clone(&config);
                let proc_files = Arc::clone(&proc_files);
                let include = Arc::clone(&include);
                let exclude = Arc::clone(&exclude);
                let walk_error = Arc::clone(&walk_error);
                let mut attribute_filters = attribute_filters.clone();

                Box::new(move |result| {
                    let entry = match result {
                        Ok(entry) => entry,
                        Err(error) => {
                            if let Ok(mut slot) = walk_error.lock()
                                && slot.is_none()
                            {
                                *slot = Some(error.to_string());
                            }
                            return WalkState::Quit;
                        }
                    };
                    log_ignore_error(&entry);
                    if !is_file(&entry)
                        || !path_matches(entry.path(), include.as_ref(), exclude.as_ref())
                        || is_excluded_by_attributes(&mut attribute_filters, entry.path())
                    {
                        return WalkState::Continue;
                    }

                    let path = entry.into_path();
                    if let Err(error) = proc_files(path.clone(), config.as_ref()) {
                        log::error!("{error:?} for file {path:?}");
                    }
                    WalkState::Continue
                })
            });
        }));

        if result.is_err() {
            return Err(ConcurrentErrors::Worker(
                "a traversal worker panicked".to_owned(),
            ));
        }
        if let Some(error) = walk_error
            .lock()
            .map_err(|error| ConcurrentErrors::Worker(error.to_string()))?
            .take()
        {
            return Err(ConcurrentErrors::Walk(error));
        }
        Ok(())
    }
}

fn is_excluded_by_attributes(filters: &mut GitAttributeFilterSet, path: &Path) -> bool {
    match filters.excludes_path(path) {
        Ok(excluded) => excluded,
        Err(error) => {
            log::warn!(
                "Failed to apply Git attributes to {}: {error}",
                path.display()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_names(paths: &[PathBuf]) -> Vec<&str> {
        let mut names: Vec<&str> = paths
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect();
        names.sort_unstable();
        names
    }

    fn files_data(root: PathBuf) -> FilesData {
        FilesData {
            include: GlobSet::empty(),
            exclude: GlobSet::empty(),
            paths: vec![root],
            respect_ignores: true,
        }
    }

    #[test]
    fn walk_files_respects_gitignore_and_nested_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/generated")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "* -linguist-generated -linguist-vendored -binary\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/.ignore"), "generated/\n").unwrap();
        std::fs::write(dir.path().join("src/main.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("build/output.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("src/generated/output.py"), "x = 1\n").unwrap();

        let paths = walk_files(&files_data(dir.path().to_path_buf()));

        assert_eq!(file_names(&paths), vec!["main.py"]);
    }

    #[test]
    fn walk_files_respects_parent_gitignore_when_root_is_nested() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("src/generated")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "src/generated/\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "* -linguist-generated -linguist-vendored -binary\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/main.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("src/generated/output.py"), "x = 1\n").unwrap();

        let paths = walk_files(&files_data(dir.path().join("src")));

        assert_eq!(file_names(&paths), vec!["main.py"]);
    }

    #[test]
    fn walk_files_respects_git_info_exclude() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::write(dir.path().join(".git/info/exclude"), "local.py\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "* -linguist-generated -linguist-vendored -binary\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("kept.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("local.py"), "x = 1\n").unwrap();

        let paths = walk_files(&files_data(dir.path().to_path_buf()));

        assert_eq!(file_names(&paths), vec!["kept.py"]);
    }

    #[test]
    fn explicit_ignored_file_is_still_processed() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        let ignored = dir.path().join("ignored.py");
        std::fs::write(dir.path().join(".gitignore"), "ignored.py\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "ignored.py linguist-generated\n",
        )
        .unwrap();
        std::fs::write(&ignored, "x = 1\n").unwrap();

        let paths = walk_files(&files_data(ignored.clone()));

        assert_eq!(paths, vec![ignored]);
    }

    #[test]
    fn no_ignore_disables_ignore_files_but_keeps_hidden_children_hidden() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join(".cache")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.py\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "generated.py linguist-generated\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("generated.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join(".cache/hidden.py"), "x = 1\n").unwrap();

        let mut data = files_data(dir.path().to_path_buf());
        data.respect_ignores = false;
        let paths = walk_files(&data);

        assert_eq!(file_names(&paths), vec!["generated.py", "ignored.py"]);
    }

    #[test]
    fn walk_files_respects_generated_vendored_and_binary_attributes() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
generated.py linguist-generated
vendored.py linguist-vendored
binary.py binary
",
        )
        .unwrap();
        for name in ["kept.py", "generated.py", "vendored.py", "binary.py"] {
            std::fs::write(dir.path().join(name), "x = 1\n").unwrap();
        }

        let paths = walk_files(&files_data(dir.path().to_path_buf()));

        assert_eq!(file_names(&paths), vec!["kept.py"]);
    }

    #[test]
    fn walk_files_applies_attributes_for_each_repository_root() {
        let first = tempfile::tempdir().unwrap();
        gix::init(first.path()).unwrap();
        std::fs::write(
            first.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
generated.py linguist-generated
",
        )
        .unwrap();
        std::fs::write(first.path().join("generated.py"), "x = 1\n").unwrap();
        std::fs::write(first.path().join("first.py"), "x = 1\n").unwrap();

        let second = tempfile::tempdir().unwrap();
        gix::init(second.path()).unwrap();
        std::fs::write(
            second.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
vendored.py linguist-vendored
",
        )
        .unwrap();
        std::fs::write(second.path().join("vendored.py"), "x = 1\n").unwrap();
        std::fs::write(second.path().join("second.py"), "x = 1\n").unwrap();

        let paths = walk_files(&FilesData {
            include: GlobSet::empty(),
            exclude: GlobSet::empty(),
            paths: vec![first.path().to_path_buf(), second.path().to_path_buf()],
            respect_ignores: true,
        });

        assert_eq!(file_names(&paths), vec!["first.py", "second.py"]);
    }

    #[test]
    fn parallel_runner_uses_the_same_ignore_policy() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "\
* -linguist-generated -linguist-vendored -binary
vendored.py linguist-vendored
",
        )
        .unwrap();
        std::fs::write(dir.path().join("main.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("vendored.py"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("node_modules/generated.py"), "x = 1\n").unwrap();

        let visited = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&visited);
        ConcurrentRunner::new(2, move |path, _: &()| {
            output.lock().unwrap().push(path);
            Ok(())
        })
        .run((), files_data(dir.path().to_path_buf()))
        .unwrap();

        let paths = visited.lock().unwrap();
        assert_eq!(file_names(&paths), vec!["main.py"]);
    }
}
