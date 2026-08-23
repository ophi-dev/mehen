// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use serde::{Deserialize, Serialize};

use crate::span::SourceSpan;

/// Severity of a [`ParseDiagnostic`].
///
/// Per the rewrite plan §9.3:
/// - `Warning`: recoverable, exit 0 unless thresholds fail.
/// - `Error`: analysis incomplete; `mehen metrics` exits 1, while
///   `mehen diff` records the affected side under `analysis_errors`
///   and only exits 1 when strict analysis gating is requested.
/// - `Fatal`: toolchain/invariant failure; follows the same command-level
///   policy as `Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Warning,
    Error,
    Fatal,
}

/// A diagnostic emitted by an analyzer.
///
/// Diagnostics are *non-fatal* by default — analyzers should produce the
/// best partial report they can and attach the diagnostic instead of
/// returning an error. Only the engine's exit-code mapping
/// (`mehen-engine::ci::exit_code_from_diagnostics`) translates severity
/// into a process exit code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParseDiagnostic {
    pub severity: DiagnosticSeverity,
    /// Stable identifier (`"python.parse_error"`, `"markdown.unclosed_fence"`).
    pub code: String,
    pub message: String,
    pub span: Option<SourceSpan>,
}

impl ParseDiagnostic {
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            code: code.into(),
            message: message.into(),
            span: None,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            code: code.into(),
            message: message.into(),
            span: None,
        }
    }

    pub fn fatal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Fatal,
            code: code.into(),
            message: message.into(),
            span: None,
        }
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }
}
