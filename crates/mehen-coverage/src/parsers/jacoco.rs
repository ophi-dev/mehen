// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>
//
// Adapted from covrs (https://github.com/scttnlsn/covrs)
// `src/parsers/jacoco.rs`, MIT-licensed by Scott Nelson. Local changes:
// house error type, camino paths, quick-xml 0.41 API, normalization
// before emit. See LICENSE-THIRD-PARTY.

//! Parser for JaCoCo XML coverage reports.
//!
//! JaCoCo XML structure:
//! ```text
//! <report name="...">
//!   <sessioninfo id="..." start="..." dump="..."/>
//!   <package name="com/example">
//!     <class name="com/example/Foo" sourcefilename="Foo.java">
//!       <method name="doStuff" desc="()V" line="10">
//!         <counter type="INSTRUCTION" missed="0" covered="5"/>
//!         <counter type="METHOD" missed="0" covered="1"/>
//!       </method>
//!     </class>
//!     <sourcefile name="Foo.java">
//!       <line nr="10" mi="0" ci="3" mb="0" cb="2"/>
//!     </sourcefile>
//!   </package>
//! </report>
//! ```
//!
//! Key differences from Cobertura:
//! - Line-level data lives inside `<sourcefile>` elements, not `<class>`.
//! - Each `<line>` has `nr` (line number), `mi`/`ci` (missed/covered
//!   instructions), and `mb`/`cb` (missed/covered branches).
//! - There is no per-line `hits` attribute; hit status derives from
//!   whether `ci > 0`.
//! - Method coverage comes from `<method>` elements inside `<class>`.
//! - Paths are `package name + source filename` — *Java package* paths
//!   (`com/example/Foo.java`), missing any `src/main/java/` prefix; the
//!   suffix matcher in [`crate::CoverageIndex`] restores the mapping.

use std::collections::HashMap;
use std::io::BufRead;

use camino::Utf8Path;
use quick_xml::events::Event;

use super::{CoverageFormat, CoverageParser, get_attr};
use crate::Result;
use crate::model::{BranchCoverage, FileCoverage, FunctionCoverage, LineCoverage};

/// JaCoCo XML format parser.
pub struct JacocoParser;

impl CoverageParser for JacocoParser {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::Jacoco
    }

    fn can_parse(&self, _path: &Utf8Path, content: &[u8]) -> bool {
        let head = super::sniff_head(content);
        // XML with a <report element and either DTD reference or JaCoCo-
        // specific child elements (sessioninfo, package, etc.)
        super::looks_like_xml(&head)
            && head.contains("<report")
            && (head.contains("jacoco") || head.contains("JACOCO") || head.contains("<package"))
    }

    fn parse_streaming(
        &self,
        reader: &mut dyn BufRead,
        emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
    ) -> Result<()> {
        parse_streaming(reader, emit)
    }
}

/// Parse JaCoCo XML coverage data from raw bytes.
pub fn parse(input: &[u8]) -> Result<crate::CoverageData> {
    let mut data = crate::CoverageData::new();
    parse_streaming(&mut &*input, &mut |file| {
        data.files.push(file);
        Ok(())
    })?;
    Ok(data)
}

/// Streaming JaCoCo parser — calls `emit` once per `</sourcefile>`.
fn parse_streaming(
    reader: &mut dyn BufRead,
    emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
) -> Result<()> {
    let mut xml = super::xml_reader(reader);
    let mut buf = Vec::new();

    // State tracking
    let mut current_package: Option<String> = None;
    let mut current_sourcefile: Option<FileCoverage> = None;
    let mut branch_indices: HashMap<u32, u32> = HashMap::new();

    // Method tracking: methods are collected from <class> elements and
    // later attached to the corresponding <sourcefile> in the same
    // package. Key: (package_name, source_filename) → Vec<FunctionCoverage>
    let mut class_methods: HashMap<(String, String), Vec<FunctionCoverage>> = HashMap::new();
    let mut current_class_source: Option<String> = None;
    let mut in_method = false;
    let mut current_method_name: Option<String> = None;
    let mut current_method_line: Option<u32> = None;
    let mut method_hit: bool = false;

    let mut emit_normalized = |mut file: FileCoverage| {
        file.normalize();
        emit(file)
    };

    loop {
        let event = xml.read_event_into(&mut buf);
        let is_start_event = matches!(&event, Ok(Event::Start(_)));
        match event {
            Err(e) => return Err(super::xml_err(e, &xml)),
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.name().as_ref() {
                    b"package" => {
                        current_package = get_attr(e, b"name");
                    }
                    b"class" if is_start_event => {
                        current_class_source = get_attr(e, b"sourcefilename");
                    }
                    b"method" => {
                        in_method = true;
                        current_method_name = get_attr(e, b"name");
                        current_method_line =
                            get_attr(e, b"line").and_then(|v| v.parse::<u32>().ok());
                        method_hit = false;
                    }
                    b"counter" if in_method => {
                        // Check the METHOD counter inside a <method> to
                        // determine whether the method was executed.
                        if let Some(counter_type) = get_attr(e, b"type")
                            && counter_type == "METHOD"
                        {
                            let covered: u64 = get_attr(e, b"covered")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                            if covered > 0 {
                                method_hit = true;
                            }
                        }
                    }
                    b"sourcefile" => {
                        if let Some(name) = get_attr(e, b"name") {
                            let path = match &current_package {
                                Some(pkg) => format!("{pkg}/{name}"),
                                None => name.clone(),
                            };
                            let mut file = FileCoverage::new(path);
                            branch_indices.clear();

                            // Attach methods collected from <class> elements
                            // that reference this source file.
                            if let Some(pkg) = &current_package {
                                let key = (pkg.clone(), name);
                                if let Some(methods) = class_methods.remove(&key) {
                                    file.functions = methods;
                                }
                            }
                            current_sourcefile = Some(file);
                        }
                    }
                    b"line" => {
                        if let Some(file) = current_sourcefile.as_mut() {
                            let mut nr: Option<u32> = None;
                            let mut ci: u64 = 0;
                            let mut mi: u64 = 0;
                            let mut cb: u32 = 0;
                            let mut mb: u32 = 0;

                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"nr" => {
                                        nr = super::attr_str(&attr).and_then(|v| v.parse().ok());
                                    }
                                    b"ci" => {
                                        ci = super::attr_str(&attr)
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(0);
                                    }
                                    b"mi" => {
                                        mi = super::attr_str(&attr)
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(0);
                                    }
                                    b"cb" => {
                                        cb = super::attr_str(&attr)
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(0);
                                    }
                                    b"mb" => {
                                        mb = super::attr_str(&attr)
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(0);
                                    }
                                    _ => {}
                                }
                            }

                            if let Some(line_number) = nr {
                                // A line is "hit" if any instructions were
                                // covered. Use ci as the hit count; if ci is
                                // 0 and mi > 0 the line is instrumentable
                                // but missed. Only emit the line when at
                                // least one instruction exists (ci + mi > 0),
                                // otherwise it's non-instrumentable (e.g.
                                // comments, blank lines).
                                if ci > 0 || mi > 0 {
                                    file.lines.push(LineCoverage {
                                        line_number,
                                        hit_count: ci,
                                    });
                                }

                                // Branch coverage
                                let total_branches =
                                    cb.saturating_add(mb).min(super::MAX_BRANCHES_PER_LINE);
                                if total_branches > 0 {
                                    let idx = branch_indices.entry(line_number).or_insert(0);
                                    for i in 0..total_branches {
                                        let branch_hit: u64 = u64::from(i < cb);
                                        file.branches.push(BranchCoverage {
                                            line_number,
                                            branch_index: *idx,
                                            hit_count: branch_hit,
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
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"package" => {
                    current_package = None;
                }
                b"class" => {
                    current_class_source = None;
                }
                b"method" => {
                    if in_method {
                        if let (Some(pkg), Some(src), Some(name)) = (
                            &current_package,
                            &current_class_source,
                            current_method_name.take(),
                        ) {
                            let key = (pkg.clone(), src.clone());
                            class_methods
                                .entry(key)
                                .or_default()
                                .push(FunctionCoverage {
                                    name,
                                    start_line: current_method_line,
                                    end_line: None,
                                    hit_count: u64::from(method_hit),
                                });
                        }
                        in_method = false;
                        current_method_name = None;
                        current_method_line = None;
                    }
                }
                b"sourcefile" => {
                    if let Some(file) = current_sourcefile.take() {
                        emit_normalized(file)?;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }

    // Handle unclosed sourcefile
    if let Some(file) = current_sourcefile.take() {
        emit_normalized(file)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_jacoco() {
        let input = include_bytes!("../../tests/fixtures/sample_jacoco.xml");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 2);

        let foo = &data.files[0];
        assert_eq!(foo.path, "com/example/Foo.java");
        assert_eq!(foo.lines.len(), 5);
        assert_eq!(foo.lines[0].line_number, 3);
        assert_eq!(foo.lines[0].hit_count, 3); // ci=3
        assert_eq!(foo.lines[1].line_number, 10);
        assert_eq!(foo.lines[1].hit_count, 5); // ci=5
        assert_eq!(foo.lines[2].line_number, 11);
        assert_eq!(foo.lines[2].hit_count, 5); // ci=5
        assert_eq!(foo.lines[3].line_number, 12);
        assert_eq!(foo.lines[3].hit_count, 0); // ci=0, mi=2 → missed
        assert_eq!(foo.lines[4].line_number, 15);
        assert_eq!(foo.lines[4].hit_count, 3); // ci=3

        // Branch on line 11: cb=1, mb=1 → 2 branch arms
        assert_eq!(foo.branches.len(), 2);
        assert_eq!(foo.branches[0].line_number, 11);
        assert_eq!(foo.branches[0].hit_count, 1); // covered arm
        assert_eq!(foo.branches[1].line_number, 11);
        assert_eq!(foo.branches[1].hit_count, 0); // missed arm

        // Methods extracted from <class>
        assert_eq!(foo.functions.len(), 2);
        assert_eq!(foo.functions[0].name, "<init>");
        assert_eq!(foo.functions[0].start_line, Some(3));
        assert_eq!(foo.functions[0].hit_count, 1);
        assert_eq!(foo.functions[1].name, "doStuff");
        assert_eq!(foo.functions[1].start_line, Some(10));
        assert_eq!(foo.functions[1].hit_count, 1);

        let bar = &data.files[1];
        assert_eq!(bar.path, "com/example/Bar.java");
        assert_eq!(bar.lines.len(), 2);
        assert_eq!(bar.branches.len(), 0);
    }

    #[test]
    fn test_parse_jacoco_no_package() {
        let input = include_bytes!("../../tests/fixtures/jacoco_no_package.xml");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 1);
        // Without a package, path is just the source filename.
        assert_eq!(data.files[0].path, "App.java");
        assert_eq!(data.files[0].lines.len(), 2);
    }

    #[test]
    fn test_parse_jacoco_empty() {
        let input = include_bytes!("../../tests/fixtures/empty_jacoco.xml");
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 0);
    }

    #[test]
    fn test_parse_jacoco_malformed() {
        let input = include_bytes!("../../tests/fixtures/malformed_jacoco.xml");
        let result = parse(input);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("position"),
            "Error should contain position info: {err_msg}",
        );
    }

    #[test]
    fn test_can_parse_jacoco() {
        let parser = JacocoParser;

        // JaCoCo with DTD reference
        let content = br#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE report PUBLIC "-//JACOCO//DTD Report 1.1//EN" "report.dtd"><report name="test">"#;
        assert!(parser.can_parse(Utf8Path::new("jacoco.xml"), content));

        // JaCoCo without DTD but with <package>
        let content = br#"<?xml version="1.0"?><report name="test"><package name="com/example">"#;
        assert!(parser.can_parse(Utf8Path::new("report.xml"), content));

        // Cobertura should NOT match
        let content = br#"<?xml version="1.0"?><coverage version="1.0">"#;
        assert!(!parser.can_parse(Utf8Path::new("coverage.xml"), content));
    }
}
