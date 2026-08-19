// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>
//
// Adapted from covrs (https://github.com/scttnlsn/covrs)
// `src/parsers/clover.rs`, MIT-licensed by Scott Nelson. Local changes:
// house error type, camino paths, quick-xml 0.41 API, normalization
// before emit. See LICENSE-THIRD-PARTY.

//! Parser for Clover XML coverage reports.
//!
//! Clover XML structure (as produced by OpenClover, Atlassian Clover, and
//! plugins like `jest --coverageReporters=clover`, PHPUnit, etc.):
//! ```text
//! <coverage generated="..." clover="4.x.x">
//!   <project timestamp="..." name="...">
//!     <package name="...">
//!       <file name="Foo.py" path="/absolute/path/to/Foo.py">
//!         <line num="1" count="5" type="stmt"/>
//!         <line num="3" count="2" type="method" signature="do_stuff()"/>
//!         <line num="5" count="1" type="cond" truecount="1" falsecount="1"/>
//!       </file>
//!     </package>
//!   </project>
//! </coverage>
//! ```
//!
//! Key differences from Cobertura:
//! - Root element is `<coverage>` with a `clover` attribute (version).
//! - Files live inside `<package>` → `<file>`.
//! - Each `<line>` has `num`, `count`, and `type` (stmt|method|cond).
//! - Methods are `<line type="method" signature="...">` entries.
//! - Branch coverage is expressed via `truecount`/`falsecount` on
//!   `<line type="cond">` elements.
//! - `<file>` has a `path` attribute with the absolute path and a `name`
//!   attribute with just the filename. `path` is preferred when present.

use std::collections::HashMap;
use std::io::BufRead;

use camino::Utf8Path;
use quick_xml::events::Event;

use super::{CoverageFormat, CoverageParser, get_attr};
use crate::Result;
use crate::model::{BranchCoverage, FileCoverage, FunctionCoverage, LineCoverage};

/// Clover XML format parser.
pub(crate) struct CloverParser;

impl CoverageParser for CloverParser {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::Clover
    }

    fn can_parse(&self, _path: &Utf8Path, content: &[u8]) -> bool {
        let head = super::sniff_head(content);
        // Clover XML has a <coverage element with a `clover` attribute
        // that distinguishes it from Cobertura (which also uses
        // <coverage> as root).
        super::looks_like_xml(&head) && head.contains("<coverage") && head.contains("clover=")
    }

    fn parse_streaming(
        &self,
        reader: &mut dyn BufRead,
        emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
    ) -> Result<()> {
        parse_streaming(reader, emit)
    }
}

/// Parse Clover XML coverage data from raw bytes.
#[cfg(test)]
pub(crate) fn parse(input: &[u8]) -> Result<crate::CoverageData> {
    let mut data = crate::CoverageData::new();
    parse_streaming(&mut &*input, &mut |file| {
        data.files.push(file);
        Ok(())
    })?;
    Ok(data)
}

/// Streaming Clover parser — calls `emit` once per `</file>`.
fn parse_streaming(
    reader: &mut dyn BufRead,
    emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
) -> Result<()> {
    let mut xml = super::xml_reader(reader);
    let mut buf = Vec::new();

    // State tracking
    let mut current_file: Option<FileCoverage> = None;
    let mut branch_indices: HashMap<u32, u32> = HashMap::new();

    let mut emit_normalized = |mut file: FileCoverage| {
        file.normalize();
        emit(file)
    };

    loop {
        let event = xml.read_event_into(&mut buf);
        match event {
            Err(e) => return Err(super::xml_err(e, &xml)),
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.name().as_ref() {
                    b"file" => {
                        // Prefer the `path` attribute (absolute) over
                        // `name` (basename).
                        let file_path = get_attr(e, b"path")
                            .or_else(|| get_attr(e, b"name"))
                            .unwrap_or_default();
                        current_file = Some(FileCoverage::new(file_path));
                        branch_indices.clear();
                    }
                    b"line" => {
                        if let Some(file) = current_file.as_mut() {
                            let mut num: Option<u32> = None;
                            let mut count: u64 = 0;
                            let mut line_type: Option<String> = None;
                            let mut signature: Option<String> = None;
                            let mut truecount: Option<u32> = None;
                            let mut falsecount: Option<u32> = None;

                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"num" => {
                                        num = super::attr_str(&attr).and_then(|v| v.parse().ok());
                                    }
                                    b"count" => {
                                        count = super::attr_str(&attr)
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(0);
                                    }
                                    b"type" => {
                                        line_type = super::attr_str(&attr);
                                    }
                                    b"signature" => {
                                        signature = super::attr_str(&attr);
                                    }
                                    b"truecount" => {
                                        truecount =
                                            super::attr_str(&attr).and_then(|v| v.parse().ok());
                                    }
                                    b"falsecount" => {
                                        falsecount =
                                            super::attr_str(&attr).and_then(|v| v.parse().ok());
                                    }
                                    _ => {}
                                }
                            }

                            if let Some(line_number) = num {
                                // Line coverage — always emit.
                                file.lines.push(LineCoverage {
                                    line_number,
                                    hit_count: count,
                                });

                                // Method/function coverage — type="method"
                                // lines represent function entry points.
                                if line_type.as_deref() == Some("method") {
                                    let name = signature
                                        .unwrap_or_else(|| format!("<anonymous@{line_number}>"));
                                    file.functions.push(FunctionCoverage {
                                        name,
                                        start_line: Some(line_number),
                                        end_line: None,
                                        hit_count: count,
                                    });
                                }

                                // Branch coverage — type="cond" lines
                                // carry truecount/falsecount, the
                                // *execution counts* of the condition's
                                // true and false outcomes (OpenClover
                                // semantics). Each condition is exactly
                                // two arms: the true arm (hit iff
                                // truecount > 0) and the false arm (hit
                                // iff falsecount > 0). The counts must
                                // not be expanded into arms — a hot
                                // condition with truecount="10",
                                // falsecount="5" is still one condition
                                // with both outcomes exercised.
                                if line_type.as_deref() == Some("cond") {
                                    let tc = truecount.unwrap_or(0);
                                    let fc = falsecount.unwrap_or(0);
                                    let idx = branch_indices.entry(line_number).or_insert(0);
                                    for hit in [u64::from(tc > 0), u64::from(fc > 0)] {
                                        file.branches.push(BranchCoverage {
                                            line_number,
                                            branch_index: *idx,
                                            hit_count: hit,
                                        });
                                        *idx += 1;
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"file"
                    && let Some(file) = current_file.take()
                {
                    emit_normalized(file)?;
                }
            }
            _ => {}
        }
        buf.clear();
    }

    // Handle unclosed file
    if let Some(file) = current_file.take() {
        emit_normalized(file)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_clover() {
        let input = include_bytes!("../../tests/fixtures/sample_clover.xml");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 2);

        let main = &data.files[0];
        assert_eq!(main.path, "/home/user/project/src/main.py");
        assert_eq!(main.lines.len(), 8);
        assert_eq!(main.lines[0].line_number, 1);
        assert_eq!(main.lines[0].hit_count, 1);
        assert_eq!(main.lines[2].line_number, 3);
        assert_eq!(main.lines[2].hit_count, 0);

        // Branch on line 8: type="cond" truecount="1" falsecount="1"
        // → 1 condition × 2 arms = 2 branch entries, both hit
        assert_eq!(main.branches.len(), 2);
        assert_eq!(main.branches[0].line_number, 8);
        assert_eq!(main.branches[0].hit_count, 1); // true arm covered
        assert_eq!(main.branches[1].line_number, 8);
        assert_eq!(main.branches[1].hit_count, 1); // false arm covered

        // One method extracted (line 5, type="method")
        assert_eq!(main.functions.len(), 1);
        assert_eq!(main.functions[0].name, "do_stuff()");
        assert_eq!(main.functions[0].start_line, Some(5));
        assert_eq!(main.functions[0].hit_count, 3);

        let util = &data.files[1];
        assert_eq!(util.path, "/home/user/project/src/util.py");
        assert_eq!(util.lines.len(), 2);
        assert_eq!(util.branches.len(), 0);
    }

    #[test]
    fn test_parse_clover_empty() {
        // A valid Clover file with no files should produce empty data.
        let input = include_bytes!("../../tests/fixtures/empty_clover.xml");
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 0);
    }

    #[test]
    fn test_parse_clover_malformed() {
        // Malformed XML should produce a meaningful error with position
        // info.
        let input = include_bytes!("../../tests/fixtures/malformed_clover.xml");
        let result = parse(input);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("position"),
            "Error should contain position info: {err_msg}",
        );
    }

    #[test]
    fn test_can_parse_clover() {
        let parser = CloverParser;

        // Clover XML with clover= attribute
        let content = br#"<?xml version="1.0"?><coverage generated="123" clover="4.4.1"><project>"#;
        assert!(parser.can_parse(Utf8Path::new("clover.xml"), content));

        // Cobertura should NOT match (no clover= attribute)
        let content = br#"<?xml version="1.0"?><coverage version="1.0">"#;
        assert!(!parser.can_parse(Utf8Path::new("coverage.xml"), content));

        // JaCoCo should NOT match
        let content = br#"<?xml version="1.0"?><report name="test"><package>"#;
        assert!(!parser.can_parse(Utf8Path::new("report.xml"), content));
    }

    #[test]
    fn test_parse_clover_no_path_attr() {
        // When <file> has no `path` attribute, fall back to `name`.
        let input = br#"<?xml version="1.0"?>
<coverage generated="123" clover="4.4.1">
  <project name="test">
    <package name="pkg">
      <file name="app.py">
        <line num="1" count="1" type="stmt"/>
      </file>
    </package>
  </project>
</coverage>"#;
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].path, "app.py");
    }

    #[test]
    fn test_parse_clover_branch_partially_covered() {
        // A cond line with truecount=1, falsecount=0 → 1 condition,
        // true arm hit, false arm missed.
        let input = br#"<?xml version="1.0"?>
<coverage generated="123" clover="4.4.1">
  <project name="test">
    <package name="pkg">
      <file name="branch.py" path="/src/branch.py">
        <line num="5" count="2" type="cond" truecount="1" falsecount="0"/>
      </file>
    </package>
  </project>
</coverage>"#;
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 1);
        let file = &data.files[0];

        assert_eq!(file.lines.len(), 1);
        assert_eq!(file.lines[0].hit_count, 2);

        // 1 condition × 2 arms
        assert_eq!(file.branches.len(), 2);
        assert_eq!(file.branches[0].hit_count, 1); // true arm
        assert_eq!(file.branches[1].hit_count, 0); // false arm
    }

    #[test]
    fn test_parse_clover_counts_are_executions_not_conditions() {
        // truecount/falsecount are *execution counts* of the two
        // outcomes, not a number of conditions: a hot condition with
        // truecount="10" falsecount="5" is exactly two arms, both
        // covered — never 20 arms with 15 covered.
        let input = br#"<?xml version="1.0"?>
<coverage generated="123" clover="4.4.1">
  <project name="test">
    <package name="pkg">
      <file name="hot.py" path="/src/hot.py">
        <line num="3" count="15" type="cond" truecount="10" falsecount="5"/>
      </file>
    </package>
  </project>
</coverage>"#;
        let data = parse(input).unwrap();
        let file = &data.files[0];
        assert_eq!(file.branches.len(), 2);
        assert!(file.branches.iter().all(|b| b.hit_count == 1));
        assert_eq!(
            file.branch_totals(),
            crate::SpanTotals {
                covered: 2,
                total: 2
            }
        );
    }
}
