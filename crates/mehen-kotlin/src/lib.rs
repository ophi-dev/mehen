// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-kotlin` — Kotlin language analyzer.
//!
//! Kotlin is parsed by a parser generated from the **official Kotlin
//! specification ANTLR grammar** (`Kotlin/kotlin-spec`, vendored in
//! `grammar/`) running on the ANTLR Rust runtime via [`mehen_antlr`]. This
//! replaces the earlier tree-sitter-kotlin backend; the ANTLR grammar is a
//! richer, semantically-named CST (`whenEntry`, `elvisExpression`,
//! `catchBlock`, `jumpExpression` with explicit `THROW`/`RETURN`/`CONTINUE`/
//! `BREAK` alternatives, `safeNav`, …) that lets the metric walker ask
//! direct structural questions instead of inferring them from anonymous
//! punctuation and parent/sibling shape.
//!
//! The generated lexer/parser modules live in [`generated`]; they are
//! produced by `cargo xtask antlr generate kotlin` and checked in verbatim
//! (see `src/generated/README.md`). They are not hand-edited and are
//! `#[rustfmt::skip]`-wrapped because ANTLR's deeply-nested output does not
//! reach a rustfmt fixed point.
//!
//! Metric coverage follows the same SonarKotlin-aligned definitions the
//! tree-sitter walker targeted (see [`walker`] for the per-metric table).
//! Where the richer grammar makes a metric *more* correct than the
//! tree-sitter approximation, the change is intentional and called out in
//! the walker's docs.

#![forbid(unsafe_code)]

mod walker;

/// ANTLR-generated Kotlin lexer and parser.
///
/// `#[rustfmt::skip]` keeps `cargo fmt --all` from touching the generated
/// modules (they are not rustfmt-stable); `#[allow(warnings)]` silences the
/// lints the generator's machine output would otherwise trip. Regenerate
/// with `cargo xtask antlr generate kotlin` — never hand-edit.
#[rustfmt::skip]
#[allow(warnings)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
mod generated {
    pub mod kotlin_lexer;
    pub mod kotlin_parser;
}

use mehen_antlr::runtime::{CommonTokenStream, InputStream};
use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, LineIndex,
    ParseDiagnostic, Result, SourceFile, SourceSpan, byte_offset_clamped,
};

use generated::kotlin_lexer::KotlinLexer;
use generated::kotlin_parser::KotlinParser;

pub struct KotlinAnalyzer;

impl KotlinAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for KotlinAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for KotlinAnalyzer {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::Antlr
    }

    fn analyze(&self, source: &SourceFile, _config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let line_index = LineIndex::new(&source.text);

        // Kotlin scripts (`.kts`) allow top-level statements; the grammar
        // has a dedicated `script` entry rule for them. Regular `.kt` files
        // use `kotlinFile` (declarations after the import section). Picking
        // the wrong entry rule recovers a script as a cascade of syntax
        // errors and yields near-empty metrics.
        let is_script = source
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("kts"));

        let lexer = KotlinLexer::new(InputStream::new(source.text.as_str()));
        let token_stream = CommonTokenStream::new(lexer);
        let mut parser = KotlinParser::new(token_stream);
        let parsed = if is_script {
            parser.script()
        } else {
            parser.kotlin_file()
        };
        let tree = match parsed {
            Ok(tree) => tree,
            Err(err) => {
                // A hard parse failure (the entry rule itself erroring) means
                // we cannot produce any tree. Surface it as fatal and return
                // an empty unit space, matching the other backends.
                let span = SourceSpan {
                    start_byte: 0,
                    end_byte: byte_offset_clamped(source.text.len()),
                    start_line: 1,
                    end_line: line_index.line_count(),
                };
                return Ok(LanguageAnalysis {
                    language: Language::Kotlin,
                    backend: AnalysisBackend::Antlr,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "kotlin.parse_error",
                        format!("kotlin ANTLR parse failed: {err}"),
                    )],
                    root: mehen_antlr::empty_space(span),
                    contributions: Vec::new(),
                });
            }
        };

        // `KotlinParser::new` consumed the stream; re-lex to recover the full
        // buffered token list (including hidden-channel comments, which never
        // appear in the parse tree) in source order. LOC is driven from this
        // ordered token pass so comments and code interleave correctly.
        let loc_tokens = collect_loc_tokens(&source.text);

        let root = walker::walk(&tree, &source.text, &line_index, &loc_tokens);

        // Recovered ANTLR error nodes are surfaced as `error` (not
        // `warning`) so the diagnostic contract (plan §9.3) treats the
        // analysis as incomplete: `mehen metrics` exits 1 and `mehen diff`
        // records the file under `analysis_errors`.
        let diagnostics = mehen_antlr::collect_errors(&tree, "kotlin.syntax_error", 16);

        Ok(LanguageAnalysis {
            language: Language::Kotlin,
            backend: AnalysisBackend::Antlr,
            diagnostics,
            root,
            contributions: Vec::new(),
        })
    }
}

/// Re-lex `source` into the source-ordered LOC token list that drives the
/// LOC family. Comments (`LineComment` / `DelimitedComment`, plus the
/// string-mode `Inside_Comment`) are classified as comments; whitespace and
/// newlines (default- and string-mode) are skipped; every other token is a
/// code token. Comments are absent from the parse tree (hidden channel), so
/// LOC must come from this full token pass rather than the tree walk.
fn collect_loc_tokens(source: &str) -> Vec<mehen_antlr::LocToken> {
    use generated::kotlin_lexer::{
        DELIMITED_COMMENT, INSIDE_COMMENT, INSIDE_NL, INSIDE_WS, LINE_COMMENT, NL, WS,
    };

    let lexer = KotlinLexer::new(InputStream::new(source));
    let mut stream = CommonTokenStream::new(lexer);
    stream.fill();
    let map = mehen_antlr::CharByteMap::new(source);
    mehen_antlr::loc_tokens(
        stream.tokens(),
        &[LINE_COMMENT, DELIMITED_COMMENT, INSIDE_COMMENT],
        &[WS, NL, INSIDE_WS, INSIDE_NL],
        &map,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, SourceFile, SpaceKind};

    fn analyze(source: &str, path: &str) -> LanguageAnalysis {
        let analyzer = KotlinAnalyzer::new();
        let file = SourceFile::new(path.into(), Language::Kotlin, source.to_string());
        analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
    }

    #[test]
    fn empty_file_yields_root_unit() {
        let a = analyze("", "test.kt");
        assert_eq!(a.root.kind, SpaceKind::Unit);
        assert!(a.root.spaces.is_empty());
    }

    #[test]
    fn fun_creates_function_space() {
        let a = analyze("fun foo(): Int { return 1 }\n", "test.kt");
        assert!(a.root.spaces.iter().any(|s| s.kind == SpaceKind::Function));
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("foo"));
    }

    #[test]
    fn class_creates_class_space_with_method() {
        let a = analyze("class C { fun m() {} }\n", "test.kt");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("C"));
        assert_eq!(a.root.spaces[0].spaces.len(), 1);
    }

    #[test]
    fn interface_creates_interface_space() {
        let a = analyze("interface I { fun m() }\n", "test.kt");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Interface);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("I"));
    }

    /// `.kts` scripts allow top-level statements, which `kotlinFile` rejects.
    /// The analyzer must select the `script` entry rule for `.kts` inputs so
    /// a normal script parses cleanly and produces metrics rather than a
    /// cascade of recovered syntax errors.
    #[test]
    fn kts_script_uses_script_entry_rule() {
        let src = "val x = 1\nfun greet() { println(\"hi\") }\nfor (i in 1..10) { println(i) }\n";
        // As a `.kts` script: parses cleanly (no error-node cascade) and
        // sees the top-level function.
        let script = analyze(src, "build.gradle.kts");
        assert!(
            script.diagnostics.len() <= 1,
            "script should parse with at most a trailing diagnostic, got {}",
            script.diagnostics.len()
        );
        assert!(
            script
                .root
                .spaces
                .iter()
                .any(|s| s.kind == SpaceKind::Function),
            "script's top-level `fun greet` should open a function space"
        );

        // The same top-level-statement content in a `.kt` file is invalid
        // (declarations only after imports), so `kotlinFile` recovers many
        // errors — confirming the entry rule is chosen by extension.
        let file = analyze(src, "Main.kt");
        assert!(
            file.diagnostics.len() > script.diagnostics.len(),
            "`.kt` should report more syntax errors than `.kts` for script content"
        );
    }
}
