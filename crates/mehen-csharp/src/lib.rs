// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-csharp` — C# language analyzer.
//!
//! C# is parsed by a parser derived from **Roslyn's own published grammar**
//! (`CSharp.Generated.g4`, vendored in `crates/mehen-csharp-parser/grammar/`),
//! running on the ANTLR Rust runtime via [`mehen_antlr`]. Roslyn generates that
//! grammar from `Syntax.xml` — the same model that generates the compiler's syntax
//! node classes — so it tracks **C# as implemented**: records, `is not`, `and`/`or`
//! and relational patterns, list patterns, collection expressions, raw strings,
//! primary constructors, `required` members, and the C# 14 additions (`field`,
//! extension blocks). No community grammar does.
//!
//! Rules are named after the compiler's syntax nodes (`class_declaration`,
//! `property_declaration`, `switch_expression_arm`, `simple_lambda_expression`, …),
//! which is what makes the walker's classification a `rule_index()` match rather
//! than a keyword probe. See [`walker`] for the shape's consequences.
//!
//! The generated lexer/parser modules live in the separate [`mehen_csharp_parser`]
//! crate, produced by `cargo xtask antlr generate csharp` and checked in verbatim.
//! Both recognizers are constructed plainly — `CSharpLexer::new`,
//! `CSharpParser::new` — because that crate ships **no hooks at all**: every
//! semantic coordinate lowers to pure SemIR through the derived `patterns.toml`,
//! and the interpolated-string brace bookkeeping lives in the grammar's own
//! `@lexer::members`.
//!
//! Metric coverage follows SonarC#'s definitions where they exist; see [`walker`]
//! for the per-metric table.
//!
//! # What the transform repairs, and what remains
//!
//! Roslyn's grammar is a **reference** grammar rather than a working parser: ANTLR
//! rejects it outright (empty rules for its "omitted" syntax nodes), it publishes
//! no lexer at all (terminals are character-level *parser* rules), and it is
//! permissive by design — it models syntax nodes including error-recovery ones, and
//! encodes no operator precedence. `prepare-grammar.py` repairs that as a step of
//! parser generation.
//!
//! The catalogue lives in `crates/mehen-csharp-parser/grammar/PROVENANCE.md`, with
//! the measured effect of each repair. Two things worth knowing here:
//!
//! - **A clean parse measures parseability, not correctness.** Seventeen distinct
//!   *silent misparses* have come out of this grammar — structurally wrong trees
//!   with zero reported errors — each caught by a metric test or a parse-tree dump,
//!   never by an error count. That is why the per-language tests assert numbers
//!   against an equivalent spelling rather than just checking for diagnostics: two of
//!   the seventeen hid behind a passing test whose input happened to use the one
//!   spelling that parses correctly.
//! - **One known limitation remains:** a preprocessor directive that splits a
//!   single expression across `#if` branches (a return type, say) yields two
//!   partial expressions where one belongs. Five of 322 files in the
//!   `System.Text.Json` corpus hit it; they carry `csharp.syntax_error` and their
//!   metrics near the split are approximations.

#![forbid(unsafe_code)]

mod walker;

use mehen_antlr::DiagnosticCollector;
use mehen_antlr::runtime::{CommonTokenStream, InputStream, ParsedFile};
use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, LineIndex,
    ParseDiagnostic, Result, SourceFile, SourceSpan, byte_offset_clamped,
};

use mehen_csharp_parser::c_sharp_lexer::CSharpLexer;
use mehen_csharp_parser::c_sharp_parser::CSharpParser;

pub struct CSharpAnalyzer;

/// A recovered parse: the flat-arena [`ParsedFile`] owns the token store and
/// CST storage, and the walker borrows [`Node`](mehen_antlr::runtime::Node)
/// views from it. `loc_tokens` is precomputed from the (eagerly buffered,
/// hidden-channel-inclusive) token store.
struct ParsedCSharp {
    parsed: ParsedFile,
    lexer_diagnostics: Vec<ParseDiagnostic>,
    loc_tokens: Vec<mehen_antlr::LocToken>,
}

impl CSharpAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Parse `source` via the single `compilation_unit` entry rule and return
    /// the recovered [`ParsedFile`] plus the source-ordered LOC token list.
    /// Returns `None` only if the rule call hard-fails (returns `Err` rather
    /// than a recovered tree).
    ///
    /// Both recognizers are constructed plainly (`CSharpLexer::new`,
    /// `CSharpParser::new`): the derived grammar needs no hooks. Its
    /// interpolated-string state lives in `@lexer::members` and every action and
    /// predicate lowers to pure SemIR through the derived `patterns.toml`, so
    /// there is no hand-written Rust to install.
    ///
    /// Replaces the runtime's default lexer console listener with a structured
    /// diagnostic collector and removes the parser console listener.
    fn parse(&self, source: &str, line_index: &LineIndex) -> Option<ParsedCSharp> {
        // No hooks: the derived grammar keeps its interpolated-string state in
        // `@lexer::members`, lowered to pure SemIR via `patterns.toml`.
        let mut lexer = CSharpLexer::new(InputStream::new(source));
        lexer.remove_error_listeners();
        let lexer_diagnostics = DiagnosticCollector::default();
        lexer.add_error_listener(lexer_diagnostics.clone());
        let tokens = CommonTokenStream::new(lexer);
        let mut parser = CSharpParser::new(tokens);
        parser.remove_error_listeners();
        let result = parser.compilation_unit().ok()?;
        let lexer_diagnostics =
            lexer_diagnostics.diagnostics("csharp.syntax_error", 16, line_index);

        // `into_parsed_file` consumes the parser and moves the eagerly-buffered
        // token store into the `ParsedFile`; the LOC token list is then read
        // straight from that store (all channels, so hidden-channel comments
        // are present — no `fill()` step needed).
        let parsed = parser.into_parsed_file(result);
        let loc_tokens = collect_loc_tokens(&parsed, line_index);
        Some(ParsedCSharp {
            parsed,
            lexer_diagnostics,
            loc_tokens,
        })
    }
}

impl Default for CSharpAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for CSharpAnalyzer {
    fn language(&self) -> Language {
        Language::CSharp
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::Antlr
    }

    fn analyze(&self, source: &SourceFile, _config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        // `with_unicode_separators`, not `new`: this grammar's lexer treats NEL,
        // U+2028, and U+2029 as line terminators (ECMA-334 §6.3.1), so they are real row
        // breaks here. The default policy is LF/CRLF-only because every
        // tree-sitter-backed analyzer's row source counts only LF, and an index that
        // disagrees with the parser produces spans the walker never routes tokens to.
        let line_index = LineIndex::with_unicode_separators(&source.text);

        let parsed = match self.parse(&source.text, &line_index) {
            Some(parsed) => parsed,
            None => {
                let span = SourceSpan {
                    start_byte: 0,
                    end_byte: byte_offset_clamped(source.text.len()),
                    start_line: 1,
                    end_line: line_index.line_count(),
                };
                return Ok(LanguageAnalysis {
                    language: Language::CSharp,
                    backend: AnalysisBackend::Antlr,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "csharp.parse_error",
                        "csharp ANTLR parse failed".to_string(),
                    )],
                    root: mehen_antlr::empty_space(span),
                    contributions: Vec::new(),
                });
            }
        };

        // The `ParsedFile` owns the token store and CST; `tree()` is the root
        // `Node` borrowing view the walker traverses.
        let tree = parsed.parsed.tree();
        let root = walker::walk(tree, &line_index, source.text.len(), &parsed.loc_tokens);

        // Recovered ANTLR error nodes are surfaced as `error` so the
        // diagnostic contract treats the analysis as incomplete.
        let mut diagnostics = parsed.lexer_diagnostics;
        let remaining = 16usize.saturating_sub(diagnostics.len());
        diagnostics.extend(mehen_antlr::collect_errors(
            tree,
            "csharp.syntax_error",
            remaining,
            &line_index,
        ));

        Ok(LanguageAnalysis {
            language: Language::CSharp,
            backend: AnalysisBackend::Antlr,
            diagnostics,
            root,
            contributions: Vec::new(),
        })
    }
}

/// Classify the parsed file's token store into the source-ordered LOC token
/// list that drives the LOC family.
///
/// C# comments come in four token types here: the two doc-comment forms (`///`
/// and `/** */`) plus the plain `//` and `/* */` forms. Whitespace is
/// `WHITESPACES` and a leading BOM is `BYTE_ORDER_MARK`; neither is code.
///
/// A preprocessor directive (`DIRECTIVE_LINE`) is **code**, not trivia. mehen does
/// not evaluate `#if` — directives go to their own channel so the parser never sees
/// them, and an inactive region is parsed as ordinary code (see the LOC tests) —
/// but the directive row itself is still a physical line carrying source text. It is
/// deliberately not a *logical* line (`#endif` is not a statement) and not a
/// comment, so leaving it out of PLOC entirely made it fall through to
/// `blank = sloc - ploc - only_comment` and report as a blank line, which it plainly
/// is not.
///
/// Unlike Kotlin, C# has no trivia-folding operator tokens, so no trivia-bearing
/// token scan is needed. The token store is eagerly buffered through EOF, so every
/// token (all channels) is present.
fn collect_loc_tokens(parsed: &ParsedFile, line_index: &LineIndex) -> Vec<mehen_antlr::LocToken> {
    use mehen_csharp_parser::c_sharp_lexer::{
        BYTE_ORDER_MARK, DELIMITED_COMMENT, DELIMITED_DOC_COMMENT, SINGLE_LINE_COMMENT,
        SINGLE_LINE_DOC_COMMENT, WHITESPACES,
    };

    mehen_antlr::loc_tokens(
        parsed.tokens(),
        &[
            SINGLE_LINE_COMMENT,
            DELIMITED_COMMENT,
            SINGLE_LINE_DOC_COMMENT,
            DELIMITED_DOC_COMMENT,
        ],
        &[WHITESPACES, BYTE_ORDER_MARK],
        &[],
        line_index,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, SourceFile, SpaceKind};

    fn analyze(source: &str, path: &str) -> LanguageAnalysis {
        let analyzer = CSharpAnalyzer::new();
        let file = SourceFile::new(path.into(), Language::CSharp, source.to_string());
        analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
    }

    #[test]
    fn empty_file_yields_root_unit() {
        let a = analyze("", "Empty.cs");
        assert_eq!(a.root.kind, SpaceKind::Unit);
        assert!(a.root.spaces.is_empty());
    }

    #[test]
    fn class_with_method_parses_cleanly() {
        let src =
            "namespace Demo\n{\n    class C\n    {\n        int M() { return 1; }\n    }\n}\n";
        let a = analyze(src, "C.cs");
        assert!(
            a.diagnostics.is_empty(),
            "compilation unit should parse cleanly, got {}",
            a.diagnostics.len()
        );
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("C"));
    }

    #[test]
    fn interpolated_string_parses_cleanly() {
        // Interpolation is the grammar's most stateful construct — three lexer
        // modes plus a brace-depth stack — and all of it lowers to SemIR, so this
        // is the case that would fail loud (`--sem-unknown error`) if a lowering
        // regressed.
        let src = "class C { string M(int x) { return $\"a{x}b\"; } }\n";
        let a = analyze(src, "C.cs");
        assert!(
            a.diagnostics.is_empty(),
            "interpolated string should lex/parse cleanly, got {:?}",
            a.diagnostics
        );
    }
}
