// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Block-level neighborhood lookups shared by tables, visuals, and code.
//!
//! Several §§11–14 formulas ask whether a prose block explains or
//! introduces a nearby artifact. "Nearby" here is the §19 convention:
//! within ±2 top-level blocks (i.e. two blocks before or after the artifact
//! in the document order). This module walks the AST once, collects a
//! flat list of block rows, and exposes a helper that tells you whether a
//! given artifact line range has a prose block ±2 positions away.

use crate::kind::NodeKind;
use crate::syntax_tree::Node;

/// A flattened block descriptor. One entry per top-level block in the
/// document. Lines are one-based and inclusive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockSpan {
    pub(crate) start_line: u64,
    pub(crate) end_line: u64,
    pub(crate) is_prose: bool,
}

/// Collects every top-level block in the document, preserving order. A
/// "top-level block" is a direct child of a `section`, `document`, or
/// generic `block` container. We deliberately do not descend into
/// artifacts — they cover exactly the same line range and we need the
/// prose blocks around them, not inside them.
pub(crate) fn collect_blocks(root: &Node<'_>) -> Vec<BlockSpan> {
    let mut out: Vec<BlockSpan> = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(node: &Node<'_>, out: &mut Vec<BlockSpan>) {
    let kind = node.kind();

    if is_block_like(&kind) {
        let start = (node.start_row() as u64) + 1;
        let (end_row, end_col) = node.end_position();
        let mut end = end_row;
        if end > node.start_row() && end_col == 0 {
            end -= 1;
        }
        let end_line = (end as u64) + 1;
        out.push(BlockSpan {
            start_line: start,
            end_line,
            is_prose: is_prose_block(&kind),
        });
        return;
    }

    for child in node.children() {
        walk(&child, out);
    }
}

fn is_block_like(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Paragraph
            | NodeKind::FencedCodeBlock
            | NodeKind::IndentedCodeBlock
            | NodeKind::HtmlBlock
            | NodeKind::MathBlock
            | NodeKind::PipeTable
            | NodeKind::BlockQuote
            | NodeKind::Callout
            | NodeKind::List
            | NodeKind::ThematicBreak
            | NodeKind::FootnoteDefinition
            | NodeKind::LinkReferenceDefinition
            | NodeKind::Heading { .. }
    )
}

fn is_prose_block(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Paragraph
            | NodeKind::BlockQuote
            | NodeKind::Callout
            | NodeKind::List
            | NodeKind::Heading { .. }
    )
}

/// True when the artifact spanning `[start_line, end_line]` has a prose
/// block within ±`radius` positions in the block-order index.
pub(crate) fn has_prose_within(
    blocks: &[BlockSpan],
    start_line: u64,
    end_line: u64,
    radius: usize,
) -> bool {
    let Some(idx) = blocks
        .iter()
        .position(|b| b.start_line <= start_line && b.end_line >= end_line)
    else {
        return false;
    };
    let lo = idx.saturating_sub(radius);
    let hi = (idx + radius).min(blocks.len().saturating_sub(1));
    for (i, block) in blocks.iter().enumerate().take(hi + 1).skip(lo) {
        if i == idx {
            continue;
        }
        if block.is_prose {
            return true;
        }
    }
    false
}
