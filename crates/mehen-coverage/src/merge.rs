// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Deterministic multi-report merging.
//!
//! Auto-discovery (and monorepo CI) routinely yields several reports for
//! one workspace: per-package Jest runs, per-assembly dotnet
//! `TestResults`, `.nyc_output` shards, or an explicit list of paths. The
//! merge folds them into one [`CoverageData`]:
//!
//! * **Union of source files** — a file measured by any report is
//!   present.
//! * **Saturating-max hit counts** for records shared between reports —
//!   "covered anywhere ⇒ covered". Max is commutative and associative,
//!   so the result is independent of report enumeration order; summing
//!   was rejected because identical re-runs would double-count, and
//!   newest-wins was rejected because git checkouts do not preserve
//!   mtimes.
//!
//! Files are keyed by their *normalized* report path (forward slashes,
//! `.`/`..` segments resolved lexically) so `./src/lib.rs` and
//! `src/lib.rs` from two merged legs collapse into one record instead of
//! racing on map order.

use std::collections::BTreeMap;

use crate::model::{BranchCoverage, CoverageData, FileCoverage, FunctionCoverage, LineCoverage};

/// Normalize a report-spelled path for identity comparison and suffix
/// matching: backslashes become forward slashes (reports written on
/// Windows must match on any host), `.` segments drop, and `..` segments
/// pop lexically where possible. The result is a component list — the
/// unit of all path matching in this crate ("`/foo/bar.rs` must not
/// match `oofoo/bar.rs`").
#[must_use]
pub(crate) fn normalize_components(path: &str) -> Vec<String> {
    let mut components: Vec<String> = Vec::new();
    for raw in path.split(['/', '\\']) {
        match raw {
            "" | "." => {}
            ".." => {
                // Pop when possible; a leading `..` that cannot pop is
                // kept literally so distinct escapes stay distinct.
                if components.last().is_some_and(|c| c != "..") {
                    components.pop();
                } else {
                    components.push("..".to_string());
                }
            }
            other => components.push(other.to_string()),
        }
    }
    components
}

/// Whether the original path spelling was absolute (POSIX root or a
/// Windows drive/UNC prefix).
#[must_use]
pub(crate) fn is_absolute_spelling(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with('\\')
        || (path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
            && matches!(path.as_bytes()[2], b'/' | b'\\'))
}

/// Merge any number of parsed reports into one normalized
/// [`CoverageData`], ordered deterministically by normalized path.
#[must_use]
pub fn merge_reports(reports: Vec<CoverageData>) -> CoverageData {
    // Keyed by (absolute-spelling flag ++ normalized path) so an
    // absolute `/ci/src/lib.rs` and a relative `src/lib.rs` stay two
    // records — the index decides later whether they describe the same
    // workspace file; collapsing them here would guess.
    let mut by_path: BTreeMap<String, FileCoverage> = BTreeMap::new();

    for report in reports {
        for mut file in report.files {
            file.normalize();
            let mut key = if is_absolute_spelling(&file.path) {
                String::from("/")
            } else {
                String::new()
            };
            key.push_str(&normalize_components(&file.path).join("/"));

            match by_path.entry(key.clone()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    // Store under the normalized spelling so identical
                    // files from differently-spelled legs land together.
                    file.path = key;
                    slot.insert(file);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    merge_file_into(slot.get_mut(), file);
                }
            }
        }
    }

    CoverageData {
        files: by_path.into_values().collect(),
    }
}

/// Fold `incoming` into `kept` with saturating-max semantics. Both sides
/// must be normalized.
fn merge_file_into(kept: &mut FileCoverage, incoming: FileCoverage) {
    // Lines: keyed by line number.
    let mut lines: BTreeMap<u32, u64> = kept
        .lines
        .drain(..)
        .map(|l| (l.line_number, l.hit_count))
        .collect();
    for l in incoming.lines {
        let slot = lines.entry(l.line_number).or_insert(0);
        *slot = (*slot).max(l.hit_count);
    }
    kept.lines = lines
        .into_iter()
        .map(|(line_number, hit_count)| LineCoverage {
            line_number,
            hit_count,
        })
        .collect();

    // Branch arms: keyed by (line, arm index).
    let mut branches: BTreeMap<(u32, u32), u64> = kept
        .branches
        .drain(..)
        .map(|b| ((b.line_number, b.branch_index), b.hit_count))
        .collect();
    for b in incoming.branches {
        let slot = branches.entry((b.line_number, b.branch_index)).or_insert(0);
        *slot = (*slot).max(b.hit_count);
    }
    kept.branches = branches
        .into_iter()
        .map(|((line_number, branch_index), hit_count)| BranchCoverage {
            line_number,
            branch_index,
            hit_count,
        })
        .collect();

    // Functions: keyed by (start line, name).
    let mut functions: BTreeMap<(Option<u32>, String), (Option<u32>, u64)> = kept
        .functions
        .drain(..)
        .map(|f| ((f.start_line, f.name), (f.end_line, f.hit_count)))
        .collect();
    for f in incoming.functions {
        let slot = functions
            .entry((f.start_line, f.name))
            .or_insert((f.end_line, 0));
        slot.0 = slot.0.max(f.end_line);
        slot.1 = slot.1.max(f.hit_count);
    }
    kept.functions = functions
        .into_iter()
        .map(
            |((start_line, name), (end_line, hit_count))| FunctionCoverage {
                name,
                start_line,
                end_line,
                hit_count,
            },
        )
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, lines: &[(u32, u64)]) -> FileCoverage {
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

    fn data(files: Vec<FileCoverage>) -> CoverageData {
        CoverageData { files }
    }

    #[test]
    fn merge_is_order_independent() {
        let a = data(vec![file("src/lib.rs", &[(1, 0), (2, 3)])]);
        let b = data(vec![
            file("src/lib.rs", &[(1, 5), (3, 0)]),
            file("src/other.rs", &[(1, 1)]),
        ]);

        let ab = merge_reports(vec![a.clone(), b.clone()]);
        let ba = merge_reports(vec![b, a]);
        assert_eq!(ab, ba);

        assert_eq!(ab.files.len(), 2);
        let lib = &ab.files[0];
        assert_eq!(lib.path, "src/lib.rs");
        // Union of lines, max hits per line.
        assert_eq!(
            lib.lines,
            vec![
                LineCoverage {
                    line_number: 1,
                    hit_count: 5
                },
                LineCoverage {
                    line_number: 2,
                    hit_count: 3
                },
                LineCoverage {
                    line_number: 3,
                    hit_count: 0
                },
            ]
        );
    }

    #[test]
    fn different_spellings_of_one_file_collapse() {
        let a = data(vec![file("./src/lib.rs", &[(1, 1)])]);
        let b = data(vec![file("src/lib.rs", &[(2, 1)])]);
        let merged = merge_reports(vec![a, b]);
        assert_eq!(merged.files.len(), 1);
        assert_eq!(merged.files[0].path, "src/lib.rs");
        assert_eq!(merged.files[0].lines.len(), 2);
    }

    #[test]
    fn absolute_and_relative_spellings_stay_distinct() {
        // Whether `/ci/build/src/lib.rs` and `src/lib.rs` are the same
        // workspace file is the index's judgement call, not the merge's.
        let a = data(vec![file("/ci/build/src/lib.rs", &[(1, 1)])]);
        let b = data(vec![file("src/lib.rs", &[(1, 0)])]);
        let merged = merge_reports(vec![a, b]);
        assert_eq!(merged.files.len(), 2);
    }

    #[test]
    fn windows_separators_normalize() {
        let a = data(vec![file(r"src\win\mod.rs", &[(1, 1)])]);
        let merged = merge_reports(vec![a]);
        assert_eq!(merged.files[0].path, "src/win/mod.rs");
    }

    #[test]
    fn normalize_components_handles_dot_segments() {
        assert_eq!(
            normalize_components("./src/./lib.rs"),
            vec!["src", "lib.rs"]
        );
        assert_eq!(
            normalize_components("src/sub/../lib.rs"),
            vec!["src", "lib.rs"]
        );
        assert_eq!(normalize_components("../lib.rs"), vec!["..", "lib.rs"]);
    }
}
