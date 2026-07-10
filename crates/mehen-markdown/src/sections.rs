// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Derived section tree per §3.4.
//!
//! The Markdown syntax layer synthesizes a nested `section` AST: each heading
//! opens a `section` that contains all downstream blocks until the next
//! same-or-higher-level heading. Heading skips (e.g. H1 → H3) keep the
//! intervening depth collapsed — no virtual sections are synthesized. This
//! module flattens that tree into the
//! [`crate::types::Section`] list consumed by the exported schema.
//!
//! Parent/child relationships are preserved by walking in pre-order and
//! emitting the parent section before its children. This matches §3.4
//! which requires a `parent_section_id` pointing to the enclosing heading's
//! section and a `child_section_ids` list of directly-nested subsections.

use crate::kind::NodeKind;
use crate::syntax_tree::Node;
use crate::types::Section;
use crate::words::count_words;

/// Collects sections (one per heading) in document order.
///
/// §3.4 defines the derived section tree as *one section per heading*. A
/// document with no headings returns an empty list. Pre-heading content
/// is accounted for in `size.words` but has no section of its own.
///
/// Internally we keep a synthetic "file" placeholder so the tree walk can
/// attribute pre-heading content and preserve parent/child ids during
/// construction; that placeholder is dropped and the remaining sections
/// are renumbered before returning to the caller.
pub(crate) fn collect_sections(root: &Node<'_>) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();

    // Synthetic root used only during walk. Dropped before return.
    sections.push(Section {
        section_id: 0,
        heading_level: None,
        heading_text: None,
        start_line: (root.start_row() as u64) + 1,
        end_line: section_end_line(root),
        parent_section_id: None,
        child_section_ids: Vec::new(),
        word_count: 0,
        block_count: 0,
    });

    walk(root, 0, &mut sections);

    populate_word_and_block_counts(root, &mut sections);

    // Strip the synthetic root and renumber remaining sections so the
    // exported `sections` array reflects only heading-rooted sections with
    // contiguous ids starting at 0.
    sections.remove(0);
    renumber_sections(&mut sections);

    sections
}

/// Renumbers `sections` so `section_id` is the array index and every
/// `parent_section_id` / `child_section_ids` entry refers to the renumbered
/// ids. Sections whose parent was the dropped synthetic root become
/// top-level (`parent_section_id = None`).
fn renumber_sections(sections: &mut [Section]) {
    // Map old section_id -> new index. Since the synthetic root lived at
    // id 0, every remaining section's old id is >= 1. The new order is
    // the current vector order.
    let old_to_new: std::collections::HashMap<usize, usize> = sections
        .iter()
        .enumerate()
        .map(|(new_idx, s)| (s.section_id, new_idx))
        .collect();

    for (new_idx, section) in sections.iter_mut().enumerate() {
        section.section_id = new_idx;
        section.parent_section_id = match section.parent_section_id {
            Some(0) | None => None,
            Some(old_parent) => old_to_new.get(&old_parent).copied(),
        };
        section.child_section_ids = section
            .child_section_ids
            .iter()
            .filter_map(|old_id| old_to_new.get(old_id).copied())
            .collect();
    }
}

fn walk(node: &Node<'_>, parent_id: usize, sections: &mut Vec<Section>) {
    for child in node.children() {
        if is_section_node(child.kind()) {
            if let Some(heading) = find_heading_in_section(&child) {
                let (level, heading_text) = {
                    let (lvl, txt) = describe_heading(&heading);
                    (Some(lvl), txt)
                };
                let section_id = sections.len();
                // Sections nest H1 → H2 → H3 by construction. Heading
                // skips (H1 → H3) keep the H3 under whichever section wraps
                // it — we do not fabricate virtual sections.
                sections[parent_id].child_section_ids.push(section_id);
                sections.push(Section {
                    section_id,
                    heading_level: level,
                    heading_text,
                    start_line: (child.start_row() as u64) + 1,
                    end_line: section_end_line(&child),
                    parent_section_id: Some(parent_id),
                    child_section_ids: Vec::new(),
                    word_count: 0,
                    block_count: 0,
                });
                walk(&child, section_id, sections);
            } else {
                // A `Section` node without a heading is a structural
                // wrapper (empty or pre-heading). Recurse into it but treat
                // its content as belonging to the enclosing section.
                walk(&child, parent_id, sections);
            }
        } else {
            // Non-section nodes can still contain sections (e.g. when a
            // block is between sections), so recurse.
            walk(&child, parent_id, sections);
        }
    }
}

fn is_section_node(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Section { .. })
}

fn find_heading_in_section<'a>(section: &Node<'a>) -> Option<Node<'a>> {
    section.children().find(|child| child.kind().is_heading())
}

fn describe_heading(heading: &Node<'_>) -> (u8, Option<String>) {
    let level = heading.kind().heading_level().unwrap_or(1);
    let text = heading_content_node(heading).map(|node| {
        let start = node.start_byte();
        let end = node.end_byte();
        let _ = (start, end);
        // Heading text extraction from source bytes is Phase-B territory
        // (needed for information-scent / RCI). Phase A leaves it as `None`
        // until the source-bytes-aware constructor lands.
        String::new()
    });
    // Drop the empty string — return `None` to preserve semantic meaning.
    let text = text.filter(|s| !s.is_empty());
    (level, text)
}

fn heading_content_node<'a>(heading: &Node<'a>) -> Option<Node<'a>> {
    heading
        .children()
        .find(|child| matches!(child.kind(), NodeKind::HeadingContent))
}

fn section_end_line(section: &Node<'_>) -> u64 {
    let (end_row, end_col) = section.end_position();
    let end = if end_col == 0 && end_row > section.start_row() {
        end_row - 1
    } else {
        end_row
    };
    (end as u64) + 1
}

fn populate_word_and_block_counts(root: &Node<'_>, sections: &mut [Section]) {
    if sections.is_empty() {
        return;
    }

    // Block counts: count paragraph / list / table / code / html / math /
    // callout / thematic-break / image-block blocks per section range. Since
    // the grammar already nests blocks inside the correct section, walking
    // each section's subtree yields the right count.
    //
    // Word counts: each section's subtree minus nested sub-section subtrees
    // to avoid double-counting. This is achieved by computing the subtree
    // word count, then subtracting the children's subtree counts.

    // Root section: every block and every word in the document.
    // We compute the root's subtree first, then per-sub-section.
    let mut subtree_words: Vec<u64> = vec![0; sections.len()];
    let mut subtree_blocks: Vec<u64> = vec![0; sections.len()];

    // For the root "document" section (id 0), traverse the whole tree.
    subtree_words[0] = count_words(root);
    subtree_blocks[0] = count_blocks(root);

    // For every other section, find its subtree by matching its start/end
    // line range against the tree.
    for s in sections.iter().skip(1) {
        if let Some(node) = find_section_node(root, s.start_line, s.end_line) {
            subtree_words[s.section_id] = count_words(&node);
            subtree_blocks[s.section_id] = count_blocks(&node);
        }
    }

    // Convert subtree counts → own counts (subtree minus children).
    let child_ids: Vec<Vec<usize>> = sections
        .iter()
        .map(|s| s.child_section_ids.clone())
        .collect();
    for (i, section) in sections.iter_mut().enumerate() {
        let mut words_own = subtree_words[i];
        let mut blocks_own = subtree_blocks[i];
        for &c in &child_ids[i] {
            words_own = words_own.saturating_sub(subtree_words[c]);
            blocks_own = blocks_own.saturating_sub(subtree_blocks[c]);
        }
        section.word_count = words_own;
        section.block_count = blocks_own;
    }
}

fn count_blocks(node: &Node<'_>) -> u64 {
    let mut total: u64 = 0;
    visit_blocks(node, &mut total);
    total
}

fn visit_blocks(node: &Node<'_>, total: &mut u64) {
    if is_block(node.kind()) {
        *total += 1;
    }
    for child in node.children() {
        visit_blocks(&child, total);
    }
}

fn is_block(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Paragraph
            | NodeKind::FencedCodeBlock
            | NodeKind::IndentedCodeBlock
            | NodeKind::HtmlBlock
            | NodeKind::MathBlock
            | NodeKind::PipeTable
            | NodeKind::ListItem { .. }
            | NodeKind::BlockQuote
            | NodeKind::Callout
            | NodeKind::List
            | NodeKind::ThematicBreak
            | NodeKind::FootnoteDefinition
            | NodeKind::LinkReferenceDefinition
    )
}

/// Locates the AST node whose start/end lines match a section's recorded
/// range. The section walk is small so a linear search is fine.
fn find_section_node<'a>(root: &Node<'a>, start_line: u64, end_line: u64) -> Option<Node<'a>> {
    let mut stack = vec![*root];
    while let Some(node) = stack.pop() {
        let (s_row, _) = node.start_position();
        let (e_row, e_col) = node.end_position();
        let s = (s_row as u64) + 1;
        let mut e = (e_row as u64) + 1;
        if e_col == 0 && e > s {
            e -= 1;
        }
        if is_section_node(node.kind()) && s == start_line && e == end_line {
            return Some(node);
        }
        stack.extend(node.children());
    }
    None
}
