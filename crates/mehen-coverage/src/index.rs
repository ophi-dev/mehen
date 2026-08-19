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
//! resolves a query in two moves:
//!
//! * **Candidate collection** — report paths spelled absolute that
//!   exist on this machine carry a canonicalized on-disk identity; a
//!   query resolving to the same real file matches outright, and an
//!   entry whose identity *provably differs* from the query's is
//!   excluded. Everything else matches by component suffix (components,
//!   never bytes: `/foo/bar.rs` must not match `oofoo/bar.rs`).
//! * **Alias merging** — several report entries can be
//!   equivalence-proven spellings of one workspace file: the exact
//!   relative spelling, the same path behind a CI-absolute prefix, an
//!   `lcov -a` leg. All proven aliases *merge* (saturating-max, the
//!   cross-report rule) instead of the best-ranked one shadowing the
//!   rest. Distinct relative entries that merely share a suffix
//!   (`src/lib.rs` vs `vendor/dep/src/lib.rs`) are **not** aliases:
//!   the exact spelling wins when present, and a genuine tie is
//!   reported as [`FileMatch::Ambiguous`] rather than resolved by map
//!   order.
//!
//! Relative report paths are **never** canonicalized against the
//! process CWD — that would silently bind them to whatever happens to
//! exist under the tool's working directory.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

use camino::Utf8Path;

use crate::merge::{is_absolute_spelling, merge_file_into, normalize_components};
use crate::model::{CoverageData, FileCoverage};

/// Result of asking the index for a workspace file's coverage.
#[derive(Debug)]
pub enum FileMatch<'a> {
    /// One or more equivalence-proven report entries matched; several
    /// aliases arrive pre-merged.
    Found { coverage: Cow<'a, FileCoverage> },
    /// Several *distinct* report entries matched with equal
    /// specificity; matching any one of them would be a coin flip, so
    /// the file reads as unmeasured and the caller diagnoses it.
    Ambiguous { candidates: usize },
    /// No report entry matched.
    NotFound,
}

struct Entry {
    coverage: FileCoverage,
    components: Vec<String>,
    /// Whether the report spelled this path absolutely.
    absolute: bool,
    /// On-disk identity, when the absolute spelling exists here.
    canonical: Option<PathBuf>,
}

/// Calculate-once query structure over merged coverage data.
pub struct CoverageIndex {
    entries: Vec<Entry>,
    /// Last path component → entry ids, deterministic order.
    by_basename: BTreeMap<String, Vec<usize>>,
}

impl CoverageIndex {
    /// Build the index from merged, normalized coverage data (the output
    /// of [`crate::merge::merge_reports`]).
    #[must_use]
    pub fn build(data: CoverageData) -> Self {
        let mut entries = Vec::with_capacity(data.files.len());
        let mut by_basename: BTreeMap<String, Vec<usize>> = BTreeMap::new();

        for file in data.files {
            let absolute = is_absolute_spelling(&file.path);
            let components = normalize_components(&file.path);
            let Some(basename) = components.last() else {
                continue; // degenerate empty path
            };
            let id = entries.len();
            by_basename.entry(basename.clone()).or_default().push(id);

            // Canonicalize only paths the report spelled absolute — and
            // only when they exist here. Missing paths (reports produced
            // on another machine) participate through suffix matching.
            // Relative paths are never resolved against the CWD.
            let canonical = if absolute {
                std::fs::canonicalize(absolute_spelling(&components)).ok()
            } else {
                None
            };

            entries.push(Entry {
                coverage: file,
                components,
                absolute,
                canonical,
            });
        }

        Self {
            entries,
            by_basename,
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

    /// Look up coverage for a workspace file, spelled however the caller
    /// spells paths (CWD-relative, repo-relative, or absolute).
    #[must_use]
    pub fn file(&self, path: &Utf8Path) -> FileMatch<'_> {
        let query = normalize_components(path.as_str());
        let Some(basename) = query.last() else {
            return FileMatch::NotFound;
        };
        let Some(bucket) = self.by_basename.get(basename) else {
            return FileMatch::NotFound;
        };
        // The identity probe is one syscall per query; skip it when no
        // entry in this bucket carries an on-disk identity to compare
        // against (relative-spelled reports: LCOV, Go, JaCoCo).
        let query_canonical = if bucket
            .iter()
            .any(|&id| self.entries[id].canonical.is_some())
        {
            std::fs::canonicalize(path.as_std_path()).ok()
        } else {
            None
        };

        // Candidate collection: suffix-valid entries, minus those whose
        // on-disk identity provably differs from the query's.
        struct Candidate {
            id: usize,
            suffix: usize,
            entry_len: usize,
            identity_match: bool,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        for &id in bucket {
            let entry = &self.entries[id];
            let identity_match = match (&entry.canonical, &query_canonical) {
                (Some(e), Some(q)) => {
                    if e != q {
                        continue; // provably different files
                    }
                    true
                }
                _ => false,
            };
            let suffix = common_suffix_len(&query, &entry.components);
            // Valid only when the shorter side is fully consumed: the
            // report path is a tail of the workspace path (JaCoCo
            // package paths) or the workspace path is a tail of the
            // report path (CI prefixes, Go module prefixes). An
            // identity-proven entry is valid regardless of spelling.
            if !identity_match && (suffix == 0 || suffix < query.len().min(entry.components.len()))
            {
                continue;
            }
            candidates.push(Candidate {
                id,
                suffix,
                entry_len: entry.components.len(),
                identity_match,
            });
        }
        if candidates.is_empty() {
            return FileMatch::NotFound;
        }

        // Alias pool. Proven members: on-disk identity matches, exact
        // component equality, or a *longer absolute* spelling ending in
        // the full query (a checkout prefix from another machine — a
        // longer *relative* spelling is a more deeply nested, different
        // workspace file and never an alias).
        let full_query = |c: &Candidate| c.suffix == query.len();
        let exact = |c: &Candidate| full_query(c) && c.suffix == c.entry_len;
        let has_anchor = candidates.iter().any(|c| c.identity_match || exact(c));

        let pool: Vec<usize> = if has_anchor {
            candidates
                .iter()
                .filter(|c| {
                    c.identity_match || exact(c) || (full_query(c) && self.entries[c.id].absolute)
                })
                .map(|c| c.id)
                .collect()
        } else {
            let rel_longer: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| full_query(c) && !self.entries[c.id].absolute && !exact(c))
                .collect();
            if rel_longer.len() > 1 {
                return FileMatch::Ambiguous {
                    candidates: rel_longer.len(),
                };
            }
            let pool: Vec<usize> = candidates
                .iter()
                .filter(|c| full_query(c))
                .map(|c| c.id)
                .collect();
            if pool.is_empty() {
                // Entry-consumed direction (report path is a tail of
                // the workspace path): the longest suffix wins; a tie
                // between distinct entries is a coin flip we refuse.
                let best = candidates.iter().map(|c| c.suffix).max().unwrap_or(0);
                let tier: Vec<usize> = candidates
                    .iter()
                    .filter(|c| c.suffix == best)
                    .map(|c| c.id)
                    .collect();
                if tier.len() > 1 {
                    return FileMatch::Ambiguous {
                        candidates: tier.len(),
                    };
                }
                tier
            } else {
                pool
            }
        };

        match pool.as_slice() {
            [] => FileMatch::NotFound,
            [single] => FileMatch::Found {
                coverage: Cow::Borrowed(&self.entries[*single].coverage),
            },
            [first, rest @ ..] => {
                // Merge equivalence-proven aliases so no spelling's
                // data is silently dropped (the cross-report
                // saturating-max rule, applied at query time).
                let mut merged = self.entries[*first].coverage.clone();
                for &id in rest {
                    merge_file_into(&mut merged, &self.entries[id].coverage);
                }
                FileMatch::Found {
                    coverage: Cow::Owned(merged),
                }
            }
        }
    }
}

/// Rebuild an absolute-spelled report path from its normalized
/// components for the on-disk identity probe. Windows drive-qualified
/// spellings (`C:\repo\src\lib.rs` → `["C:", "repo", …]`) keep the
/// drive prefix bare — a leading `/` would produce `/C:/repo/…`, which
/// `canonicalize` rejects on Windows, silently downgrading every
/// drive-spelled entry from identity matching to suffix matching.
fn absolute_spelling(components: &[String]) -> PathBuf {
    let joined = components.join("/");
    let drive_qualified = components.first().is_some_and(|first| {
        first.len() == 2 && first.as_bytes()[0].is_ascii_alphabetic() && first.ends_with(':')
    });
    if drive_qualified {
        PathBuf::from(joined)
    } else {
        PathBuf::from(format!("/{joined}"))
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
        entry_with_lines(path, &[(1, 1)])
    }

    fn entry_with_lines(path: &str, lines: &[(u32, u64)]) -> FileCoverage {
        let mut f = FileCoverage::new(path.to_string());
        f.lines = lines
            .iter()
            .map(|&(line_number, hit_count)| LineCoverage {
                line_number,
                hit_count,
            })
            .collect();
        f
    }

    fn index(paths: &[&str]) -> CoverageIndex {
        CoverageIndex::build(crate::merge::merge_reports(vec![CoverageData {
            files: paths.iter().map(|p| entry(p)).collect(),
        }]))
    }

    fn found<'a>(index: &'a CoverageIndex, query: &str) -> Option<Cow<'a, FileCoverage>> {
        match index.file(Utf8Path::new(query)) {
            FileMatch::Found { coverage } => Some(coverage),
            _ => None,
        }
    }

    fn found_path(index: &CoverageIndex, query: &str) -> Option<String> {
        found(index, query).map(|c| c.path.clone())
    }

    #[test]
    fn absolute_spelling_preserves_windows_drive_prefixes() {
        let drive = ["C:".to_string(), "repo".into(), "lib.rs".into()];
        assert_eq!(absolute_spelling(&drive), PathBuf::from("C:/repo/lib.rs"));
        // POSIX spellings regain their root slash; a first component
        // that merely contains a colon (`a:b`, valid on POSIX) is not
        // a drive.
        let posix = ["home".to_string(), "a:b".into(), "lib.rs".into()];
        assert_eq!(absolute_spelling(&posix), PathBuf::from("/home/a:b/lib.rs"));
    }

    #[test]
    fn exact_relative_path_matches() {
        let idx = index(&["src/lib.rs"]);
        assert_eq!(
            found_path(&idx, "src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn ci_absolute_prefix_is_absorbed() {
        // Report written on a CI machine whose checkout root does not
        // exist here: the workspace-relative query suffix-matches.
        let idx = index(&["/home/runner/work/repo/repo/src/lib.rs"]);
        assert_eq!(
            found_path(&idx, "src/lib.rs").as_deref(),
            Some("/home/runner/work/repo/repo/src/lib.rs")
        );
    }

    #[test]
    fn jacoco_package_path_matches_longer_workspace_path() {
        // JaCoCo spells `package/File.java`; the workspace file carries
        // the `src/main/java/` prefix the report never saw.
        let idx = index(&["com/example/Foo.java"]);
        assert_eq!(
            found_path(&idx, "src/main/java/com/example/Foo.java").as_deref(),
            Some("com/example/Foo.java")
        );
    }

    #[test]
    fn go_module_prefix_is_absorbed() {
        // Go coverprofiles spell module import paths, not filesystem
        // paths.
        let idx = index(&["github.com/org/repo/pkg/handler.go"]);
        assert_eq!(
            found_path(&idx, "pkg/handler.go").as_deref(),
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
        assert_eq!(found_path(&idx, "bar.rs").as_deref(), Some("/foo/bar.rs"));
    }

    #[test]
    fn exact_spelling_wins_over_deeper_relative_suffix() {
        // cargo-crap spec 26: `src/lib.rs` vs `vendor/dep/src/lib.rs` —
        // a deeper *relative* entry is a different workspace file, not
        // an alias, so its data must not merge into the exact match.
        let idx = CoverageIndex::build(crate::merge::merge_reports(vec![CoverageData {
            files: vec![
                entry_with_lines("src/lib.rs", &[(1, 1)]),
                entry_with_lines("vendor/dep/src/lib.rs", &[(9, 9)]),
            ],
        }]));
        assert_eq!(
            found_path(&idx, "vendor/dep/src/lib.rs").as_deref(),
            Some("vendor/dep/src/lib.rs")
        );
        let exact = found(&idx, "src/lib.rs").expect("exact match");
        assert_eq!(exact.path, "src/lib.rs");
        assert!(
            exact.lines.iter().all(|l| l.line_number != 9),
            "vendor data must not merge into the exact match"
        );
    }

    #[test]
    fn absolute_and_relative_aliases_merge_their_data() {
        // The same file measured by two reports: one leg spelled
        // repo-relative, one behind a CI-absolute prefix. Both are
        // equivalence-proven spellings of the query and must merge —
        // returning only the "best" one would silently drop the other
        // leg's coverage.
        let relative = CoverageData {
            files: vec![entry_with_lines("src/lib.rs", &[(1, 1), (2, 0)])],
        };
        let absolute = CoverageData {
            files: vec![entry_with_lines(
                "/ci/work/repo/repo/src/lib.rs",
                &[(2, 3), (7, 1)],
            )],
        };
        let idx = CoverageIndex::build(crate::merge::merge_reports(vec![relative, absolute]));
        assert_eq!(idx.len(), 2, "spellings stay distinct in the merge layer");

        let merged = found(&idx, "src/lib.rs").expect("alias merge");
        let line = |n: u32| {
            merged
                .lines
                .iter()
                .find(|l| l.line_number == n)
                .map(|l| l.hit_count)
        };
        assert_eq!(line(1), Some(1));
        assert_eq!(line(2), Some(3), "max of both legs");
        assert_eq!(line(7), Some(1), "absolute-only line preserved");
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
        assert_eq!(
            found_path(&idx, "a/sub/mod.rs").as_deref(),
            Some("a/sub/mod.rs")
        );
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
            found_path(&idx, "elsewhere/nested/Cargo.toml").as_deref(),
            Some("nested/Cargo.toml")
        );
        assert_eq!(found_path(&idx, "unrelated.rs"), None);
    }

    #[test]
    fn dot_spelled_variants_match() {
        let idx = index(&["./src/app.py"]);
        assert_eq!(
            found_path(&idx, "src/app.py").as_deref(),
            Some("src/app.py")
        );
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
        let hit = found_path(&idx, &query).expect("canonical identity match");
        assert!(hit.ends_with("target_file.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn known_different_identity_is_excluded() {
        // An absolute entry that exists locally and resolves to a
        // *different* file must not be offered as suffix evidence for
        // this query.
        let dir = std::env::temp_dir().join(format!("mehen-cov-idx2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/sub")).unwrap();
        std::fs::create_dir_all(dir.join("b/sub")).unwrap();
        std::fs::write(dir.join("a/sub/f.rs"), "a\n").unwrap();
        std::fs::write(dir.join("b/sub/f.rs"), "b\n").unwrap();

        let report = format!("{}/a/sub/f.rs", dir.to_str().unwrap());
        let idx = index(&[&report]);
        let other = format!("{}/b/sub/f.rs", dir.to_str().unwrap());
        assert!(
            found_path(&idx, &other).is_none(),
            "provably different on-disk identity must not match"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
