// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Oxc-backed analyzer entry points for TypeScript / JavaScript / TSX / JSX.
//!
//! The four `LanguageAnalyzer` impls all funnel into a single
//! [`crate::walker::analyze`] call with a different [`SourceType`] —
//! every other piece of behavior (scope detection, decision points,
//! LOC, Halstead) is shared.

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, ParseDiagnostic,
    Result, SourceFile, SourceSpan, byte_offset_clamped,
};
use mehen_metrics::{MetricEvidence, MetricTreeBuilder};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_parser::config::TokensParserConfig;
use oxc_span::{FileExtension, SourceType};

use crate::walker;

/// Refine an analyzer-default [`SourceType`] using the file extension.
///
/// The only purpose of this refinement is to flip the module flavor
/// when the extension explicitly disambiguates it: `.cjs` / `.cts` are
/// CommonJS by spec, `.mjs` / `.mts` are explicitly modules. Without
/// it, the analyzer's `SourceType::mjs()` default would reject valid
/// CommonJS scripts (top-level `return` in a `.cjs` file).
///
/// `--language` is authoritative — when the caller forces a language
/// (`--language typescript file.js`) the refinement does NOT switch
/// languages or JSX flavor. Cross-language extensions are ignored:
/// the analyzer parses the file under the requested language with
/// the analyzer's default module kind. This guarantees that mismatched
/// extensions don't silently report syntax errors under the wrong
/// language. (PR #95 discussion_r3267335273.)
fn refine_source_type(default: SourceType, source: &SourceFile) -> SourceType {
    let Some(ext) = source.path.extension() else {
        return default;
    };
    let Ok(file_ext) = ext.parse::<FileExtension>() else {
        return default;
    };
    let from_ext = SourceType::from(file_ext);
    // Reject any cross-language / cross-JSX-flavor extension — the
    // analyzer's default already encodes both invariants from the
    // requested `Language`. We only adopt `from_ext` when both
    // language family and JSX flavor are identical; otherwise we
    // fall back to the analyzer's default.
    let language_matches = from_ext.is_typescript() == default.is_typescript();
    let jsx_matches = from_ext.is_jsx() == default.is_jsx();
    if language_matches && jsx_matches {
        from_ext
    } else {
        default
    }
}

/// Run the parser and walker, returning a populated `LanguageAnalysis`.
///
/// This is the single dispatch point shared by every flavor of the
/// analyzer. The Oxc parser is constructed with `TokensParserConfig` so
/// the lexer captures every punctuation, keyword, identifier, and
/// literal token — the walker uses the token stream for Halstead
/// operator/operand classification while it walks the AST for everything
/// else (scopes, decision points, ABC, NPA/NPM/WMC, NOM, LOC).
fn analyze_with_source_type(
    language: Language,
    source: &SourceFile,
    source_type: SourceType,
    emit_contributions: bool,
) -> LanguageAnalysis {
    let source_type = refine_source_type(source_type, source);
    let allocator = Allocator::default();
    let parser_return = Parser::new(&allocator, source.text.as_str(), source_type)
        .with_config(TokensParserConfig)
        .parse();

    if parser_return.panicked {
        let span = SourceSpan {
            start_byte: 0,
            end_byte: byte_offset_clamped(source.text.len()),
            start_line: 1,
            end_line: source.line_index.line_count(),
        };
        let builder = MetricTreeBuilder::new(span);
        let diagnostics = vec![ParseDiagnostic::fatal(
            "typescript.parse_error",
            format!(
                "oxc_parser panicked with {} error(s)",
                parser_return.diagnostics.len()
            ),
        )];
        return LanguageAnalysis {
            language,
            backend: AnalysisBackend::Oxc,
            diagnostics,
            root: builder.finish(),
            contributions: Vec::new(),
        };
    }

    // Reason codes are prefixed with the analyzed language's canonical
    // name so a `.tsx` file yields `tsx.*` reasons and a `.js` file
    // `javascript.*` (plan §5.4).
    let mut evidence = MetricEvidence::new(language.canonical(), emit_contributions);
    let root = walker::walk_program(
        &parser_return.program,
        &parser_return.tokens,
        source.text.as_str(),
        &source.line_index,
        &mut evidence,
    );

    // Oxc commonly returns a non-panicking parse with `diagnostics`
    // populated for invalid TS/JS input; surface those as `error`
    // diagnostics so the metric output can't masquerade as clean
    // (plan §9.3). `ParserReturn::diagnostics` (renamed from `errors`
    // in oxc 0.137) is a `Diagnostics` newtype that derefs to
    // `Vec<OxcDiagnostic>`, so `.iter()` / `.message` are unchanged.
    let diagnostics: Vec<ParseDiagnostic> = parser_return
        .diagnostics
        .iter()
        .take(16)
        .map(|err| ParseDiagnostic::error("typescript.syntax_error", err.message.to_string()))
        .collect();

    LanguageAnalysis {
        language,
        backend: AnalysisBackend::Oxc,
        diagnostics,
        root,
        contributions: evidence.finish(),
    }
}

macro_rules! ts_analyzer {
    ($name:ident, $lang:expr, $source_type:expr) => {
        pub struct $name;

        impl $name {
            pub fn new() -> Self {
                Self
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl LanguageAnalyzer for $name {
            fn language(&self) -> Language {
                $lang
            }

            fn backend(&self) -> AnalysisBackend {
                AnalysisBackend::Oxc
            }

            fn analyze(
                &self,
                source: &SourceFile,
                config: &AnalysisConfig,
            ) -> Result<LanguageAnalysis> {
                Ok(analyze_with_source_type(
                    $lang,
                    source,
                    $source_type,
                    config.emit_contributions,
                ))
            }
        }
    };
}

ts_analyzer!(TypeScriptAnalyzer, Language::TypeScript, SourceType::ts());
ts_analyzer!(TsxAnalyzer, Language::Tsx, SourceType::tsx());
ts_analyzer!(JavaScriptAnalyzer, Language::JavaScript, SourceType::mjs());
ts_analyzer!(JsxAnalyzer, Language::Jsx, SourceType::jsx());

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, MetricKey, SourceFile, SpaceKind};
    use mehen_metrics::keys;

    fn analyze_ts(src: &str) -> LanguageAnalysis {
        let analyzer = TypeScriptAnalyzer::new();
        let file = SourceFile::new("a.ts".into(), Language::TypeScript, src.to_string());
        analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
    }

    #[test]
    fn function_creates_function_space() {
        let a = analyze_ts("function foo() { return 1; }");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Function);
        assert_eq!(a.root.spaces[0].name.as_deref(), Some("foo"));
    }

    #[test]
    fn class_creates_class_space() {
        let a = analyze_ts("class C { m() { return 1; } }");
        assert_eq!(a.root.spaces.len(), 1);
        assert_eq!(a.root.spaces[0].kind, SpaceKind::Class);
        assert_eq!(a.root.spaces[0].spaces.len(), 1);
    }

    #[test]
    fn cyclomatic_counts_decision_points() {
        let a = analyze_ts("function f(x) { if (x && x) return 1; return 2; }");
        let cy = a.root.spaces[0]
            .metrics
            .get(&MetricKey::new(keys::CYCLOMATIC))
            .unwrap()
            .as_f64();
        assert!(cy >= 3.0, "expected >= 3, got {cy}");
    }

    /// `.cjs` files use CommonJS semantics — top-level `return` is
    /// valid (Node wraps the script in an immediately-invoked function).
    /// Forcing `SourceType::mjs()` would emit a syntax error and exit
    /// the CLI non-zero. See PR #95 review comment 3265424682.
    #[test]
    fn cjs_top_level_return_parses_clean() {
        let analyzer = JavaScriptAnalyzer::new();
        let file = SourceFile::new(
            "a.cjs".into(),
            Language::JavaScript,
            "return 42;\n".to_string(),
        );
        let a = analyzer.analyze(&file, &AnalysisConfig::default()).unwrap();
        assert!(
            a.diagnostics.is_empty(),
            "expected clean parse, got {:?}",
            a.diagnostics
        );
    }

    /// `.mjs` keeps explicit module parsing — top-level `return` is
    /// invalid in modules and must surface as a syntax error.
    #[test]
    fn mjs_top_level_return_is_diagnostic() {
        let analyzer = JavaScriptAnalyzer::new();
        let file = SourceFile::new(
            "a.mjs".into(),
            Language::JavaScript,
            "return 42;\n".to_string(),
        );
        let a = analyzer.analyze(&file, &AnalysisConfig::default()).unwrap();
        assert!(
            !a.diagnostics.is_empty(),
            "expected at least one diagnostic for top-level return in module"
        );
    }

    /// `.cts` is the TypeScript counterpart of `.cjs` (CommonJS module
    /// kind per Oxc's `FileExtension::Cts`). Top-level `return` should
    /// parse without diagnostics.
    #[test]
    fn cts_top_level_return_parses_clean() {
        let analyzer = TypeScriptAnalyzer::new();
        let file = SourceFile::new(
            "a.cts".into(),
            Language::TypeScript,
            "return 42;\n".to_string(),
        );
        let a = analyzer.analyze(&file, &AnalysisConfig::default()).unwrap();
        assert!(
            a.diagnostics.is_empty(),
            "expected clean parse, got {:?}",
            a.diagnostics
        );
    }

    /// Regression: `--language` is authoritative. PR #95
    /// discussion_r3267335273 — when a caller forces TypeScript on a
    /// `.js` extension, the refinement must NOT downgrade the
    /// analyzer to JavaScript; TS-only syntax (a type alias) has to
    /// parse cleanly.
    #[test]
    fn typescript_language_with_js_extension_keeps_typescript_parser() {
        let analyzer = TypeScriptAnalyzer::new();
        let file = SourceFile::new(
            "weird.js".into(),
            Language::TypeScript,
            "type Id = number;\nconst x: Id = 1;\n".to_string(),
        );
        let a = analyzer.analyze(&file, &AnalysisConfig::default()).unwrap();
        assert!(
            a.diagnostics.is_empty(),
            "TS-only syntax must parse under --language typescript regardless of file extension, got {:?}",
            a.diagnostics
        );
    }

    /// Regression: same direction, opposite refinement.
    /// `--language javascript file.ts` must NOT promote the parser to
    /// TypeScript; type-annotation syntax should be reported as a JS
    /// syntax error, not silently accepted.
    #[test]
    fn javascript_language_with_ts_extension_keeps_javascript_parser() {
        let analyzer = JavaScriptAnalyzer::new();
        let file = SourceFile::new(
            "weird.ts".into(),
            Language::JavaScript,
            "const x: number = 1;\n".to_string(),
        );
        let a = analyzer.analyze(&file, &AnalysisConfig::default()).unwrap();
        assert!(
            !a.diagnostics.is_empty(),
            "JS parser must reject `: number` annotation when --language javascript is forced",
        );
    }

    /// Regression: TSX flavor must not be cross-mapped from a `.tsx`
    /// path when the analyzer's default is plain TS. Without the
    /// JSX-flavor guard the `.tsx` extension would override
    /// `SourceType::ts()` and accept JSX syntax under
    /// `--language typescript`.
    #[test]
    fn typescript_language_with_tsx_extension_keeps_ts_flavor() {
        let analyzer = TypeScriptAnalyzer::new();
        // A `<Foo />` JSX element is a syntax error in plain TS but
        // parses fine in TSX. The refinement must not silently switch
        // to TSX based on the extension.
        let file = SourceFile::new(
            "weird.tsx".into(),
            Language::TypeScript,
            "const el = <Foo />;\n".to_string(),
        );
        let a = analyzer.analyze(&file, &AnalysisConfig::default()).unwrap();
        assert!(
            !a.diagnostics.is_empty(),
            "TS parser must reject JSX syntax when --language typescript is forced"
        );
    }
}
