// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>
//
// Adapted from covrs (https://github.com/scttnlsn/covrs)
// `src/parsers/gocover.rs`, MIT-licensed by Scott Nelson. Local changes:
// house error type, camino paths, normalization before emit.
// See LICENSE-THIRD-PARTY.

//! Parser for Go's `-coverprofile` format.
//!
//! Reference: <https://go.dev/blog/cover>
//!
//! ```text
//! mode: set|count|atomic
//! <file>:<startLine>.<startCol>,<endLine>.<endCol> <numStatements> <count>
//! ```
//!
//! Each line describes a basic block (a range of source lines) with the
//! number of statements in the block and how many times it was executed.
//! Since mehen tracks per-line coverage, each block expands into
//! individual line entries, assigning the block's hit count to every line
//! in the range. Note the `<file>` component is a *module import path*
//! (`github.com/org/repo/pkg/file.go`), not a filesystem path — the
//! suffix matcher in [`crate::CoverageIndex`] absorbs the module prefix.

use std::collections::HashMap;
use std::io::BufRead;

use camino::Utf8Path;

use super::{CoverageFormat, CoverageParser};
use crate::Result;
use crate::model::{FileCoverage, LineCoverage};

/// Go coverage profile parser.
pub(crate) struct GocoverParser;

impl CoverageParser for GocoverParser {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::Gocover
    }

    fn can_parse(&self, path: &Utf8Path, content: &[u8]) -> bool {
        // Extension-based: .coverprofile or .gocov
        if let Some(ext) = path.extension() {
            let ext = ext.to_lowercase();
            if ext == "coverprofile" || ext == "gocov" {
                return true;
            }
        }

        // Content-based: first line starts with "mode: ", or any line
        // matches the block pattern (file.go:N.N,N.N N N). The fallback
        // catches profiles without a mode header (rare, but possible from
        // merging tools).
        let head = super::sniff_head(content);
        if let Some(first) = head.lines().next()
            && first.starts_with("mode: ")
        {
            return true;
        }

        head.lines().any(looks_like_go_block)
    }

    fn parse_streaming(
        &self,
        reader: &mut dyn BufRead,
        emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
    ) -> Result<()> {
        parse_streaming_reader(reader, emit)
    }
}

/// Parse a Go coverage profile from raw bytes.
#[cfg(test)]
pub(crate) fn parse(input: &[u8]) -> Result<crate::CoverageData> {
    let mut data = crate::CoverageData::new();
    parse_streaming_reader(&mut &*input, &mut |file| {
        data.files.push(file);
        Ok(())
    })?;
    Ok(data)
}

/// A parsed block from a single line of the coverage profile.
struct Block {
    start_line: u32,
    end_line: u32,
    /// Column of the block's exclusive end position. When the end
    /// position sits at column 1, the block ends *before* `end_line`'s
    /// first character and that line is not part of the block.
    end_col: u32,
    count: u64,
}

/// Quick heuristic: does this line look like a Go coverage block?
/// e.g. "github.com/user/repo/file.go:10.1,20.5 3 1"
fn looks_like_go_block(line: &str) -> bool {
    let Some(colon_pos) = line.rfind(".go:") else {
        return false;
    };
    let after = &line[colon_pos + 4..];
    after.contains(',') && after.split_whitespace().count() >= 2
}

/// Maximum number of lines a single block can span. Anything larger is
/// almost certainly malformed input and expanding it would consume
/// excessive memory.
const MAX_BLOCK_LINE_SPAN: u32 = 100_000;

/// Parse a single block line, returning (file_path, Block).
///
/// Format: `<file>:<startLine>.<startCol>,<endLine>.<endCol> <numStmt> <count>`
fn parse_block_line(line: &str) -> Option<(&str, Block)> {
    // Anchor on the last ".go:" to split the file path from the block
    // range. This naturally handles paths containing colons.
    let colon_pos = line.rfind(".go:")? + 3; // position of ':'

    let file = &line[..colon_pos];
    let rest = &line[colon_pos + 1..];

    // rest = "startLine.startCol,endLine.endCol numStmt count"
    let (range, tail) = rest.split_once(' ')?;
    let (start, end) = range.split_once(',')?;

    let start_line: u32 = start.split_once('.')?.0.parse().ok()?;
    let (end_line_str, end_col_str) = end.split_once('.')?;
    let end_line: u32 = end_line_str.parse().ok()?;
    let end_col: u32 = end_col_str.parse().ok()?;

    // Reject blocks with an invalid range or an absurdly large span that
    // would cause excessive memory allocation when expanded per-line.
    if end_line < start_line || (end_line - start_line) > MAX_BLOCK_LINE_SPAN {
        return None;
    }

    let mut parts = tail.split_whitespace();
    let _num_stmt = parts.next()?;
    let count: u64 = parts.next()?.parse().ok()?;

    Some((
        file,
        Block {
            start_line,
            end_line,
            end_col,
            count,
        },
    ))
}

/// Streaming Go coverage parser. Collects all blocks per file, then
/// expands them into per-line coverage and emits once per source file.
fn parse_streaming_reader(
    reader: &mut dyn BufRead,
    emit: &mut dyn FnMut(FileCoverage) -> Result<()>,
) -> Result<()> {
    // Collect blocks grouped by file path, preserving insertion order.
    let mut file_order: Vec<String> = Vec::new();
    let mut file_blocks: HashMap<String, Vec<Block>> = HashMap::new();

    let mut raw_line = String::new();
    let mut line_number: u64 = 0;
    loop {
        raw_line.clear();
        let n = reader.read_line(&mut raw_line)?;
        if n == 0 {
            break;
        }
        line_number += 1;

        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("mode:") {
            continue;
        }

        // A non-header line that is not a valid block record means the
        // input is not (or is no longer) a Go coverage profile —
        // silently dropping it would report misleadingly complete
        // coverage from a corrupt report.
        let Some((file, block)) = parse_block_line(line) else {
            return Err(crate::CoverageError::Malformed(format!(
                "malformed Go coverage block at line {line_number}: {line}"
            )));
        };
        let file_str = file.to_string();
        if !file_blocks.contains_key(&file_str) {
            file_order.push(file_str.clone());
        }
        file_blocks.entry(file_str).or_default().push(block);
    }

    // Emit one FileCoverage per source file.
    for file_path in file_order {
        if let Some(blocks) = file_blocks.remove(&file_path) {
            let file_cov = blocks_to_file_coverage(file_path, &blocks);
            emit(file_cov)?;
        }
    }

    Ok(())
}

/// Convert a list of blocks for one file into a `FileCoverage`.
///
/// Go coverage blocks describe ranges of lines. Multiple blocks may
/// overlap or cover the same line. We take the maximum hit count for
/// each line across all blocks that touch it.
fn blocks_to_file_coverage(path: String, blocks: &[Block]) -> FileCoverage {
    let mut line_hits: HashMap<u32, u64> = HashMap::new();

    for block in blocks {
        // Block positions are start-inclusive with an *exclusive* end
        // offset. An end column of 1 means the block ends before the
        // end line's first character — that line carries none of the
        // block's statements and must not inherit its count.
        let last_line = if block.end_col == 1 && block.end_line > block.start_line {
            block.end_line - 1
        } else {
            block.end_line
        };
        for line_num in block.start_line..=last_line {
            let entry = line_hits.entry(line_num).or_insert(0);
            if block.count > *entry {
                *entry = block.count;
            }
        }
    }

    let mut file = FileCoverage::new(path);
    file.lines = line_hits
        .into_iter()
        .map(|(line_number, hit_count)| LineCoverage {
            line_number,
            hit_count,
        })
        .collect();
    file.normalize();
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gocover() {
        let input = include_bytes!("../../tests/fixtures/sample.gocov");
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 2);

        let main = &data.files[0];
        assert_eq!(main.path, "github.com/user/project/main.go");
        // Lines 10-12 (count 5) + lines 14-16 (count 0) = 6 lines
        assert_eq!(main.lines.len(), 6);
        assert_eq!(main.lines[0].line_number, 10);
        assert_eq!(main.lines[0].hit_count, 5);
        assert_eq!(main.lines[2].line_number, 12);
        assert_eq!(main.lines[2].hit_count, 5);
        // Line 14 has count 0
        assert_eq!(main.lines[3].line_number, 14);
        assert_eq!(main.lines[3].hit_count, 0);

        let util = &data.files[1];
        assert_eq!(util.path, "github.com/user/project/util.go");
        assert_eq!(util.lines.len(), 3);
        assert_eq!(util.lines[0].hit_count, 3);
    }

    #[test]
    fn test_parse_gocover_overlapping_blocks() {
        // When two blocks overlap on the same line, we take the max hit count.
        let input = b"mode: count\n\
            example.com/pkg/f.go:5.1,10.10 3 2\n\
            example.com/pkg/f.go:8.1,12.10 2 7\n";
        let data = parse(input).unwrap();

        assert_eq!(data.files.len(), 1);
        let file = &data.files[0];

        // Lines 5-7 from first block only: count 2
        // Lines 8-10 overlap: max(2, 7) = 7
        // Lines 11-12 from second block only: count 7
        assert_eq!(file.lines.len(), 8); // lines 5..=12
        let line5 = file.lines.iter().find(|l| l.line_number == 5).unwrap();
        assert_eq!(line5.hit_count, 2);
        let line8 = file.lines.iter().find(|l| l.line_number == 8).unwrap();
        assert_eq!(line8.hit_count, 7);
        let line10 = file.lines.iter().find(|l| l.line_number == 10).unwrap();
        assert_eq!(line10.hit_count, 7);
        let line12 = file.lines.iter().find(|l| l.line_number == 12).unwrap();
        assert_eq!(line12.hit_count, 7);
    }

    #[test]
    fn test_parse_gocover_empty() {
        let input = include_bytes!("../../tests/fixtures/empty.gocov");
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 0);
    }

    #[test]
    fn test_parse_gocover_no_mode_header() {
        // Some merge tools produce profiles without a mode line.
        let input = b"example.com/pkg/f.go:1.1,5.10 2 3\n";
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].lines.len(), 5);
        assert_eq!(data.files[0].lines[0].hit_count, 3);
    }

    #[test]
    fn test_end_column_one_excludes_the_end_line() {
        // Block positions carry an exclusive end offset: `1.1,3.1` ends
        // *before* line 3's first character, so only lines 1 and 2
        // belong to the block.
        let input = b"mode: set\nexample.com/pkg/f.go:1.1,3.1 1 1\n";
        let data = parse(input).unwrap();
        let lines: Vec<u32> = data.files[0].lines.iter().map(|l| l.line_number).collect();
        assert_eq!(lines, vec![1, 2]);

        // A degenerate single-line block at column 1 still keeps its
        // one line rather than vanishing.
        let input = b"mode: set\nexample.com/pkg/f.go:7.1,7.1 1 1\n";
        let data = parse(input).unwrap();
        let lines: Vec<u32> = data.files[0].lines.iter().map(|l| l.line_number).collect();
        assert_eq!(lines, vec![7]);
    }

    #[test]
    fn test_malformed_block_line_is_an_error() {
        // A non-header line that is not a block record must fail the
        // parse — silently dropping it would report misleadingly
        // complete coverage from a corrupt report.
        let input = b"mode: count\nnot a profile\n";
        let err = parse(input).unwrap_err().to_string();
        assert!(err.contains("line 2"), "unexpected error: {err}");

        // A record with a corrupted range is equally fatal.
        let input = b"mode: count\nexample.com/pkg/f.go:9.1,3.1 1 1\n";
        assert!(parse(input).is_err());
    }

    #[test]
    fn test_parse_gocover_set_mode() {
        // In "set" mode, count is 0 or 1.
        let input = b"mode: set\n\
            example.com/pkg/f.go:1.1,3.10 2 1\n\
            example.com/pkg/f.go:5.1,6.10 1 0\n";
        let data = parse(input).unwrap();
        assert_eq!(data.files.len(), 1);
        let file = &data.files[0];
        assert_eq!(file.lines.len(), 5);
        assert_eq!(file.lines[0].hit_count, 1); // line 1
        assert_eq!(file.lines[3].hit_count, 0); // line 5
    }

    #[test]
    fn test_looks_like_go_block() {
        assert!(looks_like_go_block(
            "github.com/user/repo/file.go:10.1,20.5 3 1"
        ));
        assert!(!looks_like_go_block("mode: count"));
        assert!(!looks_like_go_block("SF:/src/lib.rs"));
        assert!(!looks_like_go_block(""));
    }

    #[test]
    fn test_parse_block_line() {
        let (file, block) = parse_block_line("github.com/user/repo/file.go:10.1,20.5 3 1").unwrap();
        assert_eq!(file, "github.com/user/repo/file.go");
        assert_eq!(block.start_line, 10);
        assert_eq!(block.end_line, 20);
        assert_eq!(block.count, 1);
    }

    #[test]
    fn test_can_parse_by_extension() {
        let parser = GocoverParser;
        assert!(parser.can_parse(Utf8Path::new("coverage.coverprofile"), b""));
        assert!(parser.can_parse(Utf8Path::new("coverage.gocov"), b""));
        assert!(!parser.can_parse(Utf8Path::new("coverage.txt"), b""));
    }

    #[test]
    fn test_can_parse_by_content() {
        let parser = GocoverParser;
        assert!(parser.can_parse(Utf8Path::new("coverage.out"), b"mode: count\n"));
        assert!(parser.can_parse(Utf8Path::new("coverage.out"), b"mode: set\n"));
        assert!(parser.can_parse(Utf8Path::new("coverage.out"), b"mode: atomic\n"));
        assert!(!parser.can_parse(Utf8Path::new("coverage.out"), b"random data\n"));
    }
}
