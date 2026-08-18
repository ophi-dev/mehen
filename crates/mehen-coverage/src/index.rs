// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! The coverage path index — mapping report-spelled paths onto the files
//! mehen analyzes.
//!
//! This is where coverage integrations silently fail (the cargo-crap
//! "path-matching problem"): complexity analysis sees workspace paths,
//! while reports contain whatever the coverage tool wrote —
//!
//! 1. absolute CI paths (`/home/runner/work/repo/repo/src/lib.rs`),
//! 2. workspace-relative paths (`src/lib.rs`),
//! 3. Java package paths missing the source-root prefix
//!    (`com/example/Foo.java` for `src/main/java/com/example/Foo.java`),
//! 4. Go module import paths carrying an extra prefix
//!    (`github.com/org/repo/pkg/f.go` for `pkg/f.go`),
//! 5. `./`/`../`-spelled variants of any of the above.
//!
//! A naive map lookup returns nothing for 100% of files when the two
//! sides disagree, and every function reads as 0% covered. The index
//! resolves queries in two levels:
//!
//! * **Canonical lookup** — report paths spelled absolute that exist on
//!   this machine are canonicalized at build time; a query that
//!   canonicalizes to the same real file matches exactly.
//! * **Component suffix match** — otherwise the query and entry match
//!   when one's component list is a trailing subsequence of the other's
//!   (components, never bytes: `/foo/bar.rs` must not match
//!   `oofoo/bar.rs`). The longest suffix wins; exact component equality
//!   outranks partial consumption; a genuine tie is reported as
//!   [`FileMatch::Ambiguous`] rather than resolved by map order.
//!
//! Relative report paths are **never** canonicalized against the
//! process CWD — that would silently bind them to whatever happens to
//! exist under the tool's working directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camino::Utf8Path;

use crate::merge::{is_absolute_spelling, normalize_components};
use crate::model::{CoverageData, FileCoverage};

/// Result of asking the index for a workspace file's coverage.
#[derive(Debug)]
pub enum FileMatch<'a> {
    /// Exactly one report entry matched. `entry_id` is stable for the
    /// index lifetime — callers aggregate matched ids to report
    /// report-only files afterwards.
    Found {
        coverage: &'a FileCoverage,
        entry_id: usize,
    },
    /// Several report entries matched with equal specificity; matching
    /// any one of them would be a coin flip, so the file reads as
    /// unmeasured and the caller diagnoses it.
    Ambiguous { candidates: usize },
    /// No report entry matched.
    NotFound,
}

struct Entry {
    coverage: FileCoverage,
    components: Vec<String>,
}

/// Calculate-once query structure over merged coverage data.
pub struct CoverageIndex {
    entries: Vec<Entry>,
    /// Last path component → entry ids, deterministic order.
    by_basename: BTreeMap<String, Vec<usize>>,
    /// Canonicalized on-disk identity → entry id, for report paths
    /// spelled absolute that exist on this machine.
    by_canonical: BTreeMap<PathBuf, usize>,
}

impl CoverageIndex {
    /// Build the index from merged, normalized coverage data (the output
    /// of [`crate::merge::merge_reports`]).
    #[must_use]
    pub fn build(data: CoverageData) -> Self {
        let mut entries = Vec::with_capacity(data.files.len());
        let mut by_basename: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_canonical: BTreeMap<PathBuf, usize> = BTreeMap::new();

        for file in data.files {
            let absolute = is_absolute_spelling(&file.path);
            let components = normalize_components(&file.path);
            let Some(basename) = components.last() else {
                continue; // degenerate empty path
            };
            let id = entries.len();
            by_basename.entry(basename.clone()).or_default().push(id);

            if absolute {
                // Canonicalize only paths the report spelled absolute —
                // and only when they exist here. Missing paths (reports
                // produced on another machine) fall back to suffix
                // matching. Relative paths are never resolved against
                // the CWD.
                let spelled = PathBuf::from(format!("/{}", components.join("/")));
                if let Ok(canonical) = std::fs::canonicalize(&spelled) {
                    by_canonical.entry(canonical).or_insert(id);
                }
            }

            entries.push(Entry {
                coverage: file,
                components,
            });
        }

        Self {
            entries,
            by_basename,
            by_canonical,
        }
    }

    /// Number of report file entries behind the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index holds no entries at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The normalized report path of an entry, for diagnostics.
    #[must_use]
    pub fn entry_path(&self, entry_id: usize) -> Option<&str> {
        self.entries.get(entry_id).map(|e| e.coverage.path.as_str())
    }

    /// Look up coverage for a workspace file, spelled however the caller
    /// spells paths (CWD-relative, repo-relative, or absolute).
    #[must_use]
    pub fn file(&self, path: &Utf8Path) -> FileMatch<'_> {
        // Level 1: canonical identity for absolute-spelled report paths
        // that exist on this machine.
        if !self.by_canonical.is_empty()
            && let Ok(canonical) = std::fs::canonicalize(path.as_std_path())
            && let Some(&id) = self.by_canonical.get(&canonical)
        {
            return FileMatch::Found {
                coverage: &self.entries[id].coverage,
                entry_id: id,
            };
        }

        // Level 2: component-wise suffix matching.
        let query = normalize_components(path.as_str());
        let Some(basename) = query.last() else {
            return FileMatch::NotFound;
        };
        let Some(candidates) = self.by_basename.get(basename) else {
            return FileMatch::NotFound;
        };

        // Rank: (matched suffix length, exact component equality).
        let mut best: (usize, bool) = (0, false);
        let mut best_ids: Vec<usize> = Vec::new();
        for &id in candidates {
            let entry = &self.entries[id];
            let s = common_suffix_len(&query, &entry.components);
            // Valid only when the shorter side is fully consumed: the
            // report path is a tail of the workspace path (JaCoCo
            // package paths, basename-only entries) or the workspace
            // path is a tail of the report path (CI prefixes, Go module
            // prefixes).
            if s == 0 || s < query.len().min(entry.components.len()) {
                continue;
            }
            // An entry spelled absolute that exists on this machine was
            // already given its chance at level 1; if the canonical
            // lookup didn't claim the query, a *full* absolute-path
            // consumption is still fine (same spelling), but guard
            // against a short relative query being swallowed whole by
            // an unrelated absolute path is already covered by the
            // suffix rule itself.
            let exact = s == query.len() && s == entry.components.len();
            let rank = (s, exact);
            if rank > best {
                best = rank;
                best_ids.clear();
                best_ids.push(id);
            } else if rank == best {
                best_ids.push(id);
            }
        }

        match best_ids.len() {
            0 => FileMatch::NotFound,
            1 => FileMatch::Found {
                coverage: &self.entries[best_ids[0]].coverage,
                entry_id: best_ids[0],
            },
            n => FileMatch::Ambiguous { candidates: n },
        }
    }
}

/// Number of trailing components shared by two component lists.
fn common_suffix_len(a: &[String], b: &[String]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LineCoverage;

    fn entry(path: &str) -> FileCoverage {
        let mut f = FileCoverage::new(path.to_string());
        f.lines = vec![LineCoverage {
            line_number: 1,
            hit_count: 1,
        }];
        f
    }

    fn index(paths: &[&str]) -> CoverageIndex {
        CoverageIndex::build(crate::merge::merge_reports(vec![CoverageData {
            files: paths.iter().map(|p| entry(p)).collect(),
        }]))
    }

    fn found_path<'a>(index: &'a CoverageIndex, query: &str) -> Option<&'a str> {
        match index.file(Utf8Path::new(query)) {
            FileMatch::Found { coverage, .. } => Some(coverage.path.as_str()),
            _ => None,
        }
    }

    #[test]
    fn exact_relative_path_matches() {
        let idx = index(&["src/lib.rs"]);
        assert_eq!(found_path(&idx, "src/lib.rs"), Some("src/lib.rs"));
    }

    #[test]
    fn ci_absolute_prefix_is_absorbed() {
        // Report written on a CI machine whose checkout root does not
        // exist here: the workspace-relative query suffix-matches.
        let idx = index(&["/home/runner/work/repo/repo/src/lib.rs"]);
        assert_eq!(
            found_path(&idx, "src/lib.rs"),
            Some("/home/runner/work/repo/repo/src/lib.rs")
        );
    }

    #[test]
    fn jacoco_package_path_matches_longer_workspace_path() {
        // JaCoCo spells `package/File.java`; the workspace file carries
        // the `src/main/java/` prefix the report never saw.
        let idx = index(&["com/example/Foo.java"]);
        assert_eq!(
            found_path(&idx, "src/main/java/com/example/Foo.java"),
            Some("com/example/Foo.java")
        );
    }

    #[test]
    fn go_module_prefix_is_absorbed() {
        // Go coverprofiles spell module import paths, not filesystem
        // paths.
        let idx = index(&["github.com/org/repo/pkg/handler.go"]);
        assert_eq!(
            found_path(&idx, "pkg/handler.go"),
            Some("github.com/org/repo/pkg/handler.go")
        );
    }

    #[test]
    fn component_boundaries_are_respected() {
        // "/foo/bar.rs" must not match "oofoo/bar.rs" — matching is on
        // components, never on byte suffixes.
        let idx = index(&["/foo/bar.rs"]);
        assert_eq!(found_path(&idx, "oofoo/bar.rs"), None);
        // Basename alone still matches (report consumed).
        assert_eq!(found_path(&idx, "bar.rs"), Some("/foo/bar.rs"));
    }

    #[test]
    fn longest_suffix_wins_over_shorter() {
        // cargo-crap spec 26: `src/lib.rs` vs `vendor/dep/src/lib.rs` —
        // the most specific suffix wins for the specific query.
        let idx = index(&["src/lib.rs", "vendor/dep/src/lib.rs"]);
        assert_eq!(
            found_path(&idx, "vendor/dep/src/lib.rs"),
            Some("vendor/dep/src/lib.rs")
        );
        // Exact equality outranks partial consumption of the longer
        // vendor entry.
        assert_eq!(found_path(&idx, "src/lib.rs"), Some("src/lib.rs"));
    }

    #[test]
    fn genuine_tie_is_ambiguous_not_map_order() {
        // Two distinct report entries end in `sub/mod.rs`; a query that
        // cannot tell them apart must not silently pick one.
        let idx = index(&["a/sub/mod.rs", "b/sub/mod.rs"]);
        match idx.file(Utf8Path::new("sub/mod.rs")) {
            FileMatch::Ambiguous { candidates } => assert_eq!(candidates, 2),
            other => panic!("expected ambiguous, got {other:?}"),
        }
        // A more specific query resolves it.
        assert_eq!(found_path(&idx, "a/sub/mod.rs"), Some("a/sub/mod.rs"));
    }

    #[test]
    fn relative_entries_are_not_resolved_against_cwd() {
        // Regression pinned by cargo-crap: a relative report path must
        // not be canonicalized against the process CWD. This index has a
        // relative entry whose basename exists in *this* repository —
        // matching must still go through suffix logic (and succeed on
        // component identity), not through a CWD-canonicalized identity
        // that would shadow differently-rooted queries.
        let idx = index(&["nested/Cargo.toml"]);
        assert_eq!(
            found_path(&idx, "elsewhere/nested/Cargo.toml"),
            Some("nested/Cargo.toml")
        );
        assert_eq!(found_path(&idx, "unrelated.rs"), None);
    }

    #[test]
    fn dot_spelled_variants_match() {
        let idx = index(&["./src/app.py"]);
        assert_eq!(found_path(&idx, "src/app.py"), Some("src/app.py"));
    }

    #[test]
    fn absolute_report_path_existing_on_this_machine_matches_canonically() {
        // Build a real file, spell the report path absolutely, query via
        // a differently-spelled path to the same file.
        let dir = std::env::temp_dir().join(format!("mehen-cov-idx-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let real = dir.join("sub/target_file.rs");
        std::fs::write(&real, "fn main() {}\n").unwrap();

        let report_path = real.to_str().unwrap().to_string();
        let idx = index(&[&report_path]);
        // Query through a `..`-spelled variant of the same on-disk file.
        let query = format!("{}/sub/../sub/target_file.rs", dir.to_str().unwrap());
        match idx.file(Utf8Path::new(&query)) {
            FileMatch::Found { entry_id, .. } => {
                assert!(
                    idx.entry_path(entry_id)
                        .unwrap()
                        .ends_with("target_file.rs")
                );
            }
            other => panic!("expected canonical match, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
