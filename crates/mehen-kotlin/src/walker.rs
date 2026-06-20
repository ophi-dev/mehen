// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ANTLR-based Kotlin metric walker.
//!
//! Drives a recursive descent over the ANTLR `ParseTree` (entry rule
//! `kotlinFile`) and produces a populated [`MetricSpace`]. The structure
//! follows the per-language `Visitor` pattern used by `mehen-rust` and
//! `mehen-ruby`: one [`State`] per space, finalize-and-merge on close, with
//! a parent-less ANTLR tree handled by threading context **top-down**
//! (ANTLR rule contexts expose children but no parent pointer).
//!
//! ## Metric coverage (SonarKotlin-aligned)
//!
//! - **Cyclomatic**: `ifExpression`, every loop (`forStatement`,
//!   `whileStatement`, `doWhileStatement`), every `whenEntry`, and each
//!   short-circuit `&&` (`CONJ`) / `||` (`DISJ`) operator token. `catch`
//!   is intentionally excluded (matches SonarKotlin's
//!   `CyclomaticComplexityVisitor`).
//! - **Cognitive**: nesting on `ifExpression` (skipping the inner `if` of
//!   an `else if`), loops, `whenExpression`, `catchBlock`; flat `+1` on
//!   every `else` and on label-qualified `break@`/`continue@`; per-operator
//!   boolean-sequence collapse on `&&`/`||`; statement-shape resets.
//! - **ABC**: assignments via `assignment` and `propertyDeclaration` with
//!   an initializer; branches via every `callSuffix` (a call); conditions
//!   via `ifExpression`/`whenEntry`/`catchBlock`/loops/comparison &
//!   equality operators / `&&`/`||`/`?:`/`?.`/`!!`.
//! - **NExit**: `jumpExpression` whose lead token is `RETURN`/`RETURN_AT`
//!   or `THROW`.
//! - **NArgs**: `functionValueParameter` count under a function's
//!   `functionValueParameters`; `lambdaParameter` count under a lambda's
//!   `lambdaParameters`.
//! - **NOM**: every `functionDeclaration`, `anonymousFunction`,
//!   `secondaryConstructor`, `getter`, `setter` is a function space (NOM
//!   `record_function`); every `lambdaLiteral` is a function-shaped space
//!   counted as a closure.
//! - **LOC**: PLOC from code-line token rows, LLOC from statement-shaped
//!   rules, CLOC from the hidden-channel comment pass (handled by the
//!   analyzer and applied to the unit space here).
//! - **Halstead**: per-token operator/operand classification — keyword and
//!   punctuation tokens are operators; identifiers, literals, `this`,
//!   `super`, `field` are operands (deduped by text).
//! - **NPA / NPM / WMC**: class-vs-interface routing via the
//!   `classDeclaration`'s leading `CLASS`/`INTERFACE` token. NPA counts
//!   `propertyDeclaration` directly under a `classMemberDeclaration` plus
//!   primary-constructor `classParameter`s carrying `val`/`var`. NPM counts
//!   `functionDeclaration`/`secondaryConstructor`/`getter`/`setter`
//!   directly under a class body, public unless an explicit visibility
//!   modifier says otherwise.

use mehen_antlr::runtime::token::Token;
use mehen_antlr::runtime::{ParseTree, ParserRuleContext, TerminalNode};
use mehen_antlr::{CharByteMap, CommentRows, ctx_span};
use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricTreeBuilder, State, apply_state_to,
    finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

use crate::generated::kotlin_parser as kp;

/// Drive the walk over the parsed `kotlinFile` tree and return the unit
/// `MetricSpace`. `comment_rows` carries the hidden-channel comment spans
/// recovered by the analyzer's token pass (CLOC).
pub(crate) fn walk(
    tree: &ParseTree,
    source: &str,
    line_index: &LineIndex,
    comment_rows: &[CommentRows],
) -> MetricSpace {
    let char_map = CharByteMap::new(source);

    let unit_span = match tree {
        ParseTree::Rule(rule) => ctx_span(rule.context(), &char_map, line_index),
        _ => mehen_core::SourceSpan::empty(),
    };

    let mut unit_state = State::new();
    unit_state
        .loc
        .set_span(0, line_index.line_count().saturating_sub(1), true);
    // CLOC: comments never appear in the parse tree (hidden channel), so
    // they're observed here against the unit space.
    for c in comment_rows {
        unit_state.loc.observe_comment(c.start_row, c.end_row);
    }

    let mut walker = Walker {
        char_map: &char_map,
        line_index,
        tree: MetricTreeBuilder::new(unit_span),
        stack: vec![unit_state],
        kinds: vec![SpaceKind::Unit],
        cognitive: CognitiveContext::default(),
    };

    if let ParseTree::Rule(rule) = tree {
        for child in rule.context().children() {
            walker.visit(child, ChildHint::default());
        }
    }

    let mut unit_state = walker.stack.pop().expect("walker stack underflow");
    finalize_state(&mut unit_state);
    apply_state_to(unit_state, walker.tree.metrics_mut());
    walker.tree.finish()
}

/// Per-frame cognitive context — the legacy `(nesting, depth, lambda)`
/// triple. `nesting + depth + lambda` is the effective nesting level when a
/// nesting-increasing construct is observed.
#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

/// Context threaded *down* into a child during the walk, replacing the
/// upward `node.parent()` queries the tree-sitter walker used (ANTLR
/// contexts have no parent pointer).
#[derive(Clone, Copy, Debug, Default)]
struct ChildHint {
    /// This rule is being visited as the `else`-branch body of an
    /// enclosing `ifExpression`. An `ifExpression` reached through this
    /// hint is an `else if` and must not add cognitive nesting.
    is_else_branch: bool,
    /// This node is a direct member position of the enclosing class body
    /// (a `classMemberDeclaration`'s child), so NPA/NPM should consider it.
    in_class_member: bool,
    /// When this node is (within) a class-body `propertyDeclaration`, the
    /// property's resolved visibility, so a `getter`/`setter` with no
    /// explicit modifier of its own inherits it for NPM. `None` outside a
    /// class-body property.
    property_visibility: Option<AccessorOwner>,
    /// This terminal is the token of a `simpleIdentifier` rule. Kotlin
    /// soft keywords (`value`, `field`, `data`, …) lex as dedicated token
    /// types but are identifiers in this position, so they are Halstead
    /// *operands* regardless of token type.
    in_simple_identifier: bool,
}

/// The enclosing class-body property's container + default visibility,
/// threaded down to its accessors for NPM.
#[derive(Clone, Copy, Debug)]
struct AccessorOwner {
    container: ContainerKind,
    property_is_public: bool,
}

struct Walker<'a> {
    char_map: &'a CharByteMap,
    line_index: &'a LineIndex,
    tree: MetricTreeBuilder,
    stack: Vec<State>,
    kinds: Vec<SpaceKind>,
    cognitive: CognitiveContext,
}

impl Walker<'_> {
    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    fn visit(&mut self, node: &ParseTree, hint: ChildHint) {
        match node {
            ParseTree::Rule(rule) => self.visit_rule(rule.context(), hint),
            ParseTree::Terminal(term) => self.visit_terminal(term, hint),
            ParseTree::Error(_) => {}
        }
    }

    fn visit_terminal(&mut self, term: &TerminalNode, hint: ChildHint) {
        let tt = term.symbol().token_type();

        // Cyclomatic: each short-circuit boolean operator token.
        if matches!(tt, kp::CONJ | kp::DISJ) {
            self.current().cyclomatic.record_decision();
        }

        // Cognitive: `else` adds a flat +1 (covers `else if`); the boolean
        // operators feed the sequence collapser.
        match tt {
            kp::ELSE => self.current().cognitive.increment_by_one(),
            kp::CONJ => self.current().cognitive.observe_boolean("&&"),
            kp::DISJ => self.current().cognitive.observe_boolean("||"),
            _ => {}
        }

        // ABC conditions: comparison / equality / boolean / elvis / safe-nav
        // / not-null operators.
        if is_abc_condition_token(tt) {
            self.current().abc.record_condition();
        }

        // Halstead operator/operand token classification. A token reached
        // via `simpleIdentifier` is always an operand (covers Kotlin soft
        // keywords used as identifiers, e.g. `value`/`field`/`data`).
        let class = if hint.in_simple_identifier {
            HalsteadClass::Operand
        } else {
            halstead_class(tt)
        };
        match class {
            HalsteadClass::Operator => {
                let label = kp_token_name(tt);
                self.current().halstead.observe_operator(HalsteadOperator {
                    kind: SmolStr::new(label),
                    text: None,
                });
            }
            HalsteadClass::Operand => {
                let text = term.symbol().text().unwrap_or("");
                self.current().halstead.observe_operand(HalsteadOperand {
                    kind: SmolStr::new("Operand"),
                    text: Some(SmolStr::new(text)),
                });
            }
            HalsteadClass::Skip => {}
        }

        // PLOC: a terminal token's start row is a code line, unless it's a
        // structural newline/EOF (which carry no code).
        if !matches!(tt, kp::NL) && tt >= 0 {
            let row = (term.symbol().line() as u32).saturating_sub(1);
            self.current().loc.observe_code_line(row);
        }
    }

    fn visit_rule(&mut self, ctx: &ParserRuleContext, hint: ChildHint) {
        let ri = ctx.rule_index();

        // NPA / NPM: classify direct members of the *enclosing* class
        // before we open any space for this node (so the kinds stack still
        // has the class on top). `in_class_member` is threaded through the
        // transparent wrapper rules (`classMemberDeclaration`,
        // `declaration`) so it reaches the actual member declaration.
        if hint.in_class_member && is_class_member_rule(ri) {
            self.classify_class_member(ctx, ri);
        }

        // Does this rule open a metric space?
        let opened = self.maybe_open_space(ctx, ri, hint);

        // Per-rule classification (cyclomatic / cognitive / ABC / exit /
        // LOC). Returns a possibly-mutated cognitive context to restore
        // after the subtree, plus the per-child hints.
        let saved_cognitive = self.cognitive;
        self.classify_rule(ctx, ri, hint);

        // Recurse into children, computing each child's hint.
        self.visit_children(ctx, ri, hint);

        if opened {
            self.close_space();
        }
        self.cognitive = saved_cognitive;
    }

    /// Walk the children of `ctx`, deriving each child's [`ChildHint`] from
    /// this rule's identity, the inbound hint, and the child's position.
    fn visit_children(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
        let children = ctx.children();

        // For `ifExpression`, the `else`-branch body is the
        // `controlStructureBody` that appears after the `ELSE` token. Tag
        // it so an `ifExpression` reached through it (without an
        // intervening `block`) is recognized as `else if`.
        let else_body_idx = if ri == kp::RULE_IF_EXPRESSION {
            else_branch_index(children)
        } else {
            None
        };
        // `is_else_branch` flows down through the transparent
        // statement/expression wrapper chain (`controlStructureBody` →
        // `statement` → expression ladder → `primaryExpression`) until it
        // reaches the inner `ifExpression`. A `block` (braces) stops the
        // flow: `else { if … }` is a genuinely nested `if`, not an
        // `else if`.
        let propagate_else = hint.is_else_branch && is_else_transparent(ri);

        // A class/interface/object body member position originates at
        // `classMemberDeclaration`. `in_class_member` then flows through the
        // transparent `declaration` wrapper down to the real member rule
        // (`functionDeclaration`, `propertyDeclaration`, …). Any other rule
        // (a method body, an expression) clears it so nested local
        // declarations are not counted as class members.
        let propagate_member = match ri {
            kp::RULE_CLASS_MEMBER_DECLARATION | kp::RULE_ENUM_ENTRY => true,
            kp::RULE_DECLARATION => hint.in_class_member,
            _ => false,
        };

        // When recursing into a class-body `propertyDeclaration`, resolve
        // its container + visibility once and thread it to the accessors
        // (`getter`/`setter`) for NPM. The enclosing space kind is the
        // class-like that owns the property.
        let property_owner = if ri == kp::RULE_PROPERTY_DECLARATION && hint.in_class_member {
            match self.kinds.last().cloned().unwrap_or(SpaceKind::Unit) {
                SpaceKind::Class | SpaceKind::Impl => Some(AccessorOwner {
                    container: ContainerKind::Class,
                    property_is_public: member_is_public(ctx),
                }),
                SpaceKind::Interface | SpaceKind::Trait => Some(AccessorOwner {
                    container: ContainerKind::Interface,
                    property_is_public: member_is_public(ctx),
                }),
                _ => None,
            }
        } else {
            // Propagate an already-resolved owner through transparent
            // wrappers inside the property (none in practice — accessors
            // are direct children — but keep it explicit).
            hint.property_visibility
        };

        // Tokens directly under `simpleIdentifier` are identifiers (incl.
        // soft keywords used as names) → Halstead operands.
        let in_simple_identifier = ri == kp::RULE_SIMPLE_IDENTIFIER;

        for (idx, child) in children.iter().enumerate() {
            let mut child_hint = ChildHint::default();
            if Some(idx) == else_body_idx || propagate_else {
                child_hint.is_else_branch = true;
            }
            child_hint.in_class_member = propagate_member;
            child_hint.property_visibility = property_owner;
            child_hint.in_simple_identifier = in_simple_identifier;
            self.visit(child, child_hint);
        }
    }

    /// Open a metric space for space-introducing rules. Returns whether a
    /// space was pushed.
    fn maybe_open_space(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) -> bool {
        match ri {
            kp::RULE_GETTER | kp::RULE_SETTER => {
                // A property accessor is a method of the enclosing class
                // (NPM): its visibility is its own explicit modifier, else
                // the owning property's visibility. Record it before
                // opening the function space so the class state is still on
                // top of the stack.
                if let Some(owner) = hint.property_visibility {
                    let public =
                        visibility_from_modifiers_of(ctx).unwrap_or(owner.property_is_public);
                    self.current().npm.record_method(owner.container, public);
                }
                let name = rule_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.nom.record_function();
                state.nargs.record_function_args(count_function_args(ctx));
                self.push_space(SpaceKind::Function, name, ctx, state);
                self.enter_function_cognitive();
                true
            }
            kp::RULE_FUNCTION_DECLARATION
            | kp::RULE_ANONYMOUS_FUNCTION
            | kp::RULE_SECONDARY_CONSTRUCTOR => {
                let name = rule_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.nom.record_function();
                state.nargs.record_function_args(count_function_args(ctx));
                self.push_space(SpaceKind::Function, name, ctx, state);
                self.enter_function_cognitive();
                true
            }
            kp::RULE_LAMBDA_LITERAL => {
                let mut state = self.new_space_state(ctx);
                state.nom.record_closure();
                state.nargs.record_closure_args(count_lambda_args(ctx));
                self.push_space(SpaceKind::Function, None, ctx, state);
                self.enter_function_cognitive();
                true
            }
            kp::RULE_CLASS_DECLARATION => {
                let name = rule_name(ctx);
                let is_interface = has_token(ctx, kp::INTERFACE);
                let kind = if is_interface {
                    SpaceKind::Interface
                } else {
                    SpaceKind::Class
                };
                let mut state = self.new_space_state(ctx);
                state.npa.record_class_like();
                state.npm.record_class_like();
                if !is_interface {
                    state.wmc.record_class_like();
                }
                // Primary-constructor properties (`class C(val x: Int)`) are
                // class attributes. They live under
                // `primaryConstructor → classParameters → classParameter`,
                // not in the class body, so count them here against the
                // freshly-opened class state.
                let container = if is_interface {
                    ContainerKind::Interface
                } else {
                    ContainerKind::Class
                };
                record_constructor_properties(ctx, container, &mut state);
                self.push_space(kind, name, ctx, state);
                true
            }
            kp::RULE_OBJECT_DECLARATION | kp::RULE_COMPANION_OBJECT => {
                let name = rule_name(ctx);
                let mut state = self.new_space_state(ctx);
                state.npa.record_class_like();
                state.npm.record_class_like();
                state.wmc.record_class_like();
                self.push_space(SpaceKind::Class, name, ctx, state);
                true
            }
            _ => false,
        }
    }

    fn new_space_state(&self, ctx: &ParserRuleContext) -> State {
        let mut state = State::new();
        let span = ctx_span(ctx, self.char_map, self.line_index);
        state.loc.set_span(
            span.start_line.saturating_sub(1),
            span.end_line.saturating_sub(1),
            false,
        );
        state
    }

    fn push_space(
        &mut self,
        kind: SpaceKind,
        name: Option<String>,
        ctx: &ParserRuleContext,
        state: State,
    ) {
        let span = ctx_span(ctx, self.char_map, self.line_index);
        self.tree.open(kind.clone(), span, name);
        self.stack.push(state);
        self.kinds.push(kind);
    }

    /// Cognitive function-entry: reset nesting / lambda, bump depth when
    /// nested inside another function.
    fn enter_function_cognitive(&mut self) {
        let nested_inside_function = self
            .kinds
            .iter()
            .rev()
            .skip(1)
            .any(|k| matches!(k, SpaceKind::Function));
        self.cognitive.nesting = 0;
        self.cognitive.lambda = 0;
        if nested_inside_function {
            self.cognitive.depth = self.cognitive.depth.saturating_add(1);
        }
    }

    fn close_space(&mut self) {
        let closed_kind = self.kinds.pop().expect("kinds underflow");
        let mut state = self.stack.pop().expect("stack underflow");
        if matches!(closed_kind, SpaceKind::Function) {
            state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
        }
        finalize_state(&mut state);
        apply_state_to(state.clone(), self.tree.metrics_mut());
        if let Some(parent) = self.stack.last_mut() {
            let parent_kind = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
            merge_child_into_parent(parent, &state);
            if matches!(closed_kind, SpaceKind::Function) {
                let container = container_kind(parent_kind);
                state.wmc.finalize_method_into(container, &mut parent.wmc);
            }
        }
        self.tree.close();
    }

    /// Per-rule cyclomatic / cognitive / ABC / exit / LOC classification.
    fn classify_rule(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
        // Cyclomatic decisions: if / loops / when-entry.
        if matches!(
            ri,
            kp::RULE_IF_EXPRESSION
                | kp::RULE_FOR_STATEMENT
                | kp::RULE_WHILE_STATEMENT
                | kp::RULE_DO_WHILE_STATEMENT
                | kp::RULE_WHEN_ENTRY
        ) {
            self.current().cyclomatic.record_decision();
        }

        self.classify_cognitive(ctx, ri, hint);
        self.classify_abc_rule(ctx, ri);
        self.classify_exit(ctx, ri);
        self.classify_loc_rule(ctx, ri);
    }

    fn classify_cognitive(&mut self, ctx: &ParserRuleContext, ri: usize, hint: ChildHint) {
        match ri {
            // `else if` (an ifExpression reached as an else-branch body)
            // does not add nesting — only the flat `else` +1 (emitted when
            // the ELSE terminal is visited) applies.
            kp::RULE_IF_EXPRESSION if hint.is_else_branch => {}
            kp::RULE_IF_EXPRESSION
            | kp::RULE_FOR_STATEMENT
            | kp::RULE_WHILE_STATEMENT
            | kp::RULE_DO_WHILE_STATEMENT
            | kp::RULE_WHEN_EXPRESSION
            | kp::RULE_CATCH_BLOCK => {
                let effective =
                    self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                self.current().cognitive.increase_nesting(effective);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
            }
            // Label-qualified break/continue add +1 (goto-like).
            kp::RULE_JUMP_EXPRESSION => {
                if has_token(ctx, kp::BREAK_AT) || has_token(ctx, kp::CONTINUE_AT) {
                    self.current().cognitive.increment_by_one();
                }
                self.current().cognitive.boolean_seq.reset();
            }
            // Statement-shape boolean resets.
            kp::RULE_ASSIGNMENT | kp::RULE_PROPERTY_DECLARATION => {
                self.current().cognitive.boolean_seq.reset();
            }
            _ => {}
        }
    }

    fn classify_abc_rule(&mut self, ctx: &ParserRuleContext, ri: usize) {
        match ri {
            kp::RULE_ASSIGNMENT => self.current().abc.record_assignment(),
            // A `propertyDeclaration` with an initializer (`= expr`) is an
            // assignment; `val`/`var` without `=` is not.
            kp::RULE_PROPERTY_DECLARATION if has_token(ctx, kp::ASSIGNMENT) => {
                self.current().abc.record_assignment();
            }
            // A call: the `callSuffix` rule wraps the argument list of a
            // postfix call.
            kp::RULE_CALL_SUFFIX => self.current().abc.record_branch(),
            // Multi-token operators modeled as rules: elvis (`?:`),
            // safe-nav (`?.`), and the `!!` not-null assertion.
            kp::RULE_ELVIS | kp::RULE_SAFE_NAV => self.current().abc.record_condition(),
            kp::RULE_POSTFIX_UNARY_OPERATOR if has_token(ctx, kp::EXCL_NO_WS) => {
                self.current().abc.record_condition();
            }
            kp::RULE_IF_EXPRESSION
            | kp::RULE_WHEN_ENTRY
            | kp::RULE_CATCH_BLOCK
            | kp::RULE_FOR_STATEMENT
            | kp::RULE_WHILE_STATEMENT
            | kp::RULE_DO_WHILE_STATEMENT => self.current().abc.record_condition(),
            _ => {}
        }
    }

    fn classify_exit(&mut self, ctx: &ParserRuleContext, ri: usize) {
        if ri == kp::RULE_JUMP_EXPRESSION {
            // NExit counts a bare `return` or `throw` — these exit the
            // enclosing function. A labeled `return@label` (`RETURN_AT`)
            // returns from a lambda, not the function, so it is excluded
            // (matches SonarKotlin); `break`/`continue` are excluded too.
            if has_token(ctx, kp::RETURN) || has_token(ctx, kp::THROW) {
                self.current().nexit.record_exit();
            }
        }
    }

    fn classify_loc_rule(&mut self, ctx: &ParserRuleContext, ri: usize) {
        // LLOC: statement / declaration-shaped rules.
        if matches!(
            ri,
            kp::RULE_FUNCTION_DECLARATION
                | kp::RULE_CLASS_DECLARATION
                | kp::RULE_OBJECT_DECLARATION
                | kp::RULE_COMPANION_OBJECT
                | kp::RULE_SECONDARY_CONSTRUCTOR
                | kp::RULE_PROPERTY_DECLARATION
                | kp::RULE_GETTER
                | kp::RULE_SETTER
                | kp::RULE_ASSIGNMENT
                | kp::RULE_FOR_STATEMENT
                | kp::RULE_WHILE_STATEMENT
                | kp::RULE_DO_WHILE_STATEMENT
                | kp::RULE_IF_EXPRESSION
                | kp::RULE_WHEN_EXPRESSION
                | kp::RULE_TRY_EXPRESSION
                | kp::RULE_JUMP_EXPRESSION
        ) {
            self.current().loc.observe_lloc();
        }

        // A statement-position bare expression (e.g. a call statement
        // `foo(bar())`) is one LLOC. `statement → expression` is the exact
        // signal; `declaration`/`assignment`/`loopStatement` payloads are
        // counted via their own rules above, and nested calls live deeper
        // in the expression subtree (never as a `statement`), so they don't
        // double-count.
        if ri == kp::RULE_STATEMENT && child_ctx(ctx, kp::RULE_EXPRESSION).is_some() {
            self.current().loc.observe_lloc();
        }
    }

    /// NPA / NPM classification for a direct member of an enclosing class
    /// body. `ctx` is the member declaration rule itself (the
    /// `in_class_member` hint already flowed through the transparent
    /// wrapper rules), and `ri` is its rule index.
    fn classify_class_member(&mut self, ctx: &ParserRuleContext, ri: usize) {
        let container = match self.kinds.last().cloned().unwrap_or(SpaceKind::Unit) {
            SpaceKind::Class | SpaceKind::Impl => ContainerKind::Class,
            SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
            _ => return,
        };
        let public = member_is_public(ctx);
        match ri {
            kp::RULE_PROPERTY_DECLARATION => {
                self.current().npa.record_attribute(container, public);
            }
            kp::RULE_FUNCTION_DECLARATION | kp::RULE_SECONDARY_CONSTRUCTOR => {
                self.current().npm.record_method(container, public);
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------
// Free helpers (top-down tree inspection — no parent pointers).
// --------------------------------------------------------------------

/// Index of the `else`-branch `controlStructureBody` child of an
/// `ifExpression`, if present. The else body is the `controlStructureBody`
/// that appears *after* the `ELSE` terminal among the children.
fn else_branch_index(children: &[ParseTree]) -> Option<usize> {
    let mut seen_else = false;
    for (idx, child) in children.iter().enumerate() {
        match child {
            ParseTree::Terminal(t) if t.symbol().token_type() == kp::ELSE => {
                seen_else = true;
            }
            ParseTree::Rule(rule)
                if seen_else && rule.context().rule_index() == kp::RULE_CONTROL_STRUCTURE_BODY =>
            {
                return Some(idx);
            }
            _ => {}
        }
    }
    None
}

/// Whether `ctx` has a direct child terminal of the given token type.
fn has_token(ctx: &ParserRuleContext, token_type: i32) -> bool {
    ctx.children().iter().any(|c| match c {
        ParseTree::Terminal(t) => t.symbol().token_type() == token_type,
        _ => false,
    })
}

/// The declared name of a class/function/object: its first
/// `simpleIdentifier` child's covered text.
fn rule_name(ctx: &ParserRuleContext) -> Option<String> {
    for child in ctx.children() {
        if let ParseTree::Rule(rule) = child {
            let c = rule.context();
            if matches!(
                c.rule_index(),
                kp::RULE_SIMPLE_IDENTIFIER | kp::RULE_IDENTIFIER
            ) {
                let t = c.text();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// Count `functionValueParameter`s under a function declaration's
/// `functionValueParameters`.
fn count_function_args(ctx: &ParserRuleContext) -> u32 {
    let mut total = 0;
    for child in ctx.children() {
        if let ParseTree::Rule(rule) = child {
            let c = rule.context();
            if c.rule_index() == kp::RULE_FUNCTION_VALUE_PARAMETERS {
                total += count_child_rules(c, kp::RULE_FUNCTION_VALUE_PARAMETER);
            }
        }
    }
    total
}

/// Count `lambdaParameter`s under a lambda literal's `lambdaParameters`.
fn count_lambda_args(ctx: &ParserRuleContext) -> u32 {
    let mut total = 0;
    for child in ctx.children() {
        if let ParseTree::Rule(rule) = child {
            let c = rule.context();
            if c.rule_index() == kp::RULE_LAMBDA_PARAMETERS {
                total += count_child_rules(c, kp::RULE_LAMBDA_PARAMETER);
            }
        }
    }
    total
}

fn count_child_rules(ctx: &ParserRuleContext, rule_index: usize) -> u32 {
    ctx.children()
        .iter()
        .filter(|c| matches!(c, ParseTree::Rule(rule) if rule.context().rule_index() == rule_index))
        .count() as u32
}

/// Whether a member declaration is public — default unless a
/// `visibilityModifier` (`private`/`protected`/`internal`) overrides.
fn member_is_public(ctx: &ParserRuleContext) -> bool {
    visibility_from_modifiers_of(ctx).unwrap_or(true)
}

/// Explicit visibility declared *on this node itself* (via its own
/// `modifiers` child), or `None` if it has no visibility modifier. Used for
/// property accessors, whose own modifier overrides the property's.
fn visibility_from_modifiers_of(ctx: &ParserRuleContext) -> Option<bool> {
    for child in ctx.children() {
        if let ParseTree::Rule(rule) = child {
            let c = rule.context();
            if c.rule_index() == kp::RULE_MODIFIERS {
                return visibility_from_modifiers(c);
            }
        }
    }
    None
}

/// Resolve an explicit visibility from a `modifiers` rule: `Some(false)`
/// for private/protected/internal, `Some(true)` for public, `None` if no
/// visibility modifier is present.
fn visibility_from_modifiers(modifiers: &ParserRuleContext) -> Option<bool> {
    for child in modifiers.children() {
        if let ParseTree::Rule(rule) = child {
            let modifier = rule.context();
            if modifier.rule_index() != kp::RULE_MODIFIER {
                continue;
            }
            // A `modifier` wraps a `visibilityModifier` rule whose token is
            // the visibility keyword.
            for inner in modifier.children() {
                if let ParseTree::Rule(vis_rule) = inner {
                    let vis = vis_rule.context();
                    if vis.rule_index() == kp::RULE_VISIBILITY_MODIFIER {
                        if has_token(vis, kp::PUBLIC) {
                            return Some(true);
                        }
                        if has_token(vis, kp::PRIVATE)
                            || has_token(vis, kp::PROTECTED)
                            || has_token(vis, kp::INTERNAL)
                        {
                            return Some(false);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Record primary-constructor properties as class attributes (NPA).
///
/// Walks `classDeclaration → primaryConstructor → classParameters →
/// classParameter` and counts each parameter that carries a `val`/`var`
/// keyword (a plain parameter without `val`/`var` is not a property). Each
/// counted parameter's visibility comes from its own modifiers (default
/// public).
fn record_constructor_properties(
    class_ctx: &ParserRuleContext,
    container: ContainerKind,
    state: &mut State,
) {
    let Some(primary) = child_ctx(class_ctx, kp::RULE_PRIMARY_CONSTRUCTOR) else {
        return;
    };
    let Some(params) = child_ctx(primary, kp::RULE_CLASS_PARAMETERS) else {
        return;
    };
    for child in params.children() {
        if let ParseTree::Rule(rule) = child {
            let param = rule.context();
            if param.rule_index() != kp::RULE_CLASS_PARAMETER {
                continue;
            }
            // Only `val`/`var` parameters are properties.
            if !has_token(param, kp::VAL) && !has_token(param, kp::VAR) {
                continue;
            }
            let public = member_is_public(param);
            state.npa.record_attribute(container, public);
        }
    }
}

/// First direct child rule context with the given rule index.
fn child_ctx(ctx: &ParserRuleContext, rule_index: usize) -> Option<&ParserRuleContext> {
    ctx.children().iter().find_map(|c| match c {
        ParseTree::Rule(rule) if rule.context().rule_index() == rule_index => Some(rule.context()),
        _ => None,
    })
}

fn container_kind(parent_kind: SpaceKind) -> ContainerKind {
    match parent_kind {
        SpaceKind::Class | SpaceKind::Impl => ContainerKind::Class,
        SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
        _ => ContainerKind::Other,
    }
}

// --------------------------------------------------------------------
// ABC / Halstead token classification.
// --------------------------------------------------------------------

/// Comparison / equality / boolean operator tokens that count as an ABC
/// "condition". Multi-token operators (`?:`, `?.`, `!!`) are handled as
/// rules in [`Walker::classify_abc_rule`], not here.
fn is_abc_condition_token(tt: i32) -> bool {
    matches!(
        tt,
        kp::EQEQ
            | kp::EXCL_EQ
            | kp::EQEQEQ
            | kp::EXCL_EQEQ
            | kp::LANGLE
            | kp::RANGLE
            | kp::LE
            | kp::GE
            | kp::CONJ
            | kp::DISJ
    )
}

/// Rules the `is_else_branch` hint is allowed to flow through on its way
/// from an `ifExpression`'s else `controlStructureBody` down to a directly-
/// nested `ifExpression` (the `else if` case). This is exactly the
/// statement → expression precedence-ladder chain the kotlin-spec grammar
/// inserts between a control-structure body and a bare expression.
///
/// `block` is intentionally absent: `else { if … }` introduces a real
/// nesting level and must not be flattened to an `else if`.
fn is_else_transparent(ri: usize) -> bool {
    matches!(
        ri,
        kp::RULE_CONTROL_STRUCTURE_BODY
            | kp::RULE_STATEMENT
            | kp::RULE_EXPRESSION
            | kp::RULE_DISJUNCTION
            | kp::RULE_CONJUNCTION
            | kp::RULE_EQUALITY
            | kp::RULE_COMPARISON
            | kp::RULE_GENERIC_CALL_LIKE_COMPARISON
            | kp::RULE_INFIX_OPERATION
            | kp::RULE_ELVIS_EXPRESSION
            | kp::RULE_INFIX_FUNCTION_CALL
            | kp::RULE_RANGE_EXPRESSION
            | kp::RULE_ADDITIVE_EXPRESSION
            | kp::RULE_MULTIPLICATIVE_EXPRESSION
            | kp::RULE_AS_EXPRESSION
            | kp::RULE_PREFIX_UNARY_EXPRESSION
            | kp::RULE_POSTFIX_UNARY_EXPRESSION
            | kp::RULE_PRIMARY_EXPRESSION
    )
}

/// Rules that are direct class members (NPA/NPM candidates) once the
/// `in_class_member` hint has flowed down through the transparent wrapper
/// rules.
fn is_class_member_rule(ri: usize) -> bool {
    matches!(
        ri,
        kp::RULE_PROPERTY_DECLARATION
            | kp::RULE_FUNCTION_DECLARATION
            | kp::RULE_SECONDARY_CONSTRUCTOR
    )
}

enum HalsteadClass {
    Operator,
    Operand,
    Skip,
}

/// Classify a token type as a Halstead operator, operand, or skipped.
///
/// Keywords and punctuation are operators; identifiers, literals, `this`,
/// `super`, and `field` are operands. Whitespace/newline, EOF, comments,
/// and string-delimiter tokens are skipped.
fn halstead_class(tt: i32) -> HalsteadClass {
    // Operands: identifiers, literals, this/super/field, string text.
    if matches!(
        tt,
        kp::IDENTIFIER
            | kp::INTEGER_LITERAL
            | kp::HEX_LITERAL
            | kp::BIN_LITERAL
            | kp::REAL_LITERAL
            | kp::FLOAT_LITERAL
            | kp::DOUBLE_LITERAL
            | kp::LONG_LITERAL
            | kp::UNSIGNED_LITERAL
            | kp::CHARACTER_LITERAL
            | kp::BOOLEAN_LITERAL
            | kp::NULL_LITERAL
            | kp::THIS
            | kp::SUPER
            | kp::FIELD
            | kp::LINE_STR_TEXT
            | kp::MULTI_LINE_STR_TEXT
    ) {
        return HalsteadClass::Operand;
    }

    // Skip structural / trivia tokens.
    if matches!(
        tt,
        kp::NL | kp::QUOTE_OPEN | kp::QUOTE_CLOSE | kp::SEMICOLON
    ) || tt < 0
    {
        return HalsteadClass::Skip;
    }

    // Everything else (keywords, punctuation, operators) is an operator.
    HalsteadClass::Operator
}

/// A stable string label for an operator token, used as its Halstead
/// operator key. The numeric token type is stable for a given generated
/// grammar, so we render it as a compact label.
fn kp_token_name(tt: i32) -> String {
    format!("t{tt}")
}
