//! ANTLR → Rust parser generator orchestration.
//!
//! The ANTLR analogue of `xtask/src/tree_sitter.rs`. Where the tree-sitter
//! generator renders a kind-enum from a linked grammar crate, the ANTLR path
//! invokes **`antlr4-rust-gen`** (from `ophi-dev/antlr-rust-runtime`) directly
//! over a vendored `.g4` grammar.
//!
//! The generated modules are checked in verbatim under
//! `crates/mehen-<lang>-parser/src/generated/` (see that dir's README). The
//! generator emits lint and `rustfmt::skip` attributes inside each file, so
//! the owning parser crate includes them as plain modules.
//!
//! Because this path needs the generator binary, which a normal `cargo build`
//! does not require, the tool is discovered at run time and a missing tool
//! yields a clear, actionable error.
//! `check-generated` treats missing tools as "skipped" (exit 0) so the drift
//! guard only runs where the toolchain is installed.

use askama::Template;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// The `antlr-rust-runtime` version the checked-in modules were generated
/// against — must match the `antlr4_runtime` pin in the workspace
/// `[workspace.dependencies]` (root `Cargo.toml`). Installing an *unpinned*
/// `antlr4-rust-gen` would fetch the latest crate, which after a runtime
/// release can regenerate modules that no longer match the pinned runtime and
/// silently drift. Bump this in lockstep with that pin.
const GENERATOR_VERSION: &str = "0.15.2";

/// Askama model for a parser crate's generated `README.md`.
///
/// Rendered from `xtask/templates/parser-readme.md` alongside the generated
/// modules so every ANTLR parser crate ships consume-me docs (a git-dependency
/// snippet and a parse example) without hand-maintenance. The doc code mirrors
/// the compile-tested `//!` example in the crate's `lib.rs`; keeping the two in
/// step is a review concern, not a build one (a plain README is not a
/// doctest). Metadata fields (runtime version, entry rule, upstream) are
/// drift-checked by `xtask antlr check-generated`.
#[derive(Template)]
#[template(path = "parser-readme.md", escape = "none")]
struct ReadmeTemplate<'a> {
    /// CLI slug (`kotlin`) — names the regenerate command in the header.
    slug: &'a str,
    /// Crate name as depended on (`mehen-kotlin-parser`).
    crate_name: &'a str,
    /// Crate identifier for `use` paths (`mehen_kotlin_parser`).
    crate_ident: String,
    /// Human-facing language name (`Kotlin`).
    display_name: &'a str,
    /// Workspace repository URL, used for the git-dependency snippet.
    repo_url: &'a str,
    /// Generated lexer module name (`kotlin_lexer`).
    lexer_module: String,
    /// Generated lexer type name (`KotlinLexer`).
    lexer_type: String,
    /// Generated parser module name (`kotlin_parser`).
    parser_module: String,
    /// Generated parser type name (`KotlinParser`).
    parser_type: String,
    /// Parser entry-rule method used in the example (`kotlin_file`).
    entry_rule: &'a str,
    /// One-line sample source parsed in the example (`fun main() {}`).
    sample_source: &'a str,
    /// Upstream grammar project name (`Kotlin/kotlin-spec`).
    upstream_name: &'a str,
    /// Upstream grammar project URL.
    upstream_url: &'a str,
    /// Pinned ANTLR Rust runtime + generator version.
    runtime_version: &'a str,
}

/// One per-crate ANTLR target understood by `xtask antlr generate <slug>`.
pub(crate) struct AntlrTarget {
    /// CLI slug, e.g. `kotlin`.
    pub slug: &'static str,
    /// Owning crate directory, relative to the workspace root.
    pub crate_dir: &'static str,
    /// Vendored grammar directory (holds the `.g4` files), relative to the
    /// workspace root. The lexer's `import`ed files (e.g. `UnicodeClasses`)
    /// must live here too so the generator can resolve them relative to the
    /// root grammar.
    pub grammar_dir: &'static str,
    /// Lexer grammar filename within `grammar_dir`.
    pub lexer_g4: &'static str,
    /// Parser grammar filename within `grammar_dir`.
    pub parser_g4: &'static str,
    /// Human-facing language name for the crate README (e.g. `Kotlin`).
    pub display_name: &'static str,
    /// Upstream grammar project name, as shown in the README (e.g.
    /// `Kotlin/kotlin-spec`).
    pub upstream_name: &'static str,
    /// Upstream grammar project URL.
    pub upstream_url: &'static str,
    /// Parser entry-rule method used in the README usage example, in the
    /// generated snake_case form (e.g. `kotlin_file`). Must be a real entry
    /// rule on the generated `<Lang>Parser` — it is compile-tested by the
    /// mirrored `//!` doc example in the crate's `lib.rs`.
    pub entry_rule: &'static str,
    /// A minimal valid source snippet for the README usage example (e.g.
    /// `fun main() {}`). Kept to one line — the template appends the newline.
    pub sample_source: &'static str,
}

impl AntlrTarget {
    /// Directory the generated `*.rs` modules are written to.
    fn generated_dir(&self, workspace: &Path) -> PathBuf {
        workspace.join(self.crate_dir).join("src").join("generated")
    }

    /// Path to the checked-in, generated crate `README.md`.
    fn readme_path(&self, workspace: &Path) -> PathBuf {
        workspace.join(self.crate_dir).join("README.md")
    }

    /// Crate name (the last path component of `crate_dir`, e.g.
    /// `mehen-kotlin-parser`).
    fn crate_name(&self) -> &'static str {
        self.crate_dir
            .rsplit('/')
            .next()
            .expect("crate_dir is non-empty")
    }
}

/// Every ANTLR-backed analyzer with checked-in generated modules.
pub(crate) const TARGETS: &[AntlrTarget] = &[
    AntlrTarget {
        slug: "kotlin",
        crate_dir: "crates/mehen-kotlin-parser",
        grammar_dir: "crates/mehen-kotlin-parser/grammar",
        lexer_g4: "KotlinLexer.g4",
        parser_g4: "KotlinParser.g4",
        display_name: "Kotlin",
        upstream_name: "Kotlin/kotlin-spec",
        upstream_url: "https://github.com/Kotlin/kotlin-spec",
        entry_rule: "kotlin_file",
        sample_source: "fun main() {}",
    },
    AntlrTarget {
        slug: "java",
        crate_dir: "crates/mehen-java-parser",
        grammar_dir: "crates/mehen-java-parser/grammar",
        lexer_g4: "JavaLexer.g4",
        parser_g4: "JavaParser.g4",
        display_name: "Java",
        upstream_name: "antlr/grammars-v4",
        upstream_url: "https://github.com/antlr/grammars-v4",
        entry_rule: "compilation_unit",
        sample_source: "class C {}",
    },
];

/// Resolve a target by slug.
pub(crate) fn target_for(slug: &str) -> Option<&'static AntlrTarget> {
    TARGETS.iter().find(|t| t.slug == slug)
}

/// Locations of the external tools, resolved once.
struct Toolchain {
    /// How to invoke `antlr4-rust-gen` (either a bare command on PATH or an
    /// explicit path).
    rust_gen: PathBuf,
}

/// Discover the external toolchain from the environment.
///
/// - `MEHEN_ANTLR_RUST_GEN` → path to the `antlr4-rust-gen` binary; if unset,
///   the binary is expected on `PATH` (install the matching generator via
///   `cargo install antlr-rust-runtime --version <GENERATOR_VERSION>`).
///
/// Returns `Ok(None)` when a tool is missing so callers can choose to skip
/// (check) or error (generate).
fn discover_toolchain() -> Result<Option<Toolchain>, String> {
    let rust_gen = match env::var_os("MEHEN_ANTLR_RUST_GEN") {
        Some(p) => {
            let path = PathBuf::from(p);
            if !path.is_file() {
                return Err(format!(
                    "MEHEN_ANTLR_RUST_GEN points at `{}`, which is not a file",
                    path.display()
                ));
            }
            // Canonicalize now: the generator runs with `current_dir` set to the
            // grammar directory, so a *relative* env path (e.g.
            // `target/debug/antlr4-rust-gen`) validated here from the caller's
            // cwd would otherwise fail to launch when re-resolved under
            // `crates/<lang>-parser/grammar`. An absolute path keeps it stable
            // across the cwd change.
            fs::canonicalize(&path).map_err(|e| {
                format!(
                    "MEHEN_ANTLR_RUST_GEN points at `{}`, which could not be resolved: {e}",
                    path.display()
                )
            })?
        }
        // A bare command name is resolved via PATH by the OS at spawn time, so
        // it is unaffected by the generator's `current_dir`.
        None => PathBuf::from("antlr4-rust-gen"),
    };

    // Probe now so a missing executable reads as "toolchain unavailable"
    // (and skips `check-generated`) rather than as a hard process error.
    if !can_launch(&rust_gen, "--help") {
        return Ok(None);
    }

    Ok(Some(Toolchain { rust_gen }))
}

/// Whether `program arg` can be launched and exits without an I/O error
/// (the program exists and is executable). The exit *status* is ignored —
/// some tools return non-zero for `--help`/`-version` — we only care that
/// the executable is present.
fn can_launch(program: impl AsRef<std::ffi::OsStr>, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// A process-unique scratch directory under the system temp dir.
///
/// The process id keeps concurrent `xtask antlr …` invocations (e.g. a
/// developer running `generate` while CI runs `check-generated`) from
/// sharing — and deleting — each other's working directories. `kind`
/// distinguishes the per-call purposes (`interp`, `check`, …).
fn scratch_dir(kind: &str, slug: &str) -> PathBuf {
    env::temp_dir().join(format!("mehen-antlr-{kind}-{slug}-{}", std::process::id()))
}

/// Human-readable instructions printed when the toolchain is unavailable.
///
/// Pins `--version` to [`GENERATOR_VERSION`] so following the hint installs the
/// generator matching the workspace runtime pin, not whatever is latest on
/// crates.io (an unpinned install can regenerate drifting modules after a
/// runtime release).
fn toolchain_help() -> String {
    format!(
        "ANTLR codegen needs `antlr4-rust-gen`: install it with \
         `cargo install antlr-rust-runtime --version {GENERATOR_VERSION} \
         --features codegen --bin antlr4-rust-gen`, \
         or set MEHEN_ANTLR_RUST_GEN to its path"
    )
}

/// PascalCase grammar name → the generated module's snake_case name
/// (`KotlinLexer` → `kotlin_lexer`), matching what `antlr4-rust-gen` emits and
/// what the parser crate's `lib.rs` declares. Handles acronym runs the usual
/// way (`HTMLParser` → `html_parser`).
fn to_snake_case(pascal: &str) -> String {
    let chars: Vec<char> = pascal.chars().collect();
    let mut out = String::with_capacity(pascal.len() + 2);
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() {
            // A boundary precedes an uppercase run's start (prev is lowercase)
            // and the last capital of a run before a new word (prev upper, next
            // lower) — so `KotlinLexer`→`kotlin_lexer`, `HTMLParser`→`html_parser`.
            let prev_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
            let boundary_after_acronym = i > 0
                && chars[i - 1].is_ascii_uppercase()
                && chars.get(i + 1).is_some_and(|c| c.is_ascii_lowercase());
            if prev_lower || boundary_after_acronym {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Render a parser crate's `README.md` from the shared template.
///
/// Module/type names are derived from the grammar filenames the same way the
/// generator names its output (`KotlinLexer.g4` → module `kotlin_lexer`, type
/// `KotlinLexer`), so the README's `use` paths always match the checked-in
/// modules. `repo_url` comes from the workspace `repository` field (xtask
/// inherits it), keeping the git-dependency snippet in sync with the real repo.
fn render_readme(target: &AntlrTarget, repo_url: &str) -> Result<String, String> {
    let lexer_type = target.lexer_g4.trim_end_matches(".g4");
    let parser_type = target.parser_g4.trim_end_matches(".g4");
    let crate_name = target.crate_name();
    let tmpl = ReadmeTemplate {
        slug: target.slug,
        crate_name,
        crate_ident: crate_name.replace('-', "_"),
        display_name: target.display_name,
        repo_url,
        lexer_module: to_snake_case(lexer_type),
        lexer_type: lexer_type.to_string(),
        parser_module: to_snake_case(parser_type),
        parser_type: parser_type.to_string(),
        entry_rule: target.entry_rule,
        sample_source: target.sample_source,
        upstream_name: target.upstream_name,
        upstream_url: target.upstream_url,
        runtime_version: GENERATOR_VERSION,
    };
    // Normalize the trailing newline exactly like the generated modules so a
    // fresh render compares byte-for-byte against the checked-in file.
    let body = tmpl
        .render()
        .map_err(|e| format!("failed to render README for `{}`: {e}", target.slug))?;
    Ok(format!("{}\n", body.trim_end_matches(['\n', '\r'])))
}

/// The workspace repository URL, inherited by xtask via `repository.workspace`.
fn repo_url() -> &'static str {
    env!("CARGO_PKG_REPOSITORY")
}

/// Generate the Rust modules for one target into its `src/generated/` dir.
pub(crate) fn generate(workspace: &Path, target: &AntlrTarget) -> Result<Vec<PathBuf>, String> {
    let toolchain = discover_toolchain()?
        .ok_or_else(|| format!("toolchain unavailable.\n{}", toolchain_help()))?;
    generate_with(workspace, target, &toolchain)
}

fn generate_with(
    workspace: &Path,
    target: &AntlrTarget,
    tools: &Toolchain,
) -> Result<Vec<PathBuf>, String> {
    let grammar_dir = workspace.join(target.grammar_dir);
    let generated_dir = target.generated_dir(workspace);
    fs::create_dir_all(&generated_dir).map_err(|e| e.to_string())?;

    let gen_status = Command::new(&tools.rust_gen)
        .arg(target.lexer_g4)
        .arg(target.parser_g4)
        .arg("--out-dir")
        .arg(&generated_dir)
        .current_dir(&grammar_dir)
        .status()
        .map_err(|e| {
            format!(
                "failed to launch antlr4-rust-gen: {e}\n{}",
                toolchain_help()
            )
        })?;
    if !gen_status.success() {
        return Err(format!(
            "antlr4-rust-gen failed for `{}` (exit {:?})",
            target.slug,
            gen_status.code()
        ));
    }
    normalize_generated(&generated_dir)?;

    // Render the crate README from the shared template alongside the modules,
    // so every ANTLR parser crate ships consume-me docs that stay in step with
    // the grammar/runtime it was generated against.
    let readme_path = target.readme_path(workspace);
    let readme = render_readme(target, repo_url())?;
    fs::write(&readme_path, readme)
        .map_err(|e| format!("failed writing {}: {e}", readme_path.display()))?;

    // The generator writes one module per grammar (named after the grammar)
    // plus a `semantics.json` sidecar; report every checked-in artifact, plus
    // the rendered README.
    let mut written: Vec<PathBuf> = fs::read_dir(&generated_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_generated_artifact(p))
        .collect();
    written.push(readme_path);
    written.sort();
    Ok(written)
}

/// Compare checked-in generated modules against a fresh render in a scratch
/// dir. Returns the list of drifted targets. When the toolchain is missing,
/// returns `Ok(None)` so the caller can report "skipped" rather than fail.
pub(crate) fn check_generated(
    workspace: &Path,
) -> Result<Option<Vec<&'static AntlrTarget>>, String> {
    let Some(tools) = discover_toolchain()? else {
        return Ok(None);
    };

    let mut drifted = Vec::new();
    for target in TARGETS {
        if target_has_drift(workspace, target, &tools)? {
            drifted.push(target);
        }
    }
    Ok(Some(drifted))
}

fn target_has_drift(
    workspace: &Path,
    target: &AntlrTarget,
    tools: &Toolchain,
) -> Result<bool, String> {
    let generated_dir = target.generated_dir(workspace);
    // Snapshot the checked-in modules.
    let before = read_generated(&generated_dir)?;
    // Regenerate into a scratch copy of the dir, then compare and restore.
    let scratch = scratch_dir("check", target.slug);
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;

    // Render straight into `scratch` to avoid mutating the checked-in files.
    // `run_pipeline_into` takes the output dir explicitly and never consults
    // `target.generated_dir()`, so the target can be passed as-is.
    run_pipeline_into(workspace, target, tools, &scratch)?;

    let after = read_generated(&scratch)?;
    let _ = fs::remove_dir_all(&scratch);

    if before != after {
        return Ok(true);
    }

    // The README is generated from the shared template too, so a template edit
    // or a metadata bump (runtime version, entry rule, upstream) without a
    // regenerate drifts it just like a module. Compare the checked-in file
    // against a fresh render. A missing README also counts as drift.
    let readme_path = target.readme_path(workspace);
    let expected = render_readme(target, repo_url())?;
    let actual = fs::read_to_string(&readme_path).unwrap_or_default();
    Ok(actual != expected)
}

/// Run the generator pipeline writing modules into `out_dir`.
fn run_pipeline_into(
    workspace: &Path,
    target: &AntlrTarget,
    tools: &Toolchain,
    out_dir: &Path,
) -> Result<(), String> {
    let grammar_dir = workspace.join(target.grammar_dir);
    let gen_status = Command::new(&tools.rust_gen)
        .arg(target.lexer_g4)
        .arg(target.parser_g4)
        .arg("--out-dir")
        .arg(out_dir)
        .current_dir(&grammar_dir)
        .status()
        .map_err(|e| format!("failed to launch antlr4-rust-gen: {e}"))?;
    if !gen_status.success() {
        return Err(format!("antlr4-rust-gen failed for `{}`", target.slug));
    }
    normalize_generated(out_dir)?;
    Ok(())
}

/// Whether `path` is a generated artifact that participates in the checked-in
/// snapshot and the drift comparison: the Rust lexer/parser modules (`.rs`)
/// and the generator's `semantics.json` sidecar (emitted alongside them since
/// the 0.13.0 runtime). Everything else in the dir (e.g. `README.md`) is
/// hand-authored and excluded.
fn is_generated_artifact(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| x == "rs" || x == "json")
}

/// Normalize each generated artifact's trailing newline to a single `\n`.
///
/// The `.rs` modules and the `semantics.json` sidecar are treated alike so a
/// freshly rendered tree compares byte-for-byte against the checked-in one
/// regardless of whether a given tool appends a trailing newline.
fn normalize_generated(dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path.is_file() || !is_generated_artifact(&path) {
            continue;
        }
        let body = fs::read_to_string(&path)
            .map_err(|e| format!("failed reading {}: {e}", path.display()))?;
        let normalized = format!("{}\n", body.trim_end_matches(['\n', '\r']));
        if normalized != body {
            fs::write(&path, normalized)
                .map_err(|e| format!("failed writing {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Read every generated artifact (`*.rs` modules + `semantics.json`) in `dir`
/// into a sorted `(name, contents)` list for comparison.
fn read_generated(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut entries: Vec<(String, String)> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_generated_artifact(p))
        .map(|p| -> Result<(String, String), String> {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Surface read failures instead of treating an unreadable file
            // as empty — a silent empty body would skew the drift compare.
            let body = fs::read_to_string(&p)
                .map_err(|e| format!("failed reading {}: {e}", p.display()))?;
            Ok((name, body))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

/// Locate the workspace root (the `[workspace]` `Cargo.toml`).
pub(crate) fn workspace_root() -> std::io::Result<PathBuf> {
    crate::tree_sitter::workspace_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_snake_case_matches_generated_module_names() {
        // The real grammar type names both parser crates render from.
        assert_eq!(to_snake_case("KotlinLexer"), "kotlin_lexer");
        assert_eq!(to_snake_case("KotlinParser"), "kotlin_parser");
        assert_eq!(to_snake_case("JavaLexer"), "java_lexer");
        assert_eq!(to_snake_case("JavaParser"), "java_parser");
    }

    #[test]
    fn to_snake_case_handles_acronym_runs() {
        // Defensive for future grammars whose names carry acronyms.
        assert_eq!(to_snake_case("HTMLParser"), "html_parser");
        assert_eq!(to_snake_case("CSSLexer"), "css_lexer");
        assert_eq!(to_snake_case("Lexer"), "lexer");
    }

    #[test]
    fn readme_renders_derived_names_for_every_target() {
        // Every target must render without error and place the derived
        // module/type names into the README, so a new target can't silently
        // ship a README whose `use` paths don't match its modules.
        for target in TARGETS {
            let readme = render_readme(target, "https://example.test/repo")
                .unwrap_or_else(|e| panic!("render failed for `{}`: {e}", target.slug));
            let lexer_type = target.lexer_g4.trim_end_matches(".g4");
            let parser_type = target.parser_g4.trim_end_matches(".g4");
            assert!(
                readme.contains(&to_snake_case(lexer_type)),
                "`{}` README missing lexer module name",
                target.slug
            );
            assert!(
                readme.contains(&format!("{parser_type}::{}", target.entry_rule)),
                "`{}` README missing `{parser_type}::{}` entry-rule call",
                target.slug,
                target.entry_rule
            );
            assert!(
                readme.contains(target.upstream_url),
                "`{}` README missing upstream URL",
                target.slug
            );
        }
    }
}
