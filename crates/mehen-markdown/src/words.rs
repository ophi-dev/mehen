// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Narrative word counter `W` (§4) with anti-gaming defenses (§37.5).
//!
//! Counts `word_token`, `numeric_token`, `identifier_like_token`, and
//! `path_like_token` nodes *only when they appear inside prose contexts*.
//! A prose context is a `paragraph`, `block_quote`, `callout`, or a list
//! item's inline content. Heading text is intentionally excluded from `W`
//! to preserve the metric's body-prose denominator. Inline code, link
//! destinations, image destinations, raw HTML, MDX, math, autolinks,
//! front-matter, and pipe-table delimiter characters are NOT prose and do
//! not contribute.
//!
//! Alt-text inside an image or link *label* IS prose per §37.5 item 3, so
//! traversal into `link_label` is allowed — only the destination is skipped.

use crate::kind::NodeKind;
use crate::syntax_tree::Node;
use crate::tree_helpers::{ProseContext, is_non_prose_container, opens_prose_context};

/// Counts narrative word-like tokens across the document.
pub(crate) fn count_words(root: &Node<'_>) -> u64 {
    let mut total: u64 = 0;
    visit(root, &mut total, false);
    total
}

/// Walks the tree, accumulating word counts in prose-eligible contexts.
///
/// `inside_prose` tracks whether the current walk is below a prose-shaped
/// ancestor. Technical containers flip the flag off for their subtree.
fn visit(node: &Node<'_>, total: &mut u64, inside_prose: bool) {
    let kind = node.kind();
    if is_non_prose_container(kind) {
        return;
    }

    // A prose-eligible container opens a prose context for its descendants.
    let next_inside = inside_prose || opens_prose_context(kind, ProseContext::BODY);

    if next_inside && is_word_token(kind) {
        *total += 1;
    }

    // Recurse.
    for child in node.children() {
        visit(&child, total, next_inside);
    }
}

fn is_word_token(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::WordToken
            | NodeKind::NumericToken
            | NodeKind::IdentifierLikeToken
            | NodeKind::PathLikeToken
    )
}
