// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Structured diagnostic collection for ANTLR lexers and parse trees.
//!
//! Lexer errors are reported through runtime listeners because an unrecognized
//! character can be skipped before the parser sees the token stream. Parser
//! recovery instead surfaces as `ParseTree::Error` leaves embedded in an
//! otherwise-complete tree. Per mehen's diagnostic contract (rewrite plan
//! §9.3), both must become `error`-severity diagnostics so that `mehen metrics`
//! exits 1 and `mehen diff` records the file under `analysis_errors`.
//!
//! Hard parser failures (the entry-rule call itself returning `Err`) are a
//! separate, `fatal` path handled by the analyzer crate; this module only
//! covers listener diagnostics and recovered `Error` nodes.

use std::sync::{Arc, Mutex};

use antlr4_runtime::token::Token;
use antlr4_runtime::{ErrorListener, Node, Recognizer};
use mehen_core::{ParseDiagnostic, SourceSpan, byte_offset_clamped};

#[derive(Clone, Debug)]
struct CollectedDiagnostic {
    line: usize,
    column: usize,
    message: String,
    /// Byte range of the offending source text, when the runtime resolved one.
    ///
    /// Since the 0.23 runtime (upstream #257) `syntax_error` receives a
    /// [`SyntaxErrorEvent`] carrying the resolved byte span directly, so this no
    /// longer has to be reconstructed from the offending token. Absent for
    /// custom streams that cannot resolve byte offsets.
    span: Option<(u32, u32)>,
}

/// Cloneable runtime listener that records diagnostics without writing to
/// stderr.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticCollector {
    diagnostics: Arc<Mutex<Vec<CollectedDiagnostic>>>,
}

impl<R> ErrorListener<R> for DiagnosticCollector
where
    R: Recognizer + ?Sized,
{
    fn syntax_error(&mut self, _recognizer: &R, event: &antlr4_runtime::SyntaxErrorEvent<'_>) {
        // The event's span is already a half-open byte range, so it only needs
        // narrowing to mehen's `u32` offsets. `max` keeps the range well-formed
        // if clamping collapsed the two ends.
        let span = event.span.as_ref().map(|range| {
            let start = byte_offset_clamped(range.start);
            (start, byte_offset_clamped(range.end).max(start))
        });
        self.diagnostics
            .lock()
            .expect("ANTLR diagnostic collector lock poisoned")
            .push(CollectedDiagnostic {
                line: event.line,
                column: event.column,
                message: event.message.to_owned(),
                span,
            });
    }
}

impl DiagnosticCollector {
    /// Convert at most `max_diagnostics` collected runtime diagnostics to
    /// mehen's structured form.
    ///
    /// `line_index` resolves the span's *end* line. A single token can cover
    /// several rows — a verbatim or raw string literal is one token spanning as
    /// many lines as it contains — so deriving `end_line` from the end byte keeps
    /// the byte range and the line range describing the same region. Taking
    /// `end_line` from the start line instead yields a `SourceSpan` whose halves
    /// disagree, which a renderer highlighting by line would get wrong.
    pub fn diagnostics(
        &self,
        code: &str,
        max_diagnostics: usize,
        line_index: &mehen_core::LineIndex,
    ) -> Vec<ParseDiagnostic> {
        self.diagnostics
            .lock()
            .expect("ANTLR diagnostic collector lock poisoned")
            .iter()
            .take(max_diagnostics)
            .map(|diagnostic| {
                // The offending token's byte range, when the runtime had one.
                let span = diagnostic.span.map(|(start_byte, end_byte)| {
                    // From the byte offset for the same reason as `collect_errors`
                    // below: the runtime counts only `\n`, `LineIndex` may count more,
                    // and a span whose halves disagree misdirects any renderer that
                    // highlights by line.
                    let start_line = line_index.line_at(start_byte);
                    SourceSpan {
                        start_byte,
                        end_byte,
                        start_line,
                        end_line: line_index.line_at(end_byte).max(start_line),
                    }
                });
                // The *message* row comes from the same place as the span's, so the two
                // cannot contradict each other. Printing the runtime's own `line`
                // produced a diagnostic that named row 1 while its structured span
                // highlighted row 2, on any file using a terminator the runtime's lexer
                // does not count. Without a span there is nothing better to use, so the
                // runtime's row stands — it is at least self-consistent then.
                //
                // The column is left as reported: `LineIndex` resolves rows, not
                // columns, and re-deriving one would need the row's start byte plus a
                // decision about tabs and grapheme clusters that no consumer asks for.
                let line = span.map_or(diagnostic.line, |s| s.start_line as usize);
                let mut out = ParseDiagnostic::error(
                    code,
                    format!(
                        "ANTLR error at line {}:{}: {}",
                        line, diagnostic.column, diagnostic.message
                    ),
                );
                out.span = span;
                out
            })
            .collect()
    }

    #[cfg(test)]
    fn push_for_test(&self, line: usize, column: usize, span: Option<(u32, u32)>) {
        self.diagnostics
            .lock()
            .expect("ANTLR diagnostic collector lock poisoned")
            .push(CollectedDiagnostic {
                line,
                column,
                message: "test".to_string(),
                span,
            });
    }
}

/// Walk `tree` and emit one `error`-severity [`ParseDiagnostic`] per recovered
/// error leaf ([`NodeKind::Error`](antlr4_runtime::NodeKind::Error)), capped at
/// `max_diagnostics` to bound noise on heavily corrupted input.
///
/// `code` is the language-namespaced diagnostic code, e.g.
/// `"kotlin.syntax_error"`. Returns an empty `Vec` for a clean parse.
///
/// Since the 0.11 runtime rewrite the tree is a flat arena traversed through
/// borrowing [`Node`] views. [`Node::descendants`] yields a pre-order iterator
/// over the whole subtree, so error leaves are collected by filtering it with
/// [`Node::as_error`] — no hand-rolled recursion.
pub fn collect_errors(
    tree: Node<'_>,
    code: &str,
    max_diagnostics: usize,
    line_index: &mehen_core::LineIndex,
) -> Vec<ParseDiagnostic> {
    tree.descendants()
        .filter_map(Node::as_error)
        .take(max_diagnostics)
        .map(|err| {
            let token = err.symbol();
            // The error leaf owns the offending token, so the diagnostic can carry
            // its byte range. `stop_byte` is **exclusive** — the runtime's own
            // `Token::byte_span` is `start_byte()..stop_byte()` — so it is used
            // directly. This used to add 1, which made every diagnostic span one
            // byte too long: a `(` reported as `"( "`, swallowing the next
            // character. `span.rs` and `comments.rs` already used it directly, so
            // the `+1` was also inconsistent within this crate.
            //
            // `max` keeps the range well-formed if clamping collapsed the two ends
            // (a synthesized recovery token can be zero-width).
            //
            // Both offsets are optional since the 0.23 runtime: a token source
            // that cannot resolve byte offsets reports `None`. Leave the span
            // off in that case rather than inventing one — the line number in
            // the message still locates the error.
            //
            // `end_line` comes from the end byte, not from `line`: the offending
            // token may be a multi-row literal (see `diagnostics` above).
            let span = token
                .start_byte()
                .zip(token.stop_byte())
                .map(|(start, stop)| {
                    let start_byte = byte_offset_clamped(start);
                    let end_byte = byte_offset_clamped(stop).max(start_byte);
                    // From the byte offset, not the token's `line()`: the runtime's
                    // lexer advances its line counter on `\n` alone, while `LineIndex`
                    // (and the C# lexer) also treat CR, NEL, U+2028, and U+2029 as
                    // terminators. Taking `line()` produced a span whose `start_byte`
                    // was on row 2 and whose `start_line` said row 1.
                    let start_line = line_index.line_at(start_byte);
                    SourceSpan {
                        start_byte,
                        end_byte,
                        start_line,
                        end_line: line_index.line_at(end_byte).max(start_line),
                    }
                });
            // The message row comes from the span, so the prose and the structured span
            // agree — printing `token.line()` alongside a `LineIndex`-derived span made
            // the message name row 1 while the span highlighted row 2. Falls back to the
            // runtime's row only when there is no span to derive one from.
            let line = span.map_or_else(|| token.line(), |s| s.start_line as usize);
            let mut out = ParseDiagnostic::error(
                code.to_string(),
                format!("ANTLR error node at line {line}"),
            );
            out.span = span;
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::LineIndex;

    /// A diagnostic whose byte range covers several rows must report an `end_line`
    /// that matches — a single token CAN span rows (a verbatim or raw string
    /// literal), and forcing `end_line` to the start line yields a `SourceSpan`
    /// whose byte range and line range describe different regions.
    ///
    /// Driven through the collector directly rather than through a real parse: the
    /// ANTLR recovery strategy consistently attributes the error to a short token
    /// beside the multi-row literal rather than to the literal itself, so no C#
    /// input reaches this path. The span arithmetic is still wrong if it assumes a
    /// single row, and this pins it.
    #[test]
    fn a_multi_row_span_reports_its_real_end_line() {
        let source = "let s = @\"row1\nrow2\nrow3\";\n";
        let line_index = LineIndex::new(source);
        // The literal starts on row 1 and ends on row 3.
        let start = source.find('@').expect("literal present") as u32;
        let end = source.find(';').expect("terminator present") as u32;

        let collector = DiagnosticCollector::default();
        collector.push_for_test(1, 8, Some((start, end)));
        let diagnostics = collector.diagnostics("test.syntax_error", 16, &line_index);

        let span = diagnostics[0].span.expect("span present");
        assert_eq!(span.start_line, 1);
        assert_eq!(span.end_line, 3, "the literal covers three rows");
    }

    /// Both rows come from the byte offsets, so the runtime's own line counter cannot
    /// contradict the span — and `end_line` is still clamped to at least `start_line`
    /// so a zero-width range never inverts.
    ///
    /// The injected `line` here is deliberately wrong (row 3 for a byte on row 1): the
    /// runtime's lexer advances its counter on `\n` alone, while `LineIndex` counts all
    /// five terminators, so the two disagree on any file that uses another one. The
    /// byte offset is the authority.
    #[test]
    fn rows_come_from_the_byte_offsets_not_the_runtime_line() {
        let line_index = LineIndex::new("one\ntwo\nthree\n");
        let collector = DiagnosticCollector::default();
        collector.push_for_test(3, 0, Some((0, 0)));
        let diagnostics = collector.diagnostics("test.syntax_error", 16, &line_index);

        let span = diagnostics[0].span.expect("span present");
        assert_eq!(
            span.start_line, 1,
            "byte 0 is row 1, whatever `line` claimed"
        );
        assert_eq!(span.end_line, 1);
    }
}
