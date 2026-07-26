// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use camino::Utf8Path;

use mehen_core::Language;

/// Detect a `Language` from a path's extension.
///
/// 1.0 detection rules (rewrite plan §4.2):
/// - `.py` → Python (no `.pyi` until Phase 6 explicitly adds stub fixtures);
/// - `.ts/.mts/.cts` → TypeScript; `.js/.mjs/.cjs` → JavaScript;
/// - `.tsx` → TSX; `.jsx` → JSX (split out from TS in 1.0);
/// - `.md/.mdx` (and legacy variants) → Markdown;
/// - `.sql/.ddl/.dml` → SQL.
pub fn detect_language(path: &Utf8Path) -> Option<Language> {
    let ext = path.extension()?.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "py" => Language::Python,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "js" | "mjs" | "cjs" => Language::JavaScript,
        "tsx" => Language::Tsx,
        "jsx" => Language::Jsx,
        "rs" => Language::Rust,
        "go" => Language::Go,
        "rb" => Language::Ruby,
        "kt" | "kts" => Language::Kotlin,
        "java" => Language::Java,
        // `.csx` is a C# script file (dotnet-script / `csi`); it shares the
        // compilation-unit grammar, so it routes to the same analyzer.
        "cs" | "csx" => Language::CSharp,
        "ps1" | "psm1" | "psd1" => Language::PowerShell,
        "c" | "h" => Language::C,
        "php" | "php3" | "php4" | "php5" | "php7" | "php8" | "phtml" => Language::Php,
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx" => Language::Markdown,
        "sql" | "ddl" | "dml" => Language::Sql,
        _ => return None,
    };
    Some(lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_extensions() {
        assert_eq!(
            detect_language(Utf8Path::new("foo/bar.py")),
            Some(Language::Python)
        );
        assert_eq!(
            detect_language(Utf8Path::new("FOO.MTS")),
            Some(Language::TypeScript)
        );
        assert_eq!(detect_language(Utf8Path::new("a.tsx")), Some(Language::Tsx));
        assert_eq!(
            detect_language(Utf8Path::new("README.MD")),
            Some(Language::Markdown)
        );
    }

    #[test]
    fn returns_none_for_unknown() {
        assert_eq!(detect_language(Utf8Path::new("file.xyz")), None);
        assert_eq!(detect_language(Utf8Path::new("Makefile")), None);
    }
}
