// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Line classification for the Markdown LOC family (§5).
//!
//! Each physical line of the source is assigned to exactly one class:
//! prose, code, table, math, blank, or "other-artifact" (raw HTML / MDX /
//! directive / front-matter / image-block / footnote / reference definition
//! / thematic break / heading marker outside headings). `ALOC` (§4) is
//! `CLOC + TLOC + MLOC + other_artifact`. Callers read the final counts via
//! [`LineClasses::loc_family`].

use crate::kind::NodeKind;
use crate::syntax_tree::Node;
use crate::types::{LocFamily, LocRatios};

/// One-of line categories, in precedence order when multiple nodes claim the
/// same physical line. Lower-index variants win ties, because code / math /
/// tables own their lines even when an inline prose span touches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineClass {
    /// Covered by a fenced or indented code block (including fence markers).
    Code,
    /// Covered by a pipe table (header, delimiter, or row).
    Table,
    /// Covered by a math block (including `$$` delimiters).
    Math,
    /// Covered by raw HTML / MDX / directive / front-matter / image-block /
    /// footnote-definition / link-reference-definition / thematic-break.
    OtherArtifact,
    /// Covered by paragraph / heading / blockquote / callout / list-item
    /// prose.
    Prose,
    /// Not touched by any classified node: a blank line.
    Blank,
}

/// Per-line classification map. Indexed by zero-based line number.
pub(crate) struct LineClasses {
    classes: Vec<LineClass>,
}

impl LineClasses {
    /// Builds the map by walking the AST. `total_lines` must equal the number
    /// of physical lines in the source (see [`physical_line_count`]).
    pub(crate) fn build(root: &Node<'_>, total_lines: usize) -> Self {
        let mut classes = vec![LineClass::Blank; total_lines];

        // The walk assigns each block-level node's covered line range to the
        // tightest category it represents. Later nodes (children inside
        // parents) can tighten a parent's "prose" default by overwriting
        // with a higher-precedence class. Precedence is enforced via
        // [`LineClass::replace_if_stronger`].
        let mut stack = vec![*root];
        while let Some(node) = stack.pop() {
            classify_node(&node, &mut classes);
            stack.extend(node.children());
        }

        Self { classes }
    }

    /// Aggregates the classification map into the §5 LOC family.
    pub(crate) fn loc_family(&self) -> LocFamily {
        let mut loc = LocFamily {
            dloc: self.classes.len() as u64,
            ..LocFamily::default()
        };
        for class in &self.classes {
            match class {
                LineClass::Code => loc.cloc += 1,
                LineClass::Table => loc.tloc += 1,
                LineClass::Math => loc.mloc += 1,
                LineClass::OtherArtifact => {}
                LineClass::Prose => loc.ploc += 1,
                LineClass::Blank => loc.bloc += 1,
            }
        }
        // ALOC is the sum of every artifact bucket (§4 / §5).
        let other_artifact = loc
            .dloc
            .saturating_sub(loc.ploc + loc.cloc + loc.tloc + loc.mloc + loc.bloc);
        loc.aloc = loc.cloc + loc.tloc + loc.mloc + other_artifact;
        loc
    }

    /// Returns the line class for a zero-based line number, if in range.
    pub(crate) fn class_at(&self, line: usize) -> Option<LineClass> {
        self.classes.get(line).copied()
    }
}

impl LineClass {
    /// Returns the stronger of two classes for precedence in line assignment.
    /// `Code` > `Table` > `Math` > `OtherArtifact` > `Prose` > `Blank`.
    fn rank(self) -> u8 {
        match self {
            LineClass::Code => 5,
            LineClass::Table => 4,
            LineClass::Math => 3,
            LineClass::OtherArtifact => 2,
            LineClass::Prose => 1,
            LineClass::Blank => 0,
        }
    }

    fn replace_if_stronger(current: &mut LineClass, candidate: LineClass) {
        if candidate.rank() > current.rank() {
            *current = candidate;
        }
    }
}

fn classify_node(node: &Node<'_>, classes: &mut [LineClass]) {
    let class = match node.kind() {
        // Code — both fenced and indented blocks. Fence markers are covered
        // because the `fenced_code_block` span includes them.
        NodeKind::FencedCodeBlock | NodeKind::IndentedCodeBlock => LineClass::Code,

        // Tables.
        NodeKind::PipeTable => LineClass::Table,

        // Math blocks (`$$…$$`).
        NodeKind::MathBlock => LineClass::Math,

        // Raw HTML / footnote / link reference / thematic break / front-matter.
        NodeKind::HtmlBlock
        | NodeKind::FootnoteDefinition
        | NodeKind::LinkReferenceDefinition
        | NodeKind::ThematicBreak
        | NodeKind::MinusMetadata
        | NodeKind::PlusMetadata => LineClass::OtherArtifact,

        // Prose-shaped blocks. Children like inline code / math inline do
        // not relabel their line — they appear inside a paragraph whose
        // line bucket is prose, consistent with §5. List items are prose
        // too: tight lists omit paragraph wrappers, so without this the
        // list lines would fall through to Blank and inflate BLOC.
        NodeKind::Paragraph
        | NodeKind::Heading { .. }
        | NodeKind::BlockQuote
        | NodeKind::Callout
        | NodeKind::ListItem { .. } => LineClass::Prose,

        _ => return,
    };

    let start = node.start_row();
    let (end_row, end_col) = node.end_position();
    let mut end = end_row;
    // Tree-sitter reports `end_row` as the row of the byte *after* the last
    // child. A block-level node that ends exactly at a line break leaves
    // that trailing row dangling; skip it so blank lines downstream of the
    // block are not miscounted.
    if end > start && end_col == 0 {
        end -= 1;
    }
    if classes.is_empty() {
        return;
    }
    for row in start..=end.min(classes.len() - 1) {
        LineClass::replace_if_stronger(&mut classes[row], class);
    }
}

/// Returns the number of physical lines in `source`, counting a trailing
/// newline-less line and handling CRLF consistently with parser byte spans.
pub(crate) fn physical_line_count(source: &str) -> usize {
    if source.is_empty() {
        return 0;
    }
    let mut count = 1usize;
    for byte in source.bytes() {
        if byte == b'\n' {
            count += 1;
        }
    }
    // A file that ends with `\n` has a trailing empty line we shouldn't
    // double-count.
    if source.ends_with('\n') {
        count -= 1;
    }
    count
}

/// Computes §5.1 ratios from a [`LocFamily`].
pub(crate) fn derive_ratios(loc: &LocFamily) -> LocRatios {
    let dloc = loc.dloc.max(1) as f64;
    LocRatios {
        artifact_line_ratio: loc.aloc as f64 / dloc,
        code_line_ratio: loc.cloc as f64 / dloc,
        table_line_ratio: loc.tloc as f64 / dloc,
        math_line_ratio: loc.mloc as f64 / dloc,
        blank_line_ratio: loc.bloc as f64 / dloc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_line_count_handles_trailing_newlines() {
        assert_eq!(physical_line_count(""), 0);
        assert_eq!(physical_line_count("a"), 1);
        assert_eq!(physical_line_count("a\n"), 1);
        assert_eq!(physical_line_count("a\nb"), 2);
        assert_eq!(physical_line_count("a\nb\n"), 2);
        assert_eq!(physical_line_count("\n"), 1);
    }
}
