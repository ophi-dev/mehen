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
/// (whitespace, newlines). `trivia_bearing_token_types` is the set of
/// operator tokens whose lexer rules fold optional comments into the token
/// text (e.g. Kotlin's `EXCL_WS`/`NOT_IS`/`NOT_IN`/`QUEST_WS`/`AS_*`); their
/// text is scanned for embedded comments. Every other token is treated as
/// code (its start row is a PLOC line). The end-of-file token (`< 0`) is
/// skipped.
///
/// Restricting the embedded-comment scan to `trivia_bearing_token_types`
/// (rather than every token) avoids false positives from `//` or `/*` that
/// appear inside string-literal text tokens (e.g. a URL `"http://x"`).
///
/// Byte ranges come from the runtime's UTF-8 token spans, so routing stays
/// correct for non-ASCII source.
pub fn loc_tokens(
    tokens: &[CommonToken],
    comment_token_types: &[i32],
    skip_token_types: &[i32],
    trivia_bearing_token_types: &[i32],
) -> Vec<LocToken> {
    let mut out = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let tt = tok.token_type();
        if tt < 0 || skip_token_types.contains(&tt) {
            continue;
        }
        let start_byte = mehen_core::byte_offset_clamped(tok.start_byte());
        let end_byte = mehen_core::byte_offset_clamped(tok.stop_byte()).max(start_byte);
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
            // Some lexers fold optional trivia into operator tokens — e.g.
            // Kotlin's `EXCL_WS: '!' Hidden`, `NOT_IS: '!is' (Hidden|NL)` —
            // so a comment glued to the operator (`!is/* c */`) is part of
            // the token text rather than a standalone comment token. Recover
            // those as comments so CLOC isn't undercounted, using the same
            // byte span and row offsets the embedded comment occupies. Only
            // the declared trivia-bearing operator tokens are scanned, so a
            // `//` or `/*` inside string-literal text is never misread.
            if trivia_bearing_token_types.contains(&tt)
                && let Some(text) = tok.text()
            {
                emit_embedded_comments(text, start_byte, start_row, &mut out);
            }
        }
    }
    out
}

/// Scan an operator-token's `text` for embedded `/* … */` or `// …` comment
/// runs and push a [`LocTokenKind::Comment`] for each, with rows offset from
/// the token's `start_row` and byte span offset from the token's
/// `token_start_byte`. Handles multi-line block comments.
fn emit_embedded_comments(
    text: &str,
    token_start_byte: u32,
    token_start_row: u32,
    out: &mut Vec<LocToken>,
) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    // Newlines before the current scan position → row offset within the token.
    let mut row_offset = 0u32;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                let start_row = token_start_row + row_offset;
                let comment_start = i;
                i += 2;
                let mut inner_newlines = 0u32;
                // Find the closing `*/`, counting newlines for the end row.
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    if bytes[i] == b'\n' {
                        inner_newlines += 1;
                    }
                    i += 1;
                }
                i += 2; // consume `*/` (or run off the end on an unclosed comment)
                let end = i.min(bytes.len());
                out.push(LocToken {
                    kind: LocTokenKind::Comment,
                    start_byte: token_start_byte + comment_start as u32,
                    end_byte: token_start_byte + end as u32,
                    start_row,
                    end_row: start_row + inner_newlines,
                });
                row_offset += inner_newlines;
            }
            (b'/', b'/') => {
                // Line comment runs to the next newline (or token end).
                let start_row = token_start_row + row_offset;
                let comment_start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push(LocToken {
                    kind: LocTokenKind::Comment,
                    start_byte: token_start_byte + comment_start as u32,
                    end_byte: token_start_byte + i as u32,
                    start_row,
                    end_row: start_row,
                });
            }
            (b'\n', _) => {
                row_offset += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
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
        let tokens = vec![
            tok(2, 1, 0, 3, "code"), // code
            tok(1, 1, 5, 8, "// a"), // comment (type 1)
            tok(2, 2, 10, 10, "x"),  // code
        ];
        let locs = loc_tokens(&tokens, &[1], &[], &[]);
        assert_eq!(locs.len(), 3);
        assert_eq!(locs[0].kind, LocTokenKind::Code);
        assert_eq!(locs[1].kind, LocTokenKind::Comment);
        assert_eq!(locs[1].start_row, 0);
        assert_eq!(locs[2].kind, LocTokenKind::Code);
        assert_eq!(locs[2].start_row, 1);
    }

    #[test]
    fn multiline_comment_spans_rows_and_skips_whitespace() {
        let tokens = vec![
            tok(1, 1, 0, 10, "/* a\nb\nc */"), // 3-line comment
            tok(99, 3, 11, 11, " "),           // whitespace (skipped)
        ];
        let locs = loc_tokens(&tokens, &[1], &[99], &[]);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].kind, LocTokenKind::Comment);
        assert_eq!(locs[0].start_row, 0);
        assert_eq!(locs[0].end_row, 2);
    }

    #[test]
    fn eof_token_is_skipped() {
        let tokens = vec![tok(-1, 1, 0, 0, "<EOF>")];
        assert!(loc_tokens(&tokens, &[], &[], &[]).is_empty());
    }

    #[test]
    fn recovers_comment_embedded_in_trivia_bearing_operator() {
        // Operator token type 105 (`!is`) with a glued comment: `!is/* c */`.
        // Source: `a !is/* c */ B` — operator token spans chars 2..=10.
        let tokens = vec![
            tok(7, 1, 0, 0, "a"),             // identifier (code)
            tok(105, 1, 2, 10, "!is/* c */"), // NOT_IS with embedded comment
            tok(7, 1, 13, 13, "B"),           // identifier (code)
        ];
        // 105 is declared trivia-bearing → its embedded `/* c */` is recovered.
        let locs = loc_tokens(&tokens, &[2], &[], &[105]);
        let comments: Vec<_> = locs
            .iter()
            .filter(|t| t.kind == LocTokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1, "embedded comment must be recovered");
        assert_eq!(comments[0].start_row, 0);
    }

    #[test]
    fn does_not_scan_non_trivia_tokens_for_comments() {
        // A string-literal text token containing `//` (e.g. a URL) must NOT
        // be misread as a comment — only declared trivia-bearing tokens are
        // scanned. Token type 7 is not in the trivia-bearing set.
        let tokens = vec![tok(7, 1, 0, 9, "\"http://x\"")];
        let locs = loc_tokens(&tokens, &[2], &[], &[105]);
        assert!(
            locs.iter().all(|t| t.kind == LocTokenKind::Code),
            "the // inside a string literal must not become a comment"
        );
    }
}
