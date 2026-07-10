// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Markdown Halstead metrics per §9.
//!
//! Walks the AST once and classifies each leaf / inline / block node as
//! operator or operand following §§9.1-9.2, using the Pulldown-backed Markdown
//! syntax node kinds. Operators are identified by kind (so all `##` H2
//! markers share one operator class); operands are identified by their byte
//! text (so two occurrences of the same word count once in n2 but twice in
//! N2, matching classical Halstead).
//!
//! §9.3 formulas:
//!
//! ```text
//! vocab   = n1 + n2
//! length  = N1 + N2
//! volume  = length * log2(max(2, vocab))
//! diff    = (n1 / 2) * (N2 / max(1, n2))
//! effort  = volume * diff
//! ```
//!
//! §9.4 embedded-code adjustment happens outside this module in
//! `embedded_code.rs` and is composed into the final `Halstead` record by
//! the analyzer.

use std::collections::BTreeMap;

use crate::document::MarkdownDocument;
use crate::kind::{HeadingStyle, NodeKind};
use crate::syntax_tree::Node;
use crate::types::Halstead;

/// Distinct operator classes (rich enough that MCC and Halstead use the same
/// shape). Each variant represents one row in §9.1.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OperatorKind {
    HeadingMarkerH1,
    HeadingMarkerH2,
    HeadingMarkerH3,
    HeadingMarkerH4,
    HeadingMarkerH5,
    HeadingMarkerH6,
    SetextMarkerH1,
    SetextMarkerH2,
    ListMarker,
    TaskMarkerChecked,
    TaskMarkerUnchecked,
    TableDelim,
    TableAlignLeft,
    TableAlignRight,
    LinkOp,
    ImageOp,
    /// Code fence by language tag (empty for unlabelled). Stored
    /// separately as a string so each tag is its own operator class.
    FenceTag(String),
    InlineCodeOp,
    BlockquoteMarker,
    CalloutOp,
    MathDelimiter,
    RawHtmlOp,
    /// Terminator (`. ? ! 。 …`).
    Terminator,
    /// Separator (`, ; :`).
    Separator,
    /// Bracket (`() [] {} <>`).
    Bracket,
    /// Operator-like (`= + - * / | & :: -> =>`).
    OperatorLike,
}

/// Maps a heading marker's level and style to its operator class.
///
/// Each ATX level (`#`..`######`) is a distinct operator, as is each setext
/// underline level; this mirrors the pre-refactor per-marker operator table.
/// Setext is only defined for levels 1 and 2 (see `resolve_heading`), so
/// higher setext levels fall back to the ATX class for that level.
fn heading_marker_op(level: u8, style: HeadingStyle) -> OperatorKind {
    match (style, level) {
        (HeadingStyle::Setext, 1) => OperatorKind::SetextMarkerH1,
        (HeadingStyle::Setext, 2) => OperatorKind::SetextMarkerH2,
        (_, 1) => OperatorKind::HeadingMarkerH1,
        (_, 2) => OperatorKind::HeadingMarkerH2,
        (_, 3) => OperatorKind::HeadingMarkerH3,
        (_, 4) => OperatorKind::HeadingMarkerH4,
        (_, 5) => OperatorKind::HeadingMarkerH5,
        _ => OperatorKind::HeadingMarkerH6,
    }
}

/// Core public entry point. Walks the AST and emits the §9.3 derived values.
pub(crate) fn compute_halstead(
    root: &Node<'_>,
    document: &MarkdownDocument,
    source: &str,
) -> Halstead {
    let mut operator_counts: BTreeMap<OperatorKind, u64> = BTreeMap::new();
    let mut operand_counts: BTreeMap<String, u64> = BTreeMap::new();

    let mut state = Ctx {
        operator_counts: &mut operator_counts,
        operand_counts: &mut operand_counts,
        source,
        document,
    };
    state.walk(root);

    // §9.3.
    let n1 = operator_counts.len() as u64;
    let big_n1: u64 = operator_counts.values().sum();
    let n2 = operand_counts.len() as u64;
    let big_n2: u64 = operand_counts.values().sum();

    let vocabulary = n1 + n2;
    let length = big_n1 + big_n2;
    let vocab_f = vocabulary.max(2) as f64;
    let volume = length as f64 * vocab_f.log2();
    let difficulty = if n2 > 0 {
        (n1 as f64 / 2.0) * (big_n2 as f64 / n2 as f64)
    } else {
        0.0
    };
    let effort = volume * difficulty;

    Halstead {
        operators_distinct: n1,
        operators_total: big_n1,
        operands_distinct: n2,
        operands_total: big_n2,
        vocabulary,
        length,
        volume,
        difficulty,
        effort,
        // embedded_volume + total_volume are filled in by the analyzer after
        // embedded-code dispatch.
        embedded_volume: 0.0,
        total_volume: volume,
    }
}

struct Ctx<'counts, 'source, 'doc> {
    operator_counts: &'counts mut BTreeMap<OperatorKind, u64>,
    operand_counts: &'counts mut BTreeMap<String, u64>,
    source: &'source str,
    document: &'doc MarkdownDocument,
}

impl Ctx<'_, '_, '_> {
    fn bump_op(&mut self, k: OperatorKind) {
        *self.operator_counts.entry(k).or_insert(0) += 1;
    }

    fn bump_operand(&mut self, key: String) {
        if key.is_empty() {
            return;
        }
        *self.operand_counts.entry(key).or_insert(0) += 1;
    }

    fn walk(&mut self, node: &Node<'_>) {
        use NodeKind::*;

        let kind = node.kind();

        // Stop containers for operands that are URL / code text — still
        // classify them as operators at the wrapper level.
        let mut descend = true;

        match kind {
            // Heading markers — one operator per level/style. `#` and `##`
            // are distinct operators; setext underlines have their own class.
            HeadingMarker { level, style } => {
                self.bump_op(heading_marker_op(level, style));
            }

            // List markers — every bullet/number style is one operator class.
            ListMarker => self.bump_op(OperatorKind::ListMarker),

            // Task list markers.
            TaskListMarkerChecked => self.bump_op(OperatorKind::TaskMarkerChecked),
            TaskListMarkerUnchecked => self.bump_op(OperatorKind::TaskMarkerUnchecked),

            // Table operators.
            PipeTableDelimiterRow | PipeTableDelimiterCell => {
                self.bump_op(OperatorKind::TableDelim)
            }
            PipeTableAlignLeft => self.bump_op(OperatorKind::TableAlignLeft),
            PipeTableAlignRight => self.bump_op(OperatorKind::TableAlignRight),

            // Link / image wrappers — each whole `Link` is one operator
            // occurrence for `[…](…)`. Inner LinkLabel prose descends as
            // operand text; destination operands come from pulldown-resolved
            // document facts so reference-style links use the definition
            // target instead of the local reference key.
            Link => {
                self.bump_op(OperatorKind::LinkOp);
                if let Some(dest) = self
                    .document
                    .link_destination_by_span(node.start_byte(), node.end_byte())
                {
                    self.bump_operand(dest.to_string());
                }
            }
            Image => {
                self.bump_op(OperatorKind::ImageOp);
                if let Some(dest) = self
                    .document
                    .link_destination_by_span(node.start_byte(), node.end_byte())
                {
                    self.bump_operand(dest.to_string());
                }
            }

            // Code fences: record the language tag as the operator's
            // discriminator so e.g. `rust` and `python` are distinct
            // operators.
            FencedCodeBlock | IndentedCodeBlock => {
                if let Some(block) = self.document.code_block_by_start_row(node.start_row()) {
                    let tag = block.language.clone().unwrap_or_default();
                    self.bump_op(OperatorKind::FenceTag(tag));
                    // Embedded content is a single opaque operand so §9.4's
                    // embedded-code scaling can own language-specific detail.
                    let prefix = if block.is_fenced() {
                        "code"
                    } else {
                        "indent_code"
                    };
                    self.bump_operand(format!("{prefix}:{}", sha_hex(block.content.as_bytes())));
                }
                descend = false;
            }

            InlineCode => {
                self.bump_op(OperatorKind::InlineCodeOp);
                // The inline-code content is opaque; hash it so two identical
                // `` `foo` `` references count as one operand.
                if let Some(text) = inline_code_text(node, self.source) {
                    self.bump_operand(format!("inline:{}", sha_hex(text.as_bytes())));
                }
                descend = false;
            }

            // Blockquote and callout markers.
            BlockQuoteMarker => self.bump_op(OperatorKind::BlockquoteMarker),
            CalloutMarkerOpen | CalloutMarkerClose | CalloutType => {
                self.bump_op(OperatorKind::CalloutOp)
            }

            // Math delimiters. Only the block delimiter is emitted by the
            // pulldown-backed builder; inline math carries no delimiter node.
            MathBlockDelimiter => self.bump_op(OperatorKind::MathDelimiter),

            // Raw HTML operators.
            HtmlOpenTag
            | HtmlCloseTag
            | HtmlComment
            | HtmlCdata
            | HtmlDeclaration
            | HtmlProcessingInstruction => self.bump_op(OperatorKind::RawHtmlOp),

            // Punctuation classes per §3.3.
            Terminator => self.bump_op(OperatorKind::Terminator),
            Separator => self.bump_op(OperatorKind::Separator),
            Bracket => self.bump_op(OperatorKind::Bracket),
            OperatorLike => self.bump_op(OperatorKind::OperatorLike),

            // Operand leaves.
            WordToken => {
                self.push_text_operand(node);
            }
            NumericToken => {
                self.push_text_operand(node);
            }
            IdentifierLikeToken => {
                self.push_text_operand(node);
            }
            PathLikeToken => {
                self.push_text_operand(node);
            }

            // Link definitions are plumbing for reference-style links. The
            // rendered link use owns the semantic destination operand.
            LinkReferenceDefinition => {
                descend = false;
            }

            // Link destinations are handled at the `Link` / `Image` wrapper
            // so reference-style links can resolve semantically. Autolink
            // URIs have no wrapper destination, so keep counting those here.
            LinkDestination => {
                descend = false;
            }
            Uri => {
                self.push_text_operand(node);
                descend = false;
            }

            // Table headers (header row cells) — count each cell's text as
            // an operand in addition to any word-like tokens inside it.
            // §9.2 lists "table headers" as a distinct operand class.
            PipeTableHeader => {
                for cell in node.children() {
                    if matches!(cell.kind(), NodeKind::PipeTableCell) {
                        let text = node_text(&cell, self.source).trim().to_string();
                        if !text.is_empty() {
                            self.bump_operand(format!("th:{}", text));
                        }
                    }
                }
                // Fall through: descend so each word token in the header
                // row also contributes as a regular word operand.
            }

            _ => {}
        }

        if !descend {
            return;
        }

        for child in node.children() {
            self.walk(&child);
        }
    }

    fn push_text_operand(&mut self, node: &Node<'_>) {
        let bytes = self.source.as_bytes();
        let start = node.start_byte();
        let end = node.end_byte();
        if end <= bytes.len() && start < end {
            let text = std::str::from_utf8(&bytes[start..end])
                .unwrap_or("")
                .trim()
                .to_string();
            if !text.is_empty() {
                self.bump_operand(text);
            }
        }
    }
}

fn inline_code_text(node: &Node<'_>, source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let start = node.start_byte();
    let end = node.end_byte();
    if end <= bytes.len() && start < end {
        return std::str::from_utf8(&bytes[start..end])
            .ok()
            .map(|s| s.trim().trim_matches('`').to_string());
    }
    None
}

fn node_text(node: &Node<'_>, source: &str) -> String {
    let bytes = source.as_bytes();
    let start = node.start_byte();
    let end = node.end_byte();
    if end <= bytes.len() && start < end {
        std::str::from_utf8(&bytes[start..end])
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    }
}

/// Cheap deterministic hash → lowercase hex. We only need stable
/// equivalence, so the FNV-1a 64-bit variant is plenty.
fn sha_hex(bytes: &[u8]) -> String {
    // FNV-1a 64-bit.
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn compute(src: &str) -> Halstead {
        let (tree, document) = crate::syntax_tree::parse_with_document(src);
        compute_halstead(&tree.root(), &document, src)
    }

    fn operand_counts(src: &str) -> BTreeMap<String, u64> {
        let (tree, document) = crate::syntax_tree::parse_with_document(src);
        let mut operator_counts = BTreeMap::new();
        let mut operand_counts = BTreeMap::new();
        let mut state = Ctx {
            operator_counts: &mut operator_counts,
            operand_counts: &mut operand_counts,
            source: src,
            document: &document,
        };
        state.walk(&tree.root());
        operand_counts
    }

    fn assert_halstead_close(a: &Halstead, b: &Halstead) {
        assert_eq!(a.operators_distinct, b.operators_distinct);
        assert_eq!(a.operators_total, b.operators_total);
        assert_eq!(a.operands_distinct, b.operands_distinct);
        assert_eq!(a.operands_total, b.operands_total);
        assert_eq!(a.vocabulary, b.vocabulary);
        assert_eq!(a.length, b.length);
        assert!((a.volume - b.volume).abs() < 1e-9, "{a:?} != {b:?}");
        assert!((a.difficulty - b.difficulty).abs() < 1e-9, "{a:?} != {b:?}");
        assert!((a.effort - b.effort).abs() < 1e-9, "{a:?} != {b:?}");
        assert!(
            (a.total_volume - b.total_volume).abs() < 1e-9,
            "{a:?} != {b:?}"
        );
    }

    #[test]
    fn empty_halstead_is_zero() {
        let h = compute("");
        assert_eq!(h.operators_distinct, 0);
        assert_eq!(h.operators_total, 0);
        assert_eq!(h.operands_distinct, 0);
        assert_eq!(h.operands_total, 0);
        assert_eq!(h.vocabulary, 0);
        assert_eq!(h.length, 0);
        assert_eq!(h.volume, 0.0);
        assert_eq!(h.total_volume, 0.0);
    }

    #[test]
    fn heading_plus_prose_counts() {
        let src = "# Hello world\n";
        let h = compute(src);
        // Operators: 1 H1 marker → n1=1, N1=1.
        assert_eq!(h.operators_distinct, 1);
        assert_eq!(h.operators_total, 1);
        // Operands: `Hello`, `world` → n2=2, N2=2.
        assert_eq!(h.operands_distinct, 2);
        assert_eq!(h.operands_total, 2);
        assert_eq!(h.vocabulary, 3);
        assert_eq!(h.length, 3);
        // Volume = 3 * log2(3) ≈ 4.754887.
        assert!((h.volume - 3.0 * (3.0_f64).log2()).abs() < 1e-6);
    }

    #[test]
    fn link_counts_as_operator_and_url_as_operand() {
        let src = "# H\n\nSee [here](https://example.com).\n";
        let h = compute(src);
        // Operators include: one H1 marker, one Link, one Terminator (`.`).
        assert!(h.operators_distinct >= 3);
        // The URL is one operand; `See` and `here` are word operands.
        assert!(h.operands_distinct >= 3);
    }

    #[test]
    fn reference_style_link_destinations_match_inline_halstead() {
        let inline = "# H\n\nSee [docs](https://example.com).\n";
        let cases = [
            "# H\n\nSee [docs][api].\n\n[api]: https://example.com\n",
            "# H\n\nSee [docs][].\n\n[docs]: https://example.com\n",
            "# H\n\nSee [docs].\n\n[docs]: https://example.com\n",
            "# H\n\nSee [docs][api\\]].\n\n[api\\]]: https://example.com\n",
        ];

        let expected = compute(inline);
        for reference in cases {
            let actual = compute(reference);
            assert_halstead_close(&expected, &actual);
        }

        let full_reference_operands = operand_counts(cases[0]);
        assert_eq!(full_reference_operands.get("https://example.com"), Some(&1));
        assert!(
            !full_reference_operands.contains_key("api"),
            "reference key leaked into Halstead operands: {full_reference_operands:?}"
        );
    }

    #[test]
    fn reference_style_image_destinations_match_inline_halstead() {
        let inline = "# H\n\n![diagram](https://example.com/diagram.png)\n";
        let reference = "# H\n\n![diagram][asset]\n\n[asset]: https://example.com/diagram.png\n";

        assert_halstead_close(&compute(inline), &compute(reference));

        let operands = operand_counts(reference);
        assert_eq!(operands.get("https://example.com/diagram.png"), Some(&1));
        assert!(
            !operands.contains_key("asset"),
            "image reference key leaked into Halstead operands: {operands:?}"
        );
    }
}
