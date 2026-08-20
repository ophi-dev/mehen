// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! mago-syntax-based walker that produces a populated `MetricSpace`.
//!
//! Mirrors the per-space `State` accumulator pattern used by
//! `mehen-rust` (`crates/mehen-rust/src/walker.rs`),
//! `mehen-python` (`crates/mehen-python/src/walker.rs`) and
//! `mehen-typescript` (`crates/mehen-typescript/src/walker.rs`):
//!
//! - one `State` for the unit, plus one for every opened
//!   function / closure / arrow function / class / interface /
//!   trait / enum / anonymous-class space,
//! - finalize on close, fold child stats into parent,
//! - drive the walk through Mago's `Walker` trait so node
//!   enter/leave callbacks are typed (`walk_in_class`,
//!   `walk_in_method`, `walk_in_if`, `walk_in_match`, …).
//!
//! Mago's Walker is the same one used by `mago-collector` for
//! pragma-scope attachment in Mago's own lint pipeline; here we
//! reuse it to drive metric accumulation. We do NOT depend on
//! `mago-collector` itself — it's an issue/diagnostic collector
//! whose feature set (suppression pragmas, issue codes, etc.) is
//! orthogonal to metric computation.
//!
//! PHP-specific design decisions are documented in
//! `docs/php-mago-syntax-spec.md`. Highlights:
//!
//! - **`elseif` and `else if`**: both contribute a flat `+1`
//!   cognitive (no extra nesting). Mago surfaces these as distinct
//!   AST nodes (`IfStatementBodyElseIfClause`,
//!   `IfStatementBodyElseClause`), so the flattening logic the
//!   legacy walker did via `is_else_if` lookahead now becomes a
//!   trivial AST-level callback.
//! - **`match` arms**: every arm is a cyclomatic decision; the
//!   `match` expression itself opens a cognitive nesting frame.
//!   The `default` arm contributes one ABC condition per Fitzpatrick
//!   ABC, mirroring how `else_clause` is treated.
//! - **Promoted constructor properties**: `__construct(public int $id)`
//!   really does declare a class property. We attribute them to the
//!   enclosing class space's NPA counters (Mago surfaces this
//!   directly via `FunctionLikeParameter::is_promoted_property()`).
//! - **PHP keyword case-insensitivity**: visibility modifiers
//!   (`public` / `protected` / `private`) are typed via Mago's
//!   `Modifier` enum, so the case-insensitive scan the legacy walker
//!   did over the source text is now a typed enum match — case
//!   handling falls out automatically.
//! - **`exit` / `die`**: counted as function exits (`nexit`),
//!   matching legacy. Mago models them as `Construct::Exit` /
//!   `Construct::Die`, which the legacy `ExitStatement` enum
//!   variant did not distinguish.

use mago_database::file::FileId;
use mago_span::{HasSpan, Span};
use mago_syntax::cst::{
    AnonymousClass, ArrowFunction, Assignment, Binary, BinaryOperator, Call, Class, Closure,
    Conditional, Construct, DoWhile, Enum, EnumCase, ExpressionStatement, For, Foreach, Function,
    If, IfBody, Instantiation, Interface, Match, Method, MethodBody, Modifier, NullSafeMethodCall,
    Program, Property, Return, Switch, Throw, Trait, Try, UnaryPrefix, UnaryPrefixOperator, While,
    Yield,
};
use mago_syntax::lexer::Lexer;
use mago_syntax::settings::LexerSettings;
use mago_syntax::token::TokenKind;
use mago_syntax::walker::Walker;
use mago_syntax_core::input::Input;

use mehen_core::{LineIndex, MetricSpace, SourceSpan, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricEvidence, MetricTreeBuilder,
    SpaceRangeTracker, State, apply_state_to, finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

/// Crate-internal entry point — drive the walker over a parsed
/// `Program`. Contribution evidence is recorded into the caller-owned
/// `evidence` sink (plan §5.4).
pub(crate) fn walk_program<'arena>(
    program: &Program<'arena>,
    source: &str,
    line_index: &LineIndex,
    evidence: &mut MetricEvidence,
) -> MetricSpace {
    let unit_span = SourceSpan {
        start_byte: 0,
        end_byte: clamp_offset(source.len()),
        start_line: 1,
        end_line: line_index.line_count(),
    };

    let mut visitor = Visitor::new(source, line_index, unit_span, evidence);

    let walker = MehenPhpWalker;
    walker.walk_program(program, &mut visitor);

    // Comments / docblocks live in `program.trivia`; record them
    // *after* the AST walk so the `SpaceRangeTracker` has populated
    // every opened space's byte range and each comment routes to its
    // enclosing scope's `loc.cloc` (PR #95 discussion_r3265962147 —
    // routing comments before the walk left every per-space `cloc`
    // at zero).
    visitor.observe_trivia(&program.trivia);

    visitor.finish()
}

#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    /// Structural nesting depth (incremented by `if`, `for`, `while`,
    /// `do`, `switch`, `match`, `try`, `catch`, `conditional`).
    nesting: u32,
    /// Function-call depth — incremented when we enter a nested
    /// function/method (not a top-level one). Mirrors legacy
    /// `count_specific_ancestors(FunctionDefinition | MethodDeclaration)`.
    depth: u32,
    /// Lambda depth — incremented inside an `AnonymousFunction` or
    /// `ArrowFunction`.
    lambda: u32,
}

struct Visitor<'a> {
    source: &'a str,
    line_index: &'a LineIndex,
    tree: MetricTreeBuilder,
    /// Per-space accumulator stack — index 0 is the unit.
    stack: Vec<State>,
    /// Parallel to `stack`: the SpaceKind of each open frame.
    kinds: Vec<SpaceKind>,
    /// Cognitive context for the currently-walked subtree. Saved on
    /// nesting-bumping / function-entry events and restored on leave.
    cognitive: CognitiveContext,
    /// Stack of cognitive-context savepoints, parallel to `stack`,
    /// so an Enter/Leave pair can roll back the nesting it added.
    saved_cognitive: Vec<CognitiveContext>,
    /// Whether the *next* `walk_in_if` should be treated as the
    /// inner `if` of an `else if` (set when leaving an else clause
    /// whose statement is an `If` – mago does NOT have a dedicated
    /// `ElseIf` node for the spaced form).
    suppress_next_if_nesting: bool,
    /// Routes Halstead tokens emitted by the post-AST sweep to the
    /// deepest enclosing function/class/closure space so per-space
    /// JSON entries are non-zero. PR #95 discussion_r3265658502
    /// flagged the same gap on the Python walker; the PHP walker had
    /// the same `stack[0]`-only behaviour.
    halstead_routing: SpaceRangeTracker,
    /// Contribution-evidence sink (plan §5.4). Recording happens next
    /// to each stat increment, through [`Visitor::record_evidence`].
    evidence: &'a mut MetricEvidence,
}

impl<'a> Visitor<'a> {
    fn new(
        source: &'a str,
        line_index: &'a LineIndex,
        unit_span: SourceSpan,
        evidence: &'a mut MetricEvidence,
    ) -> Self {
        let mut state = State::new();
        state.loc.set_span(
            unit_span.start_line.saturating_sub(1),
            unit_span.end_line.saturating_sub(1),
            true,
        );
        Self {
            source,
            line_index,
            tree: MetricTreeBuilder::new(unit_span),
            stack: vec![state],
            kinds: vec![SpaceKind::Unit],
            cognitive: CognitiveContext::default(),
            saved_cognitive: Vec::new(),
            suppress_next_if_nesting: false,
            halstead_routing: SpaceRangeTracker::new(),
            evidence,
        }
    }

    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    /// Record contribution evidence for the construct at `span`. The
    /// span conversion only runs when the sink is enabled, so walker
    /// callbacks can call this unconditionally next to each stat
    /// increment (mirrors `mehen_tree_sitter::WalkerCtx::record_evidence`).
    #[inline]
    fn record_evidence<F>(&mut self, span: Span, record: F)
    where
        F: FnOnce(&mut MetricEvidence, SourceSpan),
    {
        if self.evidence.is_enabled() {
            let source_span = self.span_to_source(span);
            record(self.evidence, source_span);
        }
    }

    fn observe_trivia(
        &mut self,
        trivia: &mago_syntax::cst::Sequence<'_, mago_syntax::cst::Trivia<'_>>,
    ) {
        // Route each comment to the deepest enclosing scope so a
        // `// foo` inside a method body lands on that method's
        // `loc.cloc` rather than the unit's. Lines outside every
        // recorded scope (file-level docblocks, license headers)
        // fall through to the unit's LocStats.
        for comment in trivia.iter() {
            if !comment.kind.is_comment() {
                continue;
            }
            let span = comment.span;
            let start_row = self.line_at(span.start.offset).saturating_sub(1);
            let end_row = self.line_at(span.end.offset).saturating_sub(1);
            self.halstead_routing.observe_comment(
                span.start.offset,
                span.end.offset,
                &mut self.stack[0].loc,
                start_row,
                end_row,
            );
        }
    }

    fn line_at(&self, offset: u32) -> u32 {
        self.line_index.line_at(offset)
    }

    fn span_to_source(&self, span: Span) -> SourceSpan {
        SourceSpan {
            start_byte: span.start.offset,
            end_byte: span.end.offset,
            start_line: self.line_at(span.start.offset),
            end_line: self.line_at(span.end.offset),
        }
    }

    fn finish(mut self) -> MetricSpace {
        // Final ploc/lloc accounting from a single source-text scan
        // (mirrors the rust analyzer's token sweep — done once at
        // the unit level so we don't re-walk the whole arena to
        // derive LOC).
        self.scan_source_loc();
        self.emit_halstead_from_tokens();

        let mut unit_state = self.stack.pop().expect("walker stack underflow");
        finalize_state(&mut unit_state);
        // Route post-AST tokens (Halstead operator/operand, PLOC code
        // lines, comment lines) to nested spaces; see
        // [`SpaceRangeTracker`].
        let mut unit_halstead = std::mem::take(&mut unit_state.halstead);
        let mut unit_loc = std::mem::take(&mut unit_state.loc);
        let mut tree = self.tree.finish();
        self.halstead_routing
            .finalize_into_tree(&mut tree, &mut unit_halstead, &mut unit_loc);
        unit_state.halstead = unit_halstead;
        unit_state.loc = unit_loc;
        apply_state_to(unit_state, &mut tree.metrics);
        tree
    }

    /// Re-lex the source via Mago's `Lexer` and emit Halstead
    /// operator/operand events. Each event is routed to the deepest
    /// enclosing scope via [`SpaceRangeTracker`] so per-space JSON
    /// entries are non-zero; tokens that fall outside every recorded
    /// scope go into the unit `HalsteadBuilder`.
    fn emit_halstead_from_tokens(&mut self) {
        let input = Input::new(FileId::zero(), self.source.as_bytes());
        let mut lexer = Lexer::new(input, LexerSettings::default());
        while let Some(result) = lexer.advance() {
            let token = match result {
                Ok(tok) => tok,
                // Treat lex errors as recoverable — skip the byte
                // and continue. The parse-error diagnostic was
                // already attached upstream via parser errors.
                Err(_) => continue,
            };
            // Mago tokens carry only `start: Position` and the literal
            // `value: &[u8]` (raw source bytes); the end offset is
            // start + value length, which is what `Position`
            // arithmetic does on every other code path in mago-syntax.
            let s = token.start.offset;
            let e = s + token.value.len() as u32;
            match classify_token(token.kind) {
                TokenClass::Operator(kind) => {
                    self.halstead_routing.observe_operator(
                        s,
                        e,
                        &mut self.stack[0].halstead,
                        HalsteadOperator {
                            kind: SmolStr::new(kind),
                            text: None,
                        },
                    );
                }
                TokenClass::Operand(kind) => {
                    let text = String::from_utf8_lossy(token.value);
                    self.halstead_routing.observe_operand(
                        s,
                        e,
                        &mut self.stack[0].halstead,
                        HalsteadOperand {
                            kind: SmolStr::new(kind),
                            text: Some(SmolStr::new(text.as_ref())),
                        },
                    );
                }
                TokenClass::Skip => {}
            }
        }
    }

    fn scan_source_loc(&mut self) {
        // Per-line PLOC accounting: any line whose first non-whitespace
        // token is a code token contributes a code line. Comments are
        // already handled in `observe_trivia`; here we just need PLOC
        // / blank tracking. Halstead / LLOC are recorded at AST nodes.
        // Each line is routed by its byte range so a code line inside
        // a function body lands on that function's `loc.ploc` instead
        // of the unit's.
        let total_len = self.source.len() as u32;
        let mut byte_offset: u32 = 0;
        for (idx, line) in self.source.lines().enumerate() {
            let line_start = byte_offset;
            // `lines()` strips the line terminator; advance the cursor
            // by the line's byte length plus the consumed `\n` (or
            // `\r\n`). This keeps `byte_offset` valid for the next
            // iteration regardless of which terminator the source
            // uses.
            byte_offset = byte_offset.saturating_add(line.len() as u32);
            let line_end = byte_offset.min(total_len);
            // Step past `\n` (and an optional preceding `\r`) so the
            // next iteration's `line_start` is correct.
            if (byte_offset as usize) < self.source.len() {
                let after_line = self.source.as_bytes().get(byte_offset as usize).copied();
                if after_line == Some(b'\r') {
                    byte_offset = byte_offset.saturating_add(1);
                    if self.source.as_bytes().get(byte_offset as usize).copied() == Some(b'\n') {
                        byte_offset = byte_offset.saturating_add(1);
                    }
                } else if after_line == Some(b'\n') {
                    byte_offset = byte_offset.saturating_add(1);
                }
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Skip lines that are only PHP open/close tags or only a
            // comment — those don't count as code lines.
            if trimmed == "<?php" || trimmed == "<?" || trimmed == "?>" {
                continue;
            }
            if trimmed.starts_with("//")
                || trimmed.starts_with('#')
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
            {
                continue;
            }
            self.halstead_routing.observe_code_line(
                line_start,
                line_end,
                &mut self.stack[0].loc,
                idx as u32,
            );
        }
    }

    fn open_space(&mut self, kind: SpaceKind, span: Span, name: Option<String>) {
        let mut child = State::new();
        let start_row = self.line_at(span.start.offset).saturating_sub(1);
        let end_row = self.line_at(span.end.offset).saturating_sub(1);
        child.loc.set_span(start_row, end_row, false);

        match kind {
            SpaceKind::Function => {
                child.nom.record_function();
            }
            SpaceKind::Closure => {
                child.nom.record_closure();
            }
            SpaceKind::Class => {
                child.npa.record_class_like();
                child.npm.record_class_like();
                child.wmc.record_class_like();
            }
            SpaceKind::Interface | SpaceKind::Trait | SpaceKind::Enum => {
                child.npa.record_class_like();
                child.npm.record_class_like();
            }
            _ => {}
        }
        let source_span = self.span_to_source(span);
        let space_id = self.tree.open(kind.clone(), source_span, name);
        // Record byte range for the post-AST Halstead routing pass.
        self.halstead_routing
            .record_open(space_id, span.start.offset, span.end.offset);
        self.stack.push(child);
        self.kinds.push(kind);
    }

    fn close_space(&mut self) {
        let closed_kind = self.kinds.pop().expect("kinds underflow");
        let mut state = self.stack.pop().expect("stack underflow");
        if matches!(closed_kind, SpaceKind::Function) {
            state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
        }
        finalize_state(&mut state);
        // Stash MI inputs (LOC + cyclomatic) for the post-AST Halstead
        // overlay before they get consumed by `apply_state_to`.
        if let Some(space_id) = self.tree.current_id() {
            self.halstead_routing
                .record_close(space_id, &state.loc, &state.cyclomatic);
        }
        apply_state_to(state.clone(), self.tree.metrics_mut());
        if let Some(parent) = self.stack.last_mut() {
            let parent_kind = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
            merge_child_into_parent(parent, &state);
            if matches!(closed_kind, SpaceKind::Function) {
                let container = match parent_kind {
                    SpaceKind::Class => ContainerKind::Class,
                    SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
                    _ => ContainerKind::Other,
                };
                state.wmc.finalize_method_into(container, &mut parent.wmc);
            }
        }
        self.tree.close();
    }

    fn save_cognitive(&mut self) {
        self.saved_cognitive.push(self.cognitive);
    }

    fn restore_cognitive(&mut self) {
        if let Some(saved) = self.saved_cognitive.pop() {
            self.cognitive = saved;
        }
    }

    /// Cognitive: function entry. Reset nesting/lambda; bump depth
    /// when nested inside another function/method.
    fn enter_function_like(&mut self, kind: SpaceKind) {
        self.save_cognitive();
        let nested = self
            .kinds
            .iter()
            .skip(1)
            .any(|k| matches!(k, SpaceKind::Function));
        let mut ctx = self.cognitive;
        ctx.nesting = 0;
        if matches!(kind, SpaceKind::Closure) {
            ctx.lambda = ctx.lambda.saturating_add(1);
        } else {
            ctx.lambda = 0;
        }
        if nested {
            ctx.depth = ctx.depth.saturating_add(1);
        }
        self.cognitive = ctx;
    }

    fn record_method(&mut self, method: &Method<'_>) {
        // NPM bookkeeping happens on the *enclosing* class-like state,
        // not the method's own state. Mirrors the Rust phase.
        let enclosing = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
        let container = match enclosing {
            SpaceKind::Class => ContainerKind::Class,
            SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
            _ => return,
        };
        let public = match enclosing {
            // PHP interface methods are implicitly public (the interface
            // contract; visibility modifiers may not appear).
            SpaceKind::Interface => true,
            _ => php_modifiers_are_public(&method.modifiers),
        };
        self.current().npm.record_method(container, public);
        // Only public members are evidenced — the headline NPM metric
        // counts public methods only.
        if public {
            self.record_evidence(method.span(), |e, s| e.public_method(s, "method"));
        }
    }

    fn record_property(&mut self, property: &Property<'_>) {
        let enclosing = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
        let container = match enclosing {
            SpaceKind::Class | SpaceKind::Trait | SpaceKind::Enum => ContainerKind::Class,
            SpaceKind::Interface => ContainerKind::Interface,
            _ => return,
        };
        // Each property declaration may declare multiple property
        // items: `public $a, $b;` declares two attributes — record
        // one entry per item.
        let (modifiers, items) = match property {
            Property::Plain(p) => (&p.modifiers, &p.items),
            Property::Hooked(_h) => {
                // Hooked properties always declare a single property.
                // Treat the modifier list as the visibility source.
                return;
            }
        };
        let public = php_modifiers_are_public(modifiers);
        for item in items.iter() {
            self.current().npa.record_attribute(container, public);
            // Only public members are evidenced — the headline NPA
            // metric counts public attributes only.
            if public {
                self.record_evidence(item.span(), |e, s| e.public_attribute(s, "property"));
            }
        }
    }
}

fn clamp_offset(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[derive(Clone, Copy)]
enum TokenClass {
    Operator(&'static str),
    Operand(&'static str),
    Skip,
}

/// Classify a Mago `TokenKind` for Halstead. Each operator token
/// gets its own distinct `kind` string so `n1` reflects the true
/// number of unique operators. Closing punctuation pairs with its
/// opener (classical Halstead convention) and is skipped, matching
/// the `mehen-rust` walker.
///
/// PHP-specific note: the legacy tree-sitter walker treated PHP
/// keywords as Halstead operators (it's a long list, including
/// modifiers and `array`/`list`/`callable`). We preserve that
/// behavior — the keyword IS the construct's operator.
fn classify_token(kind: TokenKind) -> TokenClass {
    use TokenClass::*;
    use TokenKind as T;
    match kind {
        // ---------- SKIPPED (whitespace, trivia, string interior) ----------
        T::Whitespace
        | T::SingleLineComment
        | T::HashComment
        | T::MultiLineComment
        | T::DocBlockComment
        | T::InlineText
        | T::InlineShebang
        | T::OpenTag
        | T::EchoTag
        | T::ShortOpenTag
        | T::CloseTag
        // String interior — already counted via `LiteralString`.
        | T::StringPart
        | T::DoubleQuote
        | T::Backtick
        | T::DocumentStart(_)
        | T::DocumentEnd
        | T::PartialLiteralString => Skip,
        // Closing punctuation: pairs with the opener; skip to avoid
        // double-counting.
        T::RightParenthesis | T::RightBrace | T::RightBracket => Skip,

        // ---------- PUNCTUATION (operators, distinct kinds) ----------
        T::LeftParenthesis => Operator("("),
        T::LeftBrace => Operator("{"),
        T::LeftBracket => Operator("["),
        T::Comma => Operator(","),
        T::Semicolon => Operator(";"),
        T::Colon => Operator(":"),
        T::ColonColon => Operator("::"),
        T::Dot => Operator("."),
        T::DotDotDot => Operator("..."),
        T::MinusGreaterThan => Operator("->"),
        T::QuestionMinusGreaterThan => Operator("?->"),
        T::EqualGreaterThan => Operator("=>"),
        T::HashLeftBracket => Operator("#["),
        T::At => Operator("@"),
        T::NamespaceSeparator => Operator("\\"),
        T::DollarLeftBrace => Operator("${"),
        T::Dollar => Operator("$"),

        // ---------- ASSIGNMENT FAMILY ----------
        T::Equal => Operator("="),
        T::PlusEqual => Operator("+="),
        T::MinusEqual => Operator("-="),
        T::AsteriskEqual => Operator("*="),
        T::AsteriskAsteriskEqual => Operator("**="),
        T::SlashEqual => Operator("/="),
        T::PercentEqual => Operator("%="),
        T::DotEqual => Operator(".="),
        T::LeftShiftEqual => Operator("<<="),
        T::RightShiftEqual => Operator(">>="),
        T::AmpersandEqual => Operator("&="),
        T::CaretEqual => Operator("^="),
        T::PipeEqual => Operator("|="),
        T::QuestionQuestionEqual => Operator("??="),
        T::AmpersandAmpersandEqual => Operator("&&="),

        // ---------- ARITHMETIC / BITWISE / UNARY / NULL-COALESCE ----------
        T::Plus => Operator("+"),
        T::Minus => Operator("-"),
        T::Asterisk => Operator("*"),
        T::AsteriskAsterisk => Operator("**"),
        T::Slash => Operator("/"),
        T::Percent => Operator("%"),
        T::PlusPlus => Operator("++"),
        T::MinusMinus => Operator("--"),
        T::Tilde => Operator("~"),
        T::Bang => Operator("!"),
        T::AmpersandAmpersand => Operator("&&"),
        T::PipePipe => Operator("||"),
        T::QuestionQuestion => Operator("??"),
        T::Question => Operator("?"),
        T::Ampersand => Operator("&"),
        T::Pipe => Operator("|"),
        T::Caret => Operator("^"),
        T::LeftShift => Operator("<<"),
        T::RightShift => Operator(">>"),

        // ---------- COMPARISON ----------
        T::EqualEqual => Operator("=="),
        T::EqualEqualEqual => Operator("==="),
        T::BangEqual => Operator("!="),
        T::BangEqualEqual => Operator("!=="),
        T::LessThanGreaterThan => Operator("<>"),
        T::LessThan => Operator("<"),
        T::GreaterThan => Operator(">"),
        T::LessThanEqual => Operator("<="),
        T::GreaterThanEqual => Operator(">="),
        T::LessThanEqualGreaterThan => Operator("<=>"),
        // Pipe operator (PHP 8.5).
        T::PipeGreaterThan => Operator("|>"),

        // ---------- TYPE CASTS (each cast is its own operator) ----------
        T::ArrayCast => Operator("(array)"),
        T::BoolCast => Operator("(bool)"),
        T::BooleanCast => Operator("(boolean)"),
        T::DoubleCast => Operator("(double)"),
        T::RealCast => Operator("(real)"),
        T::FloatCast => Operator("(float)"),
        T::IntCast => Operator("(int)"),
        T::IntegerCast => Operator("(integer)"),
        T::ObjectCast => Operator("(object)"),
        T::UnsetCast => Operator("(unset)"),
        T::StringCast => Operator("(string)"),
        T::BinaryCast => Operator("(binary)"),
        T::VoidCast => Operator("(void)"),

        // ---------- KEYWORDS (each keyword is its own operator) ----------
        T::Function => Operator("function"),
        T::Fn => Operator("fn"),
        T::Class => Operator("class"),
        T::Interface => Operator("interface"),
        T::Trait => Operator("trait"),
        T::Enum => Operator("enum"),
        T::Namespace => Operator("namespace"),
        T::Use => Operator("use"),
        T::As => Operator("as"),
        T::Insteadof => Operator("insteadof"),
        T::Const => Operator("const"),
        T::Static => Operator("static"),
        T::Var => Operator("var"),
        T::Public => Operator("public"),
        T::PublicSet => Operator("public(set)"),
        T::Protected => Operator("protected"),
        T::ProtectedSet => Operator("protected(set)"),
        T::Private => Operator("private"),
        T::PrivateSet => Operator("private(set)"),
        T::Final => Operator("final"),
        T::Abstract => Operator("abstract"),
        T::Readonly => Operator("readonly"),
        T::Extends => Operator("extends"),
        T::Implements => Operator("implements"),
        T::New => Operator("new"),
        T::Clone => Operator("clone"),
        T::Instanceof => Operator("instanceof"),
        T::If => Operator("if"),
        T::Else => Operator("else"),
        T::ElseIf => Operator("elseif"),
        T::EndIf => Operator("endif"),
        T::Switch => Operator("switch"),
        T::Case => Operator("case"),
        T::Default => Operator("default"),
        T::EndSwitch => Operator("endswitch"),
        T::Match => Operator("match"),
        T::While => Operator("while"),
        T::EndWhile => Operator("endwhile"),
        T::Do => Operator("do"),
        T::For => Operator("for"),
        T::EndFor => Operator("endfor"),
        T::Foreach => Operator("foreach"),
        T::EndForeach => Operator("endforeach"),
        T::Continue => Operator("continue"),
        T::Break => Operator("break"),
        T::Return => Operator("return"),
        T::Throw => Operator("throw"),
        T::Try => Operator("try"),
        T::Catch => Operator("catch"),
        T::Finally => Operator("finally"),
        T::Goto => Operator("goto"),
        T::Yield => Operator("yield"),
        T::From => Operator("from"),
        T::Echo => Operator("echo"),
        T::Print => Operator("print"),
        T::Exit => Operator("exit"),
        T::Die => Operator("die"),
        T::Unset => Operator("unset"),
        T::Isset => Operator("isset"),
        T::Empty => Operator("empty"),
        T::Eval => Operator("eval"),
        T::List => Operator("list"),
        T::Array => Operator("array"),
        T::Include => Operator("include"),
        T::IncludeOnce => Operator("include_once"),
        T::Require => Operator("require"),
        T::RequireOnce => Operator("require_once"),
        T::And => Operator("and"),
        T::Or => Operator("or"),
        T::Xor => Operator("xor"),
        T::Declare => Operator("declare"),
        T::EndDeclare => Operator("enddeclare"),
        T::Global => Operator("global"),
        T::HaltCompiler => Operator("__halt_compiler"),

        // ---------- IDENTIFIERS / NAMES (operands) ----------
        T::Identifier
        | T::QualifiedIdentifier
        | T::FullyQualifiedIdentifier => Operand("Identifier"),
        T::Variable => Operand("Variable"),
        T::Self_ => Operand("self"),
        T::Parent => Operand("parent"),
        // `callable` is a type hint used as a name in argument lists.
        T::Callable => Operand("Identifier"),

        // ---------- LITERALS (operands) ----------
        T::LiteralInteger => Operand("Integer"),
        T::LiteralFloat => Operand("Float"),
        T::LiteralString => Operand("String"),
        T::True => Operand("True"),
        T::False => Operand("False"),
        T::Null => Operand("Null"),

        // ---------- STRING SUBSCRIPT / INTERPOLATION OFFSETS (operands) ----------
        // `$a[5]` numeric offset, `$a[bar]` bareword offset, and the
        // `foo` name in `${foo}` interpolation. Mago 1.42 split these
        // out of the generic identifier/literal tokens; each names a
        // value, so they classify as operands alongside literals.
        T::OffsetNumber => Operand("Integer"),
        T::OffsetString => Operand("String"),
        T::StringVariableName => Operand("Variable"),

        // ---------- MAGIC CONSTANTS (operands) ----------
        T::ClassConstant
        | T::TraitConstant
        | T::FunctionConstant
        | T::MethodConstant
        | T::LineConstant
        | T::FileConstant
        | T::DirConstant
        | T::NamespaceConstant
        | T::PropertyConstant => Operand("MagicConstant"),
    }
}

/// PHP visibility default is *public* when no modifier appears.
/// PHP keywords are case-insensitive — but Mago has already
/// normalized the modifier into a typed `Modifier` enum, so the
/// scan is a clean enum match (no string comparison needed).
fn php_modifiers_are_public(
    modifiers: &mago_syntax::cst::sequence::Sequence<'_, Modifier<'_>>,
) -> bool {
    !modifiers.iter().any(|m| {
        matches!(
            m,
            Modifier::Private(_)
                | Modifier::Protected(_)
                | Modifier::PrivateSet(_)
                | Modifier::ProtectedSet(_)
        )
    })
}

/// Mago 1.28 switched identifier and token `value` fields from
/// `&str` to `&[u8]` (PHP source bytes are not guaranteed UTF-8 in
/// the literal-string lexer state). Identifiers themselves must be
/// PHP-valid (ASCII letters / digits / `_` / `\\`), so a lossy
/// conversion is exact in practice; for arbitrary token bytes we
/// accept the U+FFFD replacement on the rare invalid sequence
/// rather than failing the analysis.
fn bytes_to_string(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

/// PHP method names are case-insensitive — `__construct`, `__CONSTRUCT`,
/// and `__Construct` are all the constructor.
fn is_constructor(name: &str) -> bool {
    name.eq_ignore_ascii_case("__construct")
}

/// Stable reason-detail string for a comparison operator (ABC.C
/// evidence). Matches the PHP source token spelling, same convention
/// as the tree-sitter walkers' operator-token kinds.
fn comparison_op_str(op: &BinaryOperator<'_>) -> &'static str {
    match op {
        BinaryOperator::Equal(_) => "==",
        BinaryOperator::NotEqual(_) => "!=",
        BinaryOperator::Identical(_) => "===",
        BinaryOperator::NotIdentical(_) => "!==",
        BinaryOperator::AngledNotEqual(_) => "<>",
        BinaryOperator::LessThan(_) => "<",
        BinaryOperator::LessThanOrEqual(_) => "<=",
        BinaryOperator::GreaterThan(_) => ">",
        BinaryOperator::GreaterThanOrEqual(_) => ">=",
        BinaryOperator::Spaceship(_) => "<=>",
        // Callers only pass the comparison variants above.
        _ => "comparison",
    }
}

// =====================================================================
// Walker implementation
// =====================================================================

/// Marker type — Mago's `Walker` trait is `&self`, so the visitor's
/// state lives in the `Context` (`Visitor`).
struct MehenPhpWalker;

impl<'arena> Walker<'_, 'arena, Visitor<'_>> for MehenPhpWalker {
    // -----------------------------------------------------------------
    // Function-like spaces
    // -----------------------------------------------------------------

    fn walk_in_function(&self, function: &Function<'arena>, ctx: &mut Visitor<'_>) {
        // Function declarations are themselves an LLOC line (legacy
        // `FunctionDefinition` rule). Record on the enclosing space
        // before we open the function's own state frame.
        ctx.current().loc.observe_lloc();

        ctx.enter_function_like(SpaceKind::Function);
        let name = bytes_to_string(function.name.value);
        ctx.open_space(SpaceKind::Function, function.span(), Some(name));

        let argc = function.parameter_list.parameters.iter().count() as u32;
        ctx.current().nargs.record_function_args(argc);
        ctx.record_evidence(function.span(), |e, s| {
            e.function(s, "function");
            e.function_args(s, argc, "function");
        });
    }

    fn walk_out_function(&self, _function: &Function<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
        ctx.restore_cognitive();
    }

    fn walk_in_method(&self, method: &Method<'arena>, ctx: &mut Visitor<'_>) {
        // Method declarations contribute one LLOC line on the
        // enclosing class-like space (legacy `MethodDeclaration` rule).
        ctx.current().loc.observe_lloc();

        ctx.record_method(method);

        let name = bytes_to_string(method.name.value);
        let constructor = is_constructor(&name);

        // Attribute promoted constructor properties NOW (before we
        // open the method's child space) so they're recorded on the
        // enclosing class state, not the method's own state — that
        // mirrors the legacy rule that promoted properties belong to
        // the *class* npa, not the method.
        if constructor {
            for param in method.parameter_list.parameters.iter() {
                if param.is_promoted_property() {
                    let public = php_modifiers_are_public(&param.modifiers);
                    let container = ContainerKind::Class;
                    ctx.current().npa.record_attribute(container, public);
                    // Only public members are evidenced (headline NPA
                    // counts public attributes only).
                    if public {
                        ctx.record_evidence(param.span(), |e, s| {
                            e.public_attribute(s, "promoted_property");
                        });
                    }
                }
            }
        }

        // Abstract methods (`abstract public function f();`) are not a
        // function space — no body to analyze. They still count for
        // NPM (already recorded above), but skip the State frame.
        if matches!(method.body, MethodBody::Abstract(_)) {
            return;
        }

        ctx.enter_function_like(SpaceKind::Function);
        ctx.open_space(SpaceKind::Function, method.span(), Some(name));

        // Method NArgs counts only "real" parameters: a promoted
        // property counts both as an attribute (above) AND as a
        // parameter (it really IS a parameter at the call site).
        let argc = method.parameter_list.parameters.iter().count() as u32;
        ctx.current().nargs.record_function_args(argc);
        ctx.record_evidence(method.span(), |e, s| {
            e.function(s, "method");
            e.function_args(s, argc, "method");
        });
    }

    fn walk_out_method(&self, method: &Method<'arena>, ctx: &mut Visitor<'_>) {
        if matches!(method.body, MethodBody::Abstract(_)) {
            return;
        }
        ctx.close_space();
        ctx.restore_cognitive();
    }

    fn walk_in_closure(&self, closure: &Closure<'arena>, ctx: &mut Visitor<'_>) {
        ctx.enter_function_like(SpaceKind::Closure);
        ctx.open_space(SpaceKind::Closure, closure.span(), None);

        let argc = closure.parameter_list.parameters.iter().count() as u32;
        ctx.current().nargs.record_closure_args(argc);
        ctx.record_evidence(closure.span(), |e, s| {
            e.closure(s, "closure");
            e.closure_args(s, argc, "closure");
        });
    }

    fn walk_out_closure(&self, _closure: &Closure<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
        ctx.restore_cognitive();
    }

    fn walk_in_arrow_function(&self, arrow: &ArrowFunction<'arena>, ctx: &mut Visitor<'_>) {
        ctx.enter_function_like(SpaceKind::Closure);
        ctx.open_space(SpaceKind::Closure, arrow.span(), None);

        let argc = arrow.parameter_list.parameters.iter().count() as u32;
        ctx.current().nargs.record_closure_args(argc);
        ctx.record_evidence(arrow.span(), |e, s| {
            e.closure(s, "arrow_function");
            e.closure_args(s, argc, "arrow_function");
        });
    }

    fn walk_out_arrow_function(&self, _arrow: &ArrowFunction<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
        ctx.restore_cognitive();
    }

    // -----------------------------------------------------------------
    // Class-like spaces
    // -----------------------------------------------------------------

    fn walk_in_class(&self, class: &Class<'arena>, ctx: &mut Visitor<'_>) {
        // Class declaration is itself an LLOC line on its enclosing
        // space (legacy `ClassDeclaration`).
        ctx.current().loc.observe_lloc();
        let name = bytes_to_string(class.name.value);
        ctx.open_space(SpaceKind::Class, class.span(), Some(name));
    }
    fn walk_out_class(&self, _class: &Class<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
    }

    fn walk_in_interface(&self, interface: &Interface<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        let name = bytes_to_string(interface.name.value);
        ctx.open_space(SpaceKind::Interface, interface.span(), Some(name));
    }
    fn walk_out_interface(&self, _i: &Interface<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
    }

    fn walk_in_trait(&self, t: &Trait<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        let name = bytes_to_string(t.name.value);
        ctx.open_space(SpaceKind::Trait, t.span(), Some(name));
    }
    fn walk_out_trait(&self, _t: &Trait<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
    }

    fn walk_in_enum(&self, e: &Enum<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        let name = bytes_to_string(e.name.value);
        ctx.open_space(SpaceKind::Enum, e.span(), Some(name));
    }
    fn walk_out_enum(&self, _e: &Enum<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
    }

    fn walk_in_anonymous_class(&self, ac: &AnonymousClass<'arena>, ctx: &mut Visitor<'_>) {
        // Anonymous classes are an *expression*, not a top-level
        // declaration — they don't contribute their own LLOC line
        // (the surrounding ExpressionStatement already does).
        ctx.open_space(SpaceKind::Class, ac.span(), None);
    }
    fn walk_out_anonymous_class(&self, _ac: &AnonymousClass<'arena>, ctx: &mut Visitor<'_>) {
        ctx.close_space();
    }

    // -----------------------------------------------------------------
    // Class-like members (NPA / NPM)
    // -----------------------------------------------------------------

    fn walk_in_property(&self, p: &Property<'arena>, ctx: &mut Visitor<'_>) {
        // Each property declaration is an LLOC line (legacy
        // `PropertyDeclaration`).
        ctx.current().loc.observe_lloc();
        ctx.record_property(p);
    }

    fn walk_in_enum_case(&self, ec: &EnumCase<'arena>, ctx: &mut Visitor<'_>) {
        // PHP enum cases are typed constants on the enum, not
        // instance attributes (they have no per-instance state).
        // Record them as class-like attributes so NPA reflects the
        // enum's "surface area" the same way it does for class
        // constants — but mark them public (cases are always public
        // in PHP).
        let enclosing = ctx.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
        if !matches!(enclosing, SpaceKind::Enum) {
            return;
        }
        ctx.current()
            .npa
            .record_attribute(ContainerKind::Class, true);
        ctx.record_evidence(ec.span(), |e, s| e.public_attribute(s, "enum_case"));
    }

    // -----------------------------------------------------------------
    // Decision points (cyclomatic + cognitive + ABC)
    // -----------------------------------------------------------------

    fn walk_in_if(&self, if_node: &If<'arena>, ctx: &mut Visitor<'_>) {
        // `if` is a statement-shaped LLOC line. Inner `else if` / `else`
        // clauses do NOT bump LLOC again (they're part of the same
        // logical statement).
        ctx.current().loc.observe_lloc();
        // The `else if (with a space)` form parses as a nested `If`
        // whose immediate parent is an `else_clause`. Mago does not
        // emit a dedicated `ElseIf` AST node for that form — when the
        // walk descends from an `else_clause` into another `If`, we
        // suppress the structural nesting that this inner `If` would
        // otherwise add (its `+1` was already paid by the outer
        // `else_clause` callback). The actual flat `+1` for `else if`
        // happens via `walk_in_if_statement_body_else_clause`.
        let bumped_nesting = if ctx.suppress_next_if_nesting {
            ctx.suppress_next_if_nesting = false;
            // Cyclomatic decision still counts (each `if` is a path).
            ctx.current().cyclomatic.record_decision();
            ctx.current().abc.record_condition();
            ctx.record_evidence(if_node.span(), |e, s| {
                e.decision(s, "if");
                e.abc_condition(s, "if");
            });
            false
        } else {
            ctx.current().cyclomatic.record_decision();
            ctx.current().abc.record_condition();
            let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
            ctx.current().cognitive.increase_nesting(effective);
            ctx.record_evidence(if_node.span(), |e, s| {
                e.decision(s, "if");
                e.abc_condition(s, "if");
                e.cognitive(s, effective.saturating_add(1), "if");
            });
            true
        };

        // Mirror the legacy "if has a else clause" `+1` rule. Mago
        // surfaces the else clause as a nested AST node, so we only
        // need to detect its presence here.
        let else_span = match &if_node.body {
            IfBody::Statement(b) => b.else_clause.as_ref().map(|c| c.span()),
            IfBody::ColonDelimited(b) => b.else_clause.as_ref().map(|c| c.span()),
        };
        if let Some(else_span) = else_span {
            ctx.current().cognitive.increment_by_one();
            ctx.record_evidence(else_span, |e, s| e.cognitive(s, 1, "else"));
        }

        ctx.current().cognitive.boolean_seq.reset();
        ctx.save_cognitive();
        if bumped_nesting {
            ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
        }
    }

    fn walk_out_if(&self, _if_node: &If<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_if_statement_body_else_if_clause(
        &self,
        clause: &mago_syntax::cst::IfStatementBodyElseIfClause<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        // `elseif` keyword form: flat +1 cognitive, no extra nesting.
        // It's still a cyclomatic decision because each `elseif`
        // creates a distinct path through the function.
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        ctx.current().cognitive.increment_by_one();
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(clause.span(), |e, s| {
            e.decision(s, "elseif");
            e.abc_condition(s, "elseif");
            e.cognitive(s, 1, "elseif");
        });
    }

    fn walk_in_if_colon_delimited_body_else_if_clause(
        &self,
        clause: &mago_syntax::cst::IfColonDelimitedBodyElseIfClause<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        ctx.current().cognitive.increment_by_one();
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(clause.span(), |e, s| {
            e.decision(s, "elseif");
            e.abc_condition(s, "elseif");
            e.cognitive(s, 1, "elseif");
        });
    }

    fn walk_in_if_statement_body_else_clause(
        &self,
        clause: &mago_syntax::cst::IfStatementBodyElseClause<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        // ABC.C: `else` counts as a condition per Fitzpatrick's
        // original spec (same convention applied to `default`).
        ctx.current().abc.record_condition();
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(clause.span(), |e, s| e.abc_condition(s, "else"));
        // The `else if` (spaced) form is a single `If` statement
        // whose direct parent is this else_clause. When that's the
        // case, defer to the inner `If` callback to record the
        // decision; the structural nesting it would add is suppressed
        // via `suppress_next_if_nesting`. Also: the inner `If` will
        // record its own ABC.C from `walk_in_if`, so we'd
        // double-count. To avoid that: we already recorded one
        // above, but the inner `if` records itself once more and
        // that's fine — legacy did the same, an `else if` chain
        // contributes both an `else` condition and an `elseif`
        // condition.
        if matches!(&clause.statement, mago_syntax::cst::Statement::If(_)) {
            ctx.suppress_next_if_nesting = true;
        }
    }

    fn walk_in_if_colon_delimited_body_else_clause(
        &self,
        clause: &mago_syntax::cst::IfColonDelimitedBodyElseClause<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.current().abc.record_condition();
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(clause.span(), |e, s| e.abc_condition(s, "else"));
    }

    fn walk_in_while(&self, w: &While<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(w.span(), |e, s| {
            e.decision(s, "while");
            e.abc_condition(s, "while");
            e.cognitive(s, effective.saturating_add(1), "while");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_while(&self, _w: &While<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_for(&self, f: &For<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(f.span(), |e, s| {
            e.decision(s, "for");
            e.abc_condition(s, "for");
            e.cognitive(s, effective.saturating_add(1), "for");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_for(&self, _f: &For<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_foreach(&self, f: &Foreach<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(f.span(), |e, s| {
            e.decision(s, "foreach");
            e.abc_condition(s, "foreach");
            e.cognitive(s, effective.saturating_add(1), "foreach");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_foreach(&self, _f: &Foreach<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_do_while(&self, d: &DoWhile<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(d.span(), |e, s| {
            e.decision(s, "do_while");
            e.abc_condition(s, "do_while");
            e.cognitive(s, effective.saturating_add(1), "do_while");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_do_while(&self, _d: &DoWhile<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_switch(&self, sw: &Switch<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        // The `switch` itself does not contribute a decision; each
        // `case` does. Cognitive: opens a nesting frame so nested
        // control flow inside cases gets the depth penalty.
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(sw.span(), |e, s| {
            e.abc_condition(s, "switch");
            e.cognitive(s, effective.saturating_add(1), "switch");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_switch(&self, _s: &Switch<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_switch_expression_case(
        &self,
        c: &mago_syntax::cst::SwitchExpressionCase<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        // Each `case <expr>:` is its own LLOC line and a cyclomatic
        // decision (Sonar's PHP rule, also Fitzpatrick's ABC).
        ctx.current().loc.observe_lloc();
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        ctx.record_evidence(c.span(), |e, s| {
            e.decision(s, "switch_case");
            e.abc_condition(s, "switch_case");
        });
    }

    fn walk_in_switch_default_case(
        &self,
        c: &mago_syntax::cst::SwitchDefaultCase<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        // `default` is its own LLOC line. It's *not* a cyclomatic
        // decision (no new path), but it IS an ABC condition per
        // Fitzpatrick — same convention we use for `else_clause`.
        ctx.current().loc.observe_lloc();
        ctx.current().abc.record_condition();
        ctx.record_evidence(c.span(), |e, s| e.abc_condition(s, "switch_default_case"));
    }

    fn walk_in_match(&self, m: &Match<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.current().cognitive.boolean_seq.reset();
        ctx.record_evidence(m.span(), |e, s| {
            e.abc_condition(s, "match");
            e.cognitive(s, effective.saturating_add(1), "match");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_match(&self, _m: &Match<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_match_expression_arm(
        &self,
        a: &mago_syntax::cst::MatchExpressionArm<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        ctx.record_evidence(a.span(), |e, s| {
            e.decision(s, "match_arm");
            e.abc_condition(s, "match_arm");
        });
    }

    fn walk_in_match_default_arm(
        &self,
        a: &mago_syntax::cst::MatchDefaultArm<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.current().abc.record_condition();
        ctx.record_evidence(a.span(), |e, s| e.abc_condition(s, "match_default_arm"));
    }

    fn walk_in_try(&self, t: &Try<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.record_evidence(t.span(), |e, s| {
            e.cognitive(s, effective.saturating_add(1), "try");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_try(&self, _t: &Try<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    fn walk_in_try_catch_clause(
        &self,
        c: &mago_syntax::cst::TryCatchClause<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.record_evidence(c.span(), |e, s| {
            e.decision(s, "catch");
            e.abc_condition(s, "catch");
            e.cognitive(s, effective.saturating_add(1), "catch");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_try_catch_clause(
        &self,
        _c: &mago_syntax::cst::TryCatchClause<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.restore_cognitive();
    }

    fn walk_in_conditional(&self, c: &Conditional<'arena>, ctx: &mut Visitor<'_>) {
        // PHP ternary `cond ? then : else` is a cyclomatic decision.
        ctx.current().cyclomatic.record_decision();
        ctx.current().abc.record_condition();
        let effective = ctx.cognitive.nesting + ctx.cognitive.depth + ctx.cognitive.lambda;
        ctx.current().cognitive.increase_nesting(effective);
        ctx.record_evidence(c.span(), |e, s| {
            e.decision(s, "conditional");
            e.abc_condition(s, "conditional");
            e.cognitive(s, effective.saturating_add(1), "conditional");
        });
        ctx.save_cognitive();
        ctx.cognitive.nesting = ctx.cognitive.nesting.saturating_add(1);
    }
    fn walk_out_conditional(&self, _c: &Conditional<'arena>, ctx: &mut Visitor<'_>) {
        ctx.restore_cognitive();
    }

    // -----------------------------------------------------------------
    // Operators (cyclomatic + cognitive boolean sequences + ABC)
    // -----------------------------------------------------------------

    fn walk_in_binary(&self, b: &Binary<'arena>, ctx: &mut Visitor<'_>) {
        /// Shared shape for the four short-circuiting boolean
        /// operators: decision + ABC.C + boolean-run collapse. The
        /// cognitive evidence amount is the delta the collapser
        /// actually applied (0 for a same-operator repeat, which the
        /// sink skips). Evidence spans point at the operator token,
        /// same as the tree-sitter walkers.
        fn boolean_op(ctx: &mut Visitor<'_>, span: Span, op: &'static str) {
            ctx.current().cyclomatic.record_decision();
            ctx.current().abc.record_condition();
            let before = ctx.current().cognitive.structural;
            ctx.current().cognitive.observe_boolean(op);
            let delta = ctx.current().cognitive.structural.saturating_sub(before);
            ctx.record_evidence(span, |e, s| {
                e.decision(s, op);
                e.abc_condition(s, op);
                e.cognitive(s, delta, op);
            });
        }

        match &b.operator {
            BinaryOperator::And(_) => boolean_op(ctx, b.operator.span(), "&&"),
            BinaryOperator::Or(_) => boolean_op(ctx, b.operator.span(), "||"),
            BinaryOperator::LowAnd(_) => boolean_op(ctx, b.operator.span(), "and"),
            BinaryOperator::LowOr(_) => boolean_op(ctx, b.operator.span(), "or"),
            // `xor` is intentionally excluded: it does not short-circuit,
            // so it adds no execution path.
            BinaryOperator::LowXor(_) => {}
            // ABC.C: every comparison is a condition.
            BinaryOperator::Equal(_)
            | BinaryOperator::NotEqual(_)
            | BinaryOperator::Identical(_)
            | BinaryOperator::NotIdentical(_)
            | BinaryOperator::AngledNotEqual(_)
            | BinaryOperator::LessThan(_)
            | BinaryOperator::LessThanOrEqual(_)
            | BinaryOperator::GreaterThan(_)
            | BinaryOperator::GreaterThanOrEqual(_)
            | BinaryOperator::Spaceship(_) => {
                ctx.current().abc.record_condition();
                let op = comparison_op_str(&b.operator);
                ctx.record_evidence(b.operator.span(), |e, s| e.abc_condition(s, op));
            }
            _ => {}
        }
    }

    fn walk_in_unary_prefix(&self, u: &UnaryPrefix<'arena>, ctx: &mut Visitor<'_>) {
        match u.operator {
            UnaryPrefixOperator::Not(_) => {
                // Boolean-sequence bookkeeping only — no metric moves
                // here, so nothing is evidenced.
                ctx.current().cognitive.boolean_seq.not_operator("!");
            }
            // ABC.A: prefix `++` and `--` are assignments.
            UnaryPrefixOperator::PreIncrement(_) => {
                ctx.current().abc.record_assignment();
                ctx.record_evidence(u.span(), |e, s| e.abc_assignment(s, "pre_increment"));
            }
            UnaryPrefixOperator::PreDecrement(_) => {
                ctx.current().abc.record_assignment();
                ctx.record_evidence(u.span(), |e, s| e.abc_assignment(s, "pre_decrement"));
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------
    // ABC.A: assignments and update (++/--)
    // -----------------------------------------------------------------

    fn walk_in_assignment(&self, a: &Assignment<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().abc.record_assignment();
        ctx.record_evidence(a.span(), |e, s| e.abc_assignment(s, "assignment"));
    }

    fn walk_in_unary_postfix(
        &self,
        u: &mago_syntax::cst::UnaryPostfix<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        use mago_syntax::cst::UnaryPostfixOperator;
        match u.operator {
            UnaryPostfixOperator::PostIncrement(_) => {
                ctx.current().abc.record_assignment();
                ctx.record_evidence(u.span(), |e, s| e.abc_assignment(s, "post_increment"));
            }
            UnaryPostfixOperator::PostDecrement(_) => {
                ctx.current().abc.record_assignment();
                ctx.record_evidence(u.span(), |e, s| e.abc_assignment(s, "post_decrement"));
            }
        }
    }

    // -----------------------------------------------------------------
    // ABC.B: function / method / instantiation / construct calls
    // -----------------------------------------------------------------

    fn walk_in_call(&self, c: &Call<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().abc.record_branch();
        ctx.record_evidence(c.span(), |e, s| e.abc_branch(s, "call"));
    }

    fn walk_in_null_safe_method_call(
        &self,
        _c: &NullSafeMethodCall<'arena>,
        _ctx: &mut Visitor<'_>,
    ) {
        // Already covered by `walk_in_call` since `Call::NullSafeMethod`
        // dispatches here too — no double-counting needed (Mago's
        // walker only fires `walk_call` for the `Call` enum variant,
        // not for the inner specialized struct, see `mago_syntax`
        // walker macro). Keep this empty so the trait method exists.
    }

    fn walk_in_instantiation(&self, i: &Instantiation<'arena>, ctx: &mut Visitor<'_>) {
        // `new Foo(...)` is a branch.
        ctx.current().abc.record_branch();
        ctx.record_evidence(i.span(), |e, s| e.abc_branch(s, "instantiation"));
    }

    fn walk_in_construct(&self, c: &Construct<'arena>, ctx: &mut Visitor<'_>) {
        // PHP language-level transfer-of-control intrinsics.
        match c {
            Construct::Include(_) => {
                ctx.current().abc.record_branch();
                ctx.record_evidence(c.span(), |e, s| e.abc_branch(s, "include"));
            }
            Construct::IncludeOnce(_) => {
                ctx.current().abc.record_branch();
                ctx.record_evidence(c.span(), |e, s| e.abc_branch(s, "include_once"));
            }
            Construct::Require(_) => {
                ctx.current().abc.record_branch();
                ctx.record_evidence(c.span(), |e, s| e.abc_branch(s, "require"));
            }
            Construct::RequireOnce(_) => {
                ctx.current().abc.record_branch();
                ctx.record_evidence(c.span(), |e, s| e.abc_branch(s, "require_once"));
            }
            Construct::Exit(_) => {
                ctx.current().nexit.record_exit();
                ctx.record_evidence(c.span(), |e, s| e.exit(s, "exit"));
            }
            Construct::Die(_) => {
                ctx.current().nexit.record_exit();
                ctx.record_evidence(c.span(), |e, s| e.exit(s, "die"));
            }
            // `print` is a quirky expression-statement (returns 1) —
            // not really a branch. `isset`/`empty`/`eval` aren't
            // branches either.
            _ => {}
        }
    }

    fn walk_in_yield(&self, y: &Yield<'arena>, ctx: &mut Visitor<'_>) {
        // `yield` is a branch — same convention as Ruby's `yield`.
        ctx.current().abc.record_branch();
        ctx.record_evidence(y.span(), |e, s| e.abc_branch(s, "yield"));
    }

    // -----------------------------------------------------------------
    // Function exits
    // -----------------------------------------------------------------

    fn walk_in_return(&self, r: &Return<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
        ctx.current().nexit.record_exit();
        ctx.record_evidence(r.span(), |e, s| e.exit(s, "return"));
    }

    fn walk_in_throw(&self, t: &Throw<'arena>, ctx: &mut Visitor<'_>) {
        // `Throw` here is the *expression* form (`throw new …`).
        // The throw token itself counts as an exit (legacy `throw`
        // rule). LLOC for the *statement* form is recorded by the
        // wrapping ExpressionStatement.
        ctx.current().nexit.record_exit();
        ctx.record_evidence(t.span(), |e, s| e.exit(s, "throw"));
    }

    fn walk_in_break(&self, _b: &mago_syntax::cst::Break<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_continue(&self, _c: &mago_syntax::cst::Continue<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_goto(&self, _g: &mago_syntax::cst::Goto<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_label(&self, _l: &mago_syntax::cst::Label<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_echo(&self, _e: &mago_syntax::cst::Echo<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_echo_tag(&self, _e: &mago_syntax::cst::EchoTag<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_unset(&self, _u: &mago_syntax::cst::Unset<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_namespace(&self, _n: &mago_syntax::cst::Namespace<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_use(&self, _u: &mago_syntax::cst::Use<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_global(&self, _g: &mago_syntax::cst::Global<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_static(&self, _s: &mago_syntax::cst::Static<'arena>, ctx: &mut Visitor<'_>) {
        // PHP's `static $x = …;` *function-static-declaration* form.
        // (NOT the `static` modifier on a method.)
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_declare(&self, _d: &mago_syntax::cst::Declare<'arena>, ctx: &mut Visitor<'_>) {
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_constant(&self, _c: &mago_syntax::cst::Constant<'arena>, ctx: &mut Visitor<'_>) {
        // Top-level `const X = 1;` declaration (legacy `ConstDeclaration`).
        ctx.current().loc.observe_lloc();
    }

    fn walk_in_class_like_constant(
        &self,
        _c: &mago_syntax::cst::ClassLikeConstant<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        // `class C { const X = 1; }` — legacy `ConstDeclaration2`.
        ctx.current().loc.observe_lloc();
    }

    // -----------------------------------------------------------------
    // LLOC — every statement-shaped node
    // -----------------------------------------------------------------

    fn walk_in_statement_expression(
        &self,
        _e: &ExpressionStatement<'arena>,
        ctx: &mut Visitor<'_>,
    ) {
        ctx.current().loc.observe_lloc();
        ctx.current().cognitive.boolean_seq.reset();
    }

    // Statement-shaped declarations (function / class / method / etc.)
    // already get one LLOC per declaration via this same mechanism if
    // they wrap an `ExpressionStatement`. Direct statements like
    // `return`/`break`/`continue` aren't `ExpressionStatement`s — but
    // their function-exit semantics (return/throw) are already
    // recorded above; LLOC for them rolls up via the `Statement`
    // walker, which we don't override (Mago's default recurses into
    // children, and we let the per-node callbacks do the work).
}
