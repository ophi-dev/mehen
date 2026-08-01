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

use mehen_antlr::DiagnosticCollector;
use mehen_antlr::runtime::{CommonTokenStream, InputStream, ParsedFile};
use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, LineIndex,
    ParseDiagnostic, Result, SourceFile, SourceSpan, byte_offset_clamped,
};

use mehen_java_parser::hooks::JavaParserBase;
use mehen_java_parser::java_lexer::JavaLexer;
use mehen_java_parser::java_parser::JavaParser;

pub struct JavaAnalyzer;

/// A recovered parse: the flat-arena [`ParsedFile`] owns the token store and
/// CST storage, and the walker borrows [`Node`](mehen_antlr::runtime::Node)
/// views from it. `loc_tokens` is precomputed from the (eagerly buffered,
/// hidden-channel-inclusive) token store.
struct ParsedJava {
    parsed: ParsedFile,
    lexer_diagnostics: Vec<ParseDiagnostic>,
    loc_tokens: Vec<mehen_antlr::LocToken>,
}

impl JavaAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Parse `source` via the single `compilationUnit` entry rule and return
    /// the recovered [`ParsedFile`] plus the source-ordered LOC token list.
    /// Returns `None` only if the rule call hard-fails (returns `Err` rather
    /// than a recovered tree).
    ///
    /// The parser is constructed with the [`JavaParserBase`] typed hooks — the
    /// exact port of the grammar's `superClass` predicates. The generated
    /// modules use `--sem-unknown error`, so a hook-less parser would hard-fail
    /// (not mis-parse) on inputs reaching those predicates.
    ///
    /// Replaces the runtime's default lexer console listener with a structured
    /// diagnostic collector and removes the parser console listener.
    fn parse(&self, source: &str, line_index: &LineIndex) -> Option<ParsedJava> {
        let mut lexer = JavaLexer::new(InputStream::new(source));
        lexer.remove_error_listeners();
        let lexer_diagnostics = DiagnosticCollector::default();
        lexer.add_error_listener(lexer_diagnostics.clone());
        let tokens = CommonTokenStream::new(lexer);
        let mut parser = JavaParser::with_typed_hooks(tokens, JavaParserBase);
        parser.remove_error_listeners();
        let result = parser.compilation_unit().ok()?;
        let lexer_diagnostics = lexer_diagnostics.diagnostics("java.syntax_error", 16, line_index);

        // `into_parsed_file` consumes the parser and moves the eagerly-buffered
        // token store into the `ParsedFile`; the LOC token list is then read
        // straight from that store (all channels, so hidden-channel comments
        // are present — no `fill()` step needed).
        let parsed = parser.into_parsed_file(result);
        let loc_tokens = collect_loc_tokens(&parsed);
        Some(ParsedJava {
            parsed,
            lexer_diagnostics,
            loc_tokens,
        })
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
            "java.syntax_error",
            remaining,
            &line_index,
        ));

        Ok(LanguageAnalysis {
            language: Language::Java,
            backend: AnalysisBackend::Antlr,
            diagnostics,
            root,
            contributions: Vec::new(),
        })
    }
}

/// Classify the parsed file's token store into the source-ordered LOC token
/// list that drives the LOC family. Java comments are `COMMENT` (block, may
/// span lines) and `LINE_COMMENT`; whitespace is `WS`. Unlike Kotlin, Java
/// has no string-mode comment tokens and no trivia-folding operator tokens
/// (annotations are a plain `AT` token followed by a name), so no
/// trivia-bearing token scan is needed. The token store is eagerly buffered
/// through EOF, so every token (all channels) is present.
fn collect_loc_tokens(parsed: &ParsedFile) -> Vec<mehen_antlr::LocToken> {
    use mehen_java_parser::java_lexer::{COMMENT, LINE_COMMENT, WS};

    mehen_antlr::loc_tokens(
        // Since the 0.15 runtime `&TokenStore` is `IntoIterator` (issue #123),
        // so the eagerly-buffered store feeds the LOC sweep directly — no
        // hand-rolled index loop.
        parsed.tokens(),
        &[COMMENT, LINE_COMMENT],
        &[WS],
        &[],
    )
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

    #[test]
    fn recovered_error_nodes_carry_byte_spans() {
        // Since the 0.19 runtime the offending token reaches the diagnostic
        // path (upstream #196), so a recovered parse error must report a real
        // byte range rather than only a line number in its message.
        let src = "class C {\n    void m() { int x = @@@; }\n}\n";
        let a = analyze(src, "C.java");
        let spanned: Vec<_> = a
            .diagnostics
            .iter()
            .filter_map(|d| d.span.as_ref())
            .collect();
        assert!(
            !spanned.is_empty(),
            "recovered error nodes should carry spans, got {:?}",
            a.diagnostics
        );
        for span in spanned {
            assert!(
                span.end_byte > span.start_byte,
                "span must be non-empty: {span:?}"
            );
            assert!(
                (span.end_byte as usize) <= src.len(),
                "span must stay inside the source: {span:?}"
            );
            assert_eq!(span.start_line, 2, "the bad token is on line 2");
        }
    }
}
