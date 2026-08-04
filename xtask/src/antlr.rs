//! ANTLR → Rust parser generator orchestration.
//!
//! The ANTLR analogue of `xtask/src/tree_sitter.rs`. Where the tree-sitter
//! generator renders a kind-enum from a linked grammar crate, the ANTLR path
//! calls the workspace-pinned `antlr-rust-codegen` library directly over a
//! vendored `.g4` grammar.
//!
//! The generated modules are checked in verbatim under
//! `crates/mehen-<lang>-parser/src/generated/` (see that dir's README). The
//! generator emits lint and `rustfmt::skip` attributes inside each file, so
//! the owning parser crate includes them as plain modules.
//!
//! The generator is an xtask-only dependency. A normal `cargo build` targets
//! the CLI default member and uses the checked-in modules, while
//! `check-generated` is always available without a separately installed binary.

use antlr_rust_codegen::{Builder, Error as CodegenError, Severity, UnknownSemanticPolicy};
use askama::Template;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// The codegen package version recorded in generated parser-crate docs.
///
/// `Cargo.toml` pins the codegen and runtime packages in lockstep. Reading the
/// linked package's version removes the second hand-maintained version string
/// that the old external-binary integration required.
const CODEGEN_VERSION: &str = antlr_rust_codegen::VERSION;

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
    /// Pinned ANTLR Rust runtime + codegen version.
    runtime_version: &'a str,
    /// Hand-written lexer hooks (port of the upstream `<Lang>LexerBase`),
    /// when the grammar needs one. Switches the README examples to
    /// `with_typed_hooks` lexer construction.
    lexer_hooks: Option<HooksReadme<'a>>,
    /// Hand-written parser hooks (port of the upstream `<Lang>ParserBase`),
    /// when the grammar needs one. Switches the README examples to
    /// `with_typed_hooks` parser construction.
    parser_hooks: Option<HooksReadme<'a>>,
}

/// A hooks type as the README template references it: `path` is the
/// crate-relative module path for `use` lines (`hooks::JavaParserBase`),
/// `type_name` the bare type for expression position (`JavaParserBase`).
struct HooksReadme<'a> {
    path: &'a str,
    type_name: &'a str,
}

impl<'a> HooksReadme<'a> {
    /// Split an `AntlrTarget` hooks path (`hooks::JavaParserBase`) into the
    /// README's `use`-path and expression-position type name.
    fn from_path(path: &'a str) -> Self {
        let type_name = path.rsplit("::").next().expect("rsplit is non-empty");
        Self { path, type_name }
    }
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
    /// Semantic-pattern file within `grammar_dir` (passed as
    /// `--sem-patterns`), lowering the grammar's named base-class helpers
    /// (`this.Foo()` predicates/actions) to exact SemIR expressions or typed
    /// hooks. `None` for grammars with no helper calls. Generation always
    /// runs `--sem-unknown error --require-full-semantics`, so a helper the
    /// pattern file misses fails codegen instead of silently assuming true.
    pub sem_patterns: Option<&'static str>,
    /// Grammar options implemented by caller-supplied hooks (passed as
    /// `--option-hook KEY=VALUE`), e.g. `superClass=JavaParserBase` when the
    /// parser crate ships a hand-written port of that base class. Options not
    /// acknowledged here fail generation under `--require-full-semantics`.
    pub option_hooks: &'static [&'static str],
    /// Path (within the parser crate) of the hand-written hooks type the
    /// lexer construction must install — the Rust port of the upstream
    /// `<Lang>LexerBase` (e.g. `hooks::CSharpLexerBase`). `None` when the
    /// lexer needs no hooks. Referenced by the generated README so the usage
    /// example is semantically exact.
    pub lexer_hooks: Option<&'static str>,
    /// Path (within the parser crate) of the hand-written hooks type the
    /// parser construction must install — the Rust port of the upstream
    /// `<Lang>ParserBase` (e.g. `hooks::JavaParserBase`). `None` when every
    /// parser helper lowers to a pure pattern (or there are none).
    pub parser_hooks: Option<&'static str>,
    /// Grammar-preparation script within `grammar_dir`, run before the
    /// generator to derive [`Self::lexer_g4`] / [`Self::parser_g4`] (and any
    /// `sem_patterns`) from a vendored upstream grammar that is not directly
    /// generatable.
    ///
    /// `None` for grammars vendored in usable form (Kotlin, Java). C# vendors
    /// Roslyn's `CSharp.Generated.g4`, a machine-generated *reference* grammar
    /// that ANTLR rejects as-is, so its derived pair is a build artifact rather
    /// than a checked-in source. The script is invoked as
    ///
    /// ```text
    /// uv run --script <script> <prep_source> --out-dir . --xtask <current-exe>
    /// ```
    ///
    /// with `current_dir` set to `grammar_dir`.
    pub prep_script: Option<&'static str>,
    /// The vendored upstream grammar the [`Self::prep_script`] transforms.
    /// `None` when there is no prep step.
    pub prep_source: Option<&'static str>,
    /// The entry rule's name *as spelled in the grammar*, passed as
    /// `--entry-rule`. Distinct from [`Self::entry_rule`], which is the
    /// generated Rust method name (`kotlinFile` vs `kotlin_file`).
    ///
    /// Without this flag the generator conservatively treats every top-level
    /// rule reaching `EOF` as its own entry, so nothing is ever reported
    /// unreachable. Naming the one rule mehen actually calls turns on the
    /// `G4S078` unreachable-rule warning (runtime 0.24.0, upstream #262).
    /// Declared rather than inferred because only the caller knows which public
    /// rules matter — a grammar may legitimately ship alternative start rules.
    pub grammar_entry_rule: &'static str,
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
        prep_script: None,
        prep_source: None,
        grammar_entry_rule: "kotlinFile",
        entry_rule: "kotlin_file",
        sample_source: "fun main() {}",
        sem_patterns: None,
        option_hooks: &[],
        lexer_hooks: None,
        parser_hooks: None,
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
        prep_script: None,
        prep_source: None,
        grammar_entry_rule: "compilationUnit",
        entry_rule: "compilation_unit",
        sample_source: "class C {}",
        // The grammar's two `this.…()` predicates come from the upstream
        // `JavaParserBase` Java class; `patterns.toml` lowers both to typed
        // hooks and the parser crate ships an exact Rust port (`hooks::
        // JavaParserBase`) that the parser must be constructed with.
        sem_patterns: Some("patterns.toml"),
        option_hooks: &["superClass=JavaParserBase"],
        lexer_hooks: None,
        parser_hooks: Some("hooks::JavaParserBase"),
    },
    AntlrTarget {
        slug: "csharp",
        crate_dir: "crates/mehen-csharp-parser",
        grammar_dir: "crates/mehen-csharp-parser/grammar",
        lexer_g4: "CSharpLexer.g4",
        parser_g4: "CSharpParser.g4",
        display_name: "C#",
        upstream_name: "dotnet/roslyn",
        upstream_url: "https://github.com/dotnet/roslyn",
        prep_script: Some("prepare-grammar.py"),
        prep_source: Some("CSharp.Generated.g4"),
        grammar_entry_rule: "compilation_unit",
        entry_rule: "compilation_unit",
        sample_source: "class C {}",
        // Roslyn's grammar declares no `superClass` and calls no host-language
        // helpers, so there is nothing to acknowledge and no base class to port.
        // The semantic surfaces the *derived* grammar uses — the restored
        // `record` contextual keyword, the `>>` adjacency checks, and the
        // interpolated-string brace state in `@lexer::members` — all lower to
        // pure SemIR through the derived `patterns.toml`, so neither recognizer
        // needs a hook object.
        sem_patterns: Some("patterns.toml"),
        option_hooks: &[],
        lexer_hooks: None,
        parser_hooks: None,
    },
];

/// Resolve a target by slug.
pub(crate) fn target_for(slug: &str) -> Option<&'static AntlrTarget> {
    TARGETS.iter().find(|t| t.slug == slug)
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

/// Derive a target's `.g4` pair by running its [`AntlrTarget::prep_script`].
///
/// A no-op for targets vendored in generatable form. C# vendors Roslyn's
/// machine-generated *reference* grammar, which ANTLR rejects as-is, so its
/// lexer/parser pair and `patterns.toml` are derived here and gitignored — the
/// vendored `CSharp.Generated.g4` is the source of truth, exactly as the raw
/// `.g4` is for Kotlin and Java.
///
/// The script is run through `uv run --script`, which reads its PEP 723 block to
/// provision a matching interpreter rather than inheriting the ambient
/// `python3`. It receives this xtask executable's path because the transform
/// delegates rule reachability to [`unreachable_rules`] instead of
/// reimplementing the grammar analysis in Python.
///
/// `out_dir` is a **process-local** scratch directory, not `grammar_dir`. Writing the
/// derived pair into the shared grammar directory raced: `generate` and
/// `check-generated` running concurrently in one checkout (a developer alongside CI,
/// or two xtask invocations) would each truncate and rewrite the same
/// `CSharpLexer.g4` / `CSharpParser.g4` / `patterns.toml` while the other's generator
/// was reading them — intermittent generation failures or false drift. The generated
/// Rust output was already process-scoped; this closes the same gap for its input.
fn run_prep(grammar_dir: &Path, out_dir: &Path, target: &AntlrTarget) -> Result<(), String> {
    let (Some(script), Some(source)) = (target.prep_script, target.prep_source) else {
        return Ok(());
    };
    if !can_launch("uv", "--version") {
        return Err(format!(
            "`{}` needs a grammar-preparation step ({script}), which runs via `uv`.\n\
             Install it: https://docs.astral.sh/uv/getting-started/installation/",
            target.slug
        ));
    }
    let xtask = env::current_exe()
        .map_err(|e| format!("failed to resolve the running xtask executable: {e}"))?;
    let status = Command::new("uv")
        .args(["run", "--script", script, source, "--out-dir"])
        .arg(out_dir)
        .arg("--xtask")
        .arg(xtask)
        .current_dir(grammar_dir)
        .status()
        .map_err(|e| format!("failed to launch `uv run --script {script}`: {e}"))?;
    if !status.success() {
        return Err(format!(
            "grammar preparation failed for `{}` ({script} exited {})",
            target.slug,
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
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

/// PascalCase grammar name → the generated module's snake_case name
/// (`KotlinLexer` → `kotlin_lexer`), matching what the code generator emits and
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
        runtime_version: CODEGEN_VERSION,
        lexer_hooks: target.lexer_hooks.map(HooksReadme::from_path),
        parser_hooks: target.parser_hooks.map(HooksReadme::from_path),
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
    let generated_dir = target.generated_dir(workspace);
    fs::create_dir_all(&generated_dir).map_err(|e| e.to_string())?;
    run_pipeline_into(workspace, target, &generated_dir)?;

    // Render the crate README from the shared template alongside the modules,
    // so every ANTLR parser crate ships consume-me docs that stay in step with
    // the grammar/runtime it was generated against.
    let readme_path = target.readme_path(workspace);
    let readme = render_readme(target, repo_url())?;
    fs::write(&readme_path, readme)
        .map_err(|e| format!("failed writing {}: {e}", readme_path.display()))?;

    // The generator writes one module per grammar (named after the grammar)
    // plus JSON sidecars; report every checked-in artifact and the rendered
    // README.
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
/// dir. Returns the list of drifted targets.
pub(crate) fn check_generated(workspace: &Path) -> Result<Vec<&'static AntlrTarget>, String> {
    let mut drifted = Vec::new();
    for target in TARGETS {
        if target_has_drift(workspace, target)? {
            drifted.push(target);
        }
    }
    Ok(drifted)
}

fn target_has_drift(workspace: &Path, target: &AntlrTarget) -> Result<bool, String> {
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
    run_pipeline_into(workspace, target, &scratch)?;

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
///
/// Always applies the fail-loud semantic policy (`--sem-unknown error
/// --require-full-semantics`): mehen's metrics cannot afford a parser whose
/// semantic predicates were silently assumed true, so any grammar helper or
/// option not covered by the target's `sem_patterns`/`option_hooks` fails
/// generation instead of degrading parse fidelity.
fn run_pipeline_into(workspace: &Path, target: &AntlrTarget, out_dir: &Path) -> Result<(), String> {
    let grammar_dir = workspace.join(target.grammar_dir);

    // A target with a preparation step derives its `.g4` pair (and `patterns.toml`)
    // into a process-local scratch directory, and the generator then runs *there* — so
    // two concurrent xtask invocations in one checkout cannot rewrite each other's
    // inputs mid-read. A target without one (Kotlin, Java) vendors its pair directly,
    // so the generator runs in `grammar_dir` and reads read-only files.
    let prep_dir = target
        .prep_script
        .map(|_| scratch_dir("prep", target.slug))
        .map(Ok::<PathBuf, String>)
        .transpose()?;
    if let Some(dir) = &prep_dir {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    }
    run_prep(
        &grammar_dir,
        prep_dir.as_deref().unwrap_or(&grammar_dir),
        target,
    )?;
    // Where the generator resolves the `.g4` pair and `patterns.toml` from.
    let source_dir = prep_dir.as_deref().unwrap_or(&grammar_dir);

    let mut builder = Builder::new()
        .grammar(source_dir.join(target.lexer_g4))
        .grammar(source_dir.join(target.parser_g4))
        .library_directory(source_dir)
        .out_dir(out_dir)
        .unknown_semantics(UnknownSemanticPolicy::Error)
        .require_full_semantics(true)
        // Declaring the entry rule turns on the `G4S078` unreachable-rule
        // analysis. Without it the generator treats every top-level rule that
        // reaches `EOF` as its own entry, so nothing can be unreachable.
        //
        // Pruning is then enabled because mehen only ever calls the entry rule:
        // an unreachable rule is pure generated weight (~9.6 KB of Rust each,
        // measured), and dropping it shrinks the module, the binary, and compile
        // time. It does remove the rule's context type and accessor from the
        // generated API — acceptable here because these crates exist to serve
        // mehen's walkers, and `check-generated` catches any drift the day a
        // grammar update changes what is reachable.
        .entry_rule(target.grammar_entry_rule)
        .prune_unreachable(true);
    if let Some(patterns) = target.sem_patterns {
        builder = builder.semantic_patterns(source_dir.join(patterns));
    }
    for hook in target.option_hooks {
        builder = builder.option_hook(*hook);
    }
    let generation = builder.generate().map_err(format_codegen_error)?;
    for warning in generation.warnings() {
        eprintln!("{warning}");
    }
    normalize_generated(out_dir)?;
    if let Some(dir) = &prep_dir {
        let _ = fs::remove_dir_all(dir);
    }
    Ok(())
}

/// Return parser rule names diagnosed as unreachable from `entry_rule`.
///
/// The C# preparation script calls this private helper while its grammar is
/// still a single in-memory parser grammar. Structured `G4S078` diagnostics
/// point exactly at each rule name, so the helper can return names without
/// treating rendered warning prose as a protocol.
pub(crate) fn unreachable_rules(grammar: &Path, entry_rule: &str) -> Result<Vec<String>, String> {
    let source = fs::read_to_string(grammar).map_err(|e| {
        format!(
            "failed reading reachability probe {}: {e}",
            grammar.display()
        )
    })?;
    let scratch = scratch_dir("reachability", "probe");
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)
        .map_err(|e| format!("failed to create {}: {e}", scratch.display()))?;

    let generation = Builder::new()
        .grammar(grammar)
        .library_directory(grammar.parent().unwrap_or_else(|| Path::new(".")))
        .out_dir(&scratch)
        .entry_rule(entry_rule)
        .generate();
    let _ = fs::remove_dir_all(&scratch);
    let generation = generation.map_err(format_codegen_error)?;

    let mut rules = generation
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.code() == "G4S078")
        .map(|diagnostic| {
            let span = diagnostic.byte_span().ok_or_else(|| {
                format!(
                    "G4S078 diagnostic for {} has no source span",
                    diagnostic.path().display()
                )
            })?;
            let rule = source.get(span).ok_or_else(|| {
                format!(
                    "G4S078 diagnostic for {} has an invalid UTF-8 byte span",
                    diagnostic.path().display()
                )
            })?;
            if !is_rule_name(rule) {
                return Err(format!(
                    "G4S078 diagnostic subject `{rule}` is not a parser rule name"
                ));
            }
            Ok(rule.to_owned())
        })
        .collect::<Result<Vec<_>, String>>()?;
    rules.sort();
    rules.dedup();
    Ok(rules)
}

fn is_rule_name(subject: &str) -> bool {
    let mut chars = subject.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_lowercase())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn format_codegen_error(error: CodegenError) -> String {
    let mut rendered = error.to_string();
    for diagnostic in error.diagnostics() {
        let severity = match diagnostic.severity() {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        let _ = write!(
            rendered,
            "\n{severity}[{}]: {}",
            diagnostic.code(),
            diagnostic.path().display()
        );
        if let (Some(line), Some(column)) = (diagnostic.line(), diagnostic.column()) {
            let _ = write!(rendered, ":{line}:{column}");
        }
        let _ = write!(rendered, ": {}", diagnostic.message());
    }
    rendered
}

/// Whether `path` is a generated artifact that participates in the checked-in
/// snapshot and the drift comparison: the Rust lexer/parser modules (`.rs`) and
/// the generator's JSON sidecars — `semantics.json` (since the 0.13.0 runtime)
/// and `decisions.json` (the per-decision prediction-tier report, since 0.22.0).
/// Matching on the extension rather than a name list means a new sidecar is
/// drift-guarded the moment the generator starts emitting it. Everything else in
/// the dir (e.g. `README.md`) is hand-authored and excluded.
fn is_generated_artifact(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| x == "rs" || x == "json")
}

/// Normalize each generated artifact's trailing newline to a single `\n`.
///
/// The `.rs` modules and JSON sidecars are treated alike so a freshly rendered
/// tree compares byte-for-byte against the checked-in one regardless of whether
/// a given tool appends a trailing newline.
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

/// Read every generated artifact (`*.rs` modules + JSON sidecars) in `dir` into
/// a sorted `(name, contents)` list for comparison.
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
            // The hooks-less example calls the entry rule as a path
            // (`JavaParser::compilation_unit`); the hooks example calls it on
            // the constructed parser (`parser.compilation_unit()?`).
            let entry_call = if target.lexer_hooks.is_some() || target.parser_hooks.is_some() {
                format!("parser.{}()", target.entry_rule)
            } else {
                format!("{parser_type}::{}", target.entry_rule)
            };
            assert!(
                readme.contains(&entry_call),
                "`{}` README missing `{entry_call}` entry-rule call",
                target.slug,
            );
            assert!(
                readme.contains(target.upstream_url),
                "`{}` README missing upstream URL",
                target.slug
            );
        }
    }

    #[test]
    fn readme_hooks_example_references_every_hooks_path() {
        // A target that declares hand-written hooks must render a README whose
        // example imports and installs them — otherwise consumers copy an
        // example that fails loud at the first hooked predicate.
        for target in TARGETS {
            let readme = render_readme(target, "https://example.test/repo")
                .unwrap_or_else(|e| panic!("render failed for `{}`: {e}", target.slug));
            for hooks in [target.lexer_hooks, target.parser_hooks]
                .into_iter()
                .flatten()
            {
                assert!(
                    readme.contains(hooks),
                    "`{}` README missing `use …::{hooks}` import",
                    target.slug
                );
                let type_name = hooks.rsplit("::").next().unwrap();
                assert!(
                    readme.contains("with_typed_hooks("),
                    "`{}` README missing with_typed_hooks construction",
                    target.slug
                );
                assert!(
                    readme.contains(&format!("{type_name}::default()")),
                    "`{}` README missing `{type_name}::default()` install",
                    target.slug
                );
            }
        }
    }
}
