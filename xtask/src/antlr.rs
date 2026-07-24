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
}

impl AntlrTarget {
    /// Directory the generated `*.rs` modules are written to.
    fn generated_dir(&self, workspace: &Path) -> PathBuf {
        workspace.join(self.crate_dir).join("src").join("generated")
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
    },
    AntlrTarget {
        slug: "java",
        crate_dir: "crates/mehen-java-parser",
        grammar_dir: "crates/mehen-java-parser/grammar",
        lexer_g4: "JavaLexer.g4",
        parser_g4: "JavaParser.g4",
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

    // The generator writes one module per grammar (named after the grammar)
    // plus a `semantics.json` sidecar; report every checked-in artifact.
    let mut written: Vec<PathBuf> = fs::read_dir(&generated_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_generated_artifact(p))
        .collect();
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
    run_pipeline_into(workspace, &scratch_target, tools, &scratch)?;

    let after = read_generated(&scratch)?;
    let _ = fs::remove_dir_all(&scratch);

    Ok(before != after)
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
