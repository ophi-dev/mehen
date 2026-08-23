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
use sqruff_lib_core::dialects::init::DialectKind;
use sqruff_lib_core::dialects::syntax::{SyntaxKind, SyntaxSet};
use sqruff_lib_core::parser::segments::ErasedSegment;

use crate::facts::{ChangeRiskEvidence, ChangeRiskFactor, SqlFileFacts, StatementKind};

/// Which published metric a piece of procedural evidence contributes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProceduralMetric {
    Cyclomatic,
    Cognitive,
    /// `sql.structural_complexity.max_embedded_query` — one entry, for the
    /// winning routine.
    EmbeddedQueryMax,
    // Raw counts (Codex P1): every published `sql.procedural.*_count` is
    // evidence-backed under its own key, so output can answer *why* a value
    // moved. `max_block_depth` keeps the invariant with a single
    // contribution at the deepest opener whose amount is the observed
    // depth (Codex P1).
    BlockCount,
    RoutineCount,
    LoopCount,
    IfCount,
    CaseStatementCount,
    ExceptionHandlerCount,
    ReturnCount,
    RaiseThrowCount,
    DynamicSqlCount,
    MaxBlockDepth,
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
    /// The opener where that deepest nesting was first observed — the span
    /// of `max_block_depth`'s single evidence entry (Codex P1).
    pub max_block_depth_span: Option<SourceSpan>,
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
    pub(crate) const BLOCK: &str = "sql.procedural.block";
    pub(crate) const DEEPEST_BLOCK: &str = "sql.procedural.deepest_block";
    pub(crate) const ROUTINE: &str = "sql.procedural.routine";
    pub(crate) const RETURN: &str = "sql.procedural.return";
    pub(crate) const DYNAMIC_SQL: &str = "sql.procedural.dynamic_sql";
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

/// The nodes that *are* embedded queries — the crawl roots for
/// [`query_facts_of`]. Procedural assignment/condition expressions outside
/// these do not feed `sql.structural_complexity`: `x := ((1 + 2) * 3);`
/// embeds no query and must not score one (Codex P2). Mirrors the
/// dialect-folded DML statement sets in `facts.rs`.
const QUERY_ROOTS: SyntaxSet = SyntaxSet::new(&[
    SyntaxKind::WithCompoundStatement,
    SyntaxKind::SetExpression,
    SyntaxKind::SelectStatement,
    SyntaxKind::InsertStatement,
    SyntaxKind::OracleInsertStatement,
    SyntaxKind::BulkInsertStatement,
    SyntaxKind::UpdateStatement,
    SyntaxKind::OracleUpdateStatement,
    SyntaxKind::DeleteStatement,
    SyntaxKind::OracleDeleteStatement,
    SyntaxKind::MergeStatement,
]);

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
            // An `ELSE`-led fragment is the else branch of a control
            // statement the grammar split off (T-SQL `IF …; ELSE …`).
            "ELSE" if i == 0 => return true,
            // A *leading* control-position `IF` is a decision the grammar
            // lost whole (T-SQL `IF @a = 1 THROW …` at file level —
            // Codex P2). Mid-run `IF`s stay unadmitted (ambiguous with DDL
            // guards, `DROP TABLE IF EXISTS`), and so do condition-less
            // fragments (`if; end;` — debris of a partially parsed
            // `END IF`). The scalar `IF(…)` carve-out still applies.
            "IF" if i == 0 && !matches!(word(1), ";" | "") && !scalar_if_call(tokens, 0) => {
                return true;
            }
            "SP_EXECUTESQL" => return true,
            "EXECUTE" if word(i + 1) == "IMMEDIATE" => return true,
            // T-SQL `EXEC('…')` — an immediately executed dynamic string
            // batch (Codex P1). Plain `EXEC procname` is deliberately not a
            // marker: a static call proves nothing procedural by itself.
            "EXEC" | "EXECUTE" if word(i + 1) == "(" => return true,
            // T-SQL variable-form `EXEC @sql` executes the variable's
            // contents (Codex P1). Return-value capture is only static when
            // the right-hand side is a literal procedure name —
            // `EXEC @status = @proc_var` still executes a variable
            // (CodeRabbit).
            "EXEC" | "EXECUTE"
                if word(i + 1).starts_with('@')
                    && (word(i + 2) != "=" || word(i + 3).starts_with('@')) =>
            {
                return true;
            }
            // MySQL `PREPARE stmt FROM @sql` — dynamic SQL at any level
            // (Codex P1).
            "PREPARE" if word(i + 2) == "FROM" => return true,
            // An exception-handler section (PL/SQL, BigQuery scripting).
            "EXCEPTION" if word(i + 1) == "WHEN" => return true,
            "WHILE" => return true,
            _ => {}
        }
    }
    false
}

// ── the state machine ──────────────────────────────────────────────────

/// Scanner state a region hands to its continuation: the open context
/// stack and whether the body gate was open.
type CarriedState = (Vec<Ctx>, bool);

/// Whether the `IF` at `i` is the scalar conditional *function*
/// `IF(expr, a, b)` rather than a control statement. Inside `Unparsable`
/// runs every token is a plain `Word` — no `FunctionNameIdentifier` shape —
/// so two signals decide (Codex P2 ×2):
/// - argument commas: the function form carries commas at paren depth 1;
///   a comma-free parenthesized condition (`IF (@x > 0) BEGIN …`,
///   `IF (ready) THEN`) is control flow. Commas inside nested calls
///   (`IF (f(a, b) > 0) THEN`) sit deeper.
/// - a depth-1 comma alone is not proof: MySQL row constructors put commas
///   in real conditions (`IF (a, b) = (1, 2) THEN`). The preceding token
///   settles clear expression positions (`SET x = IF(…)`, `WHEN IF(…)`);
///   otherwise the statement shape decides — a control IF finds its own
///   `THEN` at paren depth 0 before the statement ends, a scalar call never
///   does.
///
/// Unclosed parens err toward keeping control flow visible.
fn scalar_if_call(tokens: &[PToken], i: usize) -> bool {
    let word = |j: usize| tokens.get(j).map(|t| t.word.as_str()).unwrap_or("");
    if word(i + 1) != "(" {
        return false;
    }
    // Phase 1: scan the IF's parens for a depth-1 comma.
    let mut depth = 0u32;
    let mut top_level_comma = false;
    let mut after_close = usize::MAX;
    for (j, t) in tokens.iter().enumerate().skip(i + 1) {
        match t.word.as_str() {
            "(" => depth += 1,
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    after_close = j + 1;
                    break;
                }
            }
            "," if depth == 1 => top_level_comma = true,
            _ => {}
        }
    }
    if after_close == usize::MAX || !top_level_comma {
        // Unclosed, or a comma-free condition: control.
        return false;
    }
    // A token that can only precede an *operand* proves expression
    // position. `THEN`/`ELSE`/`;`/run-start stay ambiguous — a nested
    // control IF starts there too.
    let expression_position = matches!(
        i.checked_sub(1)
            .map(|j| tokens[j].word.as_str())
            .unwrap_or(""),
        "=" | ","
            | "("
            | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "||"
            | ">"
            | "<"
            | ">="
            | "<="
            | "<>"
            | "!="
            | "SELECT"
            | "RETURN"
            | "AND"
            | "OR"
            | "NOT"
            | "WHEN"
            | "IF"
            | "ELSIF"
            | "ELSEIF"
            | "WHILE"
            | "UNTIL"
    );
    if expression_position {
        return true;
    }
    // Phase 2: statement shape. A control IF's condition continues to a
    // depth-0 `THEN` (`IF (a, b) = (1, 2) THEN`); a scalar call reaches a
    // terminator, the end of the run, or the closer of an enclosing
    // expression first.
    let mut depth = 0i32;
    for t in &tokens[after_close..] {
        match t.word.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth < 0 {
                    return true; // operand of an enclosing expression
                }
            }
            "THEN" if depth == 0 && t.keyword_like => return false,
            ";" if depth == 0 => return true,
            _ => {}
        }
    }
    true
}

/// Open construct contexts. Plain `Block` tracks `BEGIN … END` depth and
/// whether its `EXCEPTION` section has started; `Case` tracks pending `WHEN`
/// arms until `END CASE` (statement) or bare `END` (expression) resolves
/// whether they count.
#[derive(Debug)]
enum Ctx {
    Block {
        exception_section: bool,
        /// This block is a routine's *body* — opened for a pending
        /// `FUNCTION`/`PROCEDURE` header. Cognitive nesting resets at the
        /// topmost such block, so a nested subprogram's decisions don't
        /// inherit the outer routine's lexical depth (Codex P2).
        routine_body: bool,
    },
    Try,
    Catch,
    /// `bound` = a `BEGIN` block opened directly under this (T-SQL) `IF`, so
    /// the `IF` closes when that block closes, not at the next terminator.
    /// `else_taken` = an `ELSE` already bound to this IF: the next `; ELSE`
    /// belongs to an outer IF, and this one completes at that terminator
    /// (Codex P2).
    If {
        with_then: bool,
        bound: bool,
        else_taken: bool,
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
    /// Loop headers (`WHILE`/`FOR`) whose body opener has not arrived yet —
    /// a *count*, because single-statement T-SQL loops nest
    /// (`WHILE @a > 0 WHILE @b > 0 SET …;` completes both at one
    /// terminator, Codex P2). `LOOP`/`DO` consume one; a `BEGIN` block
    /// binds all pending loops (they close with it); a terminator closes
    /// every loop still pending.
    pending_loop_headers: u32,
    /// Inside `BETWEEN … AND …`: the next `AND` is not a boolean operator.
    pending_between: bool,
    /// A nested routine header (`function inner_f return number is`) is
    /// being read: between the `FUNCTION`/`PROCEDURE` keyword and its
    /// `IS`/`AS`/`BEGIN`, a `RETURN` is the signature's return *type*, not a
    /// return statement — even though the enclosing routine's body gate is
    /// already open (Codex P2).
    pending_routine_header: bool,
    /// Definition headers whose body block hasn't opened yet — the next
    /// plain `BEGIN`s consume these and tag their blocks `routine_body`
    /// (nesting baselines, Codex P2). Counter, not flag: an Oracle DECLARE
    /// section can stack several nested subprogram headers before their
    /// bodies open.
    pending_routine_bodies: u32,
    /// Previous code token was a boolean operator with this text (`AND`/
    /// `OR`); any other token breaks the run. Used for sequence-based
    /// cognitive counting (+1 per run of like operators, +1 on alternation).
    last_bool: Option<&'static str>,
}

impl Machine<'_> {
    fn nesting(&self) -> u32 {
        // Nesting is routine-local: count above the topmost routine-body
        // block, so decisions inside a nested subprogram don't inherit the
        // outer routine's open contexts (Codex P2).
        let base = self
            .stack
            .iter()
            .rposition(|c| {
                matches!(
                    c,
                    Ctx::Block {
                        routine_body: true,
                        ..
                    }
                )
            })
            .map_or(0, |i| i + 1);
        self.stack[base..]
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
            _ => {}
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
                _ => {}
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

    /// A body opener binds the pending T-SQL single-statement controls:
    /// pending loop headers become block-bound, otherwise a THEN-less IF on
    /// top of the stack binds to the opener — the control closes when the
    /// construct does, not at the first terminator inside it (Codex P2 ×2:
    /// plain `BEGIN` blocks and `BEGIN TRY … END CATCH` constructs alike).
    fn bind_pending_owner(&mut self) {
        if self.pending_loop_headers > 0 {
            let mut pending = self.pending_loop_headers as usize;
            self.pending_loop_headers = 0;
            for ctx in self.stack.iter_mut().rev() {
                if pending == 0 {
                    break;
                }
                if let Ctx::Loop { block_bound: false } = ctx {
                    *ctx = Ctx::Loop { block_bound: true };
                    pending -= 1;
                }
            }
        } else if let Some(Ctx::If {
            with_then: false,
            bound,
            ..
        }) = self.stack.last_mut()
        {
            *bound = true;
        }
    }

    /// A completed construct (bare `END` of a block, `END CATCH` of a
    /// paired TRY/CATCH) also completes the T-SQL single-statement contexts
    /// it was the body of: loops bound to it, and THEN-less IFs — both
    /// those bound directly and those whose single statement was a
    /// just-popped loop or IF. The pops interleave because the contexts
    /// nest in any order. An ELSE stops the IF pops: the decision continues
    /// through the else branch, keeping its nesting for the else body
    /// (Codex P2) — loops still pop, their bodies are done either way.
    fn close_completed_owners(&mut self, next_word: &str) {
        loop {
            match self.stack.last() {
                Some(Ctx::Loop { block_bound: true }) => {
                    self.stack.pop();
                }
                Some(Ctx::If {
                    with_then: false, ..
                }) if next_word != "ELSE" => {
                    self.stack.pop();
                }
                _ => break,
            }
        }
    }

    /// Track the deepest block nesting and remember its opener: the span
    /// of `max_block_depth`'s single evidence entry (Codex P1).
    fn note_block_depth(&mut self, span: SourceSpan) {
        let depth = self.block_depth();
        if depth > self.facts.max_block_depth {
            self.facts.max_block_depth = depth;
            self.facts.max_block_depth_span = Some(span);
        }
    }

    /// Increment a raw `sql.procedural.*_count` and emit its evidence: raw
    /// counts are evidence-backed under their own keys, so `metric == Σ
    /// contributions` holds for every published count (Codex P1). Raw
    /// increments are file-level only — per-unit tallies exist for the
    /// composites alone.
    fn raw_count(&mut self, metric: ProceduralMetric, span: SourceSpan, reason: &'static str) {
        match metric {
            ProceduralMetric::BlockCount => self.facts.block_count += 1,
            ProceduralMetric::LoopCount => self.facts.loop_count += 1,
            ProceduralMetric::IfCount => self.facts.if_count += 1,
            ProceduralMetric::CaseStatementCount => self.facts.case_statement_count += 1,
            ProceduralMetric::ExceptionHandlerCount => self.facts.exception_handler_count += 1,
            ProceduralMetric::ReturnCount => self.facts.return_count += 1,
            ProceduralMetric::RaiseThrowCount => self.facts.raise_throw_count += 1,
            ProceduralMetric::DynamicSqlCount => self.facts.dynamic_sql_count += 1,
            // Composites route through `add`; routine_count is emitted by
            // `extract` per unit.
            ProceduralMetric::Cyclomatic
            | ProceduralMetric::Cognitive
            | ProceduralMetric::EmbeddedQueryMax
            | ProceduralMetric::RoutineCount
            | ProceduralMetric::MaxBlockDepth => {
                debug_assert!(false, "not a machine-raised raw count: {metric:?}");
            }
        }
        if self.emit {
            self.facts.evidence.push(ProceduralEvidence {
                span,
                metric,
                amount: 1.0,
                reason,
            });
        }
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
                bound: false,
                ..
            })
        ) {
            self.stack.pop();
        }
    }

    fn break_bool_run(&mut self) {
        self.last_bool = None;
    }

    fn scan(&mut self, tokens: &[PToken]) {
        let word = |i: usize| tokens.get(i).map(|t| t.word.as_str()).unwrap_or("");

        let mut i = 0usize;
        while i < tokens.len() {
            let t = &tokens[i];
            let kw = t.keyword_like;
            match t.word.as_str() {
                ";" => {
                    // A routine header ending at the terminator without
                    // IS/AS/BEGIN is a *forward declaration* / prototype
                    // (`PROCEDURE helper;` in a spec or DECLARE section):
                    // no body block will open, so its pending body marker
                    // retires here — otherwise a later ordinary `BEGIN`
                    // would be mislabeled a routine body and reset cognitive
                    // nesting (Codex P2).
                    if self.pending_routine_header {
                        self.pending_routine_header = false;
                        self.pending_routine_bodies = self.pending_routine_bodies.saturating_sub(1);
                    }
                    // Loop headers still pending a body opener at the
                    // terminator were single-statement loops (`WHILE @a > 0
                    // WHILE @b > 0 SET …;`) — every one of them completes
                    // here, *before* the IF pass so an `IF … WHILE …;` chain
                    // exposes its outer IF for closing too (Codex P2).
                    while self.pending_loop_headers > 0 {
                        self.pending_loop_headers -= 1;
                        self.pop_matching(|c| matches!(c, Ctx::Loop { block_bound: false }));
                    }
                    // A single-statement then-body followed by ELSE keeps its
                    // IF open for the else branch (`IF @a > 0 SELECT 1; ELSE
                    // IF @b > 0 …`), mirroring the `END ELSE` block shape
                    // (Codex P2). But the upcoming ELSE binds the innermost
                    // IF still *without* an else branch: deeper IFs whose
                    // else just completed close here (`IF @a IF @b …; ELSE
                    // …; ELSE …` — the second ELSE is @a's, Codex P2).
                    if word(i + 1) != "ELSE" {
                        self.close_unbound_ifs();
                    } else {
                        while matches!(
                            self.stack.last(),
                            Some(Ctx::If {
                                with_then: false,
                                bound: false,
                                else_taken: true
                            })
                        ) {
                            self.stack.pop();
                        }
                    }
                    self.pending_between = false;
                    self.break_bool_run();
                }
                "IS" | "AS" if kw => {
                    self.in_body = true;
                    self.pending_routine_header = false;
                    // A call-spec body (`AS LANGUAGE JAVA …`, T-SQL CLR
                    // `AS EXTERNAL NAME …`) never opens a `BEGIN` block:
                    // retire the header's pending body marker (Codex P2).
                    if matches!(word(i + 1), "LANGUAGE" | "EXTERNAL") {
                        self.pending_routine_bodies = self.pending_routine_bodies.saturating_sub(1);
                    }
                    self.break_bool_run();
                }
                "FUNCTION" | "PROCEDURE" if kw => {
                    // A definition header arms a routine-body marker for the
                    // block that will open it — reference positions (`DROP
                    // PROCEDURE`, `ALTER FUNCTION`, `GRANT … ON PROCEDURE`,
                    // `END FUNCTION`, `COMMENT ON …`) name a routine without
                    // defining one (Codex P2).
                    let prev = i
                        .checked_sub(1)
                        .map(|j| tokens[j].word.as_str())
                        .unwrap_or("");
                    if !matches!(prev, "DROP" | "ALTER" | "ON" | "END" | "EXISTS") {
                        self.pending_routine_bodies += 1;
                    }
                    // The signature (nested subprogram in a DECLARE section,
                    // or the outer definition itself) runs until
                    // IS/AS/BEGIN.
                    self.pending_routine_header = true;
                    self.break_bool_run();
                }
                "BEGIN" if kw => {
                    self.break_bool_run();
                    // A body opener also ends any routine signature being
                    // read (MySQL headers have no IS/AS before BEGIN).
                    self.pending_routine_header = false;
                    match word(i + 1) {
                        // Transaction control, not a block.
                        "TRANSACTION" | "TRAN" | "WORK" | "DIALOG" | "DISTRIBUTED" | ";" => {}
                        "TRY" => {
                            self.in_body = true;
                            self.raw_count(ProceduralMetric::BlockCount, t.span, reason::BLOCK);
                            // The construct is the pending control's body:
                            // `IF @a > 0 BEGIN TRY …` closes the IF at
                            // `END CATCH`, not at the first `;` inside the
                            // try body (Codex P2).
                            self.bind_pending_owner();
                            self.stack.push(Ctx::Try);
                            self.note_block_depth(t.span);
                            i += 1; // consume TRY
                        }
                        "CATCH" => {
                            self.in_body = true;
                            self.raw_count(ProceduralMetric::BlockCount, t.span, reason::BLOCK);
                            self.raw_count(
                                ProceduralMetric::ExceptionHandlerCount,
                                t.span,
                                reason::EXCEPTION_HANDLER,
                            );
                            let nesting = self.nesting();
                            self.cyclo(t.span, reason::EXCEPTION_HANDLER);
                            self.cognitive(t.span, 1.0 + nesting as f64, reason::EXCEPTION_HANDLER);
                            self.stack.push(Ctx::Catch);
                            self.note_block_depth(t.span);
                            i += 1; // consume CATCH
                        }
                        _ => {
                            self.in_body = true;
                            self.raw_count(ProceduralMetric::BlockCount, t.span, reason::BLOCK);
                            // The block is a loop body when loop headers
                            // are pending (T-SQL `WHILE … BEGIN`), otherwise
                            // it binds a fresh T-SQL IF on top: the control
                            // closes with the block (Codex P2).
                            self.bind_pending_owner();
                            self.stack.push(Ctx::Block {
                                exception_section: false,
                                // Consume one armed header: this block is
                                // that routine's body (Codex P2).
                                routine_body: {
                                    let armed = self.pending_routine_bodies > 0;
                                    if armed {
                                        self.pending_routine_bodies -= 1;
                                    }
                                    armed
                                },
                            });
                            self.note_block_depth(t.span);
                        }
                    }
                }
                "END" if kw => {
                    self.break_bool_run();
                    // Compound closers consume their keyword only when a
                    // matching context actually pops — T-SQL puts a sibling
                    // statement right after a bare `END`, so the adjacent
                    // tokens of `… END IF @b > 0 …` or `… END WHILE @b … `
                    // are *not* the PL/SQL closers: the block closes bare and
                    // the sibling keyword processes as its own statement
                    // (Codex P2).
                    match word(i + 1) {
                        "IF" if self
                            .pop_matching(|c| {
                                matches!(
                                    c,
                                    Ctx::If {
                                        with_then: true,
                                        ..
                                    }
                                )
                            })
                            .is_some() =>
                        {
                            i += 1;
                        }
                        "LOOP" | "WHILE" | "REPEAT" | "FOR"
                            if matches!(
                                self.stack.last(),
                                Some(Ctx::Loop { block_bound: false })
                            ) =>
                        {
                            // A dialect compound closer (`END LOOP`,
                            // `END WHILE`, …) only when the innermost open
                            // construct is a header-opened loop. A
                            // block-bound loop (T-SQL `WHILE … BEGIN … END`)
                            // closes through the bare-END path instead, so
                            // `… END WHILE @b > 0 …` keeps the sibling WHILE
                            // as its own statement (Codex P2).
                            self.stack.pop();
                            i += 1;
                        }
                        "CASE" => {
                            // `END CASE` proves the CASE was a *statement*:
                            // count it and its WHEN arms. (T-SQL has no CASE
                            // statement, so no sibling-adjacency variant
                            // exists: a CASE context is always open here or
                            // the tokens are malformed.)
                            if let Some(Ctx::Case {
                                whens,
                                nesting,
                                open_span,
                            }) = self.pop_matching(|c| matches!(c, Ctx::Case { .. }))
                            {
                                self.raw_count(
                                    ProceduralMetric::CaseStatementCount,
                                    open_span,
                                    reason::CASE_STATEMENT,
                                );
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
                        "TRY" if self.pop_matching(|c| matches!(c, Ctx::Try)).is_some() => {
                            i += 1;
                        }
                        "CATCH" if self.pop_matching(|c| matches!(c, Ctx::Catch)).is_some() => {
                            // `END CATCH` completes the whole paired
                            // TRY/CATCH construct: controls bound at
                            // `BEGIN TRY` close now (Codex P2). The ELSE
                            // lookahead is past the consumed CATCH.
                            self.close_completed_owners(word(i + 2));
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
                                // (`IF @a > 0 WHILE @b > 0 BEGIN … END`
                                // leaves the IF unbound because the BEGIN
                                // bound the pending loop — Codex P2.)
                                self.close_completed_owners(word(i + 1));
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
                    // control flow. In parsed contexts the function is
                    // recognized by its parse shape
                    // (`FunctionNameIdentifier`), not by a following `(` —
                    // `IF (@count > 0) BEGIN … END` / `IF (ready) THEN` are
                    // ordinary statements with parenthesized conditions and
                    // must count (Codex P2). In unparsable runs everything
                    // is a `Word`, so the scalar form is recognized by its
                    // argument commas instead (`SET x = IF(flag, 1, 0)` —
                    // Codex P2).
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
                    if !ddl_guard && !t.is_function_name && !scalar_if_call(tokens, i) {
                        self.raw_count(ProceduralMetric::IfCount, t.span, reason::IF);
                        let nesting = self.nesting();
                        self.cyclo(t.span, reason::IF);
                        self.cognitive(t.span, 1.0 + nesting as f64, reason::IF);
                        self.stack.push(Ctx::If {
                            with_then: false,
                            bound: false,
                            else_taken: false,
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
                    self.raw_count(ProceduralMetric::IfCount, t.span, reason::ELSIF);
                    self.cyclo(t.span, reason::ELSIF);
                    self.cognitive(t.span, 1.0, reason::ELSIF);
                }
                "ELSE" if kw => {
                    self.break_bool_run();
                    // ELSE of a CASE (expression or statement) is not a
                    // control-flow branch here. Any other ELSE in a
                    // procedural region is an IF-else — including the T-SQL
                    // `IF … BEGIN … END ELSE …` shape, where the block kept
                    // its IF open for this branch — and costs 1 cognitive
                    // (flat, per the cognitive model).
                    if !matches!(self.stack.last(), Some(Ctx::Case { .. })) {
                        self.cognitive(t.span, 1.0, reason::ELSE);
                    }
                    // A block-bound IF entering its ELSE branch becomes
                    // terminator-bound again: a single-statement else-body
                    // (`… END ELSE SELECT 2;`) closes it at the `;`, while a
                    // block else-body re-binds it at its `BEGIN` (Codex P2).
                    // Either way the IF has its else now — the next `; ELSE`
                    // belongs to an outer IF (Codex P2).
                    if let Some(Ctx::If {
                        with_then: false,
                        bound,
                        else_taken,
                    }) = self.stack.last_mut()
                    {
                        *bound = false;
                        *else_taken = true;
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
                            self.raw_count(
                                ProceduralMetric::ExceptionHandlerCount,
                                t.span,
                                reason::EXCEPTION_HANDLER,
                            );
                            let nesting = self.nesting();
                            self.cyclo(t.span, reason::EXCEPTION_HANDLER);
                            self.cognitive(t.span, 1.0 + nesting as f64, reason::EXCEPTION_HANDLER);
                            self.stack.push(Ctx::Handler);
                        }
                        Some(Ctx::Handler) => {
                            // Next handler of the same exception section.
                            self.stack.pop();
                            self.raw_count(
                                ProceduralMetric::ExceptionHandlerCount,
                                t.span,
                                reason::EXCEPTION_HANDLER,
                            );
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
                            routine_body: false,
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
                    if self.pending_loop_headers > 0 {
                        // Body opener of a WHILE/FOR header — its Loop
                        // context is already on the stack.
                        self.pending_loop_headers -= 1;
                    } else {
                        self.count_loop(t.span);
                        self.stack.push(Ctx::Loop { block_bound: false });
                    }
                }
                "DO" if kw => {
                    self.break_bool_run();
                    // MySQL/BigQuery `WHILE … DO` / `FOR … DO` body opener —
                    // the header's Loop context is already on the stack.
                    self.pending_loop_headers = self.pending_loop_headers.saturating_sub(1);
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
                    self.pending_loop_headers += 1;
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
                        self.pending_loop_headers += 1;
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
                    // Only inside a body, and never in a routine signature:
                    // `CREATE FUNCTION … RETURN number IS` (outer, before the
                    // body gate opens) and `function inner_f return number
                    // is` (nested, while the enclosing body gate is already
                    // open) both declare the return *type*.
                    if self.in_body && !self.pending_routine_header {
                        self.raw_count(ProceduralMetric::ReturnCount, t.span, reason::RETURN);
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
                        self.raw_count(
                            ProceduralMetric::RaiseThrowCount,
                            t.span,
                            reason::RAISE_THROW,
                        );
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
                        } else if word(i + 1).starts_with('@')
                            && (word(i + 2) != "=" || word(i + 3).starts_with('@'))
                        {
                            // T-SQL `EXEC @sql` executes the *contents* of a
                            // variable — dynamic SQL (Codex P1). Return-value
                            // capture stays static only when the right-hand
                            // side is a literal procedure name:
                            // `EXEC @ret = dbo.proc` is a static call, but
                            // `EXEC @status = @proc_var` executes a variable
                            // (CodeRabbit).
                            self.count_dynamic_sql(t.span);
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
                "DBMS_SQL" if word(i + 1) == "." && word(i + 3) == "(" => {
                    // The Oracle dynamic-SQL package, recognized only in the
                    // qualified *call* shape (`DBMS_SQL.PARSE(…)`): the
                    // parsed package qualifier lexes as a `NakedIdentifier`
                    // — not keyword-like — so there is no `kw` guard, while
                    // the `.method(` requirement keeps a column or relation
                    // that happens to be named `dbms_sql` from counting
                    // (`SELECT dbms_sql.foo INTO v FROM dbms_sql`, Codex P2).
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
                    // identifiers). `BETWEEN x AND y` is range syntax, and a
                    // routine *signature* being read is declaration, not a
                    // path: `PROCEDURE p(flag BOOLEAN := TRUE AND FALSE);`
                    // in a package spec creates no branch (Codex P2).
                    if t.word == "AND" && self.pending_between {
                        self.pending_between = false;
                    } else if self.in_body && !self.pending_routine_header {
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
        // Region end: transient token-adjacent state never crosses regions.
        // A loop header still pending a body opener closes like at a
        // terminator. The context *stack* and body gate deliberately stay —
        // the caller carries them into the routine's next continuation
        // region (split bodies keep their open blocks/decisions, Codex P2)
        // or drops them for standalone regions.
        while self.pending_loop_headers > 0 {
            self.pending_loop_headers -= 1;
            self.pop_matching(|c| matches!(c, Ctx::Loop { block_bound: false }));
        }
        self.pending_between = false;
        self.pending_routine_header = false;
        self.break_bool_run();
    }

    fn count_loop(&mut self, span: SourceSpan) {
        self.raw_count(ProceduralMetric::LoopCount, span, reason::LOOP);
        let nesting = self.nesting();
        self.cyclo(span, reason::LOOP);
        self.cognitive(span, 1.0 + nesting as f64, reason::LOOP);
    }

    fn count_dynamic_sql(&mut self, span: SourceSpan) {
        self.raw_count(ProceduralMetric::DynamicSqlCount, span, reason::DYNAMIC_SQL);
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
    dialect: DialectKind,
    facts: &mut SqlFileFacts,
) {
    let bigquery = dialect == DialectKind::Bigquery;
    // The dialects whose grammars split routine bodies into *root*
    // unparsable spills — the only ones where a top-level run can be a
    // routine continuation (T-SQL batch bodies, MySQL delimiter bodies).
    let spills_routine_bodies = matches!(dialect, DialectKind::Tsql | DialectKind::Mysql);
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
    // Query facts of body continuations, accumulated per owning routine:
    // when sqruff splits a routine body into sibling statements, the queries
    // in those fragments belong to the routine's embedded score (Codex P2).
    // Facts are merged (sums for counts, max for depths) and scored once per
    // routine, so parser fragmentation cannot inflate the max-shaped
    // structural terms. Unparsable spills contribute nothing here — they
    // contain no typed query nodes to extract.
    let mut continuation_facts: Vec<SqlFileFacts> = facts
        .procedural_units
        .iter()
        .map(|_| SqlFileFacts::default())
        .collect();
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
    //
    // `carried` holds scanner state (open context stack + body gate) per
    // routine whose body sqruff split across regions: the routine's next
    // continuation resumes where the previous region stopped, so open
    // blocks/decisions keep their depth and nesting across the split
    // (Codex P2). Standalone regions never save state.
    let statements = crate::facts::top_level_statements(root, bigquery);
    // T-SQL `GO` batch separators are hard attribution boundaries: a region
    // after a `GO` starts a new, independent batch and can never be the
    // body of a routine defined before the separator (Codex P2).
    let go_boundaries: Vec<u32> = statements
        .iter()
        .zip(facts.statements.iter())
        .filter(|(node, sf)| {
            sf.kind == StatementKind::Unknown && crate::facts::is_go_separator(node)
        })
        .map(|(_, sf)| sf.start_byte)
        .collect();
    let mut region_ranges: Vec<(u32, u32)> = Vec::new();
    let mut carried: std::collections::BTreeMap<usize, CarriedState> =
        std::collections::BTreeMap::new();
    // Leftover scanner state of anonymous regions, keyed by end byte — an
    // immediately following `ELSE`-led unparsable run resumes it without
    // attributing to any routine (Codex P2).
    let mut anon_states: Vec<(u32, Option<CarriedState>)> = Vec::new();
    for (node, stmt_facts) in statements.iter().zip(facts.statements.iter()) {
        let is_procedural_region = matches!(
            stmt_facts.kind,
            StatementKind::Procedural | StatementKind::AnonymousBlock
        );
        // An `unknown` statement can still hold procedural content the
        // grammar parsed without classifying — a top-level MySQL `PREPARE
        // stmt FROM @sql` is a plain statement, not an `Unparsable` run
        // (Codex P1). Reuse the marker gate so ordinary unknowns stay
        // unscanned.
        let gated_unknown = stmt_facts.kind == StatementKind::Unknown && {
            let tokens = tokens_of(node, line_at);
            unparsable_is_procedural(&tokens)
        };
        if !is_procedural_region && !gated_unknown {
            continue;
        }
        // Statements *contained* in a unit are BigQuery-style routine
        // bodies reclassified by `classify_statements`: their tokens are
        // scanned by the routine's `MultiStatementSegment` region below, so
        // the statement itself is not a region (double-scan guard).
        if unit_ranges.iter().any(|&(s, e, _)| {
            s <= stmt_facts.start_byte
                && stmt_facts.end_byte <= e
                && (s, e) != (stmt_facts.start_byte, stmt_facts.end_byte)
        }) {
            continue;
        }
        region_ranges.push((stmt_facts.start_byte, stmt_facts.end_byte));
        let tokens = tokens_of(node, line_at);
        // A `procedural` statement that contains no routine-definition node
        // is a *continuation* — a body fragment sqruff split off its routine
        // (T-SQL/MySQL). Its increments attribute to the routine it follows
        // (Codex P2). Routine-definition statements attribute by containment;
        // anonymous blocks stay file-level.
        let contained_units: Vec<usize> = unit_ranges
            .iter()
            .filter(|(s, e, _)| stmt_facts.start_byte <= *s && *e <= stmt_facts.end_byte)
            .map(|&(_, _, idx)| idx)
            .collect();
        let fallback_unit =
            if stmt_facts.kind == StatementKind::Procedural && contained_units.is_empty() {
                last_unit_before(&unit_ranges, stmt_facts.start_byte)
            } else {
                None
            };
        // Continuations resume their routine's saved scanner state; the
        // routine a definition region leaves open is its *last* unit.
        let resumed = fallback_unit.and_then(|idx| carried.remove(&idx));
        let (stack, restored_body) = resumed.unwrap_or_default();
        let mut machine = Machine {
            facts: &mut procedural,
            unit_ranges: &unit_ranges,
            unit_tallies: &mut unit_tallies,
            change_risk: &mut change_risk,
            emit: emit_contributions,
            fallback_unit,
            stack,
            // Anonymous blocks, scripting statements, body continuations,
            // and gated unknown statements *are* body; routine definitions
            // enter their body at IS/AS/BEGIN — and so does a DECLARE-led
            // anonymous block: its declaration section is not a path, so an
            // initializer like `flag BOOLEAN := TRUE AND FALSE` counts
            // nothing (Codex P2). IF/WHILE/BEGIN-led scripting stays
            // immediate body.
            in_body: restored_body
                || (stmt_facts.kind == StatementKind::AnonymousBlock
                    && tokens.first().is_none_or(|t| t.word != "DECLARE"))
                || fallback_unit.is_some()
                || gated_unknown,
            pending_loop_headers: 0,
            pending_between: false,
            pending_routine_header: false,
            pending_routine_bodies: 0,
            last_bool: None,
        };
        machine.scan(&tokens);
        let end_state = (std::mem::take(&mut machine.stack), machine.in_body);
        drop(machine);
        // Save the scanner state for the routine this region belongs to so
        // its next continuation resumes it. An anonymous region's leftover
        // state is kept too (keyed by its end byte): sqruff can split a
        // standalone T-SQL decision between a parsed statement and an
        // `ELSE`-led root run, and the else branch needs the open IF stack
        // to keep its nesting (Codex P2).
        if let Some(idx) = fallback_unit.or_else(|| contained_units.last().copied()) {
            carried.insert(idx, end_state);
        } else if stmt_facts.kind == StatementKind::AnonymousBlock && !end_state.0.is_empty() {
            anon_states.push((stmt_facts.end_byte, Some(end_state)));
        }
        // A continuation's query constructs belong to its routine's embedded
        // score (Codex P2) — facts are merged (not scores summed) so the
        // max-shaped structural terms charge once per routine, not once per
        // parser fragment.
        if let Some(idx) = fallback_unit {
            merge_query_facts(&mut continuation_facts[idx], &query_facts_of(node));
            // The continuation is part of the routine's source extent:
            // extend the unit's span so the emitted `Function` space covers
            // the full body, not just the header fragment sqruff kept
            // inside the definition node — per-function coverage enrichment
            // and location-based consumers see the real scope (Codex P1).
            // Attribution buckets (`unit_ranges`) deliberately keep the
            // original ranges: continuation increments already reach this
            // unit via `fallback_unit`, and re-bucketing mid-scan would
            // change innermost-containment answers.
            let unit = &mut facts.procedural_units[idx];
            if stmt_facts.end_byte > unit.end_byte {
                unit.end_byte = stmt_facts.end_byte;
                unit.end_line = unit.end_line.max(stmt_facts.end_line);
            }
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
            in_body: true,
            pending_loop_headers: 0,
            pending_between: false,
            pending_routine_header: false,
            pending_routine_bodies: 0,
            last_bool: None,
        };
        machine.scan(&tokens);
        // The segment is an *anonymous* scripting region only when no
        // routine lives in it — BigQuery wraps `CREATE PROCEDURE` bodies in
        // a `MultiStatementSegment` too, and those already earn their entry
        // through the unit loop below (Codex P2).
        let overlaps_unit = unit_ranges.iter().any(|(s, e, _)| *s < end && start < *e);
        if !overlaps_unit {
            push_entry(
                &mut procedural,
                SourceSpan::new(start, end, line_at(start), line_at(end.saturating_sub(1))),
            );
        }
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
        // A top-level spill is the body of the routine it follows — but
        // only the spill dialects (T-SQL batches, MySQL delimiter bodies)
        // actually put routine continuations in *root* runs, so the
        // fallback is dialect-gated: an Oracle routine followed by an
        // unparsable anonymous block is two independent things, and
        // attaching the block would suppress its entry, extend the
        // function space, and misattribute its paths (Codex P2). Under a
        // spill dialect, attribute to the last unit ending before the run
        // and resume that routine's scanner state (Codex P2) — unless a
        // `GO` between them severs the tie: the run is a new batch
        // (Codex P2). No unit → file-level only, fresh state.
        let fallback_unit = if spills_routine_bodies {
            last_unit_before(&unit_ranges, start).filter(|&idx| {
                let unit_end = facts.procedural_units[idx].end_byte;
                !go_boundaries
                    .iter()
                    .any(|&go| unit_end <= go && go <= start)
            })
        } else {
            None
        };
        // A *standalone* control-led run is an anonymous block the parser
        // lost (`BEGIN TRY … END CATCH` at file level): one entry path,
        // like its statement-backed equivalent (Codex P2). Fragment shapes
        // (`ELSE …`-led continuations) and isolated dynamic-SQL runs
        // (`EXEC @sql`) open no scope and stay entry-free.
        if fallback_unit.is_none() {
            let control_led = tokens.first().is_some_and(|t| {
                matches!(t.word.as_str(), "IF" | "WHILE" | "FOR" | "LOOP" | "DECLARE")
                    || (t.word == "BEGIN"
                        && !matches!(
                            tokens.get(1).map(|t| t.word.as_str()).unwrap_or(""),
                            "TRANSACTION" | "TRAN" | "WORK" | "DIALOG" | "DISTRIBUTED" | ";"
                        ))
            });
            if control_led {
                push_entry(
                    &mut procedural,
                    SourceSpan::new(start, end, line_at(start), line_at(end.saturating_sub(1))),
                );
            }
        }
        let resumed = fallback_unit.and_then(|idx| carried.remove(&idx));
        // A standalone `ELSE`-led run continues the anonymous region right
        // before it: resume that region's open IF stack so the else branch
        // keeps its nesting — without attributing to any routine (Codex P2).
        let resumed_anon = if resumed.is_none() && fallback_unit.is_none() {
            let else_led = tokens.first().is_some_and(|t| t.word == "ELSE");
            if else_led {
                anon_states
                    .iter_mut()
                    .rev()
                    .find(|(anon_end, slot)| *anon_end <= start && slot.is_some())
                    .filter(|(anon_end, _)| {
                        // Adjacency: no other scanned region between.
                        !region_ranges
                            .iter()
                            .any(|&(s2, _)| *anon_end < s2 && s2 < start)
                    })
                    .and_then(|(_, slot)| slot.take())
            } else {
                None
            }
        } else {
            None
        };
        // Standalone runs delay the body gate when DECLARE-led (their
        // declaration section is not a path — the `BEGIN` opens the body,
        // Codex P2); attributed or resumed runs are proven body content
        // (a T-SQL body statement can itself start with `DECLARE @v …`).
        let standalone = fallback_unit.is_none() && resumed.is_none() && resumed_anon.is_none();
        let declare_led = tokens.first().is_some_and(|t| t.word == "DECLARE");
        let (stack, restored_body) = resumed.or(resumed_anon).unwrap_or_default();
        let mut machine = Machine {
            facts: &mut procedural,
            unit_ranges: &unit_ranges,
            unit_tallies: &mut unit_tallies,
            change_risk: &mut change_risk,
            emit: emit_contributions,
            fallback_unit,
            stack,
            // The marker gate just proved this fragment is procedural body
            // content (it may start mid-body — T-SQL spills lose the opening
            // BEGIN to the parsed part), so the body gate is open from the
            // start — except standalone DECLARE-led runs (Codex P2).
            in_body: restored_body || !(standalone && declare_led),
            pending_loop_headers: 0,
            pending_between: false,
            pending_routine_header: false,
            pending_routine_bodies: 0,
            last_bool: None,
        };
        machine.scan(&tokens);
        let end_state = (std::mem::take(&mut machine.stack), machine.in_body);
        drop(machine);
        if let Some(idx) = fallback_unit {
            carried.insert(idx, end_state);
            // The spill is part of the routine's source extent too — same
            // span extension as statement continuations above (Codex P1).
            let unit = &mut facts.procedural_units[idx];
            if end > unit.end_byte {
                unit.end_byte = end;
                unit.end_line = unit.end_line.max(line_at(end.saturating_sub(1)));
            }
        }
    }

    // Every routine unit is one evidence-backed `routine_count` increment,
    // spanning its (continuation-extended) definition (Codex P1).
    if emit_contributions {
        for unit in &facts.procedural_units {
            procedural.evidence.push(ProceduralEvidence {
                span: SourceSpan::new(
                    unit.start_byte,
                    unit.end_byte,
                    unit.start_line,
                    unit.end_line,
                ),
                metric: ProceduralMetric::RoutineCount,
                amount: 1.0,
                reason: reason::ROUTINE,
            });
        }
        // The high-water mark keeps the evidence-sum invariant with a
        // single contribution at the deepest opener whose amount is the
        // observed depth (Codex P1).
        if let Some(span) = procedural.max_block_depth_span {
            procedural.evidence.push(ProceduralEvidence {
                span,
                metric: ProceduralMetric::MaxBlockDepth,
                amount: procedural.max_block_depth as f64,
                reason: reason::DEEPEST_BLOCK,
            });
        }
    }

    // Embedded query complexity per routine (§9.3): the structural score of
    // the query constructs inside each unit's subtree — computed with the
    // unit as the crawl root so subquery depths are unit-relative — merged
    // with the facts of body continuations attributed to it above (merged as
    // facts, not summed as scores, so max-shaped terms charge once).
    let unit_nodes = crate::facts::procedural_unit_nodes(root);
    debug_assert_eq!(unit_nodes.len(), facts.procedural_units.len());
    let mut max_unit: Option<(usize, f64)> = None;
    for (idx, node) in unit_nodes.iter().enumerate() {
        let mut unit_facts = query_facts_of(node);
        merge_query_facts(&mut unit_facts, &continuation_facts[idx]);
        let score = crate::composite::structural(&unit_facts);
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

/// The declarative query facts of one region's subtree — the input to
/// `sql.structural_complexity` scoring (§8.1).
///
/// Nested routine definitions are *excluded*: a subprogram declared inside
/// the region scores its own queries when it is scored as its own unit, so
/// embedded complexity follows the same innermost-ownership contract as the
/// control-flow increments — an outer routine with no query of its own
/// cannot outrank its child on the child's query (Codex P2). The walk
/// descends past containers that hold nested definitions and runs the
/// extractors on each maximal definition-free subtree, merging with
/// [`merge_query_facts`] (sums for counts, max for depths — the same merge
/// continuations use, so depth semantics stay piece-relative either way).
fn query_facts_of(region: &ErasedSegment) -> SqlFileFacts {
    let mut acc = SqlFileFacts::default();
    let mut pending: Vec<ErasedSegment> = region.segments().to_vec();
    while let Some(node) = pending.pop() {
        if crate::facts::PROCEDURAL_UNITS.contains(node.get_type()) {
            continue; // a nested unit: scored separately
        }
        let holds_nested = !node
            .recursive_crawl(
                &crate::facts::PROCEDURAL_UNITS,
                true,
                &SyntaxSet::EMPTY,
                false,
            )
            .is_empty();
        if holds_nested {
            pending.extend(node.segments().iter().cloned());
        } else {
            // Only *query* nodes feed embedded facts (Codex P2): crawl the
            // maximal query roots (recurse_into=false keeps a WITH's inner
            // SELECTs from double-extracting) and extract within each.
            for query in node.recursive_crawl(&QUERY_ROOTS, false, &SyntaxSet::EMPTY, true) {
                merge_query_facts(&mut acc, &subtree_query_facts(&query));
            }
        }
    }
    acc
}

/// The query facts of one query-root subtree, collected with the node as
/// the crawl root so subquery depths are query-relative.
fn subtree_query_facts(region: &ErasedSegment) -> SqlFileFacts {
    let mut mini = SqlFileFacts::default();
    let selects = region.recursive_crawl(&SELECT_STATEMENT, true, &SyntaxSet::EMPTY, true);
    mini.query_block_count = selects.len() as u32;
    crate::facts::extract_joins(region, &mut mini.joins);
    crate::facts::extract_set_ops(region, &mut mini.set_ops);
    crate::facts::extract_cases(region, &mut mini.cases);
    crate::facts::extract_windows(region, &mut mini.windows);
    crate::facts::extract_aggregates(region, &mut mini.aggregates);
    crate::facts::extract_predicates(region, &mut mini.predicates);
    crate::facts::extract_subqueries(region, &selects, &mut mini.subqueries);
    crate::facts::extract_expressions(region, &mut mini.expressions);
    crate::facts::extract_cte_graph(region, &mut mini.ctes);
    mini
}

/// Merge one region's query facts into a routine's accumulator: additive
/// fields sum, depth-shaped fields take the maximum. Covers exactly the
/// fields `composite::structural` reads (§8.1) — a routine split across
/// parser fragments scores as one routine, not once per fragment
/// (Codex P2).
fn merge_query_facts(acc: &mut SqlFileFacts, other: &SqlFileFacts) {
    acc.query_block_count += other.query_block_count;
    acc.ctes.count += other.ctes.count;
    acc.ctes.max_dependency_depth = acc
        .ctes
        .max_dependency_depth
        .max(other.ctes.max_dependency_depth);
    acc.joins.total += other.joins.total;
    acc.joins.left += other.joins.left;
    acc.joins.right += other.joins.right;
    acc.joins.full += other.joins.full;
    acc.joins.cross += other.joins.cross;
    acc.subqueries.count += other.subqueries.count;
    acc.subqueries.max_depth = acc.subqueries.max_depth.max(other.subqueries.max_depth);
    acc.subqueries.correlated_count += other.subqueries.correlated_count;
    acc.subqueries.derived_table_count += other.subqueries.derived_table_count;
    acc.predicates.boolean_operator_count += other.predicates.boolean_operator_count;
    acc.predicates.max_boolean_depth = acc
        .predicates
        .max_boolean_depth
        .max(other.predicates.max_boolean_depth);
    acc.cases.count += other.cases.count;
    acc.cases.max_depth = acc.cases.max_depth.max(other.cases.max_depth);
    acc.windows.function_count += other.windows.function_count;
    acc.aggregates.function_count += other.aggregates.function_count;
    acc.set_ops.count += other.set_ops.count;
    acc.expressions.max_depth = acc.expressions.max_depth.max(other.expressions.max_depth);
}
