// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Command implementations for the 1.0 CLI.

use std::io::{self, Write};

use camino::Utf8PathBuf;

use mehen_core::{AnalysisConfig, DiagnosticSeverity, Language, MetricsReport, SourceFile};
use mehen_engine::{AnalyzeMetricsInput, analyze_metrics, detect_language};
use mehen_report::render_metrics_json;

use crate::args::{MetricsArgs, OutputFormat, Profile};
use crate::exit::ExitCode;

pub(crate) fn metrics(args: MetricsArgs, config: Option<&mehen_engine::ConfigFile>) -> ExitCode {
    let path = match Utf8PathBuf::try_from(args.path.clone()) {
        Ok(p) => p,
        Err(_) => {
            log::error!("path is not valid UTF-8: {}", args.path.display());
            return ExitCode::SetupError;
        }
    };

    let language = if let Some(lang_str) = args.language.as_deref() {
        match lang_str.parse::<Language>() {
            Ok(l) => l,
            Err(_) => {
                log::error!("unknown --language value: {lang_str}");
                return ExitCode::SetupError;
            }
        }
    } else {
        match detect_language(path.as_path()) {
            Some(l) => l,
            None => {
                log::error!(
                    "could not detect language from path `{path}`; pass --language explicitly"
                );
                return ExitCode::SetupError;
            }
        }
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            log::error!("failed to read `{path}`: {e}");
            return ExitCode::SetupError;
        }
    };

    let source = SourceFile::new(path, language, text);
    let input = AnalyzeMetricsInput {
        source,
        config: config_for_profile(args.profile),
    };

    let report = match analyze_metrics(input) {
        Ok(r) => r,
        Err(e) => {
            log::error!("analysis failed: {e}");
            return ExitCode::SetupError;
        }
    };

    // Coverage enrichment (`--coverage`, `[coverage]` config, or a
    // configured coverage threshold): folds the `coverage.*` family
    // into the metric tree before rendering and threshold evaluation.
    // An explicit report path that is missing or unparseable is a
    // setup error; "no coverage found/matched" leaves the report
    // untouched.
    let mut report = report;
    if let Err(e) = mehen_engine::enrich_metrics_with_coverage(&mut report, &args.coverage, config)
    {
        log::error!("{e}");
        return ExitCode::SetupError;
    }
    let report = report;

    if let Some(exit) = render_report(&report, args.format, args.pretty)
        && !matches!(exit, ExitCode::Success)
    {
        return exit;
    }
    let report_exit = exit_code_from_report(&report);
    if !matches!(report_exit, ExitCode::Success) {
        // Blocking diagnostics mean the metrics are partial (§9.3):
        // exit 1 without threshold-gating numbers from a broken parse.
        return report_exit;
    }

    // Configured metric thresholds (`mehen.toml`): `mehen metrics`
    // reports the full metric set, so every configured threshold that
    // matches a key this file publishes is evaluated against the root
    // space. Violations print a grouped report on stderr and fail the
    // command with the generic failure code (exit 1).
    if let Some(config) = config {
        let mut breaches =
            config
                .thresholds
                .evaluate(report.path.as_str(), language, &report.root, None);
        if !breaches.is_empty() {
            eprint!(
                "{}",
                mehen_engine::render_threshold_report(&mut breaches, &config.path)
            );
            return ExitCode::SetupError;
        }
    }
    ExitCode::Success
}

/// Map the `--profile` flag to an [`AnalysisConfig`]. Until plan §3.6
/// designs threshold/polarity profiles, the only knob `AnalysisConfig`
/// exposes is `emit_contributions`; `default` follows the production
/// preset, `ci`/`strict` skip contribution evidence to keep CI runs
/// lean.
fn config_for_profile(profile: Profile) -> AnalysisConfig {
    match profile {
        Profile::Default => AnalysisConfig::production(),
        // `ci` and `strict` are still placeholders for thresholding, but
        // they should not silently inherit `production`'s defaults — the
        // CLI flag must observably differ from the default. Both presets
        // skip contribution evidence (cheap; emits the same metric
        // numbers) so `--profile` is no longer a no-op.
        Profile::Ci | Profile::Strict => AnalysisConfig::benchmark(),
    }
}

/// Map a `MetricsReport`'s diagnostic severities to a CLI exit code per
/// the diagnostic contract (rewrite plan §9.3): `Warning` is exit 0,
/// `Error`/`Fatal` are exit 1. Configured `mehen.toml` threshold
/// violations are handled separately in [`metrics`] (also exit 1) and
/// only on reports without blocking diagnostics.
fn exit_code_from_report(report: &MetricsReport) -> ExitCode {
    let has_error_or_fatal = report.diagnostics.iter().any(|d| {
        matches!(
            d.severity,
            DiagnosticSeverity::Error | DiagnosticSeverity::Fatal
        )
    });
    if has_error_or_fatal {
        ExitCode::SetupError
    } else {
        ExitCode::Success
    }
}

fn render_report(report: &MetricsReport, format: OutputFormat, pretty: bool) -> Option<ExitCode> {
    match format {
        OutputFormat::Json => match render_metrics_json(report, pretty) {
            Ok(rendered) => {
                let mut stdout = io::stdout().lock();
                if writeln!(stdout, "{rendered}").is_err() {
                    return Some(ExitCode::SerializationError);
                }
                None
            }
            Err(e) => {
                log::error!("failed to render JSON: {e}");
                Some(ExitCode::SerializationError)
            }
        },
        OutputFormat::Markdown => {
            let rendered = mehen_report::render_metrics_markdown(report);
            let mut stdout = io::stdout().lock();
            if writeln!(stdout, "{rendered}").is_err() {
                return Some(ExitCode::SerializationError);
            }
            None
        }
        OutputFormat::Yaml | OutputFormat::Toml => {
            log::error!(
                "the {format:?} format is reserved for a future phase; use --format json or markdown."
            );
            Some(ExitCode::SetupError)
        }
    }
}
