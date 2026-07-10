// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Effective Content Units per §6.
//!
//! ```text
//! ECU = W/240
//!     + 0.35 * CLOC
//!     + 0.06 * table_cells
//!     + 0.40 * diagram_nodes
//!     + 0.25 * diagram_edges
//!     + 0.12 * math_tokens
//!     + 0.20 * raw_html_or_mdx_lines
//! ```
//!
//! Phase A produces all terms except `diagram_nodes` and `diagram_edges`,
//! which remain `0` until Phase C implements Mermaid / PlantUML / DOT
//! extraction. `table_cells` = sum of `pipe_table_cell` children across
//! `pipe_table_row`. `math_tokens` = words-like tokens inside `math_block` /
//! `math_inline`. `raw_html_or_mdx_lines` = distinct physical lines whose
//! top-level owner is a raw HTML or MDX JSX block.

use std::collections::HashSet;

use crate::kind::NodeKind;
use crate::loc::LineClass;
use crate::loc::LineClasses;
use crate::syntax_tree::Node;
use crate::types::{EcuInputs, LocFamily};

/// Counts the ECU inputs that fall out of the AST. `classes` is used to
/// derive `raw_html_or_mdx_lines` without walking again.
pub(crate) fn compute_ecu_inputs(root: &Node<'_>, classes: &LineClasses) -> EcuInputs {
    let mut table_cells: u64 = 0;
    let mut math_tokens: u64 = 0;
    let mut raw_html_lines: HashSet<usize> = HashSet::new();

    walk(
        root,
        &mut table_cells,
        &mut math_tokens,
        &mut raw_html_lines,
    );

    // Also include any line classed as OtherArtifact that was covered by a
    // raw HTML / MDX block — the class map already did the heavy lifting.
    // We reuse the classes vector to enumerate lines and filter.
    let mut raw_html_or_mdx_lines: u64 = 0;
    let mut i = 0;
    while let Some(class) = classes.class_at(i) {
        if raw_html_lines.contains(&i) && class == LineClass::OtherArtifact {
            raw_html_or_mdx_lines += 1;
        }
        i += 1;
    }

    EcuInputs {
        table_cells,
        // TODO(Phase C): wire diagram extraction from fenced code blocks
        // with `mermaid` / `plantuml` / `dot` / `d2` info strings.
        diagram_nodes: 0,
        diagram_edges: 0,
        math_tokens,
        raw_html_or_mdx_lines,
    }
}

fn walk(
    node: &Node<'_>,
    table_cells: &mut u64,
    math_tokens: &mut u64,
    raw_html_lines: &mut HashSet<usize>,
) {
    use NodeKind::*;

    let kind = node.kind();

    match kind {
        // Count body-row cells per §6. The delimiter row is excluded because
        // it is pure structure (---, :---:); the header row is included
        // because its cells carry content.
        PipeTableHeader | PipeTableRow => {
            for child in node.children() {
                if matches!(child.kind(), PipeTableCell) {
                    *table_cells += 1;
                }
            }
            // Fall through to recurse for any nested content (e.g. inline
            // code inside a cell does not itself matter for Phase A).
        }
        // Math: count word-like tokens inside math_block / math_inline.
        MathBlock | MathInline => {
            let mut tokens: u64 = 0;
            count_math_tokens(node, &mut tokens);
            *math_tokens += tokens;
            // Do not recurse further — children are already counted.
            return;
        }
        // Raw HTML / MDX lines.
        HtmlBlock => {
            let start = node.start_row();
            let (end_row, end_col) = node.end_position();
            let mut end = end_row;
            if end > start && end_col == 0 {
                end -= 1;
            }
            for row in start..=end {
                raw_html_lines.insert(row);
            }
            return;
        }
        _ => {}
    }

    for child in node.children() {
        walk(&child, table_cells, math_tokens, raw_html_lines);
    }
}

fn count_math_tokens(node: &Node<'_>, total: &mut u64) {
    let kind = node.kind();
    if matches!(
        kind,
        NodeKind::WordToken
            | NodeKind::NumericToken
            | NodeKind::IdentifierLikeToken
            | NodeKind::PathLikeToken
    ) {
        *total += 1;
    }
    for child in node.children() {
        count_math_tokens(&child, total);
    }
}

/// Final ECU value per §6. Coefficients are exact and deterministic.
pub(crate) fn effective_content_units(loc: &LocFamily, words: u64, inputs: &EcuInputs) -> f64 {
    let words = words as f64 / 240.0;
    let code = 0.35 * loc.cloc as f64;
    let table = 0.06 * inputs.table_cells as f64;
    let diagram_n = 0.40 * inputs.diagram_nodes as f64;
    let diagram_e = 0.25 * inputs.diagram_edges as f64;
    let math = 0.12 * inputs.math_tokens as f64;
    let html = 0.20 * inputs.raw_html_or_mdx_lines as f64;
    words + code + table + diagram_n + diagram_e + math + html
}
