// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Exit code contract for the 1.0 CLI (rewrite plan §4.1).

#[derive(Debug, Clone, Copy)]
pub enum ExitCode {
    Success = 0,
    /// Setup, IO, git, parser fatal, unsupported-language, or invalid-state
    /// error. Also covers "analysis errors" diagnostics on `mehen metrics`
    /// and configured `mehen.toml` metric-threshold violations — every
    /// quality gate fails with the generic non-zero code so CI treats
    /// them uniformly.
    SetupError = 1,
    /// Documentation policy failure (`mehen diff --fail-on`) and JSON
    /// emission failures inside `mehen diff` keep this historical code.
    ThresholdFailure = 2,
    /// Invalid machine-output serialization state.
    SerializationError = 3,
}

impl From<ExitCode> for i32 {
    fn from(value: ExitCode) -> Self {
        value as i32
    }
}
