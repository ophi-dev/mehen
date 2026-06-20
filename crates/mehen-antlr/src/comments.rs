// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! LOC token extraction from the ANTLR token stream.
//!
//! ANTLR lexers route comments and whitespace to a hidden channel, so they
//! never appear in the parse tree — but the buffered [`CommonTokenStream`]
//! retains every token in source order. LOC is therefore computed from a
//! single ordered pass over that full token list rather than from the tree:
//! comments and code tokens are observed *interleaved*, so a comment that
//! shares a line with code is correctly classified as a code-comment (not
//! comment-only), and per-space `loc.cloc`/`loc.ploc` reflect the tokens
//! inside each scope's body when routed through
//! `mehen_metrics::SpaceRangeTracker`.
//!
//! This mirrors how the token-driven analyzers (`mehen-rust`, `mehen-python`,
//! `mehen-typescript`) drive LOC: a flat, source-ordered token sweep.

use antlr4_runtime::token::{CommonToken, Token};

use crate::span::CharByteMap;

/// How a token contributes to LOC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocTokenKind {
    /// A code token — its start row is a PLOC line.
    Code,
    /// A comment token — contributes CLOC across `[start_row, end_row]`.
    Comment,
}

/// A source-ordered LOC observation: a code or comment token with the byte
/// range used to route it to the deepest enclosing space, and the 0-based
/// rows it spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocToken {
    pub kind: LocTokenKind,
    /// Byte offsets (UTF-8) for `SpaceRangeTracker` routing.
    pub start_byte: u32,
    pub end_byte: u32,
    /// 0-based start row (matches the LOC accumulator's row convention).
    pub start_row: u32,
    /// 0-based end row (`> start_row` only for multi-line block comments).
    pub end_row: u32,
}

/// Build the source-ordered LOC token list from the buffered token stream.
///
/// `comment_token_types` is the set of lexer token types that denote a
/// comment. `skip_token_types` is the set that contributes nothing to LOC
/// (whitespace, newlines). Every other token is treated as code (its start
/// row is a PLOC line). The end-of-file token (type `< 0`) is skipped.
///
/// Byte ranges are derived from each token's inclusive char indices via
/// `map`, so routing stays correct for non-ASCII source.
pub fn loc_tokens(
    tokens: &[CommonToken],
    comment_token_types: &[i32],
    skip_token_types: &[i32],
    map: &CharByteMap,
) -> Vec<LocToken> {
    let mut out = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let tt = tok.token_type();
        if tt < 0 || skip_token_types.contains(&tt) {
            continue;
        }
        let start_byte = map.start_byte(tok.start());
        let end_byte = map.end_byte_inclusive(tok.stop()).max(start_byte);
        let start_row = (tok.line() as u32).saturating_sub(1);
        if comment_token_types.contains(&tt) {
            // A delimited comment's text may span multiple lines; count the
            // newlines to find the end row. Line comments have none.
            let extra_lines = tok
                .text()
                .map(|t| t.bytes().filter(|&b| b == b'\n').count() as u32)
                .unwrap_or(0);
            out.push(LocToken {
                kind: LocTokenKind::Comment,
                start_byte,
                end_byte,
                start_row,
                end_row: start_row.saturating_add(extra_lines),
            });
        } else {
            out.push(LocToken {
                kind: LocTokenKind::Code,
                start_byte,
                end_byte,
                start_row,
                end_row: start_row,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(token_type: i32, line: usize, start: usize, stop: usize, text: &str) -> CommonToken {
        CommonToken::new(token_type)
            .with_text(text)
            .with_position(line, 0)
            .with_span(start, stop)
    }

    #[test]
    fn classifies_code_and_comment_in_source_order() {
        // `code // a` then a code token on the next line.
        let src = "code // a\nx";
        let map = CharByteMap::new(src);
        let tokens = vec![
            tok(2, 1, 0, 3, "code"), // code
            tok(1, 1, 5, 8, "// a"), // comment (type 1)
            tok(2, 2, 10, 10, "x"),  // code
        ];
        let locs = loc_tokens(&tokens, &[1], &[], &map);
        assert_eq!(locs.len(), 3);
        assert_eq!(locs[0].kind, LocTokenKind::Code);
        assert_eq!(locs[1].kind, LocTokenKind::Comment);
        assert_eq!(locs[1].start_row, 0);
        assert_eq!(locs[2].kind, LocTokenKind::Code);
        assert_eq!(locs[2].start_row, 1);
    }

    #[test]
    fn multiline_comment_spans_rows_and_skips_whitespace() {
        let src = "/* a\nb\nc */";
        let map = CharByteMap::new(src);
        let tokens = vec![
            tok(1, 1, 0, 10, "/* a\nb\nc */"), // 3-line comment
            tok(99, 3, 11, 11, " "),           // whitespace (skipped)
        ];
        let locs = loc_tokens(&tokens, &[1], &[99], &map);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].kind, LocTokenKind::Comment);
        assert_eq!(locs[0].start_row, 0);
        assert_eq!(locs[0].end_row, 2);
    }

    #[test]
    fn eof_token_is_skipped() {
        let src = "x";
        let map = CharByteMap::new(src);
        let tokens = vec![tok(-1, 1, 0, 0, "<EOF>")];
        assert!(loc_tokens(&tokens, &[], &[], &map).is_empty());
    }
}
