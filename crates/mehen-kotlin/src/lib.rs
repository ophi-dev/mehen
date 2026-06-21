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

use mehen_antlr::runtime::{CommonTokenStream, InputStream, ParseTree};
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

    /// Parse `source` and return the tree with the fewest recovered errors
    /// from the two top-level entry rules. The preferred rule (per the file
    /// extension) is tried first; the other is tried only if the preferred
    /// one recovered any errors, so clean input parses exactly once. Ties go
    /// to the preferred rule. Returns `None` only if both entry-rule calls
    /// hard-fail (return `Err` rather than a recovered tree).
    fn parse_best(&self, source: &str, prefers_script: bool) -> Option<ParseTree> {
        let preferred = self.parse_entry(source, prefers_script);
        // A clean (zero-error) preferred parse wins outright — no second parse.
        if let Some((_, 0)) = preferred {
            return preferred.map(|(tree, _)| tree);
        }
        let alternate = self.parse_entry(source, !prefers_script);
        match (preferred, alternate) {
            // Keep whichever recovered fewer errors; ties favor the preferred.
            (Some((pt, pe)), Some((at, ae))) => Some(if ae < pe { at } else { pt }),
            (Some((pt, _)), None) => Some(pt),
            (None, Some((at, _))) => Some(at),
            (None, None) => None,
        }
    }

    /// Parse with one entry rule (`script` when `script_rule` is true, else
    /// `kotlinFile`) and return the recovered tree alongside its recovered-
    /// error count, or `None` if the rule call hard-failed.
    fn parse_entry(&self, source: &str, script_rule: bool) -> Option<(ParseTree, usize)> {
        let lexer = KotlinLexer::new(InputStream::new(source));
        let mut parser = KotlinParser::new(CommonTokenStream::new(lexer));
        let tree = if script_rule {
            parser.script()
        } else {
            parser.kotlin_file()
        }
        .ok()?;
        let errors = mehen_antlr::collect_errors(&tree, "kotlin.syntax_error", usize::MAX).len();
        Some((tree, errors))
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

        // Kotlin has two top-level entry rules: `kotlinFile` (a compilation
        // unit — declarations after the import section) and `script` (allows
        // top-level statements, for `.kts`). Picking the wrong one recovers
        // the input as a cascade of syntax errors.
        //
        // The file extension is the *preferred* rule, but it isn't decisive:
        // the vendored `script` rule is finicky about `semi` terminators
        // (top-level `val`/`var` and blank-line-separated statements recover
        // a "missing NL"), and embedded/misnamed sources don't always match
        // their extension. So we parse with the preferred rule first and,
        // only if it recovered any errors, try the other and keep whichever
        // tree has fewer recovered errors. Clean inputs parse exactly once.
        let prefers_script = source
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("kts"));

        let tree = match self.parse_best(&source.text, prefers_script) {
            Some(tree) => tree,
            None => {
                // Both entry rules hard-failed (the rule call itself returned
                // Err, not a recovered tree) — we cannot produce any tree.
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
                        "kotlin ANTLR parse failed for both entry rules".to_string(),
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
        AS_SAFE, AT_BOTH_WS, AT_POST_WS, AT_PRE_WS, DELIMITED_COMMENT, EXCL_WS, INSIDE_COMMENT,
        INSIDE_NL, INSIDE_WS, LINE_COMMENT, NL, NOT_IN, NOT_IS, QUEST_WS, WS,
    };

    let lexer = KotlinLexer::new(InputStream::new(source));
    let mut stream = CommonTokenStream::new(lexer);
    stream.fill();
    let map = mehen_antlr::CharByteMap::new(source);
    mehen_antlr::loc_tokens(
        stream.tokens(),
        &[LINE_COMMENT, DELIMITED_COMMENT, INSIDE_COMMENT],
        &[WS, NL, INSIDE_WS, INSIDE_NL],
        // Operator tokens whose lexer rules embed the `Hidden` fragment, so a
        // comment glued to them lives inside the token text (e.g. `!is/* c */`).
        &[
            EXCL_WS, NOT_IS, NOT_IN, QUEST_WS, AS_SAFE, AT_POST_WS, AT_PRE_WS, AT_BOTH_WS,
        ],
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

    /// Top-level statements (script content) parse cleanly because the
    /// analyzer picks the entry rule (`script` vs `kotlinFile`) that recovers
    /// the fewest errors. The `.kts` extension *prefers* `script`, but the
    /// heuristic also recovers the same content in a misnamed `.kt` file —
    /// the extension is a preference, not a hard constraint.
    #[test]
    fn script_content_parses_cleanly_via_best_entry_rule() {
        // Pure top-level statements: clean under the `script` rule.
        let src = "println(\"hi\")\ngreet()\n";

        let script = analyze(src, "build.gradle.kts");
        assert!(
            script.diagnostics.is_empty(),
            "`.kts` top-level statements should parse cleanly, got {}",
            script.diagnostics.len()
        );

        // The same content in a `.kt` file: `kotlinFile` would reject the
        // statements, so the heuristic falls back to `script` and lands the
        // same clean parse.
        let file = analyze(src, "Main.kt");
        assert!(
            file.diagnostics.is_empty(),
            "`.kt` with statement content should fall back to `script`, got {}",
            file.diagnostics.len()
        );
    }

    /// A real compilation unit (declarations, no top-level statements) still
    /// parses cleanly as a `.kt` file via the preferred `kotlinFile` rule.
    #[test]
    fn kt_compilation_unit_parses_cleanly() {
        let src = "package demo\n\nclass C {\n    fun m(): Int = 1\n}\n";
        let a = analyze(src, "Main.kt");
        assert!(
            a.diagnostics.is_empty(),
            "compilation unit should parse cleanly, got {}",
            a.diagnostics.len()
        );
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
    }

    /// Regression: string-template interpolation (`"… ${expr} …"`) must parse
    /// cleanly. The Kotlin lexer pushes `DEFAULT_MODE` on `${` and relies on
    /// `}` popping back to string mode; the vendored grammar's `RCURL` rule
    /// is patched to `-> popMode` (the upstream Java action was a no-op in
    /// the Rust target), so the text after the interpolation is no longer
    /// mis-tokenized as Kotlin code.
    #[test]
    fn string_template_interpolation_parses_cleanly() {
        for src in [
            "fun f(x: Int) { val s = \"v=${x} y\" }\n",
            "fun f(b: Boolean) { val s = \"a ${ if (b) 1 else 2 } z\" }\n",
            "fun f(x: Int) {\n    val s = \"v=${x}\"\n    val y = x + 1\n}\n",
        ] {
            let a = analyze(src, "Main.kt");
            assert!(
                a.diagnostics.is_empty(),
                "string template should parse with no recovered errors, got {} for {src:?}",
                a.diagnostics.len()
            );
        }
    }
}
