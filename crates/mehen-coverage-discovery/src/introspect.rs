// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Declarative tool-config introspection — tier 1 of discovery.
//!
//! Only *data* formats are read: JSON (the c8/nyc rc family), TOML
//! (`pyproject.toml`, `tarpaulin.toml`/`.tarpaulin.toml`), and XML
//! (`phpunit.xml`/`.dist`). Executable
//! configs — `jest.config.ts`, `vitest.config.ts`, `.simplecov`, Gradle
//! DSLs, Pester scripts — are **never executed and never regex-scraped**:
//! their values are routinely computed (template strings, env vars,
//! imported constants), so extraction would be wrong often enough to be
//! worse than the fallback, and the artifact scan already covers every
//! default location those tools write to.
//!
//! Configured paths are validated like any scan candidate (existence,
//! size, content sniff) and must stay inside the root — a config naming
//! `../../elsewhere` yields a diagnostic, not a read.

use camino::{Utf8Path, Utf8PathBuf};

use crate::select::Candidate;
use crate::walk::validate_candidate;
use crate::{DiscoveryCaps, DiscoveryDiagnostics, RejectReason, Rejected, ReportOrigin};

/// Run every introspector against one root.
pub(crate) fn introspect_root(
    root: &Utf8Path,
    caps: &DiscoveryCaps,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut DiscoveryDiagnostics,
) {
    let canonical_root: Vec<std::path::PathBuf> = std::fs::canonicalize(root.as_std_path())
        .ok()
        .into_iter()
        .collect();
    let mut sink = Sink {
        root,
        caps,
        canonical_root: &canonical_root,
        candidates,
        diagnostics,
    };

    introspect_js_rc(root, &mut sink);
    introspect_pyproject(root, &mut sink);
    introspect_phpunit(root, &mut sink);
    introspect_tarpaulin(root, &mut sink);
}

/// Shared candidate-submission plumbing for the introspectors.
struct Sink<'a> {
    root: &'a Utf8Path,
    caps: &'a DiscoveryCaps,
    canonical_root: &'a [std::path::PathBuf],
    candidates: &'a mut Vec<Candidate>,
    diagnostics: &'a mut DiscoveryDiagnostics,
}

impl Sink<'_> {
    /// Submit a config-named report location. `configured` is the raw
    /// value from the tool config, resolved against the root; it must
    /// not escape it.
    fn submit(&mut self, config: &Utf8Path, configured: &str) {
        let Some(resolved) = resolve_inside_root(self.root, configured) else {
            self.reject(config, Utf8PathBuf::from(configured));
            return;
        };
        if !resolved.is_file() {
            self.reject(config, resolved);
            return;
        }
        self.diagnostics.candidates_matched += 1;
        match validate_candidate(
            &resolved,
            ReportOrigin::ToolConfig(config.to_path_buf()),
            self.canonical_root,
            self.caps,
        ) {
            Ok(candidate) => self.candidates.push(candidate),
            Err(reason) => self.diagnostics.rejected.push(Rejected {
                path: resolved,
                reason,
            }),
        }
    }

    fn reject(&mut self, config: &Utf8Path, path: Utf8PathBuf) {
        log::info!(
            "coverage config {config} names a report location that does not resolve: {path}"
        );
        self.diagnostics.rejected.push(Rejected {
            path,
            reason: RejectReason::ToolConfigPathInvalid(config.to_path_buf()),
        });
    }
}

/// Resolve a config-spelled path against the root, rejecting absolute
/// spellings and `..` escapes lexically (no filesystem access — the
/// escape must be caught before any read).
fn resolve_inside_root(root: &Utf8Path, configured: &str) -> Option<Utf8PathBuf> {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut components: Vec<&str> = Vec::new();
    // Absolute, UNC, or drive-qualified (`C:…`) spellings are outside
    // our contract. A bare ':' elsewhere is a legal POSIX filename byte
    // — `out/run:1` must resolve.
    let drive_qualified = {
        let bytes = trimmed.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    if trimmed.starts_with('/') || trimmed.starts_with('\\') || drive_qualified {
        return None;
    }
    for part in trimmed.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return None;
    }
    let mut resolved = root.to_path_buf();
    for part in components {
        resolved.push(part);
    }
    Some(resolved)
}

/// The Istanbul reporter artifacts a configured `reports-dir` can
/// contain, in detection-priority order.
const ISTANBUL_DIR_ARTIFACTS: &[&str] = &[
    "lcov.info",
    "coverage-final.json",
    "clover.xml",
    "cobertura-coverage.xml",
];

/// c8 / nyc JSON rc family. c8 searches exactly this list upward from
/// the CWD; we read the first present at the root, mirroring its
/// precedence. Keys: `reports-dir` (c8) / `report-dir` (nyc) name the
/// directory the Istanbul reporters write into.
fn introspect_js_rc(root: &Utf8Path, sink: &mut Sink<'_>) {
    const RC_NAMES: &[&str] = &[".c8rc", ".c8rc.json", ".nycrc", ".nycrc.json"];
    let Some(config) = RC_NAMES
        .iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file())
    else {
        return;
    };
    let Ok(bytes) = std::fs::read(&config) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        log::warn!("malformed JSON in {config}; skipping introspection");
        return;
    };
    let Some(dir) = value
        .get("reports-dir")
        .or_else(|| value.get("report-dir"))
        .and_then(|v| v.as_str())
    else {
        return; // default `coverage/` is covered by the artifact scan
    };
    let Some(resolved_dir) = resolve_inside_root(root, dir) else {
        sink.reject(&config, Utf8PathBuf::from(dir));
        return;
    };
    if !resolved_dir.is_dir() {
        sink.reject(&config, resolved_dir);
        return;
    }
    for artifact in ISTANBUL_DIR_ARTIFACTS {
        let path = resolved_dir.join(artifact);
        if path.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_string();
            sink.submit(&config, &relative);
        }
    }
}

/// `pyproject.toml` — coverage.py's `[tool.coverage.xml] output` /
/// `[tool.coverage.lcov] output`. Only *customized* outputs need
/// introspection; the defaults (`coverage.xml`, `coverage.lcov`) are in
/// the artifact-scan pattern table.
fn introspect_pyproject(root: &Utf8Path, sink: &mut Sink<'_>) {
    let config = root.join("pyproject.toml");
    if !config.is_file() {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&config) else {
        return;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        log::warn!("malformed TOML in {config}; skipping introspection");
        return;
    };
    let coverage = table.get("tool").and_then(|tool| tool.get("coverage"));
    let Some(coverage) = coverage else {
        return;
    };
    for section in ["xml", "lcov"] {
        if let Some(output) = coverage
            .get(section)
            .and_then(|s| s.get("output"))
            .and_then(|o| o.as_str())
        {
            sink.submit(&config, output);
        }
    }
}

/// `phpunit.xml` then `phpunit.xml.dist` (PHPUnit's own read order).
/// PHPUnit writes **no** coverage file unless configured, so this is
/// the only zero-config discovery path for PHP. Handles both the
/// modern `<coverage><report><clover|cobertura outputFile="…">` shape
/// (PHPUnit ≥ 9.3) and the legacy `<log type="coverage-clover"
/// target="…">` shape.
fn introspect_phpunit(root: &Utf8Path, sink: &mut Sink<'_>) {
    const CONFIG_NAMES: &[&str] = &["phpunit.xml", "phpunit.xml.dist"];
    let Some(config) = CONFIG_NAMES
        .iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file())
    else {
        return;
    };
    let Ok(bytes) = std::fs::read(&config) else {
        return;
    };

    let mut reader = quick_xml::reader::Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut outputs: Vec<String> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Err(error) => {
                log::warn!("malformed XML in {config}: {error}; skipping introspection");
                return;
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(quick_xml::events::Event::Start(ref e))
            | Ok(quick_xml::events::Event::Empty(ref e)) => match e.name().as_ref() {
                b"clover" | b"cobertura" => {
                    if let Some(output) = attr(e, b"outputFile") {
                        outputs.push(output);
                    }
                }
                b"log" => {
                    // Legacy PHPUnit < 9.3 logging block.
                    let kind = attr(e, b"type");
                    if matches!(
                        kind.as_deref(),
                        Some("coverage-clover") | Some("coverage-cobertura")
                    ) && let Some(target) = attr(e, b"target")
                    {
                        outputs.push(target);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }

    for output in outputs {
        sink.submit(&config, &output);
    }
}

/// `tarpaulin.toml` / `.tarpaulin.toml` (cargo-tarpaulin). Every
/// top-level table is a run profile — plus the reserved `[report]`
/// table, which only affects reporting — and any of them may carry
/// `out = ["Xml", "Lcov", …]` with an optional `output-dir`
/// redirect. The output *file names* are fixed by tarpaulin
/// (`cobertura.xml`, `lcov.info` inside `output-dir`), so only the
/// directory is configuration. Defaults need no introspection: without
/// `output-dir` the files land in the project root, which the artifact
/// scan's `**/cobertura.xml` / `**/lcov.info` patterns already match —
/// introspection recovers redirects into scan-pruned territory (e.g.
/// `output-dir = "target/cov"`, where the walk's `target/` descent
/// admits only `llvm-cov|tarpaulin|site`).
fn introspect_tarpaulin(root: &Utf8Path, sink: &mut Sink<'_>) {
    const CONFIG_NAMES: &[&str] = &["tarpaulin.toml", ".tarpaulin.toml"];
    let Some(config) = CONFIG_NAMES
        .iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file())
    else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&config) else {
        return;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        log::warn!("malformed TOML in {config}; skipping introspection");
        return;
    };

    // `out` and `output-dir` need not share a table: the reserved
    // `[report]` table applies its reporting options to every run
    // profile, so `out = ["Xml"]` under `[report]` combines with an
    // `output-dir` set in a profile. Collect the union of both keys
    // across all tables and emit the cross-product — every candidate
    // is existence-checked and content-sniffed before anything
    // believes it, so an over-approximate pair costs one stat call.
    let mut dirs: Vec<&str> = Vec::new();
    let mut artifacts: Vec<&str> = Vec::new();
    for profile in table.values() {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        if let Some(dir) = profile.get("output-dir").and_then(|v| v.as_str())
            && !dirs.contains(&dir)
        {
            dirs.push(dir);
        }
        for format in profile
            .get("out")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            // Ingestable formats only: Html/Json/Markdown/Stdout are
            // not report formats mehen parses. Values are PascalCase
            // per tarpaulin's `OutputFile` enum; compare loosely so a
            // hand-written lowercase spelling still resolves.
            let artifact = match format.to_ascii_lowercase().as_str() {
                "xml" => "cobertura.xml",
                "lcov" => "lcov.info",
                _ => continue,
            };
            if !artifacts.contains(&artifact) {
                artifacts.push(artifact);
            }
        }
    }
    // Without `output-dir` the fixed-name files land in the project
    // root — scan territory, no introspection needed.
    for dir in dirs {
        for artifact in &artifacts {
            let configured = format!("{}/{artifact}", dir.trim_end_matches(['/', '\\']));
            sink.submit(&config, &configured);
        }
    }
}

fn attr(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    let attribute = e.try_get_attribute(name).ok()??;
    attribute
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()
        .map(|v| v.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_inside_root_rejects_escapes() {
        let root = Utf8Path::new("/repo");
        assert_eq!(
            resolve_inside_root(root, "build/logs/clover.xml"),
            Some(Utf8PathBuf::from("/repo/build/logs/clover.xml"))
        );
        assert_eq!(
            resolve_inside_root(root, "./reports/../reports/cov.xml"),
            Some(Utf8PathBuf::from("/repo/reports/cov.xml"))
        );
        assert_eq!(resolve_inside_root(root, "../../etc/passwd"), None);
        assert_eq!(resolve_inside_root(root, "/etc/passwd"), None);
        assert_eq!(resolve_inside_root(root, "C:\\windows\\system32"), None);
        assert_eq!(resolve_inside_root(root, ""), None);
        assert_eq!(resolve_inside_root(root, "."), None);
        // A ':' inside a segment is a legal POSIX filename byte, not a
        // drive qualifier.
        assert_eq!(
            resolve_inside_root(root, "out/run:1/lcov.info"),
            Some(Utf8PathBuf::from("/repo/out/run:1/lcov.info"))
        );
    }
}
