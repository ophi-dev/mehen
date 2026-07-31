// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-csharp` — C# language analyzer.
//!
//! C# is parsed by a parser generated from the community-maintained
//! **grammars-v4 C# grammar** (`antlr/grammars-v4`, vendored in
//! `crates/mehen-csharp-parser/grammar/`) running on the ANTLR Rust runtime via
//! [`mehen_antlr`]. The grammar covers C# through 8.x (pattern matching,
//! interpolated strings, local functions, expression-bodied members, ranges,
//! null-coalescing assignment), giving the metric walker a semantically-named
//! CST (`class_definition`, `property_declaration`, `switch_section`,
//! `lambda_expression`, `conditional_and_expression`, …).
//!
//! The generated lexer/parser modules live in the separate
//! [`mehen_csharp_parser`] crate; they are produced by
//! `cargo xtask antlr generate csharp` and checked in verbatim. That crate also
//! ships the hand-written [`CSharpLexerBase`](mehen_csharp_parser::hooks::CSharpLexerBase)
//! hooks the grammar's `superClass` requires — the lexer **must** be built with
//! them (`with_typed_hooks`), or interpolated strings and `#if` preprocessor
//! directives fail loud instead of lexing.
//!
//! Metric coverage follows SonarC#'s definitions where they exist; see
//! [`walker`] for the per-metric table.
//!
//! # Language-version limitation
//!
//! The vendored grammar is **C# 7-era**. Nullable reference types, `??=`,
//! tuple deconstruction, and file-scoped namespaces parse fine, but several
//! post-7 constructs do not: `switch` *expressions* (C# 8), `is not` and the
//! C# 9 logical/relational patterns (`is int i and > 5`), and `record`
//! declarations. Affected files still produce metrics — mehen recovers from
//! parse errors by design — but they carry `csharp.syntax_error` diagnostics
//! and the metrics around the unparsed construct are approximations. See
//! `crates/mehen-csharp-parser/grammar/PROVENANCE.md` for the measured impact,
//! why upstream's `v8-spec` grammar is not a drop-in fix, and what would be
//! needed to move to Roslyn's own (complete) grammar instead.

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
    /// The lexer is constructed with the [`CSharpLexerBase`] typed hooks — the
    /// exact port of the grammar's `superClass` state machine (interpolated
    /// strings, `#if` preprocessor evaluation). The generated modules use
    /// `--sem-unknown error`, so a hook-less lexer would hard-fail (not
    /// mis-lex) on any file containing an interpolated string.
    ///
    /// Replaces the runtime's default lexer console listener with a structured
    /// diagnostic collector and removes the parser console listener.
    fn parse(&self, source: &str) -> Option<ParsedCSharp> {
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
        let lexer_diagnostics = lexer_diagnostics.diagnostics("csharp.syntax_error", 16);

        // `into_parsed_file` consumes the parser and moves the eagerly-buffered
        // token store into the `ParsedFile`; the LOC token list is then read
        // straight from that store (all channels, so hidden-channel comments
        // are present — no `fill()` step needed).
        let parsed = parser.into_parsed_file(result);
        let loc_tokens = collect_loc_tokens(&parsed);
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
        let line_index = LineIndex::new(&source.text);

        let parsed = match self.parse(&source.text) {
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
/// C# comments come in five token types: the three doc-comment forms
/// (`///`, `/** */`, and the empty `/***/`) plus the plain `//` and `/* */`
/// forms. Whitespace is `WHITESPACES`; the lexer also emits
/// `DIRECTIVE_WHITESPACES` inside preprocessor directives and a
/// `BYTE_ORDER_MARK` token for a leading BOM — neither is code.
///
/// `SKIPPED_SECTION` is the hidden-channel token the [`CSharpLexerBase`] hooks
/// enqueue for an *inactive* `#if` block's body. Its text is source the
/// compiler never sees, so it must count as neither code nor comment: it is
/// skipped here, leaving those rows to fall out as blank (they carry no
/// semantic weight for any metric).
///
/// Unlike Kotlin, C# has no trivia-folding operator tokens, so no
/// trivia-bearing token scan is needed. The token store is eagerly buffered
/// through EOF, so every token (all channels) is present.
fn collect_loc_tokens(parsed: &ParsedFile) -> Vec<mehen_antlr::LocToken> {
    use mehen_csharp_parser::c_sharp_lexer::{
        BYTE_ORDER_MARK, DELIMITED_COMMENT, DELIMITED_DOC_COMMENT, DIRECTIVE_LINE,
        SINGLE_LINE_COMMENT, SINGLE_LINE_DOC_COMMENT, WHITESPACES,
    };

    mehen_antlr::loc_tokens(
        parsed.tokens(),
        &[
            SINGLE_LINE_COMMENT,
            DELIMITED_COMMENT,
            SINGLE_LINE_DOC_COMMENT,
            DELIMITED_DOC_COMMENT,
        ],
        // A preprocessor directive line is neither code nor comment for LOC:
        // mehen routes directives to their own channel rather than evaluating
        // them, so an inactive `#if` region is still parsed as ordinary code.
        &[WHITESPACES, BYTE_ORDER_MARK, DIRECTIVE_LINE],
        &[],
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
        // Proves the `CSharpLexerBase` hooks are installed: without them the
        // generated lexer fails loud (`--sem-unknown error`) here.
        let src = "class C { string M(int x) { return $\"a{x}b\"; } }\n";
        let a = analyze(src, "C.cs");
        assert!(
            a.diagnostics.is_empty(),
            "interpolated string should lex/parse cleanly, got {:?}",
            a.diagnostics
        );
    }
}
