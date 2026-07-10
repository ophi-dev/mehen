// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Table burden and scaffolding metrics per §13.
//!
//! For each `pipe_table` node we compute dimensions (rows, cols, cells),
//! header presence, empty cell rate, and distinct alignments; from those we
//! derive per-table `T_burden` and `T_scaffold`, plus the aggregate scores
//! exported under `tables.*` in §23. Tables also participate in the
//! artifact-debt pipeline and in per-artifact "oversized" / "unexplained"
//! flags that Phase D consumes.

use crate::kind::NodeKind;
use crate::mathops::{clamp01, sat};
use crate::syntax_tree::Node;
use crate::types::{TableRecord, Tables};

/// Walks the tree and returns one [`TableRecord`] per `pipe_table`. The
/// caller later decides which tables have a nearby explanation (via the
/// shared prose-proximity helper in `nearby.rs`). This function sets
/// `has_local_explanation = false` by default; the analyzer patches it
/// afterwards so we can reuse the shared nearby helper.
pub(crate) fn analyze_tables(root: &Node<'_>, source: &str) -> Vec<TableRecord> {
    let mut out: Vec<TableRecord> = Vec::new();
    collect_tables(root, source, &mut out);
    out
}

fn collect_tables(node: &Node<'_>, source: &str, out: &mut Vec<TableRecord>) {
    if matches!(node.kind(), NodeKind::PipeTable) {
        out.push(build_record(node, source));
        return;
    }
    for child in node.children() {
        collect_tables(&child, source, out);
    }
}

fn build_record(table: &Node<'_>, source: &str) -> TableRecord {
    let start_line = (table.start_row() as u64) + 1;
    let (end_row, end_col) = table.end_position();
    let mut end = end_row;
    if end > table.start_row() && end_col == 0 {
        end -= 1;
    }
    let end_line = (end as u64) + 1;

    let mut rows: u64 = 0;
    let mut cols_by_row: Vec<u64> = Vec::new();
    let mut cells: u64 = 0;
    let mut empty_cells: u64 = 0;
    let mut has_header = false;
    let mut alignments: std::collections::BTreeSet<TableAlignment> = Default::default();

    for child in table.children() {
        match child.kind() {
            NodeKind::PipeTableHeader => {
                has_header = true;
                let (cells_in_row, empties) = count_cells(&child, source);
                cells += cells_in_row;
                empty_cells += empties;
                cols_by_row.push(cells_in_row);
            }
            NodeKind::PipeTableRow => {
                rows += 1;
                let (cells_in_row, empties) = count_cells(&child, source);
                cells += cells_in_row;
                empty_cells += empties;
                cols_by_row.push(cells_in_row);
            }
            NodeKind::PipeTableDelimiterRow => {
                collect_alignments(&child, &mut alignments);
            }
            _ => {}
        }
    }

    let cols = cols_by_row.iter().copied().max().unwrap_or(0);

    let empty_rate = if cells == 0 {
        0.0
    } else {
        empty_cells as f64 / cells as f64
    };

    let distinct_alignments = alignments.len() as u64;

    let wide_penalty = sat(cols as f64, 5.0, 12.0);
    let long_penalty = sat(rows as f64, 20.0, 100.0);
    let cell_penalty = sat(cells as f64, 60.0, 300.0);
    let missing_header_penalty = if has_header { 0.0 } else { 1.0 };
    let empty_penalty = sat(empty_rate, 0.10, 0.50);
    let alignment_complexity = distinct_alignments as f64 / (cols.max(1) as f64);

    let burden = clamp01(
        0.25 * wide_penalty
            + 0.25 * long_penalty
            + 0.25 * cell_penalty
            + 0.15 * missing_header_penalty
            + 0.05 * empty_penalty
            + 0.05 * alignment_complexity,
    );

    let hard_warning = cells > 300 || cols > 12 || rows > 100;

    let size_credit = if cells < 6 {
        0.2
    } else if cells <= 60 {
        1.0
    } else {
        let reduced = 1.0 - ((cells as f64 - 60.0) / 120.0);
        clamp01(reduced)
    };
    // §13.2: TableScaffold uses has_header as a boolean weight + pending
    // local_explanation. The analyzer patches the explanation in a second
    // pass; until then we store scaffold = 0. `has_local_explanation` stays
    // false by default.
    let scaffold = size_credit * (if has_header { 1.0 } else { 0.0 });

    TableRecord {
        start_line,
        end_line,
        rows,
        cols,
        cells,
        has_header,
        empty_rate,
        distinct_alignments,
        has_local_explanation: false,
        burden,
        scaffold,
        hard_warning,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TableAlignment {
    Default,
    Left,
    Right,
    Center,
}

fn collect_alignments(
    delimiter_row: &Node<'_>,
    alignments: &mut std::collections::BTreeSet<TableAlignment>,
) {
    for child in delimiter_row.children() {
        if matches!(child.kind(), NodeKind::PipeTableDelimiterCell) {
            let align = cell_alignment(&child);
            alignments.insert(align);
        }
    }
}

fn cell_alignment(cell: &Node<'_>) -> TableAlignment {
    let mut has_left = false;
    let mut has_right = false;
    for child in cell.children() {
        match child.kind() {
            NodeKind::PipeTableAlignLeft => has_left = true,
            NodeKind::PipeTableAlignRight => has_right = true,
            _ => {}
        }
    }
    match (has_left, has_right) {
        (true, true) => TableAlignment::Center,
        (true, false) => TableAlignment::Left,
        (false, true) => TableAlignment::Right,
        _ => TableAlignment::Default,
    }
}

fn count_cells(row: &Node<'_>, source: &str) -> (u64, u64) {
    let mut cells: u64 = 0;
    let mut empties: u64 = 0;
    for child in row.children() {
        if matches!(child.kind(), NodeKind::PipeTableCell) {
            cells += 1;
            let start = child.start_byte();
            let end = child.end_byte();
            let text = String::from_utf8_lossy(&source.as_bytes()[start..end]);
            if text.trim().is_empty() {
                empties += 1;
            }
        }
    }
    (cells, empties)
}

/// Aggregate §13 `TableBurdenScore` + `TableScaffoldScore` over all tables.
pub(crate) fn aggregate_tables(records: &[TableRecord]) -> Tables {
    let mut t = Tables {
        count: records.len() as u64,
        max_cells: records.iter().map(|r| r.cells).max().unwrap_or(0),
        hard_warnings: records.iter().filter(|r| r.hard_warning).count() as u64,
        ..Tables::default()
    };
    if records.is_empty() {
        return t;
    }
    let mean = records.iter().map(|r| r.burden).sum::<f64>() / records.len() as f64;
    let max = records.iter().map(|r| r.burden).fold(f64::MIN, f64::max);
    t.table_burden_score = 0.5 * mean + 0.5 * max;

    let sum_scaffold: f64 = records
        .iter()
        .map(|r| r.scaffold * if r.has_local_explanation { 1.0 } else { 0.0 })
        .sum();
    let divisor = (records.len() as f64).sqrt().max(1.0);
    t.table_scaffold_score = clamp01(sum_scaffold / divisor);
    t
}
