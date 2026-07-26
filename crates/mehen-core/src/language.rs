// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// The set of languages mehen knows how to identify.
///
/// The enum is intentionally not feature-gated. A variant can exist even
/// when its analyzer crate is disabled in the current build — in that case,
/// `mehen-engine` returns an `AnalyzerUnavailable` diagnostic. This keeps
/// `match` statements stable across feature combinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Php,
    Ruby,
    Rust,
    Go,
    Kotlin,
    Java,
    CSharp,
    PowerShell,
    C,
    Markdown,
    Sql,
}

/// Error returned by [`Language::from_str`] for unknown identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageParseError(String);

impl fmt::Display for LanguageParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown language identifier: `{}`", self.0)
    }
}

impl core::error::Error for LanguageParseError {}

impl Language {
    /// The canonical lowercase identifier used in JSON and CLI output.
    pub fn canonical(&self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Jsx => "jsx",
            Language::Php => "php",
            Language::Ruby => "ruby",
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Kotlin => "kotlin",
            Language::Java => "java",
            Language::CSharp => "csharp",
            Language::PowerShell => "powershell",
            Language::C => "c",
            Language::Markdown => "markdown",
            Language::Sql => "sql",
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical())
    }
}

impl FromStr for Language {
    type Err = LanguageParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Mapping documented in the rewrite plan §4.2.
        let normalized = s.trim().to_ascii_lowercase();
        let lang = match normalized.as_str() {
            "python" | "py" => Language::Python,
            "typescript" | "ts" | "mts" | "cts" => Language::TypeScript,
            "javascript" | "js" | "mjs" | "cjs" => Language::JavaScript,
            "tsx" => Language::Tsx,
            "jsx" => Language::Jsx,
            "php" | "php3" | "php4" | "php5" | "php7" | "php8" | "phtml" => Language::Php,
            "ruby" | "rb" => Language::Ruby,
            "rust" | "rs" => Language::Rust,
            "go" => Language::Go,
            "kotlin" | "kt" | "kts" => Language::Kotlin,
            "java" => Language::Java,
            // `cs` is the file extension; `csx` is a C# script. `c#`/`c-sharp`
            // are accepted spellings a user may type on the CLI.
            "csharp" | "cs" | "csx" | "c#" | "c-sharp" => Language::CSharp,
            "powershell" | "pwsh" | "ps1" | "psm1" | "psd1" => Language::PowerShell,
            "c" | "h" => Language::C,
            "markdown" | "md" | "mdx" | "mdown" | "mkd" | "mkdn" => Language::Markdown,
            "sql" | "ddl" | "dml" => Language::Sql,
            _ => return Err(LanguageParseError(s.to_string())),
        };
        Ok(lang)
    }
}

/// Returns the list of accepted identifiers for a given language. Useful for
/// CLI help text and migration guides.
pub fn language_aliases(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Python => &["python", "py"],
        Language::TypeScript => &["typescript", "ts", "mts", "cts"],
        Language::JavaScript => &["javascript", "js", "mjs", "cjs"],
        Language::Tsx => &["tsx"],
        Language::Jsx => &["jsx"],
        Language::Php => &["php", "php3", "php4", "php5", "php7", "php8", "phtml"],
        Language::Ruby => &["ruby", "rb"],
        Language::Rust => &["rust", "rs"],
        Language::Go => &["go"],
        Language::Kotlin => &["kotlin", "kt", "kts"],
        Language::Java => &["java"],
        Language::CSharp => &["csharp", "cs", "csx", "c#", "c-sharp"],
        Language::PowerShell => &["powershell", "pwsh", "ps1", "psm1", "psd1"],
        Language::C => &["c", "h"],
        Language::Markdown => &["markdown", "md", "mdx", "mdown", "mkd", "mkdn"],
        Language::Sql => &["sql", "ddl", "dml"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_identifiers() {
        for lang in [
            Language::Python,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::Jsx,
            Language::Php,
            Language::Ruby,
            Language::Rust,
            Language::Go,
            Language::Kotlin,
            Language::Java,
            Language::CSharp,
            Language::PowerShell,
            Language::C,
            Language::Markdown,
        ] {
            assert_eq!(lang.canonical().parse::<Language>().unwrap(), lang);
        }
    }

    #[test]
    fn parses_aliases() {
        assert_eq!("py".parse::<Language>().unwrap(), Language::Python);
        assert_eq!("MTS".parse::<Language>().unwrap(), Language::TypeScript);
        assert_eq!("rb".parse::<Language>().unwrap(), Language::Ruby);
        assert_eq!("kts".parse::<Language>().unwrap(), Language::Kotlin);
        assert_eq!("mdx".parse::<Language>().unwrap(), Language::Markdown);
        assert_eq!("cs".parse::<Language>().unwrap(), Language::CSharp);
        assert_eq!("C#".parse::<Language>().unwrap(), Language::CSharp);
    }

    #[test]
    fn rejects_unknown() {
        assert!("klingon".parse::<Language>().is_err());
    }
}
