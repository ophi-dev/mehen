// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-core` — parser-neutral domain types and analyzer traits.
//!
//! This crate is the contract layer between language analyzers and the
//! orchestration in `mehen-engine`. It exposes the shape of an analyzer's
//! output (`LanguageAnalysis`), the trait analyzers implement
//! (`LanguageAnalyzer`), and the re-entrance hook (`LanguageDispatcher`)
//! used by Markdown's embedded-code path and any future analyzer that
//! needs to recursively analyze a nested language fragment.
//!
//! Design notes (see `docs/mehen-1-0-from-scratch-rewrite-plan.md`):
//! - `LanguageAnalysis` is owned and `Send + 'static` — no parser-arena
//!   borrows leak across the API boundary.
//! - `SpaceKind` is intentionally open via `Custom(SmolStr)` so declarative
//!   analyzers (CloudFormation, Terraform, Kubernetes) can publish their
//!   own scope kinds without amending a closed enum.
//! - `MetricKey` is an open namespace, not a closed enum, for the same
//!   reason — language families can publish their own keys
//!   (e.g. `cloudformation.iam_spcm`) under the shared namespace.

#![forbid(unsafe_code)]

mod analysis;
mod analyzer;
mod backend;
mod config;
mod diagnostic;
mod language;
mod line_index;
mod metric_key;
mod report;
mod selector;
mod source;
mod space;
mod span;
mod threshold;

pub use analysis::{
    ContributionCollector, ContributionReason, LanguageAnalysis, MetricContribution, MetricSet,
    MetricValue,
};
pub use analyzer::{LanguageAnalyzer, LanguageDispatcher};
pub use backend::AnalysisBackend;
pub use config::AnalysisConfig;
pub use diagnostic::{DiagnosticSeverity, ParseDiagnostic};
pub use language::{Language, LanguageParseError, language_aliases};
pub use line_index::LineIndex;
pub use metric_key::{MetricKey, keys};
pub use report::{
    AnalysisErrorRecord, AnalyzeMetricsInput, DiffFile, DiffInput, DiffReport, DiffSide,
    MetricsReport, TopOffenderEntry, TopOffendersInput, TopOffendersReport,
};
pub use selector::{MetricSelector, SelectorAggregator, SelectorParseError};
pub use source::SourceFile;
pub use space::{MetricSpace, SpaceId, SpaceKind};
// `SmolStr` is part of the public API via `SpaceKind::Custom(SmolStr)`, so
// re-export it — callers (and tests) can construct custom spaces without a
// separate `smol_str` dependency.
pub use smol_str::SmolStr;
pub use span::{SourceSpan, byte_offset_checked, byte_offset_clamped};
pub use threshold::{Polarity, Threshold, ThresholdEvaluation, ThresholdViolation};

/// The result type used by analyzers and the dispatcher.
pub type Result<T> = core::result::Result<T, AnalysisError>;

/// Errors that flow through the analyzer interface. Recoverable issues
/// (parse errors, partial reports) belong on [`LanguageAnalysis::diagnostics`]
/// instead — `AnalysisError` is reserved for fatal conditions that prevent
/// producing any report at all.
#[derive(Debug)]
#[non_exhaustive]
pub enum AnalysisError {
    /// The requested language is structurally invalid for the operation.
    UnsupportedLanguage(Language),
    /// The owning analyzer crate was not compiled into this build.
    AnalyzerUnavailable(Language),
    /// Internal invariant violation — file a bug.
    Internal(String),
    /// IO failure reaching the source.
    Io(String),
}

impl core::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedLanguage(l) => write!(f, "unsupported language: {l:?}"),
            Self::AnalyzerUnavailable(l) => {
                write!(
                    f,
                    "language `{l:?}` has no registered analyzer in this build"
                )
            }
            Self::Internal(msg) => write!(f, "internal invariant: {msg}"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl core::error::Error for AnalysisError {}

impl From<std::io::Error> for AnalysisError {
    fn from(value: std::io::Error) -> Self {
        AnalysisError::Io(value.to_string())
    }
}
