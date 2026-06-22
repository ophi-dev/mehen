// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Span and position conversion between the ANTLR runtime and mehen.
//!
//! `antlr-rust-runtime` exposes token positions in both its native Unicode
//! scalar index space (`Token::start`/`Token::stop`) and UTF-8 byte space
//! (`Token::start_byte`/`Token::stop_byte`). mehen uses UTF-8 byte offsets
//! throughout, so this module lifts ANTLR context token ranges into byte/line
//! [`SourceSpan`](mehen_core::SourceSpan)s.

use antlr4_runtime::ParserRuleContext;
use antlr4_runtime::token::{TOKEN_EOF, Token};
use mehen_core::{LineIndex, SourceSpan, byte_offset_clamped};

/// Lift an ANTLR token span into a byte- and line-resolved [`SourceSpan`].
pub fn span_from_tokens(
    start_token: &impl Token,
    stop_token: &impl Token,
    line_index: &LineIndex,
    source_len: usize,
) -> SourceSpan {
    let start_byte = byte_offset_clamped(start_token.start_byte());
    let stop_byte = if stop_token.token_type() == TOKEN_EOF {
        source_len
    } else {
        stop_token.stop_byte()
    };
    let end_byte = byte_offset_clamped(stop_byte).max(start_byte);
    SourceSpan {
        start_byte,
        end_byte,
        start_line: line_index.line_at(start_byte),
        end_line: line_index.line_at(end_byte.saturating_sub(1).max(start_byte)),
    }
}

/// Lift a rule context's covered token range into a [`SourceSpan`].
///
/// Reads the context's `start`/`stop` tokens and maps their runtime-provided
/// byte span to mehen's byte/line coordinates. A context covering no tokens
/// (an empty optional rule) yields [`SourceSpan::empty`].
pub fn ctx_span(ctx: &ParserRuleContext, line_index: &LineIndex, source_len: usize) -> SourceSpan {
    match ctx.start() {
        Some(start_tok) => {
            let stop_tok = ctx.stop().unwrap_or(start_tok);
            span_from_tokens(start_tok, stop_tok, line_index, source_len)
        }
        None => SourceSpan::empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antlr4_runtime::token::CommonToken;
    use std::rc::Rc;

    #[test]
    fn span_uses_runtime_byte_bounds() {
        let src = "fun f() {}\nclass C\n";
        let li = LineIndex::new(src);
        let source = Rc::from(src);
        let start =
            CommonToken::new(1)
                .with_span(11, 15)
                .with_source_text(Rc::clone(&source), 11, 16);
        let stop =
            CommonToken::new(1)
                .with_span(17, 17)
                .with_source_text(Rc::clone(&source), 17, 18);
        let span = span_from_tokens(&start, &stop, &li, src.len());
        assert_eq!(span.start_byte, 11);
        assert_eq!(span.end_byte, 18);
        assert_eq!(span.start_line, 2);
    }

    #[test]
    fn eof_stop_uses_source_byte_len() {
        let src = "é\nx";
        let li = LineIndex::new(src);
        let source: Rc<str> = Rc::from(src);
        let start = CommonToken::new(1)
            .with_span(0, 0)
            .with_source_text(Rc::clone(&source), 0, 2);
        let eof = CommonToken::eof("", src.chars().count(), 2, 1);

        let span = span_from_tokens(&start, &eof, &li, src.len());

        assert_eq!(span.start_byte, 0);
        assert_eq!(span.end_byte, 4);
        assert_eq!(span.end_line, 2);
    }
}
