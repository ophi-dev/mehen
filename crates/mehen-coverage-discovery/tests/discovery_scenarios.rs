// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! End-to-end discovery scenarios over a synthetic monorepo tempdir:
//! pattern hits inside gitignored/hidden directories, targeted descent,
//! pruning, content-sniff disambiguation, tool-config introspection,
//! same-directory supersede, `TestResults` re-run clustering, and
//! determinism of the whole outcome.

use camino::{Utf8Path, Utf8PathBuf};
use mehen_coverage_discovery::{DiscoveryOptions, DiscoveryOutcome, ReportOrigin, discover};

const LCOV: &str = "TN:\nSF:src/app.js\nDA:1,1\nDA:2,0\nend_of_record\n";
const ISTANBUL: &str = r#"{"/w/src/app.js": {"statementMap": {"0": {"start": {"line": 1}}}, "s": {"0": 1}, "fnMap": {}, "f": {}}}"#;
const CLOVER: &str = r#"<?xml version="1.0"?><coverage generated="1" clover="4.4.1"><project><package><file name="app.js" path="/w/src/app.js"><line num="1" count="1" type="stmt"/></file></package></project></coverage>"#;
const JACOCO: &str = r#"<?xml version="1.0"?><report name="pester"><package name="scripts"><sourcefile name="run.ps1"><line nr="1" ci="1" mi="0"/></sourcefile></package></report>"#;
const COBERTURA: &str = r#"<?xml version="1.0"?><coverage version="7.4"><sources><source>/w</source></sources><packages><package name="p"><classes><class name="app" filename="src/app.py"><lines><line number="1" hits="1"/></lines></class></classes></package></packages></coverage>"#;
const GOCOVER: &str = "mode: set\nexample.com/m/pkg/a.go:1.1,2.2 1 1\n";

fn write(root: &Utf8Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn set_mtime(root: &Utf8Path, relative: &str, seconds: u64) {
    let path = root.join(relative);
    let file = std::fs::File::options()
        .write(true)
        .open(path.as_std_path())
        .unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
        .unwrap();
}

/// Flatten an outcome into stable, root-relative one-line records so
/// insta snapshots stay platform- and tempdir-independent.
fn projection(root: &Utf8Path, outcome: &DiscoveryOutcome) -> Vec<String> {
    let rel = |path: &Utf8PathBuf| -> String {
        path.strip_prefix(root)
            .map_or_else(|_| path.to_string(), ToString::to_string)
    };
    let mut lines: Vec<String> = outcome
        .reports
        .iter()
        .map(|report| {
            let origin = match &report.origin {
                ReportOrigin::ToolConfig(config) => format!("tool-config:{}", rel(config)),
                ReportOrigin::Scan => "scan".to_string(),
            };
            format!("report {} {} ({origin})", report.format, rel(&report.path))
        })
        .collect();
    for rejected in &outcome.diagnostics.rejected {
        let reason = match &rejected.reason {
            mehen_coverage_discovery::RejectReason::Superseded(kept) => {
                format!("Superseded by {}", rel(kept))
            }
            mehen_coverage_discovery::RejectReason::OlderRun(kept) => {
                format!("OlderRun kept {}", rel(kept))
            }
            mehen_coverage_discovery::RejectReason::ToolConfigPathInvalid(config) => {
                format!("ToolConfigPathInvalid from {}", rel(config))
            }
            other => format!("{other:?}"),
        };
        lines.push(format!("rejected {} {reason}", rel(&rejected.path)));
    }
    for cap in &outcome.diagnostics.caps_hit {
        lines.push(format!("cap {cap}"));
    }
    lines
}

fn build_monorepo(root: &Utf8Path) {
    // Jest triple in a (conventionally gitignored) coverage/ dir.
    write(root, "coverage/lcov.info", LCOV);
    write(root, "coverage/coverage-final.json", ISTANBUL);
    write(root, "coverage/clover.xml", CLOVER);
    // c8 raw V8 staging — must not be visited.
    write(root, "coverage/tmp/raw.json", ISTANBUL);
    // Hidden nyc shard dir — must be visited.
    write(root, ".nyc_output/aaa.json", ISTANBUL);
    // Rust: cargo-llvm-cov artifact inside target/ (targeted descent).
    write(root, "target/llvm-cov/lcov.info", LCOV);
    // Compiler output — must not be visited.
    write(root, "target/debug/deps/junk.lcov", LCOV);
    // PHP: phpunit.xml.dist names the clover report (Jenkins layout).
    write(
        root,
        "phpunit.xml.dist",
        r#"<?xml version="1.0"?><phpunit><coverage><report><clover outputFile="build/logs/clover.xml"/></report></coverage></phpunit>"#,
    );
    write(root, "build/logs/clover.xml", CLOVER);
    // Gradle output dir that is not a report location — not visited.
    write(root, "build/classes/junk.lcov", LCOV);
    // dotnet coverlet re-runs: GUID dirs under TestResults/.
    write(
        root,
        "TestResults/aaaa-1111/coverage.cobertura.xml",
        COBERTURA,
    );
    write(
        root,
        "TestResults/bbbb-2222/coverage.cobertura.xml",
        COBERTURA,
    );
    set_mtime(root, "TestResults/aaaa-1111/coverage.cobertura.xml", 1_000);
    set_mtime(root, "TestResults/bbbb-2222/coverage.cobertura.xml", 2_000);
    // The coverage.xml name collision: coverage.py (Cobertura) vs
    // Pester (JaCoCo) — content sniffing separates them.
    write(root, "py/coverage.xml", COBERTURA);
    write(root, "ps/coverage.xml", JACOCO);
    // Go conventions.
    write(root, "go/coverage.out", GOCOVER);
    // Python: pyproject.toml routes the XML report to a custom path.
    write(
        root,
        "pyproject.toml",
        "[tool.coverage.xml]\noutput = \"qa/cov.xml\"\n",
    );
    write(root, "qa/cov.xml", COBERTURA);
    // Pruned locations.
    write(root, "node_modules/pkg/lcov.info", LCOV);
    write(root, "vendor/lib/lcov.info", LCOV);
    // Pattern hits that fail validation.
    write(root, "notes/coverage.txt", "a plain text summary\n");
    write(root, "empty.lcov", "");
}

#[test]
fn monorepo_discovery_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    build_monorepo(root);

    let outcome = discover(&DiscoveryOptions {
        roots: vec![root.to_path_buf()],
        ..Default::default()
    });

    // The observability counter must reflect the walk (regression: it
    // used to stay 0 while dirents_left was decremented).
    assert!(
        outcome.diagnostics.dirents_visited > 0,
        "dirents_visited must count visited entries"
    );

    insta::assert_yaml_snapshot!("monorepo_discovery", projection(root, &outcome));
}

#[test]
fn discovery_is_deterministic_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    build_monorepo(root);

    let options = DiscoveryOptions {
        roots: vec![root.to_path_buf()],
        ..Default::default()
    };
    let first = projection(root, &discover(&options));
    let second = projection(root, &discover(&options));
    assert_eq!(first, second);

    // A copy of the same tree at a different absolute path produces the
    // same relative outcome.
    let dir2 = tempfile::tempdir().unwrap();
    let root2 = Utf8Path::from_path(dir2.path()).unwrap();
    build_monorepo(root2);
    let elsewhere = projection(
        root2,
        &discover(&DiscoveryOptions {
            roots: vec![root2.to_path_buf()],
            ..Default::default()
        }),
    );
    assert_eq!(first, elsewhere);
}

#[test]
fn extra_patterns_lift_prune_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    write(root, "node_modules/.cache/cov/lcov.info", LCOV);

    let none = discover(&DiscoveryOptions {
        roots: vec![root.to_path_buf()],
        ..Default::default()
    });
    assert!(none.reports.is_empty());

    let lifted = discover(&DiscoveryOptions {
        roots: vec![root.to_path_buf()],
        extra_patterns: vec!["node_modules/.cache/**/lcov.info".to_string()],
        ..Default::default()
    });
    assert_eq!(lifted.reports.len(), 1);
    assert!(
        lifted.reports[0]
            .path
            .ends_with("node_modules/.cache/cov/lcov.info")
    );
}

#[test]
#[cfg(unix)]
fn symlink_escape_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.info");
    std::fs::write(&secret, LCOV).unwrap();
    std::fs::create_dir_all(root.join("coverage").as_std_path()).unwrap();
    std::os::unix::fs::symlink(&secret, root.join("coverage/lcov.info").as_std_path()).unwrap();

    let outcome = discover(&DiscoveryOptions {
        roots: vec![root.to_path_buf()],
        ..Default::default()
    });
    assert!(outcome.reports.is_empty());
    assert!(
        outcome
            .diagnostics
            .rejected
            .iter()
            .any(|r| format!("{:?}", r.reason) == "SymlinkEscape"),
        "expected a SymlinkEscape rejection, got {:?}",
        outcome.diagnostics.rejected
    );
}

#[test]
fn nyc_shard_flood_hits_per_dir_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap();
    for i in 0..80 {
        write(root, &format!(".nyc_output/{i:04}.json"), ISTANBUL);
    }

    let outcome = discover(&DiscoveryOptions {
        roots: vec![root.to_path_buf()],
        ..Default::default()
    });
    // Default per-dir cap is 64; the name-sorted walk keeps the
    // lexicographically first shards.
    assert_eq!(outcome.reports.len(), 64);
    assert!(
        outcome
            .diagnostics
            .caps_hit
            .contains(&"per_dir_candidates".to_string())
    );
    assert!(outcome.reports[0].path.ends_with(".nyc_output/0000.json"));
}
