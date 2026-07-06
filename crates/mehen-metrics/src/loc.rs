// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::collections::HashSet;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

/// Legacy line-class enum, kept for the small number of generic helpers
/// (`default_line_classifier`) that still classify whole physical lines
/// rather than AST nodes. Per-language LOC computation now goes through
/// the AST-based observation methods on [`LocStats`] so the per-
/// language rules (which nodes are containers, statements, comments)
/// match the pre-1.0 `Loc::compute` semantics exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineClass {
    Blank,
    Comment,
    Code,
    Logical,
}

/// Accumulator for the LOC family.
///
/// Mirrors the pre-1.0 `src/metrics/loc.rs` algorithm exactly so parity
/// snapshots compare directly:
///
/// - **SLOC**: physical line span of the space — `end_row - start_row`
///   for the unit, `end_row - start_row + 1` for nested spaces. Set
///   once via [`LocStats::set_span`] when the space opens.
/// - **PLOC**: number of distinct lines on which a "code" AST node
///   started — tracked as a `HashSet<u32>`.
/// - **LLOC**: count of statement-shaped AST nodes. Each language
///   classifies a node as LLOC via its rules.
/// - **CLOC**: tracks comment-only lines vs. comments adjacent to code
///   lines per the pre-1.0 `add_cloc_lines` /
///   `check_comment_ends_on_code_line` rules.
/// - **Blank** = `sloc - ploc - only_comment_lines`.
///
/// On space close, [`LocStats::finalize_minmax`] snapshots the per-
/// space totals into the rolled-up sums and `*_min` / `*_max` bounds
/// and bumps `space_count`. Averages divide by `space_count`.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct LocStats {
    // Span (set once at space open; SLOC is derived).
    span_start: u32,
    span_end: u32,
    span_is_unit: bool,
    span_set: bool,

    /// Distinct line numbers on which a non-container, non-comment,
    /// non-LLOC-statement node started. Counts as PLOC.
    ploc_lines: HashSet<u32>,
    /// Per-language statement count. Each LLOC node bumps this by one.
    lloc_count: u32,
    /// Lines that are *only* comments — neither preceded by code nor
    /// followed by it on the same line.
    only_comment_lines: u32,
    /// Comments that share their line with a code line.
    code_comment_lines: u32,
    /// End row of the most recent comment, used to detect a comment
    /// that ends just before a code line.
    last_comment_end: Option<u32>,

    // Rolled-up min/max bounds across spaces.
    pub sloc_min: u32,
    pub sloc_max: u32,
    pub ploc_min: u32,
    pub ploc_max: u32,
    pub lloc_min: u32,
    pub lloc_max: u32,
    pub cloc_min: u32,
    pub cloc_max: u32,
    pub blank_min: u32,
    pub blank_max: u32,

    /// Number of spaces folded into the bounds. Bumped by
    /// `finalize_minmax`; used as the average denominator.
    pub space_count: u32,
    /// Sentinel — set on first finalize so 0-valued bounds don't get
    /// wiped on subsequent finalizes.
    pub minmax_seen: bool,
}

impl LocStats {
    /// Set the physical line span of this space. The walker calls this
    /// once when the space opens (before any node observations).
    pub fn set_span(&mut self, start_row: u32, end_row: u32, is_unit: bool) {
        self.span_start = start_row;
        self.span_end = end_row;
        self.span_is_unit = is_unit;
        self.span_set = true;
    }

    /// Per-space SLOC = span (rows). The legacy convention adds `+1`
    /// for non-unit spaces to count the function-signature line, and
    /// uses bare `end - start` for the unit (where `end` is exclusive).
    pub fn sloc(&self) -> u32 {
        if !self.span_set {
            return 0;
        }
        let span = self.span_end.saturating_sub(self.span_start);
        if self.span_is_unit { span } else { span + 1 }
    }

    /// Per-space PLOC = number of distinct code lines.
    pub fn ploc(&self) -> u32 {
        self.ploc_lines.len() as u32
    }

    /// Per-space LLOC = number of statement-shaped nodes.
    pub fn lloc(&self) -> u32 {
        self.lloc_count
    }

    /// Per-space CLOC = comment-only lines + code-comment lines.
    pub fn cloc(&self) -> u32 {
        self.only_comment_lines
            .saturating_add(self.code_comment_lines)
    }

    /// Per-space blank = sloc - ploc - only_comment_lines.
    pub fn blank(&self) -> u32 {
        self.sloc()
            .saturating_sub(self.ploc())
            .saturating_sub(self.only_comment_lines)
    }

    /// Record a code line — a non-comment, non-container, non-LLOC
    /// node started on this row. Mirrors the `_` arm of the pre-1.0
    /// per-language `Loc::compute` match.
    pub fn observe_code_line(&mut self, start_row: u32) {
        self.check_comment_ends_on_code_line(start_row);
        self.ploc_lines.insert(start_row);
    }

    /// Record an LLOC statement.
    pub fn observe_lloc(&mut self) {
        self.lloc_count = self.lloc_count.saturating_add(1);
    }

    /// Record a comment node spanning rows `[start, end]` (inclusive).
    /// Mirrors `add_cloc_lines` semantics.
    pub fn observe_comment(&mut self, start_row: u32, end_row: u32) {
        let comment_diff = end_row.saturating_sub(start_row);
        let is_after_code = self.ploc_lines.contains(&start_row);
        if is_after_code && comment_diff == 0 {
            self.code_comment_lines = self.code_comment_lines.saturating_add(1);
        } else if is_after_code && comment_diff > 0 {
            self.code_comment_lines = self.code_comment_lines.saturating_add(1);
            self.only_comment_lines = self.only_comment_lines.saturating_add(comment_diff);
        } else if comment_diff > 0 && self.ploc_lines.contains(&end_row) {
            // Multi-line comment whose *closing* row carries code (e.g.
            // `/* c\n*/ val x = 1`). That last row is a code-comment, not
            // comment-only; only the rows strictly before it are
            // comment-only. Without this, the code-bearing closing row is
            // double-counted as comment-only and masks a real blank line
            // elsewhere (`blank = sloc - ploc - only_comment`).
            self.code_comment_lines = self.code_comment_lines.saturating_add(1);
            self.only_comment_lines = self.only_comment_lines.saturating_add(comment_diff);
        } else {
            self.only_comment_lines = self.only_comment_lines.saturating_add(comment_diff + 1);
            self.last_comment_end = Some(end_row);
        }
    }

    /// Pre-1.0 `check_comment_ends_on_code_line`: when a code node
    /// starts on the line right after the last comment ends, that
    /// comment is reclassified from "independent" to "before code".
    fn check_comment_ends_on_code_line(&mut self, start_code_row: u32) {
        if let Some(end) = self.last_comment_end
            && end == start_code_row
            && !self.ploc_lines.contains(&start_code_row)
        {
            self.only_comment_lines = self.only_comment_lines.saturating_sub(1);
            self.code_comment_lines = self.code_comment_lines.saturating_add(1);
        }
    }

    /// Snapshot the per-space totals into the `*_min` / `*_max` bounds.
    /// Mirrors the pre-1.0 `compute_minmax`: the parent space only
    /// snapshots its own values when no children have already
    /// initialized the bounds via merge. The `space_count` always
    /// bumps so averages divide by total spaces.
    pub fn finalize_minmax(&mut self) {
        self.space_count = self.space_count.saturating_add(1);
        if self.minmax_seen {
            // Children already initialized the bounds via merge — the
            // parent's per-space values were already part of `self`'s
            // accumulators, but the legacy convention does NOT fold
            // them into min/max again at the parent close.
            return;
        }
        let sloc = self.sloc();
        let ploc = self.ploc();
        let lloc = self.lloc();
        let cloc = self.cloc();
        let blank = self.blank();
        self.sloc_min = sloc;
        self.ploc_min = ploc;
        self.lloc_min = lloc;
        self.cloc_min = cloc;
        self.blank_min = blank;
        self.sloc_max = sloc;
        self.ploc_max = ploc;
        self.lloc_max = lloc;
        self.cloc_max = cloc;
        self.blank_max = blank;
        self.minmax_seen = true;
    }

    /// Copy the `ploc_lines` set from another `LocStats` into this
    /// one. Used by [`crate::SpaceRangeTracker::record_close`] to
    /// seed a token-routed accumulator with the AST-walk's known
    /// PLOC lines so comment-after-code classification works
    /// correctly. Does NOT touch any other field — only the set is
    /// copied.
    pub fn seed_ploc_lines(&mut self, source: &LocStats) {
        for line in &source.ploc_lines {
            self.ploc_lines.insert(*line);
        }
    }

    /// Adopt the source's code rows that fall in `[start_row, end_row)` into
    /// this space's PLOC set. Used when a member's own-line modifiers/
    /// annotations were observed on the enclosing class (they are visited
    /// before the member's space is pushed): the member space adopts those
    /// rows so its PLOC covers its full declaration. PLOC is hierarchical, so
    /// the enclosing space legitimately keeps the rows too. Only rows the
    /// source actually recorded as code are adopted (blank lines are skipped).
    pub fn adopt_code_lines_in_range(&mut self, source: &LocStats, start_row: u32, end_row: u32) {
        for line in &source.ploc_lines {
            if *line >= start_row && *line < end_row {
                self.ploc_lines.insert(*line);
            }
        }
    }

    /// Absorb a sibling's *token-only* observations into this state.
    ///
    /// Used by [`crate::SpaceRangeTracker`] to fold post-AST token
    /// events (PLOC code lines, CLOC comment lines) into a space's
    /// AST-driven LocStats without mutating min/max bounds, the
    /// space-count denominator, or the LLOC counter (which is
    /// AST-driven and would double-count if folded here).
    ///
    /// Semantics:
    /// - `ploc_lines` is set-unioned (a code line is counted once
    ///   regardless of how many tokens started on it).
    /// - `only_comment_lines` and `code_comment_lines` accumulate
    ///   (each comment-token contribution adds to the count).
    /// - `lloc_count`, `space_count`, span fields, and min/max
    ///   bounds are intentionally NOT touched here — those are
    ///   AST-driven invariants finalized before token routing.
    pub fn merge_token_observations(&mut self, other: &LocStats) {
        for line in &other.ploc_lines {
            self.ploc_lines.insert(*line);
        }
        self.only_comment_lines = self
            .only_comment_lines
            .saturating_add(other.only_comment_lines);
        self.code_comment_lines = self
            .code_comment_lines
            .saturating_add(other.code_comment_lines);
    }

    /// Merge a finalized child's stats into this (parent) one.
    ///
    /// Mirrors the pre-1.0 `loc::Stats::merge`:
    /// - SLOC: parent's span is unchanged; child contributes only via
    ///   min/max bounds (already snapshotted into `child.sloc_min/max`).
    /// - PLOC: parent's `ploc_lines` set absorbs the child's lines.
    /// - LLOC / CLOC: parent's per-space counters add the child's.
    /// - Blank is recomputed at publish time from the merged values.
    pub fn merge(&mut self, other: &LocStats) {
        for line in &other.ploc_lines {
            self.ploc_lines.insert(*line);
        }
        self.lloc_count = self.lloc_count.saturating_add(other.lloc_count);
        self.only_comment_lines = self
            .only_comment_lines
            .saturating_add(other.only_comment_lines);
        self.code_comment_lines = self
            .code_comment_lines
            .saturating_add(other.code_comment_lines);
        self.space_count = self.space_count.saturating_add(other.space_count);
        if !other.minmax_seen {
            return;
        }
        if self.minmax_seen {
            self.sloc_min = self.sloc_min.min(other.sloc_min);
            self.ploc_min = self.ploc_min.min(other.ploc_min);
            self.lloc_min = self.lloc_min.min(other.lloc_min);
            self.cloc_min = self.cloc_min.min(other.cloc_min);
            self.blank_min = self.blank_min.min(other.blank_min);
        } else {
            self.sloc_min = other.sloc_min;
            self.ploc_min = other.ploc_min;
            self.lloc_min = other.lloc_min;
            self.cloc_min = other.cloc_min;
            self.blank_min = other.blank_min;
            self.minmax_seen = true;
        }
        self.sloc_max = self.sloc_max.max(other.sloc_max);
        self.ploc_max = self.ploc_max.max(other.ploc_max);
        self.lloc_max = self.lloc_max.max(other.lloc_max);
        self.cloc_max = self.cloc_max.max(other.cloc_max);
        self.blank_max = self.blank_max.max(other.blank_max);
    }

    /// Comments-as-fraction-of-sloc, used by the maintainability index.
    pub fn comments_percentage(&self) -> f64 {
        let sloc = self.sloc();
        if sloc == 0 {
            0.0
        } else {
            f64::from(self.cloc()) / f64::from(sloc)
        }
    }

    pub fn sloc_average(&self) -> f64 {
        average(self.sloc(), self.space_count)
    }
    pub fn ploc_average(&self) -> f64 {
        average(self.ploc(), self.space_count)
    }
    pub fn lloc_average(&self) -> f64 {
        average(self.lloc(), self.space_count)
    }
    pub fn cloc_average(&self) -> f64 {
        average(self.cloc(), self.space_count)
    }
    pub fn blank_average(&self) -> f64 {
        average(self.blank(), self.space_count)
    }
}

fn average(numerator: u32, denominator: u32) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        f64::from(numerator) / f64::from(denominator)
    }
}

impl Serialize for LocStats {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("loc", 20)?;
        st.serialize_field("sloc", &self.sloc())?;
        st.serialize_field("ploc", &self.ploc())?;
        st.serialize_field("lloc", &self.lloc())?;
        st.serialize_field("cloc", &self.cloc())?;
        st.serialize_field("blank", &self.blank())?;
        st.serialize_field("sloc_average", &self.sloc_average())?;
        st.serialize_field("ploc_average", &self.ploc_average())?;
        st.serialize_field("lloc_average", &self.lloc_average())?;
        st.serialize_field("cloc_average", &self.cloc_average())?;
        st.serialize_field("blank_average", &self.blank_average())?;
        st.serialize_field("sloc_min", &self.sloc_min)?;
        st.serialize_field("sloc_max", &self.sloc_max)?;
        st.serialize_field("cloc_min", &self.cloc_min)?;
        st.serialize_field("cloc_max", &self.cloc_max)?;
        st.serialize_field("ploc_min", &self.ploc_min)?;
        st.serialize_field("ploc_max", &self.ploc_max)?;
        st.serialize_field("lloc_min", &self.lloc_min)?;
        st.serialize_field("lloc_max", &self.lloc_max)?;
        st.serialize_field("blank_min", &self.blank_min)?;
        st.serialize_field("blank_max", &self.blank_max)?;
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_percentage_when_empty() {
        let s = LocStats::default();
        assert_eq!(s.comments_percentage(), 0.0);
    }

    #[test]
    fn merge_sums_buckets() {
        let mut a = LocStats::default();
        a.set_span(0, 1, true);
        a.observe_code_line(0);
        a.observe_lloc();
        a.finalize_minmax();

        let mut b = LocStats::default();
        b.set_span(2, 4, false);
        b.observe_code_line(2);
        b.observe_lloc();
        b.observe_comment(3, 3);
        b.finalize_minmax();

        a.merge(&b);
        assert_eq!(a.lloc(), 2);
        assert_eq!(a.space_count, 2);
        assert_eq!(a.cloc(), 1);
    }
}
