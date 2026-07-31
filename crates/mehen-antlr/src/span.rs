// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Span and position conversion between the ANTLR runtime and mehen.
//!
//! `antlr-rust-runtime` exposes token positions in both its native Unicode
//! scalar index space (`Token::start`/`Token::stop`) and UTF-8 byte space
//! (`Token::start_byte`/`Token::stop_byte`). mehen uses UTF-8 byte offsets
//! throughout, so this module lifts ANTLR rule token ranges into byte/line
//! [`SourceSpan`](mehen_core::SourceSpan)s.
//!
//! Since the 0.11 runtime rewrite the concrete syntax tree is a flat arena
//! addressed by [`NodeId`](antlr4_runtime::NodeId) and traversed through
//! borrowing views. A rule's covered token range is read from a
//! [`RuleNodeView`], whose `start`/`stop` accessors return [`TokenView`]s
//! directly — the view already carries the shared [`TokenStore`], so no token
//! store has to be threaded through here.

use antlr4_runtime::RuleNodeView;
use antlr4_runtime::token::{TOKEN_EOF, Token};
use mehen_core::{LineIndex, SourceSpan, byte_offset_clamped};

/// Lift an ANTLR token span into a byte- and line-resolved [`SourceSpan`].
///
/// Generic over [`Token`] so it works with both the parser-owned
/// [`TokenView`](antlr4_runtime::TokenView) and any test double.
pub fn span_from_tokens(
    start_token: &impl Token,
    stop_token: &impl Token,
    line_index: &LineIndex,
    source_len: usize,
) -> SourceSpan {
    // Byte offsets are optional since the 0.23 runtime: a token source that
    // cannot resolve them reports `None`. mehen's streams always can, so this is
    // defensive — an unresolvable start yields an empty span rather than a
    // fabricated one, keeping every downstream metric span truthful.
    let Some(start) = start_token.start_byte() else {
        return SourceSpan::empty();
    };
    let start_byte = byte_offset_clamped(start);
    let stop_byte = if stop_token.token_type() == TOKEN_EOF {
        Some(source_len)
    } else {
        stop_token.stop_byte()
    };
    let end_byte = stop_byte.map_or(start_byte, |stop| byte_offset_clamped(stop).max(start_byte));
    SourceSpan {
        start_byte,
        end_byte,
        start_line: line_index.line_at(start_byte),
        end_line: line_index.line_at(end_byte.saturating_sub(1).max(start_byte)),
    }
}

/// Lift a rule node's covered token range into a [`SourceSpan`].
///
/// Reads the rule's `start`/`stop` tokens and maps their runtime-provided byte
/// span to mehen's byte/line coordinates. A rule covering no tokens (an empty
/// optional rule) yields [`SourceSpan::empty`].
pub fn ctx_span(rule: RuleNodeView<'_>, line_index: &LineIndex, source_len: usize) -> SourceSpan {
    match rule.start() {
        Some(start_tok) => {
            let stop_tok = rule.stop().unwrap_or(start_tok);
            span_from_tokens(&start_tok, &stop_tok, line_index, source_len)
        }
        None => SourceSpan::empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antlr4_runtime::token::{TOKEN_EOF, TokenId};

    /// Minimal [`Token`] test double. The 0.11 runtime rewrite made real
    /// tokens live only inside the parser-owned `TokenStore` (no public
    /// builder), but `span_from_tokens` is generic over [`Token`], so its
    /// byte-math contract is exercised through a local impl instead.
    #[derive(Debug)]
    struct FakeToken {
        token_type: i32,
        start_byte: usize,
        stop_byte: usize,
    }

    impl Token for FakeToken {
        fn token_id(&self) -> TokenId {
            TokenId::try_from(0usize).expect("0 is a valid token id")
        }
        fn token_type(&self) -> i32 {
            self.token_type
        }
        fn channel(&self) -> i32 {
            0
        }
        fn start(&self) -> usize {
            self.start_byte
        }
        fn stop(&self) -> usize {
            self.stop_byte
        }
        fn line(&self) -> usize {
            1
        }
        fn column(&self) -> usize {
            0
        }
        fn text(&self) -> Option<&str> {
            None
        }
        fn source_name(&self) -> &str {
            "<test>"
        }
        fn start_byte(&self) -> Option<usize> {
            Some(self.start_byte)
        }
        fn stop_byte(&self) -> Option<usize> {
            Some(self.stop_byte)
        }
    }

    #[test]
    fn span_uses_runtime_byte_bounds() {
        let src = "fun f() {}\nclass C\n";
        let li = LineIndex::new(src);
        // A token covering `class C` on line 2 (bytes 11..=17, exclusive 18).
        let start = FakeToken {
            token_type: 1,
            start_byte: 11,
            stop_byte: 16,
        };
        let stop = FakeToken {
            token_type: 1,
            start_byte: 17,
            stop_byte: 18,
        };
        let span = span_from_tokens(&start, &stop, &li, src.len());
        assert_eq!(span.start_byte, 11);
        assert_eq!(span.end_byte, 18);
        assert_eq!(span.start_line, 2);
    }

    #[test]
    fn eof_stop_uses_source_byte_len() {
        let src = "é\nx";
        let li = LineIndex::new(src);
        // `é` is 2 bytes; the EOF stop token forces the span end to the full
        // source byte length rather than the EOF token's own byte offset.
        let start = FakeToken {
            token_type: 1,
            start_byte: 0,
            stop_byte: 2,
        };
        let eof = FakeToken {
            token_type: TOKEN_EOF,
            start_byte: 0,
            stop_byte: 0,
        };

        let span = span_from_tokens(&start, &eof, &li, src.len());

        assert_eq!(span.start_byte, 0);
        assert_eq!(span.end_byte, 4);
        assert_eq!(span.end_line, 2);
    }
}
