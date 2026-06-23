// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC / size metrics (research foundation §6.1).
//!
//! Line classification is comment-aware: every line touched by a sqruff
//! comment token (`-- …`, `/* … */`, dialect comments) is a comment line; a
//! line with code tokens is a code line; otherwise it is blank. A line that
//! holds both code and a trailing comment counts as code (code dominates) and
//! also as a comment, matching the research foundation's overlapping
//! definitions where `comment + code` can exceed physical lines.

use std::collections::BTreeSet;

use sqruff_lib_core::dialects::syntax::{SyntaxKind, SyntaxSet};
use sqruff_lib_core::parser::segments::ErasedSegment;

/// LOC family values for one `.sql` file.
#[derive(Clone, Debug, Default)]
pub(crate) struct SqlLoc {
    pub physical: u32,
    pub code: u32,
    pub comment: u32,
    pub blank: u32,
    /// Logical statement count (AST statements, not `;` count).
    pub logical: u32,
    pub max_statement_lines: u32,
    pub avg_statement_lines: f64,
}

impl SqlLoc {
    pub(crate) fn comment_density(&self) -> f64 {
        let denom = (self.code + self.comment).max(1);
        self.comment as f64 / denom as f64
    }
}

const COMMENT_KINDS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::Comment,
    SyntaxKind::InlineComment,
    SyntaxKind::BlockComment,
]);

/// Compute LOC family values.
///
/// `text` is the original source; `root` is the parsed tree (used to mark
/// comment lines precisely); `statement_spans` are the 1-based inclusive line
/// ranges of top-level statements (for max/avg statement length).
pub(crate) fn compute(
    text: &str,
    root: &ErasedSegment,
    line_at: impl Fn(u32) -> u32,
    statement_spans: &[(u32, u32)],
) -> SqlLoc {
    let mut loc = SqlLoc::default();

    // Physical lines: count newlines + 1 for a non-empty trailing segment.
    let physical = if text.is_empty() {
        0
    } else {
        let nl = text.bytes().filter(|b| *b == b'\n').count() as u32;
        // A trailing newline means the last line is empty and already counted.
        if text.ends_with('\n') { nl } else { nl + 1 }
    };
    loc.physical = physical;

    // Lines touched by comment tokens.
    let mut comment_lines: BTreeSet<u32> = BTreeSet::new();
    let comments = root.recursive_crawl(&COMMENT_KINDS, true, &SyntaxSet::EMPTY, true);
    for c in &comments {
        if let Some(pm) = c.get_position_marker() {
            let start = line_at(pm.source_slice.start as u32);
            let end = line_at(pm.source_slice.end.saturating_sub(1) as u32);
            for l in start..=end {
                comment_lines.insert(l);
            }
        }
    }

    // Classify each physical line.
    let mut blank = 0u32;
    let mut comment = 0u32;
    let mut code = 0u32;
    for (idx, line) in text.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let trimmed = line.trim();
        let is_comment_line = comment_lines.contains(&line_no);
        if trimmed.is_empty() {
            blank += 1;
        } else if is_comment_line && !line_has_code_outside_comment(line) {
            comment += 1;
        } else {
            code += 1;
            // A code line that also carries a comment counts toward comment too.
            if is_comment_line {
                comment += 1;
            }
        }
    }
    loc.blank = blank;
    loc.comment = comment;
    loc.code = code;

    // Logical statements + statement line stats.
    loc.logical = statement_spans.len() as u32;
    let mut max_lines = 0u32;
    let mut total_lines = 0u64;
    for (start, end) in statement_spans {
        let lines = end.saturating_sub(*start) + 1;
        max_lines = max_lines.max(lines);
        total_lines += lines as u64;
    }
    loc.max_statement_lines = max_lines;
    loc.avg_statement_lines = if statement_spans.is_empty() {
        0.0
    } else {
        total_lines as f64 / statement_spans.len() as f64
    };

    loc
}

/// Whether a physical line contains code outside of its comment portion.
/// Used to keep `code -- trailing comment` classified as a code line.
fn line_has_code_outside_comment(line: &str) -> bool {
    // Strip a trailing `-- …` line comment, then check for remaining tokens.
    let before_line_comment = match line.find("--") {
        Some(idx) => &line[..idx],
        None => line,
    };
    // Strip a `/* … */` block comment occurrence on this line (best-effort:
    // single-line blocks). Multi-line blocks are already covered because each
    // of their lines is in `comment_lines` and we only call this for lines
    // that also could hold code; the residual after removing one block is a
    // good-enough signal for the overlapping count.
    let residual = strip_inline_block_comment(before_line_comment);
    !residual.trim().is_empty()
}

fn strip_inline_block_comment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("/*") {
        out.push_str(&rest[..open]);
        if let Some(close) = rest[open + 2..].find("*/") {
            rest = &rest[open + 2 + close + 2..];
        } else {
            // Unterminated on this line — drop the remainder.
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailing_line_comment() {
        assert!(line_has_code_outside_comment("SELECT 1 -- hi"));
        assert!(!line_has_code_outside_comment("-- just a comment"));
        assert!(!line_has_code_outside_comment("   -- indented comment"));
    }

    #[test]
    fn strips_inline_block_comment() {
        assert!(line_has_code_outside_comment("SELECT /* x */ 1"));
        assert!(!line_has_code_outside_comment("/* whole line */"));
    }
}
