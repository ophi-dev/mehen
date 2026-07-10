// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Node kinds for the internal Markdown syntax tree.
//!
//! This is a hand-authored vocabulary that mirrors the `pulldown-cmark`
//! event surface the [`crate::syntax_tree`] builder consumes — not a
//! generated tree-sitter kind table. The crate migrated off tree-sitter to
//! `pulldown-cmark` (see `Cargo.toml`), so kinds are modeled directly as a
//! Rust enum: levels and flags that tree-sitter would encode as separate
//! node *types* (`atx_heading` vs `atx_heading2` …) are carried here as
//! *data* on a single variant instead.
//!
//! Only kinds the builder actually constructs are represented. Passes match
//! on [`NodeKind`] by value; heading level and the atx/setext distinction are
//! read from the variant payload rather than from a child marker or a named
//! field.

use pulldown_cmark::HeadingLevel;

/// The style of a heading marker.
///
/// `Atx` is `#`-prefixed; `Setext` is the underlined form (`===` / `---`).
/// Halstead treats each as a distinct operator (`heading_marker_op`), so the
/// flag is preserved as data on `Heading` / `HeadingMarker`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadingStyle {
    Atx,
    Setext,
}

/// A node kind in the compact Markdown tree.
///
/// Variants correspond to the blocks, inlines, and synthesized sub-spans the
/// builder emits. Numbered tree-sitter families (`atx_heading2..6`,
/// `list_item2..5`, `section1..6`) are folded into data-carrying variants:
/// the level/flag lives in the payload, and passes that previously matched
/// the whole family now match one variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    // ── Document root & structure ──────────────────────────────────────
    /// The document root.
    Document,
    /// A synthesized section wrapper introduced by heading nesting.
    ///
    /// `level` is the 1..=6 level of the heading that opens the section;
    /// heading-less top-level content is not wrapped in a `Section`.
    Section {
        level: u8,
    },

    // ── Headings ───────────────────────────────────────────────────────
    /// A heading. `level` is 1..=6; `style` distinguishes `#` from setext.
    Heading {
        level: u8,
        style: HeadingStyle,
    },
    /// The heading marker span (`#`/`##`/… or the `===`/`---` underline).
    ///
    /// Kept distinct from the heading because Halstead counts the marker as
    /// a per-level operator.
    HeadingMarker {
        level: u8,
        style: HeadingStyle,
    },
    /// The inline-content span of a heading.
    HeadingContent,

    // ── Block containers ───────────────────────────────────────────────
    Paragraph,
    BlockQuote,
    /// A blockquote carrying a callout marker (`> [!NOTE]`).
    Callout,
    CalloutMarkerOpen,
    CalloutType,
    CalloutMarkerClose,
    BlockQuoteMarker,
    List,
    /// A list item. `task` marks a `- [ ]`/`- [x]` checklist item.
    ListItem {
        task: bool,
    },
    ListItemContent {
        task: bool,
    },
    ListMarker,
    TaskListMarkerChecked,
    TaskListMarkerUnchecked,

    // ── Code ───────────────────────────────────────────────────────────
    FencedCodeBlock,
    IndentedCodeBlock,
    CodeFenceContent,
    IndentedChunk,
    InfoString,
    Language,
    InlineCode,
    InlineCodeContent,

    // ── Math ───────────────────────────────────────────────────────────
    MathBlock,
    MathBlockDelimiter,
    MathBlockContent,
    MathInline,
    MathInlineContent,

    // ── Tables ─────────────────────────────────────────────────────────
    PipeTable,
    PipeTableHeader,
    PipeTableRow,
    PipeTableCell,
    PipeTableDelimiterRow,
    PipeTableDelimiterCell,
    PipeTableAlignLeft,
    PipeTableAlignRight,

    // ── Links, images, references ──────────────────────────────────────
    Link,
    Image,
    Autolink,
    Uri,
    Email,
    LinkLabel,
    LinkDestination,
    LinkTitle,
    LinkReferenceDefinition,
    FootnoteDefinition,
    FootnoteLabel,
    FootnoteReference,
    FootnoteReferenceLabel,

    // ── HTML ───────────────────────────────────────────────────────────
    HtmlBlock,
    HtmlInline,
    HtmlOpenTag,
    HtmlCloseTag,
    HtmlComment,
    HtmlCdata,
    HtmlProcessingInstruction,
    HtmlDeclaration,

    // ── Inline emphasis ────────────────────────────────────────────────
    Emphasis,
    Strong,
    Strikethrough,

    // ── Front matter ───────────────────────────────────────────────────
    MinusMetadata,
    PlusMetadata,

    // ── Breaks & tokens ────────────────────────────────────────────────
    Newline,
    ThematicBreak,
    /// A word-shaped token classified by shape (see [`Self::WordToken`] etc.).
    WordToken,
    NumericToken,
    PathLikeToken,
    IdentifierLikeToken,
    /// Sentence-terminating punctuation (`.`/`?`/`!`/`。`/`…`).
    Terminator,
    /// Clause-separating punctuation (`,`/`;`/`:`).
    Separator,
    /// Bracketing punctuation.
    Bracket,
    /// Operator-like punctuation.
    OperatorLike,
}

impl NodeKind {
    /// The heading level (1..=6) when this kind is a [`NodeKind::Heading`].
    pub(crate) fn heading_level(self) -> Option<u8> {
        match self {
            NodeKind::Heading { level, .. } => Some(level),
            _ => None,
        }
    }

    /// Whether this kind is a heading (of any level or style).
    pub(crate) fn is_heading(self) -> bool {
        matches!(self, NodeKind::Heading { .. })
    }

    /// Whether this kind is a list item (task or plain).
    pub(crate) fn is_list_item(self) -> bool {
        matches!(self, NodeKind::ListItem { .. })
    }
}

/// Converts a `pulldown-cmark` [`HeadingLevel`] to a 1..=6 level.
pub(crate) fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
