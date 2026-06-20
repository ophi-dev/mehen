// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Comment-line (CLOC) extraction from the ANTLR token stream.
//!
//! ANTLR lexers route comments and whitespace to a hidden channel, so they
//! never appear in the parse tree. The buffered [`CommonTokenStream`] still
//! retains them, which is where CLOC comes from. This module turns a list
//! of comment tokens into `(start_row, end_row)` row pairs (0-based, to
//! match the LOC accumulator's row convention) that the analyzer feeds into
//! the unit space's `mehen_metrics::LocStats::observe_comment`.
//!
//! Why CLOC lands only on the unit space: a hidden-channel token carries no
//! enclosing-rule context, so attributing a comment to a specific
//! function/class space would require a separate range-overlap pass. The
//! pre-1.0 LOC semantics already roll child comment counts up to the unit,
//! and CLOC at the file level is what the maintainability index and the
//! `loc.cloc` metric report. Per-space CLOC attribution can be layered on
//! later via `mehen_metrics::SpaceRangeTracker` if a metric needs it.

use antlr4_runtime::token::{CommonToken, Token};

/// A comment occupying source rows `[start_row, end_row]` (inclusive,
/// 0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommentRows {
    pub start_row: u32,
    pub end_row: u32,
}

/// Scan `tokens` for comment tokens and return their row spans in source
/// order.
///
/// `comment_token_types` is the set of lexer token types that denote a
/// comment (e.g. Kotlin's `LineComment`, `DelimitedComment`,
/// `Inside_Comment`). Tokens whose type is not in the set are ignored, so
/// callers pass only their language's comment kinds and whitespace/newline
/// tokens are skipped.
///
/// Multi-line delimited comments report `end_row > start_row`, matching
/// what `LocStats::observe_comment` expects for the
/// only-comment-vs-code-comment classification.
pub fn comment_rows(tokens: &[CommonToken], comment_token_types: &[i32]) -> Vec<CommentRows> {
    let mut out = Vec::new();
    for tok in tokens {
        if !comment_token_types.contains(&tok.token_type()) {
            continue;
        }
        let start_row = (tok.line() as u32).saturating_sub(1);
        // A delimited comment's text may span multiple lines; count the
        // newlines in its text to find the end row. Line comments have no
        // embedded newline, so `end_row == start_row`.
        let extra_lines = tok
            .text()
            .map(|t| t.bytes().filter(|&b| b == b'\n').count() as u32)
            .unwrap_or(0);
        out.push(CommentRows {
            start_row,
            end_row: start_row.saturating_add(extra_lines),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(token_type: i32, line: usize, text: &str) -> CommonToken {
        CommonToken::new(token_type)
            .with_text(text)
            .with_position(line, 0)
    }

    #[test]
    fn picks_only_comment_token_types() {
        let tokens = vec![
            tok(1, 1, "// a"), // comment
            tok(2, 2, "code"), // not a comment
            tok(1, 3, "// b"), // comment
        ];
        let rows = comment_rows(&tokens, &[1]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            CommentRows {
                start_row: 0,
                end_row: 0
            }
        );
        assert_eq!(
            rows[1],
            CommentRows {
                start_row: 2,
                end_row: 2
            }
        );
    }

    #[test]
    fn multiline_comment_spans_rows() {
        let tokens = vec![tok(3, 4, "/* line1\nline2\nline3 */")];
        let rows = comment_rows(&tokens, &[3]);
        assert_eq!(
            rows[0],
            CommentRows {
                start_row: 3,
                end_row: 5
            }
        );
    }
}
