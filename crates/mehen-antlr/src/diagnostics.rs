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
    pub fn diagnostics(&self, code: &str, max_diagnostics: usize) -> Vec<ParseDiagnostic> {
        self.diagnostics
            .lock()
            .expect("ANTLR diagnostic collector lock poisoned")
            .iter()
            .take(max_diagnostics)
            .map(|diagnostic| {
                let mut out = ParseDiagnostic::error(
                    code,
                    format!(
                        "ANTLR error at line {}:{}: {}",
                        diagnostic.line, diagnostic.column, diagnostic.message
                    ),
                );
                // The offending token's byte range, when the runtime had one.
                // `line` is 1-based in ANTLR and in `SourceSpan`, so it carries
                // over directly; a single token never spans rows here (a
                // multi-line literal is still reported at its start line).
                out.span = diagnostic.span.map(|(start_byte, end_byte)| SourceSpan {
                    start_byte,
                    end_byte,
                    start_line: diagnostic.line.max(1) as u32,
                    end_line: diagnostic.line.max(1) as u32,
                });
                out
            })
            .collect()
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
pub fn collect_errors(tree: Node<'_>, code: &str, max_diagnostics: usize) -> Vec<ParseDiagnostic> {
    tree.descendants()
        .filter_map(Node::as_error)
        .take(max_diagnostics)
        .map(|err| {
            let token = err.symbol();
            let line = token.line();
            let mut out = ParseDiagnostic::error(
                code.to_string(),
                format!("ANTLR error node at line {line}"),
            );
            // The error leaf owns the offending token, so the diagnostic can
            // carry its byte range. `stop_byte` is inclusive; a synthesized
            // (missing) recovery token can be zero-width, hence the `max`.
            //
            // Both offsets are optional since the 0.23 runtime: a token source
            // that cannot resolve byte offsets reports `None`. Leave the span
            // off in that case rather than inventing one — the line number in
            // the message still locates the error.
            out.span = token
                .start_byte()
                .zip(token.stop_byte())
                .map(|(start, stop)| {
                    let start_byte = byte_offset_clamped(start);
                    SourceSpan {
                        start_byte,
                        end_byte: byte_offset_clamped(stop.saturating_add(1)).max(start_byte),
                        start_line: line.max(1) as u32,
                        end_line: line.max(1) as u32,
                    }
                });
            out
        })
        .collect()
}
