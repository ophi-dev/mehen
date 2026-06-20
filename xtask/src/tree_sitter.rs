//! Tree-sitter kind-enum generator.
//!
//! Per rewrite plan §6.7, each per-language analyzer crate owns its
//! generated `grammar.rs` kind enum. xtask is the single tool that
//! produces those files: `cargo xtask tree-sitter generate <language>`
//! writes one crate's grammar, and `cargo xtask tree-sitter
//! check-generated` regenerates every grammar to a tempdir and exits
//! non-zero if the result diverges from what's checked in.
//!
//! Each entry in [`TARGETS`] reaches its grammar through the analyzer
//! crate's `__grammar_language()` accessor (see e.g.
//! `mehen_go::__grammar_language`). That makes the analyzer crate the
//! sole owner of the grammar pin: dependabot bumping
//! `tree-sitter-go = "=0.25.x"` in `crates/mehen-go/Cargo.toml`
//! propagates to xtask through the `mehen-go` path dep, so kind
//! ordinals always match the grammar the analyzer links at runtime.
//! Markdown is not listed here because `mehen-markdown` is backed by
//! pulldown-cmark; Kotlin is not listed because it moved to an ANTLR
//! grammar (`xtask antlr generate kotlin`).
//!
//! The generator itself is a small askama template (see
//! `xtask/templates/grammar.rs`); the heavy lifting is the kind-name
//! sanitization in [`token_names`].

use std::collections::BTreeMap;
use std::collections::hash_map::{Entry, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use askama::Template;
use tree_sitter::Language;

/// One per-crate target understood by `xtask tree-sitter generate <slug>`.
///
/// Each entry pairs the slug a developer types on the command line with
/// the `Language` instance the kind-enum is generated from and the
/// CamelCase name the enum gets in source. The `crate_dir` field is the
/// owning crate's `src/` directory relative to the workspace root; the
/// generator writes `grammar.rs` into it.
pub(crate) struct GeneratorTarget {
    pub slug: &'static str,
    pub enum_name: &'static str,
    pub crate_dir: &'static str,
    pub language: fn() -> Language,
}

/// Every grammar that has a checked-in `grammar.rs` consumed by an
/// analyzer crate. Order matters for `--list` output and for the loop
/// in `check_generated`.
pub(crate) const TARGETS: &[GeneratorTarget] = &[
    GeneratorTarget {
        slug: "c",
        enum_name: "C",
        crate_dir: "crates/mehen-c/src",
        language: mehen_c::__grammar_language,
    },
    GeneratorTarget {
        slug: "go",
        enum_name: "Go",
        crate_dir: "crates/mehen-go/src",
        language: mehen_go::__grammar_language,
    },
    // Kotlin is no longer tree-sitter-backed — it moved to an ANTLR
    // grammar (see `xtask antlr generate kotlin` and `crates/mehen-kotlin`).
];

/// Resolve a generator target by slug. Returns `None` for unknown slugs;
/// the caller is expected to surface a friendly error listing the known
/// slugs.
pub(crate) fn target_for(slug: &str) -> Option<&'static GeneratorTarget> {
    TARGETS.iter().find(|t| t.slug == slug)
}

#[derive(Template)]
#[template(path = "grammar.rs", escape = "none")]
struct GrammarTemplate {
    c_name: String,
    names: Vec<(String, bool, String)>,
}

/// Render the grammar text for a target. Pure — the same `target` always
/// yields the same string, which is what makes `check-generated` work.
///
/// The rendered output is piped through `rustfmt --emit=stdout
/// --edition=2024` so the checked-in `grammar.rs` files match what
/// `cargo fmt` would produce. The legacy `recreate-grammars.sh` ran
/// `cargo fmt` as a separate step after codegen, but inline-formatting
/// here means `check-generated` doesn't need a separate `cargo fmt
/// --check` pass — the render itself enforces formatting parity.
pub(crate) fn render_grammar(target: &GeneratorTarget) -> String {
    let language = (target.language)();
    let names = token_names(&language);
    let template = GrammarTemplate {
        c_name: target.enum_name.to_string(),
        names,
    };
    let raw = template.render().expect("askama render failed");
    rustfmt(&raw).unwrap_or(raw)
}

/// Pipe `source` through `rustfmt --emit=stdout --edition=2024`.
///
/// Returns `None` on any failure (rustfmt missing, non-zero exit, write
/// error) so the caller can fall back to the unformatted text. xtask is
/// developer tooling and the formatter ships with every Rust toolchain
/// the project supports, so the fallback is a defensive measure rather
/// than an expected path.
fn rustfmt(source: &str) -> Option<String> {
    let mut child = Command::new("rustfmt")
        .arg("--emit=stdout")
        .arg("--edition=2024")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        None
    }
}

/// Generate `grammar.rs` for one target, writing it to
/// `<workspace>/<crate_dir>/grammar.rs`.
pub(crate) fn generate(workspace: &Path, target: &GeneratorTarget) -> std::io::Result<PathBuf> {
    let dir = workspace.join(target.crate_dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join("grammar.rs");
    fs::write(&path, render_grammar(target))?;
    Ok(path)
}

/// Locate the workspace root. Walks up from CWD until a `Cargo.toml`
/// declaring `[workspace]` is found. xtask is launched via
/// `cargo xtask` so this should hit the repo root immediately, but the
/// walk keeps the binary usable from a subdirectory.
pub(crate) fn workspace_root() -> std::io::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)?;
            if text.contains("[workspace]") {
                return Ok(dir.to_path_buf());
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no [workspace] Cargo.toml found above the current directory",
                ));
            }
        }
    }
}

/// Build the `(variant_name, is_disambiguated, grammar_name)` table for
/// every kind in `language`, plus the synthetic `Error` sentinel. The
/// algorithm matches the legacy `enums::common::get_token_names` so
/// regenerating doesn't reshuffle ordinals.
///
/// Order: named tokens first, then anonymous, both sorted by node-kind
/// id. Ordinals in the rendered enum are `loop.index0` over this Vec,
/// which means each variant's discriminant is its position here, not its
/// tree-sitter node-kind id.
fn token_names(language: &Language) -> Vec<(String, bool, String)> {
    let count = language.node_kind_count();
    let mut by_id: BTreeMap<usize, (String, bool, String)> = BTreeMap::new();
    let mut name_count: HashMap<String, usize> = HashMap::new();

    for anon in &[false, true] {
        for i in 0..count {
            let anonymous = !language.node_kind_is_named(i as u16);
            if anonymous != *anon {
                continue;
            }
            let kind = language.node_kind_for_id(i as u16).unwrap();
            let sanitized = sanitize_identifier(kind);
            let camel = camel_case(&sanitized);
            let ts_name = sanitize_string(kind);
            let entry = match name_count.entry(camel.clone()) {
                Entry::Occupied(mut e) => {
                    *e.get_mut() += 1;
                    (format!("{}{}", camel, e.get()), true, ts_name)
                }
                Entry::Vacant(e) => {
                    e.insert(1);
                    (camel, false, ts_name)
                }
            };
            by_id.insert(i, entry);
        }
    }

    let mut names: Vec<(String, bool, String)> = by_id.into_values().collect();
    // tree-sitter's ERROR sentinel is always appended. If a grammar also
    // defines an anonymous token whose sanitized name collides with `Error`,
    // suffix the explicit sentinel so both variants compile.
    let sentinel = if names.iter().any(|(n, _, _)| n == "Error") {
        "ErrorSentinel".to_string()
    } else {
        "Error".to_string()
    };
    names.push((sentinel, false, "ERROR".to_string()));
    names
}

/// Map a grammar's kind-name into a Rust identifier. Mirrors
/// `enums::common::sanitize_identifier`. Punctuation maps to mnemonic
/// names (e.g. `+` → `PLUS`); leading-digit names get an `N` prefix so
/// they parse as identifiers.
fn sanitize_identifier(name: &str) -> String {
    if name == "\u{feff}" {
        return "BOM".to_string();
    }
    if name == "_" {
        return "UNDERSCORE".to_string();
    }
    if name == "self" {
        return "Zelf".to_string();
    }
    if name == "Self" {
        return "SELF".to_string();
    }
    // A token composed solely of underscores survives the loop below as
    // `__`, which then collapses to an empty identifier in `camel_case`.
    // Map such names to a run of `UNDERSCORE` tokens joined with `_` so
    // each contributes a word boundary and the generated variant compiles.
    if !name.is_empty() && name.chars().all(|c| c == '_') {
        return std::iter::repeat_n("UNDERSCORE", name.len())
            .collect::<Vec<_>>()
            .join("_");
    }

    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
            result.push(c);
        } else {
            let replacement = match c {
                '~' => "TILDE",
                '`' => "BQUOTE",
                '!' => "BANG",
                '@' => "AT",
                '#' => "HASH",
                '$' => "DOLLAR",
                '%' => "PERCENT",
                '^' => "CARET",
                '&' => "AMP",
                '*' => "STAR",
                '(' => "LPAREN",
                ')' => "RPAREN",
                '-' => "DASH",
                '+' => "PLUS",
                '=' => "EQ",
                '{' => "LBRACE",
                '}' => "RBRACE",
                '[' => "LBRACK",
                ']' => "RBRACK",
                '\\' => "BSLASH",
                '|' => "PIPE",
                ':' => "COLON",
                ';' => "SEMI",
                '"' => "DQUOTE",
                '\'' => "SQUOTE",
                '<' => "LT",
                '>' => "GT",
                ',' => "COMMA",
                '.' => "DOT",
                '?' => "QMARK",
                '/' => "SLASH",
                '\n' => "LF",
                '\r' => "CR",
                '\t' => "TAB",
                _ => continue,
            };
            if !result.is_empty() && !result.ends_with('_') {
                result.push('_');
            }
            result += replacement;
        }
    }

    // If every character was unmapped (e.g. Unicode symbols like `·`),
    // fall back to a codepoint-based identifier so the variant compiles.
    if result.is_empty() {
        if name.is_empty() {
            result = "EMPTY".to_string();
        } else {
            result = name
                .chars()
                .map(|c| format!("U{:04X}", c as u32))
                .collect::<Vec<_>>()
                .join("_");
        }
    }

    // Rust identifiers cannot start with a digit. Some grammars (e.g.
    // PowerShell's `2>`, `3>&1` redirection operators) expose tokens
    // whose first character is a digit; prefix with `N` so the
    // generated variant compiles.
    if result.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        result.insert(0, 'N');
    }

    result
}

/// Escape a grammar kind-name for embedding in the `&'static str`
/// constant table. Unlike the legacy two-mode `sanitize_string`, the
/// xtask generator only writes one form (the unescaped variant); the
/// JSON / Go output paths the legacy crate carried are gone.
fn sanitize_string(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '"' => result += "\\\"",
            '\\' => result += "\\\\",
            '\t' => result += "\\t",
            '\n' => result += "\\n",
            '\r' => result += "\\r",
            other => result.push(other),
        }
    }
    result
}

/// Capitalize the first letter and uppercase every character following
/// an underscore (which is then dropped). `kind_name` → `KindName`.
fn camel_case(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut cap = true;
    for c in name.chars() {
        if c == '_' {
            cap = true;
        } else if cap {
            result.extend(c.to_uppercase());
            cap = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Compare every checked-in `grammar.rs` against a fresh render. Returns
/// the list of diverging targets so the caller can print them and exit
/// non-zero. An empty Vec means everything is up to date.
pub(crate) fn check_generated(workspace: &Path) -> std::io::Result<Vec<&'static GeneratorTarget>> {
    let mut drifted = Vec::new();
    for target in TARGETS {
        let path = workspace.join(target.crate_dir).join("grammar.rs");
        let actual = fs::read_to_string(&path)?;
        let expected = render_grammar(target);
        if actual != expected {
            drifted.push(target);
        }
    }
    Ok(drifted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_handles_underscores() {
        assert_eq!(camel_case("function_definition"), "FunctionDefinition");
        assert_eq!(camel_case("ifStmt"), "IfStmt");
        assert_eq!(camel_case(""), "");
    }

    #[test]
    fn sanitize_identifier_maps_punctuation() {
        assert_eq!(sanitize_identifier("+"), "PLUS");
        assert_eq!(sanitize_identifier("=="), "EQ_EQ");
        assert_eq!(sanitize_identifier("foo_bar"), "foo_bar");
        assert_eq!(sanitize_identifier("self"), "Zelf");
        assert_eq!(sanitize_identifier("__"), "UNDERSCORE_UNDERSCORE");
    }

    #[test]
    fn sanitize_identifier_prefixes_leading_digits() {
        assert_eq!(sanitize_identifier("2>"), "N2_GT");
    }

    #[test]
    fn target_for_known_slugs() {
        assert!(target_for("c").is_some());
        assert!(target_for("go").is_some());
        // Kotlin moved to the ANTLR backend; it's no longer a tree-sitter
        // generator target.
        assert!(target_for("kotlin").is_none());
        assert!(target_for("markdown").is_none());
        assert!(target_for("nonexistent").is_none());
    }
}
