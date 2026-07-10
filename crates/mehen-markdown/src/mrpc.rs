// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Markdown Reading Path Complexity (MRPC) per §7.
//!
//! Builds a navigation graph `G_doc = (N, E)`:
//!
//! - Nodes: sections, large code blocks (≥ 12 LOC), tables with ≥ 12 cells,
//!   diagrams, footnotes/reference definitions, linked documents, and
//!   external domains (one node per domain).
//! - Edges: sequential section, parent-child heading, internal link (to
//!   same-doc anchor or other section), relative repo link, external link,
//!   artifact explanation, footnote/reference.
//!
//! §7.2: `mrpc_raw = |E| - |N| + 2P`.
//! §7.3: `mrpc = max(1, sum(edge_weight) - |N| + 2P)`.
//!
//! Phase B treats external links as valid (weight 1.00). Phase C will add the
//! link validator and bump broken links to 1.20.

use std::collections::BTreeMap;

use crate::document::{MarkdownDocument, is_diagram_language};
// `crate::kind::NodeKind` is aliased to `MdKind` to avoid clashing with the
// local graph-node `NodeKind` enum defined below.
use crate::kind::NodeKind as MdKind;
use crate::syntax_tree::Node;
use crate::tree_helpers::{count_table_cells, has_scheme};

/// Per-edge weights from §7.3.
mod weights {
    pub(super) const HIERARCHY: f64 = 0.15;
    pub(super) const SEQUENTIAL: f64 = 0.20;
    pub(super) const INTERNAL: f64 = 0.50;
    pub(super) const FOOTNOTE: f64 = 0.65;
    pub(super) const RELATIVE: f64 = 0.80;
    pub(super) const EXTERNAL: f64 = 1.00;
    // TODO(Phase C): link validator bumps broken links from EXTERNAL (1.00) /
    // INTERNAL (0.50) to BROKEN (1.20).
    #[allow(dead_code)]
    pub(super) const _BROKEN: f64 = 1.20;
    pub(super) const ARTIFACT: f64 = 0.40;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum NodeKind {
    Section,
    LargeCode,
    LargeTable,
    Diagram,
    Footnote,
    LinkedDoc,
    ExternalDomain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct GraphNodeId {
    kind: NodeKind,
    /// Stable deterministic index within its kind (e.g. 0-based section id
    /// in document order or alphabetical domain index).
    index: u32,
}

#[derive(Debug, Clone, Copy)]
enum EdgeKind {
    Hierarchy,
    Sequential,
    InternalAnchor,
    Footnote,
    Relative,
    External,
    // TODO(Phase D): artifact-explanation edges fire when a section's
    // adjacency table shows explanatory prose near an artifact. Held
    // here so the edge-weight table stays intact once Phase D adds the
    // nearby-prose walker.
    #[allow(dead_code)]
    Artifact,
}

impl EdgeKind {
    fn weight(self) -> f64 {
        match self {
            EdgeKind::Hierarchy => weights::HIERARCHY,
            EdgeKind::Sequential => weights::SEQUENTIAL,
            EdgeKind::InternalAnchor => weights::INTERNAL,
            EdgeKind::Footnote => weights::FOOTNOTE,
            EdgeKind::Relative => weights::RELATIVE,
            EdgeKind::External => weights::EXTERNAL,
            EdgeKind::Artifact => weights::ARTIFACT,
        }
    }
}

#[derive(Debug)]
struct Edge {
    from: GraphNodeId,
    to: GraphNodeId,
    kind: EdgeKind,
}

/// Minimum rows/cells/LOC thresholds from §7.1.
const LARGE_CODE_LOC: usize = 12;
const LARGE_TABLE_CELLS: usize = 12;

/// Classification of a link's destination URL for MRPC edge typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkClass {
    /// Starts with `#` — same-document anchor.
    InternalAnchor,
    /// Relative path like `foo.md`, `../src/lib.rs` or `docs/api#auth`.
    Relative,
    /// Absolute URL with a scheme (http / https / mailto / etc.).
    External,
}

/// MRPC output bundle.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MrpcResult {
    pub(crate) weighted: f64,
    pub(crate) raw: f64,
}

/// Public entry point: walks the parsed AST to build `G_doc` and compute
/// both the §7.2 raw form and the §7.3 weighted form.
pub(crate) fn compute_mrpc(
    root: &Node<'_>,
    document: &MarkdownDocument,
    source: &str,
) -> MrpcResult {
    let mut graph = GraphBuilder::new(document);
    graph.walk(root, source);
    graph.emit()
}

struct GraphBuilder<'doc> {
    document: &'doc MarkdownDocument,
    sections: Vec<SectionInfo>,
    large_codes: u32,
    large_tables: u32,
    diagrams: u32,
    footnotes: BTreeMap<String, u32>,
    linked_docs: BTreeMap<String, u32>,
    external_domains: BTreeMap<String, u32>,
    /// Sequential order in which block-level nodes occurred within each
    /// section — the artifact-explanation edge fires when an artifact
    /// (large code / table / diagram) is adjacent to a paragraph.
    section_artifacts: Vec<Vec<GraphNodeId>>,
    edges: Vec<Edge>,
    /// Current section stack by level (index = depth - 1). The top of the
    /// stack is the current enclosing section for artifacts and links.
    section_stack: Vec<u32>,
    /// Slug (GFM style) → section id, built as sections are opened so
    /// internal anchors can resolve to an existing section node instead
    /// of fabricating one per unique anchor (Codex P1 on PR #83).
    section_slugs: BTreeMap<String, u32>,
}

struct SectionInfo {
    _id: u32,
    parent: Option<u32>,
}

impl<'doc> GraphBuilder<'doc> {
    fn new(document: &'doc MarkdownDocument) -> Self {
        GraphBuilder {
            document,
            sections: Vec::new(),
            large_codes: 0,
            large_tables: 0,
            diagrams: 0,
            footnotes: BTreeMap::new(),
            linked_docs: BTreeMap::new(),
            external_domains: BTreeMap::new(),
            section_artifacts: Vec::new(),
            edges: Vec::new(),
            section_stack: Vec::new(),
            section_slugs: BTreeMap::new(),
        }
    }

    fn walk(&mut self, root: &Node<'_>, source: &str) {
        self.walk_recurse(root, source);
        // Sequential section edges (document order, only within the same
        // parent).
        self.add_sequential_edges();
    }

    fn walk_recurse(&mut self, node: &Node<'_>, source: &str) {
        let kind = node.kind();

        match kind {
            MdKind::Section { .. } => {
                // §3.4 defines the derived section tree as "one section per
                // heading". Tree-sitter emits headingless wrapper sections
                // for pre-heading / blank content — those must not inflate
                // the MRPC node set, otherwise a newline-only file reports
                // non-zero `reading_path_complexity_raw` while
                // `size.sections` correctly reports 0. Only create a
                // Section graph node when an actual heading exists
                // (Codex P2 on PR #83).
                let Some(slug) = extract_heading_slug(node, source) else {
                    // Headingless wrapper: recurse into children so their
                    // artifacts/links reach the enclosing section, but do
                    // not create a graph node.
                    self.recurse_children(node, source);
                    return;
                };
                let parent = self.section_stack.last().copied();
                let id = self.sections.len() as u32;
                self.sections.push(SectionInfo { _id: id, parent });
                self.section_artifacts.push(Vec::new());
                if let Some(p) = parent {
                    self.edges.push(Edge {
                        from: GraphNodeId {
                            kind: NodeKind::Section,
                            index: p,
                        },
                        to: GraphNodeId {
                            kind: NodeKind::Section,
                            index: id,
                        },
                        kind: EdgeKind::Hierarchy,
                    });
                }
                self.section_slugs.entry(slug).or_insert(id);
                self.section_stack.push(id);
                self.recurse_children(node, source);
                self.section_stack.pop();
                return;
            }
            MdKind::FencedCodeBlock | MdKind::IndentedCodeBlock => {
                if let Some(block) = self.document.code_block_by_start_row(node.start_row()) {
                    let loc = block.content_line_count();
                    let is_diagram = block.language.as_deref().is_some_and(is_diagram_language);
                    if is_diagram {
                        let id = self.diagrams;
                        self.diagrams += 1;
                        self.add_artifact_node(GraphNodeId {
                            kind: NodeKind::Diagram,
                            index: id,
                        });
                    } else if loc >= LARGE_CODE_LOC {
                        let id = self.large_codes;
                        self.large_codes += 1;
                        self.add_artifact_node(GraphNodeId {
                            kind: NodeKind::LargeCode,
                            index: id,
                        });
                    }
                }
                return;
            }
            MdKind::PipeTable => {
                let cells = count_table_cells(node);
                if cells >= LARGE_TABLE_CELLS {
                    let id = self.large_tables;
                    self.large_tables += 1;
                    self.add_artifact_node(GraphNodeId {
                        kind: NodeKind::LargeTable,
                        index: id,
                    });
                }
                return;
            }
            MdKind::FootnoteDefinition => {
                let label = footnote_def_label(node, source).unwrap_or_default();
                let next = self.footnotes.len() as u32;
                let id = *self.footnotes.entry(label).or_insert(next);
                // Create an artifact-style node for the footnote so it
                // contributes to N even without any reference edge.
                let gid = GraphNodeId {
                    kind: NodeKind::Footnote,
                    index: id,
                };
                // Link from the enclosing section (if any) to the footnote —
                // definitions are traversed alongside their referencing
                // section, so model that as a hierarchy edge so it counts as
                // part of N but does not inflate weighted MRPC above the
                // footnote reference's own weight.
                if let Some(section_id) = self.section_stack.last().copied() {
                    self.edges.push(Edge {
                        from: GraphNodeId {
                            kind: NodeKind::Section,
                            index: section_id,
                        },
                        to: gid,
                        kind: EdgeKind::Hierarchy,
                    });
                }
                // Recurse into children so any Link/Image nodes inside the
                // footnote body still emit relative/external/internal edges
                // — long-form docs often store references inside footnotes
                // (Codex P2 on PR #83).
                self.recurse_children(node, source);
                return;
            }
            MdKind::LinkReferenceDefinition => {
                // Reference definitions never contribute a graph node of
                // their own. Reference-style links resolve through
                // `MarkdownDocument` facts in the `Link | Image` branch, so
                // `[text][id]` + `[id]: ...` carries the same navigation cost
                // as the equivalent inline `[text](...)`.
                return;
            }
            MdKind::FootnoteReference => {
                // Edge from enclosing section to the footnote node.
                let label = footnote_ref_label(node, source).unwrap_or_default();
                let next = self.footnotes.len() as u32;
                let id = *self.footnotes.entry(label).or_insert(next);
                if let Some(section_id) = self.section_stack.last().copied() {
                    self.edges.push(Edge {
                        from: GraphNodeId {
                            kind: NodeKind::Section,
                            index: section_id,
                        },
                        to: GraphNodeId {
                            kind: NodeKind::Footnote,
                            index: id,
                        },
                        kind: EdgeKind::Footnote,
                    });
                }
                // Fall through in case nested content matters — but a
                // footnote reference has no relevant children.
                return;
            }
            MdKind::Link | MdKind::Image => {
                // Pulldown resolves inline and reference-style destinations
                // before these compact syntax nodes are walked.
                if let Some(dest) = self
                    .document
                    .link_destination_by_span(node.start_byte(), node.end_byte())
                {
                    self.handle_link(dest);
                }
                return;
            }
            MdKind::Autolink => {
                // Autolinks (`<https://example.com>`) are semantically
                // equivalent to inline external links — they should emit
                // the same navigation edge (Codex P2 on PR #83).
                if let Some(dest) = autolink_destination(node, source) {
                    self.handle_link(&dest);
                }
                return;
            }
            _ => {}
        }

        self.recurse_children(node, source);
    }

    fn recurse_children(&mut self, node: &Node<'_>, source: &str) {
        for child in node.children() {
            self.walk_recurse(&child, source);
        }
    }

    fn add_artifact_node(&mut self, node: GraphNodeId) {
        // The artifact-explanation edge fires only when explanatory prose
        // lives adjacent to the artifact — §7.1 describes it that way, and
        // unconditionally adding the edge for every artifact inflates MRPC
        // for artifact-heavy docs and erases the explained/unexplained
        // distinction (Codex P2 on PR #83). The adjacency check itself is
        // Phase D (via the section-level paragraph walker); until then,
        // the artifact node is created (contributing to |N|) without an
        // edge. Phase D will insert the edge when the nearby-prose table
        // is populated.
        if let Some(section_id) = self.section_stack.last().copied() {
            let idx = section_id as usize;
            if idx < self.section_artifacts.len() {
                self.section_artifacts[idx].push(node);
            }
        }
    }

    fn handle_link(&mut self, dest: &str) {
        let Some(section_id) = self.section_stack.last().copied() else {
            // Pre-heading links: no MRPC section to anchor on, so they do not
            // contribute an edge. This matches the research-doc philosophy
            // that MRPC measures navigation between sections of a structured
            // document.
            return;
        };
        let from = GraphNodeId {
            kind: NodeKind::Section,
            index: section_id,
        };
        let class = classify_link(dest);
        match class {
            LinkClass::InternalAnchor => {
                // Internal anchors target another section of the *same*
                // document. §7.1 nodes already include every section, so
                // anchor resolution should land on one of those — not
                // fabricate a new node. Phase C will build the authoritative
                // heading-slug → section map; until then, match on GFM slug
                // built from the source text of each known heading.
                //
                // If the anchor fails to match any heading, route the edge
                // to section 0 (the enclosing document) so it still
                // contributes to the edge budget and the graph's connected
                // component count stays stable. Fabricating a per-anchor
                // `LinkedDoc` node would inflate |N| and depress MRPC on
                // TOC-heavy documents (Codex P1).
                let target_section = resolve_anchor_to_section(dest, &self.section_slugs)
                    .or_else(|| (!self.sections.is_empty()).then_some(0usize));
                if let Some(sid) = target_section {
                    self.edges.push(Edge {
                        from,
                        to: GraphNodeId {
                            kind: NodeKind::Section,
                            index: sid as u32,
                        },
                        kind: EdgeKind::InternalAnchor,
                    });
                }
            }
            LinkClass::Relative => {
                let key = normalize_relative_path(dest);
                let next = self.linked_docs.len() as u32;
                let id = *self.linked_docs.entry(key).or_insert(next);
                self.edges.push(Edge {
                    from,
                    to: GraphNodeId {
                        kind: NodeKind::LinkedDoc,
                        index: id,
                    },
                    kind: EdgeKind::Relative,
                });
            }
            LinkClass::External => {
                let domain = extract_domain(dest).unwrap_or_else(|| "unknown".to_string());
                let next = self.external_domains.len() as u32;
                let id = *self.external_domains.entry(domain).or_insert(next);
                self.edges.push(Edge {
                    from,
                    to: GraphNodeId {
                        kind: NodeKind::ExternalDomain,
                        index: id,
                    },
                    // TODO(Phase C): link validator promotes `External` to
                    // `Broken` (weight 1.20) when the target is unreachable.
                    kind: EdgeKind::External,
                });
            }
        }
    }

    fn add_sequential_edges(&mut self) {
        // Group sections by parent, then connect siblings in document order.
        let mut siblings: BTreeMap<Option<u32>, Vec<u32>> = BTreeMap::new();
        for (i, s) in self.sections.iter().enumerate() {
            siblings.entry(s.parent).or_default().push(i as u32);
        }
        for (_parent, ids) in siblings {
            for pair in ids.windows(2) {
                self.edges.push(Edge {
                    from: GraphNodeId {
                        kind: NodeKind::Section,
                        index: pair[0],
                    },
                    to: GraphNodeId {
                        kind: NodeKind::Section,
                        index: pair[1],
                    },
                    kind: EdgeKind::Sequential,
                });
            }
        }
    }

    fn emit(self) -> MrpcResult {
        let n_sections = self.sections.len() as u32;
        let n_large_code = self.large_codes;
        let n_large_table = self.large_tables;
        let n_diagram = self.diagrams;
        let n_footnote = self.footnotes.len() as u32;
        let n_linked_doc = self.linked_docs.len() as u32;
        let n_external = self.external_domains.len() as u32;

        let total_nodes = n_sections
            + n_large_code
            + n_large_table
            + n_diagram
            + n_footnote
            + n_linked_doc
            + n_external;

        if total_nodes == 0 {
            return MrpcResult::default();
        }

        // Connected components via union-find. We only care about components
        // among reachable nodes in the edge list; isolated artifact nodes
        // that were created but never referenced are rare (§7.1 says edges
        // make the node exist), but we count every declared node to stay
        // faithful to `|N|`.
        let p = connected_components(&self, total_nodes);
        let n = total_nodes as f64;

        let sum_w: f64 = self.edges.iter().map(|e| e.kind.weight()).sum();
        let raw_edges = self.edges.len() as f64;

        let weighted = (sum_w - n + 2.0 * p).max(1.0);
        let raw = raw_edges - n + 2.0 * p;

        MrpcResult { weighted, raw }
    }
}

fn connected_components(g: &GraphBuilder<'_>, total_nodes: u32) -> f64 {
    use std::collections::HashMap;

    // Assign a compact integer id to every declared node so union-find is
    // dense.
    let mut ids: HashMap<GraphNodeId, usize> = HashMap::new();
    for i in 0..g.sections.len() {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::Section,
                index: i as u32,
            },
            ids.len(),
        );
    }
    for i in 0..g.large_codes {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::LargeCode,
                index: i,
            },
            ids.len(),
        );
    }
    for i in 0..g.large_tables {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::LargeTable,
                index: i,
            },
            ids.len(),
        );
    }
    for i in 0..g.diagrams {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::Diagram,
                index: i,
            },
            ids.len(),
        );
    }
    for idx in g.footnotes.values() {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::Footnote,
                index: *idx,
            },
            ids.len(),
        );
    }
    for idx in g.linked_docs.values() {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::LinkedDoc,
                index: *idx,
            },
            ids.len(),
        );
    }
    for idx in g.external_domains.values() {
        ids.insert(
            GraphNodeId {
                kind: NodeKind::ExternalDomain,
                index: *idx,
            },
            ids.len(),
        );
    }

    let mut parent: Vec<usize> = (0..(total_nodes as usize)).collect();

    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }

    for e in &g.edges {
        let (Some(&a), Some(&b)) = (ids.get(&e.from), ids.get(&e.to)) else {
            continue;
        };
        union(&mut parent, a, b);
    }

    let mut roots = std::collections::HashSet::new();
    for i in 0..(total_nodes as usize) {
        roots.insert(find(&mut parent, i));
    }
    roots.len() as f64
}

fn classify_link(dest: &str) -> LinkClass {
    if dest.starts_with('#') {
        LinkClass::InternalAnchor
    } else if has_scheme(dest) {
        LinkClass::External
    } else {
        LinkClass::Relative
    }
}

fn extract_domain(dest: &str) -> Option<String> {
    let pos = dest.find("://")?;
    let rest = &dest[pos + 3..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return None;
    }
    // Strip userinfo ("user:pass@host").
    let host = match host.rfind('@') {
        Some(at) => &host[at + 1..],
        None => host,
    };
    // Strip port. Bracketed IPv6 literals need special handling — a naive
    // `rfind(':')` would split the address (`https://[2001:db8::1]/` →
    // `[2001:db8:`) because the host itself contains colons. Per RFC 3986
    // the bracketed host is `[ … ]` and any port follows the closing `]`.
    let host = if host.starts_with('[') {
        match host.find(']') {
            // `[ipv6]` or `[ipv6]:port` — keep everything up to and
            // including the closing `]`; anything after (including an
            // optional `:port`) is discarded.
            Some(close) => &host[..=close],
            // Malformed host (no closing `]`) — fall back to the whole
            // slice rather than produce something worse.
            None => host,
        }
    } else {
        match host.rfind(':') {
            Some(p) => &host[..p],
            None => host,
        }
    };
    Some(host.to_ascii_lowercase())
}

fn normalize_relative_path(dest: &str) -> String {
    // Drop any fragment so `foo.md#section` and `foo.md` collapse to one
    // linked-doc node. Keep query components — they usually point at
    // different targets.
    if let Some(pos) = dest.find('#') {
        dest[..pos].to_string()
    } else {
        dest.to_string()
    }
}

/// GFM slug builder: lowercases the source text of a heading, strips
/// punctuation except `-` and whitespace, collapses whitespace runs to `-`.
/// Returns `None` when the section has no heading (shouldn't happen in
/// well-formed grammars but we guard anyway).
fn extract_heading_slug(section: &Node<'_>, source: &str) -> Option<String> {
    let heading_node = find_heading_text_node(section)?;
    let start = heading_node.start_byte();
    let end = heading_node.end_byte();
    let text = source.get(start..end)?.trim();
    Some(gfm_slug(text))
}

fn find_heading_text_node<'a>(section: &Node<'a>) -> Option<Node<'a>> {
    for child in section.children() {
        if child.kind().is_heading() {
            // The visible text lives in the heading-content child; fall back
            // to the heading node itself if the grammar surface changes.
            for n in child.children() {
                if matches!(n.kind(), MdKind::HeadingContent) {
                    return Some(n);
                }
            }
            return Some(child);
        }
    }
    None
}

/// Compute a GFM-style anchor slug. This intentionally mirrors GitHub's
/// anchor generation: lowercase, drop punctuation (except `-`/`_`),
/// collapse whitespace to `-`, strip leading/trailing dashes.
fn gfm_slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        // drop everything else
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Resolve `#anchor` to an existing section id. Leading `#` is stripped,
/// and the remainder is slugified the same way section headings were,
/// so the two forms compare equal even with upper-case or punctuated
/// source text.
fn resolve_anchor_to_section(dest: &str, section_slugs: &BTreeMap<String, u32>) -> Option<usize> {
    let stripped = dest.strip_prefix('#')?;
    let slug = gfm_slug(stripped);
    if slug.is_empty() {
        return None;
    }
    section_slugs.get(&slug).copied().map(|x| x as usize)
}

/// Extract the URL from an `autolink` node. Autolinks wrap the URL in
/// `<>` and emit a `Uri` child; the text between the angle brackets is
/// the destination.
fn autolink_destination(node: &Node<'_>, source: &str) -> Option<String> {
    // Look for the `Uri` (or fallback to the whole node text) and strip
    // the surrounding angle brackets.
    for child in node.children() {
        if matches!(child.kind(), MdKind::Uri) {
            let bytes = source.as_bytes();
            let start = child.start_byte();
            let end = child.end_byte();
            if end <= bytes.len() && start < end {
                let text = std::str::from_utf8(&bytes[start..end]).ok()?.trim();
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    // Fallback: strip `<>` from the raw node text.
    let bytes = source.as_bytes();
    let start = node.start_byte();
    let end = node.end_byte();
    if end <= bytes.len() && start < end {
        let text = std::str::from_utf8(&bytes[start..end]).ok()?.trim();
        let clean = text.trim_start_matches('<').trim_end_matches('>');
        if !clean.is_empty() {
            return Some(clean.to_string());
        }
    }
    None
}

fn footnote_def_label(node: &Node<'_>, source: &str) -> Option<String> {
    label_text(node, source, MdKind::FootnoteLabel)
}

fn footnote_ref_label(node: &Node<'_>, source: &str) -> Option<String> {
    label_text(node, source, MdKind::FootnoteReferenceLabel)
        .or_else(|| label_text(node, source, MdKind::FootnoteLabel))
}

fn label_text(node: &Node<'_>, source: &str, target: MdKind) -> Option<String> {
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if n.kind() == target {
            let bytes = source.as_bytes();
            let start = n.start_byte();
            let end = n.end_byte();
            if end <= bytes.len() {
                return std::str::from_utf8(&bytes[start..end])
                    .ok()
                    .map(|s| s.trim().to_string());
            }
        }
        stack.extend(n.children());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute(src: &str) -> MrpcResult {
        let (tree, document) = crate::syntax_tree::parse_with_document(src);
        compute_mrpc(&tree.root(), &document, src)
    }

    #[test]
    fn empty_document_has_zero_mrpc() {
        let r = compute("");
        assert_eq!(r.weighted, 0.0);
        assert_eq!(r.raw, 0.0);
    }

    #[test]
    fn pure_prose_has_minimal_mrpc() {
        let src = "# Title\n\nSome prose with no links or artifacts.\n";
        let r = compute(src);
        // One section, no edges → weighted = max(1, 0 - 1 + 2*1) = 1.0.
        assert_eq!(r.weighted, 1.0);
    }

    #[test]
    fn internal_anchor_resolves_to_existing_section() {
        // Codex P1 on PR #83: `#anchor` links must resolve to an existing
        // section node rather than fabricate a new LinkedDoc node per
        // unique anchor. TOC-heavy documents used to bleed MRPC because
        // every anchor added one node and one edge.
        let src = "# Intro\n\n- [Install](#install)\n- [Usage](#usage)\n\n\
                   # Install\n\nInstall prose.\n\n# Usage\n\nUsage prose.\n";
        let r = compute(src);
        // 3 sections, 2 sequential edges (Intro→Install, Install→Usage),
        // 2 internal-anchor edges (Intro→Install, Intro→Usage). No extra
        // LinkedDoc nodes → node count stays at 3.
        // Weighted: 2*0.20 (sequential) + 2*0.50 (internal) = 1.40.
        // MRPC = max(1, 1.40 - 3 + 2*1) = max(1, 0.40) = 1.0.
        assert_eq!(r.weighted, 1.0);
        // Raw: 4 edges - 3 nodes + 2*1 = 3.
        assert_eq!(r.raw, 3.0);
    }

    #[test]
    fn code_block_fact_line_count_excludes_delimiters() {
        // Codex P2 on PR #83: LOC for a fenced code block must count
        // content only. A fence with exactly 10 content lines used to
        // report 12 LOC (opening + closing + 10) and trip the ≥12 LARGE
        // threshold.
        let src = "# Demo\n\n```text\n\
                   a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n\
                   ```\n";
        let (_tree, document) = crate::syntax_tree::parse_with_document(src);
        let block = document.code_blocks.first().expect("fenced block");
        assert_eq!(block.content_line_count(), 10);
    }

    #[test]
    fn gfm_slug_matches_github_style() {
        assert_eq!(gfm_slug("Hello World!"), "hello-world");
        assert_eq!(gfm_slug("Install & Use"), "install-use");
        assert_eq!(gfm_slug("§3.4 Section"), "34-section");
        assert_eq!(gfm_slug("   "), "");
        assert_eq!(gfm_slug("__underscore__"), "underscore");
    }

    #[test]
    fn external_link_gets_external_weight() {
        let src = "# Title\n\nSee [rust-lang](https://www.rust-lang.org) for more.\n";
        let r = compute(src);
        // N = 2 (section + external domain); E = 1 with weight 1.0.
        // Weighted MRPC = max(1, 1.0 - 2 + 2*1) = 1.0.
        assert_eq!(r.weighted, 1.0);
    }

    #[test]
    fn multi_section_mrpc_grows() {
        let src = "# A\n\ntext\n\n## B\n\ntext\n\n## C\n\n[x](https://example.com)\n";
        let r = compute(src);
        // 3 sections + 1 external domain = 4 nodes.
        // Edges: 2 hierarchy (A→B, A→C), 1 sequential (B→C), 1 external.
        // Sum weights = 0.15 + 0.15 + 0.20 + 1.00 = 1.50
        // |N| = 4, P = 1 → weighted = 1.50 - 4 + 2 = -0.50 → max(1, …) = 1.0
        assert_eq!(r.weighted, 1.0);
    }

    #[test]
    fn classify_link_behaves() {
        assert_eq!(classify_link("#intro"), LinkClass::InternalAnchor);
        assert_eq!(classify_link("./foo.md"), LinkClass::Relative);
        assert_eq!(classify_link("foo.md#x"), LinkClass::Relative);
        assert_eq!(classify_link("https://example.com/"), LinkClass::External);
        assert_eq!(classify_link("mailto:x@y.z"), LinkClass::External);
    }

    #[test]
    fn extract_domain_handles_userinfo_and_port() {
        assert_eq!(
            extract_domain("https://user:pw@Example.COM:8443/x?y=1"),
            Some("example.com".to_string())
        );
        assert_eq!(extract_domain("http://a.b/"), Some("a.b".to_string()));
        assert_eq!(extract_domain("mailto:x@y.z"), None);
    }

    #[test]
    fn extract_domain_handles_bracketed_ipv6() {
        // Codex P2 + Gemini medium on PR #83: bracketed IPv6 hosts with or
        // without a port must not collapse onto a prefix-of-address key.
        // Previously `rfind(':')` struck inside the literal and returned
        // `[2001:db8:`, merging distinct endpoints into one external-domain
        // node.
        assert_eq!(
            extract_domain("https://[2001:db8::1]/"),
            Some("[2001:db8::1]".to_string())
        );
        assert_eq!(
            extract_domain("https://[::1]:8080/path"),
            Some("[::1]".to_string())
        );
        assert_eq!(extract_domain("https://[::1]/"), Some("[::1]".to_string()));
        // Upper-case hex digits inside the literal lowercase as a unit;
        // brackets are kept intact.
        assert_eq!(
            extract_domain("https://[FE80::1]:443/"),
            Some("[fe80::1]".to_string())
        );
        // Non-bracketed hosts with port continue to work.
        assert_eq!(
            extract_domain("https://example.com:443/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn reference_style_link_mrpc_matches_inline() {
        // Codex P1 on PR #83: reference-style links must produce the same
        // MRPC as their inline counterpart. Both links point at
        // `../api.md` so the graph has 1 section + 1 linked_doc and the
        // weighted form collapses to max(1, 0.80 - 2 + 2) = 1.0.
        let inline = "# Title\n\nSee [docs](../api.md) for details.\n";
        let reference =
            "# Title\n\nSee [docs][api-docs\\]] for details.\n\n[api-docs\\]]: ../api.md\n";
        let a = compute(inline);
        let b = compute(reference);
        assert_eq!(
            a.weighted, b.weighted,
            "inline vs reference-style weighted mismatch: {:?} vs {:?}",
            a, b
        );
        assert_eq!(
            a.raw, b.raw,
            "inline vs reference-style raw mismatch: {:?} vs {:?}",
            a, b
        );
    }

    #[test]
    fn reference_style_external_links_match_inline_mrpc() {
        let inline = "# Title\n\n[A](https://example.com/a) [B](https://example.com/b)\n";
        let reference =
            "# Title\n\n[A][a] [B][b]\n\n[a]: https://example.com/a\n[b]: https://example.com/b\n";
        let a = compute(inline);
        let b = compute(reference);

        assert_eq!(
            a.weighted, b.weighted,
            "inline vs external reference weighted mismatch: {:?} vs {:?}",
            a, b
        );
        assert_eq!(
            a.raw, b.raw,
            "inline vs external reference raw mismatch: {:?} vs {:?}",
            a, b
        );
    }
}
