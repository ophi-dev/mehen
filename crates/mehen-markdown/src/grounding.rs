// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Repository Grounding Score (§15) and Evidence Coverage Score (§16).
//!
//! Grounding captures how strongly a Markdown document ties back to concrete
//! repository reality (files, commands, identifiers, versions, issues).
//! Evidence coverage asks whether every section carries at least some
//! evidence anchor. Both are Phase-D metrics and never modify Phase A/B/C/E
//! outputs — they only *read* from them.
//!
//! §15.2 formula:
//!
//! ```text
//! RepositoryGroundingScore = clamp01(
//!     0.25 * sat(repo_link_density; 0.5, 4.0)
//!   + 0.25 * path_resolution_rate
//!   + 0.20 * sat(code_example_density; 0.5, 3.0)
//!   + 0.15 * sat(identifier_density; 0.02, 0.12)
//!   + 0.15 * sat(version_fact_density; 0.01, 0.08)
//! )
//! ```
//!
//! §16.3 formula: `0.5 * mean(section_evidence) + 0.5 * p25(section_evidence)`.

use std::path::Path;

use crate::kind::NodeKind;
use crate::mathops::{clamp01, sat};
use crate::syntax_tree::Node;
use crate::tree_helpers::{ProseContext, is_non_prose_container, node_text, opens_prose_context};
use crate::types::{ArtifactKind, ArtifactRecord, LinkClass, LinkRecord, Section, TableRecord};

/// Inputs collected from the AST walk that feed both §15 and §17.6 /
/// §17.2 specificity / artifact-density computations.
#[derive(Debug, Default, Clone)]
pub(crate) struct GroundingTokenCounts {
    /// `identifier_like_token` occurrences inside prose contexts.
    pub(crate) identifier_like_tokens: u64,
    /// `path_like_token` occurrences inside prose contexts.
    pub(crate) path_like_tokens: u64,
    /// `path_like_token` occurrences that resolve to a repo file/dir.
    #[allow(dead_code)]
    pub(crate) resolved_path_like_tokens: u64,
    /// Numeric tokens matching `^v?\d+\.\d+(\.\d+)?$`. Subset of
    /// `numeric_tokens`.
    #[allow(dead_code)]
    pub(crate) version_tokens: u64,
    /// All `numeric_token` occurrences inside prose.
    pub(crate) numeric_tokens: u64,
    /// Raw inline code tokens (inline_code nodes).
    pub(crate) inline_code_tokens: u64,
}

/// Final §15 output plus per-section anchor counts used for §16 / §17.
///
/// Several fields here are populated for auditability and Phase F's
/// `mehen diff` sticky comment, and are not read by the analyzer itself;
/// they are annotated with `#[allow(dead_code)]` rather than dropped so the
/// intermediate surface stays traceable. The analyzer only reads
/// `repository_grounding_score`, `evidence_coverage_score`, `anchored_words`,
/// and `tokens`.
#[derive(Debug, Clone)]
pub(crate) struct GroundingOutputs {
    pub(crate) repository_grounding_score: f64,
    pub(crate) evidence_coverage_score: f64,
    /// Per-section evidence anchor counts, indexed by `section_id`.
    /// Sections at index 0 → section_id 0, etc. Length matches `sections.len()`.
    #[allow(dead_code)]
    pub(crate) per_section_anchors: Vec<u64>,
    /// Normalized per-section evidence score per §16.2 (saturated
    /// `anchor_density_s`). Indexed by `section_id`.
    #[allow(dead_code)]
    pub(crate) per_section_evidence: Vec<f64>,
    /// Words in sections that have at least one evidence anchor.
    pub(crate) anchored_words: u64,
    /// Copy-back of the §15 intermediate token counts so Phase D
    /// filler/specificity modules don't walk the tree again.
    pub(crate) tokens: GroundingTokenCounts,
    /// Labelled code fences (info_string non-empty). Re-exported so §15
    /// code_example_density and §19 artifact debt don't recount.
    #[allow(dead_code)]
    pub(crate) labelled_code_fences: u64,
    /// Command-shell fences (`bash`, `sh`, `shell`, `zsh`). Subset of
    /// `labelled_code_fences`.
    #[allow(dead_code)]
    pub(crate) command_blocks: u64,
    /// Path-like tokens containing at least one `.` (heuristic for
    /// package/API/config identifiers per §15).
    #[allow(dead_code)]
    pub(crate) package_api_config_tokens: u64,
    /// Resolved relative links.
    #[allow(dead_code)]
    pub(crate) resolved_relative_links: u64,
    /// Resolved internal anchors.
    #[allow(dead_code)]
    pub(crate) resolved_internal_anchors: u64,
    /// Issue/PR references.
    #[allow(dead_code)]
    pub(crate) issue_pr_refs: u64,
}

/// Top-level Phase-D entry point for grounding + evidence.
#[allow(clippy::too_many_arguments)]
pub(crate) fn analyze_grounding(
    root: &Node<'_>,
    source: &str,
    file_path: &Path,
    words: u64,
    sections: &[Section],
    links: &[LinkRecord],
    artifacts: &[ArtifactRecord],
    tables: &[TableRecord],
) -> GroundingOutputs {
    let tokens = collect_token_counts(root, source, file_path);
    let labelled_code_fences = artifacts
        .iter()
        .filter(|a| a.kind == ArtifactKind::Code && a.has_label)
        .count() as u64;
    let command_blocks = artifacts
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::Code
                && a.language_tag
                    .as_deref()
                    .map(is_command_shell)
                    .unwrap_or(false)
        })
        .count() as u64;
    let package_api_config_tokens = collect_package_api_config_tokens(root, source, file_path);

    let resolved_relative_links = links
        .iter()
        .filter(|l| l.class == LinkClass::Relative && matches!(l.resolved, Some(true)))
        .count() as u64;
    let resolved_internal_anchors = links
        .iter()
        .filter(|l| l.class == LinkClass::Internal && matches!(l.resolved, Some(true)))
        .count() as u64;
    let issue_pr_refs = links
        .iter()
        .filter(|l| l.class == LinkClass::IssuePr)
        .count() as u64;

    // §15.2 densities.
    let w = words as f64;
    let repo_link_density = resolved_relative_links as f64 / (w / 500.0).max(1.0);
    let path_resolution_rate = if tokens.path_like_tokens == 0 {
        // §15.2: max(1, path_like_tokens). No paths → score 0 for this term.
        0.0
    } else {
        tokens.resolved_path_like_tokens as f64 / tokens.path_like_tokens as f64
    };
    let code_example_density = labelled_code_fences as f64 / (w / 800.0).max(1.0);
    let identifier_density = tokens.identifier_like_tokens as f64 / w.max(1.0);
    let version_fact_density = tokens.version_tokens as f64 / w.max(1.0);

    let repository_grounding_score = clamp01(
        0.25 * sat(repo_link_density, 0.5, 4.0)
            + 0.25 * path_resolution_rate.clamp(0.0, 1.0)
            + 0.20 * sat(code_example_density, 0.5, 3.0)
            + 0.15 * sat(identifier_density, 0.02, 0.12)
            + 0.15 * sat(version_fact_density, 0.01, 0.08),
    );

    // §16: per-section evidence anchors.
    //
    // Walk the tree once more to attribute each resolved `path_like_token`
    // to its enclosing section so §16.1 "path-like token resolved to
    // repository" actually shows up in per-section anchor density.
    let base_dir = file_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let per_section_resolved_paths =
        collect_per_section_resolved_paths(root, source, &base_dir, sections);
    let per_section_anchors = compute_per_section_anchors(
        sections,
        links,
        artifacts,
        tables,
        &per_section_resolved_paths,
    );

    let mut per_section_evidence: Vec<f64> = Vec::with_capacity(sections.len());
    let mut anchored_words: u64 = 0;
    for (i, section) in sections.iter().enumerate() {
        let ws = section.word_count as f64;
        let anchor_density = per_section_anchors[i] as f64 / (ws / 250.0).max(1.0);
        let sec_ev = sat(anchor_density, 0.2, 1.5);
        per_section_evidence.push(sec_ev);
        if per_section_anchors[i] > 0 {
            anchored_words += section.word_count;
        }
    }

    // §16.3 aggregate: 0.5 * mean + 0.5 * p25.
    let evidence_coverage_score = if per_section_evidence.is_empty() {
        0.0
    } else {
        let mean: f64 =
            per_section_evidence.iter().sum::<f64>() / per_section_evidence.len() as f64;
        let p25 = percentile(&per_section_evidence, 0.25);
        0.5 * mean + 0.5 * p25
    };

    GroundingOutputs {
        repository_grounding_score,
        evidence_coverage_score,
        per_section_anchors,
        per_section_evidence,
        anchored_words,
        tokens,
        labelled_code_fences,
        command_blocks,
        package_api_config_tokens,
        resolved_relative_links,
        resolved_internal_anchors,
        issue_pr_refs,
    }
}

/// Count §15.1 / §17.6 token classes. Only walks prose contexts so shell
/// snippets inside a code fence don't inflate the identifier density.
fn collect_token_counts(root: &Node<'_>, source: &str, file_path: &Path) -> GroundingTokenCounts {
    let mut out = GroundingTokenCounts::default();
    let base = file_path.parent().unwrap_or_else(|| Path::new("."));
    visit_token_counts(root, source, &base.to_path_buf(), &mut out, false);
    out
}

fn visit_token_counts(
    node: &Node<'_>,
    source: &str,
    base: &std::path::PathBuf,
    out: &mut GroundingTokenCounts,
    inside_prose: bool,
) {
    use NodeKind::*;

    let kind = node.kind();

    // Inline code is machine-readable but still a grounding signal inside
    // prose, so count the node itself and skip its children.
    if kind == InlineCode {
        if inside_prose {
            out.inline_code_tokens += 1;
        }
        return;
    }

    if is_non_prose_container(kind) {
        return;
    }

    let next_inside =
        inside_prose || opens_prose_context(kind, ProseContext::BODY_AND_HEADING_TEXT);

    if next_inside {
        match kind {
            IdentifierLikeToken => {
                out.identifier_like_tokens += 1;
            }
            PathLikeToken => {
                out.path_like_tokens += 1;
                let text = node_text(node, source);
                if repo_resolves(base, &text) {
                    out.resolved_path_like_tokens += 1;
                }
            }
            NumericToken => {
                out.numeric_tokens += 1;
                let text = node_text(node, source);
                if is_version_like(&text) {
                    out.version_tokens += 1;
                }
            }
            _ => {}
        }
    }

    for child in node.children() {
        visit_token_counts(&child, source, base, out, next_inside);
    }
}

/// Count `path_like_token` occurrences that contain at least one `.`.
/// Heuristic per task spec for "package/API/config identifier" signals.
fn collect_package_api_config_tokens(root: &Node<'_>, source: &str, _file_path: &Path) -> u64 {
    let mut total = 0u64;
    visit_package_api_config(root, source, &mut total, false);
    total
}

fn visit_package_api_config(node: &Node<'_>, source: &str, total: &mut u64, inside_prose: bool) {
    use NodeKind::*;
    let kind = node.kind();
    if is_non_prose_container(kind) {
        return;
    }
    let next_inside =
        inside_prose || opens_prose_context(kind, ProseContext::BODY_AND_HEADING_TEXT);

    if next_inside && kind == PathLikeToken {
        let text = node_text(node, source);
        if text.contains('.') {
            *total += 1;
        }
    }

    for child in node.children() {
        visit_package_api_config(&child, source, total, next_inside);
    }
}

fn repo_resolves(base: &Path, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.contains("://")
        || path.starts_with("http:")
        || path.starts_with("https:")
        || path.starts_with("mailto:")
        || path.starts_with("tel:")
    {
        return false;
    }
    // Strip fragment / query, since path_like_tokens may carry them.
    let path = path.split_once('#').map(|x| x.0).unwrap_or(path);
    let path = path.split_once('?').map(|x| x.0).unwrap_or(path);
    if path.is_empty() {
        return false;
    }
    let stripped = path.strip_prefix('/').unwrap_or(path);
    let candidate = base.join(stripped);
    candidate.exists()
}

/// Matches `\d+\.\d+(\.\d+)?` with an optional leading `v`.
fn is_version_like(text: &str) -> bool {
    let s = text.trim();
    let s = s.strip_prefix('v').or(s.strip_prefix('V')).unwrap_or(s);
    let mut iter = s.split('.');
    let Some(first) = iter.next() else {
        return false;
    };
    let Some(second) = iter.next() else {
        return false;
    };
    let third = iter.next();
    if iter.next().is_some() {
        return false;
    }
    if !is_all_ascii_digits(first) || !is_all_ascii_digits(second) {
        return false;
    }
    match third {
        None => true,
        Some(t) => is_all_ascii_digits(t),
    }
}

fn is_all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn is_command_shell(lang: &str) -> bool {
    matches!(
        lang.trim().to_ascii_lowercase().as_str(),
        "bash" | "sh" | "shell" | "zsh"
    )
}

/// Count per-section evidence anchors per §16.1. An anchor is:
/// - resolved relative link OR external link OR internal-link to non-trivial section
/// - labelled code fence, table with header, parseable diagram
/// - image with alt/caption
/// - math block with nearby explanation
/// - issue/PR/scholarly reference link
/// - path-like token resolved to repository
///
/// Each artifact / link / token counts the section it falls inside once.
fn compute_per_section_anchors(
    sections: &[Section],
    links: &[LinkRecord],
    artifacts: &[ArtifactRecord],
    tables: &[TableRecord],
    per_section_resolved_paths: &[u64],
) -> Vec<u64> {
    let n = sections.len();
    if n == 0 {
        return Vec::new();
    }
    let mut anchors: Vec<u64> = vec![0; n];

    for l in links {
        let is_anchor = match l.class {
            LinkClass::Relative => matches!(l.resolved, Some(true)),
            LinkClass::External | LinkClass::ExternalVendor | LinkClass::Scholarly => true,
            LinkClass::Internal => {
                // internal link to non-trivial section: treat resolved internal as evidence.
                matches!(l.resolved, Some(true))
            }
            LinkClass::IssuePr => true,
            LinkClass::AbsoluteSameRepo
            | LinkClass::Footnote
            | LinkClass::UnresolvedReferenceUse
            | LinkClass::ReferenceDefinition => false,
        };
        if !is_anchor {
            continue;
        }
        if let Some(idx) = locate_section_by_line(sections, l.line) {
            anchors[idx] += 1;
        }
    }

    for a in artifacts {
        let is_anchor = match a.kind {
            ArtifactKind::Code => a.has_label, // labelled fence
            // §16.1 "parseable diagram": a diagram counts only when it
            // has a label AND parsed cleanly — diagrams that errored
            // during parsing (Phase C sets `oversized`/parse-error in
            // the diagram analyzer) are not evidence.
            ArtifactKind::Diagram => a.has_label && !a.oversized,
            ArtifactKind::Image => a.has_label, // image with alt/caption
            ArtifactKind::Math => a.has_explanation, // math with nearby explanation
            ArtifactKind::Table => false,       // handled below to check header separately
            ArtifactKind::Html => false,
        };
        if !is_anchor {
            continue;
        }
        if let Some(idx) = locate_section_by_line(sections, a.start_line) {
            anchors[idx] += 1;
        }
    }

    for t in tables {
        if !t.has_header {
            continue;
        }
        if let Some(idx) = locate_section_by_line(sections, t.start_line) {
            anchors[idx] += 1;
        }
    }

    // §16.1 "path-like token resolved to repository": credit each
    // section for every resolved `path_like_token` that lives inside it.
    for (idx, count) in per_section_resolved_paths.iter().enumerate() {
        if idx < anchors.len() {
            anchors[idx] = anchors[idx].saturating_add(*count);
        }
    }

    anchors
}

/// Walk the AST and tally the number of resolved `path_like_token`s per
/// section. Prose-context-gated (same logic as `visit_token_counts`) so
/// tokens inside code fences / HTML / front-matter don't double-count.
fn collect_per_section_resolved_paths(
    root: &Node<'_>,
    source: &str,
    base: &Path,
    sections: &[Section],
) -> Vec<u64> {
    let mut counts: Vec<u64> = vec![0; sections.len()];
    if sections.is_empty() {
        return counts;
    }
    visit_per_section_resolved_paths(root, source, base, &mut counts, sections, false);
    counts
}

fn visit_per_section_resolved_paths(
    node: &Node<'_>,
    source: &str,
    base: &Path,
    counts: &mut [u64],
    sections: &[Section],
    inside_prose: bool,
) {
    use NodeKind::*;
    let kind = node.kind();
    if is_non_prose_container(kind) {
        return;
    }
    let next_inside = inside_prose || opens_prose_context(kind, ProseContext::SECTION_TEXT);
    if next_inside && matches!(kind, PathLikeToken) {
        let text = node_text(node, source);
        if repo_resolves(base, &text) {
            let line = (node.start_row() as u64) + 1;
            if let Some(idx) = locate_section_by_line(sections, line) {
                counts[idx] = counts[idx].saturating_add(1);
            }
        }
    }
    for child in node.children() {
        visit_per_section_resolved_paths(&child, source, base, counts, sections, next_inside);
    }
}

fn locate_section_by_line(sections: &[Section], line: u64) -> Option<usize> {
    // §3.4: walk the leaf-most section whose [start_line, end_line] contains `line`.
    // Sections are stored in a pre-order walk, so the last matching section is
    // the innermost.
    let mut best: Option<(usize, u64)> = None;
    for (i, s) in sections.iter().enumerate() {
        if line >= s.start_line && line <= s.end_line {
            let width = s.end_line.saturating_sub(s.start_line);
            match best {
                Some((_, best_width)) if width >= best_width => {}
                _ => best = Some((i, width)),
            }
        }
    }
    best.map(|(i, _)| i)
}

fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Linear interpolation between closest ranks (NIST C=1 / type 7).
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = q * (n as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_like_detects_common_shapes() {
        assert!(is_version_like("1.0"));
        assert!(is_version_like("1.2.3"));
        assert!(is_version_like("v12.4"));
        assert!(is_version_like("V1.0.0"));
        assert!(!is_version_like("1"));
        assert!(!is_version_like("1.2.3.4"));
        assert!(!is_version_like("v.1.2"));
        assert!(!is_version_like("abc"));
    }

    #[test]
    fn percentile_q25_of_four_values() {
        // Type-7 percentile of [1, 2, 3, 4] at q=0.25 → pos = 0.75, so
        // 1 * 0.25 + 2 * 0.75 = 1.75. Reference value for the §16.3 p25
        // term.
        let p = percentile(&[1.0, 2.0, 3.0, 4.0], 0.25);
        assert!((p - 1.75).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn percentile_single_element() {
        assert_eq!(percentile(&[5.0], 0.25), 5.0);
        assert_eq!(percentile(&[], 0.25), 0.0);
    }

    #[test]
    fn command_shell_matches_canonical_tags() {
        assert!(is_command_shell("bash"));
        assert!(is_command_shell("SH"));
        assert!(is_command_shell(" shell"));
        assert!(is_command_shell("zsh"));
        assert!(!is_command_shell("rust"));
    }
}
