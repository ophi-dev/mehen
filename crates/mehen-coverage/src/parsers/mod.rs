// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>
//
// Adapted from covrs (https://github.com/scttnlsn/covrs)
// `src/parsers/mod.rs`, MIT-licensed by Scott Nelson. Local changes:
// camino paths, house error type, serde on `CoverageFormat`, quick-xml
// 0.41 API. See LICENSE-THIRD-PARTY.

//! The six coverage-report format parsers and their shared detection
//! machinery. Every parser is streaming (one [`FileCoverage`] emitted per
//! source-file record) and every detector is cheap by contract: the
//! extension/filename plus content markers within the first 4 KiB.
//!
//! The per-format modules are implementation detail: external consumers
//! go through the format-neutral entry points ([`detect`],
//! [`for_format`], [`crate::detect_format`], [`crate::parse_report`]).

pub(crate) mod clover;
pub(crate) mod cobertura;
pub(crate) mod gocover;
pub(crate) mod istanbul;
pub(crate) mod jacoco;
pub(crate) mod lcov;

use std::io::BufRead;

use camino::Utf8Path;
use quick_xml::events::BytesStart;
use quick_xml::reader::Reader;

use crate::Result;
use crate::model::FileCoverage;

/// Maximum number of branch arms to emit for a single source line. Any
/// parsed branch count above this is almost certainly malformed input and
/// expanding it would consume excessive memory. Even 1024 is far beyond
/// any real-world branch count per line.
pub(crate) const MAX_BRANCHES_PER_LINE: u32 = 1024;

/// Parser for a specific coverage format.
pub trait CoverageParser {
    /// The format this parser handles.
    fn format(&self) -> CoverageFormat;

    /// Whether this parser can handle the given file, based on its path
    /// and content. Implementations must be cheap — only inspect the
    /// extension/filename and/or the first few KiB of content.
    fn can_parse(&self, path: &Utf8Path, content: &[u8]) -> bool;

    /// Streaming parse from a buffered reader: calls `emit` once per
    /// source file instead of collecting everything into memory.
    fn parse_streaming(
        &self,
        reader: &mut dyn BufRead,
        emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
    ) -> Result<()>;
}

// ── Shared helpers used by the clover, cobertura & jacoco parsers ──

/// Peek at the first 4 KiB of content as a string for format detection.
pub(crate) fn sniff_head(content: &[u8]) -> std::borrow::Cow<'_, str> {
    let n = content.len().min(4096);
    String::from_utf8_lossy(&content[..n])
}

/// Whether the given text snippet looks like XML.
pub(crate) fn looks_like_xml(head: &str) -> bool {
    head.contains("<?xml") || head.trim_start().starts_with('<')
}

/// Extract a single attribute value from an XML element.
pub(crate) fn get_attr(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    let attr = e.try_get_attribute(name).ok()??;
    attr_str(&attr)
}

/// Normalize an attribute value per XML 1.0 rules (the version every
/// supported coverage tool emits), swallowing malformed values.
pub(crate) fn attr_str(attr: &quick_xml::events::attributes::Attribute<'_>) -> Option<String> {
    attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()
        .map(|v| v.into_owned())
}

/// Create a configured XML reader from a buffered source.
///
/// quick-xml never resolves DTDs or external entities (custom entities
/// stay unexpanded), so XXE and entity-expansion attacks are structurally
/// absent — `dtd_is_inert_by_construction` in the cobertura tests pins
/// that assumption against future upgrades.
pub(crate) fn xml_reader<R: BufRead>(input: R) -> Reader<R> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().trim_text(true);
    reader
}

/// Map a quick_xml error to a crate error with buffer position context.
pub(crate) fn xml_err<R>(e: quick_xml::Error, reader: &Reader<R>) -> crate::CoverageError {
    let pos = reader.buffer_position();
    crate::CoverageError::Malformed(format!("XML parse error at position {pos}: {e}"))
}

/// Supported coverage-report formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverageFormat {
    Clover,
    Cobertura,
    Gocover,
    Istanbul,
    Jacoco,
    Lcov,
}

impl CoverageFormat {
    /// All formats in detection priority order — most-specific content
    /// markers first, so a format can never false-positive on a report
    /// that a later, laxer detector would also accept.
    ///
    /// LCOV first: `SF:`/`DA:` markers are unambiguous. Go cover next —
    /// its `mode:` header and `.go:` block pattern are equally
    /// distinctive. Istanbul before the XML formats because its JSON
    /// `statementMap`/`fnMap` markers cannot collide with XML. JaCoCo
    /// before Cobertura since both are XML but JaCoCo's `<report>` +
    /// `jacoco`/`<package>` markers are more specific than Cobertura's
    /// `<coverage>`. Clover before Cobertura because both use
    /// `<coverage>` as the root element, but Clover detection requires
    /// the `clover=` attribute.
    ///
    /// The same priority order also decides which same-directory sibling
    /// wins when one test run emits several formats at once (e.g. Jest
    /// writing `lcov.info` + `coverage-final.json` + `clover.xml`).
    pub const DETECTION_ORDER: &[CoverageFormat] = &[
        CoverageFormat::Lcov,
        CoverageFormat::Gocover,
        CoverageFormat::Istanbul,
        CoverageFormat::Jacoco,
        CoverageFormat::Clover,
        CoverageFormat::Cobertura,
    ];
}

impl std::fmt::Display for CoverageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoverageFormat::Clover => f.write_str("clover"),
            CoverageFormat::Cobertura => f.write_str("cobertura"),
            CoverageFormat::Gocover => f.write_str("gocover"),
            CoverageFormat::Istanbul => f.write_str("istanbul"),
            CoverageFormat::Jacoco => f.write_str("jacoco"),
            CoverageFormat::Lcov => f.write_str("lcov"),
        }
    }
}

impl std::str::FromStr for CoverageFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "clover" => Ok(CoverageFormat::Clover),
            "cobertura" => Ok(CoverageFormat::Cobertura),
            "gocover" | "go" => Ok(CoverageFormat::Gocover),
            "istanbul" | "nyc" => Ok(CoverageFormat::Istanbul),
            "jacoco" => Ok(CoverageFormat::Jacoco),
            "lcov" => Ok(CoverageFormat::Lcov),
            _ => Err(format!(
                "unknown coverage format '{s}' — supported: clover, cobertura, gocover, istanbul, jacoco, lcov"
            )),
        }
    }
}

/// Get the parser for a specific format.
#[must_use]
pub fn for_format(format: CoverageFormat) -> &'static dyn CoverageParser {
    match format {
        CoverageFormat::Clover => &clover::CloverParser,
        CoverageFormat::Cobertura => &cobertura::CoberturaParser,
        CoverageFormat::Gocover => &gocover::GocoverParser,
        CoverageFormat::Istanbul => &istanbul::IstanbulParser,
        CoverageFormat::Jacoco => &jacoco::JacocoParser,
        CoverageFormat::Lcov => &lcov::LcovParser,
    }
}

/// Detect the format and return the matching parser, or `None`.
#[must_use]
pub fn detect(path: &Utf8Path, content: &[u8]) -> Option<&'static dyn CoverageParser> {
    CoverageFormat::DETECTION_ORDER
        .iter()
        .map(|&f| for_format(f))
        .find(|p| p.can_parse(path, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_lcov_by_extension() {
        let parser = detect(Utf8Path::new("coverage.info"), b"").unwrap();
        assert_eq!(parser.format(), CoverageFormat::Lcov);

        let parser = detect(Utf8Path::new("coverage.lcov"), b"").unwrap();
        assert_eq!(parser.format(), CoverageFormat::Lcov);
    }

    #[test]
    fn test_detect_lcov_by_content() {
        let content = b"TN:test\nSF:/src/lib.rs\nDA:1,5\nend_of_record\n";
        let parser = detect(Utf8Path::new("coverage.txt"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Lcov);
    }

    #[test]
    fn test_detect_jacoco_by_content() {
        let content =
            b"<?xml version=\"1.0\"?>\n<report name=\"test\"><package name=\"com/example\">";
        let parser = detect(Utf8Path::new("jacoco.xml"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Jacoco);
    }

    #[test]
    fn test_detect_jacoco_by_doctype() {
        let content =
            b"<?xml version=\"1.0\"?><!DOCTYPE report PUBLIC \"-//JACOCO//DTD Report 1.1//EN\" \"report.dtd\"><report name=\"test\">";
        let parser = detect(Utf8Path::new("report.xml"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Jacoco);
    }

    #[test]
    fn test_detect_cobertura_by_content() {
        let content = b"<?xml version=\"1.0\"?>\n<coverage version=\"1.0\">";
        let parser = detect(Utf8Path::new("coverage.xml"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Cobertura);
    }

    #[test]
    fn test_detect_gocover_by_extension() {
        let parser = detect(Utf8Path::new("coverage.coverprofile"), b"").unwrap();
        assert_eq!(parser.format(), CoverageFormat::Gocover);

        let parser = detect(Utf8Path::new("coverage.gocov"), b"").unwrap();
        assert_eq!(parser.format(), CoverageFormat::Gocover);
    }

    #[test]
    fn test_detect_gocover_by_content() {
        let content = b"mode: count\ngithub.com/user/repo/main.go:10.1,20.5 3 1\n";
        let parser = detect(Utf8Path::new("coverage.out"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Gocover);
    }

    #[test]
    fn test_detect_istanbul_by_filename() {
        let parser = detect(Utf8Path::new("coverage-final.json"), b"").unwrap();
        assert_eq!(parser.format(), CoverageFormat::Istanbul);
    }

    #[test]
    fn test_detect_istanbul_by_content() {
        let content = br#"{ "/src/lib.js": { "statementMap": { "0": { "start": { "line": 1 } } }, "s": { "0": 1 }, "fnMap": {}, "f": {} } }"#;
        let parser = detect(Utf8Path::new("coverage.json"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Istanbul);
    }

    #[test]
    fn test_detect_clover_by_content() {
        let content =
            b"<?xml version=\"1.0\"?>\n<coverage generated=\"123\" clover=\"4.4.1\"><project>";
        let parser = detect(Utf8Path::new("clover.xml"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Clover);
    }

    #[test]
    fn test_detect_clover_not_cobertura() {
        // Cobertura XML should not be detected as Clover.
        let content = b"<?xml version=\"1.0\"?>\n<coverage version=\"1.0\">";
        let parser = detect(Utf8Path::new("coverage.xml"), content).unwrap();
        assert_eq!(parser.format(), CoverageFormat::Cobertura);
    }

    #[test]
    fn test_detect_unknown() {
        assert!(detect(Utf8Path::new("random.dat"), b"hello world").is_none());
        // GNU info documentation must not be claimed by the `.info`
        // extension alone… it is: extension wins for LCOV. Content that
        // is *plainly not* a coverage report but carries a coverage-ish
        // name is the artifact-scan sniffing gate's problem; here we pin
        // the fully-unrelated case only.
        assert!(detect(Utf8Path::new("notes.txt"), b"just some text").is_none());
    }

    #[test]
    fn format_round_trips_through_display_and_from_str() {
        for &format in CoverageFormat::DETECTION_ORDER {
            let spelled = format.to_string();
            assert_eq!(spelled.parse::<CoverageFormat>().unwrap(), format);
        }
        assert!("perf-profile".parse::<CoverageFormat>().is_err());
    }
}
