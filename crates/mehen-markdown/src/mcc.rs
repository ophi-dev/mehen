// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Markdown Cognitive Complexity (MCC) per §8.
//!
//! Walks the AST, accumulates per-element base weights from §8.1, applies the
//! §8.2 nesting multiplier (`1 + 0.18 * nest(n)`), and the §8.3 cluster
//! multiplier computed from a rolling 20-line window of artifact density,
//! then subtracts scaffold credit per §8.4 (capped at `0.25 * MCC_positive`).
//!
//! Phase-B stubs:
//! - Broken internal/relative link (+3.00) → 0.00 until Phase C link
//!   validator lands.
//! - External link unchecked (+0.30) → applied (external link always adds a
//!   small penalty pending validation).
//! - External link broken (+4.00) → 0.00 until Phase C.
//! - Diagram parse error (+3.00) → 0.00 until Phase C diagram parser lands.

use crate::document::{MarkdownDocument, is_diagram_language};
use crate::kind::NodeKind;
use crate::syntax_tree::Node;
use crate::tree_helpers::{
    count_table_cells, find_link_label, has_scheme as is_external, node_line_span,
};
use mehen_core::{ContributionCollector, SourceSpan};

/// The published metric key MCC evidence attaches to.
const MCC_KEY: &str = "markdown.complexity.cognitive_complexity";

/// §8 aggregate: positive weight before credit, credit amount used, final
/// MCC. Only `mcc` is exported to the public record; `positive` and
/// `credit_used` stay accessible to in-crate tests so we can assert the
/// intermediate arithmetic.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MccResult {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) positive: f64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) credit_used: f64,
    pub(crate) mcc: f64,
}

/// Public entry point.
///
/// `evidence` receives contribution records (plan §5.4) for the
/// prose-structure events a doc author can act on directly — heading
/// skips, oversized flat sections, over-long paragraphs, dense link
/// clusters — with the exact weighted amount each added to the positive
/// MCC term. Scaffold credit is applied globally (capped), so the final
/// `mcc` is not a plain sum of the evidence; the evidence explains the
/// positive side.
pub(crate) fn compute_mcc(
    root: &Node<'_>,
    document: &MarkdownDocument,
    source: &str,
    evidence: &mut ContributionCollector,
) -> MccResult {
    let mut ctx = Walker::new(source, document, evidence);
    // Pass 1: collect artifact lines for the 20-line-window cluster density
    // and record each block's sequence index for §8.4 locality lookup.
    ctx.scan_blocks(root);
    // Pass 2: accumulate weights and queue scaffold-credit candidates.
    ctx.walk(root);

    let credit_raw: f64 = ctx.pending_credits.iter().sum();
    let credit = credit_raw.min(0.25 * ctx.positive);
    let mcc = (ctx.positive - credit).max(0.0);
    MccResult {
        positive: ctx.positive,
        credit_used: credit,
        mcc,
    }
}

struct Walker<'a, 'doc, 'ev> {
    source: &'a str,
    document: &'doc MarkdownDocument,
    /// Contribution-evidence sink (plan §5.4). `record` is a no-op when
    /// collection is disabled.
    evidence: &'ev mut ContributionCollector,
    positive: f64,
    /// Individual scaffold-credit contributions queued during the walk.
    /// They are summed and capped at `0.25 * positive` after the walk.
    pending_credits: Vec<f64>,
    last_heading_level: Option<u8>,
    /// Each physical line has `1` if an artifact block touches it, else `0`.
    /// Used for the §8.3 cluster multiplier.
    artifact_line: Vec<bool>,
    /// The ordered list of block-level node starts keyed by `BlockKind`.
    /// Used to check "prose / heading within ±2 blocks" for §8.4.
    blocks: Vec<(BlockKind, u32)>,
    // Nesting depths tracked during recursive walk.
    list_depth: u32,
    blockquote_depth: u32,
    callout_depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Paragraph,
    Code,
    Table,
    Math,
    RawHtml,
    Heading,
    Other,
}

impl<'a, 'doc, 'ev> Walker<'a, 'doc, 'ev> {
    fn new(
        source: &'a str,
        document: &'doc MarkdownDocument,
        evidence: &'ev mut ContributionCollector,
    ) -> Self {
        let mut lines = 1usize;
        for b in source.bytes() {
            if b == b'\n' {
                lines += 1;
            }
        }
        if source.ends_with('\n') {
            lines = lines.saturating_sub(1);
        }
        Self {
            source,
            document,
            evidence,
            positive: 0.0,
            pending_credits: Vec::new(),
            last_heading_level: None,
            artifact_line: vec![false; lines.max(1)],
            blocks: Vec::new(),
            list_depth: 0,
            blockquote_depth: 0,
            callout_depth: 0,
        }
    }

    /// Span of `node` in byte + 1-based-line coordinates for evidence
    /// records. The syntax tree exposes byte offsets and 0-based rows
    /// directly, so no `LineIndex` round-trip is needed.
    fn node_span(node: &Node<'_>) -> SourceSpan {
        let (end_row, end_col) = node.end_position();
        let mut end = end_row;
        if end > node.start_row() && end_col == 0 {
            end -= 1;
        }
        SourceSpan {
            start_byte: mehen_core::byte_offset_clamped(node.start_byte()),
            end_byte: mehen_core::byte_offset_clamped(node.end_byte()),
            start_line: node.start_row() as u32 + 1,
            end_line: end as u32 + 1,
        }
    }

    fn scan_blocks(&mut self, node: &Node<'_>) {
        use NodeKind::*;
        let kind = node.kind();
        let bk = classify_block(kind);
        let is_artifact = matches!(
            kind,
            FencedCodeBlock | IndentedCodeBlock | PipeTable | MathBlock | HtmlBlock
        );
        if is_artifact {
            let start = node.start_row();
            let (end_row, end_col) = node.end_position();
            let mut end = end_row;
            if end > start && end_col == 0 {
                end -= 1;
            }
            for row in start..=end.min(self.artifact_line.len().saturating_sub(1)) {
                self.artifact_line[row] = true;
            }
        }
        if bk != BlockKind::Other {
            self.blocks.push((bk, node.start_row() as u32));
        }
        for child in node.children() {
            self.scan_blocks(&child);
        }
    }

    fn walk(&mut self, node: &Node<'_>) {
        use NodeKind::*;

        let kind = node.kind();

        // Headings.
        if kind.is_heading() {
            let level = kind.heading_level().unwrap_or(1);
            if let Some(prev) = self.last_heading_level
                && level > prev
            {
                // Deeper level. Penalize a heading skip (>= 2 steps)
                // with 1.00; a smooth +1 step earns the normal 0.20.
                let delta = level.saturating_sub(prev);
                if delta == 1 {
                    self.positive += 0.20 * self.current_nest_multiplier();
                } else {
                    let amount = 1.00 * self.current_nest_multiplier();
                    self.positive += amount;
                    // Plan §5.4's canonical Markdown example: a heading
                    // that skips levels (## → ####) is a structure
                    // defect a reader can point at.
                    self.evidence.record(
                        MCC_KEY,
                        Self::node_span(node),
                        amount,
                        "markdown.heading_skip",
                    );
                }
            }
            // First heading and going-shallower: no penalty.
            self.last_heading_level = Some(level);
        }

        // Section without subheading + > 800 words — checked only when the
        // node is a `Section` container.
        if is_section(kind) && section_has_no_sub_heading(node) {
            let words = count_section_words(node);
            if words > 800 {
                let amount = 2.00 * self.current_nest_multiplier();
                self.positive += amount;
                self.evidence.record(
                    MCC_KEY,
                    Self::node_span(node),
                    amount,
                    "markdown.oversized_flat_section",
                );
            }
        }

        // Paragraph > 160 words → 1.25.
        if matches!(kind, Paragraph) {
            let words = count_word_tokens(node);
            if words > 160 {
                let amount = 1.25 * self.current_nest_multiplier();
                self.positive += amount;
                self.evidence.record(
                    MCC_KEY,
                    Self::node_span(node),
                    amount,
                    "markdown.overlong_paragraph",
                );
            }
            // Dense link cluster: > 4 inline links in a paragraph → 1.50.
            let links = count_inline_links(node);
            if links > 4 {
                let amount = 1.50 * self.cluster_multiplier(node) * self.current_nest_multiplier();
                self.positive += amount;
                self.evidence.record(
                    MCC_KEY,
                    Self::node_span(node),
                    amount,
                    "markdown.dense_link_cluster",
                );
            }
        }

        // Lists and list structures.
        match kind {
            List => {
                self.positive += 0.40 * self.current_nest_multiplier();
                self.list_depth += 1;
                self.recurse(node);
                self.list_depth -= 1;
                return;
            }
            ListItem { task: false } => {
                // Nested list level: charge 0.50 * depth per §8.1. `depth`
                // here is the current list-depth *before* the list-item
                // increments it further; using list_depth directly approximates
                // "level" since each outer list already incremented the depth.
                self.positive +=
                    0.50 * self.list_depth.max(1) as f64 * self.current_nest_multiplier();
            }
            ListItem { task: true } => {
                self.positive += 0.35 * self.current_nest_multiplier();
            }
            BlockQuote => {
                self.positive += 0.50 * self.current_nest_multiplier();
                self.blockquote_depth += 1;
                self.recurse(node);
                self.blockquote_depth -= 1;
                return;
            }
            Callout => {
                self.positive += 0.75 * self.current_nest_multiplier();
                self.callout_depth += 1;
                self.recurse(node);
                self.callout_depth -= 1;
                return;
            }
            _ => {}
        }

        // Inline links / images (not the whole paragraph).
        if matches!(kind, Link) {
            self.positive += 0.25 * self.current_nest_multiplier();
            // External link unchecked → +0.30 per §8.1. Phase B applies this
            // universally until Phase C differentiates valid / broken.
            if let Some(dest) = self
                .document
                .link_destination_by_span(node.start_byte(), node.end_byte())
                && is_external(dest)
            {
                self.positive += 0.30 * self.current_nest_multiplier();
            }
            // TODO(Phase C): broken internal/relative link → +3.00;
            // external broken → +4.00. Left at 0.00 until the link
            // validator lands.
        }

        // Footnote reference.
        if matches!(kind, FootnoteReference) {
            self.positive += 0.60 * self.current_nest_multiplier();
        }

        // Images.
        if matches!(kind, Image) {
            self.positive += 0.50 * self.current_nest_multiplier();
            // §8.4 credit: image with alt/caption + nearby explanation,
            // bounded. We approximate `alt` as the non-empty link-label
            // text inside the Image node.
            let label = find_link_label(node, self.source).unwrap_or_default();
            let has_label = !label.trim().is_empty();
            if has_label {
                let start = node.start_row() as u32;
                let local = local_explanation(&self.blocks, start);
                // Base credit for image 0.80; bounded = 1 since we have no
                // size for the rendered image yet — Phase C can refine.
                let credit = 0.80 * (local as f64) * 1.0;
                if credit > 0.0 {
                    self.pending_credits.push(credit);
                }
            }
        }
        // Code fences.
        if matches!(kind, FencedCodeBlock | IndentedCodeBlock)
            && let Some(block) = self.document.code_block_by_start_row(node.start_row())
        {
            // LOC counts pulldown code text only — fence markers never
            // enter the size-based weighting.
            let loc = block.content_line_count();
            let is_diagram = block.language.as_deref().is_some_and(is_diagram_language);
            if is_diagram {
                self.positive +=
                    1.50 * self.cluster_multiplier(node) * self.current_nest_multiplier();
                // §8.4 diagram credit: 1.25 * local_explanation *
                // has_label * bounded. Phase B doesn't have a caption
                // detector yet — use a conservative `has_label = 1`
                // (the diagram language tag makes the type clear) and
                // local explanation via ±2 blocks.
                let start = block.start_line.saturating_sub(1) as u32;
                let local = local_explanation(&self.blocks, start);
                let credit = 1.25 * (local as f64) * 1.0;
                if credit > 0.0 {
                    self.pending_credits.push(credit);
                }
                // TODO(Phase C): diagram parse error → +3.00. Stub.
            } else {
                let base = if loc <= 12 {
                    1.00
                } else {
                    1.00 + 0.08 * (loc as f64 - 12.0)
                };
                let unlabelled = !block.is_fenced() || block.language.is_none();
                let mut weight = base;
                if unlabelled {
                    weight += 1.50;
                }
                self.positive +=
                    weight * self.cluster_multiplier(node) * self.current_nest_multiplier();
                // §8.4 scaffold credit for code examples:
                //   0.75 * local_explanation * has_label * bounded
                // where has_label = language tag present, bounded = 1 if
                // loc <= 30 decaying to 0 at loc == 60.
                if !unlabelled {
                    let start = block.start_line.saturating_sub(1) as u32;
                    let local = local_explanation(&self.blocks, start);
                    let bounded = bounded_size(loc as f64, 30.0, 60.0);
                    let credit = 0.75 * (local as f64) * bounded;
                    if credit > 0.0 {
                        self.pending_credits.push(credit);
                    }
                }
            }
        }

        // Pipe tables.
        if matches!(kind, PipeTable) {
            let cells = count_table_cells(node);
            let weight = if cells <= 60 {
                0.75
            } else {
                0.75 + 0.03 * (cells as f64 - 60.0).powf(0.85)
            };
            self.positive +=
                weight * self.cluster_multiplier(node) * self.current_nest_multiplier();
            // §8.4 table credit: 1.00 * local_explanation * has_header *
            // bounded. `bounded` fades from 1 at 60 cells to 0 at 150.
            let has_header = pipe_table_has_header(node);
            if has_header && cells > 0 {
                let start = node.start_row() as u32;
                let local = local_explanation(&self.blocks, start);
                let bounded = bounded_size(cells as f64, 60.0, 150.0);
                let credit = 1.00 * (local as f64) * bounded;
                if credit > 0.0 {
                    self.pending_credits.push(credit);
                }
            }
        }

        // Math blocks.
        if matches!(kind, MathBlock) {
            self.positive += 1.50 * self.cluster_multiplier(node) * self.current_nest_multiplier();
            // §8.4 math credit: 0.50 * local_explanation * bounded. Use
            // line span as the size proxy.
            let start = node.start_row() as u32;
            let local = local_explanation(&self.blocks, start);
            let lines = node_line_span(node) as f64;
            let bounded = bounded_size(lines, 6.0, 20.0);
            let credit = 0.50 * (local as f64) * bounded;
            if credit > 0.0 {
                self.pending_credits.push(credit);
            }
        }

        // Raw HTML blocks: 0.30 * lines, cap 8.
        if matches!(kind, HtmlBlock) {
            let lines = node_line_span(node) as f64;
            let weight = (0.30 * lines).min(8.0);
            self.positive +=
                weight * self.cluster_multiplier(node) * self.current_nest_multiplier();
        }

        self.recurse(node);
    }

    fn recurse(&mut self, node: &Node<'_>) {
        for child in node.children() {
            self.walk(&child);
        }
    }

    fn current_nest_multiplier(&self) -> f64 {
        let nest = self.list_depth + self.blockquote_depth + self.callout_depth;
        1.0 + 0.18 * nest as f64
    }

    fn cluster_multiplier(&self, node: &Node<'_>) -> f64 {
        // 20-line window centered on the node's start row.
        let start = node.start_row();
        let lo = start.saturating_sub(10);
        let hi = (start + 10).min(self.artifact_line.len());
        let window = &self.artifact_line[lo..hi];
        if window.is_empty() {
            return 1.0;
        }
        let hits = window.iter().filter(|b| **b).count() as f64;
        let density = hits / window.len() as f64;
        1.0 + saturate(density, 0.15, 0.45) * 0.35
    }
}

/// `1` if a prose / heading block exists within ±2 blocks of the `start_row`.
///
/// `blocks` is the document-order block list. We find the nearest block
/// matching `start_row` and peek 2 neighbours to each side. A prose block
/// (Paragraph) or Heading counts as a local explanation.
fn local_explanation(blocks: &[(BlockKind, u32)], start_row: u32) -> u8 {
    let idx = match blocks.iter().position(|(_, row)| *row == start_row) {
        Some(i) => i,
        None => {
            // Fall back: closest block by absolute row distance.
            let mut best: Option<usize> = None;
            let mut best_d = u32::MAX;
            for (i, (_, r)) in blocks.iter().enumerate() {
                let d = r.abs_diff(start_row);
                if d < best_d {
                    best_d = d;
                    best = Some(i);
                }
            }
            match best {
                Some(i) => i,
                None => return 0,
            }
        }
    };
    let lo = idx.saturating_sub(2);
    let hi = (idx + 3).min(blocks.len());
    for (i, (bk, _)) in blocks[lo..hi].iter().enumerate() {
        let abs = lo + i;
        if abs == idx {
            continue;
        }
        if matches!(bk, BlockKind::Paragraph | BlockKind::Heading) {
            return 1;
        }
    }
    0
}

/// Returns `1 - sat(size; useful_hi, severe_hi)` per §8.4 `bounded(a)`.
fn bounded_size(size: f64, useful_hi: f64, severe_hi: f64) -> f64 {
    1.0 - saturate(size, useful_hi, severe_hi)
}

fn saturate(x: f64, lo: f64, hi: f64) -> f64 {
    if hi <= lo {
        return 0.0;
    }
    ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
}

fn classify_block(kind: NodeKind) -> BlockKind {
    use NodeKind::*;
    match kind {
        Paragraph => BlockKind::Paragraph,
        FencedCodeBlock | IndentedCodeBlock => BlockKind::Code,
        PipeTable => BlockKind::Table,
        MathBlock => BlockKind::Math,
        HtmlBlock => BlockKind::RawHtml,
        Heading { .. } => BlockKind::Heading,
        _ => BlockKind::Other,
    }
}

fn is_section(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Section { .. })
}

fn section_has_no_sub_heading(section: &Node<'_>) -> bool {
    !section.children().any(|child| is_section(child.kind()))
}

fn count_section_words(node: &Node<'_>) -> u64 {
    let mut total = 0u64;
    walk_words(node, &mut total);
    total
}

fn walk_words(node: &Node<'_>, total: &mut u64) {
    use NodeKind::*;
    let kind = node.kind();
    // Don't descend into stop-containers — mirrors `words.rs` rules.
    match kind {
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
        | Autolink
        | Uri
        | Email
        | LinkDestination
        | LinkTitle
        | MinusMetadata
        | PlusMetadata
        | PipeTableDelimiterRow => {
            return;
        }
        _ => {}
    }
    if matches!(
        kind,
        WordToken | NumericToken | IdentifierLikeToken | PathLikeToken
    ) {
        *total += 1;
    }
    for child in node.children() {
        walk_words(&child, total);
    }
}

fn count_word_tokens(node: &Node<'_>) -> u64 {
    let mut total = 0u64;
    walk_words(node, &mut total);
    total
}

fn count_inline_links(node: &Node<'_>) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), NodeKind::Link) {
            total += 1;
        }
        stack.extend(n.children());
    }
    total
}

fn pipe_table_has_header(node: &Node<'_>) -> bool {
    node.children()
        .any(|child| matches!(child.kind(), NodeKind::PipeTableHeader))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute(src: &str) -> MccResult {
        let (tree, document) = crate::syntax_tree::parse_with_document(src);
        let mut evidence = ContributionCollector::new(false);
        compute_mcc(&tree.root(), &document, src, &mut evidence)
    }

    fn compute_with_evidence(src: &str) -> (MccResult, Vec<mehen_core::MetricContribution>) {
        let (tree, document) = crate::syntax_tree::parse_with_document(src);
        let mut evidence = ContributionCollector::new(true);
        let result = compute_mcc(&tree.root(), &document, src, &mut evidence);
        (result, evidence.finish())
    }

    #[test]
    fn heading_skip_records_evidence_with_the_applied_weight() {
        let src = "# Top\n\n### Skipped\n";
        let (result, contributions) = compute_with_evidence(src);
        let skip: Vec<_> = contributions
            .iter()
            .filter(|c| c.reason.as_str() == "markdown.heading_skip")
            .collect();
        assert_eq!(skip.len(), 1);
        assert_eq!(skip[0].amount, 1.0);
        assert_eq!(skip[0].span.start_line, 3);
        assert_eq!(
            skip[0].metric.as_str(),
            "markdown.complexity.cognitive_complexity"
        );
        assert!(result.positive >= skip[0].amount);
    }

    #[test]
    fn oversized_flat_section_and_overlong_paragraph_record_evidence() {
        let filler = "word ".repeat(801);
        let src = format!("# Title\n\n{}\n", filler);
        let (_, contributions) = compute_with_evidence(&src);
        let reasons: Vec<&str> = contributions.iter().map(|c| c.reason.as_str()).collect();
        assert!(reasons.contains(&"markdown.oversized_flat_section"));
        assert!(reasons.contains(&"markdown.overlong_paragraph"));
    }

    #[test]
    fn empty_doc_mcc_zero() {
        let r = compute("");
        assert_eq!(r.mcc, 0.0);
        assert_eq!(r.positive, 0.0);
    }

    #[test]
    fn heading_skip_penalizes() {
        let src = "# Top\n\n### Skipped\n";
        let r = compute(src);
        // Heading skip H1→H3 contributes 1.00 with nest_multiplier=1.
        assert!(r.positive >= 1.0, "positive: {}", r.positive);
    }

    #[test]
    fn section_800_words_charges() {
        // Build a section with ≥ 801 words.
        let filler = "word ".repeat(801);
        let src = format!("# Title\n\n{}\n", filler);
        let r = compute(&src);
        // §8.1 charges 2.00 per section-without-subheading > 800 words.
        assert!(r.positive >= 2.0, "positive: {}", r.positive);
    }

    #[test]
    fn fences_and_tables_adjust_cluster() {
        let src = "# T\n\n```\nfoo\n```\n\n```\nbar\n```\n";
        let r = compute(src);
        // Two unlabelled code fences: 1.00 + 1.50 penalty each. They sit in
        // an artifact-dense window, so cluster multiplier > 1.
        assert!(r.positive > 5.0, "positive: {}", r.positive);
    }

    #[test]
    fn unlabelled_code_fence_adds_1_5() {
        let labelled = "# H\n\nIntro prose.\n\n```rust\nlet x = 1;\n```\n\nExplanation.\n";
        let unlabelled = "# H\n\nIntro prose.\n\n```\nlet x = 1;\n```\n\nExplanation.\n";
        let r1 = compute(labelled);
        let r2 = compute(unlabelled);
        // The positive difference between unlabelled and labelled should be
        // at least 1.50 (after matching cluster multipliers). Allow for tiny
        // numeric drift due to cluster windows.
        assert!(
            r2.positive - r1.positive >= 1.49,
            "unlabelled delta: {:.4}",
            r2.positive - r1.positive
        );
    }

    #[test]
    fn reference_style_external_link_matches_inline_mcc() {
        let inline = "# H\n\nSee [docs](https://example.com).\n";
        let reference = "# H\n\nSee [docs][api\\]].\n\n[api\\]]: https://example.com\n";
        let a = compute(inline);
        let b = compute(reference);

        assert_eq!(
            a.positive, b.positive,
            "inline vs external reference positive mismatch: {:?} vs {:?}",
            a, b
        );
        assert_eq!(
            a.mcc, b.mcc,
            "inline vs external reference mcc mismatch: {:?} vs {:?}",
            a, b
        );
    }

    #[test]
    fn scaffold_credit_subtracts_cap() {
        // A code example with language tag + adjacent prose → non-zero
        // credit. MCC should be lower than positive.
        let src = "# Example\n\nThis shows how to print:\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\nThat prints `hi`.\n";
        let r = compute(src);
        assert!(r.credit_used > 0.0, "credit should apply");
        assert!(r.mcc < r.positive, "{} !< {}", r.mcc, r.positive);
        assert!(r.credit_used <= 0.25 * r.positive + 1e-9);
    }
}
