// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Top-level Markdown analysis entry point.
//!
//! Parses a Markdown source buffer with pulldown-cmark and produces a
//! [`MarkdownMetrics`] record covering:
//!
//! - §5 LOC family,
//! - §4 word count `W`,
//! - §3.4 section tree,
//! - §6 Effective Content Units,
//! - §7 Markdown Reading Path Complexity (weighted + raw),
//! - §8 Markdown Cognitive Complexity,
//! - §9 Markdown Halstead + §9.4 embedded-code adjustment,
//! - §10 Documentation Maintainability Index (full formula — all terms),
//! - §11 link classification + debt + scent + review burden,
//! - §12 visuals (images + diagrams),
//! - §13 tables,
//! - §14.1 per-code-fence burden (stored in `artifacts`),
//! - §14.3 per-math-block burden (stored in `artifacts`),
//! - §15 Repository Grounding Score,
//! - §16 Evidence Coverage Score,
//! - §17 Filler / Lazy Structure Risk + diagnostic labels,
//! - §18 Review Criticality Index,
//! - §19 artifact debt score,
//! - §20 Section Balance Score,
//! - §21 Good Scaffold Score,
//! - §§29–38 language-aware prose layer.
//!
//! The prose layer is kept strictly separate per §29.1 — it never modifies
//! any structural score.
//!
//! Pipeline order: Phase A (LOC, sections, words, ECU) → Phase B (MRPC,
//! MCC, Halstead) → Phase C (links, visuals, tables, artifacts, artifact
//! debt) → Phase D (section balance, grounding, filler, good scaffold,
//! DMI, RCI) → Phase E (prose). Phase C feeds real diagram node/edge
//! counts back into ECU so §6 does not stay at zero.

use std::path::Path;

use crate::artifact_debt::{DebtInputs, artifact_debt_score};
use crate::code_burden::{CodeFence, analyze_code_fences};
use crate::dmi::{DmiInputs, compute_dmi};
use crate::ecu::{compute_ecu_inputs, effective_content_units};
use crate::embedded_code::embedded_volume;
use crate::filler::analyze_filler;
use crate::good_scaffold::analyze_good_scaffold;
use crate::grounding::analyze_grounding;
use crate::halstead::compute_halstead;
use crate::links::analyze_links;
use crate::loc::{LineClasses, derive_ratios, physical_line_count};
use crate::math_burden::{MathBlock, analyze_math_blocks};
use crate::mcc::compute_mcc;
use crate::mrpc::compute_mrpc;
use crate::nearby::{BlockSpan, collect_blocks, has_prose_within};
use crate::prose::analyze_prose;
use crate::rci::{RciInputs, compute_rci};
use crate::section_balance::analyze_section_balance;
use crate::sections::collect_sections;
use crate::syntax_tree::{Node, parse_with_document};
use crate::tables::{aggregate_tables, analyze_tables};
use crate::types::{
    AiEra, ArtifactKind, ArtifactRecord, Complexity, DiagramRecord, Grounding, ImageRecord,
    Maintainability, MarkdownMetrics, Review, Size, TableRecord,
};
use crate::visuals::analyze_visuals;
use crate::words::count_words;

/// Parses `source` as Markdown and returns a metric record covering Phase A,
/// Phase B, Phase C, and Phase E. `path` is recorded verbatim into the
/// output's `path` field; the caller controls whether it is absolute or
/// relative.
pub fn analyze_markdown(source: &str, path: &Path) -> MarkdownMetrics {
    let (tree, document) = parse_with_document(source);
    let root = tree.root();

    // Phase A: LOC family, ratios, size, sections, ECU inputs.
    let total_lines = physical_line_count(source);
    let classes = LineClasses::build(&root, total_lines);
    let loc = classes.loc_family();
    let loc_ratios = derive_ratios(&loc);

    let words = count_words(&root);
    let sections = collect_sections(&root);
    // §3.4: the derived section tree has one section per heading. No
    // synthetic root is exported, so `sections.len()` is the heading count.
    let heading_sections = sections.len() as u64;
    let headings = heading_sections;

    let ecu_inputs = compute_ecu_inputs(&root, &classes);
    let ecu = effective_content_units(&loc, words, &ecu_inputs);

    // Phase B: complexity surface (MRPC, MCC, Halstead). DMI is deferred
    // until Phase D has computed its inputs.
    let mrpc = compute_mrpc(&root, &document, source);
    let mcc = compute_mcc(&root, &document, source);
    let mut halstead = compute_halstead(&root, &document, source);
    let emb = embedded_volume(&document);
    halstead.embedded_volume = emb;
    halstead.total_volume = halstead.volume + emb;

    // Phase C: block index for nearby-prose queries.
    let blocks: Vec<BlockSpan> = collect_blocks(&root);

    // Phase C: links.
    // TODO(link-check): the `resolved = None` external links become a CLI
    // flag so `--link-check` probes external URLs. For now they stay
    // unchecked to keep analysis offline and deterministic.
    let (link_records, link_agg) = analyze_links(&document, path, &sections, &[]);

    // Phase C: visuals (images + diagrams).
    let visual_analysis = analyze_visuals(&root, &document, source, path, words, &blocks);

    // Phase C: tables. Patch has_local_explanation afterwards via the
    // nearby-block index.
    let mut table_records = analyze_tables(&root, source);
    for t in &mut table_records {
        t.has_local_explanation = has_prose_within(&blocks, t.start_line, t.end_line, 2);
        // Recompute scaffold now that the explanation flag is known. The
        // per-table formula is a product of size_credit × has_header ×
        // has_local_explanation.
        if !t.has_local_explanation {
            t.scaffold = 0.0;
        }
    }
    let tables_agg = aggregate_tables(&table_records);

    // Phase C: code fences (skipping diagram-tagged fences which are owned
    // by visuals.rs).
    let code_fences: Vec<CodeFence> = analyze_code_fences(&document, &blocks);

    // Phase C: math blocks.
    let math_blocks: Vec<MathBlock> = analyze_math_blocks(&root, source, &blocks);

    // Diagram ECU contribution: feed diagram_nodes / diagram_edges now that
    // we have real counts. This fixes up §6 ECU without changing existing
    // code in `ecu.rs` (Phase A leaves zeros).
    let mut ecu_inputs_final = ecu_inputs.clone();
    ecu_inputs_final.diagram_nodes = visual_analysis.diagrams.iter().map(|d| d.nodes).sum();
    ecu_inputs_final.diagram_edges = visual_analysis.diagrams.iter().map(|d| d.edges).sum();
    let ecu_final = if ecu_inputs_final.diagram_nodes > 0 || ecu_inputs_final.diagram_edges > 0 {
        effective_content_units(&loc, words, &ecu_inputs_final)
    } else {
        ecu
    };

    // Phase C: unified artifact list — used by §19 and later by Phase D.
    let html_records = collect_html_blocks(&root);
    let artifacts = build_artifact_list(
        &code_fences,
        &table_records,
        &visual_analysis.diagrams,
        &visual_analysis.images,
        &math_blocks,
        &html_records,
        &blocks,
    );

    // Phase C: artifact debt score.
    let debt_inputs = DebtInputs {
        artifacts: &artifacts,
        links: &link_records,
        loc: &loc,
        raw_html_or_mdx_lines: ecu_inputs_final.raw_html_or_mdx_lines,
        diagram_parse_errors: visual_analysis
            .diagrams
            .iter()
            .filter(|d| d.parse_error)
            .count() as u64,
    };
    let artifact_debt = artifact_debt_score(&debt_inputs);

    // Phase D: section balance (§20).
    let section_balance = analyze_section_balance(&sections, words);

    // Phase D: grounding (§15) + evidence coverage (§16).
    let grounding = analyze_grounding(
        &root,
        source,
        path,
        words,
        &sections,
        &link_records,
        &artifacts,
        &table_records,
    );

    // Phase D: filler / lazy risk (§17).
    let filler = analyze_filler(
        &root,
        source,
        words,
        &sections,
        &artifacts,
        &link_records,
        &loc,
        &grounding,
        &section_balance,
    );

    // Phase D: good scaffold (§21).
    let good_scaffold = analyze_good_scaffold(
        &artifacts,
        &link_records,
        &link_agg,
        &visual_analysis.aggregate,
        &tables_agg,
    );

    // Phase D: DMI now that every §10 term is populated.
    let dmi = compute_dmi(DmiInputs {
        mrpc: mrpc.weighted,
        mcc: mcc.mcc,
        total_volume: halstead.total_volume,
        link_debt_score: link_agg.link_debt_score,
        table_burden_score: tables_agg.table_burden_score,
        artifact_debt_score: artifact_debt,
        section_imbalance: 1.0 - section_balance.section_balance_score,
        filler_lazy_risk: filler.filler_lazy_risk,
        good_scaffold_score: good_scaffold.good_scaffold_score,
    });

    // Phase D: RCI (§18). `metric_delta_percent` + `changed_links_or_artifacts`
    // default to 0 — those are `mehen diff` inputs (Phase F).
    let rci = compute_rci(RciInputs {
        mcc: mcc.mcc,
        words,
        mdh_volume_total: halstead.total_volume,
        repository_grounding_score: grounding.repository_grounding_score,
        evidence_coverage_score: grounding.evidence_coverage_score,
        link_review_burden: link_agg.review_burden,
        embedded_code_complexity: halstead.embedded_volume,
        metric_delta_percent: 0.0,
        changed_links_or_artifacts: 0,
    });

    // §§29–38 Prose layer. Kept strictly separate per §29.1 — it never
    // modifies any structural score.
    let prose = analyze_prose(&root, source.as_bytes());

    MarkdownMetrics {
        path: path.to_string_lossy().to_string(),
        loc,
        loc_ratios,
        size: Size {
            words,
            effective_content_units: ecu_final,
            sections: heading_sections,
            headings,
        },
        ecu_inputs: ecu_inputs_final,
        sections,
        complexity: Complexity {
            reading_path_complexity: mrpc.weighted,
            reading_path_complexity_raw: mrpc.raw,
            cognitive_complexity: mcc.mcc,
            halstead,
        },
        links: link_agg,
        link_records,
        visuals: visual_analysis.aggregate,
        tables: tables_agg,
        maintainability: Maintainability {
            documentation_maintainability_index: dmi,
            section_balance_score: section_balance.section_balance_score,
            good_scaffold_score: good_scaffold.good_scaffold_score,
            artifact_debt_score: artifact_debt,
        },
        grounding: Grounding {
            repository_grounding_score: grounding.repository_grounding_score,
            evidence_coverage_score: grounding.evidence_coverage_score,
        },
        ai_era: AiEra {
            filler_lazy_structure_risk: filler.filler_lazy_risk,
            labels: filler.labels,
            top_contributors: filler.top_contributors,
        },
        review: Review {
            review_criticality_index: rci.review_criticality_index,
        },
        artifacts,
        prose,
    }
}

#[derive(Debug, Clone)]
struct HtmlBlockRecord {
    start_line: u64,
    end_line: u64,
}

fn collect_html_blocks(root: &Node<'_>) -> Vec<HtmlBlockRecord> {
    let mut out: Vec<HtmlBlockRecord> = Vec::new();
    walk_html(root, &mut out);
    out.sort_by_key(|a| a.start_line);
    out
}

fn walk_html(node: &Node<'_>, out: &mut Vec<HtmlBlockRecord>) {
    use crate::kind::NodeKind::*;
    let kind = node.kind();
    if matches!(kind, HtmlBlock) {
        let start_line = (node.start_row() as u64) + 1;
        let (end_row, end_col) = node.end_position();
        let mut end = end_row;
        if end > node.start_row() && end_col == 0 {
            end -= 1;
        }
        let end_line = (end as u64) + 1;
        out.push(HtmlBlockRecord {
            start_line,
            end_line,
        });
        return;
    }
    for child in node.children() {
        walk_html(&child, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_artifact_list(
    code: &[CodeFence],
    tables: &[TableRecord],
    diagrams: &[DiagramRecord],
    images: &[ImageRecord],
    math: &[MathBlock],
    html: &[HtmlBlockRecord],
    blocks: &[BlockSpan],
) -> Vec<ArtifactRecord> {
    let mut out: Vec<ArtifactRecord> = Vec::new();

    for c in code {
        let oversized = c.loc > 120;
        out.push(ArtifactRecord {
            id: 0,
            kind: ArtifactKind::Code,
            start_line: c.start_line,
            end_line: c.end_line,
            language_tag: c.language.clone(),
            size: c.loc,
            has_explanation: c.has_nearby_prose,
            has_label: c.has_language_tag,
            oversized,
            burden: c.burden,
        });
    }

    for t in tables {
        let oversized = t.cells > 300;
        out.push(ArtifactRecord {
            id: 0,
            kind: ArtifactKind::Table,
            start_line: t.start_line,
            end_line: t.end_line,
            language_tag: None,
            size: t.cells,
            has_explanation: t.has_local_explanation,
            has_label: t.has_header,
            oversized,
            burden: t.burden,
        });
    }

    for d in diagrams {
        let oversized = d.nodes > 80;
        out.push(ArtifactRecord {
            id: 0,
            kind: ArtifactKind::Diagram,
            start_line: d.start_line,
            end_line: d.end_line,
            language_tag: Some(d.language.clone()),
            size: d.nodes,
            has_explanation: d.has_title_or_caption,
            has_label: d.has_title_or_caption,
            oversized,
            burden: d.complexity,
        });
    }

    for img in images {
        out.push(ArtifactRecord {
            id: 0,
            kind: ArtifactKind::Image,
            start_line: img.line,
            end_line: img.line,
            language_tag: None,
            size: 1,
            has_explanation: img.has_nearby_reference,
            has_label: img.has_alt_or_caption,
            oversized: false,
            burden: img.image_complexity,
        });
    }

    for m in math {
        let oversized = m.tokens > 50;
        out.push(ArtifactRecord {
            id: 0,
            kind: ArtifactKind::Math,
            start_line: m.start_line,
            end_line: m.end_line,
            language_tag: None,
            size: m.tokens,
            has_explanation: m.has_nearby_prose,
            has_label: false,
            oversized,
            burden: m.burden,
        });
    }

    for h in html {
        let size = h.end_line.saturating_sub(h.start_line) + 1;
        let has_explanation = has_prose_within(blocks, h.start_line, h.end_line, 2);
        out.push(ArtifactRecord {
            id: 0,
            kind: ArtifactKind::Html,
            start_line: h.start_line,
            end_line: h.end_line,
            language_tag: None,
            size,
            has_explanation,
            has_label: false,
            oversized: false,
            burden: size as f64,
        });
    }

    // Sort: start_line, then kind (lexicographic) for determinism, then
    // assign sequential ids.
    out.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then((a.kind as u8).cmp(&(b.kind as u8)))
            .then(a.end_line.cmp(&b.end_line))
    });
    for (i, rec) in out.iter_mut().enumerate() {
        rec.id = i as u64;
    }
    out
}
