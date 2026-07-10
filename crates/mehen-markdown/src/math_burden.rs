// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Per-block math burden per §14.3.
//!
//! ```text
//! MathBurden(m) =
//!     1.0
//!   + 0.10 * math_tokens
//!   + 0.25 * distinct_math_commands
//!   + 1.00 * no_nearby_explanation
//! ```
//!
//! `distinct_math_commands` is the count of distinct `\command` tokens
//! inside the math body — a crude proxy for LaTeX vocabulary but enough
//! for §14.3. `no_nearby_explanation = 1 - has_nearby_prose`.

use std::collections::BTreeSet;

use crate::kind::NodeKind;
use crate::nearby::{BlockSpan, has_prose_within};
use crate::syntax_tree::Node;

#[derive(Debug, Clone)]
pub(crate) struct MathBlock {
    pub(crate) start_line: u64,
    pub(crate) end_line: u64,
    pub(crate) tokens: u64,
    /// Distinct `\command` tokens. Not serialized directly; read by Phase D's
    /// grounding / filler metrics that want math vocabulary spread.
    #[allow(dead_code)]
    pub(crate) distinct_commands: u64,
    pub(crate) has_nearby_prose: bool,
    pub(crate) burden: f64,
}

/// Walks the tree and builds per-`math_block` records. Inline math is
/// excluded because §14.3 explicitly scores display math blocks only.
pub(crate) fn analyze_math_blocks(
    root: &Node<'_>,
    source: &str,
    blocks: &[BlockSpan],
) -> Vec<MathBlock> {
    let mut out: Vec<MathBlock> = Vec::new();
    walk(root, source, blocks, &mut out);
    out.sort_by_key(|a| a.start_line);
    out
}

fn walk(node: &Node<'_>, source: &str, blocks: &[BlockSpan], out: &mut Vec<MathBlock>) {
    let kind = node.kind();
    if matches!(kind, NodeKind::MathBlock) {
        out.push(build(node, source, blocks));
        return;
    }
    for child in node.children() {
        walk(&child, source, blocks, out);
    }
}

fn build(node: &Node<'_>, source: &str, blocks: &[BlockSpan]) -> MathBlock {
    let start_line = (node.start_row() as u64) + 1;
    let (end_row, end_col) = node.end_position();
    let mut end = end_row;
    if end > node.start_row() && end_col == 0 {
        end -= 1;
    }
    let end_line = (end as u64) + 1;

    let start = node.start_byte();
    let end_b = node.end_byte();
    let body = &source[start..end_b];

    let tokens = body
        .split_whitespace()
        .filter(|t| !t.is_empty() && *t != "$$")
        .count() as u64;

    let mut commands: BTreeSet<String> = Default::default();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphabetic()) {
                j += 1;
            }
            if j > i + 1 {
                commands.insert(body[i..j].to_string());
                i = j;
                continue;
            }
        }
        i += 1;
    }

    let has_nearby_prose = has_prose_within(blocks, start_line, end_line, 2);
    let no_nearby_explanation = if has_nearby_prose { 0.0 } else { 1.0 };

    let burden =
        1.0 + 0.10 * tokens as f64 + 0.25 * commands.len() as f64 + 1.00 * no_nearby_explanation;

    MathBlock {
        start_line,
        end_line,
        tokens,
        distinct_commands: commands.len() as u64,
        has_nearby_prose,
        burden,
    }
}
