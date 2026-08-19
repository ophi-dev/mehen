// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use core::fmt;

/// Failure while reading or parsing a coverage report.
///
/// Kept deliberately small: callers either surface the message as a
/// diagnostic (discovered reports degrade to warnings) or as a hard,
/// user-attributable error (explicit `--coverage` paths).
#[derive(Debug)]
pub enum CoverageError {
    /// I/O failure while reading report bytes.
    Io(std::io::Error),
    /// Malformed report content. The message carries position context
    /// where the underlying parser provides it.
    Malformed(String),
}

impl fmt::Display for CoverageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error reading coverage report: {e}"),
            Self::Malformed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for CoverageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Malformed(_) => None,
        }
    }
}

impl From<std::io::Error> for CoverageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
