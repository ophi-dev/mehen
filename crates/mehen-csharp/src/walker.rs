// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ANTLR-based C# metric walker.
//!
//! Drives a recursive descent over the ANTLR `ParseTree` (entry rule
//! `compilation_unit`) and produces a populated [`MetricSpace`]. The structure
//! mirrors the `mehen-java` / `mehen-kotlin` walkers — one [`State`] per space,
//! finalize-and-merge on close, with the parent-less ANTLR tree handled by
//! threading context **top-down**.
//!
//! ## Grammar shape
//!
//! The parser is derived from Roslyn's own published grammar (see
//! `mehen-csharp-parser/grammar/PROVENANCE.md`), which names rules after the
//! compiler's *syntax nodes*. Three consequences shape every classification here:
//!
//! - **Each statement form is its own rule.** `if_statement`, `while_statement`,
//!   `switch_statement`, `catch_clause`, `else_clause`, … so control flow is
//!   dispatched on `rule_index()`. That is also more precise than the keyword
//!   probing the previous grammars-v4 grammar required: a `has_token(IF)` test on
//!   a shared `simple_embedded_statement` fires for an `if` anywhere in the node,
//!   whereas a rule match cannot.
//! - **Each declaration carries its own `attribute_list* modifier*`.** So a
//!   member's span already covers its attributes, and its visibility is readable
//!   on the declaration itself — no wrapper rule to open the space at, no span
//!   widening, and no threading of resolved visibility down a wrapper chain.
//! - **The expression cycle is inlined.** Roslyn's `expression` participates in a
//!   mutual left-recursion cycle, and the generator's hub inlining (upstream
//!   #221) folds 16 of 17 satellites into it — so `invocation_expression`,
//!   `assignment_expression`, `binary_expression`, and `conditional_expression`
//!   have no rule index of their own. They are classified by *shape* through the
//!   typed `ExpressionContext`: one `expression` child plus an `argument_list` is
//!   an invocation, two children a binary/assignment, three a ternary.
//!
//! ## Typed contexts
//!
//! Navigation uses the generated typed contexts, whose accessors reach only a
//! rule's *declared direct children* — the property the untyped `child_rule` /
//! `has_token` probes could assert only by comment. Dispatch stays on
//! `rule_index()`, since a metric walker is fundamentally one match over rule
//! kinds. This mirrors `mehen-java` and `mehen-kotlin`.
//!
//! ## Metric coverage (SonarC#-aligned)
//!
//! - **Cyclomatic**: `if`, every loop (`while`/`do`/`for`/`foreach`), each
//!   `case` label, each switch-*expression* arm, the ternary `?:`, each
//!   short-circuit `&&`/`||`, and each `and`/`or` pattern combinator. `catch`,
//!   `switch` itself, `default:`, and a `_` arm are not decisions (matches SonarC#,
//!   which follows the same rule as SonarJava; `catch` counts only in cognitive).
//!   A switch expression scores exactly like the equivalent switch statement —
//!   rewriting one into the other must not move the number.
//! - **Cognitive**: nesting on `if`, loops, `switch` (statement *and* expression),
//!   `catch`, and the ternary; flat `+1` on `else`/`else if` and on `goto`; a
//!   sequence-collapsing boolean run on `&&`/`||` and on the `and`/`or` pattern
//!   combinators (+1 per operator-kind change, reset per statement and per call
//!   argument, matching SonarSource). `try` and `lock` add nothing — the spec
//!   increments on the *handler* (`catch`), not the guarded block, and `lock` is not
//!   an increment at all.
//! - **ABC**: assignments via the assignment-shaped `expression` (all
//!   `=`/compound/`??=` forms), `++`/`--`, and any initialized declarator;
//!   branches via every invocation-shaped `expression`, object creation, and
//!   `constructor_initializer` (NOT member access — that is qualification, not a
//!   call, so a qualified call still scores exactly one branch; and NOT `nameof`,
//!   which only has the invocation *shape*); conditions via
//!   `if`/`case`/`catch`/`when`/loops/comparison & equality/`&&`/`||`/ternary/
//!   `??`/`is`/`as`/`and`/`or`. An `operator_declaration`'s own symbol is excluded —
//!   `operator ++` declares an operator rather than applying one.
//!
//!   `<` and `>` need care, because C# spells three unrelated things with them and
//!   only the enclosing rule tells them apart. A comparison counts; the other two
//!   do not, and each has its own hint:
//!   - a **shift** — the prep spells `>>` as adjacent `>` tokens rejoined in the
//!     parser (so a generic closer is never mis-lexed as a shift), which means a
//!     shift reaches the token scan as two bare `GT` tokens
//!     (`ChildHint::in_shift_operator`);
//!   - a **generic or function-pointer delimiter** — `List<int>` would otherwise
//!     score two conditions, and `Dictionary<string, List<int>>` four
//!     (`ChildHint::in_type_delimiter`).
//! - **NExit**: `return_statement`, `throw_statement`, `throw_expression`, and
//!   `yield_statement` (both `yield return` and `yield break`).
//! - **NArgs**: the `parameter` count of a `parameter_list` /
//!   `bracketed_parameter_list`. Roslyn uses one `parameter` rule for every
//!   position — methods, operators, lambdas, anonymous methods alike — with
//!   `params` as a modifier rather than a distinct rule.
//! - **NOM**: every `method_declaration`, `constructor_declaration`,
//!   `destructor_declaration`, `operator_declaration`,
//!   `conversion_operator_declaration`, `local_function_statement`, and
//!   `accessor_declaration` (one rule covers get/set/init/add/remove) is a
//!   function space; `simple_lambda_expression`,
//!   `parenthesized_lambda_expression`, and `anonymous_method_expression` are
//!   closure-shaped function spaces.
//! - **LOC**: PLOC from per-space code-token rows during the walk, LLOC from
//!   statement/declaration-shaped rules, CLOC from a source-ordered pass over the
//!   hidden-channel comment tokens routed via `SpaceRangeTracker`. Preprocessor
//!   directives are routed through that same post-walk pass: they are on their own
//!   channel, so the tree walk never sees them, but a `#if` row still carries source
//!   text and must count as PLOC rather than falling out as a phantom blank.
//! - **Halstead**: per-token operator/operand classification — keywords and
//!   punctuation are operators; identifiers, literals, `this`, `base` are
//!   operands (deduped by text). A terminal reached through `identifier_token` is
//!   always an operand, which is what makes C#'s contextual keywords come out
//!   right: the prep widens that rule to accept all 42 of them. The UTF-8 literal
//!   suffix (`"text"u8`) is an operand too — real C# lexes it as part of the
//!   literal, and Roslyn splits it off only to model the syntax node.
//! - **NPA / NPM / WMC**: class-vs-interface routing by the declaration rule
//!   (`struct` and `record` count as class-like containers; `interface` as an
//!   interface). NPA counts `field_declaration` / `event_field_declaration`
//!   declarators, named `event_declaration`s, and `enum_member_declaration`s
//!   directly under a type body. NPM counts methods/constructors/operators/
//!   properties/indexers directly under a type body. C# visibility: a type
//!   member with no access modifier is `private` (NOT public), so only an
//!   explicit `public` counts toward NPA/NPM; interface members are implicitly
//!   public. `enum` members are implicitly public.

use mehen_antlr::runtime::token::Token;
use mehen_antlr::runtime::{FromRuleNode, Node, RuleNodeView, TerminalNodeView};
use mehen_antlr::{LocToken, LocTokenKind, ctx_span};
use mehen_core::{LineIndex, MetricSpace, SpaceKind};
use mehen_metrics::{
    ContainerKind, HalsteadOperand, HalsteadOperator, MetricTreeBuilder, SpaceRangeTracker, State,
    apply_state_to, finalize_state, merge_child_into_parent,
};
use smol_str::SmolStr;

use mehen_csharp_parser::c_sharp_lexer as cl;
use mehen_csharp_parser::c_sharp_parser as cp;
// Typed contexts, used for *navigation* — their accessors reach only a rule's
// declared direct children, which is the property the untyped `child_rule` /
// `has_token` probes could only assert by comment. Dispatch stays on
// `rule_index()`: a metric walker is fundamentally one match over rule kinds.
use mehen_csharp_parser::c_sharp_parser::{
    ExpressionContext, OperatorDeclarationContext, PatternContext, PrefixUnaryExpressionContext,
    SwitchExpressionArmContext,
};

/// Drive the walk over the parsed `compilation_unit` tree and return the unit
/// `MetricSpace`. LOC is computed from `loc_tokens` in a single ordered pass
/// *after* the tree walk has opened and closed every space.
pub(crate) fn walk(
    tree: Node<'_>,
    line_index: &LineIndex,
    source_len: usize,
    loc_tokens: &[LocToken],
) -> MetricSpace {
    let unit_span = match tree.as_rule() {
        Some(rule) => ctx_span(rule, line_index, source_len),
        None => mehen_core::SourceSpan::empty(),
    };

    let mut unit_state = State::new();
    unit_state
        .loc
        .set_span(0, line_index.line_count().saturating_sub(1), true);

    let mut walker = Walker {
        line_index,
        source_len,
        tree: MetricTreeBuilder::new(unit_span),
        stack: vec![unit_state],
        kinds: vec![SpaceKind::Unit],
        suppress_parent_wmc: vec![false],
        cognitive: CognitiveContext::default(),
        loc_routing: SpaceRangeTracker::new(),
    };

    if let Some(rule) = tree.as_rule() {
        for child in rule.children() {
            walker.visit(child, &ChildHint::default());
        }
    }

    let mut unit_state = walker.stack.pop().expect("walker stack underflow");

    // CLOC pass: route each comment to the deepest enclosing space (or the
    // unit) in source order (mirrors `mehen-java`/`mehen-kotlin`).
    //
    // Preprocessor directives are routed here as well, and only here. PLOC is
    // otherwise recorded during the tree walk (`visit_terminal`), which cannot see
    // them: a directive goes to its own channel, so it never reaches the parser and
    // never appears as a terminal. Without this pass a `#if` / `#define` / `#endif`
    // row carried no PLOC observation and fell out as
    // `blank = sloc - ploc - only_comment` — reported as a blank line despite
    // plainly carrying source text.
    for t in loc_tokens {
        match t.kind {
            LocTokenKind::Comment => walker.loc_routing.observe_comment(
                t.start_byte,
                t.end_byte,
                &mut unit_state.loc,
                t.start_row,
                t.end_row,
            ),
            // Only the off-channel tokens need this; an ordinary code token was
            // already observed during the walk, and `observe_code_line` inserts into
            // a row set, so a repeat is idempotent rather than double-counted.
            LocTokenKind::Code => walker.loc_routing.observe_code_line(
                t.start_byte,
                t.end_byte,
                &mut unit_state.loc,
                t.start_row,
            ),
        }
    }

    finalize_state(&mut unit_state);

    let mut root = walker.tree.finish();
    let mut unit_halstead = std::mem::take(&mut unit_state.halstead);
    let mut unit_loc = std::mem::take(&mut unit_state.loc);
    walker
        .loc_routing
        .finalize_into_tree(&mut root, &mut unit_halstead, &mut unit_loc);
    unit_state.halstead = unit_halstead;
    unit_state.loc = unit_loc;
    apply_state_to(unit_state, &mut root.metrics);
    root
}

/// Per-frame cognitive context — the `(nesting, depth, lambda)` triple used
/// exactly as the Java/Kotlin walkers use it.
#[derive(Clone, Copy, Debug, Default)]
struct CognitiveContext {
    nesting: u32,
    depth: u32,
    lambda: u32,
}

/// Context threaded *down* into a child during the walk (ANTLR contexts have
/// no parent pointer).
///
/// `Clone` (not `Copy`): `accessor_owner` carries an owned name so a property's
/// accessors can be named after it. Cloning a `SmolStr` is a cheap refcount
/// bump / inline copy, so threading the hint per child stays allocation-free in
/// the common case.
#[derive(Clone, Debug, Default)]
struct ChildHint {
    /// This `if_body`/`embedded_statement` is the `else`-branch body of an
    /// enclosing `if`. An `if` reached through this hint is an `else if` and
    /// must not add cognitive nesting (only the flat `else` +1 applies).
    is_else_branch: bool,
    /// This node is a direct member position of the enclosing type body, so
    /// NPA/NPM should consider it.
    in_type_member: bool,
    /// The container kind of the enclosing type body, so a member's counters
    /// route to class-vs-interface buckets and inherit the
    /// interface-default-public rule.
    member_container: Option<ContainerKind>,
    /// The member's resolved visibility, captured at the member-declaration
    /// wrapper (`class_member_declaration: attributes? all_member_modifiers?
    /// (…)`) where the modifiers are siblings of the inner declaration — the
    /// declaration itself has no parent pointer and does not carry them.
    /// `None` outside a member position.
    member_is_public: Option<bool>,
    /// This node is (within) a `for` statement's initializer, so a
    /// `local_variable_declaration` reached through it is the loop initializer,
    /// not a standalone statement — it must not add its own LLOC (the `for`
    /// statement already contributes the single header logical line). Also set
    /// for a `using`/`fixed` resource acquisition and a `foreach` header.
    in_for_init: bool,
    /// This terminal is the token of an `identifier` rule. C#'s contextual
    /// keywords (`var`, `async`, `await`, `get`, `set`, `value`, `when`,
    /// `where`, `from`, `select`, `nameof`, …) lex as dedicated token types but
    /// are identifiers in name position, so a terminal reached through this
    /// hint is a Halstead *operand* regardless of its token type (mirrors the
    /// Java walker's `identifier` handling).
    in_identifier: bool,
    /// We are inside an `attributes` list (`[Obsolete("x")]`). Attribute
    /// arguments are compile-time metadata, not executable code, so
    /// cyclomatic/cognitive/ABC accounting is suppressed for the whole subtree
    /// (LOC/Halstead still count — the tokens physically exist).
    in_attributes: bool,
    /// This terminal belongs to a `right_shift` / `unsigned_right_shift` rule, so
    /// its bare `>` tokens are half a shift operator rather than comparisons.
    ///
    /// The prep spells `>>` as adjacent `>` tokens rejoined in the parser (so a
    /// generic closer is never mis-lexed as a shift), which means a *shift* now
    /// presents to the token scan as two `GT` tokens. Only the enclosing rule
    /// tells them apart.
    in_shift_operator: bool,
    /// This terminal is a `<`/`>` used as a *delimiter* — a generic argument or
    /// parameter list, or a function-pointer signature — not a comparison.
    ///
    /// C# spells all three with the same `LT`/`GT` tokens it uses for relational
    /// operators, so `List<int> f;` would otherwise score two ABC conditions and
    /// two Halstead comparison operators. `Dictionary<string, List<int>>` would
    /// score four. Only the enclosing rule distinguishes them, exactly as for
    /// [`ChildHint::in_shift_operator`].
    ///
    /// This is deliberately NOT merged with `in_shift_operator`: a shift's `>`
    /// tokens are suppressed at the token scan and the operator is recorded once
    /// at its rule, whereas a delimiter is not an operator at all and is dropped
    /// outright.
    in_type_delimiter: bool,
    /// This node is inside an `accessor_declaration`'s body. An accessor opens a
    /// metric space but is not itself a logical line, so an expression-bodied
    /// accessor (`get => _x;`) has nothing else to make its space non-empty —
    /// [`Walker::classify_loc_rule`] counts the `arrow_expression_clause` for it.
    /// Every other expression body hangs off a declaration that is already
    /// counted, where counting the clause too would double.
    in_accessor_body: bool,
    /// The declared name of the enclosing property / indexer / event, threaded
    /// down so a `get`/`set` accessor space can be named `Prop.get` rather than
    /// anonymous.
    ///
    /// (No `member_decl_start` or `space_opened_by_wrapper` here: Roslyn puts
    /// `attribute_list* modifier*` directly on each declaration, so a member's
    /// own span already covers its attributes and there is no wrapper to open the
    /// space at. Both fields existed only for the grammars-v4 shape.)
    accessor_owner: Option<SmolStr>,
    /// This terminal is the operator symbol in an `operator_declaration`'s
    /// signature — the operator being *declared*, not one being applied.
    ///
    /// Roslyn spells the symbol as a direct token choice on the declaration
    /// (`… KW_OPERATOR KW_CHECKED? (PLUS | PLUS_PLUS | AMP_AMP | LT | …)`), so those
    /// tokens reach the scan looking exactly like real operators: `operator ++`
    /// recorded an ABC assignment, and `operator &&` / `operator <` would have
    /// recorded a decision and a comparison. Set only for the declaration's own
    /// direct terminals, so the body and parameter defaults still count normally.
    in_operator_symbol: bool,
    /// The NArgs an accessor of the enclosing member takes: the *indexer*'s
    /// parameter count (`this[int i]`'s getter is a one-argument function), or 0 for
    /// a property or event.
    ///
    /// `accessor_declaration` carries no parameter list of its own, so the count has
    /// to come from the owner. Without it, NArgs for the same indexer depended on
    /// body syntax — the expression-bodied form opens its space at
    /// `indexer_declaration`, where the list is present, and reported 1 where the
    /// block-bodied form reported 0.
    accessor_args: u32,
}

struct Walker<'a> {
    line_index: &'a LineIndex,
    source_len: usize,
    tree: MetricTreeBuilder,
    stack: Vec<State>,
    kinds: Vec<SpaceKind>,
    /// Parallel to `stack`/`kinds`: whether the closing function space must NOT
    /// contribute its cyclomatic to the parent's WMC. Set for local functions
    /// and lambdas, whose complexity belongs to the enclosing method (already
    /// counted there), not as a separate weighted method of the class.
    suppress_parent_wmc: Vec<bool>,
    cognitive: CognitiveContext,
    loc_routing: SpaceRangeTracker,
}

impl Walker<'_> {
    fn current(&mut self) -> &mut State {
        self.stack.last_mut().expect("walker stack empty")
    }

    fn visit(&mut self, node: Node<'_>, hint: &ChildHint) {
        if let Some(rule) = node.as_rule() {
            self.visit_rule(rule, hint);
        } else if let Some(term) = node.as_terminal() {
            self.visit_terminal(term, hint);
        }
        // Error leaves carry no metric contribution; they are surfaced as
        // diagnostics by `mehen_antlr::collect_errors` in the analyzer.
    }

    fn visit_terminal(&mut self, term: TerminalNodeView<'_>, hint: &ChildHint) {
        let tt = term.symbol().token_type();

        // Cognitive: `else` adds a flat +1 (covers `else if`). The boolean
        // operator tokens feed the sequence collapser; unlike Java's flat
        // `expression` rule, C#'s precedence cascade gives each `&&`/`||` its
        // own node, so observing the tokens in source order is exactly
        // SonarSource's flattened sequence.
        //
        // `in_operator_symbol` suppresses the whole family: in
        // `public static C operator ++(C v)` the `++` is the operator being
        // *declared*, not a mutation of anything, and `operator &&` / `operator <`
        // would likewise have scored a boolean decision and a comparison from their
        // own signatures.
        if !hint.in_attributes && !hint.in_operator_symbol {
            match tt {
                cl::KW_ELSE => self.current().cognitive.increment_by_one(),
                cl::AMP_AMP => self.current().cognitive.observe_boolean("&&"),
                cl::PIPE_PIPE => self.current().cognitive.observe_boolean("||"),
                _ => {}
            }

            // Cyclomatic: each short-circuit boolean operator token is a
            // decision (independent of the cognitive run collapse).
            if matches!(tt, cl::AMP_AMP | cl::PIPE_PIPE) {
                self.current().cyclomatic.record_decision();
            }

            // ABC conditions: comparison / equality / boolean / null-coalescing
            // operator tokens.
            //
            // Relational `<`/`>` are counted here, but the two constructs that
            // reuse those tokens for something else must not be:
            // - a shift — the prep spells `>>` as adjacent `>` tokens, so `a >> b`
            //   would read as two comparisons (`in_shift_operator`);
            // - a generic argument/parameter list or function-pointer signature,
            //   where they are delimiters — `List<int> f;` would read as two
            //   comparisons (`in_type_delimiter`).
            if is_abc_condition_token(tt) && !hint.in_shift_operator && !hint.in_type_delimiter {
                self.current().abc.record_condition();
            }

            // ABC assignments: `++`/`--` (Fitzpatrick lists both under A).
            // The `=`/compound forms are handled at the assignment expression.
            if matches!(tt, cl::PLUS_PLUS | cl::MINUS_MINUS) {
                self.current().abc.record_assignment();
            }
        }

        // Halstead operator/operand token classification. A terminal reached
        // through an `identifier` rule is always an operand — this covers C#'s
        // contextual keywords (`var`, `async`, `get`, `where`, …) used as names,
        // which carry dedicated token types but are identifiers here.
        let class = if hint.in_identifier {
            HalsteadClass::Operand
        } else {
            halstead_class(tt)
        };
        match class {
            HalsteadClass::Operator => {
                // `>>` and `>>>` are spelled as two or three adjacent `>` tokens
                // (so a generic closer is never mis-lexed as a shift), but they
                // are ONE Halstead operator. Recording each `>` would inflate
                // length/volume and would conflate the shift with the `>`
                // comparison in the distinct-operator set, so the whole operator
                // is recorded once at its enclosing rule instead.
                if hint.in_shift_operator {
                    return;
                }
                // A generic/function-pointer `<`…`>` is a delimiter, not a
                // comparison. It stays an operator (Halstead counts bracket pairs,
                // just as `(`/`)` are counted), but under its own name so the
                // distinct-operator count does not conflate `List<int>` with
                // `a < b`. Halstead pairs a bracket as one operator, so only the
                // opener is recorded.
                if hint.in_type_delimiter {
                    if tt == cl::LT {
                        self.current().halstead.observe_operator(HalsteadOperator {
                            kind: SmolStr::new("<>"),
                            text: None,
                        });
                    }
                    return;
                }
                self.current().halstead.observe_operator(HalsteadOperator {
                    kind: SmolStr::new(kp_token_name(tt)),
                    text: None,
                });
            }
            HalsteadClass::Operand => {
                let text = term.symbol().text_or_empty();
                self.current().halstead.observe_operand(HalsteadOperand {
                    kind: SmolStr::new("Operand"),
                    text: Some(SmolStr::new(text)),
                });
            }
            HalsteadClass::Skip => {}
        }

        // PLOC: a visible code token's start row is a code line, recorded into
        // the current space during the AST walk. Comments are hidden-channel
        // (routed after the walk), and EOF (`tt < 0`) is not code.
        //
        // A single visible token can span multiple physical lines — a verbatim
        // string (`@"…"`, `VERBATIM_STRING_LIT`), a raw string
        // (`"""…"""`, `ML_RAW_STRING_LIT`), or an interpolated-string content token
        // is one token covering several rows. Record *every* row it covers as code,
        // or the interior rows sit inside the enclosing span with no PLOC
        // observation and are reported as phantom blank lines
        // (`blank = sloc - ploc - only_comment`).
        if tt >= 0 {
            let start_row = (term.symbol().line() as u32).saturating_sub(1);
            let extra_rows = term
                .symbol()
                .text_or_empty()
                .bytes()
                .filter(|&b| b == b'\n')
                .count() as u32;
            for row in start_row..=start_row.saturating_add(extra_rows) {
                self.current().loc.observe_code_line(row);
            }
        }
    }

    fn visit_rule(&mut self, ctx: RuleNodeView<'_>, hint: &ChildHint) {
        let ri = ctx.rule_index();
        // Snapshot the cognitive context of the *enclosing* construct before
        // anything in this subtree mutates it. Must happen before
        // `maybe_open_space`, which resets nesting/lambda and bumps depth for
        // function spaces.
        let saved_cognitive = self.cognitive;

        // NPA / NPM: classify a direct member of the enclosing type body before
        // opening any space for this node, so the kinds stack still has the type
        // on top.
        //
        // Visibility is resolved from THIS node's own `modifier*` children, not
        // inherited from the hint. Roslyn puts the modifiers on the declaration
        // itself, so by the time the walk reaches `field_declaration` the inbound
        // hint (set at `member_declaration`) has nothing to carry — inheriting it
        // would fall back to "public" for every member.
        if hint.in_type_member
            && let Some(container) = hint.member_container
        {
            // An unmarked class/struct member is private; an unmarked interface
            // member is implicitly public.
            let default_public = matches!(container, ContainerKind::Interface);
            let public = visibility_from_modifiers(ctx).unwrap_or(default_public);
            self.classify_type_member(ctx, ri, container, public);
        }

        // Capture the enclosing type container BEFORE opening any space for
        // this node: a member wrapper may open a nested type space here, and
        // `member_propagation` (run after the open) would then see that
        // just-opened type instead of the real enclosing scope.
        let container_before_open = self.enclosing_container();

        let opened = self.maybe_open_space(ctx, ri, hint);
        self.classify_rule(ctx, ri, hint);

        // A call argument (`G(a && b)`) is an independent boolean context: its
        // inner short-circuit run must not collapse with a same-kind operator
        // outside the call, and vice-versa. Save the enclosing run's `last_op`,
        // start the argument fresh, then restore it so the *outer* run
        // continues across the call as if it were a single operand. Same for a
        // parenthesized sub-expression's interior? No: parentheses ARE
        // transparent to SonarSource's flattening, so only arguments isolate.
        let saved_bool = if ri == cp::RULE_ARGUMENT {
            Some(self.current().cognitive.boolean_seq.last_op.take())
        } else {
            None
        };

        self.visit_children(ctx, ri, hint, container_before_open);

        if let Some(prev) = saved_bool {
            self.current().cognitive.boolean_seq.last_op = prev;
        }

        if opened {
            self.close_space();
        }

        // No accessor-sibling hoisting needed: Roslyn's `accessor_list` holds
        // every accessor as a flat sibling, so `{ get; set; }` walks as two peer
        // `accessor_declaration` children. (grammars-v4 nested the second
        // accessor inside the first accessor's rule, which the walker had to
        // undo by re-visiting it after the first space closed.)
        self.cognitive = saved_cognitive;
    }

    fn visit_children(
        &mut self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        hint: &ChildHint,
        container_before_open: Option<ContainerKind>,
    ) {
        // `NodeChildren` is a cheap `Clone` slice-iterator, so it is re-walked
        // below without allocating.

        // An `else if` must not add cognitive nesting — only the flat `else` +1
        // applies. Roslyn spells the else branch as its own `else_clause`, so the
        // flag is set there and only when its body is a bare `if_statement`;
        // `else { if … }` is genuinely nested and gets no flag. That replaces the
        // old index-scan for the `if_body` following an `ELSE` token, plus the
        // transparency chain that carried the flag down to the nested `if`.
        // Set at the `else_clause`, then carried through the intervening
        // `statement` dispatch layer so it reaches the nested `if_statement`.
        // `else { if … }` is genuinely nested, so a `block` child stops it.
        let propagate_else = match ri {
            cp::RULE_ELSE_CLAUSE => else_clause_is_else_if(ctx),
            cp::RULE_STATEMENT => hint.is_else_branch,
            _ => false,
        };

        // Type body member positions originate at the member-declaration
        // wrappers, then flow through the transparent `common_member_declaration`
        // / `typed_member_declaration` wrappers to the real member rule. The
        // visibility is resolved at the wrapper because `all_member_modifiers`
        // is a sibling of the inner declaration, not its child.
        let (propagate_member, member_container, member_is_public) =
            self.member_propagation(ctx, ri, hint, container_before_open);

        // Capture the member wrapper's start so a member space can widen its
        // span upward over its own-line attributes/modifiers. A nested type or
        // function resets it (their members compute from their own wrappers).
        // A `for`/`foreach`/`using`/`fixed` header declaration must not let its
        // declaration add a second LLOC — the statement already contributes the
        // single header logical line. Tag ONLY the direct children of the
        // header-bearing rules (a non-sticky flag), so a real local declaration
        // nested inside a lambda in the initializer still counts.
        //
        // Roslyn inlines the header declaration into the statement itself
        // (`for_statement : … LPAREN (variable_declaration? | …)`), so the tag
        // goes on the statement rather than on a separate
        // `for_initializer`/`resource_acquisition` rule.
        let in_for_init = matches!(
            ri,
            cp::RULE_FOR_STATEMENT | cp::RULE_USING_STATEMENT | cp::RULE_FIXED_STATEMENT
        );

        // A terminal directly under `identifier_token` is a name → Halstead
        // operand. This is what makes C#'s contextual keywords come out right:
        // the prep widens `identifier_token` to accept all 42 of them, so a
        // `KW_VAR` reached here is an operand rather than an operator.
        let in_identifier = ri == cp::RULE_IDENTIFIER_TOKEN;

        // Once inside an attribute, stay inside for the whole subtree so
        // attribute metadata records no executable complexity.
        let in_attributes = hint.in_attributes || ri == cp::RULE_ATTRIBUTE_LIST;

        // Thread the property/indexer/event name down so its accessors can be
        // named `Prop.get` / `Prop.set`. Set at the member-bearing declaration;
        // a nested function/type resets it.
        let accessor_owner = if let Some(name) = accessor_owner_name(ctx, ri) {
            Some(name)
        } else if opens_type_like(ri) || opens_function_space(ri) {
            None
        } else {
            hint.accessor_owner.clone()
        };

        // The accessors' arity travels with the owner's name, for the same reason:
        // `accessor_declaration` has no parameter list, so only the owning
        // `indexer_declaration` knows it. A property or event resets it to 0 — its
        // accessors take no arguments even though `set`'s `value` is implicit.
        let accessor_args = if accessor_owner_name(ctx, ri).is_some() {
            ctx.child_rule(cp::RULE_BRACKETED_PARAMETER_LIST)
                .map(count_parameters)
                .unwrap_or(0)
        } else if opens_type_like(ri) || opens_function_space(ri) {
            0
        } else {
            hint.accessor_args
        };

        // When this wrapper opened the member OR type space itself (to capture
        // own-line attributes/modifiers), tell the inner declaration to skip
        // its own open. The flag flows through the transparent
        // `common_member_declaration`/`typed_member_declaration` wrappers to
        // the declaration node, which consumes it; a real space open clears it
        // so a nested declaration inside the body still opens normally.
        // `>>` and `>>>` are spelled as adjacent `>` tokens; tag them so the
        // token-level ABC scan does not read a shift as two comparisons.
        let in_shift_operator = matches!(
            ri,
            cp::RULE_RIGHT_SHIFT
                | cp::RULE_UNSIGNED_RIGHT_SHIFT
                | cp::RULE_RIGHT_SHIFT_ASSIGNMENT
                | cp::RULE_UNSIGNED_RIGHT_SHIFT_ASSIGNMENT
        );

        // A generic argument/parameter list and a function-pointer signature
        // delimit with the same `<`/`>` tokens as a comparison. Only the enclosing
        // rule tells them apart, so tag the subtree.
        //
        // The flag does NOT propagate into children: a type argument can contain a
        // real comparison (`Func<bool>` holding a lambda body, or the `expression`
        // inside `relational_pattern`), and a nested `List<List<int>>` re-enters
        // the delimiter rule for its own angle brackets anyway. Only each list's
        // *own* `<`/`>` terminals need suppressing, which is exactly the extent of
        // a non-propagating flag.
        let in_type_delimiter = matches!(
            ri,
            cp::RULE_TYPE_ARGUMENT_LIST
                | cp::RULE_TYPE_PARAMETER_LIST
                | cp::RULE_FUNCTION_POINTER_PARAMETER_LIST
        );

        // The operator symbol in `public static C operator ++(C v)` is a direct
        // token of the declaration, so a non-propagating flag covers exactly the
        // signature's own terminals and leaves the parameter list and body alone.
        let in_operator_symbol = ri == cp::RULE_OPERATOR_DECLARATION;

        // Inside an accessor's body from the accessor declaration downward, so
        // an expression-bodied `get => _x;` can count its own logical line. A
        // nested function or type resets it — their bodies are counted normally.
        let in_accessor_body = if ri == cp::RULE_ACCESSOR_DECLARATION {
            true
        } else if opens_type_like(ri) || opens_function_space(ri) {
            false
        } else {
            hint.in_accessor_body
        };

        for child in ctx.children() {
            let child_hint = ChildHint {
                is_else_branch: propagate_else,
                in_type_member: propagate_member,
                member_container,
                member_is_public,
                in_for_init,
                in_identifier,
                in_attributes,
                in_shift_operator,
                in_type_delimiter,
                in_operator_symbol,
                in_accessor_body,
                accessor_owner: accessor_owner.clone(),
                accessor_args,
            };
            self.visit(child, &child_hint);
        }
    }

    /// Compute the `(in_type_member, container, is_public)` hint for this rule's
    /// children.
    ///
    /// `member_declaration` marks a member position; the `base_*` rules beneath
    /// it are pure dispatch alternations (Roslyn's syntax model has abstract
    /// bases like `BaseMethodDeclarationSyntax`, so the generator emits one
    /// alternation rule per abstraction) and simply pass the position through.
    ///
    /// Visibility is read on the real declaration, because Roslyn puts
    /// `modifier*` there rather than on a wrapper — so unlike the grammars-v4
    /// shape there is nothing to resolve early and thread down.
    fn member_propagation(
        &self,
        _ctx: RuleNodeView<'_>,
        ri: usize,
        hint: &ChildHint,
        container_before_open: Option<ContainerKind>,
    ) -> (bool, Option<ContainerKind>, Option<bool>) {
        match ri {
            // A member position opens here. The container is captured BEFORE this
            // node's `maybe_open_space`, since a member that is itself a nested
            // type has already pushed that type's space.
            cp::RULE_MEMBER_DECLARATION => (true, container_before_open, None),
            // Enum members bypass `member_declaration` entirely: Roslyn inlines
            // them into `enum_declaration` (`… '{' (enum_member_declaration (','
            // enum_member_declaration)*)? '}'`), so the position must open here
            // too. They are implicitly public constants of the enum.
            cp::RULE_ENUM_DECLARATION => (true, Some(ContainerKind::Class), Some(true)),
            // Pure dispatch layers: keep the inbound member position so the hint
            // reaches the real declaration one level deeper.
            cp::RULE_BASE_FIELD_DECLARATION
            | cp::RULE_BASE_METHOD_DECLARATION
            | cp::RULE_BASE_PROPERTY_DECLARATION
            | cp::RULE_BASE_TYPE_DECLARATION
            | cp::RULE_TYPE_DECLARATION => (
                hint.in_type_member,
                hint.member_container,
                hint.member_is_public,
            ),
            // The real declarations keep the member position so nested spaces
            // still know their container. Visibility is NOT computed here: it is
            // read from the declaration's own `modifier*` at the point of
            // classification (see `visit_rule`), because Roslyn puts the
            // modifiers on the declaration rather than on a wrapper above it.
            _ if hint.in_type_member && declares_member(ri) => (true, hint.member_container, None),
            _ => (false, None, None),
        }
    }

    /// The `ContainerKind` of the type-like space currently on top of the kinds
    /// stack (for member NPA/NPM routing), or `None` if the top is not a
    /// type-like scope.
    fn enclosing_container(&self) -> Option<ContainerKind> {
        match self.kinds.last() {
            Some(SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum) => {
                Some(ContainerKind::Class)
            }
            Some(SpaceKind::Interface | SpaceKind::Trait) => Some(ContainerKind::Interface),
            _ => None,
        }
    }

    /// Open a `Function` space for a method-shaped member. `span_ctx` supplies
    /// the span (the member wrapper when opening at the wrapper, so the span
    /// covers own-line attributes/modifiers; otherwise the declaration itself);
    /// `fn_ctx` supplies the name and NArgs.
    fn open_function_space(
        &mut self,
        span_ctx: RuleNodeView<'_>,
        fn_ctx: RuleNodeView<'_>,
        hint: &ChildHint,
        kind: SpaceKind,
    ) {
        let name = function_name(fn_ctx, hint);
        // Node identity: the arena addresses every node by a `NodeId`, so
        // "different node" is an id comparison, not pointer equality.
        let opened_at_wrapper = span_ctx.node().id() != fn_ctx.node().id();
        // When opening at the wrapper, NPM must be recorded into the enclosing
        // type BEFORE the member space is pushed (member classification
        // normally runs at the inner declaration, but that node now sits inside
        // this space and would misroute NPM into the member).
        if opened_at_wrapper && let Some(container) = self.enclosing_container() {
            let default_public = matches!(container, ContainerKind::Interface);
            let public = visibility_from_modifiers(span_ctx).unwrap_or(default_public);
            self.current().npm.record_method(container, public);
        }
        // Widen the declaration-node span up to its member wrapper so own-line
        // attributes/modifiers belong to the member. Unused when opening at the
        // wrapper (the span already starts there).
        let mut state = self.new_space_state(span_ctx);
        // No row adoption needed: Roslyn puts `attribute_list* modifier*` on the
        // declaration itself, so the space's own span already starts at the
        // member's first attribute row rather than after it.
        let is_closure = matches!(kind, SpaceKind::Closure);
        if is_closure {
            state.nom.record_closure();
            state.nargs.record_closure_args(count_args(fn_ctx, hint));
        } else {
            state.nom.record_function();
            state.nargs.record_function_args(count_args(fn_ctx, hint));
        }
        // A local function's and a lambda's complexity belongs to the enclosing
        // method (already counted there), so neither rolls into the type's WMC.
        let suppress_wmc = is_closure || fn_ctx.rule_index() == cp::RULE_LOCAL_FUNCTION_STATEMENT;
        self.push_space(kind, name, span_ctx, state, suppress_wmc);
        self.enter_function_cognitive(is_closure);
    }

    /// Open a type-like (`Class`/`Enum`/`Interface`) space for `type_ctx`.
    /// `span_ctx` supplies the span — the wrapper when opening there (so
    /// own-line attributes/modifiers are covered), otherwise the definition.
    fn open_type_space(&mut self, span_ctx: RuleNodeView<'_>, type_ctx: RuleNodeView<'_>) {
        let name = name_from_identifier(type_ctx);
        let ri = type_ctx.rule_index();
        let mut state = self.new_space_state(span_ctx);
        state.npa.record_class_like();
        state.npm.record_class_like();
        let kind = match ri {
            cp::RULE_ENUM_DECLARATION => {
                state.wmc.record_class_like();
                SpaceKind::Enum
            }
            // An `interface` carries no WMC (its members are not weighted),
            // matching the Java walker's interface handling.
            cp::RULE_INTERFACE_DECLARATION => SpaceKind::Interface,
            // `class`, `struct`, `record`, and `delegate` are class-like. A
            // `delegate` is a type declaration with a signature but no body; it
            // opens a (childless) class space so its own LOC/NArgs are
            // attributed.
            _ => {
                state.wmc.record_class_like();
                SpaceKind::Class
            }
        };
        self.push_space(kind, name.clone(), span_ctx, state, false);
        self.enter_class_cognitive();

        // A primary constructor (`class C(int x)`, `record R(int X)`) puts its
        // parameters on the *type* declaration, and no `constructor_declaration` node
        // exists anywhere in the tree. Without this the constructor is absent from
        // NOM entirely and its parameters from NArgs — so `class C(int x)` reported
        // NOM 0 where the identical `class C { public C(int x) { } }` reported 1.
        //
        // Opened as a named function space rather than folded into the type's own
        // counters so it appears in the per-space tree the way an explicit
        // constructor does, and is named after the type for the same reason. It is
        // opened immediately after the type space and closed here: the parameter list
        // is the whole of it, since a primary constructor has no body of its own.
        //
        // `extension_block_declaration` is excluded even though it carries the same
        // `parameter_list?`: that list is the extension *receiver* (`extension(string
        // s)`), not a constructor — nothing is constructed, and the block has no name
        // to give the space.
        if ri != cp::RULE_EXTENSION_BLOCK_DECLARATION
            && let Some(params) = type_ctx.child_rule(cp::RULE_PARAMETER_LIST)
        {
            let args = count_parameters(params);
            let mut ctor = self.new_space_state(params);
            ctor.nom.record_function();
            ctor.nargs.record_function_args(args);
            self.push_space(SpaceKind::Function, name, params, ctor, false);
            self.enter_function_cognitive(false);
            self.close_space();
        }
    }

    /// Open a metric space for space-introducing rules. Returns whether a space
    /// was pushed.
    /// Open a metric space when `ctx` is a declaration that owns one.
    ///
    /// Roslyn's grammar puts `attribute_list* modifier*` directly on every
    /// declaration, so a member's span already starts at its attributes and its
    /// visibility is readable on the declaration itself. The grammars-v4 shape
    /// needed a wrapper rule (`class_member_declaration`, holding
    /// `all_member_modifiers` alongside `common_member_declaration`) to factor
    /// that prefix out of an LL decision, and the walker had to open the space at
    /// the wrapper and widen the span back. None of that applies here — hence no
    /// wrapper handling and no span widening.
    fn maybe_open_space(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) -> bool {
        match ri {
            cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_LOCAL_FUNCTION_STATEMENT => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // Property / indexer / event accessors are each their own function
            // space (SonarC# counts them as methods): `get`/`set` bodies carry
            // real complexity and are the C# analogue of Kotlin's
            // `getter`/`setter`.
            //
            // One rule covers all of get/set/init/add/remove, and `accessor_list`
            // holds them as flat siblings — so unlike grammars-v4 there is no
            // asymmetry where the second accessor nests inside the first, and no
            // sibling-hoisting is needed.
            cp::RULE_ACCESSOR_DECLARATION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // An expression-bodied property or indexer (`int P => 1;`) is
            // semantically a getter, but has no `accessor_list` for the arm above
            // to fire on — Roslyn spells it as an `arrow_expression_clause`
            // directly on the declaration. Without this, two identical getters
            // would produce different NOM / NArgs / WMC depending only on which
            // syntax the author chose. SonarC# counts both as methods.
            cp::RULE_PROPERTY_DECLARATION | cp::RULE_INDEXER_DECLARATION
                if ctx.child_rule(cp::RULE_ACCESSOR_LIST).is_none()
                    && ctx.child_rule(cp::RULE_ARROW_EXPRESSION_CLAUSE).is_some() =>
            {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Function);
                true
            }
            // Closures: a lambda (`x => …`, `(a, b) => …`) or an anonymous
            // method (`delegate(int x) { … }`). NOM/NArgs record them as
            // closures, and their cyclomatic must NOT roll into the enclosing
            // type's WMC (WMC weights *methods*). Roslyn splits lambdas by
            // parameter shape, and all three are real rules rather than labeled
            // alternatives — so no `is_anonymous_method` probe.
            cp::RULE_SIMPLE_LAMBDA_EXPRESSION
            | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
            | cp::RULE_ANONYMOUS_METHOD_EXPRESSION => {
                self.open_function_space(ctx, ctx, hint, SpaceKind::Closure);
                true
            }
            _ if opens_type_like(ri) => {
                self.open_type_space(ctx, ctx);
                true
            }
            _ => false,
        }
    }

    fn new_space_state(&self, ctx: RuleNodeView<'_>) -> State {
        self.new_space_state_widened(ctx, None)
    }

    /// Build a space's initial `State`, optionally widening the span's start
    /// (byte + line) upward to `widened_start`.
    fn new_space_state_widened(
        &self,
        ctx: RuleNodeView<'_>,
        widened_start: Option<(u32, u32)>,
    ) -> State {
        let mut state = State::new();
        let span = ctx_span(ctx, self.line_index, self.source_len);
        let start_line = match widened_start {
            Some((_, line)) if line < span.start_line => line,
            _ => span.start_line,
        };
        state.loc.set_span(
            start_line.saturating_sub(1),
            span.end_line.saturating_sub(1),
            false,
        );
        state
    }

    fn push_space(
        &mut self,
        kind: SpaceKind,
        name: Option<String>,
        ctx: RuleNodeView<'_>,
        state: State,
        suppress_parent_wmc: bool,
    ) {
        self.push_space_widened(kind, name, ctx, state, suppress_parent_wmc, None);
    }

    /// Like [`push_space`], but widens the recorded span's start (byte + line)
    /// upward to `widened_start` when it precedes the context's own start — so
    /// comment routing and the tree span cover the member's own-line
    /// attributes/modifiers.
    fn push_space_widened(
        &mut self,
        kind: SpaceKind,
        name: Option<String>,
        ctx: RuleNodeView<'_>,
        state: State,
        suppress_parent_wmc: bool,
        widened_start: Option<(u32, u32)>,
    ) {
        let mut span = ctx_span(ctx, self.line_index, self.source_len);
        if let Some((start_byte, start_line)) = widened_start
            && start_byte < span.start_byte
        {
            span.start_byte = start_byte;
            span.start_line = start_line;
        }
        let space_id = self.tree.open(kind.clone(), span, name);
        self.loc_routing
            .record_open(space_id, span.start_byte, span.end_byte);
        self.stack.push(state);
        self.kinds.push(kind);
        self.suppress_parent_wmc.push(suppress_parent_wmc);
    }

    /// Reset the cognitive context when opening a type-like space. A type body
    /// is a fresh scope: code that runs *directly* in it (field initializers)
    /// must not inherit the enclosing statement's nesting.
    fn enter_class_cognitive(&mut self) {
        self.cognitive = CognitiveContext::default();
    }

    fn enter_function_cognitive(&mut self, is_closure: bool) {
        // Depth is inherited only from an *enclosing function/closure within
        // the same type scope* — a lambda or local function nested directly in
        // another function's body. A type scope resets the baseline: a method in
        // a nested type is fresh, so its cognitive nesting starts at 0.
        let nested_inside_function = self
            .kinds
            .iter()
            .rev()
            .skip(1)
            .take_while(|k| {
                !matches!(
                    k,
                    SpaceKind::Class
                        | SpaceKind::Interface
                        | SpaceKind::Trait
                        | SpaceKind::Impl
                        | SpaceKind::Enum
                )
            })
            .any(|k| matches!(k, SpaceKind::Function | SpaceKind::Closure));
        self.cognitive.nesting = 0;
        self.cognitive.lambda = 0;
        if nested_inside_function {
            self.cognitive.depth = self.cognitive.depth.saturating_add(1);
        }
        let _ = is_closure;
    }

    fn close_space(&mut self) {
        let closed_kind = self.kinds.pop().expect("kinds underflow");
        let suppress_wmc = self.suppress_parent_wmc.pop().unwrap_or(false);
        let mut state = self.stack.pop().expect("stack underflow");
        // A function OR closure space carries its own McCabe value (base + 1),
        // used for its per-space cyclomatic and (for methods) the WMC rollup.
        if matches!(closed_kind, SpaceKind::Function | SpaceKind::Closure) {
            state.wmc.set_cyclomatic(state.cyclomatic.cyclomatic + 1);
        }
        finalize_state(&mut state);
        if let Some(space_id) = self.tree.current_id() {
            self.loc_routing
                .record_close(space_id, &state.loc, &state.cyclomatic);
        }
        apply_state_to(state.clone(), self.tree.metrics_mut());
        if let Some(parent) = self.stack.last_mut() {
            let parent_kind = self.kinds.last().cloned().unwrap_or(SpaceKind::Unit);
            merge_child_into_parent(parent, &state);
            // Roll a closing method's cyclomatic into the parent's WMC. C# WMC
            // is *per class* — an interface's members are not weighted, so only
            // roll into a class/struct/enum parent. Local functions and lambdas
            // are suppressed (their complexity is the enclosing method's).
            if matches!(closed_kind, SpaceKind::Function)
                && !suppress_wmc
                && matches!(
                    parent_kind,
                    SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum
                )
            {
                let container = container_kind(parent_kind);
                state.wmc.finalize_method_into(container, &mut parent.wmc);
            }
        }
        self.tree.close();
    }

    /// Per-rule cyclomatic / cognitive / ABC / exit / LOC classification.
    fn classify_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) {
        // Attribute arguments are compile-time metadata, not executable code,
        // so they record no executable complexity. LOC/Halstead still count —
        // the tokens physically exist.
        if !hint.in_attributes {
            self.classify_control_flow(ctx, ri, hint);
            self.classify_expression(ctx, ri);
            self.classify_abc_rule(ctx, ri);
        }
        // A split shift operator is one Halstead operator, recorded here because
        // `visit_terminal` skips its individual `>` tokens (see the
        // `in_shift_operator` branch there). Keyed by the rule so `>>` and `>>>`
        // stay distinct from each other and from the `>` comparison.
        if matches!(
            ri,
            cp::RULE_RIGHT_SHIFT
                | cp::RULE_UNSIGNED_RIGHT_SHIFT
                | cp::RULE_RIGHT_SHIFT_ASSIGNMENT
                | cp::RULE_UNSIGNED_RIGHT_SHIFT_ASSIGNMENT
        ) {
            self.current().halstead.observe_operator(HalsteadOperator {
                kind: SmolStr::new(format!("shift{ri}")),
                text: None,
            });
        }
        self.classify_loc_rule(ctx, ri, hint);
    }

    /// Classify the control-flow constructs. Roslyn gives each its own rule, so
    /// these are rule-index matches rather than keyword probes.
    fn classify_control_flow(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) {
        let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;

        // Roslyn gives each statement form its own rule, so these are rule-index
        // matches rather than keyword probes on a shared
        // `simple_embedded_statement`. That is also more precise: a
        // `has_token(IF)` probe fires for an `if` anywhere inside the node,
        // whereas a rule match cannot.
        match ri {
            // Cyclomatic + ABC always; cognitive nesting unless this is an
            // `else if`, whose flat +1 is emitted at the `else_clause`.
            cp::RULE_IF_STATEMENT => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                if !hint.is_else_branch {
                    self.current().cognitive.increase_nesting(eff);
                    self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                }
                self.current().cognitive.boolean_seq.reset();
            }
            cp::RULE_WHILE_STATEMENT
            | cp::RULE_DO_STATEMENT
            | cp::RULE_FOR_STATEMENT
            | cp::RULE_FOR_EACH_STATEMENT
            | cp::RULE_FOR_EACH_VARIABLE_STATEMENT => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().cognitive.boolean_seq.reset();
            }
            // `switch` itself adds cognitive nesting but not cyclomatic — the
            // `case` labels carry the decisions.
            cp::RULE_SWITCH_STATEMENT => {
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().cognitive.boolean_seq.reset();
            }
            // A switch *expression* arm (`v switch { 1 => …, _ => … }`) is the same
            // decision as a `case` label and must score identically: rewriting a
            // switch statement into the expression form does not make the code
            // simpler, so it must not lower the score.
            //
            // The nesting increment belongs to the whole switch expression, not to
            // each arm — but hub inlining folds `switch_expression` into
            // `expression`, so it has no rule index and is handled by shape in
            // `classify_expression`. Only the per-arm decision lives here.
            //
            // A discard arm (`_ => …`) is excluded for the same reason `default:` is
            // not a decision: it is the fall-through, not a test.
            cp::RULE_SWITCH_EXPRESSION_ARM => {
                if !is_discard_arm(ctx) {
                    self.current().cyclomatic.record_decision();
                    self.current().abc.record_condition();
                }
            }
            // NOTE: `try` and `lock` deliberately score NOTHING. SonarSource's
            // cognitive-complexity spec increments on `catch` (a handler is the
            // structure a reader must follow), not on the `try` block itself, and
            // `lock`/`synchronized` is not in the increment list at all. The
            // `catch` nesting is applied at `catch_clause` below, matching
            // `mehen-java`.
            cp::RULE_RETURN_STATEMENT | cp::RULE_THROW_STATEMENT => {
                self.current().nexit.record_exit();
                self.current().cognitive.boolean_seq.reset();
            }
            // `yield return` / `yield break` both leave the iterator.
            cp::RULE_YIELD_STATEMENT => {
                self.current().nexit.record_exit();
                self.current().cognitive.boolean_seq.reset();
            }
            // `goto` (including `goto case` / `goto default`) is goto-like: a
            // flat +1, no nesting.
            cp::RULE_GOTO_STATEMENT => {
                self.current().cognitive.increment_by_one();
                self.current().cognitive.boolean_seq.reset();
            }
            // A `case` label is a decision (cyclomatic) and a condition (ABC) in
            // both its constant and pattern forms; `default:` is its own rule and
            // is neither. The `switch` already opened the cognitive nesting
            // level, so a `case` adds no further nesting.
            cp::RULE_CASE_SWITCH_LABEL | cp::RULE_CASE_PATTERN_SWITCH_LABEL => {
                self.current().cyclomatic.record_decision();
                self.current().abc.record_condition();
            }
            // A `when` guard is a distinct boolean test — on a `case` label
            // (`case int i when i > 0:`) or a switch-expression arm — so it
            // records one ABC condition of its own. `catch (E e) when (…)` is a
            // separate rule with the same meaning.
            cp::RULE_WHEN_CLAUSE | cp::RULE_CATCH_FILTER_CLAUSE => {
                self.current().abc.record_condition();
            }
            // A pattern combinator (`o is int and > 5`, `is not null`) is a boolean
            // decision, the same as the `&&`/`||`/`!` it replaces. C# 9 spells these
            // with the contextual keywords `and`/`or`/`not` rather than the operator
            // tokens `visit_terminal` scans, so without this a pattern-heavy method
            // reports the complexity of a straight-line one.
            //
            // Hub inlining folds `binary_pattern` into `pattern` (as it does
            // `binary_expression` into `expression`), so the combinator is read from
            // the typed context's own `and`/`or` token rather than from a rule index.
            // The run tracker is fed the actual keyword because SonarSource collapses
            // a run of the *same* operator into one increment, so `a and b or c` must
            // stay two.
            //
            // `not` is handled like the prefix `!`: it marks the run so a following
            // same-kind combinator is not collapsed across the negation, but is not
            // itself a decision. A relational pattern (`is > 5`) needs nothing here —
            // its operator token is an ordinary `GT`/`LE`/… that `visit_terminal`
            // already counts as an ABC condition.
            cp::RULE_PATTERN => {
                if let Some(pattern) = PatternContext::from_rule_node(ctx) {
                    let combinator = if pattern.kw_or_token().is_some() {
                        Some("or")
                    } else if pattern.kw_and_token().is_some() {
                        Some("and")
                    } else {
                        None
                    };
                    if let Some(op) = combinator {
                        self.current().cyclomatic.record_decision();
                        self.current().abc.record_condition();
                        self.current().cognitive.observe_boolean(op);
                    }
                }
            }
            cp::RULE_UNARY_PATTERN => {
                self.current().cognitive.boolean_seq.not_operator("not");
            }
            // `catch` is cognitive-only (matches SonarC#/SonarJava): a nesting
            // increment plus an ABC condition, but no cyclomatic decision. One
            // rule now covers both the typed and bare forms.
            cp::RULE_CATCH_CLAUSE => {
                self.current().cognitive.increase_nesting(eff);
                self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                self.current().abc.record_condition();
            }
            // A `throw` *expression* (`x ?? throw new E()`, C# 7) is an exit that
            // the statement form above never sees.
            cp::RULE_THROW_EXPRESSION => self.current().nexit.record_exit(),
            // Statement-shaped positions that are not one of the forms above
            // still start a fresh boolean sequence, so operators never collapse
            // across a boundary — `F(a && b); G(c && d)` is +2, not +1.
            //
            // `equals_value_clause` is the initializer of a field, property, or
            // parameter default. It belongs here for the same reason: two sibling
            // field initializers are independent boolean contexts, but they share
            // the enclosing *type* space rather than a statement, so without a
            // reset `bool A = x && y; bool B = u && v;` collapsed into one run and
            // scored 1 where the equivalent pair of statements scores 2.
            cp::RULE_EXPRESSION_STATEMENT
            | cp::RULE_LOCAL_DECLARATION_STATEMENT
            | cp::RULE_ARROW_EXPRESSION_CLAUSE
            | cp::RULE_EQUALS_VALUE_CLAUSE
            | cp::RULE_BLOCK => {
                self.current().cognitive.boolean_seq.reset();
            }
            // The prefix `!` records a not-operator so a following same-kind
            // boolean operator is not collapsed with the one before the negation
            // (`a && !b && c` is one run in SonarSource's model, but the run
            // tracker needs the marker to keep parity with Kotlin).
            cp::RULE_PREFIX_UNARY_EXPRESSION
                if PrefixUnaryExpressionContext::from_rule_node(ctx)
                    .is_some_and(|expr| expr.bang_token().is_some()) =>
            {
                self.current().cognitive.boolean_seq.not_operator("!");
            }
            _ => {}
        }
    }

    /// Classify the inlined `expression` rule.
    ///
    /// Hub inlining (upstream #221) folds `invocation_expression`,
    /// `assignment_expression`, `binary_expression`, and `conditional_expression`
    /// into `expression`, so none has a rule index of its own. Classification is
    /// therefore by *shape*, read entirely through the typed context:
    ///
    /// | form                | `expression_children` | distinguishing feature |
    /// |---------------------|-----------------------|------------------------|
    /// | invocation `F(x)`   | 1                     | has an `argument_list` |
    /// | binary `a + b`      | 2                     | the operator terminal  |
    /// | assignment `y = 1`  | 2                     | the operator terminal  |
    /// | ternary `a ? b : c` | 3                     | a `?` terminal         |
    ///
    /// `direct_terminals()` (upstream #271) is what makes the operator readable
    /// without dropping to untyped scanning: it yields only the node's *own*
    /// terminals, so an operator from a nested subexpression cannot leak in.
    fn classify_expression(&mut self, ctx: RuleNodeView<'_>, ri: usize) {
        if ri != cp::RULE_EXPRESSION {
            return;
        }
        let Some(expr) = ExpressionContext::from_rule_node(ctx) else {
            return;
        };

        // A call or object creation is a branch (ABC's B counts function calls,
        // method calls, and message sends).
        //
        // Member access (`a.B`) is deliberately NOT counted: it is the
        // qualification `.B`, not a call. Counting it would (a) score a plain
        // field/property *read* as a branch, which ABC does not, and (b) score a
        // qualified call twice, since `o.Helper()` nests a member access inside
        // the invocation. Counting only the invocation keeps one branch per call
        // regardless of qualification depth, matching `mehen-java` (which counts
        // `methodCall`/`creator`, never field access).
        //
        // `nameof(x)` is excluded: it has the invocation shape but is a
        // compile-time operator that evaluates to a string constant — no call is
        // made, nothing is dispatched, and the argument is never evaluated. Scoring
        // it as a branch would rank `throw new ArgumentNullException(nameof(arg))`
        // above the same throw with a literal. (`typeof`/`sizeof`/`default` are
        // dedicated rules and so never reach here at all; `nameof` is only a
        // contextual keyword, so it parses as an ordinary invocation and has to be
        // filtered by name.)
        if expr.argument_list().is_some() && !is_nameof_callee(&expr) {
            self.current().abc.record_branch();
        }

        // `>>=` and `>>>=` are the only assignment operators the prep splits into
        // separate tokens (`GT GE` / `GT GT GE`), so they reach this node as child
        // *rules* rather than as an operator terminal and the token match below
        // never sees them. Without this, `a >>= 2` scores no assignment while the
        // otherwise-identical `a <<= 2` scores one.
        if expr.right_shift_assignment().is_some()
            || expr.unsigned_right_shift_assignment().is_some()
        {
            self.current().abc.record_assignment();
        }

        // A switch expression nests exactly like a switch statement (SonarSource
        // scores both the same way). Recognised by its `switch` keyword, since hub
        // inlining leaves it without a rule index of its own. Its arms carry the
        // decisions, recorded at `switch_expression_arm`.
        if expr.kw_switch_token().is_some() {
            let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
            self.current().cognitive.increase_nesting(eff);
            self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
            self.current().cognitive.boolean_seq.reset();
        }

        for terminal in expr.direct_terminals() {
            // A recovery-inserted token is not in the source, so scoring it would
            // invent a metric the file never contained.
            if terminal.is_error() {
                continue;
            }
            match terminal.symbol().token_type() {
                // Every assignment form (`=`, compound, `??=`) is one A.
                cl::EQ
                | cl::PLUS_EQ
                | cl::MINUS_EQ
                | cl::STAR_EQ
                | cl::SLASH_EQ
                | cl::PERCENT_EQ
                | cl::AMP_EQ
                | cl::CARET_EQ
                | cl::PIPE_EQ
                | cl::LT_LT_EQ
                | cl::QUESTION_QUESTION_EQ => self.current().abc.record_assignment(),
                // The ternary `?:` — a decision, an ABC condition, and a
                // cognitive nesting structure (SonarSource scores it like an
                // `if`). Keyed on `?` so the `:` does not score a second time.
                cl::QUESTION => {
                    let eff = self.cognitive.nesting + self.cognitive.depth + self.cognitive.lambda;
                    self.current().cyclomatic.record_decision();
                    self.current().cognitive.increase_nesting(eff);
                    self.cognitive.nesting = self.cognitive.nesting.saturating_add(1);
                    self.current().abc.record_condition();
                }
                // The type tests. Equality, relational, `&&`/`||`, and `??` are
                // counted by the token-level scan in `visit_terminal`, which sees
                // every token exactly once — so they must not be counted again
                // here.
                cl::KW_IS | cl::KW_AS => self.current().abc.record_condition(),
                _ => {}
            }
        }
    }

    /// ABC accounting for the non-`expression` rules that carry an assignment or
    /// a branch.
    fn classify_abc_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize) {
        match ri {
            // Object creation is its own rule rather than part of the inlined
            // expression cycle, so its branch is recorded here.
            cp::RULE_OBJECT_CREATION_EXPRESSION
            | cp::RULE_IMPLICIT_OBJECT_CREATION_EXPRESSION
            | cp::RULE_ARRAY_CREATION_EXPRESSION
            | cp::RULE_IMPLICIT_ARRAY_CREATION_EXPRESSION
            | cp::RULE_CONSTRUCTOR_INITIALIZER => self.current().abc.record_branch(),
            // An initialized declarator is an assignment. Roslyn spells the
            // initializer as an `equals_value_clause` child rather than a bare
            // `=` token, so the presence of that child *is* the initialization.
            cp::RULE_VARIABLE_DECLARATOR
            | cp::RULE_PARAMETER
            | cp::RULE_ENUM_MEMBER_DECLARATION
                if ctx.child_rule(cp::RULE_EQUALS_VALUE_CLAUSE).is_some() =>
            {
                self.current().abc.record_assignment();
            }
            _ => {}
        }
    }

    fn classify_loc_rule(&mut self, ctx: RuleNodeView<'_>, ri: usize, hint: &ChildHint) {
        // A `for`/`foreach`/`using` header's declaration is part of the
        // statement's single logical line — not its own LLOC.
        if ri == cp::RULE_VARIABLE_DECLARATION && hint.in_for_init {
            return;
        }
        // An *expression-bodied* lambda (`x => x + 1`) opens a closure space
        // but its body contains no statement/declaration, so the closure would
        // report `lloc = 0`. Count the lambda itself as one logical line to
        // match a block-bodied lambda (whose inner statements already count).
        if matches!(
            ri,
            cp::RULE_SIMPLE_LAMBDA_EXPRESSION | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
        ) && !lambda_body_is_block(ctx)
        {
            self.current().loc.observe_lloc();
            return;
        }
        // An expression-bodied accessor (`get => _x;`) opens a space whose only
        // content is that expression — no statement — so the space would report
        // `lloc = 0` without counting the body itself as one logical line.
        //
        // Roslyn spells every expression body as `arrow_expression_clause`, so
        // this is one rule rather than the old `body`/`accessor_body`/
        // `local_function_body` trio.
        //
        // It must fire only where the enclosing declaration is not itself counted
        // below, or `int F() => 1;` would count twice. `in_accessor_body` marks
        // the one case that needs it: an `accessor_declaration` opens a space but
        // is not a logical line of its own, so an expression-bodied accessor has
        // nothing else to count.
        if ri == cp::RULE_ARROW_EXPRESSION_CLAUSE && hint.in_accessor_body {
            self.current().loc.observe_lloc();
            return;
        }

        if matches!(
            ri,
            // Statement-shaped rules. Roslyn gives each statement form its own
            // rule, so this replaces the single `simple_embedded_statement`.
            cp::RULE_EXPRESSION_STATEMENT
                | cp::RULE_LOCAL_DECLARATION_STATEMENT
                | cp::RULE_LOCAL_FUNCTION_STATEMENT
                | cp::RULE_IF_STATEMENT
                | cp::RULE_WHILE_STATEMENT
                | cp::RULE_DO_STATEMENT
                | cp::RULE_FOR_STATEMENT
                | cp::RULE_FOR_EACH_STATEMENT
                | cp::RULE_FOR_EACH_VARIABLE_STATEMENT
                | cp::RULE_SWITCH_STATEMENT
                | cp::RULE_TRY_STATEMENT
                | cp::RULE_USING_STATEMENT
                | cp::RULE_LOCK_STATEMENT
                | cp::RULE_FIXED_STATEMENT
                | cp::RULE_CHECKED_STATEMENT
                | cp::RULE_UNSAFE_STATEMENT
                | cp::RULE_RETURN_STATEMENT
                | cp::RULE_THROW_STATEMENT
                | cp::RULE_YIELD_STATEMENT
                | cp::RULE_BREAK_STATEMENT
                | cp::RULE_CONTINUE_STATEMENT
                | cp::RULE_GOTO_STATEMENT
                | cp::RULE_LABELED_STATEMENT
                // Declaration-shaped rules. `empty_statement` is deliberately
                // absent: a bare `;` is not a logical line. So is `block`, which
                // is a wrapper whose inner statements each count.
                | cp::RULE_FIELD_DECLARATION
                | cp::RULE_EVENT_FIELD_DECLARATION
                | cp::RULE_EVENT_DECLARATION
                | cp::RULE_METHOD_DECLARATION
                | cp::RULE_CONSTRUCTOR_DECLARATION
                | cp::RULE_DESTRUCTOR_DECLARATION
                | cp::RULE_OPERATOR_DECLARATION
                | cp::RULE_CONVERSION_OPERATOR_DECLARATION
                | cp::RULE_PROPERTY_DECLARATION
                | cp::RULE_INDEXER_DECLARATION
                | cp::RULE_ENUM_MEMBER_DECLARATION
                | cp::RULE_CLASS_DECLARATION
                | cp::RULE_STRUCT_DECLARATION
                | cp::RULE_INTERFACE_DECLARATION
                | cp::RULE_ENUM_DECLARATION
                | cp::RULE_RECORD_DECLARATION
                | cp::RULE_DELEGATE_DECLARATION
                | cp::RULE_NAMESPACE_DECLARATION
                | cp::RULE_FILE_SCOPED_NAMESPACE_DECLARATION
                | cp::RULE_USING_DIRECTIVE
                | cp::RULE_EXTERN_ALIAS_DIRECTIVE
        ) {
            self.current().loc.observe_lloc();
        }
    }

    /// NPA / NPM classification for a direct member of an enclosing type body.
    /// `ctx` is the member declaration rule itself; `public` is the visibility
    /// resolved from the member wrapper's modifiers (threaded via [`ChildHint`]).
    fn classify_type_member(
        &mut self,
        ctx: RuleNodeView<'_>,
        ri: usize,
        container: ContainerKind,
        public: bool,
    ) {
        match ri {
            // A field or event-field declaration can declare several variables
            // (`int a, b, c;` / `event E a, b;`). `const` is a modifier here
            // rather than a separate rule, so there is no `constant_declaration`
            // arm — a `const int a, b;` reaches this same path.
            cp::RULE_FIELD_DECLARATION | cp::RULE_EVENT_FIELD_DECLARATION => {
                let count = declarator_count(ctx).max(1);
                for _ in 0..count {
                    self.current().npa.record_attribute(container, public);
                }
            }
            // A named `event` with accessors declares exactly one member.
            cp::RULE_EVENT_DECLARATION => {
                self.current().npa.record_attribute(container, public);
            }
            // An `enum` member (`enum E { A, B }`) is a public constant field
            // of the enum → a public class attribute.
            cp::RULE_ENUM_MEMBER_DECLARATION => {
                self.current()
                    .npa
                    .record_attribute(ContainerKind::Class, true);
            }
            // Methods, constructors, operators, and the property/indexer forms
            // are all methods for NPM purposes (SonarC# counts a property as a
            // member of the type's public API).
            cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_PROPERTY_DECLARATION
            | cp::RULE_INDEXER_DECLARATION => {
                self.current().npm.record_method(container, public);
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------
// Free helpers (top-down tree inspection — no parent pointers).
// --------------------------------------------------------------------

/// Rules that open a type-like metric space (see `maybe_open_space`).
fn opens_type_like(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_CLASS_DECLARATION
            | cp::RULE_STRUCT_DECLARATION
            | cp::RULE_INTERFACE_DECLARATION
            | cp::RULE_ENUM_DECLARATION
            | cp::RULE_DELEGATE_DECLARATION
            // Roslyn models `record` / `record struct` as their own node rather
            // than a modifier on a class, so it is a peer here.
            | cp::RULE_RECORD_DECLARATION
            // A C# 14 `extension(T x) { … }` block is a member container in its own
            // right: it holds `member_declaration*` exactly as a class body does. It
            // has no name of its own, so the space is anonymous — but it must open
            // one, or its members would attach to the enclosing static class and
            // report as that class's methods.
            | cp::RULE_EXTENSION_BLOCK_DECLARATION
    )
}

/// Rules that open a function/closure metric space (mirrors the function arms
/// of `maybe_open_space`).
///
/// Roslyn declares one rule per accessor *list* entry rather than one per
/// accessor keyword, so a single `accessor_declaration` covers get/set/init/
/// add/remove — the keyword is a child token, not part of the rule name.
fn opens_function_space(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_LOCAL_FUNCTION_STATEMENT
            | cp::RULE_ACCESSOR_DECLARATION
            // Lambdas are split by parameter shape (`x => …` vs `(x, y) => …`),
            // and `delegate { … }` is a third node.
            | cp::RULE_SIMPLE_LAMBDA_EXPRESSION
            | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
            | cp::RULE_ANONYMOUS_METHOD_EXPRESSION
    )
}

/// Whether a lambda's body is a block (`… => { … }`) rather than an expression.
/// A block body's statements are counted individually for LLOC; an expression
/// body makes the lambda itself one logical line.
///
/// Roslyn writes the body as `(block | expression)` directly on the lambda rule,
/// so there is no `anonymous_function_body` wrapper to descend through.
fn lambda_body_is_block(ctx: RuleNodeView<'_>) -> bool {
    ctx.child_rule(cp::RULE_BLOCK).is_some()
}

/// Whether an `else_clause`'s body is a bare `if_statement` — i.e. this is an
/// `else if` chain rather than a nested `if` inside an `else` block.
///
/// Roslyn spells the else branch as its own `else_clause : KW_ELSE statement`
/// rule, so this is one direct-child check. (grammars-v4 wrote
/// `if_statement : IF (…) if_body (ELSE if_body)?`, which forced the walker to
/// find the `if_body` appearing *after* the `ELSE` terminal by index, then track
/// transparency through an `if_body`/`embedded_statement`/`statement` chain.)
///
/// A block stops the chain: `else { if … }` is genuinely nested, and so is a
/// statement with its own control-flow keyword.
fn else_clause_is_else_if(ctx: RuleNodeView<'_>) -> bool {
    ctx.child_rule(cp::RULE_STATEMENT)
        .and_then(|stmt| stmt.child_rule(cp::RULE_IF_STATEMENT))
        .is_some()
}

/// The declared name of a member/type: its first `identifier` child's covered
/// text. Falls back to the `member_name`/`method_member_name` wrapper's text
/// for the members that spell their name through one.
fn name_from_identifier(ctx: RuleNodeView<'_>) -> Option<String> {
    for child in ctx.children() {
        let Some(c) = child.as_rule() else { continue };
        // Roslyn spells every declared name as `identifier_token`; there is no
        // `member_name` / `method_member_name` indirection.
        if c.rule_index() == cp::RULE_IDENTIFIER_TOKEN {
            let t = c.text();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// The space name for a function-shaped node. Accessors have no name of their
/// own, so they are named `<owner>.get` / `.set` / `.add` / `.remove` from the
/// property/indexer/event name threaded down through [`ChildHint`].
fn function_name(ctx: RuleNodeView<'_>, hint: &ChildHint) -> Option<String> {
    // An expression-bodied property / indexer is its own implicit getter, named
    // to match the block form so `int P => 1;` and `int P { get { … } }` report
    // the same space name.
    if matches!(
        ctx.rule_index(),
        cp::RULE_PROPERTY_DECLARATION | cp::RULE_INDEXER_DECLARATION
    ) {
        let owner = if ctx.rule_index() == cp::RULE_INDEXER_DECLARATION {
            Some(SmolStr::new("this[]"))
        } else {
            name_from_identifier(ctx).map(SmolStr::new)
        };
        return Some(match owner {
            Some(name) => format!("{name}.get"),
            None => "get".to_string(),
        });
    }
    // One `accessor_declaration` covers every accessor kind, with the keyword as
    // a direct child token — so the kind is read rather than inferred from which
    // of five rules matched. That also picks up `init` (C# 9), which the
    // grammars-v4 shape had no rule for.
    if ctx.rule_index() == cp::RULE_ACCESSOR_DECLARATION {
        let suffix = if ctx.has_token(cl::KW_GET) {
            "get"
        } else if ctx.has_token(cl::KW_SET) {
            "set"
        } else if ctx.has_token(cl::KW_INIT) {
            "init"
        } else if ctx.has_token(cl::KW_ADD) {
            "add"
        } else if ctx.has_token(cl::KW_REMOVE) {
            "remove"
        } else {
            // The grammar also allows a bare `identifier_token` here, for
            // Roslyn's error-recovery shapes.
            "accessor"
        };
        return Some(match &hint.accessor_owner {
            Some(owner) => format!("{owner}.{suffix}"),
            None => suffix.to_string(),
        });
    }
    // An operator's name is `operator <op>`. Roslyn spells the operator as a
    // direct token choice on the declaration (`… KW_OPERATOR KW_CHECKED? (PLUS |
    // MINUS | …)`) rather than a separate `overloadable_operator` rule, so the
    // symbol is read from the token that follows `operator`. Using the token text
    // avoids mapping ~30 token types by hand, and `direct_terminals()` cannot
    // reach into the parameter list or body.
    if ctx.rule_index() == cp::RULE_OPERATOR_DECLARATION {
        let typed = OperatorDeclarationContext::from_rule_node(ctx)?;
        let mut seen_operator_keyword = false;
        for terminal in typed.direct_terminals() {
            let tt = terminal.symbol().token_type();
            if tt == cl::KW_OPERATOR {
                seen_operator_keyword = true;
            } else if seen_operator_keyword && tt != cl::KW_CHECKED {
                let symbol = terminal.symbol().text().unwrap_or_default();
                return Some(format!("operator {symbol}"));
            }
        }
        return Some("operator".to_string());
    }
    // A conversion operator is named by its target type (`implicit operator
    // int`), which is a rule child rather than a token.
    if ctx.rule_index() == cp::RULE_CONVERSION_OPERATOR_DECLARATION {
        return Some("operator".to_string());
    }
    // A lambda / anonymous method is anonymous.
    if matches!(
        ctx.rule_index(),
        cp::RULE_SIMPLE_LAMBDA_EXPRESSION
            | cp::RULE_PARENTHESIZED_LAMBDA_EXPRESSION
            | cp::RULE_ANONYMOUS_METHOD_EXPRESSION
    ) {
        return None;
    }
    // Every other function-shaped declaration — including a local function —
    // carries its own `identifier_token` directly, so there is no header rule to
    // descend into.
    name_from_identifier(ctx)
}

/// The property/indexer/event name to thread down to its accessors, if `ctx` is
/// one of those member forms.
fn accessor_owner_name(ctx: RuleNodeView<'_>, ri: usize) -> Option<SmolStr> {
    match ri {
        cp::RULE_PROPERTY_DECLARATION | cp::RULE_EVENT_DECLARATION => {
            name_from_identifier(ctx).map(SmolStr::new)
        }
        cp::RULE_INDEXER_DECLARATION => Some(SmolStr::new("this[]")),
        _ => None,
    }
}

/// Count the declared parameters of a function-shaped node.
///
/// Roslyn spells every parameter position with the same `parameter` rule, so one
/// lookup covers methods, constructors, operators, local functions, parenthesized
/// lambdas, and anonymous methods alike. (grammars-v4 needed five distinct shapes —
/// `fixed_parameter`, `parameter_array`, `arg_declaration`,
/// `explicit_anonymous_function_parameter`, and a bare identifier — because LL
/// parsing forced a separate rule per position.) A `params` array is a `KW_PARAMS`
/// modifier on an ordinary parameter, so it counts as one.
///
/// Two positions are not a `parameter_list` and need naming:
/// - `simple_lambda_expression : … identifier_token ARROW …` — the one parameter is
///   a bare identifier, with no `parameter` node at all.
/// - `accessor_declaration` — an accessor's arity is its owning *indexer*'s, which
///   lives on `indexer_declaration`, not on the accessor. Threaded down through
///   [`ChildHint::accessor_args`].
fn count_args(ctx: RuleNodeView<'_>, hint: &ChildHint) -> u32 {
    match ctx.rule_index() {
        // `x => …`: the single parameter is a bare `identifier_token`, so there is no
        // `parameter` child to count. Its arity is always exactly one — the grammar
        // has no zero- or multi-parameter form of this rule (`() => …` and
        // `(a, b) => …` are both `parenthesized_lambda_expression`).
        cp::RULE_SIMPLE_LAMBDA_EXPRESSION => 1,
        // An accessor of an indexer takes the indexer's parameters (`this[int i]`'s
        // getter is a one-argument function); a property's accessor takes none.
        // Either way the count comes from the owner, since `accessor_declaration`
        // carries no parameter list of its own — without this, NArgs for the *same*
        // indexer differed by body syntax, because the expression-bodied form opens
        // its space at `indexer_declaration` where the list IS present.
        cp::RULE_ACCESSOR_DECLARATION => hint.accessor_args,
        // An indexer's parameters are bracketed (`this[int i]`).
        _ => ctx
            .child_rule(cp::RULE_PARAMETER_LIST)
            .or_else(|| ctx.child_rule(cp::RULE_BRACKETED_PARAMETER_LIST))
            .map(count_parameters)
            .unwrap_or(0),
    }
}

/// Count the non-empty `parameter` children of `ctx`.
///
/// Every element of Roslyn's `parameter` rule is optional, so it matches the
/// empty string — `Zero()` parses as a `parameter_list` containing one *empty*
/// `parameter`. (The generator flags this as `G4A004`.) That is deliberate in
/// Roslyn's model, which has a node for every slot including absent ones, so the
/// walker filters rather than the grammar being changed.
fn count_parameters(ctx: RuleNodeView<'_>) -> u32 {
    ctx.child_rules(cp::RULE_PARAMETER)
        .filter(|parameter| parameter.child_count() > 0)
        .count() as u32
}

/// Count the declarators of a field / event declaration (`int a, b, c;`).
///
/// Roslyn has one `variable_declaration : type variable_declarator (','
/// variable_declarator)*` for all of them — no separate `constant_declarators`
/// list, since `const` is a modifier rather than a distinct declaration rule.
fn declarator_count(ctx: RuleNodeView<'_>) -> u32 {
    ctx.child_rule(cp::RULE_VARIABLE_DECLARATION)
        .map(|decl| decl.child_rules(cp::RULE_VARIABLE_DECLARATOR).count() as u32)
        .unwrap_or(0)
}

/// Resolve an explicit visibility from a member/type wrapper's
/// `all_member_modifiers`: `Some(true)` if it carries `public`, `Some(false)`
/// if it carries `private`/`protected`/`internal`, `None` if no access modifier
/// is present (caller applies the container default).
///
/// `internal` is *not* public: it is assembly-scoped, so it does not
/// contribute to the type's public API surface (NPA/NPM).
/// The rules that are a *real* member declaration — the ones carrying their own
/// `attribute_list* modifier*`, as opposed to the `base_*` dispatch alternations
/// above them.
fn declares_member(ri: usize) -> bool {
    matches!(
        ri,
        cp::RULE_FIELD_DECLARATION
            | cp::RULE_EVENT_FIELD_DECLARATION
            | cp::RULE_METHOD_DECLARATION
            | cp::RULE_CONSTRUCTOR_DECLARATION
            | cp::RULE_DESTRUCTOR_DECLARATION
            | cp::RULE_OPERATOR_DECLARATION
            | cp::RULE_CONVERSION_OPERATOR_DECLARATION
            | cp::RULE_PROPERTY_DECLARATION
            | cp::RULE_INDEXER_DECLARATION
            | cp::RULE_EVENT_DECLARATION
            | cp::RULE_DELEGATE_DECLARATION
            | cp::RULE_CLASS_DECLARATION
            | cp::RULE_STRUCT_DECLARATION
            | cp::RULE_INTERFACE_DECLARATION
            | cp::RULE_ENUM_DECLARATION
            | cp::RULE_RECORD_DECLARATION
    )
}

/// Resolve a declaration's access from its own `modifier*` children.
///
/// `Some(true)` for an explicit `public`, `Some(false)` when another access
/// modifier is present, `None` when the declaration states none — the caller
/// supplies the container's default, since an unmarked class member is private
/// while an unmarked interface member is public.
///
/// Roslyn puts `modifier*` directly on each declaration, so this reads the real
/// node rather than a wrapper. `modifier_children()` is typed and reaches only
/// direct children, so a modifier on a *nested* declaration cannot leak in.
fn visibility_from_modifiers(ctx: RuleNodeView<'_>) -> Option<bool> {
    let mut saw_non_public = false;
    for modifier in ctx.child_rules(cp::RULE_MODIFIER) {
        if modifier.has_token(cl::KW_PUBLIC) {
            return Some(true);
        }
        if modifier.has_token(cl::KW_PRIVATE)
            || modifier.has_token(cl::KW_PROTECTED)
            || modifier.has_token(cl::KW_INTERNAL)
            || modifier.has_token(cl::KW_FILE)
        {
            saw_non_public = true;
        }
    }
    saw_non_public.then_some(false)
}

fn container_kind(parent_kind: SpaceKind) -> ContainerKind {
    match parent_kind {
        SpaceKind::Class | SpaceKind::Impl | SpaceKind::Enum => ContainerKind::Class,
        SpaceKind::Interface | SpaceKind::Trait => ContainerKind::Interface,
        _ => ContainerKind::Other,
    }
}

/// Is this switch-expression arm the discard (`_ => …`) catch-all?
///
/// The discard is the fall-through, not a test, so it is not a decision — the same
/// treatment `default:` gets in a switch statement.
///
/// Roslyn does give the discard its own `discard_pattern : '_'` rule, but it cannot
/// be reached here: `constant_pattern : expression` is listed *before* it among
/// `pattern`'s alternatives and `_` is a legal identifier expression, so ANTLR takes
/// the constant form first. (Reordering is not an option — `discard_pattern` first
/// would be right for `_` yet the two rules are otherwise unrelated, and Roslyn's own
/// parser distinguishes them semantically, by knowing whether `_` resolves to a
/// declared name.) So the arm's pattern is matched on its text, which is exactly `_`
/// for the discard and cannot be anything else for a one-token pattern.
fn is_discard_arm(ctx: RuleNodeView<'_>) -> bool {
    SwitchExpressionArmContext::from_rule_node(ctx)
        .and_then(|arm| arm.pattern().ok())
        .is_some_and(|pattern| pattern.discard_pattern().is_some() || pattern.text() == "_")
}

/// Is this invocation-shaped `expression` the `nameof` pseudo-call?
///
/// `nameof` is a *contextual* keyword: the grammar has no rule for it, so
/// `nameof(x)` parses as an ordinary invocation over the identifier `nameof`. It is
/// nonetheless a compile-time operator — it yields a string constant, calls nothing,
/// and never evaluates its argument — so it must not count as an ABC branch.
///
/// The callee is the invocation's first `expression` child. Hub inlining collapses a
/// bare identifier callee all the way down (no `simple_name` layer survives on it),
/// so the check is on that child's own text. That is exact rather than a substring
/// probe: the child spans the callee and nothing else, so it equals `"nameof"` only
/// when the callee IS the bare identifier. A qualified `X.nameof(y)` has a `.` in the
/// child's text and a local variable named `nameof` is never in callee position, so
/// neither is mistaken for the operator.
///
/// Only ever consulted for a node that already has an `argument_list`, so the first
/// `expression` child is the callee by construction.
fn is_nameof_callee(expr: &ExpressionContext<'_>) -> bool {
    expr.expression_children()
        .next()
        .is_some_and(|callee| callee.text() == "nameof")
}

/// Equality / relational / boolean / null-coalescing operator tokens that count as
/// an ABC "condition".
///
/// Equality (`==`/`!=`), `&&`/`||` and `??` have dedicated tokens that appear
/// nowhere else, so they are safe on this cheap token scan. Relational `<`/`>` are
/// counted here too, but the caller must first exclude the two constructs that
/// reuse those tokens for something other than a comparison — a split shift
/// operator and a generic/function-pointer delimiter (see
/// [`ChildHint::in_shift_operator`] and [`ChildHint::in_type_delimiter`]).
///
/// `is`/`as` are NOT here: they are counted at the `expression` rule, where the
/// typed context distinguishes them from the `is`-pattern forms.
fn is_abc_condition_token(tt: i32) -> bool {
    matches!(
        tt,
        cl::EQ_EQ
            | cl::NE
            | cl::AMP_AMP
            | cl::PIPE_PIPE
            | cl::QUESTION_QUESTION
            // Relational comparisons. `>` is also half of a split `>>`, so the
            // caller must exclude tokens inside a `right_shift` rule.
            | cl::LT
            | cl::GT
            | cl::LE
            | cl::GE
    )
}

// --------------------------------------------------------------------
// Halstead token classification.
// --------------------------------------------------------------------

enum HalsteadClass {
    Operator,
    Operand,
    Skip,
}

/// Classify a token type as a Halstead operator, operand, or skipped.
///
/// Operands: identifiers, literals (including every interpolated-string content
/// token), `this`, `base`. Skipped: whitespace, comments, the BOM, the
/// inactive-`#if` `SKIPPED_SECTION`, and EOF. Everything else (keywords,
/// punctuation, operators) is an operator.
fn halstead_class(tt: i32) -> HalsteadClass {
    if matches!(
        tt,
        cl::IDENTIFIER
            | cl::DEC_INT_LIT
            | cl::HEX_INT_LIT
            | cl::BIN_INT_LIT
            | cl::REAL_LIT
            | cl::CHAR_LIT
            | cl::STRING_LIT
            | cl::VERBATIM_STRING_LIT
            | cl::ML_RAW_STRING_LIT
            | cl::SL_RAW_STRING_LIT
            | cl::KW_TRUE
            | cl::KW_FALSE
            | cl::KW_NULL
            | cl::KW_THIS
            | cl::KW_BASE
            // Every interpolated-string content piece — literal text, escapes,
            // doubled braces, and the format specifier — arrives as this one
            // token: the hand-written lexer's mode rules all `type(…)` to it.
            | cl::INTERPOLATED_TEXT
            | cl::XML_TEXT_LIT
            // The C# 11 UTF-8 literal suffix (`"text"u8`, either case). Real C#
            // lexes this as part of the literal token, and Roslyn's grammar spells
            // it as a separate trailing token only because it models the syntax
            // node that way. It is part of the *operand*, not an operator applied
            // to one — classifying it as an operator inflated the distinct-operator
            // count and made `"text"u8` cost more than `"text"`.
            | cl::KW_U8
            | cl::KW_U8_LOWER
    ) {
        return HalsteadClass::Operand;
    }

    if matches!(
        tt,
        cl::WHITESPACES
            | cl::BYTE_ORDER_MARK
            | cl::SINGLE_LINE_COMMENT
            | cl::DELIMITED_COMMENT
            | cl::SINGLE_LINE_DOC_COMMENT
            | cl::DELIMITED_DOC_COMMENT
            // mehen routes preprocessor directives to their own channel rather
            // than evaluating them, so a directive line is neither operator nor
            // operand for Halstead. (It IS a physical code line for PLOC — see
            // `collect_loc_tokens` — since the row carries source text.)
            | cl::DIRECTIVE_LINE
    ) || tt < 0
    {
        return HalsteadClass::Skip;
    }

    HalsteadClass::Operator
}

/// A stable string label for an operator token, used as its Halstead operator
/// key. The numeric token type is stable for a given generated grammar.
fn kp_token_name(tt: i32) -> String {
    format!("t{tt}")
}
