// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-java` — Java language analyzer.
//!
//! Java is parsed by a parser generated from the community-maintained
//! **grammars-v4 Java grammar** (`antlr/grammars-v4`, vendored in `grammar/`)
//! running on the ANTLR Rust runtime via [`mehen_antlr`]. The grammar covers
//! modern Java (records, sealed types, switch expressions, text blocks,
//! pattern matching, modules), giving the metric walker a semantically-named
//! CST (`classDeclaration`, `methodDeclaration`, `recordDeclaration`,
//! `switchExpression`, `lambdaExpression`, …).
//!
//! The generated lexer/parser modules live in [`generated`]; they are
//! produced by `cargo run -p xtask -- antlr generate java` and checked in
//! verbatim (see `src/generated/README.md`). They are not hand-edited and are
//! self-contained generated modules with their own lint and formatting
//! attributes.
//!
//! Metric coverage follows SonarJava's definitions where they exist; see
//! [`walker`] for the per-metric table.

#![forbid(unsafe_code)]

mod walker;

/// ANTLR-generated Java lexer and parser.
///
/// Regenerate with `cargo run -p xtask -- antlr generate java` — never
/// hand-edit.
mod generated {
    pub(crate) mod java_lexer;
    pub(crate) mod java_parser;
}

use mehen_antlr::runtime::{CommonToken, CommonTokenStream, InputStream, ParseTree};
use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, LineIndex,
    ParseDiagnostic, Result, SourceFile, SourceSpan, byte_offset_clamped,
};

use generated::java_lexer::JavaLexer;
use generated::java_parser::JavaParser;

pub struct JavaAnalyzer;

struct ParsedJava {
    tree: ParseTree,
    loc_tokens: Vec<mehen_antlr::LocToken>,
}

impl JavaAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Parse `source` via the single `compilationUnit` entry rule and return
    /// the recovered tree plus the source-ordered LOC token list. Returns
    /// `None` only if the rule call hard-fails (returns `Err` rather than a
    /// recovered tree).
    fn parse(&self, source: &str) -> Option<ParsedJava> {
        let lexer = JavaLexer::new(InputStream::new(source));
        let tokens = CommonTokenStream::new(lexer);
        let mut parser = JavaParser::new(tokens);
        let tree = parser.compilation_unit().ok()?;
        let mut tokens = parser.into_token_stream();
        tokens.fill();
        let loc_tokens = collect_loc_tokens(tokens.tokens());
        Some(ParsedJava { tree, loc_tokens })
    }
}

impl Default for JavaAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for JavaAnalyzer {
    fn language(&self) -> Language {
        Language::Java
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
                    language: Language::Java,
                    backend: AnalysisBackend::Antlr,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "java.parse_error",
                        "java ANTLR parse failed".to_string(),
                    )],
                    root: mehen_antlr::empty_space(span),
                    contributions: Vec::new(),
                });
            }
        };

        let root = walker::walk(
            &parsed.tree,
            &line_index,
            source.text.len(),
            &parsed.loc_tokens,
        );

        // Recovered ANTLR error nodes are surfaced as `error` so the
        // diagnostic contract treats the analysis as incomplete.
        let diagnostics = mehen_antlr::collect_errors(&parsed.tree, "java.syntax_error", 16);

        Ok(LanguageAnalysis {
            language: Language::Java,
            backend: AnalysisBackend::Antlr,
            diagnostics,
            root,
            contributions: Vec::new(),
        })
    }
}

/// Classify the parser-owned token stream into the source-ordered LOC token
/// list that drives the LOC family. Java comments are `COMMENT` (block, may
/// span lines) and `LINE_COMMENT`; whitespace is `WS`. Unlike Kotlin, Java
/// has no string-mode comment tokens and no trivia-folding operator tokens
/// (annotations are a plain `AT` token followed by a name), so no
/// trivia-bearing token scan is needed.
fn collect_loc_tokens(tokens: &[CommonToken]) -> Vec<mehen_antlr::LocToken> {
    use generated::java_lexer::{COMMENT, LINE_COMMENT, WS};

    mehen_antlr::loc_tokens(tokens, &[COMMENT, LINE_COMMENT], &[WS], &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, SourceFile, SpaceKind};

    fn analyze(source: &str, path: &str) -> LanguageAnalysis {
        let analyzer = JavaAnalyzer::new();
        let file = SourceFile::new(path.into(), Language::Java, source.to_string());
        analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
    }

    #[test]
    fn empty_file_yields_root_unit() {
        let a = analyze("", "Empty.java");
        assert_eq!(a.root.kind, SpaceKind::Unit);
        assert!(a.root.spaces.is_empty());
    }

    #[test]
    fn class_with_method_parses_cleanly() {
        let src = "package demo;\n\nclass C {\n    int m() { return 1; }\n}\n";
        let a = analyze(src, "C.java");
        assert!(
            a.diagnostics.is_empty(),
            "compilation unit should parse cleanly, got {}",
            a.diagnostics.len()
        );
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("C"));
    }
}
