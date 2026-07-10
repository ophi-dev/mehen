// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Small Markdown syntax tree used internally by the metric passes.
//!
//! The analyzer consumes a compact owned tree built from `pulldown-cmark`
//! events. It exposes byte spans, row positions, a `children()` iterator, and
//! the [`NodeKind`] of each node — the shape the metric modules navigate.
//!
//! The tree is deliberately event-shaped: `pulldown-cmark` is a single-pass,
//! consuming event stream with no parent/child access, so the builder reifies
//! it once into this owned structure that the many independent metric passes
//! can each walk top-down.

use std::ops::Range;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, LinkType, MetadataBlockKind,
    Parser, Tag, TagEnd,
};

use crate::document::{
    DocumentBuilder, MarkdownDocument, ReferenceDefinition, line_starts, markdown_options,
    preserve_broken_reference_link, reference_definitions_from_source, row_at,
};
use crate::kind::{HeadingStyle, NodeKind, level_number};

#[derive(Debug)]
pub(crate) struct Tree {
    nodes: Vec<NodeData>,
}

#[derive(Clone, Debug)]
struct NodeData {
    kind: NodeKind,
    start_byte: usize,
    end_byte: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
    children: Vec<usize>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Node<'a> {
    tree: &'a Tree,
    index: usize,
}

impl Tree {
    pub(crate) fn root(&self) -> Node<'_> {
        Node {
            tree: self,
            index: 0,
        }
    }
}

impl<'a> Node<'a> {
    fn data(&self) -> &NodeData {
        &self.tree.nodes[self.index]
    }

    pub(crate) fn kind(&self) -> NodeKind {
        self.data().kind
    }

    pub(crate) fn start_byte(&self) -> usize {
        self.data().start_byte
    }

    pub(crate) fn end_byte(&self) -> usize {
        self.data().end_byte
    }

    #[allow(dead_code)]
    pub(crate) fn start_position(&self) -> (usize, usize) {
        (self.data().start_row, self.data().start_col)
    }

    pub(crate) fn end_position(&self) -> (usize, usize) {
        (self.data().end_row, self.data().end_col)
    }

    pub(crate) fn start_row(&self) -> usize {
        self.data().start_row
    }

    /// Iterates the direct children of this node in document order.
    pub(crate) fn children(&self) -> Children<'a> {
        Children {
            tree: self.tree,
            children: &self.tree.nodes[self.index].children,
            pos: 0,
        }
    }
}

/// Iterator over a node's direct children.
///
/// Replaces the former tree-sitter-style mutable `Cursor`
/// (`goto_first_child` / `goto_next_sibling`): all navigation in this crate is
/// strictly top-down over direct children, which a plain iterator expresses
/// directly.
pub(crate) struct Children<'a> {
    tree: &'a Tree,
    children: &'a [usize],
    pos: usize,
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let index = *self.children.get(self.pos)?;
        self.pos += 1;
        Some(Node {
            tree: self.tree,
            index,
        })
    }
}

#[cfg(test)]
pub(crate) fn parse(source: &str) -> Tree {
    parse_with_document(source).0
}

pub(crate) fn parse_with_document(source: &str) -> (Tree, MarkdownDocument) {
    Builder::new(source).parse_with_document()
}

struct Builder<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
    nodes: Vec<NodeData>,
    stack: Vec<usize>,
}

impl<'a> Builder<'a> {
    fn new(source: &'a str) -> Self {
        let line_starts = line_starts(source);
        let mut builder = Self {
            source,
            line_starts,
            nodes: Vec::new(),
            stack: Vec::new(),
        };
        let root = builder.new_node(NodeKind::Document, 0..source.len());
        builder.stack.push(root);
        builder
    }

    fn parse_with_document(mut self) -> (Tree, MarkdownDocument) {
        let reference_definitions = reference_definitions_from_source(self.source);
        let mut document = DocumentBuilder::new(self.source, reference_definitions.clone());
        let parser = Parser::new_with_broken_link_callback(
            self.source,
            markdown_options(),
            Some(preserve_broken_reference_link),
        );
        let offset_iter = parser.into_offset_iter();

        for (event, range) in offset_iter {
            document.handle_event(event.clone(), range.clone());
            self.handle_event(event, range);
        }

        self.add_reference_definitions(reference_definitions);
        self.recompute_empty_spans();
        self.wrap_sections();
        self.recompute_all_spans();

        (Tree { nodes: self.nodes }, document.finish())
    }

    fn handle_event(&mut self, event: Event<'a>, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start_tag(tag, range),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(_) => self.add_text(range),
            Event::Code(text) => self.add_inline_code(range, &text),
            Event::InlineMath(_) => self.add_math_inline(range),
            Event::DisplayMath(_) => self.add_math_block(range),
            Event::Html(_) => self.add_html(range, false),
            Event::InlineHtml(_) => self.add_html(range, true),
            Event::FootnoteReference(label) => self.add_footnote_reference(&label, range),
            Event::SoftBreak | Event::HardBreak => {
                self.add_child(NodeKind::Newline, range);
            }
            Event::Rule => {
                self.add_child(NodeKind::ThematicBreak, range);
            }
            Event::TaskListMarker(checked) => self.add_task_marker(checked, range),
        }
    }

    fn start_tag(&mut self, tag: Tag<'a>, range: Range<usize>) {
        match tag {
            Tag::Paragraph => self.push(NodeKind::Paragraph, range),
            Tag::Heading { level, .. } => self.push_heading(level, range),
            Tag::BlockQuote(kind) => self.push_blockquote(kind, range),
            Tag::CodeBlock(CodeBlockKind::Fenced(info)) => self.push_fenced_code(&info, range),
            Tag::CodeBlock(CodeBlockKind::Indented) => {
                self.push(NodeKind::IndentedCodeBlock, range)
            }
            Tag::HtmlBlock => self.push(NodeKind::HtmlBlock, range),
            Tag::List(start) => self.push_list(start, range),
            Tag::Item => self.push_list_item(range),
            Tag::FootnoteDefinition(label) => self.push_footnote_definition(&label, range),
            Tag::Table(alignments) => self.push_table(alignments, range),
            Tag::TableHead => self.push(NodeKind::PipeTableHeader, range),
            Tag::TableRow => self.push(NodeKind::PipeTableRow, range),
            Tag::TableCell => self.push(NodeKind::PipeTableCell, range),
            Tag::Emphasis => self.push(NodeKind::Emphasis, range),
            Tag::Strong => self.push(NodeKind::Strong, range),
            Tag::Strikethrough => self.push(NodeKind::Strikethrough, range),
            Tag::Superscript | Tag::Subscript => self.push(NodeKind::Emphasis, range),
            Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            } => self.push_link(link_type, &dest_url, &title, &id, range, false),
            Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            } => self.push_link(link_type, &dest_url, &title, &id, range, true),
            Tag::MetadataBlock(kind) => {
                let kind = metadata_kind(kind);
                self.push(kind, range);
            }
            Tag::DefinitionList => self.push(NodeKind::List, range),
            Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                self.push(NodeKind::ListItem { task: false }, range)
            }
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.pop_if(NodeKind::HeadingContent);
                self.pop_if_matches(NodeKind::is_heading);
            }
            TagEnd::Item => {
                self.pop_if_matches(|kind| matches!(kind, NodeKind::ListItemContent { .. }));
                self.pop_if_matches(NodeKind::is_list_item);
            }
            TagEnd::Link | TagEnd::Image => {
                self.pop_if(NodeKind::LinkLabel);
                self.pop_one_of(&[NodeKind::Link, NodeKind::Image, NodeKind::Autolink]);
            }
            TagEnd::Paragraph => self.pop_if(NodeKind::Paragraph),
            TagEnd::BlockQuote(_) => self.pop_one_of(&[NodeKind::BlockQuote, NodeKind::Callout]),
            TagEnd::CodeBlock => {
                self.pop_one_of(&[NodeKind::FencedCodeBlock, NodeKind::IndentedCodeBlock]);
            }
            TagEnd::HtmlBlock => self.pop_if(NodeKind::HtmlBlock),
            TagEnd::List(_) => self.pop_if(NodeKind::List),
            TagEnd::FootnoteDefinition => self.pop_if(NodeKind::FootnoteDefinition),
            TagEnd::Table => self.pop_if(NodeKind::PipeTable),
            TagEnd::TableHead => self.pop_if(NodeKind::PipeTableHeader),
            TagEnd::TableRow => self.pop_if(NodeKind::PipeTableRow),
            TagEnd::TableCell => self.pop_if(NodeKind::PipeTableCell),
            TagEnd::Emphasis | TagEnd::Superscript | TagEnd::Subscript => {
                self.pop_if(NodeKind::Emphasis)
            }
            TagEnd::Strong => self.pop_if(NodeKind::Strong),
            TagEnd::Strikethrough => self.pop_if(NodeKind::Strikethrough),
            TagEnd::MetadataBlock(kind) => {
                let kind = metadata_kind(kind);
                self.pop_if(kind);
            }
            TagEnd::DefinitionList => self.pop_if(NodeKind::List),
            TagEnd::DefinitionListTitle | TagEnd::DefinitionListDefinition => {
                self.pop_if_matches(NodeKind::is_list_item)
            }
        }
    }

    fn push_heading(&mut self, level: HeadingLevel, range: Range<usize>) {
        let (level, style) = resolve_heading(level, is_setext_heading(self.source, &range));
        // The resolved style (not the raw detection) drives the marker range:
        // a degenerate setext level (>2) is coerced to ATX, so its marker is
        // the `#` scan, not the underline scan.
        let setext = matches!(style, HeadingStyle::Setext);
        let heading = self.add_child(NodeKind::Heading { level, style }, range.clone());
        if let Some(marker_range) = heading_marker_range(self.source, &range, setext) {
            self.add_child_to(
                heading,
                NodeKind::HeadingMarker { level, style },
                marker_range,
            );
        }
        let content = self.add_child_to(heading, NodeKind::HeadingContent, empty_at(range.start));
        self.stack.push(heading);
        self.stack.push(content);
    }

    fn push_blockquote(&mut self, kind: Option<BlockQuoteKind>, range: Range<usize>) {
        let node_kind = if kind.is_some() {
            NodeKind::Callout
        } else {
            NodeKind::BlockQuote
        };
        let node = self.add_child(node_kind, range.clone());
        self.add_child_to(node, NodeKind::BlockQuoteMarker, first_byte(range.start));
        if kind.is_some()
            && let Some(marker) = callout_marker_ranges(self.source, &range)
        {
            self.add_child_to(node, NodeKind::CalloutMarkerOpen, marker.open);
            self.add_child_to(node, NodeKind::CalloutType, marker.callout_type);
            self.add_child_to(node, NodeKind::CalloutMarkerClose, marker.close);
        }
        self.stack.push(node);
    }

    fn push_fenced_code(&mut self, info: &str, range: Range<usize>) {
        let node = self.add_child(NodeKind::FencedCodeBlock, range.clone());
        if !info.trim().is_empty() {
            let info_range = find_in_range(self.source, &range, info).unwrap_or_else(|| {
                let start = range.start.min(range.end);
                start..start
            });
            let info_node = self.add_child_to(node, NodeKind::InfoString, info_range.clone());
            let lang_end = info
                .find(|c: char| c.is_whitespace() || c == ',' || c == '{')
                .unwrap_or(info.len());
            let lang = &info[..lang_end];
            if !lang.is_empty() {
                let lang_range =
                    find_in_range(self.source, &info_range, lang).unwrap_or(info_range);
                self.add_child_to(info_node, NodeKind::Language, lang_range);
            }
        }
        self.stack.push(node);
    }

    fn push_list(&mut self, start: Option<u64>, range: Range<usize>) {
        let node = self.add_child(NodeKind::List, range.clone());
        let _ = start;
        self.stack.push(node);
    }

    fn push_list_item(&mut self, range: Range<usize>) {
        let item = self.add_child(NodeKind::ListItem { task: false }, range.clone());
        self.add_child_to(
            item,
            NodeKind::ListMarker,
            list_item_marker_range(self.source, &range),
        );
        let content = self.add_child_to(
            item,
            NodeKind::ListItemContent { task: false },
            empty_at(range.start),
        );
        self.stack.push(item);
        self.stack.push(content);
    }

    fn push_footnote_definition(&mut self, label: &str, range: Range<usize>) {
        let node = self.add_child(NodeKind::FootnoteDefinition, range.clone());
        if let Some(label_range) = find_footnote_label_range(self.source, &range, label) {
            self.add_child_to(node, NodeKind::FootnoteLabel, label_range);
        }
        self.stack.push(node);
    }

    fn push_table(&mut self, alignments: Vec<Alignment>, range: Range<usize>) {
        let table = self.add_child(NodeKind::PipeTable, range.clone());
        let delim = self.add_child_to(
            table,
            NodeKind::PipeTableDelimiterRow,
            empty_at(range.start),
        );
        for align in alignments {
            let cell = self.add_child_to(
                delim,
                NodeKind::PipeTableDelimiterCell,
                empty_at(range.start),
            );
            match align {
                Alignment::Left => {
                    self.add_child_to(cell, NodeKind::PipeTableAlignLeft, empty_at(range.start));
                }
                Alignment::Right => {
                    self.add_child_to(cell, NodeKind::PipeTableAlignRight, empty_at(range.start));
                }
                Alignment::Center => {
                    self.add_child_to(cell, NodeKind::PipeTableAlignLeft, empty_at(range.start));
                    self.add_child_to(cell, NodeKind::PipeTableAlignRight, empty_at(range.start));
                }
                Alignment::None => {}
            }
        }
        self.stack.push(table);
    }

    fn push_link(
        &mut self,
        link_type: LinkType,
        dest_url: &str,
        title: &str,
        reference_id: &str,
        range: Range<usize>,
        image: bool,
    ) {
        if !image && matches!(link_type, LinkType::Autolink | LinkType::Email) {
            let node = self.add_child(NodeKind::Autolink, range.clone());
            let kind = if matches!(link_type, LinkType::Email) {
                NodeKind::Email
            } else {
                NodeKind::Uri
            };
            let dest_range = visible_autolink_range(self.source, &range, dest_url)
                .or_else(|| find_in_range(self.source, &range, dest_url))
                .unwrap_or_else(|| range.clone());
            self.add_child_to(node, kind, dest_range);
            self.stack.push(node);
            return;
        }

        let node = self.add_child(
            if image {
                NodeKind::Image
            } else {
                NodeKind::Link
            },
            range.clone(),
        );
        let dest_range =
            find_link_destination_range(self.source, &range, link_type, dest_url, reference_id);
        if let Some(dest_range) = dest_range.clone() {
            self.add_child_to(node, NodeKind::LinkDestination, dest_range);
        }
        if !title.is_empty()
            && let Some(title_range) =
                find_link_title_range(self.source, &range, title, dest_range.as_ref())
        {
            self.add_child_to(node, NodeKind::LinkTitle, title_range);
        }
        let label = self.add_child_to(node, NodeKind::LinkLabel, empty_at(range.start));
        self.stack.push(node);
        self.stack.push(label);
    }

    fn add_text(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        let parent = self.current();
        let parent_kind = self.nodes[parent].kind;
        if matches!(parent_kind, NodeKind::FencedCodeBlock) {
            self.add_child_to(parent, NodeKind::CodeFenceContent, range);
            return;
        }
        if matches!(parent_kind, NodeKind::IndentedCodeBlock) {
            self.add_child_to(parent, NodeKind::IndentedChunk, range);
            return;
        }
        self.tokenize_text(range);
    }

    fn add_inline_code(&mut self, range: Range<usize>, text: &str) {
        let node = self.add_child(NodeKind::InlineCode, range.clone());
        let content = inline_code_content_range(self.source, &range, text);
        self.add_child_to(node, NodeKind::InlineCodeContent, content);
    }

    fn add_math_inline(&mut self, range: Range<usize>) {
        let node = self.add_child(NodeKind::MathInline, range.clone());
        self.add_child_to(node, NodeKind::MathInlineContent, range.clone());
        self.tokenize_text_into(node, range);
    }

    fn add_math_block(&mut self, range: Range<usize>) {
        let node = self.add_child(NodeKind::MathBlock, range.clone());
        self.add_child_to(node, NodeKind::MathBlockDelimiter, first_byte(range.start));
        self.add_child_to(node, NodeKind::MathBlockContent, range.clone());
        self.tokenize_text_into(node, range);
    }

    fn add_html(&mut self, range: Range<usize>, inline: bool) {
        let text = self.source.get(range.clone()).unwrap_or("");
        if text.trim().is_empty() {
            return;
        }
        let parent = self.current();
        let node = if inline || !matches!(self.nodes[parent].kind, NodeKind::HtmlBlock) {
            self.add_child(
                if inline {
                    NodeKind::HtmlInline
                } else {
                    NodeKind::HtmlBlock
                },
                range.clone(),
            )
        } else {
            parent
        };
        let kind = classify_html(text);
        self.add_child_to(node, kind, range);
    }

    fn add_footnote_reference(&mut self, label: &str, range: Range<usize>) {
        let node = self.add_child(NodeKind::FootnoteReference, range.clone());
        if let Some(label_range) = find_footnote_label_range(self.source, &range, label) {
            self.add_child_to(node, NodeKind::FootnoteReferenceLabel, label_range);
        }
    }

    fn add_task_marker(&mut self, checked: bool, range: Range<usize>) {
        let marker = if checked {
            NodeKind::TaskListMarkerChecked
        } else {
            NodeKind::TaskListMarkerUnchecked
        };
        self.add_child(marker, range);
        for &idx in self.stack.iter().rev() {
            match self.nodes[idx].kind {
                NodeKind::ListItem { .. } => {
                    self.nodes[idx].kind = NodeKind::ListItem { task: true };
                    break;
                }
                NodeKind::ListItemContent { .. } => {
                    self.nodes[idx].kind = NodeKind::ListItemContent { task: true };
                }
                _ => {}
            }
        }
    }

    fn tokenize_text(&mut self, range: Range<usize>) {
        let parent = self.current();
        self.tokenize_text_into(parent, range);
    }

    fn tokenize_text_into(&mut self, parent: usize, range: Range<usize>) {
        let Some(text) = self.source.get(range.clone()) else {
            return;
        };
        let chars: Vec<_> = text.char_indices().collect();
        let mut token_start: Option<usize> = None;
        for (idx, (offset, ch)) in chars.iter().copied().enumerate() {
            let abs = range.start + offset;
            let prev = idx
                .checked_sub(1)
                .and_then(|prev_idx| chars.get(prev_idx))
                .map(|(_, ch)| *ch);
            let next = chars.get(idx + 1).map(|(_, ch)| *ch);
            if is_token_char(ch, prev, next) {
                token_start.get_or_insert(abs);
                continue;
            }
            if let Some(start) = token_start.take() {
                self.add_wordish_token(parent, start..abs);
            }
            if !ch.is_whitespace() {
                let end = abs + ch.len_utf8();
                if let Some(kind) = punctuation_kind(ch) {
                    self.add_child_to(parent, kind, abs..end);
                }
            }
        }
        if let Some(start) = token_start {
            self.add_wordish_token(parent, start..range.end);
        }
    }

    fn add_wordish_token(&mut self, parent: usize, range: Range<usize>) {
        let text = self.source.get(range.clone()).unwrap_or("");
        let kind = classify_wordish(text);
        self.add_child_to(parent, kind, range);
    }

    fn add_reference_definitions(&mut self, refdefs: Vec<ReferenceDefinition>) {
        for def in refdefs {
            let parent_span = def.label_span.start..def.span.end;
            let parent = self.reference_definition_parent(&parent_span);
            let node =
                self.add_child_to(parent, NodeKind::LinkReferenceDefinition, def.span.clone());
            self.add_child_to(node, NodeKind::LinkLabel, def.label_span);
            self.add_child_to(node, NodeKind::LinkDestination, def.destination_span);
            if let Some(title_span) = def.title_span {
                self.add_child_to(node, NodeKind::LinkTitle, title_span);
            }
        }
    }

    fn reference_definition_parent(&self, span: &Range<usize>) -> usize {
        let mut best = 0;
        let mut best_width = usize::MAX;
        for (idx, node) in self.nodes.iter().enumerate().skip(1) {
            if !is_reference_definition_container(node.kind) {
                continue;
            }
            if node.start_byte <= span.start && span.end <= node.end_byte {
                let width = node.end_byte.saturating_sub(node.start_byte);
                if width <= best_width {
                    best = idx;
                    best_width = width;
                }
            }
        }
        best
    }

    fn wrap_sections(&mut self) {
        let mut top = self.nodes[0].children.clone();
        top.sort_by_key(|idx| (self.nodes[*idx].start_byte, self.nodes[*idx].end_byte));
        self.nodes[0].children.clear();

        let mut section_stack: Vec<(u8, usize)> = Vec::new();
        for child in top {
            if let Some(level) = self.nodes[child].kind.heading_level() {
                while section_stack
                    .last()
                    .map(|(stack_level, _)| *stack_level >= level)
                    .unwrap_or(false)
                {
                    section_stack.pop();
                }
                let section = self.new_node(
                    NodeKind::Section { level },
                    self.nodes[child].start_byte..self.nodes[child].end_byte,
                );
                self.nodes[section].children.push(child);
                if let Some((_, parent)) = section_stack.last().copied() {
                    self.nodes[parent].children.push(section);
                } else {
                    self.nodes[0].children.push(section);
                }
                section_stack.push((level, section));
            } else if let Some((_, section)) = section_stack.last().copied() {
                self.nodes[section].children.push(child);
            } else {
                self.nodes[0].children.push(child);
            }
        }
    }

    fn recompute_empty_spans(&mut self) {
        for idx in 0..self.nodes.len() {
            if self.nodes[idx].start_byte == self.nodes[idx].end_byte {
                self.refresh_span_from_children(idx);
            }
        }
    }

    fn recompute_all_spans(&mut self) {
        self.recompute_span_rec(0);
    }

    fn recompute_span_rec(&mut self, idx: usize) -> Option<Range<usize>> {
        let children = self.nodes[idx].children.clone();
        let mut start = self.nodes[idx].start_byte;
        let mut end = self.nodes[idx].end_byte;
        for child in children {
            if let Some(child_range) = self.recompute_span_rec(child) {
                start = start.min(child_range.start);
                end = end.max(child_range.end);
            }
        }
        if !self.nodes[idx].children.is_empty()
            && recompute_span_from_children(self.nodes[idx].kind)
        {
            self.set_range(idx, start..end);
        }
        Some(self.nodes[idx].start_byte..self.nodes[idx].end_byte)
    }

    fn refresh_span_from_children(&mut self, idx: usize) {
        let children = self.nodes[idx].children.clone();
        let Some(first) = children.first().copied() else {
            return;
        };
        let mut start = self.nodes[first].start_byte;
        let mut end = self.nodes[first].end_byte;
        for child in children.iter().copied().skip(1) {
            start = start.min(self.nodes[child].start_byte);
            end = end.max(self.nodes[child].end_byte);
        }
        self.set_range(idx, start..end);
    }

    fn push(&mut self, kind: NodeKind, range: Range<usize>) {
        let node = self.add_child(kind, range);
        self.stack.push(node);
    }

    fn add_child(&mut self, kind: NodeKind, range: Range<usize>) -> usize {
        let parent = self.current();
        self.add_child_to(parent, kind, range)
    }

    fn add_child_to(&mut self, parent: usize, kind: NodeKind, range: Range<usize>) -> usize {
        let node = self.new_node(kind, range);
        self.nodes[parent].children.push(node);
        node
    }

    fn new_node(&mut self, kind: NodeKind, range: Range<usize>) -> usize {
        let range = clamp_range(range, self.source.len());
        let (start_row, start_col) = self.position(range.start);
        let (end_row, end_col) = self.position(range.end);
        let idx = self.nodes.len();
        self.nodes.push(NodeData {
            kind,
            start_byte: range.start,
            end_byte: range.end,
            start_row,
            start_col,
            end_row,
            end_col,
            children: Vec::new(),
        });
        idx
    }

    fn set_range(&mut self, idx: usize, range: Range<usize>) {
        let range = clamp_range(range, self.source.len());
        let (start_row, start_col) = self.position(range.start);
        let (end_row, end_col) = self.position(range.end);
        let node = &mut self.nodes[idx];
        node.start_byte = range.start;
        node.end_byte = range.end;
        node.start_row = start_row;
        node.start_col = start_col;
        node.end_row = end_row;
        node.end_col = end_col;
    }

    fn current(&self) -> usize {
        *self.stack.last().expect("builder stack is empty")
    }

    fn pop_if(&mut self, kind: NodeKind) {
        if self.stack.last().map(|idx| self.nodes[*idx].kind) == Some(kind) {
            self.stack.pop();
        }
    }

    /// Pops the stack top when its kind satisfies `pred`.
    ///
    /// Used for the folded families (headings, list items) where the top can
    /// be any level/flag variant of a group.
    fn pop_if_matches(&mut self, pred: impl Fn(NodeKind) -> bool) {
        if self
            .stack
            .last()
            .map(|idx| pred(self.nodes[*idx].kind))
            .unwrap_or(false)
        {
            self.stack.pop();
        }
    }

    fn pop_one_of(&mut self, kinds: &[NodeKind]) {
        let Some(idx) = self.stack.last().copied() else {
            return;
        };
        let actual = self.nodes[idx].kind;
        if kinds.contains(&actual) {
            self.stack.pop();
        } else {
            debug_assert!(
                self.stack.len() <= 1,
                "unexpected markdown builder stack top: expected one of {kinds:?}, got {actual:?}"
            );
        }
    }

    fn position(&self, byte: usize) -> (usize, usize) {
        let byte = byte.min(self.source.len());
        let row = row_at(&self.line_starts, self.source.len(), byte);
        (row, byte.saturating_sub(self.line_starts[row]))
    }
}

fn metadata_kind(kind: MetadataBlockKind) -> NodeKind {
    match kind {
        MetadataBlockKind::YamlStyle => NodeKind::MinusMetadata,
        MetadataBlockKind::PlusesStyle => NodeKind::PlusMetadata,
    }
}

/// Whether a container's span should be widened to cover its children.
///
/// These are synthesized or empty-initialized spans (sections, content
/// wrappers, table delimiter scaffolding) whose true extent is only known
/// once children are attached.
fn recompute_span_from_children(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Section { .. }
            | NodeKind::HeadingContent
            | NodeKind::LinkLabel
            | NodeKind::ListItemContent { .. }
            | NodeKind::PipeTableDelimiterRow
            | NodeKind::PipeTableDelimiterCell
    )
}

fn clamp_range(range: Range<usize>, len: usize) -> Range<usize> {
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);
    start..end
}

fn empty_at(byte: usize) -> Range<usize> {
    byte..byte
}

fn first_byte(byte: usize) -> Range<usize> {
    byte..byte.saturating_add(1)
}

/// Resolves the `(level, style)` of a heading from its pulldown level and
/// whether the source span looks like a setext underline.
///
/// pulldown-cmark only surfaces setext headings at H1/H2. A detected setext
/// underline at any other level is a degenerate case; it is coerced to ATX
/// H1, preserving the pre-refactor `heading_kinds` fallback behavior.
fn resolve_heading(level: HeadingLevel, setext: bool) -> (u8, HeadingStyle) {
    match (level, setext) {
        (HeadingLevel::H1, true) => (1, HeadingStyle::Setext),
        (HeadingLevel::H2, true) => (2, HeadingStyle::Setext),
        (_, true) => (1, HeadingStyle::Atx),
        (level, false) => (level_number(level), HeadingStyle::Atx),
    }
}

fn is_setext_heading(source: &str, range: &Range<usize>) -> bool {
    let Some(slice) = source.get(range.clone()) else {
        return false;
    };
    let mut non_empty = slice.lines().filter(|line| !line.trim().is_empty());
    let Some(_first) = non_empty.next() else {
        return false;
    };
    let Some(last) = non_empty.next_back() else {
        return false;
    };
    let trimmed = last.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '=' || c == '-')
}

fn heading_marker_range(source: &str, range: &Range<usize>, setext: bool) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    if setext {
        let mut offset = range.start;
        for line in slice.split_inclusive('\n') {
            let trimmed = line.trim();
            if !trimmed.is_empty() && trimmed.chars().all(|c| c == '=' || c == '-') {
                let ws = line.len() - line.trim_start().len();
                let len = trimmed.len();
                return Some(offset + ws..offset + ws + len);
            }
            offset += line.len();
        }
        return None;
    }
    let line = slice.lines().next().unwrap_or(slice);
    let leading = line.len() - line.trim_start().len();
    let hashes = line[leading..].bytes().take_while(|b| *b == b'#').count();
    (hashes > 0).then_some(range.start + leading..range.start + leading + hashes)
}

fn list_item_marker_range(source: &str, range: &Range<usize>) -> Range<usize> {
    let Some(line) = source
        .get(range.clone())
        .and_then(|slice| slice.lines().next())
    else {
        return first_byte(range.start);
    };
    let leading = line.len() - line.trim_start().len();
    let trimmed = &line[leading..];
    let len = if trimmed.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        trimmed
            .find(|ch| ['.', ')'].contains(&ch))
            .map(|idx| idx + 1)
            .unwrap_or(1)
    } else {
        1
    };
    range.start + leading..range.start + leading + len
}

fn callout_type_range(source: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    let local = slice.find("[!")?;
    let start = range.start + local + 2;
    let end = source[start..].find(']').map(|n| start + n)?;
    Some(start..end)
}

struct CalloutMarkerRanges {
    open: Range<usize>,
    callout_type: Range<usize>,
    close: Range<usize>,
}

fn callout_marker_ranges(source: &str, range: &Range<usize>) -> Option<CalloutMarkerRanges> {
    let callout_type = callout_type_range(source, range)?;
    let open_start = callout_type.start.checked_sub(2)?;
    let close_start = callout_type.end.min(range.end);
    let close_end = close_start.saturating_add(1).min(range.end);
    Some(CalloutMarkerRanges {
        open: open_start..callout_type.start,
        callout_type,
        close: close_start..close_end,
    })
}

fn visible_autolink_range(source: &str, range: &Range<usize>, dest: &str) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    let inner = slice.trim().trim_start_matches('<').trim_end_matches('>');
    if inner.is_empty() {
        return None;
    }
    find_in_range(source, range, inner).or_else(|| find_in_range(source, range, dest))
}

fn find_footnote_label_range(
    source: &str,
    range: &Range<usize>,
    label: &str,
) -> Option<Range<usize>> {
    find_in_range(source, range, &format!("[^{label}]"))
}

fn find_link_destination_range(
    source: &str,
    range: &Range<usize>,
    link_type: LinkType,
    destination: &str,
    reference_id: &str,
) -> Option<Range<usize>> {
    match link_type {
        LinkType::Inline | LinkType::WikiLink { .. } if !destination.is_empty() => {
            let search_range =
                inline_link_payload_range(source, range).unwrap_or_else(|| range.clone());
            find_in_range(source, &search_range, destination)
        }
        LinkType::Reference | LinkType::ReferenceUnknown if !reference_id.is_empty() => {
            reference_link_key_range(source, range)
        }
        _ => None,
    }
}

fn find_link_title_range(
    source: &str,
    range: &Range<usize>,
    title: &str,
    destination: Option<&Range<usize>>,
) -> Option<Range<usize>> {
    let search_range = destination
        .map(|dest| dest.end..range.end)
        .or_else(|| inline_link_payload_range(source, range))
        .unwrap_or_else(|| range.clone());
    find_in_range(source, &search_range, title)
}

fn inline_link_payload_range(source: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    let payload_start = slice.find("](")? + 2;
    Some(range.start + payload_start..range.end)
}

fn reference_link_key_range(source: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    let close = slice.rfind(']')?;
    let before_close = &slice[..close];
    let open = before_close.rfind('[')?;
    if open == 0 || !before_close[..open].ends_with(']') {
        return None;
    }
    trim_byte_range(source, range.start + open + 1..range.start + close)
}

fn trim_byte_range(source: &str, range: Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(range.clone())?;
    let start_offset = slice.len() - slice.trim_start().len();
    let end_offset = slice.trim_end().len();
    let trimmed = range.start + start_offset..range.start + end_offset;
    (trimmed.start < trimmed.end).then_some(trimmed)
}

fn inline_code_content_range(source: &str, range: &Range<usize>, text: &str) -> Range<usize> {
    let Some(slice) = source.get(range.clone()) else {
        return range.clone();
    };
    let opening = slice.bytes().take_while(|byte| *byte == b'`').count();
    let closing = slice.bytes().rev().take_while(|byte| *byte == b'`').count();
    let inner_start = range.start.saturating_add(opening).min(range.end);
    let inner_end = range.end.saturating_sub(closing).max(inner_start);
    let inner = inner_start..inner_end;
    find_in_range(source, &inner, text).unwrap_or(inner)
}

fn find_in_range(source: &str, range: &Range<usize>, needle: &str) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let slice = source.get(range.clone())?;
    let local = slice.find(needle)?;
    Some(range.start + local..range.start + local + needle.len())
}

fn classify_html(text: &str) -> NodeKind {
    let trimmed = text.trim_start();
    if trimmed.starts_with("<!--") {
        NodeKind::HtmlComment
    } else if trimmed.starts_with("<![CDATA[") {
        NodeKind::HtmlCdata
    } else if trimmed.starts_with("<?") {
        NodeKind::HtmlProcessingInstruction
    } else if trimmed.starts_with("<!") {
        NodeKind::HtmlDeclaration
    } else if trimmed.starts_with("</") {
        NodeKind::HtmlCloseTag
    } else {
        NodeKind::HtmlOpenTag
    }
}

fn is_token_char(ch: char, prev: Option<char>, next: Option<char>) -> bool {
    if ch.is_alphanumeric() || ch == '_' {
        return true;
    }
    let prev_word = prev.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'));
    let next_word = next.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'));
    match ch {
        '-' => prev_word && next_word,
        '.' => {
            (prev_word && next_word)
                || matches!(next, Some('/' | '.'))
                || matches!(prev, Some('.')) && next_word
        }
        '/' | '\\' => prev_word || next_word || matches!(prev, Some(':' | '/' | '\\')),
        ':' => {
            prev_word
                && (matches!(next, Some('/' | ':')) || next.is_some_and(|c| c.is_alphanumeric()))
        }
        '@' => prev_word && next_word,
        _ => false,
    }
}

fn classify_wordish(text: &str) -> NodeKind {
    let trimmed = text.trim_matches(|c: char| c == '-' || c == '_' || c == '.');
    if trimmed.is_empty() {
        return NodeKind::WordToken;
    }
    if is_numeric_like(trimmed) {
        NodeKind::NumericToken
    } else if is_path_like(trimmed) {
        NodeKind::PathLikeToken
    } else if is_identifier_like(trimmed) {
        NodeKind::IdentifierLikeToken
    } else {
        NodeKind::WordToken
    }
}

fn is_numeric_like(text: &str) -> bool {
    let s = text
        .strip_prefix('v')
        .or(text.strip_prefix('V'))
        .unwrap_or(text);
    let mut has_digit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            has_digit = true;
        } else if ch != '.' && ch != '_' && ch != '-' {
            return false;
        }
    }
    has_digit
}

fn is_path_like(text: &str) -> bool {
    text.contains('/')
        || text.contains('\\')
        || text.starts_with("./")
        || text.starts_with("../")
        || text
            .rsplit_once('.')
            .map(|(_, ext)| ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or(false)
}

fn is_identifier_like(text: &str) -> bool {
    text.contains('_')
        || text.contains("::")
        || text.contains('@')
        || text.chars().any(|c| c.is_ascii_digit())
        || has_camel_hump(text)
}

fn has_camel_hump(text: &str) -> bool {
    let mut prev_lower = false;
    for ch in text.chars() {
        if prev_lower && ch.is_ascii_uppercase() {
            return true;
        }
        prev_lower = ch.is_ascii_lowercase();
    }
    false
}

fn punctuation_kind(ch: char) -> Option<NodeKind> {
    Some(match ch {
        '.' | '?' | '!' | '。' | '…' => NodeKind::Terminator,
        ',' | ';' | ':' => NodeKind::Separator,
        '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' => NodeKind::Bracket,
        '=' | '+' | '-' | '*' | '/' | '|' | '&' | '^' | '%' | '~' => NodeKind::OperatorLike,
        _ => return None,
    })
}

fn is_reference_definition_container(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::BlockQuote
            | NodeKind::Callout
            | NodeKind::List
            | NodeKind::ListItem { .. }
            | NodeKind::ListItemContent { .. }
            | NodeKind::FootnoteDefinition
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_text<'a>(tree: &Tree, source: &'a str, idx: usize) -> &'a str {
        let node = &tree.nodes[idx];
        &source[node.start_byte..node.end_byte]
    }

    fn first_node(tree: &Tree, kind: NodeKind) -> usize {
        tree.nodes
            .iter()
            .position(|node| node.kind == kind)
            .unwrap_or_else(|| panic!("missing {kind:?}"))
    }

    fn has_child(tree: &Tree, parent: usize, child: usize) -> bool {
        tree.nodes[parent].children.contains(&child)
    }

    fn has_descendant(tree: &Tree, parent: usize, child: usize) -> bool {
        tree.nodes[parent]
            .children
            .iter()
            .copied()
            .any(|idx| idx == child || has_descendant(tree, idx, child))
    }

    #[test]
    fn unknown_reference_link_is_preserved_as_link_node() {
        let source = "See [missing][nope].";
        let tree = parse(source);
        let link = first_node(&tree, NodeKind::Link);
        let label = tree.nodes[link]
            .children
            .iter()
            .copied()
            .find(|idx| tree.nodes[*idx].kind == NodeKind::LinkLabel)
            .expect("link label");

        assert_eq!(node_text(&tree, source, label), "missing");
    }

    #[test]
    fn inline_link_destination_uses_payload_match_not_label_match() {
        let source = "[foo](foo \"foo\")";
        let tree = parse(source);
        let destination = first_node(&tree, NodeKind::LinkDestination);
        let title = first_node(&tree, NodeKind::LinkTitle);

        assert_eq!(tree.nodes[destination].start_byte, 6);
        assert_eq!(node_text(&tree, source, destination), "foo");
        assert_eq!(tree.nodes[title].start_byte, 11);
        assert_eq!(node_text(&tree, source, title), "foo");
    }

    #[test]
    fn reference_link_destination_uses_reference_key_not_label() {
        let source = "[visible][ref]\n\n[ref]: docs.md\n";
        let tree = parse(source);
        let destination = first_node(&tree, NodeKind::LinkDestination);

        assert_eq!(node_text(&tree, source, destination), "ref");
    }

    #[test]
    fn reference_definition_destination_uses_payload_match_not_label_match() {
        let source = "[foo]: foo \"foo\"\n";
        let tree = parse(source);
        let destination = first_node(&tree, NodeKind::LinkDestination);
        let title = first_node(&tree, NodeKind::LinkTitle);

        assert_eq!(tree.nodes[destination].start_byte, 7);
        assert_eq!(node_text(&tree, source, destination), "foo");
        assert_eq!(tree.nodes[title].start_byte, 12);
        assert_eq!(node_text(&tree, source, title), "foo");
    }

    #[test]
    fn duplicate_reference_definitions_are_preserved() {
        let source = "[dup]: /one\n[dup]: /two\n";
        let tree = parse(source);
        let destinations = tree
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::LinkDestination)
            .map(|node| &source[node.start_byte..node.end_byte])
            .collect::<Vec<_>>();

        assert_eq!(destinations, vec!["/one", "/two"]);
    }

    #[test]
    fn reference_definition_inside_blockquote_keeps_container_parent() {
        let source = "> [ref]: /url\n";
        let tree = parse(source);
        let blockquote = first_node(&tree, NodeKind::BlockQuote);
        let refdef = first_node(&tree, NodeKind::LinkReferenceDefinition);

        assert!(has_child(&tree, blockquote, refdef));
    }

    #[test]
    fn reference_definition_inside_list_item_keeps_container_parent() {
        let source = "- [ref]: /url\n";
        let tree = parse(source);
        let item = first_node(&tree, NodeKind::ListItem { task: false });
        let refdef = first_node(&tree, NodeKind::LinkReferenceDefinition);

        assert!(has_descendant(&tree, item, refdef));
    }

    #[test]
    fn reference_definition_label_allows_escaped_closing_bracket() {
        let source = "[foo\\]]: /url\n";
        let tree = parse(source);
        let label = first_node(&tree, NodeKind::LinkLabel);
        let destination = first_node(&tree, NodeKind::LinkDestination);

        assert_eq!(node_text(&tree, source, label), "[foo\\]]");
        assert_eq!(node_text(&tree, source, destination), "/url");
    }

    #[test]
    fn footnote_definition_is_not_link_reference_definition() {
        let source = "[^1]: footnote text\n";
        let tree = parse(source);

        assert!(
            tree.nodes
                .iter()
                .all(|node| node.kind != NodeKind::LinkReferenceDefinition)
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.kind == NodeKind::FootnoteDefinition)
        );
    }

    #[test]
    fn inline_code_content_excludes_backtick_delimiters() {
        let source = "Use `foo` now.";
        let tree = parse(source);
        let content = first_node(&tree, NodeKind::InlineCodeContent);

        assert_eq!(node_text(&tree, source, content), "foo");
    }

    #[test]
    fn callout_marker_ranges_match_legacy_tokens() {
        let source = "> [!NOTE]\n> text\n";
        let tree = parse(source);
        let open = first_node(&tree, NodeKind::CalloutMarkerOpen);
        let callout_type = first_node(&tree, NodeKind::CalloutType);
        let close = first_node(&tree, NodeKind::CalloutMarkerClose);

        assert_eq!(node_text(&tree, source, open), "[!");
        assert_eq!(node_text(&tree, source, callout_type), "NOTE");
        assert_eq!(node_text(&tree, source, close), "]");
    }

    #[test]
    fn multiline_setext_heading_uses_setext_kind() {
        let source = "Foo\nbar\n---\n";
        let tree = parse(source);

        assert!(tree.nodes.iter().any(|node| matches!(
            node.kind,
            NodeKind::Heading {
                level: 2,
                style: HeadingStyle::Setext
            }
        )));
    }
}
