// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Coverage-report ingestion: parsing, merging, and source-path matching.
//!
//! This crate is the format layer of mehen's `coverage.*` metric family.
//! It stays deliberately pure — no filesystem walking, no git, no metric
//! spaces — so the engine can feed it bytes from any origin (explicit CLI
//! paths, tool-config introspection, or the artifact scan in
//! `mehen-coverage-discovery`) and query the result per analyzed file:
//!
//! 1. [`detect_format`] sniffs a candidate file (path + first 4 KiB) and
//!    assigns one of the six supported [`CoverageFormat`]s.
//! 2. Each format parser streams a report into per-file
//!    [`FileCoverage`] records (lines, branch arms, functions).
//! 3. [`merge::merge_reports`] folds any number of reports into one
//!    deterministic, normalized [`CoverageData`] (union of files,
//!    saturating-max hit counts — "covered anywhere ⇒ covered").
//! 4. [`CoverageIndex`] answers "coverage for this workspace file?" via a
//!    two-level path match: canonical absolute lookup first, then a
//!    component-wise longest-suffix match that absorbs CI prefixes, JaCoCo
//!    package paths, and Go module import paths.
//!
//! The parsers are adapted from the MIT-licensed
//! [covrs](https://github.com/scttnlsn/covrs) project by Scott Nelson —
//! see `LICENSE-THIRD-PARTY` at the repository root for attribution and
//! the per-file provenance headers for local changes.

#![deny(unsafe_code)]

mod error;
mod index;
pub mod merge;
mod model;
pub mod parsers;

pub use error::CoverageError;
pub use index::{CoverageIndex, FileMatch};
pub use model::{
    BranchCoverage, CoverageData, FileCoverage, FunctionCoverage, LineCoverage, SpanTotals, rate,
};
pub use parsers::{CoverageFormat, CoverageParser, detect, for_format};

/// Convenience result alias used across the crate.
pub type Result<T> = std::result::Result<T, CoverageError>;

/// Sniff a candidate report (path plus the first few KiB of content) and
/// return the detected format, if any. Detection is cheap by contract:
/// extension/filename checks plus content markers within the first 4 KiB.
#[must_use]
pub fn detect_format(path: &camino::Utf8Path, head: &[u8]) -> Option<CoverageFormat> {
    parsers::detect(path, head).map(|p| p.format())
}

/// Parse a complete report of a known format from raw bytes.
pub fn parse_report(format: CoverageFormat, input: &[u8]) -> Result<CoverageData> {
    let parser = for_format(format);
    let mut data = CoverageData::new();
    parser.parse_streaming(&mut &*input, &mut |file| {
        data.files.push(file);
        Ok(())
    })?;
    Ok(data)
}
