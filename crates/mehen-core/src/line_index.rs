// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use serde::{Deserialize, Serialize};

/// Maps byte offsets to 1-based line numbers within a source file.
///
/// This exists in `mehen-core` rather than each analyzer crate because every
/// analyzer needs a single canonical byte/line mapping implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineIndex {
    /// Byte offsets at which each line starts. `line_starts[0]` is always 0.
    line_starts: Vec<u32>,
}

impl Default for LineIndex {
    fn default() -> Self {
        Self {
            line_starts: vec![0],
        }
    }
}

impl LineIndex {
    /// Build the row index for `text`.
    ///
    /// A row ends at `\n` or at a `\r` not followed by one (so CRLF is one break, not
    /// two). This is the **default** policy, and the one every tree-sitter-backed
    /// analyzer needs: tree-sitter's own `Point::row` advances at LF only, and a space's
    /// LOC span is set from those rows, so an index that counted more terminators than
    /// the parser would claim rows the walker never routes tokens to.
    ///
    /// Use [`LineIndex::with_unicode_separators`] for a language whose *lexer* also
    /// treats NEL (U+0085), LS (U+2028), or PS (U+2029) as terminators — C# does
    /// (ECMA-334 §6.3.1 lists all five). When the index and the lexer disagree, a file
    /// parses correctly while reporting the wrong number of rows and attributing
    /// declarations to rows they are not on.
    pub fn new(text: &str) -> Self {
        Self::build(text, false)
    }

    /// As [`LineIndex::new`], but NEL (U+0085), LS (U+2028), and PS (U+2029) also end a
    /// row.
    ///
    /// For a language whose lexer accepts them, which makes them real row breaks in that
    /// file. Kept opt-in rather than universal because the row *source* has to agree: a
    /// tree-sitter parser reports LF-only rows, so widening the index there produces
    /// spans whose `end_line` exceeds any row the walker observes — a phantom blank
    /// line.
    pub fn with_unicode_separators(text: &str) -> Self {
        Self::build(text, true)
    }

    /// Scanning `char_indices` rather than bytes keeps the multi-byte separators from
    /// being missed when `unicode_separators` is set.
    fn build(text: &str, unicode_separators: bool) -> Self {
        let mut line_starts = Vec::with_capacity(text.len() / 32 + 1);
        line_starts.push(0u32);
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let is_break = match c {
                // CRLF is a single break. Consume the `\n` here so the pair does not
                // push two row starts.
                '\r' => {
                    if chars.peek().is_some_and(|&(_, next)| next == '\n') {
                        chars.next();
                    }
                    true
                }
                '\n' => true,
                '\u{85}' | '\u{2028}' | '\u{2029}' => unicode_separators,
                _ => false,
            };
            if is_break {
                // The next row starts after whatever was consumed, which for CRLF is
                // two characters.
                let consumed = if c == '\r' && text[i..].starts_with("\r\n") {
                    2
                } else {
                    c.len_utf8()
                };
                line_starts.push((i + consumed) as u32);
            }
        }
        Self { line_starts }
    }

    /// Returns the 1-based line number containing `byte_offset`.
    pub fn line_at(&self, byte_offset: u32) -> u32 {
        // Binary search for the largest `line_starts[i] <= byte_offset`.
        match self.line_starts.binary_search(&byte_offset) {
            Ok(i) => (i + 1) as u32,
            Err(i) => i.max(1) as u32,
        }
    }

    /// Total line count (a final blank line is included).
    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// Returns `(start_byte, end_byte)` for a 1-based line number, exclusive
    /// of the trailing newline. Returns `None` for out-of-range lines.
    pub fn line_byte_range(&self, line: u32, total_len: u32) -> Option<(u32, u32)> {
        if line == 0 || (line as usize) > self.line_starts.len() {
            return None;
        }
        let idx = (line - 1) as usize;
        let start = self.line_starts[idx];
        let end = self
            .line_starts
            .get(idx + 1)
            .map(|next| next.saturating_sub(1))
            .unwrap_or(total_len);
        Some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_line_separators_start_new_rows_when_opted_in() {
        // REGRESSION. Only `\n` counted, so a file split by NEL / U+2028 / U+2029
        // reported one physical row and attributed every declaration to it. C#'s lexer
        // accepts all five terminators (ECMA-334 §6.3.1), so it could tokenize a
        // multi-row file that this index called one line.
        for separator in ['\n', '\u{85}', '\u{2028}', '\u{2029}'] {
            let text = format!("a{separator}b");
            let index = LineIndex::with_unicode_separators(&text);
            assert_eq!(
                index.line_count(),
                2,
                "U+{:04X} must start a new row",
                separator as u32
            );
            // The character after the separator is on row 2.
            let after = (1 + separator.len_utf8()) as u32;
            assert_eq!(index.line_at(after), 2);
        }
    }

    #[test]
    fn the_default_policy_ignores_unicode_separators() {
        // `new` stays LF/CRLF-only, and that is load-bearing rather than conservative:
        // every tree-sitter-backed analyzer sets a space's LOC span from tree-sitter's
        // own `Point::row`, which advances at LF alone. An index counting more
        // terminators than the parser claims rows the walker never routes tokens to —
        // a phantom blank line in, say, a Go raw string containing U+2028.
        for separator in ['\u{85}', '\u{2028}', '\u{2029}'] {
            let text = format!("a{separator}b");
            assert_eq!(
                LineIndex::new(&text).line_count(),
                1,
                "U+{:04X} is not a row break under the default policy",
                separator as u32
            );
        }
        // LF and CRLF break under both policies.
        assert_eq!(LineIndex::new("a\nb").line_count(), 2);
        assert_eq!(LineIndex::new("a\r\nb").line_count(), 2);
    }

    #[test]
    fn a_lone_carriage_return_is_a_row_break() {
        // A previous revision deliberately excluded a bare `\r`, on the reasoning that
        // CRLF works through its `\n` and a stray `\r` is a classic-Mac artifact. That
        // was wrong for mehen's purpose: *lexers* treat it as a terminator (C#'s does,
        // ECMA-334 §6.3.1), so excluding it made the row index disagree with the lexer
        // that produced the tokens — a file split by `\r` parsed correctly while every
        // declaration was attributed to row 1.
        let index = LineIndex::new("a\rb");
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_at(2), 2);
    }

    #[test]
    fn crlf_counts_one_row_break() {
        let index = LineIndex::new("a\r\nb");
        assert_eq!(index.line_count(), 2);
    }

    #[test]
    fn empty_text_has_one_line() {
        let idx = LineIndex::new("");
        assert_eq!(idx.line_count(), 1);
        assert_eq!(idx.line_at(0), 1);
    }

    #[test]
    fn line_at_boundaries() {
        // bytes:    0 1 2 3 4 5 6 7 8 9
        // text:     a b \n c d \n e f \n
        let idx = LineIndex::new("ab\ncd\nef\n");
        assert_eq!(idx.line_at(0), 1);
        assert_eq!(idx.line_at(2), 1); // '\n' on line 1
        assert_eq!(idx.line_at(3), 2);
        assert_eq!(idx.line_at(5), 2);
        assert_eq!(idx.line_at(6), 3);
    }

    #[test]
    fn byte_range_for_line() {
        let text = "ab\ncd\nef";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_byte_range(1, text.len() as u32), Some((0, 2)));
        assert_eq!(idx.line_byte_range(2, text.len() as u32), Some((3, 5)));
        assert_eq!(idx.line_byte_range(3, text.len() as u32), Some((6, 8)));
    }
}
