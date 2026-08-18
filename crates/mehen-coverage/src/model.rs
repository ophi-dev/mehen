// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>
//
// Adapted from covrs (https://github.com/scttnlsn/covrs) `src/model.rs`,
// MIT-licensed by Scott Nelson. Local changes: serde derives, record
// normalization (sort + dedupe-by-max), and span-scoped totals for
// per-function metric injection. See LICENSE-THIRD-PARTY.

//! Uniform in-memory representation of coverage data, independent of any
//! specific report format. Parsers produce [`FileCoverage`] records; the
//! merge layer folds many reports into one [`CoverageData`]; the engine
//! queries totals at file scope and per function span.

use serde::Serialize;

/// Compute a coverage rate as a percentage in `0.0..=100.0`, returning
/// `None` when nothing was instrumentable — "no data" must stay
/// distinguishable from "0% covered" all the way to the metric layer.
#[must_use]
pub fn rate(covered: u64, total: u64) -> Option<f64> {
    if total == 0 {
        None
    } else {
        Some(covered as f64 * 100.0 / total as f64)
    }
}

/// A single line that was instrumentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineCoverage {
    pub line_number: u32,
    pub hit_count: u64,
}

/// A single branch arm on a given line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchCoverage {
    pub line_number: u32,
    pub branch_index: u32,
    pub hit_count: u64,
}

/// A function/method that was instrumentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionCoverage {
    pub name: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub hit_count: u64,
}

/// Covered/total counters for one measurement dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SpanTotals {
    pub covered: u64,
    pub total: u64,
}

impl SpanTotals {
    /// Coverage percentage, `None` when nothing was instrumentable.
    #[must_use]
    pub fn rate(self) -> Option<f64> {
        rate(self.covered, self.total)
    }
}

/// Coverage data for a single source file, as spelled by the report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileCoverage {
    pub path: String,
    pub lines: Vec<LineCoverage>,
    pub branches: Vec<BranchCoverage>,
    pub functions: Vec<FunctionCoverage>,
}

impl FileCoverage {
    #[must_use]
    pub fn new(path: String) -> Self {
        Self {
            path,
            ..Default::default()
        }
    }

    /// Canonicalize record order and collapse duplicates.
    ///
    /// Reports produced by merge tools (`lcov -a`) or emitted with the
    /// same line under two containers (Cobertura `<method>` + `<class>`)
    /// can repeat a record; the maximum hit count wins, matching the
    /// cross-report merge semantics ("covered anywhere ⇒ covered").
    /// Normalized records are the precondition for the binary-searched
    /// span queries below and for deterministic serialization.
    pub fn normalize(&mut self) {
        self.lines.sort_by_key(|l| l.line_number);
        self.lines.dedup_by(|next, kept| {
            if next.line_number == kept.line_number {
                kept.hit_count = kept.hit_count.max(next.hit_count);
                true
            } else {
                false
            }
        });

        self.branches
            .sort_by_key(|b| (b.line_number, b.branch_index));
        self.branches.dedup_by(|next, kept| {
            if (next.line_number, next.branch_index) == (kept.line_number, kept.branch_index) {
                kept.hit_count = kept.hit_count.max(next.hit_count);
                true
            } else {
                false
            }
        });

        self.functions
            .sort_by(|a, b| (a.start_line, &a.name).cmp(&(b.start_line, &b.name)));
        self.functions.dedup_by(|next, kept| {
            if next.name == kept.name && next.start_line == kept.start_line {
                kept.hit_count = kept.hit_count.max(next.hit_count);
                kept.end_line = kept.end_line.max(next.end_line);
                true
            } else {
                false
            }
        });
    }

    /// Line totals across the whole file.
    #[must_use]
    pub fn line_totals(&self) -> SpanTotals {
        SpanTotals {
            covered: self.lines.iter().filter(|l| l.hit_count > 0).count() as u64,
            total: self.lines.len() as u64,
        }
    }

    /// Branch-arm totals across the whole file.
    #[must_use]
    pub fn branch_totals(&self) -> SpanTotals {
        SpanTotals {
            covered: self.branches.iter().filter(|b| b.hit_count > 0).count() as u64,
            total: self.branches.len() as u64,
        }
    }

    /// Function totals across the whole file (report-recorded functions).
    #[must_use]
    pub fn function_totals(&self) -> SpanTotals {
        SpanTotals {
            covered: self.functions.iter().filter(|f| f.hit_count > 0).count() as u64,
            total: self.functions.len() as u64,
        }
    }

    /// Line totals restricted to an inclusive 1-based line range —
    /// the query per-function metric injection runs against each
    /// `MetricSpace` span. Requires [`Self::normalize`]d records.
    #[must_use]
    pub fn span_line_totals(&self, start_line: u32, end_line: u32) -> SpanTotals {
        let from = self.lines.partition_point(|l| l.line_number < start_line);
        let mut totals = SpanTotals::default();
        for line in &self.lines[from..] {
            if line.line_number > end_line {
                break;
            }
            totals.total += 1;
            if line.hit_count > 0 {
                totals.covered += 1;
            }
        }
        totals
    }

    /// Branch-arm totals restricted to an inclusive 1-based line range.
    /// Requires [`Self::normalize`]d records.
    #[must_use]
    pub fn span_branch_totals(&self, start_line: u32, end_line: u32) -> SpanTotals {
        let from = self
            .branches
            .partition_point(|b| b.line_number < start_line);
        let mut totals = SpanTotals::default();
        for branch in &self.branches[from..] {
            if branch.line_number > end_line {
                break;
            }
            totals.total += 1;
            if branch.hit_count > 0 {
                totals.covered += 1;
            }
        }
        totals
    }
}

/// The complete result of parsing a single coverage report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CoverageData {
    pub files: Vec<FileCoverage>,
}

impl CoverageData {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(line_number: u32, hit_count: u64) -> LineCoverage {
        LineCoverage {
            line_number,
            hit_count,
        }
    }

    #[test]
    fn normalize_dedupes_lines_keeping_max_hits() {
        let mut file = FileCoverage::new("a.rs".into());
        file.lines = vec![line(3, 0), line(1, 2), line(3, 5), line(2, 0)];
        file.normalize();
        assert_eq!(file.lines, vec![line(1, 2), line(2, 0), line(3, 5)]);
    }

    #[test]
    fn rate_distinguishes_no_data_from_zero() {
        assert_eq!(rate(0, 0), None);
        assert_eq!(rate(0, 4), Some(0.0));
        assert_eq!(rate(3, 4), Some(75.0));
        assert_eq!(rate(4, 4), Some(100.0));
    }

    #[test]
    fn span_totals_are_inclusive_and_bounded() {
        let mut file = FileCoverage::new("a.rs".into());
        file.lines = vec![line(1, 1), line(5, 0), line(6, 2), line(9, 0), line(20, 1)];
        file.normalize();
        // Function spanning lines 5..=9: three instrumentable lines, one hit.
        let totals = file.span_line_totals(5, 9);
        assert_eq!(
            totals,
            SpanTotals {
                covered: 1,
                total: 3
            }
        );
        assert_eq!(totals.rate(), Some(100.0 / 3.0));
        // A span with no instrumentable lines is "no data", not 0%.
        assert_eq!(file.span_line_totals(10, 19).rate(), None);
    }

    #[test]
    fn function_totals_count_hit_functions() {
        let mut file = FileCoverage::new("a.rs".into());
        file.functions = vec![
            FunctionCoverage {
                name: "hit".into(),
                start_line: Some(1),
                end_line: None,
                hit_count: 3,
            },
            FunctionCoverage {
                name: "missed".into(),
                start_line: Some(10),
                end_line: None,
                hit_count: 0,
            },
        ];
        assert_eq!(
            file.function_totals(),
            SpanTotals {
                covered: 1,
                total: 2
            }
        );
    }
}
