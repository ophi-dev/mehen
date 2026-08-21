// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Procedural SQL metrics (research foundation §6.17, Phase 3).
//!
//! PL/SQL, T-SQL, MySQL, and BigQuery-scripting control flow is measured by a
//! single dialect-agnostic **token state machine** rather than per-dialect
//! typed-node walks. The empirical basis (parser comparison §9, probed on
//! sqruff v0.40.0):
//!
//! - Oracle bodies parse into rich typed nodes (`OracleIfThenStatement`,
//!   `WhileLoopStatement`, …), but their leaves are ordinary `Keyword` tokens;
//! - T-SQL bodies parse as keyword-led `Statement` shapes or spill into
//!   top-level `Unparsable` runs;
//! - MySQL routine bodies are one `Unparsable` run;
//! - tokens inside `Unparsable` stay classified (`Word`/`SingleQuote`/
//!   `InlineComment`), so keyword scanning is trivia-safe: a comment
//!   `-- exec this` or a literal `'goto'` can never false-match.
//!
//! One machine over the classified token stream therefore covers all four
//! families uniformly, cannot double-count a construct that happens to be
//! typed *and* keyword-visible, and degrades gracefully exactly where the
//! parser does. This is the "linter-grade" procedural depth the research
//! foundation scopes for Phase 3 — deeper T-SQL semantics would go through
//! the ANTLR `tsql` reserve path (parser comparison §7.4), not through more
//! token heuristics.
//!
//! ## What is measured where
//!
//! The machine scans three region kinds:
//!
//! 1. **Routine definitions** (statements classified `procedural`):
//!    `CREATE PROCEDURE`/`FUNCTION`/`TRIGGER`, package/type bodies.
//! 2. **Anonymous blocks / scripting statements** (statements classified
//!    `anonymous_block`): `DECLARE … BEGIN … END`, T-SQL `IF`/`WHILE`/
//!    `BEGIN`-led batch statements, BigQuery scripting statements.
//! 3. **Top-level `Unparsable` runs** outside those statements — this is
//!    where T-SQL procedure bodies spill — gated by a marker pre-scan so a
//!    broken `SELECT` never grows procedural metrics.
//!
//! Cyclomatic complexity follows Sonar's documented PL/SQL increments
//! (research foundation §3.1): +1 per routine/anonymous block entry, `IF`,
//! `ELSIF`, loop, `CASE`-statement `WHEN` arm, exception handler (`WHEN …
//! THEN` handler / `BEGIN CATCH`), `EXIT WHEN`/`CONTINUE WHEN`, raise/throw,
//! and boolean `AND`/`OR` inside bodies. One documented deviation: `WHEN`
//! arms of CASE **expressions** are *not* counted here — they already belong
//! to the declarative `sql.case.*` family, and mehen keeps the declarative
//! and procedural families disjoint where Sonar has a single number.
//!
//! Cognitive complexity mirrors the spirit of code cognitive complexity:
//! control structures cost `1 + nesting`, flat `ELSIF`/`ELSE` branches cost
//! 1, `GOTO` costs 1, and boolean operator *sequences* (not individual
//! operators) cost 1 each.
//!
//! Every increment emits [`ProceduralEvidence`] so the published metric is
//! the sum of its evidence by construction (the crate-wide explainability
//! invariant, `tests/contributions.rs`).

use mehen_core::SourceSpan;
use sqruff_lib_core::dialects::syntax::{SyntaxKind, SyntaxSet};
use sqruff_lib_core::parser::segments::ErasedSegment;

use crate::facts::{ChangeRiskEvidence, ChangeRiskFactor, SqlFileFacts, StatementKind};

/// Which composite a piece of procedural evidence contributes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProceduralMetric {
    Cyclomatic,
    Cognitive,
    /// `sql.structural_complexity.max_embedded_query` — one entry, for the
    /// winning routine.
    EmbeddedQueryMax,
}

/// One source-resolved increment of a procedural composite. `amount` is the
/// full contribution of the construct (for cognitive that is `1 + nesting`),
/// so `metric value == Σ evidence.amount` holds by construction.
#[derive(Clone, Debug)]
pub(crate) struct ProceduralEvidence {
    pub span: SourceSpan,
    pub metric: ProceduralMetric,
    pub amount: f64,
    pub reason: &'static str,
}

/// Aggregated procedural facts for one file (research foundation §6.17).
#[derive(Clone, Debug, Default)]
pub(crate) struct ProceduralFacts {
    /// `BEGIN … END` block openers (plain blocks, `BEGIN TRY`, `BEGIN CATCH`;
    /// transaction-control `BEGIN` excluded).
    pub block_count: u32,
    /// Routine definitions — mirrors `procedural_units.len()`.
    pub routine_count: u32,
    pub cyclomatic_complexity: f64,
    pub cognitive_complexity: f64,
    /// Deepest `BEGIN … END` nesting observed.
    pub max_block_depth: u32,
    /// Loops of any flavor: `LOOP`, `WHILE`, `FOR … LOOP/DO`, `REPEAT`.
    pub loop_count: u32,
    /// `IF` statements plus `ELSIF`/`ELSEIF` branches.
    pub if_count: u32,
    /// Procedural `CASE` **statements** (closed by `END CASE`) — CASE
    /// expressions stay in the declarative `sql.case.*` family.
    pub case_statement_count: u32,
    /// PL/SQL `EXCEPTION WHEN … THEN` handlers and T-SQL `BEGIN CATCH`.
    pub exception_handler_count: u32,
    pub return_count: u32,
    /// `RAISE`, `RAISE_APPLICATION_ERROR`, `THROW`, `RAISERROR`, `SIGNAL`,
    /// `RESIGNAL`.
    pub raise_throw_count: u32,
    /// Dynamic SQL: `EXECUTE IMMEDIATE`, `sp_executesql`, `EXEC(…)`,
    /// `DBMS_SQL` usage.
    pub dynamic_sql_count: u32,
    /// Max `sql.structural_complexity` over the query facts embedded in a
    /// single routine (§9.3 `sql.structural_complexity.max_embedded_query`).
    pub max_embedded_query_structural: f64,
    /// Every cyclomatic/cognitive increment with span and reason.
    pub evidence: Vec<ProceduralEvidence>,
}

/// Evidence reason codes (stable public identifiers).
mod reason {
    pub(crate) const ENTRY: &str = "sql.procedural.entry";
    pub(crate) const IF: &str = "sql.procedural.if";
    pub(crate) const ELSIF: &str = "sql.procedural.elsif";
    pub(crate) const ELSE: &str = "sql.procedural.else";
    pub(crate) const LOOP: &str = "sql.procedural.loop";
    pub(crate) const CASE_STATEMENT: &str = "sql.procedural.case_statement";
    pub(crate) const CASE_WHEN: &str = "sql.procedural.case_when";
    pub(crate) const EXCEPTION_HANDLER: &str = "sql.procedural.exception_handler";
    pub(crate) const CONDITIONAL_EXIT: &str = "sql.procedural.conditional_exit";
    pub(crate) const RAISE_THROW: &str = "sql.procedural.raise_throw";
    pub(crate) const GOTO: &str = "sql.procedural.goto";
    pub(crate) const BOOLEAN_SEQUENCE: &str = "sql.procedural.boolean_sequence";
    pub(crate) const BOOLEAN_OPERATOR: &str = "sql.procedural.boolean_operator";
    pub(crate) const EMBEDDED_QUERY: &str = "sql.procedural.embedded_query";
}

// ── token model ────────────────────────────────────────────────────────

/// One classified code token (trivia excluded) with its source position.
struct PToken {
    /// Uppercased raw text.
    word: String,
    /// Whether the lexer classified it as keyword-like. `Keyword` in parsed
    /// regions; `Word` in `Unparsable` runs (where everything keyword-shaped
    /// lexes as `Word`); `FunctionNameIdentifier` so `RAISE_APPLICATION_ERROR`
    /// and `sp_executesql` count when they parse as calls. A `NakedIdentifier`
    /// (e.g. a column named `raise` in parsed SQL) is *not* keyword-like and
    /// can never trip the machine.
    keyword_like: bool,
    /// Whether the token is a parsed function-call name
    /// (`FunctionNameIdentifier`) — distinguishes the scalar `IF(…)` function
    /// from a statement-level `IF (cond)` with a parenthesized condition.
    is_function_name: bool,
    span: SourceSpan,
}

/// Flatten the classified leaf tokens of `region`, excluding comments,
/// whitespace, and meta tokens.
fn tokens_of(region: &ErasedSegment, line_at: &impl Fn(u32) -> u32) -> Vec<PToken> {
    fn walk(node: &ErasedSegment, line_at: &impl Fn(u32) -> u32, out: &mut Vec<PToken>) {
        let children = node.segments();
        if children.is_empty() {
            if node.is_comment() || node.is_whitespace() || node.is_meta() {
                return;
            }
            let raw = node.raw();
            let raw = raw.trim();
            if raw.is_empty() {
                return;
            }
            let kind = node.get_type();
            let span = node
                .get_position_marker()
                .map(|pm| {
                    let start = pm.source_slice.start as u32;
                    let end = pm.source_slice.end as u32;
                    SourceSpan::new(start, end, line_at(start), line_at(end.saturating_sub(1)))
                })
                .unwrap_or_else(SourceSpan::empty);
            out.push(PToken {
                word: raw.to_ascii_uppercase(),
                keyword_like: matches!(
                    kind,
                    SyntaxKind::Keyword | SyntaxKind::Word | SyntaxKind::FunctionNameIdentifier
                ),
                is_function_name: kind == SyntaxKind::FunctionNameIdentifier,
                span,
            });
            return;
        }
        for child in children {
            walk(child, line_at, out);
        }
    }
    let mut out = Vec::new();
    walk(region, line_at, &mut out);
    out
}

// ── region collection ──────────────────────────────────────────────────

const UNPARSABLE: SyntaxSet = SyntaxSet::single(SyntaxKind::Unparsable);
const MULTI_STATEMENT: SyntaxSet = SyntaxSet::single(SyntaxKind::MultiStatementSegment);
const SELECT_STATEMENT: SyntaxSet = SyntaxSet::single(SyntaxKind::SelectStatement);

/// Whether an `Unparsable` run looks procedural. Gate before scanning so a
/// broken `SELECT` (or any non-procedural parse failure) never grows
/// procedural metrics. Markers are chosen to be unambiguous: reserved control
/// keywords and closer pairs that cannot appear in declarative SQL.
fn unparsable_is_procedural(tokens: &[PToken]) -> bool {
    let word = |i: usize| tokens.get(i).map(|t| t.word.as_str()).unwrap_or("");
    for (i, t) in tokens.iter().enumerate() {
        match t.word.as_str() {
            // `BEGIN` (block, not `BEGIN TRANSACTION`) is procedural context.
            "BEGIN"
                if !matches!(
                    word(i + 1),
                    "TRANSACTION" | "TRAN" | "WORK" | "DISTRIBUTED" | ";"
                ) =>
            {
                return true;
            }
            // Construct closers that only procedural dialects produce.
            "END" if matches!(word(i + 1), "IF" | "LOOP" | "WHILE" | "CASE" | "REPEAT") => {
                return true;
            }
            "ELSIF" | "ELSEIF" => return true,
            "SP_EXECUTESQL" => return true,
            "EXECUTE" if word(i + 1) == "IMMEDIATE" => return true,
            // T-SQL `EXEC('…')` — an immediately executed dynamic string
            // batch (Codex P1). Plain `EXEC procname` is deliberately not a
            // marker: a static call proves nothing procedural by itself.
            "EXEC" | "EXECUTE" if word(i + 1) == "(" => return true,
            // An exception-handler section (PL/SQL, BigQuery scripting).
            "EXCEPTION" if word(i + 1) == "WHEN" => return true,
            "WHILE" => return true,
            _ => {}
        }
    }
    false
}

// ── the state machine ──────────────────────────────────────────────────

/// Open construct contexts. Plain `Block` tracks `BEGIN … END` depth and
/// whether its `EXCEPTION` section has started; `Case` tracks pending `WHEN`
/// arms until `END CASE` (statement) or bare `END` (expression) resolves
/// whether they count.
#[derive(Debug)]
enum Ctx {
    Block {
        exception_section: bool,
    },
    Try,
    Catch,
    /// `bound` = a `BEGIN` block opened directly under this (T-SQL) `IF`, so
    /// the `IF` closes when that block closes, not at the next terminator.
    If {
        with_then: bool,
        bound: bool,
    },
    /// Pushed at the loop *header* (`WHILE`/`FOR`) or at a bare `LOOP`, so
    /// the body carries the loop's nesting regardless of body shape:
    /// `block_bound` loops (T-SQL `WHILE … BEGIN … END`) pop with their
    /// block; header loops whose body opener was `LOOP`/`DO` pop at
    /// `END LOOP/WHILE/REPEAT/FOR`; a loop still pending a body opener at
    /// `;` was single-statement (`WHILE @x > 0 SET …;`) and pops there.
    Loop {
        block_bound: bool,
    },
    Case {
        whens: Vec<SourceSpan>,
        nesting: u32,
        open_span: SourceSpan,
    },
    /// One `WHEN … THEN` exception-handler body (PL/SQL); popped by the next
    /// handler or the enclosing block's `END`.
    Handler,
}

/// Per-region scanner state.
struct Machine<'a> {
    facts: &'a mut ProceduralFacts,
    /// Byte-range buckets for per-unit attribution: `(start, end, index)`.
    unit_ranges: &'a [(u32, u32, usize)],
    /// Per-unit `(cyclomatic, cognitive)` tallies, parallel to
    /// `SqlFileFacts::procedural_units`.
    unit_tallies: &'a mut [(f64, f64)],
    change_risk: &'a mut Vec<ChangeRiskEvidence>,
    /// Mirrors `AnalysisConfig::emit_contributions`: counts and tallies are
    /// always exact; the evidence vectors are only populated when a consumer
    /// will read them (same gating as the change-risk evidence in
    /// `extract_objects`).
    emit: bool,
    /// Unit index increments fall back to when no unit range *contains*
    /// their span: the routine whose body sqruff split into sibling
    /// statements or top-level `Unparsable` spills (the continuation
    /// regions). `None` for standalone regions (anonymous blocks, top-level
    /// scripting) — their increments are file-level only (Codex P2).
    fallback_unit: Option<usize>,
    stack: Vec<Ctx>,
    /// Set once the region enters a body (`BEGIN`/`IS`/`AS` seen). Gates the
    /// text-only patterns (`RETURN`, raise family, booleans, dynamic SQL) so
    /// routine headers (`RETURN number IS`, `CREATE OR REPLACE`) and
    /// identifier-shaped words in non-body positions never count.
    in_body: bool,
    /// A `WHILE`/`FOR` header was seen; the next `LOOP`/`DO` opens its body
    /// rather than a separate loop.
    pending_loop_header: bool,
    /// Inside `BETWEEN … AND …`: the next `AND` is not a boolean operator.
    pending_between: bool,
    /// Previous code token was a boolean operator with this text (`AND`/
    /// `OR`); any other token breaks the run. Used for sequence-based
    /// cognitive counting (+1 per run of like operators, +1 on alternation).
    last_bool: Option<&'static str>,
}

impl Machine<'_> {
    fn nesting(&self) -> u32 {
        self.stack
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    Ctx::If { .. }
                        | Ctx::Loop { .. }
                        | Ctx::Case { .. }
                        | Ctx::Catch
                        | Ctx::Handler
                )
            })
            .count() as u32
    }

    fn block_depth(&self) -> u32 {
        self.stack
            .iter()
            .filter(|c| matches!(c, Ctx::Block { .. } | Ctx::Try | Ctx::Catch))
            .count() as u32
    }

    fn add(
        &mut self,
        metric: ProceduralMetric,
        span: SourceSpan,
        amount: f64,
        reason: &'static str,
    ) {
        match metric {
            ProceduralMetric::Cyclomatic => self.facts.cyclomatic_complexity += amount,
            ProceduralMetric::Cognitive => self.facts.cognitive_complexity += amount,
            ProceduralMetric::EmbeddedQueryMax => {}
        }
        // Innermost containing unit: units are pre-order, so among containing
        // ranges the *last* is the deepest. Increments in continuation
        // regions (split bodies, unparsable spills) lie *outside* every unit
        // range and attribute to the owning routine via the region's
        // fallback (Codex P2).
        let unit = self
            .unit_ranges
            .iter()
            .rfind(|(s, e, _)| *s <= span.start_byte && span.end_byte <= *e)
            .map(|&(_, _, idx)| idx)
            .or(self.fallback_unit);
        if let Some(idx) = unit {
            match metric {
                ProceduralMetric::Cyclomatic => self.unit_tallies[idx].0 += amount,
                ProceduralMetric::Cognitive => self.unit_tallies[idx].1 += amount,
                ProceduralMetric::EmbeddedQueryMax => {}
            }
        }
        if self.emit {
            self.facts.evidence.push(ProceduralEvidence {
                span,
                metric,
                amount,
                reason,
            });
        }
    }

    fn cyclo(&mut self, span: SourceSpan, reason: &'static str) {
        self.add(ProceduralMetric::Cyclomatic, span, 1.0, reason);
    }

    fn cognitive(&mut self, span: SourceSpan, amount: f64, reason: &'static str) {
        self.add(ProceduralMetric::Cognitive, span, amount, reason);
    }

    /// Pop the topmost context matching `pred` and everything above it
    /// (forgiving on malformed/unparsable input: contexts left open by a
    /// parse gap are abandoned rather than corrupting deeper state).
    fn pop_matching(&mut self, pred: impl Fn(&Ctx) -> bool) -> Option<Ctx> {
        let idx = self.stack.iter().rposition(pred)?;
        let ctx = self.stack.swap_remove(idx);
        self.stack.truncate(idx);
        Some(ctx)
    }

    /// A statement terminator closes any T-SQL-style `IF` (no `THEN`, no
    /// bound block) sitting on top of the stack.
    fn close_unbound_ifs(&mut self) {
        while matches!(
            self.stack.last(),
            Some(Ctx::If {
                with_then: false,
                bound: false
            })
        ) {
            self.stack.pop();
        }
    }

    fn break_bool_run(&mut self) {
        self.last_bool = None;
    }

    fn scan(&mut self, tokens: &[PToken], body_from_start: bool) {
        self.in_body = body_from_start;
        let word = |i: usize| tokens.get(i).map(|t| t.word.as_str()).unwrap_or("");

        let mut i = 0usize;
        while i < tokens.len() {
            let t = &tokens[i];
            let kw = t.keyword_like;
            match t.word.as_str() {
                ";" => {
                    self.close_unbound_ifs();
                    // A loop header still pending a body opener at the
                    // terminator was a single-statement loop (`WHILE @x > 0
                    // SET …;`) — its Loop context closes here (Codex P2).
                    if self.pending_loop_header {
                        self.pending_loop_header = false;
                        self.pop_matching(|c| matches!(c, Ctx::Loop { block_bound: false }));
                    }
                    self.pending_between = false;
                    self.break_bool_run();
                }
                "IS" | "AS" if kw => {
                    self.in_body = true;
                    self.break_bool_run();
                }
                "BEGIN" if kw => {
                    self.break_bool_run();
                    match word(i + 1) {
                        // Transaction control, not a block.
                        "TRANSACTION" | "TRAN" | "WORK" | "DIALOG" | "DISTRIBUTED" | ";" => {}
                        "TRY" => {
                            self.in_body = true;
                            self.facts.block_count += 1;
                            self.stack.push(Ctx::Try);
                            self.facts.max_block_depth =
                                self.facts.max_block_depth.max(self.block_depth());
                            i += 1; // consume TRY
                        }
                        "CATCH" => {
                            self.in_body = true;
                            self.facts.block_count += 1;
                            self.facts.exception_handler_count += 1;
                            let nesting = self.nesting();
                            self.cyclo(t.span, reason::EXCEPTION_HANDLER);
                            self.cognitive(t.span, 1.0 + nesting as f64, reason::EXCEPTION_HANDLER);
                            self.stack.push(Ctx::Catch);
                            self.facts.max_block_depth =
                                self.facts.max_block_depth.max(self.block_depth());
                            i += 1; // consume CATCH
                        }
                        _ => {
                            self.in_body = true;
                            self.facts.block_count += 1;
                            // The block is a loop body when a loop header is
                            // pending (T-SQL `WHILE … BEGIN`): the Loop
                            // context pushed at the header stays open and
                            // binds to this block, closing with it. Otherwise
                            // a block opening directly under a fresh T-SQL IF
                            // binds to that IF: it closes with the block.
                            if self.pending_loop_header {
                                self.pending_loop_header = false;
                                if let Some(Ctx::Loop { block_bound }) = self.stack.last_mut() {
                                    *block_bound = true;
                                }
                            } else if let Some(Ctx::If {
                                with_then: false,
                                bound,
                            }) = self.stack.last_mut()
                            {
                                *bound = true;
                            }
                            self.stack.push(Ctx::Block {
                                exception_section: false,
                            });
                            self.facts.max_block_depth =
                                self.facts.max_block_depth.max(self.block_depth());
                        }
                    }
                }
                "END" if kw => {
                    self.break_bool_run();
                    match word(i + 1) {
                        "IF" => {
                            self.pop_matching(|c| {
                                matches!(
                                    c,
                                    Ctx::If {
                                        with_then: true,
                                        ..
                                    }
                                )
                            });
                            i += 1;
                        }
                        "LOOP" | "WHILE" | "REPEAT" | "FOR" => {
                            self.pop_matching(|c| matches!(c, Ctx::Loop { .. }));
                            i += 1;
                        }
                        "CASE" => {
                            // `END CASE` proves the CASE was a *statement*:
                            // count it and its WHEN arms.
                            if let Some(Ctx::Case {
                                whens,
                                nesting,
                                open_span,
                            }) = self.pop_matching(|c| matches!(c, Ctx::Case { .. }))
                            {
                                self.facts.case_statement_count += 1;
                                self.cognitive(
                                    open_span,
                                    1.0 + nesting as f64,
                                    reason::CASE_STATEMENT,
                                );
                                for when_span in whens {
                                    self.cyclo(when_span, reason::CASE_WHEN);
                                }
                            }
                            i += 1;
                        }
                        "TRY" => {
                            self.pop_matching(|c| matches!(c, Ctx::Try));
                            i += 1;
                        }
                        "CATCH" => {
                            self.pop_matching(|c| matches!(c, Ctx::Catch));
                            i += 1;
                        }
                        _ => {
                            // Bare END closes the nearest block or CASE
                            // *expression* (whose WHEN arms stay
                            // declarative).
                            if let Some(Ctx::Block { .. }) = self.pop_matching(|c| {
                                matches!(
                                    c,
                                    Ctx::Block { .. } | Ctx::Case { .. } | Ctx::Try | Ctx::Catch
                                )
                            }) {
                                // A loop whose body was this block closes
                                // with it.
                                while matches!(
                                    self.stack.last(),
                                    Some(Ctx::Loop { block_bound: true })
                                ) {
                                    self.stack.pop();
                                }
                                // A T-SQL IF bound to this block closes too —
                                // unless an ELSE follows: the IF's decision
                                // continues through the else branch, keeping
                                // its nesting for the else body (Codex P2).
                                if word(i + 1) != "ELSE" {
                                    while matches!(
                                        self.stack.last(),
                                        Some(Ctx::If {
                                            with_then: false,
                                            bound: true
                                        })
                                    ) {
                                        self.stack.pop();
                                    }
                                }
                            }
                        }
                    }
                }
                "IF" if kw => {
                    self.break_bool_run();
                    let prev = i
                        .checked_sub(1)
                        .map(|j| tokens[j].word.as_str())
                        .unwrap_or("");
                    // `DROP TABLE IF EXISTS` / `CREATE TABLE IF NOT EXISTS`
                    // guards and the `IF(…)` conditional *function* are not
                    // control flow. The function is recognized by its parse
                    // shape (`FunctionNameIdentifier`), not by a following
                    // `(` — `IF (@count > 0) BEGIN … END` / `IF (ready)
                    // THEN` are ordinary statements with parenthesized
                    // conditions and must count (Codex P2). In unparsable
                    // runs everything is a `Word`, so a scalar `IF(…)` there
                    // counts as a branch — erring toward keeping control
                    // flow visible.
                    let ddl_guard = matches!(
                        prev,
                        "TABLE"
                            | "VIEW"
                            | "INDEX"
                            | "SCHEMA"
                            | "DATABASE"
                            | "FUNCTION"
                            | "PROCEDURE"
                            | "TRIGGER"
                            | "SEQUENCE"
                            | "COLUMN"
                            | "CONSTRAINT"
                            | "EXTENSION"
                            | "TYPE"
                            | "ROLE"
                            | "USER"
                            | "EXISTS"
                    );
                    if !ddl_guard && !t.is_function_name {
                        self.facts.if_count += 1;
                        let nesting = self.nesting();
                        self.cyclo(t.span, reason::IF);
                        self.cognitive(t.span, 1.0 + nesting as f64, reason::IF);
                        self.stack.push(Ctx::If {
                            with_then: false,
                            bound: false,
                        });
                    }
                }
                "THEN" if kw => {
                    self.break_bool_run();
                    if let Some(Ctx::If { with_then, .. }) = self.stack.last_mut() {
                        *with_then = true;
                    }
                }
                "ELSIF" | "ELSEIF" if kw => {
                    self.break_bool_run();
                    self.facts.if_count += 1;
                    self.cyclo(t.span, reason::ELSIF);
                    self.cognitive(t.span, 1.0, reason::ELSIF);
                }
                "ELSE" if kw => {
                    self.break_bool_run();
                    // ELSE of a CASE (expression or statement) is not a
                    // control-flow branch here. Any other ELSE in a
                    // procedural region is an IF-else — including the T-SQL
                    // `IF … BEGIN … END ELSE …` shape, where the IF context
                    // was already closed together with its bound block — and
                    // costs 1 cognitive (flat, per the cognitive model).
                    if !matches!(self.stack.last(), Some(Ctx::Case { .. })) {
                        self.cognitive(t.span, 1.0, reason::ELSE);
                    }
                }
                "CASE" if kw => {
                    self.break_bool_run();
                    let nesting = self.nesting();
                    self.stack.push(Ctx::Case {
                        whens: Vec::new(),
                        nesting,
                        open_span: t.span,
                    });
                }
                "WHEN" if kw => {
                    self.break_bool_run();
                    // MERGE clauses (`WHEN MATCHED` / `WHEN NOT MATCHED`) are
                    // declarative. Only that exact token shape is excluded —
                    // a searched CASE arm like `WHEN NOT done THEN …` is a
                    // real branch (Codex P2).
                    if word(i + 1) == "MATCHED"
                        || (word(i + 1) == "NOT" && word(i + 2) == "MATCHED")
                    {
                        i += 1;
                        continue;
                    }
                    match self.stack.last_mut() {
                        Some(Ctx::Case { whens, .. }) => whens.push(t.span),
                        Some(Ctx::Block {
                            exception_section: true,
                            ..
                        }) => {
                            self.facts.exception_handler_count += 1;
                            let nesting = self.nesting();
                            self.cyclo(t.span, reason::EXCEPTION_HANDLER);
                            self.cognitive(t.span, 1.0 + nesting as f64, reason::EXCEPTION_HANDLER);
                            self.stack.push(Ctx::Handler);
                        }
                        Some(Ctx::Handler) => {
                            // Next handler of the same exception section.
                            self.stack.pop();
                            self.facts.exception_handler_count += 1;
                            let nesting = self.nesting();
                            self.cyclo(t.span, reason::EXCEPTION_HANDLER);
                            self.cognitive(t.span, 1.0 + nesting as f64, reason::EXCEPTION_HANDLER);
                            self.stack.push(Ctx::Handler);
                        }
                        _ => {}
                    }
                }
                "EXCEPTION" if kw => {
                    self.break_bool_run();
                    // PL/SQL & BigQuery: the block's handler section starts.
                    // A Handler context from a previous section cannot be on
                    // top here, so marking the nearest block suffices. In an
                    // unparsable *fragment* the opening BEGIN may be lost —
                    // seed a block so the section's handlers still count.
                    match self
                        .stack
                        .iter_mut()
                        .rev()
                        .find(|c| matches!(c, Ctx::Block { .. }))
                    {
                        Some(Ctx::Block {
                            exception_section, ..
                        }) => *exception_section = true,
                        _ => self.stack.push(Ctx::Block {
                            exception_section: true,
                        }),
                    }
                }
                "EXIT" | "CONTINUE" if kw => {
                    self.break_bool_run();
                    // `EXIT WHEN <cond>` / `CONTINUE WHEN <cond>` embed a
                    // condition: one extra path each. The WHEN is consumed so
                    // the handler-WHEN logic never sees it. Bare
                    // EXIT/CONTINUE/BREAK add no path.
                    if word(i + 1) == "WHEN" {
                        self.cyclo(t.span, reason::CONDITIONAL_EXIT);
                        self.cognitive(t.span, 1.0, reason::CONDITIONAL_EXIT);
                        i += 1;
                    }
                }
                "LOOP" if kw => {
                    self.break_bool_run();
                    if self.pending_loop_header {
                        // Body opener of a WHILE/FOR header — its Loop
                        // context is already on the stack.
                        self.pending_loop_header = false;
                    } else {
                        self.count_loop(t.span);
                        self.stack.push(Ctx::Loop { block_bound: false });
                    }
                }
                "DO" if kw => {
                    self.break_bool_run();
                    // MySQL/BigQuery `WHILE … DO` / `FOR … DO` body opener —
                    // the header's Loop context is already on the stack.
                    if self.pending_loop_header {
                        self.pending_loop_header = false;
                    }
                }
                "WHILE" if kw => {
                    self.break_bool_run();
                    self.count_loop(t.span);
                    // The Loop context opens at the *header*, so the body
                    // carries the loop's nesting in every shape: PL/SQL
                    // `WHILE … LOOP … END LOOP`, MySQL `WHILE … DO … END
                    // WHILE`, T-SQL `WHILE … BEGIN … END` (the block binds
                    // to it), and T-SQL single-statement `WHILE … IF …;`
                    // (closed by the terminator) (Codex P2).
                    self.stack.push(Ctx::Loop { block_bound: false });
                    self.pending_loop_header = true;
                }
                "REPEAT" if kw => {
                    self.break_bool_run();
                    // MySQL `REPEAT … UNTIL … END REPEAT`; `REPEAT(…)` is the
                    // string function.
                    if word(i + 1) != "(" {
                        self.count_loop(t.span);
                        self.stack.push(Ctx::Loop { block_bound: false });
                    }
                }
                "FOR" if kw => {
                    self.break_bool_run();
                    // A for-loop header (`FOR i IN 1..10 LOOP`, `FOR rec IN
                    // (…) DO`) is a loop variable followed by IN. The
                    // variable lexes as `NakedIdentifier` in parsed regions
                    // and as `Word` in unparsable runs, so the discriminator
                    // is the word shape, not the token kind. Everything else
                    // (`FOR EACH ROW`, `FOR UPDATE`, `CURSOR … FOR SELECT`,
                    // `FOR XML`) is not a loop.
                    if !matches!(
                        word(i + 1),
                        "EACH"
                            | "UPDATE"
                            | "SELECT"
                            | "XML"
                            | "JSON"
                            | "BROWSE"
                            | "SHARE"
                            | "KEY"
                            | "NO"
                            | "DELETE"
                            | "INSERT"
                            | "("
                    ) && word(i + 2) == "IN"
                    {
                        self.count_loop(t.span);
                        self.stack.push(Ctx::Loop { block_bound: false });
                        self.pending_loop_header = true;
                    }
                }
                "GOTO" if kw => {
                    self.break_bool_run();
                    if self.in_body {
                        self.cognitive(t.span, 1.0, reason::GOTO);
                    }
                }
                "RETURN" if kw => {
                    self.break_bool_run();
                    // Only inside a body: `CREATE FUNCTION … RETURN number IS`
                    // declares the return *type* before the body starts.
                    if self.in_body {
                        self.facts.return_count += 1;
                    }
                }
                "RAISE"
                | "RAISE_APPLICATION_ERROR"
                | "THROW"
                | "RAISERROR"
                | "SIGNAL"
                | "RESIGNAL"
                    if kw =>
                {
                    self.break_bool_run();
                    if self.in_body {
                        self.facts.raise_throw_count += 1;
                        self.cyclo(t.span, reason::RAISE_THROW);
                    }
                }
                "EXECUTE" | "EXEC" if kw => {
                    self.break_bool_run();
                    // A qualified method call (`DBMS_SQL.EXECUTE(c)`) is not
                    // the T-SQL `EXEC(…)` string form — the package
                    // qualifier already counted it (Codex P2).
                    let qualified = i
                        .checked_sub(1)
                        .map(|j| tokens[j].word.as_str() == ".")
                        .unwrap_or(false);
                    if self.in_body && !qualified {
                        if word(i + 1) == "IMMEDIATE" {
                            self.count_dynamic_sql(t.span);
                            i += 1;
                        } else if word(i + 1) == "(" {
                            // T-SQL `EXEC('…')` executes a string.
                            self.count_dynamic_sql(t.span);
                        } else if word(i + 1) == "SP_EXECUTESQL" {
                            self.count_dynamic_sql(t.span);
                            i += 1;
                        }
                        // Plain `EXEC procname` is a static call — no count.
                    }
                }
                "SP_EXECUTESQL" if kw => {
                    // Reached only without a preceding EXEC/EXECUTE (which
                    // consumes it above).
                    self.break_bool_run();
                    if self.in_body {
                        self.count_dynamic_sql(t.span);
                    }
                }
                "PREPARE" if kw => {
                    // MySQL dynamic SQL: `PREPARE stmt FROM @sql`. The
                    // matching `EXECUTE stmt` deliberately does not count —
                    // the dynamic statement is counted once, at its
                    // definition site.
                    self.break_bool_run();
                    if self.in_body && word(i + 2) == "FROM" {
                        self.count_dynamic_sql(t.span);
                    }
                }
                "DBMS_SQL" => {
                    // The Oracle dynamic-SQL package. In a parsed call
                    // (`DBMS_SQL.PARSE(…)`) the package qualifier lexes as a
                    // `NakedIdentifier` — not keyword-like — so this arm
                    // deliberately has no `kw` guard (Codex P2).
                    self.break_bool_run();
                    if self.in_body {
                        self.count_dynamic_sql(t.span);
                    }
                }
                "BETWEEN" if kw => {
                    self.break_bool_run();
                    self.pending_between = true;
                }
                "AND" | "OR" => {
                    // Booleans count in bodies only (kw check is implicit:
                    // AND/OR lex as Keyword/BinaryOperator/Word — never as
                    // identifiers). `BETWEEN x AND y` is range syntax.
                    if t.word == "AND" && self.pending_between {
                        self.pending_between = false;
                    } else if self.in_body {
                        let op: &'static str = if t.word == "AND" { "AND" } else { "OR" };
                        self.cyclo(t.span, reason::BOOLEAN_OPERATOR);
                        // Cognitive: +1 per *sequence* of same-operator runs.
                        if self.last_bool != Some(op) {
                            self.cognitive(t.span, 1.0, reason::BOOLEAN_SEQUENCE);
                        }
                        self.last_bool = Some(op);
                    }
                }
                _ => {
                    // Ordinary operands (`a AND b AND c`) keep the boolean
                    // run alive — only expression boundaries end it, so a
                    // homogeneous chain costs one cognitive sequence, not
                    // one per operator (Codex P2). Every control keyword has
                    // an explicit arm above that breaks the run; here only
                    // clause starters and argument separators do.
                    if matches!(
                        t.word.as_str(),
                        "," | "WHERE"
                            | "HAVING"
                            | "ON"
                            | "SET"
                            | "SELECT"
                            | "FROM"
                            | "GROUP"
                            | "ORDER"
                            | "VALUES"
                            | "INTO"
                            | "UNION"
                            | "JOIN"
                            | "QUALIFY"
                    ) {
                        self.break_bool_run();
                    }
                }
            }
            i += 1;
        }
        // Region end: abandon any contexts left open (unparsable gaps).
        self.stack.clear();
        self.pending_loop_header = false;
        self.pending_between = false;
        self.break_bool_run();
    }

    fn count_loop(&mut self, span: SourceSpan) {
        self.facts.loop_count += 1;
        let nesting = self.nesting();
        self.cyclo(span, reason::LOOP);
        self.cognitive(span, 1.0 + nesting as f64, reason::LOOP);
    }

    fn count_dynamic_sql(&mut self, span: SourceSpan) {
        self.facts.dynamic_sql_count += 1;
        if self.emit {
            self.change_risk.push(ChangeRiskEvidence {
                span,
                factor: ChangeRiskFactor::DynamicSql,
            });
        }
    }
}

// ── entry point ────────────────────────────────────────────────────────

/// Extract procedural facts for the whole file. Requires
/// `facts.statements` (classification) and `facts.procedural_units` to be
/// populated. Fills `facts.procedural`, per-unit tallies on
/// `facts.procedural_units`, and appends dynamic-SQL change-risk evidence.
pub(crate) fn extract(
    root: &ErasedSegment,
    line_at: &impl Fn(u32) -> u32,
    emit_contributions: bool,
    facts: &mut SqlFileFacts,
) {
    let mut procedural = ProceduralFacts {
        routine_count: facts.procedural_units.len() as u32,
        ..ProceduralFacts::default()
    };
    let unit_ranges: Vec<(u32, u32, usize)> = facts
        .procedural_units
        .iter()
        .enumerate()
        .map(|(idx, u)| (u.start_byte, u.end_byte, idx))
        .collect();
    let mut unit_tallies = vec![(0.0f64, 0.0f64); facts.procedural_units.len()];
    // Query-structural score of body continuations, accumulated per owning
    // routine: when sqruff splits a routine body into sibling statements,
    // the queries in those fragments belong to the routine's embedded score
    // (Codex P2). Unparsable spills contribute nothing here — they contain
    // no typed query nodes to extract.
    let mut continuation_scores = vec![0.0f64; facts.procedural_units.len()];
    let mut change_risk: Vec<ChangeRiskEvidence> = Vec::new();

    let push_entry = |procedural: &mut ProceduralFacts, span: SourceSpan| {
        procedural.cyclomatic_complexity += 1.0;
        if emit_contributions {
            procedural.evidence.push(ProceduralEvidence {
                span,
                metric: ProceduralMetric::Cyclomatic,
                amount: 1.0,
                reason: reason::ENTRY,
            });
        }
    };

    // Statement regions, zipped with their classification (the same
    // `top_level_statements` crawl `classify_statements` consumed, so the
    // zip is aligned by construction).
    let statements = crate::facts::top_level_statements(root);
    let mut region_ranges: Vec<(u32, u32)> = Vec::new();
    for (node, stmt_facts) in statements.iter().zip(facts.statements.iter()) {
        let is_procedural_region = matches!(
            stmt_facts.kind,
            StatementKind::Procedural | StatementKind::AnonymousBlock
        );
        if !is_procedural_region {
            continue;
        }
        region_ranges.push((stmt_facts.start_byte, stmt_facts.end_byte));
        let tokens = tokens_of(node, line_at);
        // A `procedural` statement that contains no routine-definition node
        // is a *continuation* — a body fragment sqruff split off its routine
        // (T-SQL/MySQL). Its increments attribute to the routine it follows
        // (Codex P2). Routine-definition statements attribute by containment;
        // anonymous blocks stay file-level.
        let contains_unit = unit_ranges
            .iter()
            .any(|(s, e, _)| stmt_facts.start_byte <= *s && *e <= stmt_facts.end_byte);
        let fallback_unit = if stmt_facts.kind == StatementKind::Procedural && !contains_unit {
            last_unit_before(&unit_ranges, stmt_facts.start_byte)
        } else {
            None
        };
        let mut machine = Machine {
            facts: &mut procedural,
            unit_ranges: &unit_ranges,
            unit_tallies: &mut unit_tallies,
            change_risk: &mut change_risk,
            emit: emit_contributions,
            fallback_unit,
            stack: Vec::new(),
            in_body: false,
            pending_loop_header: false,
            pending_between: false,
            last_bool: None,
        };
        // Anonymous blocks, scripting statements, and body continuations
        // *are* body; routine definitions enter their body at IS/AS/BEGIN.
        machine.scan(
            &tokens,
            stmt_facts.kind == StatementKind::AnonymousBlock || fallback_unit.is_some(),
        );
        // A continuation's query constructs belong to its routine's embedded
        // score (Codex P2).
        if let Some(idx) = fallback_unit {
            continuation_scores[idx] += embedded_query_structural(node);
        }
        // Entry path: +1 for an anonymous block itself (a routine-definition
        // statement's entries come from its units below).
        if stmt_facts.kind == StatementKind::AnonymousBlock {
            push_entry(&mut procedural, crate::facts::statement_span(stmt_facts));
        }
    }

    // BigQuery-style top-level scripting (`IF … THEN DROP TABLE …; END IF;`
    // at file level) parses as a `MultiStatementSegment` directly under
    // `File` — *outside* any `Statement` node. Its inner DDL/DML statements
    // are the file's top-level statements (so object/risk scans see them
    // normally), but the scripting control flow around them is only visible
    // here: scan each such segment as an anonymous-block region. Segments
    // inside already-collected regions (a routine body's scripting) are
    // skipped by containment.
    let multi_statements = root.recursive_crawl(&MULTI_STATEMENT, false, &SyntaxSet::EMPTY, true);
    for seg in &multi_statements {
        let Some(pm) = seg.get_position_marker() else {
            continue;
        };
        let (start, end) = (pm.source_slice.start as u32, pm.source_slice.end as u32);
        if region_ranges.iter().any(|(s, e)| *s <= start && end <= *e) {
            continue;
        }
        region_ranges.push((start, end));
        let tokens = tokens_of(seg, line_at);
        let mut machine = Machine {
            facts: &mut procedural,
            unit_ranges: &unit_ranges,
            unit_tallies: &mut unit_tallies,
            change_risk: &mut change_risk,
            emit: emit_contributions,
            fallback_unit: None,
            stack: Vec::new(),
            in_body: false,
            pending_loop_header: false,
            pending_between: false,
            last_bool: None,
        };
        machine.scan(&tokens, true);
        push_entry(
            &mut procedural,
            SourceSpan::new(start, end, line_at(start), line_at(end.saturating_sub(1))),
        );
    }

    // Entry paths per routine unit (independent of the region loop so units
    // in a partially-parsed statement still count; each subprogram is its own
    // path — Sonar's model).
    for (idx, unit) in facts.procedural_units.iter().enumerate() {
        let span = SourceSpan::new(
            unit.start_byte,
            unit.end_byte,
            unit.start_line,
            unit.end_line,
        );
        push_entry(&mut procedural, span);
        unit_tallies[idx].0 += 1.0;
    }

    // Top-level unparsable runs outside procedural statements — T-SQL bodies
    // spill here. Marker-gated so broken declarative SQL contributes nothing.
    let unparsables = root.recursive_crawl(&UNPARSABLE, true, &SyntaxSet::EMPTY, true);
    for run in &unparsables {
        let Some(pm) = run.get_position_marker() else {
            continue;
        };
        let (start, end) = (pm.source_slice.start as u32, pm.source_slice.end as u32);
        let contained = region_ranges.iter().any(|(s, e)| *s <= start && end <= *e);
        if contained {
            continue; // already scanned within its statement's region
        }
        let tokens = tokens_of(run, line_at);
        if !unparsable_is_procedural(&tokens) {
            continue;
        }
        let mut machine = Machine {
            facts: &mut procedural,
            unit_ranges: &unit_ranges,
            unit_tallies: &mut unit_tallies,
            change_risk: &mut change_risk,
            emit: emit_contributions,
            // A top-level spill is the body of the routine it follows —
            // attribute its increments to the last unit ending before it
            // (Codex P2). No preceding unit → file-level only.
            fallback_unit: last_unit_before(&unit_ranges, start),
            stack: Vec::new(),
            in_body: false,
            pending_loop_header: false,
            pending_between: false,
            last_bool: None,
        };
        // The marker gate just proved this fragment is procedural body
        // content (it may start mid-body — T-SQL spills lose the opening
        // BEGIN to the parsed part), so the body gate is open from the start.
        machine.scan(&tokens, true);
    }

    // Embedded query complexity per routine (§9.3): the structural score of
    // the query constructs inside each unit's subtree — computed with the
    // unit as the crawl root so subquery depths are unit-relative — plus the
    // scores of body continuations attributed to it above.
    let unit_nodes = crate::facts::procedural_unit_nodes(root);
    debug_assert_eq!(unit_nodes.len(), facts.procedural_units.len());
    let mut max_unit: Option<(usize, f64)> = None;
    for (idx, node) in unit_nodes.iter().enumerate() {
        let score = embedded_query_structural(node) + continuation_scores[idx];
        if let Some(unit) = facts.procedural_units.get_mut(idx) {
            unit.embedded_query_structural = score;
        }
        if score > procedural.max_embedded_query_structural {
            procedural.max_embedded_query_structural = score;
            max_unit = Some((idx, score));
        }
    }
    // The published maximum is evidence-backed like every other composite:
    // one entry naming the winning routine (§4.7, Codex P1).
    if emit_contributions
        && let Some((idx, score)) = max_unit
        && let Some(unit) = facts.procedural_units.get(idx)
    {
        procedural.evidence.push(ProceduralEvidence {
            span: SourceSpan::new(
                unit.start_byte,
                unit.end_byte,
                unit.start_line,
                unit.end_line,
            ),
            metric: ProceduralMetric::EmbeddedQueryMax,
            amount: score,
            reason: reason::EMBEDDED_QUERY,
        });
    }

    for (idx, (cyclo, cognitive)) in unit_tallies.into_iter().enumerate() {
        if let Some(unit) = facts.procedural_units.get_mut(idx) {
            unit.cyclomatic_complexity = cyclo;
            unit.cognitive_complexity = cognitive;
        }
    }
    facts.change_risk_evidence.extend(change_risk);
    facts.procedural = procedural;
}

/// The index of the last routine unit whose range ends at or before `byte` —
/// the routine a body continuation (split statement or unparsable spill)
/// belongs to.
fn last_unit_before(unit_ranges: &[(u32, u32, usize)], byte: u32) -> Option<usize> {
    unit_ranges
        .iter()
        .filter(|(_, e, _)| *e <= byte)
        .max_by_key(|(_, e, _)| *e)
        .map(|&(_, _, idx)| idx)
}

/// `sql.structural_complexity` (§8.1) of the query constructs embedded in
/// one routine, reusing the declarative family extractors with the unit as
/// the crawl root.
fn embedded_query_structural(unit: &ErasedSegment) -> f64 {
    let mut mini = SqlFileFacts::default();
    let selects = unit.recursive_crawl(&SELECT_STATEMENT, true, &SyntaxSet::EMPTY, true);
    mini.query_block_count = selects.len() as u32;
    crate::facts::extract_joins(unit, &mut mini.joins);
    crate::facts::extract_set_ops(unit, &mut mini.set_ops);
    crate::facts::extract_cases(unit, &mut mini.cases);
    crate::facts::extract_windows(unit, &mut mini.windows);
    crate::facts::extract_aggregates(unit, &mut mini.aggregates);
    crate::facts::extract_predicates(unit, &mut mini.predicates);
    crate::facts::extract_subqueries(unit, &selects, &mut mini.subqueries);
    crate::facts::extract_expressions(unit, &mut mini.expressions);
    crate::facts::extract_cte_graph(unit, &mut mini.ctes);
    crate::composite::structural(&mini)
}
