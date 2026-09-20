# SQL Parser Selection for `mehen-sql`

**Status:** decision-support analysis
**Author:** evaluation pass (hands-on, repos cloned and one candidate built)
**Date:** 2026-05-24
**Companion doc:** [`mehen_sql_metrics_research_foundation.md`](./mehen_sql_metrics_research_foundation.md)
**Addendum:** [§8 — `apache/datafusion-sqlparser-rs` re-evaluation](#8-addendum--apachedatafusion-sqlparser-rs-2026-07-27) (2026-07-27, post-adoption)

## 0. TL;DR

| | **sqruff** (`quarylabs/sqruff`) | **sqlfluffrs** (`sqlfluff/sqlfluff/sqlfluffrs`) | **ANTLR grammars-v4 + `antlr-rust-runtime`** | **`sqlparser`** (`apache/datafusion-sqlparser-rs`) † |
|---|---|---|---|---|
| Verdict | **Recommended primary parser** | Not recommended as a dependency now | Niche supplement for deep PL/SQL / T-SQL only | Rejected — AST discards comments, no error recovery |
| Language | Native Rust | Rust, but a build-component of a Python project | Generated Rust over a young Rust runtime | Native Rust |
| License | Apache-2.0 | MIT | MIT/BSD per-grammar + BSD-3 runtime | Apache-2.0 (ASF-governed) |
| Build as git dep | Plain `cargo build` (verified) | **Requires Python + SQLFluff source to codegen dialects at build time** | Needs ANTLR (Java) at dev time; generated Rust can be committed | Published on crates.io, semver |
| Node model | One `SyntaxKind` enum (1087 variants) shared across all dialects | String-typed segments shared across dialects | One generic `ParseTree`; **rule vocabulary differs per dialect grammar** | Typed `enum Statement`/`Expr` — **AST, lossy** |
| Built-in analysis | CTE/query graph, scopes, aliases, wildcards, **column lineage** | None (pure lex+parse) | None (pure CST) | None (explicitly syntax-only, no semantics) |
| Dialects | 17, all hand-written Rust, feature-gated | ~28 (transpiled from Python) | 20 independent grammars | 16 dialect structs |
| Source spans | Verified line:col on every node | `pos_marker` per token | Token line:col | `Spanned` trait, **officially incomplete** (#1548) |
| Comments in tree | **Yes** (comment nodes w/ byte spans) | Yes (tokens) | Yes (hidden channel) | **No** — tokenizer-only |
| Error recovery | **Yes** (`Unparsable` nodes) | Yes (`unparsable`) | Yes (`Error` nodes) | **No** — one error ⇒ zero statements |

**Bottom line:** sqruff is the only candidate that compiles as an ordinary Rust git dependency, exposes a single dialect-agnostic typed node model with reliable spans, and already ships the higher-level CTE/scope/lineage analysis that the metrics document assumes. It covers essentially the entire proposed metric catalogue. The other three each carry a structural blocker (sqlfluffrs: a Python build-time dependency; ANTLR: no shared node vocabulary + unrunnable semantic predicates for the most important dialects; `sqlparser`: a lossy AST with no comment nodes and no error recovery, so the `sql.loc.*` and `sql.parser.*` families cannot be computed).

† Added by the [§8 addendum](#8-addendum--apachedatafusion-sqlparser-rs-2026-07-27) (2026-07-27); not part of the original 2026-05-24 evaluation.

---

## 1. What the metrics actually demand from a parser

Distilled from the research foundation, the parser must provide:

1. **Reliable per-node source spans** (line/col) — for top-offender attribution (§4.7, §10).
2. **A dialect-agnostic node vocabulary** — so one extractor serves many dialects, matching mehen's "shared output, language-owned semantics" model (§2).
3. **Statement-kind classification** across DDL/DML/DCL/TCL/procedural (§5.2, §6.14).
4. **Query-block + CTE structure with a dependency graph** (§5.3–§5.5, §6.3–§6.4).
5. **Join, subquery (incl. correlation), set-op, CASE, window, predicate trees** (§6.5–§6.12).
6. **Scope/identifier resolution** — CTE vs table vs alias, qualification, wildcards (§5.4, §6.13).
7. **Graceful failure surface** — unparsable segments + diagnostics for confidence metrics (§6.16).
8. **Procedural control-flow nodes** for PL/SQL & T-SQL (§6.17, Phase 3).
9. **Optional column lineage** (§8.7, Phase 4).

The recurring theme: the document does not just want a token stream — it wants a *structured, dialect-normalized* tree plus some graph/scope analysis on top.

---

## 2. Candidate A — sqruff (quarylabs)

Cloned at `v0.38.0` (commit `63ae4c4f`). Crates: `lib-core` (lexer+parser+segment model+analysis utils), `lib-dialects` (17 dialects), `lineage` (column lineage), `sqlinference`, `lib` (linter/templaters), `lsp`, `cli`.

### 2.1 Node model and spans

- The CST is `ErasedSegment` (= `Rc<NodeOrToken>`); every node carries a `SyntaxKind` (single enum, **1087 variants**) and an optional `PositionMarker`.
- Traversal is first-class: `recursive_crawl(types, …)`, `child`/`children(SyntaxSet)`, `get_start_loc()/get_end_loc()` returning `(line, col)`, plus `is_code/is_comment/is_whitespace/is_meta` and `is_templated()` (literal vs templated spans).
- **One enum across all 17 dialects** is the single biggest ergonomic win: a metric extractor written once (`SyntaxKind::JoinClause`, `CaseExpression`, `OverClause`, …) works for postgres, tsql, snowflake, bigquery, etc.

### 2.2 Built-in higher-level analysis (this is the differentiator)

`utils/analysis/query.rs` ships a `Query`/`Selectable` model that already provides, for free, much of §5:

- `QueryInner { query_type, selectables, ctes: IndexMap<name, Query>, parent, subqueries, cte_definition_segment, cte_name_segment }`.
- `crawl_sources()` resolves each source as **CTE-reference vs base table** (`Source::Query` vs `Source::TableReference`) — i.e. the CTE dependency graph is derivable directly.
- `select_info()` → table aliases, select targets, column aliases, `using` columns; `wildcard_info()` → `SELECT *` / `t.*` with the tables they expand.
- `TableReference::is_qualified()` → directly feeds `sql.identifier.unqualified_column_ratio`.
- A separate `lineage` crate (`Lineage::new(parser, column, sql).build()`) gives column-level lineage for the optional `sql.lineage.*` family (Phase 4) on the same parser.

### 2.3 Empirical verification (built and run)

I added `sqruff-lib-core` + `sqruff-lib-dialects` (postgres feature only) as path deps to a throwaway crate and parsed a deliberately gnarly query (recursive CTE + `UNION ALL`, `LEFT JOIN` with compound `ON`, window function with explicit `ROWS` frame, nested `CASE`, `IN (subquery)`, correlated scalar subquery, `r.*`). Output:

```
lex errors: 0
unparsable segments: 0
SelectStatement       = 7      CommonTableExpression = 3
JoinClause            = 3      SetExpression         = 1   (UNION ALL)
CaseExpression        = 2      OverClause            = 1
WindowSpecification   = 1      FrameClause           = 1
ColumnReference       = 31     WildcardExpression    = 1   (r.*)
FunctionContents      = 2
  join span: L6:20..L6:62   "JOIN region_tree rt ON r.parent_id = rt.id"
  join span: L11:5..L11:70  "LEFT JOIN customers c ON s.customer_id = c.id AND c.active = true"
  join span: L25:1..L25:43  "JOIN region_tree rt ON r.region_id = rt.id"
query_type: WithCompound
CTEs detected: ["REGION_TREE", "SALES_BASE", "RANKED"]
top-level subqueries: 1
```

Everything the Phase-1 catalogue needs came out of one parse, with correct spans, **zero** unparsable segments, and the CTE/subquery graph recovered by the built-in analyzer. Incremental rebuild after the first compile was 0.31s.

### 2.4 Coverage of the proposed metric families

Confirmed `SyntaxKind` variants exist for: `CommonTableExpression`, `JoinClause`, `JoinOnCondition`, `SetExpression`/`SetOperator`, `CaseExpression`/`WhenClause`, `OverClause`/`WindowSpecification`/`FrameClause`/`PartitionClause`, `GroupbyClause`/`CubeRollupClause`/`GroupingSetsClause`, `MergeStatement`/`MergeMatch`, `QualifyClause`, `FromPivotExpression`/`FromUnpivotExpression`, `WildcardExpression`, `CastExpression`, `FunctionContents`, `Expression`, `ColumnReference`, every `*Statement` (insert/update/delete/truncate/drop/alter/access/transaction…), and procedural ones (`IfStatement`, `LoopStatement`, `WhileStatement`, `ForLoopStatement`, `BeginEndBlock`, `TryCatch`, `RaiseStatement`, `ReturnStatement`, `CreateProcedureStatement`, `DeclareStatement`, `ExecuteStatement`). `SyntaxKind::Unparsable` is the recovery node for confidence metrics.

### 2.5 Cons / risks

- **`Rc`-based tree is not `Send`/`Sync`.** mehen's `LanguageAnalyzer` is `Send + Sync` and returns *owned* `LanguageAnalysis`. This is fine because parsing+extraction happen inside a single `analyze()` call and only owned `MetricSet`/`MetricContribution` escape — the same pattern mehen already uses around non-`Send` parse state. Constraint to respect: do not hold an `ErasedSegment` across threads; extract facts within the call.
- **API stability is not guaranteed** (Open question #1 in the research doc). `lib-core` is an internal crate of an app, version `0.x`, no semver promise. Mitigation: the metrics doc already mandates a `parser_adapter` boundary that converts `SyntaxKind` nodes into mehen `SqlFact`s — keep that thin seam so a sqruff bump is contained.
- **Procedural depth is linter-grade, not exhaustive.** The procedural `SyntaxKind`s exist and tsql/oracle dialects use them, but sqruff's oracle/PL-SQL surface is narrower than the dedicated ANTLR `plsql` grammar. Acceptable for Phase 1–2; revisit for a deep Phase-3 procedural push (see §4).
- **Dependency weight:** pulls `fancy-regex`, `strum`, `indexmap`, `hashbrown`, `smol_str`, `serde_yaml` (in dialects). Comparable to what mehen already absorbs for ruff/tree-sitter. `lib-dialects` is feature-gated, so you can compile only the dialects you ship.
- **Templating (Jinja/dbt) lives in the heavier `lib` crate**, which pulls Python templater plumbing. For standalone `.sql` you only need `lib-core` + `lib-dialects`; treat templating as an opt-in later decision (Open question #3).

---

## 3. Candidate B — sqlfluffrs (the Rust crate inside SQLFluff)

Cloned at `v4.2.1` (commit `3fdeaf50`). Workspace: `sqlfluffrs_types` (token/marker/grammar tables), `sqlfluffrs_lexer`, `sqlfluffrs_dialects`, `sqlfluffrs_parser` (table-driven), `sqlfluffrs_python` (pyo3).

### 3.1 The decisive blocker: dialects are generated from Python at build time

`sqlfluffrs_dialects/build.rs` (quoting its own header): the generated dialect sources `src/dialect/<name>/{parser,matcher}.rs` and `src/dialect/mod.rs` are **not checked into version control**; they are produced by running `python utils/rustify.py build`, which imports the SQLFluff Python package (`from sqlfluff.core.dialects import dialect_readout`) and transpiles each Python dialect into Rust. I confirmed `sqlfluffrs_dialects/src/dialect/` does not exist in a fresh checkout.

Consequences for using it as a Cargo git dependency:

- `cargo build` in mehen would shell out to a **Python interpreter** and require the SQLFluff source tree importable (build.rs prepends `<repo>/src` to `PYTHONPATH`). That is a hard, non-Rust build prerequisite on every dev machine and CI runner.
- It contradicts the whole point of mehen's generated-code policy (commit the generated `grammar.rs`, verify drift in CI). sqlfluffrs regenerates on mtime, into `OUT_DIR`-adjacent paths, from a Python toolchain you don't control.
- The project README is explicit: *"not intended to be used as a standalone linting solution… experimental,"* and AGENTS.md: *"Experimental and incomplete… may have compatibility issues with some dialects."* Its release cadence is tied to SQLFluff's Python releases, and the `python` feature wires in `pyo3`.

### 3.2 If that blocker were removed

The token model would be workable but weaker than sqruff:

- `Token { token_type: String, class_types: HashSet<String>, pos_marker: Option<PositionMarker>, segments: Vec<Token>, … }` — node types are **strings** (mirrors SQLFluff's dynamic Python typing). You'd match `"select_statement"`, `"join_clause"`, `"common_table_expression"` by string — no enum exhaustiveness, slower comparisons, easy to typo.
- **No Rust analysis layer at all** — `sqlfluffrs` is lexer+parser only. The CTE/query graph, scope resolution, wildcard expansion, correlation detection, and lineage that sqruff hands you would all have to be re-implemented from scratch in Rust against string-typed nodes.
- Spans exist (`pos_marker`), and dialect breadth (~28, transpiled) is the widest in theory — but only as good as the in-progress transpiler, which the maintainers call incomplete.
- The owned `Vec<Token>` tree (with `Weak<Token>` parents) is likely `Send`, a minor plus over sqruff's `Rc`, but irrelevant given the build blocker.

### 3.3 Verdict

Re-evaluate only if upstream ever ships **pre-generated, checked-in Rust dialects** (or a published crate on crates.io with no Python build step). Until then, the build-time Python dependency disqualifies it for a Rust-only CLI.

---

## 4. Candidate C — ANTLR grammars-v4 + `ophidiarium/antlr-rust-runtime`

`grammars-v4/sql` has 20 independent dialect grammars (postgresql, plsql, tsql, mysql, sqlite, snowflake, db2, hive, trino, clickhouse, databricks, mariadb, teradata, …; **no generic ANSI, no BigQuery, no DuckDB**). `antlr-rust-runtime` is `v0.3.0`, BSD-3, a clean-room runtime with a **metadata-first** generator: `antlr4-rust-gen` consumes ANTLR `.interp` files (serialized ATN + token/rule names) and emits Rust. It passes the full upstream runtime-testsuite (357 descriptors).

### 4.1 Structural blockers for the metrics use case

1. **No shared node vocabulary.** The generated tree is a generic `ParseTree { Rule(RuleContext), Terminal, Error }`; you navigate by `rule_index → rule_names[idx]` (a string) and positional children — there are no typed accessors. Worse, each dialect grammar is authored independently, so postgresql's rule names bear no relation to tsql's or sqlite's. A metric extractor would have to be **rewritten per dialect grammar** — the opposite of mehen's shared-vocabulary model and an N× maintenance burden.
2. **Semantic predicates/actions can't run from `.interp`.** The runtime's path deserializes the ATN but cannot execute target-language semantic predicates or `superClass` helper methods (their code isn't in `.interp`). The two most important relational dialects depend on exactly this:
   - `postgresql` → `superClass = PostgreSQLLexerBase/ParserBase` + 9 predicates (dollar-quoting, etc.).
   - `plsql` → `superClass = PlSqlLexerBase/ParserBase` + 20 predicates.
   - `mysql` (Oracle/original) → `superClass = MySQLBaseRecognizer` + many `{this.serverVersion >= …}?` predicates.

   These base classes are shipped for Java/C#/Go/JS/Python/TS/C++ — **not Rust**. Using those grammars means hand-porting the base classes to Rust *and* wiring predicate evaluation, per grammar. `tsql`, `snowflake`, and `sqlite` are the clean ones (no `superClass`, 0 predicates, no embedded actions) and would generate/parse cleanly.
3. **No analysis layer whatsoever.** Pure CST. CTE graph, scopes, correlation, wildcard expansion, lineage — all from scratch, on top of generic rule contexts.
4. **No normalized statement kinds.** `select` vs `insert` vs `create_procedure` is just a rule name that differs per grammar; you build the §5.2 taxonomy by hand for each.

### 4.2 The one place ANTLR wins

The `plsql` (12.6k lines) and `tsql` (7.6k lines) grammars are the most complete procedural-SQL grammars in existence. For a *deep* Phase-3 procedural push (full PL/SQL exception/cursor/loop semantics, T-SQL `TRY/CATCH`/`WHILE`/cursors), the dedicated ANTLR grammars model far more than sqruff's linter-oriented procedural surface. `tsql` is the sweet spot: no base classes, no predicates → generates cleanly onto `antlr-rust-runtime`, and `.interp`-generated Rust can be **committed** (matching mehen's generated-`grammar.rs` policy, with `antlr4-rust-gen` playing the role `xtask tree-sitter generate` plays today).

### 4.3 Verdict

Not viable as the primary/general SQL parser: no cross-dialect vocabulary, broken predicate handling for postgres/plsql/mysql, and everything above the CST built from zero. Worth keeping in the back pocket as a **dedicated procedural augmentation** (tsql first, then plsql if the base classes are ported) once Phase-3 demands depth sqruff can't reach.

---

## 5. Side-by-side metric-coverage matrix

Rating each parser by how much work the proposed metric family needs.
**Direct** = node/API exists, count/measure immediately · **Derive** = straightforward traversal/aggregation on existing nodes · **Build** = must implement a non-trivial analysis layer yourself · **Blocked** = structural obstacle before you can start.

| Metric family (doc §) | sqruff | sqlfluffrs* | ANTLR (clean dialects)** |
|---|:--:|:--:|:--:|
| LOC / size / comments (6.1) | Direct | Direct | Direct |
| Statement kinds DDL/DML/DCL/TCL (6.2, 6.14) | Direct | Derive | Build (per grammar) |
| Query blocks + depth (6.3) | Direct | Derive | Derive |
| CTE count + dependency graph (6.4) | **Direct** (`Query.ctes`, `crawl_sources`) | Build | Build |
| Joins + kinds (6.5) | Direct | Derive | Derive |
| Subquery + derived tables (6.6) | Direct | Derive | Derive |
| Correlated-subquery detection (6.6) | Derive (parent links exist) | Build | Build |
| Predicate / boolean tree (6.7) | Direct/Derive | Derive | Derive |
| CASE incl. nesting (6.8) | Direct | Derive | Derive |
| Aggregation / GROUPING SETS / ROLLUP (6.9) | Direct | Derive | Derive |
| Window incl. frames (6.10) | Direct | Derive | Derive |
| Set ops + depth (6.11) | Direct | Derive | Derive |
| Expression depth / function nesting (6.12) | Direct | Derive | Derive |
| Output shape: `*`, alias coverage (6.13) | **Direct** (`wildcard_info`, `select_info`) | Build | Build |
| Unqualified-column ratio (6.13) | Derive (`is_qualified`) | Build | Build |
| Object touch / migration risk (6.14) | Direct | Derive | Build |
| Halstead operators/operands (7) | Derive | Derive | Derive |
| Dialect / portability (6.15) | Direct (`DialectKind` + dialect kinds) | Derive | Build (no generic ANSI) |
| Parser health / unparsable (6.16) | **Direct** (`SyntaxKind::Unparsable`) | Direct (`unparsable`) | Derive (`Error` nodes) |
| Procedural cyclomatic/cognitive (6.17) | Derive (linter-grade) | Derive | **Direct** (plsql/tsql richest) |
| Column lineage (8.7) | **Direct** (`lineage` crate) | Build | Build |

\* sqlfluffrs ratings assume the **Python build-time blocker is solved** — otherwise the whole column is Blocked.
\*\* ANTLR ratings are for `tsql`/`snowflake`/`sqlite`; for `postgresql`/`plsql`/`mysql` every cell is **Blocked** until Rust base classes + predicate evaluation are hand-ported.

---

## 6. Fit with mehen's architecture

- **Git-dependency precedent:** mehen already pins ruff via tagged git deps. sqruff fits the same pattern cleanly (path/git, feature-gated dialects, plain `cargo build`). sqlfluffrs breaks it (Python at build time). ANTLR sidesteps it by committing generated Rust, but needs the Java ANTLR tool at *generation* time (a dev/xtask step, not a build step).
- **Generated-code policy:** mehen forbids hand-editing generated `grammar.rs` and checks drift in CI via `xtask`. ANTLR's `.interp → antlr4-rust-gen → committed Rust` maps onto this policy naturally; sqlfluffrs violates it (regenerates from Python into uncommitted paths); sqruff is hand-written Rust (no codegen concern).
- **`Send + Sync` analyzer contract:** sqruff (`Rc`) and tree-sitter (borrowed nodes) both require extract-within-the-call — mehen already does this. sqlfluffrs (`Vec<Token>`/`Arc`) is friendliest here; ANTLR depends on the generated context ownership.
- **Adapter seam:** regardless of choice, implement the doc's `parser_adapter` → `SqlFact` boundary so metrics never reference parser-internal node names directly. This is cheap with sqruff's enum, essential with ANTLR's per-grammar vocabularies, and the only thing that would make a future parser swap survivable.

---

## 7. Recommendation

1. **Adopt sqruff (`lib-core` + `lib-dialects`) as the `mehen-sql` parser.** It is the only candidate that builds as a normal Rust dependency, gives one typed node vocabulary across 17 dialects with verified spans, ships the CTE/scope/wildcard analysis the metrics assume, and even has a column-lineage crate for Phase 4. The hands-on probe showed it covers the entire Phase-1 catalogue from a single parse.
2. **Wrap it behind the `parser_adapter`/`SqlFact` boundary** the research doc already specifies, so the `0.x` API surface and the `Rc` tree stay contained and a later swap is localized.
3. **Defer templating:** start with `lib-core`+`lib-dialects` for standalone `.sql`; only pull sqruff's `lib` templaters (or emit templating-burden metrics) once Open question #3 is decided.
4. **Hold ANTLR `tsql`/`plsql` in reserve for Phase 3** *iff* procedural depth becomes a hard requirement that sqruff's linter-grade procedural nodes can't satisfy. If pursued, start with `tsql` (clean grammar, commits generated Rust via `antlr4-rust-gen` like the tree-sitter `xtask` flow). Do **not** take on postgres/plsql/mysql ANTLR grammars without budgeting the Rust base-class + predicate-evaluation port.
5. **Drop sqlfluffrs** from consideration unless it later publishes pre-generated, checked-in Rust dialects (or a crates.io release with no Python build step).

### Suggested `mehen-sql/Cargo.toml` shape

```toml
# Pinned here (single consumer), mirroring the ruff pattern.
sqruff-lib-core = { git = "https://github.com/quarylabs/sqruff", tag = "v0.38.0" }
sqruff-lib-dialects = { git = "https://github.com/quarylabs/sqruff", tag = "v0.38.0",
                        default-features = false,
                        features = ["postgres", "tsql", "snowflake", "bigquery",
                                    "mysql", "sqlite", "duckdb", "oracle"] }
# Phase 4 (optional): sqruff-lineage for sql.lineage.*
```

---

## Appendix — evidence log

- Repos cloned to `/tmp/sql-parser-eval/`: `sqruff` (`v0.38.0`), `sqlfluff` (incl. `sqlfluffrs` `v4.2.1`), `grammars-v4`, `antlr-rust-runtime` (`v0.3.0`).
- sqruff parse probe: `/tmp/sql-parser-eval/probe` (path-deps on `lib-core` + `lib-dialects[postgres]`), built and run with `rustc 1.89.0`; results in §2.3.
- sqlfluffrs build blocker: read from `sqlfluffrs/sqlfluffrs_dialects/build.rs` and confirmed `src/dialect/` absent in a fresh checkout; dialect count from `src/sqlfluff/dialects/dialect_*.py` (28).
- ANTLR predicate/base-class findings: `rg` over `grammars-v4/sql/*/*.g4` (`superClass`, `}?`) and the shipped per-language `*Base` directories (no Rust); runtime capabilities from `antlr-rust-runtime/README.md` + `docs/runtime-testsuite.md` and the generic `ParseTree` walker in `tests/kotlin-parity/dumper/src/main.rs`.

---

## 8. Addendum — `apache/datafusion-sqlparser-rs` (2026-07-27)

**Status:** post-adoption re-evaluation · **Verdict: keep sqruff; do not migrate.**

A fourth candidate that the original pass never evaluated: the `sqlparser` crate
(`apache/datafusion-sqlparser-rs`), the SQL front end for Apache DataFusion.
It is the most prominent SQL parser in the Rust ecosystem, so its absence from
§0 was a real gap. This addendum closes it.

Probed at **`sqlparser` v0.62.0** (crates.io, Apache-2.0) against
**sqruff v0.39.0** as currently pinned in `crates/mehen-sql/Cargo.toml`.

### 8.1 The decisive difference: AST vs CST

sqruff produces a **lossless CST** — every byte of input, including comments and
whitespace, is a node. `sqlparser` produces an **abstract** syntax tree that
discards trivia by design; its README advertises round-tripping "with comments
removed, normalized whitespace and keyword capitalization".

For a query engine that is the correct trade-off: DataFusion wants semantics,
not formatting. For a *metrics* tool it is disqualifying. Two published metric
families are trivia- or recovery-derived and have no AST equivalent:

- `sql.loc.{physical,code,comment,blank,logical,comment_density,max_statement_lines,avg_statement_lines}`
  — `loc.rs` classifies lines by **comment byte coverage** taken from
  `SyntaxKind::{Comment,InlineComment,BlockComment}` nodes. Its module doc
  explains why a marker scan is wrong: an interior line of a multi-line block
  comment carries no `/*`/`*/` of its own. The four unit tests at the foot of
  `loc.rs` encode exactly those edge cases.
- `sql.parser.{unparsable_segment_count,unparsable_line_count,unparsable_ratio,diagnostic_count}`
  — these exist only because sqruff emits `SyntaxKind::Unparsable` recovery
  nodes and keeps going.

That is 12 of the crate's metric keys that depend on properties `sqlparser`
does not expose in its tree, plus the per-statement `MetricSpace` attribution
and `change_risk_evidence` contributions that need spans on deep nodes.

### 8.2 Empirical probe

A throwaway crate (`cargo add sqlparser --features visitor`) run against the
same inputs as our `SqlAnalyzer`. Every row below was executed, not inferred.

| Probe | `sqlparser` 0.62 | sqruff 0.39 (via `mehen-sql`) |
|---|---|---|
| §2.3 "gnarly" query (recursive CTE, window+frame, nested CASE, correlated subquery) | ✅ parses, 1 stmt, span `L1:1..L29:17` | ✅ parses, 0 unparsable |
| **Comments in tree** | ❌ **absent** — AST debug contains no comment text; `SELECT 1 AS x -- trailing` round-trips to `SELECT 1 AS x FROM t` | ✅ comment nodes with byte spans |
| Comments from tokenizer | ⚠️ 3 tokens as `Token::Whitespace(SingleLineComment/MultiLineComment)` with line:col — recoverable via a second pass | ✅ already tree-attached |
| **Error recovery** on `SELECT a FROM t; SELCT SELCT bogus ***; SELECT b FROM u;` | ❌ **hard `Err`, zero statements** — both valid statements lost | ✅ `Ok`: `loc.code=3`, `unparsable_segment_count=1`, `unparsable_ratio=0.67`, warning diagnostic |
| T-SQL `BEGIN TRY … END CATCH` | ❌ `Err` *even with* `MsSqlDialect` | ✅ 0 unparsable (with `-- sqlfluff:dialect:tsql`) |
| T-SQL `WHILE @i < 10 BEGIN … END` | ❌ `Err` with `MsSqlDialect` | ✅ 0 unparsable |
| PL/SQL `BEGIN IF x > 1 THEN NULL; END IF; END;` | ❌ `Err` with `OracleDialect` | ✅ 0 unparsable (with `:oracle`) |
| `CREATE PROCEDURE p AS BEGIN … END` | ❌ `Err` (both MsSql and Oracle) | ✅ 0 unparsable |
| QUALIFY · UNNEST · `$$…$$` · MERGE · GROUPING SETS · PIVOT | ✅ all OK on `GenericDialect` | ✅ all supported |
| `Send + Sync` tree | ✅ `Vec<Statement>` is both | ❌ `Rc`-based `ErasedSegment` is neither |
| Transitive crates (`cargo tree --edges normal`, parser subtree only) | **13** | **40** |

**Caveat on the procedural rows.** sqruff's advantage there is *conditional*: it
only materializes with an explicit dialect. Under inference all three fall back
to `ansi` and report `unparsable=1, ratio=1.00`. Since `requested_dialect()` in
`lib.rs` still returns `None`, the only way to set one today is an in-file
`-- sqlfluff:dialect:<name>` directive. See §8.5.

This result also **inverts §4.2's assumption** that procedural depth requires
the ANTLR `plsql`/`tsql` grammars: sqruff handles all four procedural probes
that `sqlparser` rejects outright.

### 8.3 Where `sqlparser` genuinely wins

1. **`Send + Sync` AST.** The one real architectural improvement. `facts.rs`
   documents the current workaround in its module header — extract everything
   into owned `SqlFileFacts` inside one `analyze()` call because the `Rc` tree
   cannot cross threads. With `sqlparser` the adapter seam would be optional
   rather than mandatory.
2. **Governance and API stability.** ASF-owned, on crates.io with semver, 3.3k
   stars, 300 contributors, 66M all-time downloads, 323 reverse dependencies.
   The README states the maintainers "do not plan for any substantial changes
   to this crate's API." This directly addresses the §2.5 risk — sqruff is a
   `0.x` internal crate of a linter app, git-tag pinned, with no semver promise
   (cf. the duplicate-`Dialect` `E0308` breakage from ungrouped bumps).
3. **Lighter tree:** 13 vs 40 crates. No `fancy-regex`, `serde_yaml`, `strum`,
   or `unsafe-libyaml`.
4. **Typed ergonomics.** A real `enum Statement` beats matching a 1087-variant
   flat `SyntaxKind`. Our own code shows the cost of the latter: `facts.rs`
   repeatedly falls back to raw-text sniffing (`stmt.raw().to_ascii_uppercase()`,
   `seg.raw().eq_ignore_ascii_case("NOT")`, string-matching `"USING"`) where a
   typed AST would offer field access.

### 8.4 Where it loses

- **Comments absent from the AST** (§8.1). Recoverable via
  `tokenize_with_location()`, but that means a second tokenizer pass plus
  re-deriving the trivia/code interleaving sqruff supplies directly.
- **No error recovery.** `parse_sql` returns `Result<Vec<Statement>>`: one
  syntax error anywhere yields nothing. For a tool pointed at whole repos this
  is not an edge case — a single vendor-specific statement in a migration file
  would zero out that file's metrics instead of degrading them.
- **Weaker procedural SQL,** contrary to expectation (§8.2).
- **Spans officially incomplete.** The `Spanned` docs state nodes "may be
  missing span information entirely, in which case they return `Span::empty()`",
  with per-type "partial span / Missing spans" annotations on `Expr`,
  `JoinOperator`, `GroupByExpr`, `JoinConstraint` and more
  ([issue #1548](https://github.com/apache/datafusion-sqlparser-rs/issues/1548)).
  Simple projection and `WHERE` spans were correct in the probe, but the gaps
  sit exactly where per-statement attribution needs them.
- **No analysis layer.** No CTE graph, scopes, `wildcard_info`, or lineage.
  This costs less than §5 assumed, since `facts.rs` already re-derives the CTE
  graph itself — but Phase-4 `sql.lineage.*` would lose sqruff's `lineage`
  crate entirely.

### 8.5 Recommendation

**Keep sqruff.** A migration would rewrite `facts.rs`, `loc.rs`, and
`dialect.rs` against a tree carrying *less* information than the current one —
trading comment nodes, error recovery, and procedural coverage for
`Send + Sync`, a lighter tree, and ASF governance. All 142 metric keys and
every `insta` snapshot would need revalidation. The `SqlFileFacts` adapter seam
(§6) makes the swap mechanically possible, which is the seam working as
designed; it should stay unexercised. The two costs that would justify it — the
API-churn tax and the missing-`Send` friction — are each currently cheaper than
the rewrite.

Two cheaper follow-ups, both parser-agnostic:

1. **Wire up `requested_dialect()`** (`lib.rs`). sqruff's procedural advantage
   only materializes with an explicit dialect, and there is no CLI flag to set
   one. Highest-value SQL change currently available.
2. **Keep the sqruff Dependabot group aligned** so the two `sqruff-*` git tags
   never drift apart.

**Where `sqlparser` could still earn a place:** as an optional cross-check
oracle for `sql.dialect.confidence`. Parsing a file with both and comparing
statement counts is genuine signal — its permissive `GenericDialect`
disagreeing with sqruff's inferred dialect indicates low confidence. Additive,
no rewrite required.

### 8.6 Addendum evidence log

- Probe crate: `/tmp/sqlparser-probe` (`sqlparser` v0.62.0, `visitor` feature),
  `rustc` 1.97.1. Covered: gnarly-query parse, comment presence in AST vs
  tokenizer, error recovery, `Send + Sync` (compile-time assertion), inner-node
  span quality, and a dialect syntax matrix over `Generic`/`MsSql`/`Oracle`/
  `Snowflake`.
- sqruff side: a temporary integration test in `crates/mehen-sql/tests/`
  driving `SqlAnalyzer::analyze` on identical inputs (removed afterwards;
  working tree left clean).
- Metric-key counts: `grep -oh '"sql\.[a-z_.]*"' crates/mehen-sql/src/*.rs`
  → 142 distinct literal keys, of which 8 `sql.loc.*` and 4 `sql.parser.*` are
  trivia/recovery-derived. Two families are built dynamically via `format!`
  (`sql.dialect.is_<name>`, `sql.statement.kind_count.<label>`) and so are not
  in that literal count.
- Dependency counts: `cargo tree --edges normal --prefix none`, deduplicated,
  excluding dev-dependencies and (on our side) `mehen-*`/`smol_str`, leaving
  the parser subtree only.
- Dialect inventory: `sqlparser::dialect` exposes 16 dialect structs plus
  `dialect_from_str`; our sqruff build compiles 12 feature-gated dialects
  alongside the always-present `ansi`.


---

## 9. Addendum — Phase-3 procedural re-probe (2026-08-20)

**Status:** pre-implementation check before building `sql.procedural.*` ·
**Verdict: §8.5 stands — keep sqruff for Phase 3 and Phase 4.**

Before implementing Phase 3 (procedural metrics), `sqlparser` was re-probed
specifically on the constructs Phase 3 must parse — *definitions with bodies*,
not isolated control-flow statements. `sqlparser` is still at **v0.62.0**
(May 2026, no newer release), so §8's structural findings stand; this pass
adds the procedural detail §8.2 lacked. sqruff side probed at **v0.40.0**
(current pin) via a throwaway CST dumper, not via `SqlAnalyzer`.

| Probe (realistic bodies) | `sqlparser` 0.62 | sqruff 0.40 |
|---|---|---|
| T-SQL `CREATE PROCEDURE dbo.p @x INT AS BEGIN … END` | ❌ `Err` at the header (`Expected: AS, found: @batch`) — zero statements | ✅ statement + header typed; `IF`/`WHILE` parse as keyword+`Expression`+nested `Statement`; tail of long bodies can degrade to `Unparsable` |
| T-SQL `BEGIN TRY … END CATCH` (isolated) | ❌ `Err` | ✅ parses (keyword run + typed nested statements) |
| T-SQL `EXEC sp_executesql` / `PRINT` / `THROW` / `GOTO` / cursor DDL at top level | ❌ `Err` (except bare `WHILE`) | ⚠️ `Unparsable`, but tokens stay classified (`Word`/`SingleQuote`/`InlineComment`), so token-level counting stays trivia-safe |
| PL/SQL `CREATE OR REPLACE PROCEDURE … IS … BEGIN … EXCEPTION … END` | ❌ `Err` — grammar has no Oracle `CREATE PROCEDURE` at all | ✅ rich typed nodes: `OracleCreateProcedureStatement`, `OracleBeginEndBlock`, `OracleIfThenStatement`/`OracleIfClause`, `WhileLoopStatement`, `OracleLoopStatement`, `OracleExitStatement`, `OracleExecuteImmediateStatement`, `RaiseStatement`, `OracleReturnStatement`, `OracleNullStatement`, `DeclareCursorVariable` |
| PL/SQL anonymous block `BEGIN IF … END IF; END;` | ❌ `Err` | ✅ parses |
| PL/SQL cursor `FOR rec IN c LOOP` / procedural `CASE` statement | n/a (whole file already `Err`) | ⚠️ `Unparsable` (graceful; surfaces in `sql.parser.*`) |

Key insight, sharper than §8.2 put it: `sqlparser`'s typed procedural AST
(`IfStatement`, `WhileStatement`, `RaiseStatement`, …) exists primarily for
BigQuery-style *scripting at top level*. The procedure/function *definitions*
that contain 95 % of real procedural SQL hard-fail to parse for both MsSql and
Oracle dialects — and with no error recovery, one such definition zeroes out
every metric for the file. That is the exact opposite of what Phase 3 needs,
and Phase 4 (`sql.lineage.*`) would additionally lose sqruff's `lineage` crate
(in-repo `crates/lineage`; note it is **not published to crates.io**, so
Phase 4 will need either a git pin exception or an upstream publish request).

Phase-3 implementation consequences adopted from this probe:

1. **Oracle/PL-SQL metrics ride the typed CST nodes** listed above.
2. **T-SQL metrics fall back to lexed-token counting** (`Keyword`/`Word`
   tokens inside procedural statements and `Unparsable` runs). This is still
   lexer-derived, never regex-on-text: comments lex as `InlineComment`/
   `BlockComment` and string literals as `SingleQuote` even inside
   `Unparsable`, so `-- exec this` or `'goto'` cannot false-match.
3. **ANTLR `tsql` stays the escalation path** (§7.4) if linter-grade T-SQL
   depth ever stops being enough — the runtime/codegen infrastructure now
   exists in-repo (`mehen-antlr`, `cargo xtask antlr generate`), which §4
   predates. `sqlparser` is not that path.

Evidence: probe crates `/tmp/sqlparser-probe3` (sqlparser v0.62.0) and
`/tmp/mehen-sql-dump` (sqruff v0.40.0 CST dumper), 2026-08-20.
