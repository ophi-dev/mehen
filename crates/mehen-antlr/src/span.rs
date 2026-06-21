// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Span and position conversion between the ANTLR runtime and mehen.
//!
//! The ANTLR runtime works in **Unicode scalar (`char`) units**: an
//! `InputStream` is a `Vec<char>`, a token's `start()`/`stop()` are
//! inclusive char indices into that buffer, and `line()`/`column()` are
//! likewise char-based (1-based line, 0-based column). mehen's
//! [`SourceSpan`], by contrast, is expressed in **UTF-8 byte offsets** and
//! 1-based lines, the same as every other backend.
//!
//! For ASCII source the two coincide, but a single non-ASCII character
//! (a Unicode identifier, an emoji in a string literal, a `//` comment in
//! Cyrillic) shifts every later byte offset relative to its char index.
//! To keep spans correct regardless of content, [`CharByteMap`] precomputes
//! a `char index → byte offset` table once per file. Conversion is then a
//! single indexed lookup, and the byte offsets feed mehen's existing
//! byte-based [`LineIndex`].

use antlr4_runtime::ParserRuleContext;
use antlr4_runtime::token::Token;
use mehen_core::{LineIndex, SourceSpan, byte_offset_clamped};

/// Maps ANTLR char (Unicode-scalar) indices to UTF-8 byte offsets.
///
/// Built once from the source text. `offsets[i]` is the byte offset at
/// which the `i`-th `char` begins; a final sentinel entry holds the total
/// byte length so the exclusive end of the last char is representable.
pub struct CharByteMap {
    /// `offsets.len() == char_count + 1`. `offsets[char_count]` is the
    /// total byte length of the source.
    offsets: Vec<u32>,
}

impl CharByteMap {
    /// Build the map for `source`. O(n) over the source's chars.
    pub fn new(source: &str) -> Self {
        let mut offsets = Vec::with_capacity(source.len() + 1);
        for (byte_idx, _) in source.char_indices() {
            offsets.push(byte_offset_clamped(byte_idx));
        }
        // Sentinel: exclusive end of the final char == total byte length.
        offsets.push(byte_offset_clamped(source.len()));
        Self { offsets }
    }

    /// Number of `char`s in the source (the ANTLR index space size).
    #[inline]
    pub fn char_count(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Byte offset where the char at `char_index` begins. Clamps to the
    /// total byte length for out-of-range indices (e.g. a synthetic EOF
    /// token whose `stop()` is `usize::MAX`).
    #[inline]
    pub fn start_byte(&self, char_index: usize) -> u32 {
        let i = char_index.min(self.char_count());
        self.offsets[i]
    }

    /// Exclusive byte offset *after* the char at the inclusive `char_index`.
    /// ANTLR stop indices are inclusive, so the exclusive byte end is the
    /// start of the *next* char.
    #[inline]
    pub fn end_byte_inclusive(&self, char_index: usize) -> u32 {
        // The exclusive end of char `i` is `offsets[i + 1]`.
        let next = char_index.saturating_add(1).min(self.offsets.len() - 1);
        self.offsets[next]
    }
}

/// Lift an ANTLR rule context's `start`/`stop` token positions into a byte-
/// and line-resolved [`SourceSpan`].
///
/// `start_char`/`stop_char` are the inclusive char indices from
/// `ParserRuleContext::start()`/`stop()` (i.e. `Token::start()`/`stop()`).
/// A context with no tokens (empty optional rule) yields an empty span at
/// byte 0, matching how the tree-sitter path treats degenerate nodes.
pub fn span_from_char_range(
    start_char: usize,
    stop_char: usize,
    map: &CharByteMap,
    line_index: &LineIndex,
) -> SourceSpan {
    let start_byte = map.start_byte(start_char);
    let end_byte = map.end_byte_inclusive(stop_char).max(start_byte);
    SourceSpan {
        start_byte,
        end_byte,
        start_line: line_index.line_at(start_byte),
        end_line: line_index.line_at(end_byte.saturating_sub(1).max(start_byte)),
    }
}

/// Lift a rule context's covered token range into a [`SourceSpan`].
///
/// Reads the context's `start`/`stop` tokens (inclusive char indices) and
/// maps them to byte/line coordinates. A context covering no tokens (an
/// empty optional rule) yields [`SourceSpan::empty`].
pub fn ctx_span(ctx: &ParserRuleContext, map: &CharByteMap, line_index: &LineIndex) -> SourceSpan {
    match ctx.start() {
        Some(start_tok) => {
            let start = start_tok.start();
            let stop = ctx.stop().map_or(start, Token::stop);
            span_from_char_range(start, stop, map, line_index)
        }
        None => SourceSpan::empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_char_and_byte_indices_coincide() {
        let map = CharByteMap::new("abc");
        assert_eq!(map.char_count(), 3);
        assert_eq!(map.start_byte(0), 0);
        assert_eq!(map.start_byte(1), 1);
        assert_eq!(map.end_byte_inclusive(2), 3);
    }

    #[test]
    fn non_ascii_shifts_byte_offsets() {
        // "é" is 2 bytes (U+00E9), "中" is 3 bytes.
        let src = "é中x";
        let map = CharByteMap::new(src);
        assert_eq!(map.char_count(), 3);
        // char 0 ('é') starts at byte 0
        assert_eq!(map.start_byte(0), 0);
        // char 1 ('中') starts at byte 2 (after the 2-byte 'é')
        assert_eq!(map.start_byte(1), 2);
        // char 2 ('x') starts at byte 5 (after 2 + 3 bytes)
        assert_eq!(map.start_byte(2), 5);
        // inclusive end of char 2 ('x', 1 byte) -> exclusive byte 6
        assert_eq!(map.end_byte_inclusive(2), 6);
    }

    #[test]
    fn out_of_range_clamps_to_total_len() {
        let map = CharByteMap::new("ab");
        assert_eq!(map.start_byte(999), 2);
        assert_eq!(map.end_byte_inclusive(usize::MAX), 2);
    }

    #[test]
    fn span_maps_through_byte_line_index() {
        let src = "fun f() {}\nclass C\n";
        let map = CharByteMap::new(src);
        let li = LineIndex::new(src);
        // "class" begins at char index 11 == byte 11, on line 2.
        let span = span_from_char_range(11, 15, &map, &li);
        assert_eq!(span.start_byte, 11);
        assert_eq!(span.start_line, 2);
    }
}
