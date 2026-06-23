// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC / size metrics (research foundation §6.1).
//!
//! Line classification is comment-aware and *byte-span based*: a line is code
//! when it has any non-whitespace byte outside every sqruff comment span, a
//! comment when it is non-blank but fully covered by comment spans, and blank
//! otherwise. A line that holds both code and a comment (e.g. `SELECT 1 -- x`,
//! or a line that opens/closes a block comment beside code) counts as code
//! *and* comment, matching the research foundation's overlapping definitions
//! where `comment + code` can exceed physical lines. Using the parser's
//! authoritative comment byte ranges (rather than scanning text for `--` /
//! `/* */` markers) correctly classifies interior lines of multi-line block
//! comments, which carry no marker of their own.

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

    // Comment byte ranges, straight from the parser. Used to classify lines by
    // *byte coverage* rather than re-scanning text for `--`/`/* */` markers:
    // an interior line of a multi-line block comment carries no marker but is
    // fully inside a comment span, so a marker heuristic would wrongly count it
    // as code (Codex P2).
    let mut comment_spans: Vec<(u32, u32)> = Vec::new();
    let mut comment_lines: BTreeSet<u32> = BTreeSet::new();
    let comments = root.recursive_crawl(&COMMENT_KINDS, true, &SyntaxSet::EMPTY, true);
    for c in &comments {
        if let Some(pm) = c.get_position_marker() {
            let s = pm.source_slice.start as u32;
            let e = pm.source_slice.end as u32;
            comment_spans.push((s, e));
            let start_line = line_at(s);
            let end_line = line_at(e.saturating_sub(1));
            for l in start_line..=end_line {
                comment_lines.insert(l);
            }
        }
    }

    // Classify each physical line by whether it has any non-whitespace byte
    // *outside* every comment span.
    let mut blank = 0u32;
    let mut comment = 0u32;
    let mut code = 0u32;
    let mut line_start = 0usize; // byte offset of the current line's start
    for (idx, line) in text.split_inclusive('\n').enumerate() {
        let line_no = (idx + 1) as u32;
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        let is_comment_line = comment_lines.contains(&line_no);
        if content.trim().is_empty() {
            blank += 1;
        } else if line_has_code_outside_comments(content, line_start, &comment_spans) {
            code += 1;
            // A code line that also carries a comment counts toward comment too.
            if is_comment_line {
                comment += 1;
            }
        } else {
            // Non-blank, but every non-whitespace byte is inside a comment.
            comment += 1;
        }
        line_start += line.len();
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

/// Whether the line whose content is `content` (starting at byte `line_start`
/// in the source) has any non-whitespace byte that falls *outside* every
/// comment span. This is the span-based code/comment test: a line fully inside
/// a multi-line block comment has all its bytes covered → no code; a line like
/// `SELECT 1 -- note` has code bytes before the comment span → code.
fn line_has_code_outside_comments(
    content: &str,
    line_start: usize,
    comment_spans: &[(u32, u32)],
) -> bool {
    for (i, b) in content.bytes().enumerate() {
        if b.is_ascii_whitespace() {
            continue;
        }
        let abs = (line_start + i) as u32;
        let inside_comment = comment_spans.iter().any(|&(s, e)| abs >= s && abs < e);
        if !inside_comment {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_before_comment_span_is_code() {
        // "SELECT 1 -- note": the `-- note` starts at byte 9.
        let content = "SELECT 1 -- note";
        assert!(line_has_code_outside_comments(content, 0, &[(9, 16)]));
    }

    #[test]
    fn line_fully_inside_comment_span_is_not_code() {
        // A `-- whole line` comment covers the entire line.
        let content = "-- just a comment";
        assert!(!line_has_code_outside_comments(content, 0, &[(0, 17)]));
    }

    #[test]
    fn block_comment_interior_line_has_no_marker_but_is_not_code() {
        // An interior block-comment line carries no `/*`/`*/` marker. With a
        // span fully covering it (offsets 20..50 here), it must NOT count as
        // code even though a marker-scan heuristic would have missed it.
        let content = "   explain why this query exists";
        let start = 20usize;
        let end = start + content.len();
        assert!(!line_has_code_outside_comments(
            content,
            start,
            &[(start as u32, end as u32)]
        ));
    }

    #[test]
    fn code_after_block_comment_close_is_code() {
        // "*/ SELECT 1": the comment span ends mid-line; the SELECT is code.
        let content = "*/ SELECT 1";
        // Comment covers the leading `*/` only (bytes 0..2).
        assert!(line_has_code_outside_comments(content, 0, &[(0, 2)]));
    }
}
