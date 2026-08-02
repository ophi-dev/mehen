// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Shared harness for the C# metric tests.
//!
//! Included via `mod common;` by each test binary, so the helpers are `pub`
//! for the including binary's benefit but never re-exported across a crate
//! boundary — hence the `unreachable_pub` allowance (the workspace lint is
//! aimed at library code, where an unreachable `pub` really is dead surface).
//!
//! `mehen-java` and `mehen-kotlin` inline an equivalent helper per test file
//! instead, because an earlier attempt at a shared module tripped the workspace's
//! `-D warnings` policy. This crate keeps the shared module deliberately: the
//! `allow` above resolves that (a `mod common` is compiled once per including
//! binary, so a helper any *one* file leaves unused is dead only in that binary),
//! and `analyze_clean`'s diagnostics assertion is load-bearing here in a way the
//! Java/Kotlin harnesses have no equivalent of. Nine copies of it would be nine
//! chances for one file to drop the assertion and start measuring error recovery
//! instead of the construct under test — the exact failure mode
//! `grammar/PROVENANCE.md` catalogues nineteen instances of.
#![allow(unreachable_pub, dead_code)]

use mehen_core::{AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, SourceFile};
use mehen_csharp::CSharpAnalyzer;

/// Analyze a C# snippet, normalizing the trailing newline the way the other
/// per-language test suites do.
pub fn analyze(source: &str) -> LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = CSharpAnalyzer::new();
    let file = SourceFile::new("Foo.cs".into(), Language::CSharp, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

/// Analyze a snippet and assert it produced no parse/lex diagnostics — every
/// metric test's input must be valid C#, or the numbers are measuring recovery
/// rather than the construct under test.
pub fn analyze_clean(source: &str) -> LanguageAnalysis {
    let a = analyze(source);
    assert!(
        a.diagnostics.is_empty(),
        "snippet must parse cleanly, got {:?}",
        a.diagnostics
    );
    a
}
