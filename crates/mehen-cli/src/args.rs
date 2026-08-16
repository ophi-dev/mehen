// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// `mehen` — code metrics CLI.
///
/// `--version` is implemented as a global flag (rather than via clap's
/// auto-generated handling) so it can pair with `--json` to produce a
/// machine-readable shape that the GitHub Action reads to stamp its
/// sticky comment footer with the running mehen version. Without this
/// pairing, `mehen --version --json` would just print the plain
/// "mehen X.Y.Z" string and the action's JSON parser would silently
/// drop the version.
#[derive(Debug, Parser)]
#[command(
    name = "mehen",
    bin_name = "mehen",
    about = "Compute and report code metrics.",
    disable_version_flag = true
)]
pub(crate) struct Cli {
    /// Print version information and exit.
    #[arg(long, short = 'V', global = true)]
    pub(crate) version: bool,

    /// Emit output as JSON. Currently only meaningful with
    /// `--version`; clap rejects the flag unless `--version` is also
    /// passed.
    #[arg(long, global = true, requires = "version")]
    pub(crate) json: bool,

    /// Path to a configuration file. When omitted, `mehen.toml` (or
    /// `.mehen.toml`) is discovered from the current directory upward,
    /// stopping at the enclosing git repository root.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Subcommands flatten the legacy `DiffOpts` / `TopOffendersOpts`
/// argument shapes so the existing pre-1.0 tests against those flag
/// surfaces keep passing through the new binary. Each pre-1.0
/// argument is physically reachable via
/// `cargo run -p mehen -- diff …`.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Analyze exactly one file and emit a metrics report.
    Metrics(MetricsArgs),
    /// Compare metrics between two git revisions.
    Diff(mehen_engine::DiffOpts),
    /// Rank files by one or more metrics (worst offenders first).
    TopOffenders(mehen_engine::TopOffendersOpts),
}

#[derive(Debug, Args)]
pub(crate) struct MetricsArgs {
    /// Path to the file to analyze. `mehen metrics` never walks directories.
    pub(crate) path: PathBuf,

    /// Override language detection.
    #[arg(long)]
    pub(crate) language: Option<String>,

    /// Output format.
    #[arg(long, default_value = "json")]
    pub(crate) format: OutputFormat,

    /// Pretty-print JSON output.
    #[arg(long)]
    pub(crate) pretty: bool,

    /// Built-in profile preset.
    #[arg(long, default_value = "default")]
    pub(crate) profile: Profile,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    Json,
    Markdown,
    Yaml,
    Toml,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum Profile {
    Default,
    Ci,
    Strict,
}
