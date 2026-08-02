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
//!
//! Since the 0.11 runtime rewrite there is no owned `CommonToken`: tokens live
//! once in the parser-owned [`TokenStore`](antlr4_runtime::TokenStore) and are
//! read through borrowing [`TokenView`]s. The LOC sweep therefore takes an
//! iterator of `TokenView` — e.g. `CommonTokenStream::tokens()` — instead of a
//! `&[CommonToken]` slice.

use antlr4_runtime::token::{Token, TokenView};

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
///
/// `tokens` is any source-ordered iterator of [`TokenView`]s over the full
/// (hidden-channel-inclusive) token stream. The owned
/// [`ParsedFile`](antlr4_runtime::ParsedFile)'s token store satisfies this
/// directly — `&TokenStore` is `IntoIterator` since the 0.15 runtime (issue
/// #123) — so callers pass `parsed.tokens()`; a live
/// [`CommonTokenStream::tokens`](antlr4_runtime::CommonTokenStream::tokens)
/// works too.
pub fn loc_tokens<'a>(
    tokens: impl IntoIterator<Item = TokenView<'a>>,
    comment_token_types: &[i32],
    skip_token_types: &[i32],
    trivia_bearing_token_types: &[i32],
    line_index: &mehen_core::LineIndex,
) -> Vec<LocToken> {
    let tokens = tokens.into_iter();
    let mut out = Vec::with_capacity(tokens.size_hint().0);
    for tok in tokens {
        // Since the 0.15 runtime `TokenView::text()` returns `Option<&str>`
        // (aligned with the `Token` trait); `text_or_empty()` is the runtime's
        // own convenience for the "empty when absent" behavior the LOC
        // classifier wants (a token with no recorded text contributes no
        // embedded newlines). Everything past this point works on plain
        // fields, so the classification is unit-testable without a
        // `TokenStore`.
        // Byte offsets are optional since the 0.23 runtime (a token source that
        // cannot resolve them reports `None`). A token with no position cannot
        // be attributed to a source row, so it contributes no LOC — the same
        // treatment absent text already gets above.
        let (Some(start_byte), Some(stop_byte)) = (tok.start_byte(), tok.stop_byte()) else {
            continue;
        };
        // BOTH rows come from `LineIndex`, not from `tok.line()` and not from counting
        // terminators in the token text.
        //
        // The start row, because the runtime's lexer advances its line counter on `\n`
        // alone while `LineIndex` may count more — so after any other terminator the
        // token's own line is short, and a comment gets routed onto the preceding code
        // row with its real row falling out as a phantom blank.
        //
        // The end row, because *which characters break a row is the caller's policy*.
        // This used to count the five C# terminators inline, which was wrong for every
        // caller that does not share that policy: Java and Kotlin pass
        // `LineIndex::new` (LF/CRLF only), so `/*a<U+2028>b*/` was counted as two
        // comment rows against a one-row file — CLOC 2 > SLOC 1, which also skews
        // `blank = sloc - ploc - only_comment` and every MI variant downstream. Asking
        // the index resolves it for free and leaves the terminator set knowledge in one
        // place.
        let start_row = line_index.line_at(mehen_core::byte_offset_clamped(start_byte));
        let end_row = line_index.line_at(mehen_core::byte_offset_clamped(stop_byte));
        push_loc_token(
            tok.token_type(),
            start_byte,
            stop_byte,
            start_row,
            end_row.max(start_row),
            tok.text_or_empty(),
            comment_token_types,
            skip_token_types,
            trivia_bearing_token_types,
            &mut out,
        );
    }
    out
}

/// Classify a single token's plain fields into zero or more [`LocToken`]s,
/// pushing them onto `out`. Split out of [`loc_tokens`] so the classification
/// is exercised by unit tests without constructing runtime
/// [`TokenView`]s (which the 0.11 rewrite made un-buildable outside a real
/// `TokenStore`). `text` is the token's UTF-8 text (empty when absent).
///
/// `start_line` and `end_line` are 1-based rows already resolved through the
/// caller's [`LineIndex`](mehen_core::LineIndex) — this function deliberately knows
/// nothing about which characters break a row, since that is per-language policy.
#[allow(clippy::too_many_arguments)]
fn push_loc_token(
    tt: i32,
    start_byte: usize,
    stop_byte: usize,
    start_line: u32,
    end_line: u32,
    text: &str,
    comment_token_types: &[i32],
    skip_token_types: &[i32],
    trivia_bearing_token_types: &[i32],
    out: &mut Vec<LocToken>,
) {
    if tt < 0 || skip_token_types.contains(&tt) {
        return;
    }
    let start_byte = mehen_core::byte_offset_clamped(start_byte);
    let end_byte = mehen_core::byte_offset_clamped(stop_byte).max(start_byte);
    let start_row = start_line.saturating_sub(1);
    if comment_token_types.contains(&tt) {
        // A delimited comment's text may span multiple rows, so its end row comes from
        // the end byte. A line comment's two rows coincide, which needs no special
        // case.
        out.push(LocToken {
            kind: LocTokenKind::Comment,
            start_byte,
            end_byte,
            start_row,
            end_row: end_line.saturating_sub(1).max(start_row),
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
        if trivia_bearing_token_types.contains(&tt) {
            emit_embedded_comments(text, start_byte, start_row, out);
        }
    }
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

    /// One fake token's fields, mirroring what a [`TokenView`] would expose.
    /// The 0.11 runtime rewrite made real tokens un-buildable outside a
    /// parser-owned `TokenStore`, so the classification is exercised through
    /// [`push_loc_token`] on plain fields instead.
    /// `line` / `end_line` are the 1-based rows the caller resolves through its
    /// [`LineIndex`](mehen_core::LineIndex) — `push_loc_token` no longer derives the end
    /// row from the text, because which characters break a row is per-language policy.
    struct Tok {
        tt: i32,
        line: usize,
        end_line: usize,
        start: usize,
        stop: usize,
        text: &'static str,
    }

    /// A token occupying one row.
    const fn tok(tt: i32, line: usize, start: usize, stop: usize, text: &'static str) -> Tok {
        Tok {
            tt,
            line,
            end_line: line,
            start,
            stop,
            text,
        }
    }

    /// A token spanning `line..=end_line`, as `LineIndex` would resolve it.
    const fn spanning_tok(
        tt: i32,
        line: usize,
        end_line: usize,
        start: usize,
        stop: usize,
        text: &'static str,
    ) -> Tok {
        Tok {
            tt,
            line,
            end_line,
            start,
            stop,
            text,
        }
    }

    /// Run the LOC classification over a fake token list, mirroring what
    /// [`loc_tokens`] does per [`TokenView`].
    fn classify(tokens: &[Tok], comment: &[i32], skip: &[i32], trivia: &[i32]) -> Vec<LocToken> {
        let mut out = Vec::new();
        for t in tokens {
            push_loc_token(
                t.tt,
                t.start,
                t.stop,
                t.line as u32,
                t.end_line as u32,
                t.text,
                comment,
                skip,
                trivia,
                &mut out,
            );
        }
        out
    }

    #[test]
    fn classifies_code_and_comment_in_source_order() {
        // `code // a` then a code token on the next line.
        let tokens = [
            tok(2, 1, 0, 3, "code"), // code
            tok(1, 1, 5, 8, "// a"), // comment (type 1)
            tok(2, 2, 10, 10, "x"),  // code
        ];
        let locs = classify(&tokens, &[1], &[], &[]);
        assert_eq!(locs.len(), 3);
        assert_eq!(locs[0].kind, LocTokenKind::Code);
        assert_eq!(locs[1].kind, LocTokenKind::Comment);
        assert_eq!(locs[1].start_row, 0);
        assert_eq!(locs[2].kind, LocTokenKind::Code);
        assert_eq!(locs[2].start_row, 1);
    }

    #[test]
    fn multiline_comment_spans_rows_and_skips_whitespace() {
        let tokens = [
            spanning_tok(1, 1, 3, 0, 10, "/* a\nb\nc */"), // 3-line comment
            tok(99, 3, 11, 11, " "),                       // whitespace (skipped)
        ];
        let locs = classify(&tokens, &[1], &[99], &[]);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].kind, LocTokenKind::Comment);
        assert_eq!(locs[0].start_row, 0);
        assert_eq!(locs[0].end_row, 2);
    }

    #[test]
    fn a_comments_end_row_comes_from_the_caller_not_from_its_text() {
        // REGRESSION. This counted the five C# line terminators inline, which was wrong
        // for every caller not sharing that policy: Java and Kotlin pass
        // `LineIndex::new` (LF/CRLF only), so `/*a<U+2028>b*/` was reported as two
        // comment rows in a one-row file — CLOC 2 against SLOC 1, which also skews
        // `blank = sloc - ploc - only_comment` and every MI variant downstream.
        //
        // The text here HAS a U+2028 and the caller says one row, which is what a
        // LF/CRLF-only index resolves. The end row must follow the caller.
        let one_row = classify(&[tok(1, 1, 0, 9, "/*a\u{2028}b*/")], &[1], &[], &[]);
        assert_eq!(one_row[0].end_row, 0, "the caller's index says one row");

        // Same text, a caller whose policy DOES split on U+2028 (C#'s does).
        let two_rows = classify(
            &[spanning_tok(1, 1, 2, 0, 9, "/*a\u{2028}b*/")],
            &[1],
            &[],
            &[],
        );
        assert_eq!(two_rows[0].end_row, 1);
    }

    #[test]
    fn an_end_row_never_precedes_the_start_row() {
        // A synthesized or zero-width token can resolve both ends to the same byte, and
        // a caller could in principle hand back an inverted pair; clamping keeps the
        // range well-formed rather than underflowing the row subtraction.
        let locs = classify(&[spanning_tok(1, 3, 1, 5, 5, "//x")], &[1], &[], &[]);
        assert_eq!(locs[0].start_row, 2);
        assert_eq!(locs[0].end_row, 2);
    }

    #[test]
    fn eof_token_is_skipped() {
        let tokens = [tok(-1, 1, 0, 0, "<EOF>")];
        assert!(classify(&tokens, &[], &[], &[]).is_empty());
    }

    #[test]
    fn recovers_comment_embedded_in_trivia_bearing_operator() {
        // Operator token type 105 (`!is`) with a glued comment: `!is/* c */`.
        // Source: `a !is/* c */ B` — operator token spans chars 2..=10.
        let tokens = [
            tok(7, 1, 0, 0, "a"),             // identifier (code)
            tok(105, 1, 2, 10, "!is/* c */"), // NOT_IS with embedded comment
            tok(7, 1, 13, 13, "B"),           // identifier (code)
        ];
        // 105 is declared trivia-bearing → its embedded `/* c */` is recovered.
        let locs = classify(&tokens, &[2], &[], &[105]);
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
        let tokens = [tok(7, 1, 0, 9, "\"http://x\"")];
        let locs = classify(&tokens, &[2], &[], &[105]);
        assert!(
            locs.iter().all(|t| t.kind == LocTokenKind::Code),
            "the // inside a string literal must not become a comment"
        );
    }
}
