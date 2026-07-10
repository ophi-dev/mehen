// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Language-aware prose metric layer (§§29–38).
//!
//! This module adds a *separate* top-level `prose` section to the Markdown
//! output schema. It never modifies the structural scores computed by Phase A
//! (LOC, words, sections, ECU) or later phases (MCC, DMI, FillerLazyRisk).
//!
//! Entry point: [`analyze_prose`] takes the parsed tree + the source buffer
//! and produces a [`ProseReport`] that the analyzer attaches to
//! [`crate::types::MarkdownMetrics`].
//!
//! Tier 0 scope (§38):
//!   - Unicode-script block-ratio language detection (no trigram model).
//!   - UAX #29 word + sentence segmentation (English), with abbreviation list.
//!   - Vowel-group English syllables (no CMU, no hyphenation).
//!   - Classical English readability: FRES, FKGL, Fog, SMOG, ARI, CLI,
//!     Dale-Chall (NGSL-backed), FORCAST, LIX, RIX.
//!   - English wording: passive, hedges, weasels, wordy phrases, adverbs,
//!     nominalizations, expletives, lexical illusions, clichés, nonwords,
//!     long sentences, WQS.
//!   - Inclusive-language flags.
//!   - Japanese script composition, Tateishi simplified RS, Jōyō grade,
//!     jukugo density, politeness, JTF mechanical rules, textlint subset.
//!
//! Tier 1/2 features (CMU syllables, Lindera, Vibrato, JLPT, Lingua) are
//! feature-gated and OFF by default. See `Cargo.toml`.

pub mod english;
pub mod japanese;
pub mod lang_detect;

use serde::Serialize;

use crate::syntax_tree::Node;

use self::english::EnglishReport;
use self::japanese::JapaneseReport;
use self::lang_detect::{DetectedBlock, Language};

/// Whole-document prose output (§29.2).
#[derive(Debug, Clone, Serialize, Default)]
pub struct ProseReport {
    pub language_detection: LanguageDetection,
    pub english: Option<EnglishReport>,
    pub japanese: Option<JapaneseReport>,
    pub meta: ProseMeta,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LanguageDetection {
    pub dominant_language: String,
    pub blocks: Vec<BlockLangReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockLangReport {
    pub start_line: u64,
    pub end_line: u64,
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProseMeta {
    pub short_doc_warning: bool,
    pub words_counted: u64,
    pub sentences_counted: u64,
    /// The kinds of blocks stripped from prose input.
    pub blocks_stripped: Vec<String>,
}

/// Analyzes prose layers over the Markdown parse tree. `source` is the raw
/// file bytes so we can slice text without re-walking the tree repeatedly.
pub(crate) fn analyze_prose(root: &Node<'_>, source: &[u8]) -> ProseReport {
    // 1. Enumerate prose-eligible blocks with per-block text spans.
    let blocks = lang_detect::collect_prose_blocks(root, source);

    // 2. Per-block language tagging.
    let detected: Vec<DetectedBlock> = blocks
        .iter()
        .map(|b| lang_detect::classify_block(b))
        .collect();

    // Inherit parent-heading language for short blocks that classify as
    // Other. This is a second pass over `detected` because a block's
    // inheritance depends on the preceding heading context.
    let detected = lang_detect::propagate_heading_inheritance(detected);

    // 3. Document-level dominant language: majority among {en, ja}, falling
    //    back to `mixed` when both appear, `other` otherwise.
    let dominant = lang_detect::dominant_language(&detected);

    // 4. Run per-language pipelines on the concatenated text of their blocks.
    let en_text = lang_detect::concat_lang_text(&detected, Language::En);
    let ja_text = lang_detect::concat_lang_text(&detected, Language::Ja);

    let english = if !en_text.trim().is_empty() {
        Some(english::analyze(&en_text))
    } else {
        None
    };
    let japanese = if !ja_text.trim().is_empty() {
        Some(japanese::analyze(&ja_text))
    } else {
        None
    };

    // 5. Meta: word / sentence totals, short-doc warning, blocks-stripped.
    let (words_counted, sentences_counted) = match (english.as_ref(), japanese.as_ref()) {
        (Some(en), Some(ja)) => (
            en.lexical.words_total + ja.lexical.char_count,
            en.lexical.sentence_count + ja.lexical.sentence_count,
        ),
        (Some(en), None) => (en.lexical.words_total, en.lexical.sentence_count),
        (None, Some(ja)) => (ja.lexical.char_count, ja.lexical.sentence_count),
        (None, None) => (0, 0),
    };

    // Either language crossing the short-doc threshold propagates up to the
    // document-level warning. This matters for bilingual docs where the
    // English half can be very short (e.g. a README with a long Japanese
    // body and a tiny English summary): we must not hide the warning just
    // because the dominant language has enough prose.
    let short_doc_warning = match (english.as_ref(), japanese.as_ref()) {
        (Some(en), Some(ja)) => en.short_doc_warning || ja.short_doc_warning,
        (Some(en), None) => en.short_doc_warning,
        (None, Some(ja)) => ja.short_doc_warning,
        (None, None) => true,
    };

    let blocks_stripped = blocks_stripped_kinds(root);

    let blocks_out: Vec<BlockLangReport> = detected
        .iter()
        .filter_map(|b| {
            if matches!(b.language, Language::None) {
                None
            } else {
                Some(BlockLangReport {
                    start_line: b.start_line,
                    end_line: b.end_line,
                    language: b.language.as_str().to_string(),
                })
            }
        })
        .collect();

    ProseReport {
        language_detection: LanguageDetection {
            dominant_language: dominant.as_str().to_string(),
            blocks: blocks_out,
        },
        english,
        japanese,
        meta: ProseMeta {
            short_doc_warning,
            words_counted,
            sentences_counted,
            blocks_stripped,
        },
    }
}

/// Enumerates the kinds of blocks that were excluded from prose analysis.
/// Deterministic: always emitted in a fixed order so snapshots are stable.
fn blocks_stripped_kinds(root: &Node<'_>) -> Vec<String> {
    use crate::kind::NodeKind;

    let mut has_code = false;
    let mut has_frontmatter = false;
    let mut has_html = false;
    let mut has_math = false;
    let mut has_table = false;

    fn walk(
        node: &Node<'_>,
        code: &mut bool,
        fm: &mut bool,
        html: &mut bool,
        math: &mut bool,
        table: &mut bool,
    ) {
        let kind = node.kind();
        match kind {
            NodeKind::FencedCodeBlock | NodeKind::IndentedCodeBlock | NodeKind::InlineCode => {
                *code = true;
            }
            NodeKind::MinusMetadata | NodeKind::PlusMetadata => {
                *fm = true;
            }
            NodeKind::HtmlBlock | NodeKind::HtmlInline => {
                *html = true;
            }
            NodeKind::MathBlock | NodeKind::MathInline => {
                *math = true;
            }
            NodeKind::PipeTable => {
                *table = true;
            }
            _ => {}
        }
        for child in node.children() {
            walk(&child, code, fm, html, math, table);
        }
    }

    walk(
        root,
        &mut has_code,
        &mut has_frontmatter,
        &mut has_html,
        &mut has_math,
        &mut has_table,
    );

    // Note: pulldown-cmark never emits MDX nodes, so no "mdx" entry is
    // produced (the tree-sitter grammar had MDX kinds; this backend does not).
    let mut out = Vec::new();
    if has_code {
        out.push("code".to_string());
    }
    if has_frontmatter {
        out.push("frontmatter".to_string());
    }
    if has_html {
        out.push("html".to_string());
    }
    if has_math {
        out.push("math".to_string());
    }
    if has_table {
        out.push("table".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_src(src: &str) -> ProseReport {
        let tree = crate::syntax_tree::parse(src);
        let root = tree.root();
        analyze_prose(&root, src.as_bytes())
    }

    #[test]
    fn bilingual_short_english_triggers_warning() {
        // Codex P2 regression: when both English and Japanese prose are
        // present, `meta.short_doc_warning` was taken from the English branch
        // only. If English was long enough but Japanese was short (or vice
        // versa), the warning never propagated. The fix is a logical OR so
        // EITHER language hitting the short-doc threshold surfaces the flag.
        //
        // Fixture: ~3 English words (way below 100 / 5 sentences) + a long
        // Japanese body far beyond the 300-char / 5-sentence threshold.
        let src = "\
# Bilingual short-English doc

Hi there.

## 本文

\
これは日本語の長い本文です。最初の段落では、言語検出と短文警告の挙動を確認します。\
検出器はひらがなとカタカナの比率に基づいてブロック単位で言語を判定し、それぞれの言語に対して別々のパイプラインを実行します。\
短文警告はそれぞれの言語パイプラインが独立に判断するため、片方が短ければ文書全体で警告を出すべきです。

続く段落では、文書全体の評価について述べます。\
英語側の段落が短くても、日本語側に十分な量のテキストがあれば、メトリックは日本語に対して計算されます。\
ただし、短文警告は英語側の判断も反映しなければなりません。なぜなら、バイリンガル文書で片方の言語が不足している場合、\
読者はその情報を必要とするからです。従来のコードでは英語側の判断だけを採用していたため、日本語だけが短いケースでは警告が出ませんでした。

最後の段落として、修正後の挙動をまとめます。今後はどちらかの言語パイプラインが短文判定を返した時点で、\
文書全体の短文警告を真にします。この変更によって、バイリンガル文書の信頼性が向上し、\
片方の言語だけが不足しているケースを見逃すことがなくなります。テストはこの挙動を保証します。
";
        let report = analyze_src(src);

        // Sanity: both pipelines fired.
        assert!(
            report.english.is_some(),
            "expected English pipeline to fire, got {:?}",
            report.english
        );
        assert!(
            report.japanese.is_some(),
            "expected Japanese pipeline to fire, got {:?}",
            report.japanese
        );

        // English must have flagged short; Japanese must NOT have flagged.
        let en = report.english.as_ref().unwrap();
        let ja = report.japanese.as_ref().unwrap();
        assert!(
            en.short_doc_warning,
            "sanity: English half is short; en.short_doc_warning must be true"
        );
        assert!(
            !ja.short_doc_warning,
            "sanity: Japanese half is long; ja.short_doc_warning must be false, got {}",
            ja.short_doc_warning
        );

        // Regression: top-level meta must carry the English short flag
        // through even when Japanese is not short.
        assert!(
            report.meta.short_doc_warning,
            "bilingual doc with short English half must set meta.short_doc_warning=true"
        );
    }
}
