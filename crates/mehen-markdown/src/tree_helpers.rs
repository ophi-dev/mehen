// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Markdown tree walking helpers shared across the per-metric analyzers.
//!
//! The Markdown crate has many sibling analyzers (`mcc`, `mrpc`,
//! `halstead`, `embedded_code`, …) that all walk the same syntax tree
//! and need the same low-level extraction primitives: line spans,
//! table-cell counts, prose-container checks, and small node slices.
//! Without a shared module each analyzer was carrying byte-identical
//! helper copies; CPD flagged ~155 lines of duplication between `mcc.rs`
//! and `mrpc.rs` alone.
//!
//! This module is the consolidation point for structural tree mechanics.
//! Markdown semantic facts such as resolved reference links live in
//! `MarkdownDocument`, where pulldown-cmark already exposes them.

use crate::kind::NodeKind;
use crate::syntax_tree::Node;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProseContext {
    include_heading_content: bool,
    include_heading_blocks: bool,
    include_link_labels: bool,
    include_tables: bool,
}

impl ProseContext {
    pub(crate) const BODY: Self = Self {
        include_heading_content: false,
        include_heading_blocks: false,
        include_link_labels: true,
        include_tables: true,
    };

    pub(crate) const BODY_AND_HEADING_TEXT: Self = Self {
        include_heading_content: true,
        include_heading_blocks: false,
        include_link_labels: true,
        include_tables: true,
    };

    pub(crate) const PLACEHOLDER_TEXT: Self = Self {
        include_heading_content: true,
        include_heading_blocks: false,
        include_link_labels: false,
        include_tables: false,
    };

    pub(crate) const SECTION_TEXT: Self = Self {
        include_heading_content: true,
        include_heading_blocks: true,
        include_link_labels: true,
        include_tables: true,
    };
}

/// Containers whose descendants are structural or machine-readable rather than
/// narrative prose for Markdown metric walks.
pub(crate) fn is_non_prose_container(kind: NodeKind) -> bool {
    use NodeKind::*;
    matches!(
        kind,
        FencedCodeBlock
            | IndentedCodeBlock
            | InlineCode
            | CodeFenceContent
            | InlineCodeContent
            | InfoString
            | Language
            | MathBlock
            | MathInline
            | MathBlockContent
            | MathInlineContent
            | HtmlBlock
            | HtmlInline
            | HtmlComment
            | HtmlCdata
            | HtmlDeclaration
            | HtmlProcessingInstruction
            | HtmlOpenTag
            | HtmlCloseTag
            | Autolink
            | Uri
            | Email
            | LinkDestination
            | LinkTitle
            | MinusMetadata
            | PlusMetadata
            | PipeTableDelimiterRow
            | PipeTableDelimiterCell
            | PipeTableAlignLeft
            | PipeTableAlignRight
    )
}

pub(crate) fn opens_prose_context(kind: NodeKind, context: ProseContext) -> bool {
    use NodeKind::*;
    matches!(
        kind,
        Paragraph | BlockQuote | Callout | ListItemContent { .. }
    ) || (context.include_heading_content && matches!(kind, HeadingContent))
        || (context.include_heading_blocks && kind.is_heading())
        || (context.include_link_labels && matches!(kind, LinkLabel | FootnoteLabel))
        || (context.include_tables
            && matches!(kind, PipeTableCell | PipeTableHeader | PipeTableRow))
}

/// Number of source lines a node covers, with the same end-of-line
/// convention every Markdown analyzer wants:
///
/// - When the node ends at column 0 of a row past its start, the row
///   it ends *on* belongs to the next block — strip it.
/// - Otherwise the end row is the last row containing node content.
///
/// Returns at least `1` for any non-empty node.
pub(crate) fn node_line_span(node: &Node<'_>) -> usize {
    let start = node.start_row();
    let (end_row, end_col) = node.end_position();
    let mut end = end_row;
    if end > start && end_col == 0 {
        end -= 1;
    }
    end.saturating_sub(start) + 1
}

/// Total number of table cells (header + body rows) below `node`.
/// Walks the immediate children for `pipe_table_header` /
/// `pipe_table_row` and counts each `pipe_table_cell` inside.
pub(crate) fn count_table_cells(node: &Node<'_>) -> usize {
    let mut total = 0usize;
    for child in node.children() {
        if matches!(
            child.kind(),
            NodeKind::PipeTableHeader | NodeKind::PipeTableRow
        ) {
            for cell in child.children() {
                if matches!(cell.kind(), NodeKind::PipeTableCell) {
                    total += 1;
                }
            }
        }
    }
    total
}

/// Find a `link_label` inside `node` and return its raw text.
pub(crate) fn find_link_label(node: &Node<'_>, source: &str) -> Option<String> {
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), NodeKind::LinkLabel) {
            let bytes = source.as_bytes();
            let start = n.start_byte();
            let end = n.end_byte();
            if end <= bytes.len() && start < end {
                return std::str::from_utf8(&bytes[start..end])
                    .ok()
                    .map(|s| s.to_string());
            }
        }
        stack.extend(n.children());
    }
    None
}

/// Find the first direct child whose kind matches `target`. Returns
/// `None` when the node has no children or no matching child. Common
/// shape across `code_burden`, `visuals`, etc. for plucking a single
/// known child (e.g. the `link_destination` inside an `inline_link`).
pub(crate) fn find_first<'a>(node: &Node<'a>, target: NodeKind) -> Option<Node<'a>> {
    node.children().find(|child| child.kind() == target)
}

/// Slice the source bytes covered by `node` as a UTF-8 lossy `String`.
/// Lossy conversion (rather than `from_utf8`) keeps every analyzer
/// resilient to a syntax node stepping into an invalid byte
/// run inside an HTML-block / MDX raw segment.
pub(crate) fn node_text(node: &Node<'_>, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    String::from_utf8_lossy(&source.as_bytes()[start..end]).into_owned()
}

/// Whether a link destination has an explicit URL scheme.
///
/// Matches RFC 3986 scheme grammar: an ALPHA followed by zero or more
/// of ALPHA / DIGIT / `+` / `-` / `.`, terminated by `:`. Returns
/// `false` for relative paths, anchors, fragments, and bare paths.
pub(crate) fn has_scheme(dest: &str) -> bool {
    if let Some(colon) = dest.find(':') {
        let scheme = &dest[..colon];
        let chars: Vec<char> = scheme.chars().collect();
        if chars.is_empty() {
            return false;
        }
        if !chars[0].is_ascii_alphabetic() {
            return false;
        }
        for c in &chars[1..] {
            if !(c.is_ascii_alphanumeric() || *c == '+' || *c == '-' || *c == '.') {
                return false;
            }
        }
        return true;
    }
    false
}
