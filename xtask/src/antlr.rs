//! ANTLR → Rust parser generator orchestration.
//!
//! The ANTLR analogue of `xtask/src/tree_sitter.rs`. Where the tree-sitter
//! generator renders a kind-enum from a linked grammar crate, the ANTLR path
//! orchestrates two external tools over a vendored `.g4` grammar:
//!
//! 1. the official **ANTLR tool jar** (`java -jar antlr-4.13.2-complete.jar`)
//!    turns `*.g4` into `*.interp` metadata, then
//! 2. **`antlr4-rust-gen`** (from `ophi-dev/antlr-rust-runtime`) turns the
//!    `*.interp` metadata into Rust lexer/parser modules.
//!
//! The generated modules are checked in verbatim under
//! `crates/mehen-<lang>/src/generated/` (see that dir's README). The
//! generator emits lint and `rustfmt::skip` attributes inside each file, so
//! owning analyzer crates include them as plain modules.
//!
//! Because this path needs Java + the ANTLR jar + the generator binary —
//! none of which a normal `cargo build` requires — the tools are discovered
//! at run time and a missing tool yields a clear, actionable error.
//! `check-generated` treats missing tools as "skipped" (exit 0) so the drift
//! guard only runs where the toolchain is installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// One per-crate ANTLR target understood by `xtask antlr generate <slug>`.
pub(crate) struct AntlrTarget {
    /// CLI slug, e.g. `kotlin`.
    pub slug: &'static str,
    /// Owning crate directory, relative to the workspace root.
    pub crate_dir: &'static str,
    /// Vendored grammar directory (holds the `.g4` files), relative to the
    /// workspace root. The lexer's `import`ed files (e.g. `UnicodeClasses`)
    /// must live here too — the ANTLR jar resolves imports from the lexer's
    /// directory.
    pub grammar_dir: &'static str,
    /// Lexer grammar filename within `grammar_dir`.
    pub lexer_g4: &'static str,
    /// Parser grammar filename within `grammar_dir`.
    pub parser_g4: &'static str,
}

impl AntlrTarget {
    /// Directory the generated `*.rs` modules are written to.
    fn generated_dir(&self, workspace: &Path) -> PathBuf {
        workspace.join(self.crate_dir).join("src").join("generated")
    }
}

/// Every ANTLR-backed analyzer with checked-in generated modules.
pub(crate) const TARGETS: &[AntlrTarget] = &[AntlrTarget {
    slug: "kotlin",
    crate_dir: "crates/mehen-kotlin",
    grammar_dir: "crates/mehen-kotlin/grammar",
    lexer_g4: "KotlinLexer.g4",
    parser_g4: "KotlinParser.g4",
}];

/// Resolve a target by slug.
pub(crate) fn target_for(slug: &str) -> Option<&'static AntlrTarget> {
    TARGETS.iter().find(|t| t.slug == slug)
}

/// Locations of the external tools, resolved once.
struct Toolchain {
    /// Path to the ANTLR tool jar.
    antlr_jar: PathBuf,
    /// How to invoke `antlr4-rust-gen` (either a bare command on PATH or an
    /// explicit path).
    rust_gen: PathBuf,
}

/// Discover the external toolchain from the environment.
///
/// - `MEHEN_ANTLR_JAR` → path to `antlr-4.13.2-complete.jar` (required).
/// - `MEHEN_ANTLR_RUST_GEN` → path to the `antlr4-rust-gen` binary; if unset,
///   the binary is expected on `PATH` (install via
///   `cargo install antlr-rust-runtime`).
///
/// Returns `Ok(None)` when a tool is missing so callers can choose to skip
/// (check) or error (generate).
fn discover_toolchain() -> Result<Option<Toolchain>, String> {
    let Some(jar) = env::var_os("MEHEN_ANTLR_JAR") else {
        return Ok(None);
    };
    let antlr_jar = PathBuf::from(jar);
    if !antlr_jar.is_file() {
        return Err(format!(
            "MEHEN_ANTLR_JAR points at `{}`, which is not a file",
            antlr_jar.display()
        ));
    }

    let rust_gen = match env::var_os("MEHEN_ANTLR_RUST_GEN") {
        Some(p) => {
            let path = PathBuf::from(p);
            if !path.is_file() {
                return Err(format!(
                    "MEHEN_ANTLR_RUST_GEN points at `{}`, which is not a file",
                    path.display()
                ));
            }
            path
        }
        None => PathBuf::from("antlr4-rust-gen"),
    };

    // The jar path existing is not enough — `java` and `antlr4-rust-gen`
    // must actually be launchable, or the pipeline would fail mid-run.
    // Probe both now so a missing executable reads as "toolchain
    // unavailable" (→ skip for `check-generated`) rather than a hard error.
    if !can_launch("java", "-version") || !can_launch(&rust_gen, "--help") {
        return Ok(None);
    }

    Ok(Some(Toolchain {
        antlr_jar,
        rust_gen,
    }))
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
fn toolchain_help() -> String {
    "ANTLR codegen needs external tools:\n\
     - set MEHEN_ANTLR_JAR to an `antlr-4.13.2-complete.jar` \
       (https://www.antlr.org/download/)\n\
     - install the generator with `cargo install antlr-rust-runtime` (provides \
       `antlr4-rust-gen`), or set MEHEN_ANTLR_RUST_GEN to its path\n\
     - a Java runtime must be on PATH to run the jar"
        .to_string()
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

    // Stage 1: ANTLR jar → .interp metadata, into a scratch dir.
    let interp_dir = scratch_dir("interp", target.slug);
    let _ = fs::remove_dir_all(&interp_dir);
    fs::create_dir_all(&interp_dir).map_err(|e| e.to_string())?;

    let jar_status = Command::new("java")
        .arg("-jar")
        .arg(&tools.antlr_jar)
        .arg("-o")
        .arg(&interp_dir)
        .arg("-Xexact-output-dir")
        .arg(target.lexer_g4)
        .arg(target.parser_g4)
        .current_dir(&grammar_dir)
        .status()
        .map_err(|e| format!("failed to launch java: {e}\n{}", toolchain_help()))?;
    if !jar_status.success() {
        return Err(format!(
            "ANTLR jar failed for `{}` (exit {:?})",
            target.slug,
            jar_status.code()
        ));
    }

    // Stage 2: antlr4-rust-gen → Rust modules.
    let lexer_interp = interp_dir.join(interp_name(target.lexer_g4));
    let parser_interp = interp_dir.join(interp_name(target.parser_g4));
    let gen_status = Command::new(&tools.rust_gen)
        .arg("--lexer")
        .arg(&lexer_interp)
        .arg("--parser")
        .arg(&parser_interp)
        .arg("--out-dir")
        .arg(&generated_dir)
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
    normalize_generated_rs(&generated_dir)?;

    let _ = fs::remove_dir_all(&interp_dir);

    // The generator writes one module per grammar, named after the grammar.
    let mut written: Vec<PathBuf> = fs::read_dir(&generated_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    written.sort();
    Ok(written)
}

/// `KotlinLexer.g4` → `KotlinLexer.interp`.
fn interp_name(g4: &str) -> String {
    let stem = g4.strip_suffix(".g4").unwrap_or(g4);
    format!("{stem}.interp")
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

    // Generate into the scratch dir by temporarily retargeting. We render
    // straight into `scratch` to avoid mutating the checked-in files.
    let scratch_target = AntlrTarget {
        slug: target.slug,
        crate_dir: target.crate_dir,
        grammar_dir: target.grammar_dir,
        lexer_g4: target.lexer_g4,
        parser_g4: target.parser_g4,
    };
    // Override generated dir via a sibling helper: generate into scratch.
    let interp_dir = scratch_dir("checkinterp", target.slug);
    let _ = fs::remove_dir_all(&interp_dir);
    fs::create_dir_all(&interp_dir).map_err(|e| e.to_string())?;
    run_pipeline_into(workspace, &scratch_target, tools, &interp_dir, &scratch)?;

    let after = read_generated(&scratch)?;
    let _ = fs::remove_dir_all(&scratch);
    let _ = fs::remove_dir_all(&interp_dir);

    Ok(before != after)
}

/// Run the jar + generator pipeline writing modules into `out_dir`.
fn run_pipeline_into(
    workspace: &Path,
    target: &AntlrTarget,
    tools: &Toolchain,
    interp_dir: &Path,
    out_dir: &Path,
) -> Result<(), String> {
    let grammar_dir = workspace.join(target.grammar_dir);
    let jar_status = Command::new("java")
        .arg("-jar")
        .arg(&tools.antlr_jar)
        .arg("-o")
        .arg(interp_dir)
        .arg("-Xexact-output-dir")
        .arg(target.lexer_g4)
        .arg(target.parser_g4)
        .current_dir(&grammar_dir)
        .status()
        .map_err(|e| format!("failed to launch java: {e}"))?;
    if !jar_status.success() {
        return Err(format!("ANTLR jar failed for `{}`", target.slug));
    }
    let gen_status = Command::new(&tools.rust_gen)
        .arg("--lexer")
        .arg(interp_dir.join(interp_name(target.lexer_g4)))
        .arg("--parser")
        .arg(interp_dir.join(interp_name(target.parser_g4)))
        .arg("--out-dir")
        .arg(out_dir)
        .status()
        .map_err(|e| format!("failed to launch antlr4-rust-gen: {e}"))?;
    if !gen_status.success() {
        return Err(format!("antlr4-rust-gen failed for `{}`", target.slug));
    }
    normalize_generated_rs(out_dir)?;
    Ok(())
}

fn normalize_generated_rs(dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path.is_file() || path.extension().is_none_or(|x| x != "rs") {
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

/// Read every `*.rs` in `dir` into a sorted `(name, contents)` list for
/// comparison.
fn read_generated(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut entries: Vec<(String, String)> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
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
