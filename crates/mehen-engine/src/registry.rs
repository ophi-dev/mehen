// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use mehen_core::{Language, LanguageAnalyzer};

/// Registry that maps a `Language` to its analyzer.
///
/// Per the rewrite plan §4.6, analyzers are constructed per worker (or per
/// analyze call) — they never share parser instances across threads. The
/// registry holds *factory* trait objects so each `analyzer_for` call hands
/// the caller a fresh analyzer struct to drive a single source file.
///
/// In 1.0 the analyzer structs are stateless (Phase 1 tree-sitter
/// placeholders); Phase 7+ may switch them to arena-backed parsers, at
/// which point the same registry shape continues to work because
/// `LanguageAnalysis` is owned and `Send + 'static`.
pub struct AnalyzerRegistry {
    entries: Vec<RegistryEntry>,
}

struct RegistryEntry {
    language: Language,
    factory: AnalyzerFactory,
}

type AnalyzerFactory = Box<dyn Fn() -> Box<dyn LanguageAnalyzer> + Send + Sync>;

#[derive(Debug)]
pub enum RegistryError {
    DuplicateLanguage(Language),
}

impl AnalyzerRegistry {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn register<F>(&mut self, language: Language, factory: F) -> Result<(), RegistryError>
    where
        F: Fn() -> Box<dyn LanguageAnalyzer> + Send + Sync + 'static,
    {
        if self.entries.iter().any(|e| e.language == language) {
            return Err(RegistryError::DuplicateLanguage(language));
        }
        self.entries.push(RegistryEntry {
            language,
            factory: Box::new(factory),
        });
        Ok(())
    }

    /// Returns a freshly-constructed analyzer for `language`, or `None` if
    /// no analyzer is registered (e.g. the owning crate is feature-gated
    /// off in this build).
    pub fn analyzer_for(&self, language: Language) -> Option<Box<dyn LanguageAnalyzer>> {
        self.entries
            .iter()
            .find(|e| e.language == language)
            .map(|e| (e.factory)())
    }

    /// Whether an analyzer is registered for `language`, without
    /// constructing one. Used by `mehen.toml` validation: a static
    /// threshold for a language this build cannot analyze is a gate
    /// that can never fire.
    pub fn has_analyzer_for(&self, language: Language) -> bool {
        self.entries.iter().any(|e| e.language == language)
    }

    /// Default registry assembling every analyzer enabled by feature flags.
    ///
    /// Also registers the Markdown embedded-code dispatcher
    /// (idempotent — backed by `OnceLock` inside `mehen-markdown`).
    /// Without this, library callers that use
    /// `analyze_metrics`/`analyze_diff`/`rank_top_offenders` directly
    /// would receive `0.0` for every fenced-code complexity term —
    /// `embedded_code::analyze_fence` returns zero whenever no
    /// dispatch function is set, and only the CLI binary used to call
    /// `init_markdown()`. See PR #95 review and the
    /// `default_set_initializes_markdown_dispatch` test below.
    pub fn default_set() -> Self {
        let mut registry = Self::new();
        register_default_analyzers(&mut registry);
        crate::init_markdown();
        registry
    }
}

impl Default for AnalyzerRegistry {
    fn default() -> Self {
        Self::default_set()
    }
}

fn register_default_analyzers(registry: &mut AnalyzerRegistry) {
    #[cfg(feature = "lang-python")]
    {
        let _ = registry.register(Language::Python, || {
            Box::new(mehen_python::PythonAnalyzer::new())
        });
    }
    #[cfg(feature = "lang-typescript")]
    {
        let _ = registry.register(Language::TypeScript, || {
            Box::new(mehen_typescript::TypeScriptAnalyzer::new())
        });
        let _ = registry.register(Language::JavaScript, || {
            Box::new(mehen_typescript::JavaScriptAnalyzer::new())
        });
        let _ = registry.register(Language::Tsx, || {
            Box::new(mehen_typescript::TsxAnalyzer::new())
        });
        let _ = registry.register(Language::Jsx, || {
            Box::new(mehen_typescript::JsxAnalyzer::new())
        });
    }
    #[cfg(feature = "lang-php")]
    {
        let _ = registry.register(Language::Php, || Box::new(mehen_php::PhpAnalyzer::new()));
    }
    #[cfg(feature = "lang-ruby")]
    {
        let _ = registry.register(Language::Ruby, || Box::new(mehen_ruby::RubyAnalyzer::new()));
    }
    #[cfg(feature = "lang-rust")]
    {
        let _ = registry.register(Language::Rust, || Box::new(mehen_rust::RustAnalyzer::new()));
    }
    #[cfg(feature = "lang-go")]
    {
        let _ = registry.register(Language::Go, || Box::new(mehen_go::GoAnalyzer::new()));
    }
    #[cfg(feature = "lang-c")]
    {
        let _ = registry.register(Language::C, || Box::new(mehen_c::CAnalyzer::new()));
    }
    #[cfg(feature = "lang-kotlin")]
    {
        let _ = registry.register(Language::Kotlin, || {
            Box::new(mehen_kotlin::KotlinAnalyzer::new())
        });
    }
    #[cfg(feature = "lang-java")]
    {
        let _ = registry.register(Language::Java, || Box::new(mehen_java::JavaAnalyzer::new()));
    }
    #[cfg(feature = "lang-csharp")]
    {
        let _ = registry.register(Language::CSharp, || {
            Box::new(mehen_csharp::CSharpAnalyzer::new())
        });
    }
    #[cfg(feature = "lang-powershell")]
    {
        let _ = registry.register(Language::PowerShell, || {
            Box::new(mehen_powershell::PowerShellAnalyzer::new())
        });
    }
    #[cfg(feature = "lang-sql")]
    {
        let _ = registry.register(Language::Sql, || Box::new(mehen_sql::SqlAnalyzer::new()));
    }
    {
        let _ = registry.register(Language::Markdown, || {
            Box::new(mehen_markdown::MarkdownAnalyzer::new())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, MetricKey, SourceFile};

    /// Library callers (anyone using `analyze_metrics`/`analyze_diff`/
    /// `rank_top_offenders` directly without invoking
    /// `mehen_engine::init_markdown` first) must still get real
    /// embedded-fence metrics. `default_set` now wires the Markdown
    /// dispatcher itself; without that fix, the assertion below
    /// regresses to `0.0`.
    #[test]
    #[cfg(all(feature = "lang-python", feature = "lang-c"))]
    fn default_set_initializes_markdown_dispatch() {
        let registry = AnalyzerRegistry::default_set();
        let analyzer = registry
            .analyzer_for(Language::Markdown)
            .expect("Markdown analyzer registered");

        // Markdown source with one fenced Python block and one
        // fenced C block. Both languages have analyzers in the
        // registry, so the Markdown embedded-code dispatcher should
        // route the bodies through them and surface a non-zero
        // Halstead-derived `embedded_volume`.
        let source = "# Heading\n\n\
                      Text before code.\n\n\
                      ```python\n\
                      def add(a, b):\n    \
                          return a + b\n\
                      ```\n\n\
                      ```c\n\
                      int add(int a, int b) { return a + b; }\n\
                      ```\n";
        let file = SourceFile::new("doc.md".into(), Language::Markdown, source.to_string());
        let analysis = analyzer
            .analyze(&file, &AnalysisConfig::default())
            .expect("Markdown analysis succeeds");
        let key = MetricKey::new("markdown.halstead.embedded_volume");
        let value = analysis
            .root
            .metrics
            .get(&key)
            .map(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!(
            value > 0.0,
            "library callers using AnalyzerRegistry::default_set() must see \
             non-zero embedded fence metrics; got embedded_volume={value} \
             — did register_default_analyzers() forget to register the \
             Markdown dispatcher? See PR #95 review."
        );
    }
}
