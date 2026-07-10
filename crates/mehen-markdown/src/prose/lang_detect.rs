// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Block-level language detection (§30).
//!
//! Tier 0 uses a zero-dependency Unicode-script block-ratio heuristic:
//!
//! ```text
//! kana = hiragana + katakana
//! cjk  = kana + han
//! latin = ascii_letter + fullwidth_latin_letter
//! total = non_whitespace_non_punct
//!
//! if kana / total >= 0.15                          -> ja
//! elif cjk / total >= 0.40 and kana == 0           -> other (Chinese)
//! elif latin / total >= 0.80                       -> en
//! else                                             -> other
//! ```
//!
//! Short blocks (< 15 visible chars) that classify as `Other` inherit the
//! enclosing heading's language — a stable deterministic fallback that keeps
//! short list items from fragmenting a document's classification.
//!
//! Code fences, link destinations, front-matter, HTML, MDX, math and tables
//! are tagged [`Language::None`] and excluded from prose analysis entirely.

use serde::Serialize;
use unicode_script::{Script, UnicodeScript};

use crate::kind::NodeKind;
use crate::syntax_tree::Node;

/// Sentinel character inserted where an `InlineCode` span was stripped.
///
/// Downstream stages (sentence splitter, word tokenizer, wording metrics)
/// see this as a single "object" placeholder. It survives sentence
/// splitting (it isn't a sentence terminator) and gets filtered out of
/// word tokenization (it isn't alphanumeric), so metric rates are
/// unchanged — but a sentence that originally contained `` `foo` `` can
/// still be detected as "had inline code" by checking for the sentinel.
///
/// U+FFFC OBJECT REPLACEMENT CHARACTER is the canonical Unicode marker
/// for a removed inline object and will never appear in real prose.
pub const INLINE_CODE_SENTINEL: char = '\u{FFFC}';

/// Per-block language tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// English (or other Latin-script prose).
    En,
    /// Japanese (any hiragana/katakana presence above threshold).
    Ja,
    /// Non-EN, non-JA — e.g. Chinese, Korean, Thai, Arabic, etc.
    Other,
    /// Mixed — aggregated at document level when both en and ja appear.
    Mixed,
    /// Not prose: code, front-matter, HTML, table, math, image target.
    None,
}

impl Language {
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::En => "en",
            Language::Ja => "ja",
            Language::Other => "other",
            Language::Mixed => "mixed",
            Language::None => "none",
        }
    }
}

/// One prose-eligible block extracted from the tree.
#[derive(Debug, Clone)]
pub(crate) struct ProseBlock<'a> {
    pub(crate) kind: NodeKind,
    pub start_line: u64,
    pub end_line: u64,
    /// Stripped prose text: inline code / URLs / alt-text destination already
    /// removed so script ratios aren't polluted by literal tokens.
    pub text: String,
    pub _raw: &'a [u8],
}

/// Like [`ProseBlock`] but carries a resolved language tag for downstream
/// metric dispatch.
#[derive(Debug, Clone)]
pub(crate) struct DetectedBlock {
    pub(crate) kind: NodeKind,
    pub start_line: u64,
    pub end_line: u64,
    pub text: String,
    pub language: Language,
}

/// Walks the parse tree and collects every prose-eligible block in document
/// order.
pub(crate) fn collect_prose_blocks<'a>(root: &Node<'_>, source: &'a [u8]) -> Vec<ProseBlock<'a>> {
    let mut blocks = Vec::new();
    walk(root, source, &mut blocks);
    blocks
}

fn walk<'a>(node: &Node<'_>, source: &'a [u8], blocks: &mut Vec<ProseBlock<'a>>) {
    let kind = node.kind();

    // Prose-carrying blocks we record. For list items we recurse so that
    // nested paragraphs / blockquotes / callouts are recorded individually
    // — matching §30.3's per-block tagging requirement.
    let is_prose_block = kind.is_heading()
        || matches!(
            kind,
            NodeKind::Paragraph
                | NodeKind::BlockQuote
                | NodeKind::Callout
                | NodeKind::ListItemContent { .. }
        );

    // Stop containers: never descend, never emit.
    let is_stop = matches!(
        kind,
        NodeKind::FencedCodeBlock
            | NodeKind::IndentedCodeBlock
            | NodeKind::HtmlBlock
            | NodeKind::MinusMetadata
            | NodeKind::PlusMetadata
            | NodeKind::MathBlock
            | NodeKind::PipeTable
            | NodeKind::LinkReferenceDefinition
            | NodeKind::ThematicBreak
    );

    if is_stop {
        return;
    }

    if is_prose_block {
        let start_line = (node.start_row() as u64) + 1;
        let (end_row, end_col) = node.end_position();
        let mut end_line = (end_row as u64) + 1;
        if end_col == 0 && end_line > start_line {
            end_line -= 1;
        }
        let is_container = matches!(kind, NodeKind::BlockQuote | NodeKind::Callout)
            || (matches!(kind, NodeKind::ListItemContent { .. }) && has_nested_prose_block(node));

        if is_container {
            // Containers (blockquote / callout) don't emit their own slice:
            // recurse into children so nested paragraphs are counted exactly
            // once. Emitting both the container text AND descending into
            // children would double-count every word / sentence inside.
            for child in node.children() {
                walk(&child, source, blocks);
            }
            return;
        }

        // Leaf prose block (paragraph / heading): emit its own slice and
        // don't recurse further — paragraphs / headings never nest more
        // prose blocks.
        let text = extract_prose_text(node, source);
        if !text.trim().is_empty() {
            blocks.push(ProseBlock {
                kind,
                start_line,
                end_line,
                text,
                _raw: source,
            });
        }
        return;
    }

    // Recurse into everything else (sections, lists, list items, documents).
    for child in node.children() {
        walk(&child, source, blocks);
    }
}

fn has_nested_prose_block(node: &Node<'_>) -> bool {
    for child in node.children() {
        let kind = child.kind();
        if kind.is_heading()
            || matches!(
                kind,
                NodeKind::Paragraph | NodeKind::BlockQuote | NodeKind::Callout
            )
        {
            return true;
        }
        if has_nested_prose_block(&child) {
            return true;
        }
    }
    false
}

/// Produces the clean prose text for a prose-block node.
///
/// Strategy: take the block's full byte slice from the source, then excise
/// every descendant sub-range that belongs to a skip class (inline code,
/// URLs, HTML inline, MDX inline, math inline, autolinks, front-matter,
/// pipe-table delimiters, heading markers).
///
/// Excised ranges are substituted with a single replacement character so
/// adjacent tokens never fuse:
///   - `InlineCode` spans leave behind [`INLINE_CODE_SENTINEL`] so
///     downstream wording metrics can detect that a sentence originally
///     carried an inline-code token (used to suppress weasel / hedge
///     noise around backtick-wrapped identifiers — §37.5 item 3).
///   - All other skip kinds collapse to a single space.
///
/// This byte-slice approach preserves the original whitespace between
/// tokens, which is critical for sentence- and word-segmentation.
pub(crate) fn extract_prose_text(node: &Node<'_>, source: &[u8]) -> String {
    let block_start = node.start_byte();
    let block_end = node.end_byte();
    if block_end <= block_start || block_end > source.len() {
        return String::new();
    }

    // Collect skip ranges relative to the source buffer. Each entry
    // carries whether the skip was an `InlineCode` (so we can emit a
    // sentinel) or another skip kind (emit a space).
    let mut skip_ranges: Vec<(usize, usize, SkipKind)> = Vec::new();
    collect_skip_ranges(node, &mut skip_ranges);

    // Sort + merge overlapping skip ranges so we can linearly excise them.
    // When two overlapping ranges disagree on kind, `InlineCode` wins so
    // the sentinel is still emitted.
    skip_ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize, SkipKind)> = Vec::with_capacity(skip_ranges.len());
    for (s, e, k) in skip_ranges {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
            if k == SkipKind::InlineCode {
                last.2 = SkipKind::InlineCode;
            }
        } else {
            merged.push((s, e, k));
        }
    }

    let mut out = String::new();
    let mut cursor = block_start;
    for (s, e, k) in merged {
        let s = s.max(block_start).min(block_end);
        let e = e.max(block_start).min(block_end);
        if cursor < s
            && let Ok(slice) = std::str::from_utf8(&source[cursor..s])
        {
            out.push_str(slice);
        }
        match k {
            SkipKind::InlineCode => out.push(INLINE_CODE_SENTINEL),
            SkipKind::Other => out.push(' '),
        }
        cursor = e.max(cursor);
    }
    if cursor < block_end
        && let Ok(slice) = std::str::from_utf8(&source[cursor..block_end])
    {
        out.push_str(slice);
    }

    normalize_whitespace(&out)
}

/// Internal tag on a skip range so the extractor knows whether to emit a
/// sentinel or just a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipKind {
    /// `InlineCode` span — emit [`INLINE_CODE_SENTINEL`] so wording metrics
    /// can still detect the original inline-code context.
    InlineCode,
    /// Every other skip kind (math, HTML, URLs, markers…) collapses to a
    /// single space.
    Other,
}

/// Walks the subtree at `node` and appends (start_byte, end_byte, kind)
/// entries for every descendant whose kind should be stripped from prose.
fn collect_skip_ranges(node: &Node<'_>, out: &mut Vec<(usize, usize, SkipKind)>) {
    let kind = node.kind();
    if is_skip_kind(&kind) {
        let sk = match kind {
            NodeKind::InlineCode | NodeKind::InlineCodeContent => SkipKind::InlineCode,
            _ => SkipKind::Other,
        };
        out.push((node.start_byte(), node.end_byte(), sk));
        return;
    }
    for child in node.children() {
        collect_skip_ranges(&child, out);
    }
}

fn is_skip_kind(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::InlineCode
            | NodeKind::CodeFenceContent
            | NodeKind::InlineCodeContent
            | NodeKind::MathInline
            | NodeKind::MathInlineContent
            | NodeKind::MathBlock
            | NodeKind::MathBlockContent
            | NodeKind::HtmlInline
            | NodeKind::HtmlBlock
            | NodeKind::HtmlOpenTag
            | NodeKind::HtmlCloseTag
            | NodeKind::HtmlComment
            | NodeKind::HtmlCdata
            | NodeKind::HtmlDeclaration
            | NodeKind::HtmlProcessingInstruction
            | NodeKind::Autolink
            | NodeKind::Uri
            | NodeKind::Email
            | NodeKind::LinkDestination
            | NodeKind::LinkTitle
            | NodeKind::MinusMetadata
            | NodeKind::PlusMetadata
            | NodeKind::PipeTableDelimiterRow
            | NodeKind::PipeTableDelimiterCell
            | NodeKind::HeadingMarker { .. }
            | NodeKind::BlockQuoteMarker
            | NodeKind::CalloutMarkerOpen
            | NodeKind::CalloutMarkerClose
            | NodeKind::CalloutType
            | NodeKind::ListMarker
            | NodeKind::TaskListMarkerChecked
            | NodeKind::TaskListMarkerUnchecked
    )
}

/// Collapses runs of whitespace and line breaks to a single space.
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

/// Applies the Unicode-script block-ratio heuristic to one prose block.
pub(crate) fn classify_block(block: &ProseBlock<'_>) -> DetectedBlock {
    let language = classify_text(&block.text);
    DetectedBlock {
        kind: block.kind,
        start_line: block.start_line,
        end_line: block.end_line,
        text: block.text.clone(),
        language,
    }
}

/// Classifies a text span using the Tier-0 rule from §30.1.
pub fn classify_text(text: &str) -> Language {
    let mut kana = 0usize;
    let mut han = 0usize;
    let mut ascii_letter = 0usize;
    let mut fullwidth_letter = 0usize;
    let mut total = 0usize;
    let mut visible_chars = 0usize;

    for c in text.chars() {
        if c.is_whitespace() || c == INLINE_CODE_SENTINEL {
            // Skip inline-code sentinels so substituting `InlineCode`
            // spans with U+FFFC doesn't skew the script ratios.
            continue;
        }
        visible_chars += 1;
        // Filter out punctuation and digits from the denominator. Digits are
        // technical tokens that don't signal language; punctuation is shared.
        // Fullwidth digits (`０`–`９`, U+FF10–U+FF19) must also be excluded
        // or they stay in `total` but never count as kana/han/latin, pushing
        // the ratios down and misclassifying Japanese headings with
        // fullwidth numerals as `other` (Codex P2 on PR #85).
        if c.is_ascii_punctuation()
            || is_cjk_punctuation(c)
            || c.is_ascii_digit()
            || ('\u{FF10}'..='\u{FF19}').contains(&c)
        {
            continue;
        }
        total += 1;

        // Classify.
        if is_hiragana(c) || is_katakana(c) {
            kana += 1;
        } else if is_han(c) {
            han += 1;
        } else if c.is_ascii_alphabetic() {
            ascii_letter += 1;
        } else if is_fullwidth_latin(c) {
            fullwidth_letter += 1;
        }
    }

    if total == 0 {
        // Block contained only punctuation / digits / whitespace. Very short.
        if visible_chars == 0 {
            return Language::None;
        }
        return Language::Other;
    }

    let cjk = kana + han;
    let latin = ascii_letter + fullwidth_letter;
    let t = total as f64;

    let kana_ratio = kana as f64 / t;
    let cjk_ratio = cjk as f64 / t;
    let latin_ratio = latin as f64 / t;

    if kana_ratio >= 0.15 {
        Language::Ja
    } else if cjk_ratio >= 0.40 && kana == 0 {
        // Likely Chinese (no kana); treat as Other for our metric pipelines.
        Language::Other
    } else if latin_ratio >= 0.80 {
        Language::En
    } else {
        Language::Other
    }
}

fn is_hiragana(c: char) -> bool {
    let u = c as u32;
    (0x3040..=0x309F).contains(&u) || (0x1B130..=0x1B16F).contains(&u)
}

fn is_katakana(c: char) -> bool {
    let u = c as u32;
    (0x30A0..=0x30FF).contains(&u)
        || (0x31F0..=0x31FF).contains(&u)
        || (0xFF65..=0xFF9F).contains(&u)
}

fn is_han(c: char) -> bool {
    // Use unicode-script for Han detection: covers CJK Unified, Ext A-G,
    // and compatibility blocks. Cheaper than enumerating explicitly.
    matches!(c.script(), Script::Han)
}

fn is_fullwidth_latin(c: char) -> bool {
    let u = c as u32;
    (0xFF21..=0xFF3A).contains(&u) || (0xFF41..=0xFF5A).contains(&u)
}

fn is_cjk_punctuation(c: char) -> bool {
    let u = c as u32;
    (0x3000..=0x303F).contains(&u)
        || (0xFF00..=0xFF0F).contains(&u)
        || (0xFF1A..=0xFF20).contains(&u)
        || (0xFF3B..=0xFF40).contains(&u)
        || (0xFF5B..=0xFF65).contains(&u)
}

/// Second pass: short blocks that came back `Other` inherit from the
/// surrounding context. Deterministic because the block list is in document
/// order.
///
/// Rules:
/// 1. Non-heading short blocks (< 15 visible chars) that classified as Other
///    inherit the preceding heading's language.
/// 2. Headings that classified as Other inherit the nearest non-`Other`
///    neighboring block's language (earlier preferred; else later).
pub(crate) fn propagate_heading_inheritance(blocks: Vec<DetectedBlock>) -> Vec<DetectedBlock> {
    let mut out = blocks;

    // Pass 1: non-heading short blocks inherit from preceding heading.
    inherit_short_blocks_from_headings(&mut out);

    // Pass 2: headings that came back `Other` inherit from nearest neighbor.
    // Kanji-only headings ("## 目的") are a common trigger.
    let langs: Vec<Language> = out.iter().map(|b| b.language).collect();
    for (i, block) in out.iter_mut().enumerate() {
        if !is_heading_kind(&block.kind) {
            continue;
        }
        if !matches!(block.language, Language::Other) {
            continue;
        }
        // Search forward and backward for the nearest non-Other, non-None
        // language. Prefer the next-neighboring paragraph because it
        // represents the section's body.
        let mut inh: Option<Language> = langs
            .iter()
            .skip(i + 1)
            .copied()
            .find(|l| matches!(l, Language::En | Language::Ja));
        if inh.is_none() {
            inh = langs
                .iter()
                .take(i)
                .rev()
                .copied()
                .find(|l| matches!(l, Language::En | Language::Ja));
        }
        if let Some(l) = inh {
            block.language = l;
        }
    }

    // Pass 3: re-apply short-block inheritance now that pass 2 resolved
    // kanji-only headings like `## 目的`. Without this pass, short body
    // blocks right after such a heading stayed `Other` because pass 1 had
    // no `last_heading_lang` yet (Codex P2 on PR #85).
    inherit_short_blocks_from_headings(&mut out);

    out
}

fn inherit_short_blocks_from_headings(blocks: &mut [DetectedBlock]) {
    let mut last_heading_lang: Option<Language> = None;
    for b in blocks.iter_mut() {
        if is_heading_kind(&b.kind) {
            if !matches!(b.language, Language::None | Language::Other) {
                last_heading_lang = Some(b.language);
            }
            continue;
        }
        let visible_len = b.text.chars().filter(|c| !c.is_whitespace()).count();
        if visible_len < 15
            && matches!(b.language, Language::Other)
            && let Some(inh) = last_heading_lang
        {
            b.language = inh;
        }
    }
}

fn is_heading_kind(kind: &NodeKind) -> bool {
    kind.is_heading()
}

/// Picks the document-level dominant language by simple majority over
/// detected blocks. Ties and mixed-bilingual documents return `Mixed`.
pub(crate) fn dominant_language(blocks: &[DetectedBlock]) -> Language {
    let mut en_blocks = 0usize;
    let mut ja_blocks = 0usize;
    let mut other_blocks = 0usize;

    for b in blocks {
        match b.language {
            Language::En => en_blocks += 1,
            Language::Ja => ja_blocks += 1,
            Language::Other => other_blocks += 1,
            _ => {}
        }
    }

    if en_blocks == 0 && ja_blocks == 0 {
        if other_blocks == 0 {
            // No prose at all.
            return Language::Other;
        }
        return Language::Other;
    }
    if en_blocks > 0 && ja_blocks > 0 {
        return Language::Mixed;
    }
    if en_blocks > 0 {
        Language::En
    } else {
        Language::Ja
    }
}

/// Concatenates the text of all blocks tagged with `language` into a single
/// string separated by `\n\n` so downstream sentence segmentation treats
/// block boundaries as hard terminators (§31.12).
pub(crate) fn concat_lang_text(blocks: &[DetectedBlock], language: Language) -> String {
    let mut out = String::new();
    for b in blocks {
        if b.language == language {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&b.text);
        }
    }
    out
}

// `Serialize` for Language is only used in BlockLangReport indirectly
// via `as_str()`. Declared here for completeness if the enum ever grows.
impl Serialize for Language {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_en() {
        let t = "This paragraph contains ten words of ordinary English prose.";
        assert_eq!(classify_text(t), Language::En);
    }

    #[test]
    fn classify_ja() {
        let t = "これは日本語のテキストです。読みやすい文章を書きましょう。";
        assert_eq!(classify_text(t), Language::Ja);
    }

    #[test]
    fn classify_chinese_as_other() {
        // No hiragana/katakana, all Han: treated as Other.
        let t = "这是一段中文文本没有任何假名字符存在";
        assert_eq!(classify_text(t), Language::Other);
    }

    #[test]
    fn classify_empty() {
        assert_eq!(classify_text(""), Language::None);
    }

    #[test]
    fn classify_bilingual_picks_ja_when_kana_present() {
        // Mixed, but kana ≥ 15 % → ja.
        let t = "設定 config file を open して編集します";
        assert_eq!(classify_text(t), Language::Ja);
    }

    fn parse_blocks(src: &str) -> Vec<ProseBlock<'static>> {
        // Leak the buffer so we can return borrowed `ProseBlock` values with
        // a 'static lifetime for this test-only helper.
        let bytes: &'static [u8] = Box::leak(src.as_bytes().to_vec().into_boxed_slice());
        let source = std::str::from_utf8(bytes).unwrap();
        let tree = crate::syntax_tree::parse(source);
        let root = tree.root();
        // SAFETY: the source buffer `bytes` outlives the returned Vec; tree
        // goes out of scope at function end but the collected blocks own
        // their extracted text and only borrow `_raw` which points at the
        // leaked buffer.
        let blocks: Vec<ProseBlock<'static>> =
            collect_prose_blocks(&root, bytes).into_iter().collect();
        std::mem::forget(tree);
        blocks
    }

    #[test]
    fn blockquote_with_paragraph_emits_one_block() {
        // Codex P1 regression: a blockquote containing a single paragraph
        // used to emit TWO prose blocks — once for the container's full
        // slice and again for the nested paragraph — which caused every
        // word and sentence inside quoted material to be counted twice.
        //
        // Fix: containers (blockquote / callout) recurse into children and
        // do NOT emit their own slice. Only the nested paragraph surfaces
        // as a prose block.
        let src = "> A quoted paragraph with exactly nine common English words.\n";
        let blocks = parse_blocks(src);
        let paragraph_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| matches!(b.kind, NodeKind::Paragraph))
            .collect();
        let container_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| matches!(b.kind, NodeKind::BlockQuote | NodeKind::Callout))
            .collect();
        assert_eq!(
            paragraph_blocks.len(),
            1,
            "expected exactly one paragraph block inside the blockquote, got {}: {:?}",
            paragraph_blocks.len(),
            blocks
                .iter()
                .map(|b| (b.kind, b.text.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            container_blocks.is_empty(),
            "container blocks (blockquote/callout) must not emit their own slice, got: {:?}",
            container_blocks
                .iter()
                .map(|b| (b.kind, b.text.clone()))
                .collect::<Vec<_>>()
        );
    }
}
