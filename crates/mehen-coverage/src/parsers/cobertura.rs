// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>
//
// Adapted from covrs (https://github.com/scttnlsn/covrs)
// `src/parsers/cobertura.rs`, MIT-licensed by Scott Nelson. Local
// changes: house error type, camino paths, quick-xml 0.41 API,
// hand-rolled condition-coverage parsing (drops the regex dependency),
// normalization before emit. See LICENSE-THIRD-PARTY.

//! Parser for Cobertura XML coverage reports.
//!
//! Cobertura XML structure:
//! ```text
//! <coverage>
//!   <sources><source>...</source></sources>
//!   <packages>
//!     <package name="...">
//!       <classes>
//!         <class name="..." filename="..." line-rate="...">
//!           <methods>
//!             <method name="..." ...>
//!               <lines><line number="..." hits="..." /></lines>
//!             </method>
//!           </methods>
//!           <lines>
//!             <line number="..." hits="..." branch="true|false"
//!                   condition-coverage="50% (1/2)" />
//!           </lines>
//!         </class>
//!       </classes>
//!     </package>
//!   </packages>
//! </coverage>
//! ```
//!
//! File paths are `<class filename="…">` values — usually relative —
//! resolved against the first non-empty `<source>` root when present.

use std::collections::HashMap;
use std::io::BufRead;

use camino::Utf8Path;
use quick_xml::events::Event;

use super::{CoverageFormat, CoverageParser, get_attr};
use crate::Result;
use crate::model::{BranchCoverage, FileCoverage, FunctionCoverage, LineCoverage};

/// Cobertura XML format parser.
pub(crate) struct CoberturaParser;

impl CoverageParser for CoberturaParser {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::Cobertura
    }

    fn can_parse(&self, _path: &Utf8Path, content: &[u8]) -> bool {
        let head = super::sniff_head(content);
        super::looks_like_xml(&head) && head.contains("<coverage")
    }

    fn parse_streaming(
        &self,
        reader: &mut dyn BufRead,
        emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
    ) -> Result<()> {
        parse_streaming(reader, emit)
    }
}

/// Parse Cobertura XML coverage data from raw bytes.
#[cfg(test)]
pub(crate) fn parse(input: &[u8]) -> Result<crate::CoverageData> {
    let mut data = crate::CoverageData::new();
    parse_streaming(&mut &*input, &mut |file| {
        data.files.push(file);
        Ok(())
    })?;
    Ok(data)
}

/// Extract `(covered, total)` from a Cobertura `condition-coverage`
/// attribute like `"75% (3/4)"`. Replaces the upstream regex with a
/// hand-rolled scan so the crate carries no regex dependency.
fn parse_condition_fraction(cond: &str) -> Option<(u32, u32)> {
    let open = cond.find('(')?;
    let rest = &cond[open + 1..];
    let close = rest.find(')')?;
    let (covered, total) = rest[..close].split_once('/')?;
    Some((covered.trim().parse().ok()?, total.trim().parse().ok()?))
}

/// Streaming Cobertura parser — calls `emit` once per `</class>`.
fn parse_streaming(
    reader: &mut dyn BufRead,
    emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
) -> Result<()> {
    let mut xml = super::xml_reader(reader);
    let mut buf = Vec::new();

    // State tracking
    let mut current_file: Option<FileCoverage> = None;
    let mut in_method = false;
    let mut current_method_name: Option<String> = None;
    let mut method_hit: bool = false;
    let mut method_start_line: Option<u32> = None;
    let mut branch_indices: HashMap<u32, u32> = HashMap::new();
    let mut line_index_map: HashMap<u32, usize> = HashMap::new();

    // Source prefix from <source> elements. Text accumulates until the
    // closing tag because quick-xml 0.41 splits entity references out
    // of text: `<source>/srv/a&amp;b</source>` arrives as
    // Text("/srv/a") + GeneralRef("amp") + Text("b").
    let mut sources: Vec<String> = Vec::new();
    let mut in_source = false;
    let mut source_text = String::new();

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
                    b"source" => {
                        // Only set in_source for Start events; self-closing
                        // <source/> (Empty) has no text content and no
                        // corresponding End event, so setting the flag
                        // would cause the next unrelated Text event to be
                        // captured.
                        if is_start_event {
                            in_source = true;
                            source_text.clear();
                        }
                    }
                    b"class" => {
                        if let Some(filename) = get_attr(e, b"filename") {
                            let path = resolve_source_path(&filename, &sources);
                            current_file = Some(FileCoverage::new(path));
                            branch_indices.clear();
                            line_index_map.clear();
                        }
                    }
                    b"method" => {
                        in_method = true;
                        current_method_name = get_attr(e, b"name");
                        method_hit = false;
                        method_start_line = None;
                    }
                    b"line" => {
                        if let Some(file) = current_file.as_mut() {
                            // Extract all needed attributes in a single pass
                            let mut number: Option<u32> = None;
                            let mut hits: u64 = 0;
                            let mut is_branch = false;
                            let mut cond_cov: Option<String> = None;

                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"number" => {
                                        number =
                                            super::attr_str(&attr).and_then(|v| v.parse().ok());
                                    }
                                    b"hits" => {
                                        hits = super::attr_str(&attr)
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(0);
                                    }
                                    b"branch" => {
                                        is_branch =
                                            super::attr_str(&attr).is_some_and(|v| v == "true");
                                    }
                                    b"condition-coverage" => {
                                        cond_cov = super::attr_str(&attr);
                                    }
                                    _ => {}
                                }
                            }

                            if let Some(line_number) = number {
                                let hit_count = hits;

                                // Always collect line coverage. Lines may
                                // appear both under <method><lines> and
                                // <class><lines>, or only in one of them
                                // depending on the generator. Deduplicate
                                // by keeping the max hit_count per line.
                                if let Some(&idx) = line_index_map.get(&line_number) {
                                    if hit_count > file.lines[idx].hit_count {
                                        file.lines[idx].hit_count = hit_count;
                                    }
                                } else {
                                    line_index_map.insert(line_number, file.lines.len());
                                    file.lines.push(LineCoverage {
                                        line_number,
                                        hit_count,
                                    });
                                }

                                // Track method start line and hit status
                                if in_method {
                                    if method_start_line.is_none() {
                                        method_start_line = Some(line_number);
                                    }
                                    if hit_count > 0 {
                                        method_hit = true;
                                    }
                                }

                                // Branch coverage — only process on first
                                // encounter of this line to avoid double-
                                // counting when the same line appears in
                                // both <method> and <class> blocks.
                                if is_branch
                                    && !branch_indices.contains_key(&line_number)
                                    && let Some(cond) = cond_cov.as_deref()
                                    && let Some((covered, total)) = parse_condition_fraction(cond)
                                {
                                    let total = total.min(super::MAX_BRANCHES_PER_LINE);
                                    // Clamp to the arms actually emitted:
                                    // a malformed fraction like
                                    // "100% (4/2)" (or a capped total)
                                    // must not mark every arm covered.
                                    let covered = covered.min(total);
                                    for i in 0..total {
                                        // Cobertura's condition-coverage
                                        // only says how many branches were
                                        // taken, not per-branch execution
                                        // counts. Use 1 for covered arms
                                        // and 0 for uncovered.
                                        let branch_hit: u64 = u64::from(i < covered);
                                        let idx = branch_indices.entry(line_number).or_insert(0);
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
            Ok(Event::Text(ref e)) => {
                if in_source && let Ok(text) = e.decode() {
                    source_text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                // Entity/character references inside <source> content.
                // Only numeric char refs and the five predefined XML
                // entities resolve; custom entities are never expanded
                // (no DTD processing) — an unresolvable ref makes the
                // prefix unusable, so it is dropped with its record
                // left to suffix matching.
                if in_source {
                    if let Ok(Some(ch)) = e.resolve_char_ref() {
                        source_text.push(ch);
                    } else if let Some(entity) = e
                        .decode()
                        .ok()
                        .and_then(|name| quick_xml::escape::resolve_predefined_entity(&name))
                    {
                        source_text.push_str(entity);
                    }
                }
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"source" => {
                    if in_source && !source_text.trim().is_empty() {
                        sources.push(std::mem::take(&mut source_text));
                    }
                    in_source = false;
                }
                b"class" => {
                    if let Some(file) = current_file.take() {
                        emit_normalized(file)?;
                    }
                }
                b"method" if in_method => {
                    if let (Some(file), Some(name)) =
                        (current_file.as_mut(), current_method_name.take())
                    {
                        file.functions.push(FunctionCoverage {
                            name,
                            start_line: method_start_line,
                            end_line: None,
                            hit_count: u64::from(method_hit),
                        });
                    }
                    in_method = false;
                    method_start_line = None;
                }
                _ => {}
            },
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

/// Resolve a filename against the list of `<source>` prefixes.
///
/// - If the filename is already absolute — POSIX (`/…`), Windows
///   drive-qualified (`C:\…`, `C:/…`), or UNC (`\\server\…`) — return
///   it as-is; the path index normalizes separators later.
/// - Otherwise, prepend the first non-empty source prefix.
/// - If no non-empty sources exist, return the filename unchanged.
fn resolve_source_path(filename: &str, sources: &[String]) -> String {
    if is_absolute_filename(filename) {
        return filename.to_string();
    }
    for source in sources {
        let base = source.trim_end_matches('/');
        if !base.is_empty() {
            return format!("{base}/{filename}");
        }
    }
    filename.to_string()
}

/// POSIX-absolute, Windows drive-qualified, or UNC spelling.
fn is_absolute_filename(filename: &str) -> bool {
    if filename.starts_with('/') || filename.starts_with("\\\\") {
        return true;
    }
    let bytes = filename.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cobertura() {
        let input = include_bytes!("../../tests/fixtures/sample_cobertura.xml");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 2);

        let main = &data.files[0];
        assert_eq!(main.path, "/home/user/project/src/main.py");
        assert_eq!(main.lines.len(), 8);
        assert_eq!(main.lines[0].line_number, 1);
        assert_eq!(main.lines[0].hit_count, 1);
        assert_eq!(main.lines[2].line_number, 3);
        assert_eq!(main.lines[2].hit_count, 0);

        // Branch on line 8: 50% (1/2) → 2 branch arms, one hit one miss
        assert_eq!(main.branches.len(), 2);
        assert_eq!(main.branches[0].line_number, 8);
        assert_eq!(main.branches[0].hit_count, 1); // covered arm
        assert_eq!(main.branches[1].hit_count, 0); // uncovered arm

        // One method extracted
        assert_eq!(main.functions.len(), 1);
        assert_eq!(main.functions[0].name, "do_stuff");
        assert_eq!(main.functions[0].start_line, Some(5));
        assert_eq!(main.functions[0].hit_count, 1);

        let util = &data.files[1];
        assert_eq!(util.path, "/home/user/project/src/util.py");
        assert_eq!(util.lines.len(), 2);
        assert_eq!(util.branches.len(), 0);
    }

    #[test]
    fn test_parse_cobertura_branch_dedup() {
        // Branch line appears in both <method><lines> and <class><lines>.
        // We must not double-count the branch arms.
        let input = include_bytes!("../../tests/fixtures/cobertura_branch_in_method_and_class.xml");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 1);
        let file = &data.files[0];

        // Lines should be deduplicated: 4 unique lines, not 7
        assert_eq!(file.lines.len(), 4);

        // Branch on line 3: 50% (1/2) → exactly 2 arms, not 4
        assert_eq!(file.branches.len(), 2);
        assert_eq!(file.branches[0].line_number, 3);
        assert_eq!(file.branches[0].branch_index, 0);
        assert_eq!(file.branches[0].hit_count, 1); // covered arm
        assert_eq!(file.branches[1].line_number, 3);
        assert_eq!(file.branches[1].branch_index, 1);
        assert_eq!(file.branches[1].hit_count, 0); // uncovered arm
    }

    #[test]
    fn test_parse_cobertura_multiple_sources() {
        // First <source> is empty, second is the real prefix.
        let input = include_bytes!("../../tests/fixtures/cobertura_multiple_sources.xml");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 1);
        // Should use the first non-empty source as prefix, not the empty
        // one.
        assert_eq!(data.files[0].path, "/home/user/project/src/app.py");
    }

    #[test]
    fn test_parse_cobertura_no_sources() {
        let input = include_bytes!("../../tests/fixtures/cobertura_no_sources.xml");
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].path, "src/f.rs");
    }

    #[test]
    fn test_parse_cobertura_empty() {
        // A valid Cobertura file with no classes should produce empty
        // CoverageData.
        let input = include_bytes!("../../tests/fixtures/empty_cobertura.xml");
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 0);
    }

    #[test]
    fn test_parse_cobertura_malformed() {
        // Malformed XML should produce a meaningful error with position
        // info.
        let input = include_bytes!("../../tests/fixtures/malformed_cobertura.xml");
        let result = parse(input);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("position"),
            "Error should contain position info: {err_msg}",
        );
    }

    #[test]
    fn test_parse_condition_fraction() {
        assert_eq!(parse_condition_fraction("75% (3/4)"), Some((3, 4)));
        assert_eq!(parse_condition_fraction("50% (1/2)"), Some((1, 2)));
        assert_eq!(parse_condition_fraction("100%"), None);
        assert_eq!(parse_condition_fraction("(x/y)"), None);
        assert_eq!(parse_condition_fraction(""), None);
    }

    #[test]
    fn test_malformed_condition_fraction_cannot_exceed_arm_count() {
        // A generator writing covered > total ("100% (4/2)") must not
        // report more covered arms than emitted arms — coverage.branch
        // backs CI gates, so an uncapped count would mask a failure.
        let input = br#"<?xml version="1.0"?>
<coverage version="1.0">
  <packages><package name="p"><classes>
    <class name="C" filename="src/f.rs">
      <lines><line number="3" hits="1" branch="true" condition-coverage="100% (4/2)"/></lines>
    </class>
  </classes></package></packages>
</coverage>"#;
        let data = parse(input).unwrap();
        let file = &data.files[0];
        assert_eq!(file.branches.len(), 2);
        assert!(file.branches.iter().all(|b| b.hit_count == 1));
    }

    #[test]
    fn test_source_with_entity_reference_is_complete() {
        // quick-xml splits `&amp;` out of text content; the <source>
        // accumulator must reassemble the full prefix.
        let input = br#"<?xml version="1.0"?>
<coverage version="1.0">
  <sources><source>/srv/a&amp;b</source></sources>
  <packages><package name="p"><classes>
    <class name="C" filename="src/f.py">
      <lines><line number="1" hits="1"/></lines>
    </class>
  </classes></package></packages>
</coverage>"#;
        let data = parse(input).unwrap();
        assert_eq!(data.files[0].path, "/srv/a&b/src/f.py");
    }

    #[test]
    fn test_windows_absolute_filenames_are_preserved() {
        // Drive-qualified and UNC filenames must not receive a <source>
        // prefix; the path index normalizes separators later.
        for absolute in [
            r"C:\proj\src\a.cs",
            "C:/proj/src/a.cs",
            r"\\server\share\a.cs",
        ] {
            assert_eq!(
                resolve_source_path(absolute, &["/ignored".to_string()]),
                absolute,
                "{absolute} must stay as spelled"
            );
        }
        // POSIX-relative still receives the prefix.
        assert_eq!(
            resolve_source_path("src/a.cs", &["/root".to_string()]),
            "/root/src/a.cs"
        );
    }

    #[test]
    fn dtd_is_inert_by_construction() {
        // quick-xml never resolves DTDs or expands custom entities: a
        // billion-laughs preamble parses as an inert DocType event and
        // the entity reference stays unexpanded (attribute unescape
        // fails → attribute skipped) instead of exploding memory. This
        // pins the security assumption the discovery pipeline relies on
        // when sniffing untrusted artifacts.
        let input = br#"<?xml version="1.0"?>
<!DOCTYPE lolz [
  <!ENTITY lol "lol">
  <!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
  <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
]>
<coverage version="1.0">
  <sources><source>/src</source></sources>
  <packages><package name="p"><classes>
    <class name="C" filename="f.rs">
      <lines><line number="1" hits="1"/></lines>
    </class>
  </classes></package></packages>
</coverage>"#;
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].lines.len(), 1);
    }
}
