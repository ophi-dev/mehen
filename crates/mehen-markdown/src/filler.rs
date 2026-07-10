// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Filler / Lazy Structure Risk per §17.
//!
//! Each sub-score below matches the research-foundation formula exactly.
//! The aggregation (§17.9) is:
//!
//! ```text
//! FillerLazyRisk = clamp01(
//!     0.20 * UnanchoredProseMass
//!   + 0.15 * LowArtifactDensity
//!   + 0.20 * LowRepoGrounding
//!   + 0.15 * LazySectioning
//!   + 0.12 * RepetitionDensity
//!   + 0.12 * SpecificityScarcity
//!   + 0.04 * ReferenceHollowness
//!   + 0.02 * PlaceholderDensity
//! )
//! ```
//!
//! §17.11 diagnostic labels are emitted whenever a contributing sub-score is
//! non-trivial. The top-3 contributors (sorted by score desc) are surfaced
//! as `(label, score)` pairs for the exported schema's
//! `ai_era.top_contributors`.
//!
//! Phase D never touches Phase A/B/C metrics — it reads from them only.

use std::collections::BTreeSet;

use crate::grounding::GroundingOutputs;
use crate::kind::NodeKind;
use crate::mathops::{clamp01, sat};
use crate::section_balance::SectionBalance;
use crate::syntax_tree::Node;
use crate::tree_helpers::{ProseContext, is_non_prose_container, node_text, opens_prose_context};
use crate::types::{ArtifactRecord, LinkClass, LinkRecord, LocFamily, Section};

/// Diagnostic labels from §17.11.
pub(crate) mod labels {
    pub(super) const LARGE_UNANCHORED: &str = "large-unanchored-prose";
    pub(super) const LOW_REPO_GROUNDING: &str = "low-repository-grounding";
    pub(super) const LAZY_SECTIONING: &str = "lazy-sectioning";
    pub(super) const LOW_ARTIFACT_DENSITY: &str = "low-artifact-density";
    pub(super) const NEAR_DUP_PARAGRAPHS: &str = "near-duplicate-paragraphs";
    pub(super) const SPECIFICITY_SCARCITY: &str = "specificity-scarcity";
    pub(super) const HOLLOW_REFERENCES: &str = "hollow-references";
    pub(super) const PLACEHOLDER_HEAVY: &str = "placeholder-heavy";
}

/// Sub-scores + final FillerLazyRisk.
///
/// Sub-score fields marked `#[allow(dead_code)]` are populated for §17's
/// diagnostic surface (Phase F / `mehen diff`) but not read by the
/// analyzer's return value directly. Keeping them on the result makes the
/// internal plumbing visible for audit.
#[derive(Debug, Default, Clone)]
pub(crate) struct FillerResult {
    #[allow(dead_code)]
    pub(crate) unanchored_prose_mass: f64,
    #[allow(dead_code)]
    pub(crate) low_artifact_density: f64,
    #[allow(dead_code)]
    pub(crate) low_repo_grounding: f64,
    #[allow(dead_code)]
    pub(crate) lazy_sectioning: f64,
    #[allow(dead_code)]
    pub(crate) repetition_density: f64,
    #[allow(dead_code)]
    pub(crate) specificity_scarcity: f64,
    #[allow(dead_code)]
    pub(crate) reference_hollowness: f64,
    #[allow(dead_code)]
    pub(crate) placeholder_density: f64,
    pub(crate) filler_lazy_risk: f64,
    pub(crate) labels: Vec<String>,
    pub(crate) top_contributors: Vec<(String, f64)>,
    #[allow(dead_code)]
    pub(crate) near_duplicate_paragraph_rate: f64,
    #[allow(dead_code)]
    pub(crate) repeated_heading_rate: f64,
}

/// Main entry. Takes every Phase A/B/C output Phase D needs; never mutates
/// them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn analyze_filler(
    root: &Node<'_>,
    source: &str,
    words: u64,
    sections: &[Section],
    artifacts: &[ArtifactRecord],
    links: &[LinkRecord],
    loc: &LocFamily,
    grounding: &GroundingOutputs,
    section_balance: &SectionBalance,
) -> FillerResult {
    // §17 assumes substantive prose. A document with 0 words has nothing
    // to judge; returning a high filler risk would be a false positive on
    // placeholder / stub files. Emit a zero risk with no labels in that
    // case — every sub-score still computes zero individually because
    // every denominator max()'s to `1`.
    if words == 0 {
        return FillerResult::default();
    }

    // §17.1 UnanchoredProseMass
    let w = words.max(1) as f64;
    let unanchored_words = words.saturating_sub(grounding.anchored_words);
    let unanchored_prose_mass = sat(unanchored_words as f64 / w, 0.35, 0.85);

    // §17.2 LowArtifactDensity: A = total artifact count (per §4).
    let a = artifacts.len() as f64;
    let artifact_density = a / (w / 800.0).max(1.0);
    let low_artifact_density = 1.0 - sat(artifact_density, 0.5, 2.0);

    // §17.3 LowRepoGrounding = 1 - RepositoryGroundingScore.
    let low_repo_grounding = 1.0 - grounding.repository_grounding_score;

    // §17.4 LazySectioning.
    let lazy_sectioning = compute_lazy_sectioning(words, sections, section_balance);

    // §17.5 RepetitionDensity.
    let (near_duplicate_rate, repeated_heading_rate) =
        compute_repetition_signals(root, source, sections);
    let repetition_density = clamp01(
        0.75 * sat(near_duplicate_rate, 0.02, 0.20) + 0.25 * sat(repeated_heading_rate, 0.02, 0.15),
    );

    // §17.6 SpecificityScarcity.
    let specific_tokens = grounding.tokens.identifier_like_tokens
        + grounding.tokens.path_like_tokens
        + grounding.tokens.numeric_tokens
        + grounding.tokens.inline_code_tokens;
    let specificity_density = specific_tokens as f64 / w;
    let specificity_scarcity = 1.0 - sat(specificity_density, 0.03, 0.15);

    // §17.7 ReferenceHollowness.
    let reference_hollowness = compute_reference_hollowness(links);

    // §17.8 PlaceholderDensity.
    let placeholder_tokens = count_placeholder_tokens(root, source, links);
    let placeholder_density = sat(placeholder_tokens as f64 / (w / 1000.0).max(1.0), 0.5, 4.0);

    let raw = 0.20 * unanchored_prose_mass
        + 0.15 * low_artifact_density
        + 0.20 * low_repo_grounding
        + 0.15 * lazy_sectioning
        + 0.12 * repetition_density
        + 0.12 * specificity_scarcity
        + 0.04 * reference_hollowness
        + 0.02 * placeholder_density;
    let filler_lazy_risk = clamp01(raw);

    // §17.11 diagnostic labels: emit when sub-score > 0.40 or any
    // concrete signal hit (broken/duplicate paragraphs, placeholders).
    let mut labels_set: BTreeSet<String> = BTreeSet::new();
    if unanchored_prose_mass > 0.40 {
        labels_set.insert(labels::LARGE_UNANCHORED.to_string());
    }
    if low_repo_grounding > 0.40 {
        labels_set.insert(labels::LOW_REPO_GROUNDING.to_string());
    }
    if lazy_sectioning > 0.40 {
        labels_set.insert(labels::LAZY_SECTIONING.to_string());
    }
    if low_artifact_density > 0.40 {
        labels_set.insert(labels::LOW_ARTIFACT_DENSITY.to_string());
    }
    if near_duplicate_rate > 0.0 {
        labels_set.insert(labels::NEAR_DUP_PARAGRAPHS.to_string());
    }
    if specificity_scarcity > 0.40 {
        labels_set.insert(labels::SPECIFICITY_SCARCITY.to_string());
    }
    if reference_hollowness > 0.40 {
        labels_set.insert(labels::HOLLOW_REFERENCES.to_string());
    }
    if placeholder_tokens > 0 {
        labels_set.insert(labels::PLACEHOLDER_HEAVY.to_string());
    }
    let labels_vec: Vec<String> = labels_set.into_iter().collect();

    // Top-3 contributors by score desc, then label asc.
    let contributors = [
        (labels::LARGE_UNANCHORED.to_string(), unanchored_prose_mass),
        (
            labels::LOW_ARTIFACT_DENSITY.to_string(),
            low_artifact_density,
        ),
        (labels::LOW_REPO_GROUNDING.to_string(), low_repo_grounding),
        (labels::LAZY_SECTIONING.to_string(), lazy_sectioning),
        (labels::NEAR_DUP_PARAGRAPHS.to_string(), repetition_density),
        (
            labels::SPECIFICITY_SCARCITY.to_string(),
            specificity_scarcity,
        ),
        (labels::HOLLOW_REFERENCES.to_string(), reference_hollowness),
        (labels::PLACEHOLDER_HEAVY.to_string(), placeholder_density),
    ];
    let mut sorted: Vec<(String, f64)> = contributors.to_vec();
    sorted.sort_by(|a, b| match b.1.partial_cmp(&a.1) {
        Some(std::cmp::Ordering::Equal) | None => a.0.cmp(&b.0),
        Some(o) => o,
    });
    let top_contributors: Vec<(String, f64)> = sorted.into_iter().take(3).collect();

    let _ = loc; // reserved for a future §17.x extension; kept in signature.

    FillerResult {
        unanchored_prose_mass,
        low_artifact_density,
        low_repo_grounding,
        lazy_sectioning,
        repetition_density,
        specificity_scarcity,
        reference_hollowness,
        placeholder_density,
        filler_lazy_risk,
        labels: labels_vec,
        top_contributors,
        near_duplicate_paragraph_rate: near_duplicate_rate,
        repeated_heading_rate,
    }
}

fn compute_lazy_sectioning(
    words: u64,
    sections: &[Section],
    section_balance: &SectionBalance,
) -> f64 {
    let w = words as f64;
    let h = sections.len() as f64;
    let heading_density = h / (w / 700.0).max(1.0);
    let shallow = if section_balance.shallow_large_doc {
        1.0
    } else {
        0.0
    };
    clamp01(
        0.35 * (1.0 - sat(heading_density, 0.6, 2.0))
            + 0.35 * sat(section_balance.long_section_rate, 0.10, 0.60)
            + 0.30 * shallow,
    )
}

/// §17.5 repetition signals.
///
/// Paragraphs are collected in document order (sorted by `start_byte`) and
/// each is converted to a normalized 5-token shingle set using a
/// `BTreeSet<String>` so iteration is deterministic. Pairs with Jaccard
/// similarity > 0.82 are counted; each paragraph is counted at most once so
/// the rate stays in `[0, 1]`.
///
/// Repeated heading rate: duplicate normalized (lowercased, trimmed) heading
/// text occurrences / headings.
fn compute_repetition_signals(root: &Node<'_>, source: &str, sections: &[Section]) -> (f64, f64) {
    let mut paragraphs = collect_paragraphs(root, source);
    // Sort by start_byte so iteration order is deterministic even if the
    // walker produces them in a different order.
    paragraphs.sort_by_key(|p| p.start_byte);

    let shingles: Vec<BTreeSet<String>> = paragraphs
        .iter()
        .map(|p| paragraph_shingles(&p.text))
        .collect();

    let n = paragraphs.len();
    let mut is_near_dup: Vec<bool> = vec![false; n];
    for i in 0..n {
        if shingles[i].is_empty() {
            continue;
        }
        for j in (i + 1)..n {
            if shingles[j].is_empty() {
                continue;
            }
            let sim = jaccard(&shingles[i], &shingles[j]);
            if sim > 0.82 {
                is_near_dup[i] = true;
                is_near_dup[j] = true;
            }
        }
    }
    let near_dup_count = is_near_dup.iter().filter(|x| **x).count() as f64;
    let near_duplicate_rate = if n == 0 {
        0.0
    } else {
        near_dup_count / n as f64
    };

    let repeated_heading_rate = compute_repeated_heading_rate(sections);

    (near_duplicate_rate, repeated_heading_rate)
}

fn compute_repeated_heading_rate(sections: &[Section]) -> f64 {
    let total = sections.len() as f64;
    if total == 0.0 {
        return 0.0;
    }
    // Phase A `heading_text` is often `None` — the grammar extracts only
    // structural text. We fall back to the first source-line heading slug
    // via `start_line`: the sections list is derived directly from the AST
    // so sections with matching heading text share an identical
    // `(heading_level, heading_text)` key. When `heading_text` is `None`
    // we cannot measure repetition from this source, so this metric is
    // effectively zero in Phase D until a Phase-E heading-text extractor
    // lands. Until then: iterate all sections and count duplicate
    // normalized heading_text values.
    let mut seen: std::collections::BTreeMap<String, u64> = Default::default();
    let mut duplicates = 0u64;
    for s in sections {
        if let Some(text) = s.heading_text.as_ref() {
            let key = normalize_heading(text);
            let entry = seen.entry(key).or_insert(0);
            *entry += 1;
            if *entry > 1 {
                duplicates += 1;
            }
        }
    }
    duplicates as f64 / total
}

fn normalize_heading(s: &str) -> String {
    s.trim().to_lowercase()
}

#[derive(Debug, Clone)]
struct Paragraph {
    text: String,
    start_byte: usize,
}

fn collect_paragraphs(root: &Node<'_>, source: &str) -> Vec<Paragraph> {
    let mut out: Vec<Paragraph> = Vec::new();
    walk_paragraphs(root, source, &mut out);
    out
}

fn walk_paragraphs(node: &Node<'_>, source: &str, out: &mut Vec<Paragraph>) {
    let kind = node.kind();
    if matches!(kind, NodeKind::Paragraph) {
        let start = node.start_byte();
        let end = node.end_byte();
        let raw = source.as_bytes().get(start..end).unwrap_or(&[]);
        let text = String::from_utf8_lossy(raw).into_owned();
        out.push(Paragraph {
            text,
            start_byte: start,
        });
        // Don't descend — nested paragraphs are rare and the top-level
        // paragraph text is what we want for shingle matching.
        return;
    }
    for child in node.children() {
        walk_paragraphs(&child, source, out);
    }
}

/// Normalize to whitespace-separated ASCII-lowercase tokens, drop markdown
/// punctuation, then produce the set of 5-token shingles. Returns an empty
/// set when the paragraph has fewer than 5 tokens.
fn paragraph_shingles(text: &str) -> BTreeSet<String> {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() < 5 {
        return BTreeSet::new();
    }
    let mut set: BTreeSet<String> = BTreeSet::new();
    for window in tokens.windows(5) {
        set.insert(window.join(" "));
    }
    set
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

/// §17.7 ReferenceHollowness: bibliography entries + footnote definitions +
/// external citations vs. the subset with verifiable URLs. Phase D has no
/// link-check, so "verifiable" here means:
///
/// - External-family URLs with a valid host (classified as External /
///   ExternalVendor / Scholarly / IssuePr).
/// - Reference definitions with a non-empty destination matching a URL or
///   path pattern.
/// - Footnote definitions that have a matching reference (we can approximate
///   by treating footnote-definition links as verifiable).
fn compute_reference_hollowness(links: &[LinkRecord]) -> f64 {
    let mut total_refs: u64 = 0;
    let mut verifiable: u64 = 0;
    for l in links {
        match l.class {
            LinkClass::ReferenceDefinition => {
                total_refs += 1;
                if !l.destination.trim().is_empty() && looks_reference(&l.destination) {
                    verifiable += 1;
                }
            }
            LinkClass::Footnote => {
                total_refs += 1;
                // A footnote reference is "verifiable" if the destination
                // resolves to a definition (already reflected in `resolved`).
                if matches!(l.resolved, Some(true)) {
                    verifiable += 1;
                }
            }
            LinkClass::External | LinkClass::ExternalVendor | LinkClass::Scholarly => {
                total_refs += 1;
                // External URLs are not link-checked in Phase D; count them
                // as verifiable because the host/URL shape parses.
                verifiable += 1;
            }
            _ => {}
        }
    }
    if total_refs == 0 {
        return 0.0;
    }
    1.0 - (verifiable as f64 / total_refs as f64)
}

fn looks_reference(s: &str) -> bool {
    s.contains("://") || s.starts_with('/') || s.contains('.') || s.contains('#')
}

/// §17.8 placeholder tokens: TODO / TBD / FIXME / XXX / lorem / placeholder,
/// plus empty links / empty images.
fn count_placeholder_tokens(root: &Node<'_>, source: &str, links: &[LinkRecord]) -> u64 {
    let mut count = count_placeholder_words(root, source);
    for l in links {
        let dest = l.destination.trim();
        let dest_lower = dest.to_lowercase();
        if dest.is_empty()
            || dest_lower == "tbd"
            || dest_lower == "todo"
            || dest_lower == "#"
            || dest_lower == "placeholder"
        {
            count += 1;
        }
    }
    count
}

fn count_placeholder_words(root: &Node<'_>, source: &str) -> u64 {
    let mut total = 0u64;
    walk_placeholder_words(root, source, &mut total, false);
    total
}

fn walk_placeholder_words(node: &Node<'_>, source: &str, total: &mut u64, inside_prose: bool) {
    use NodeKind::*;
    let kind = node.kind();
    if is_non_prose_container(kind) {
        return;
    }
    let next_inside = inside_prose || opens_prose_context(kind, ProseContext::PLACEHOLDER_TEXT);

    if next_inside
        && matches!(kind, WordToken | IdentifierLikeToken)
        && is_placeholder(node_text(node, source).trim())
    {
        *total += 1;
    }

    for child in node.children() {
        walk_placeholder_words(&child, source, total, next_inside);
    }
}

fn is_placeholder(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let upper = token.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "TODO" | "TBD" | "FIXME" | "XXX" | "LOREM" | "PLACEHOLDER"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_shingles_produce_five_token_windows() {
        let s = paragraph_shingles("one two three four five six");
        assert_eq!(s.len(), 2);
        assert!(s.contains("one two three four five"));
        assert!(s.contains("two three four five six"));
    }

    #[test]
    fn paragraph_shingles_under_five_tokens_is_empty() {
        let s = paragraph_shingles("alpha beta gamma delta");
        assert!(s.is_empty());
    }

    #[test]
    fn jaccard_of_identical_sets_is_one() {
        let a: BTreeSet<String> = ["x", "y", "z"].iter().map(|s| s.to_string()).collect();
        let b: BTreeSet<String> = ["x", "y", "z"].iter().map(|s| s.to_string()).collect();
        assert_eq!(jaccard(&a, &b), 1.0);
    }

    #[test]
    fn jaccard_of_disjoint_sets_is_zero() {
        let a: BTreeSet<String> = ["x"].iter().map(|s| s.to_string()).collect();
        let b: BTreeSet<String> = ["y"].iter().map(|s| s.to_string()).collect();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn placeholder_detection_is_case_insensitive() {
        assert!(is_placeholder("TODO"));
        assert!(is_placeholder("tbd"));
        assert!(is_placeholder("FIXME"));
        assert!(is_placeholder("lorem"));
        assert!(!is_placeholder("todo_list"));
        assert!(!is_placeholder("fixture"));
    }
}
