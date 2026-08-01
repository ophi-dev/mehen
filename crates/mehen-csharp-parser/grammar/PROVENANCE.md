# C# ANTLR grammar — provenance

The **source of truth** is the single vendored file `CSharp.Generated.g4`, taken
verbatim from `dotnet/roslyn`. Everything else in this directory is either input
to the transform or produced by it:

| File | Role |
|---|---|
| `CSharp.Generated.g4` | vendored upstream grammar (see Source) |
| `lexer-tokens.g4.in` | hand-written lexer — Roslyn publishes none |
| `prepare-grammar.py` | the transform; a step of parser generation |
| `CSharpLexer.g4`, `CSharpParser.g4`, `patterns.toml` | **derived, gitignored** |

`cargo run -p xtask -- antlr generate csharp` runs the transform and then
`antlr4-rust-gen`, writing the Rust modules in `../src/generated/`. That needs
[`uv`](https://docs.astral.sh/uv/) in addition to the generator; the script's PEP
723 block pins the interpreter.

## Source

| Field | Value |
|---|---|
| Upstream | [`dotnet/roslyn`](https://github.com/dotnet/roslyn) — the C# compiler itself |
| Path | `src/Compilers/CSharp/Portable/Generated/CSharp.Generated.g4` |
| Branch | `main` |
| Repo revision | `c9f12709e0cd477febd54d1a5b5e3e3731a1ada2` (2026-07-31) |
| File last changed | `76234ec6a1ba46f05b8b07dbeaeb7e39c5054810` (2026-06-24) |

The grammar is machine-generated from `Syntax.xml`, the same model that generates
the compiler's own syntax nodes, so it tracks **C# as implemented** — records,
`is not`, `and`/`or`/relational patterns, list patterns, collection expressions,
raw strings, primary constructors, `required` members. No community grammar does.

It is also a *reference* grammar rather than a working parser, which is what
`prepare-grammar.py` exists to fix; the transform's repairs are catalogued below.

### Why not `antlr/grammars-v4`

This crate previously vendored `csharp/v7/{CSharpLexer,CSharpParser}.g4` from
`antlr/grammars-v4`. It is a genuine C# 7-era grammar, so mainstream post-7
syntax simply does not parse — `switch` expressions, `is not`, `and`/`or`
patterns, and `record` declarations all fail, and C# 9 pattern syntax appears
throughout modern .NET. On the 321-file `System.Text.Json` corpus it parsed 93
files cleanly versus 308 for the derived Roslyn grammar.

Upstream's `csharp/v8-spec` is not the answer either: it stops at C# 8, so it
still lacks the C# 9 patterns that cause most failures, and its `superClass`
surface is ~35 helpers including a **symbol-table-driven scope stack**
(`EnterTypeScope`, `IsClassTypeName`, `IsTypeParameterName`, …). Those resolve
identifiers against declarations seen so far, so porting them means implementing
a semantic model rather than lookahead checks — and mehen's `--sem-unknown error`
policy requires every one to be exact or generation fails.

Note also that the grammar is **permissive by design** — it models Roslyn's
syntax nodes (including error-recovery nodes such as `incomplete_member`), not
the exact accepted language — and it encodes **no operator precedence**, since
real C# precedence lives in Roslyn's hand-written parser. Neither matters for
mehen's token-level metrics, but both would matter for a validating parser.

## What the transform repairs

Roslyn's grammar needs mutual (indirect) left recursion, which ANTLR rejects and
runtime 0.21.0 accepts via hub inlining (upstream #221). Beyond that, measured on
321 files of `dotnet/runtime`'s `System.Text.Json`:

| | clean | notes |
|---|---|---|
| `grammars-v4` C# 7 (previous) | 93 | C# 8+ syntax unsupported |
| Roslyn, first working prep | 115 | interpolated strings failed |
| Roslyn, current prep | **317** | 4 with diagnostics, no crashes or timeouts |

Measured end to end through `mehen metrics`, not just the parser: ~179 s for the
321 files. All 4 remaining files are the directive-split-expression case below.

Note that a "clean" corpus count measures *parseability*, not correctness — four
separate faults in this grammar produced structurally wrong trees with zero
reported errors (`declaration_expression` shadowing every invocation, bodiless
members falling through to `global_statement`, `(a, b) => …` parsing as the simple
lambda form, and `parameter` matching the empty string). Each was caught by a
metric test or a parse-tree dump, never by an error count.

**No runtime capability is missing.** Every failure traced to either the prep or
an upstream-generator blind spot, and each was fixable declaratively — SemIR
patterns via the derived `patterns.toml` plus one lexer lifecycle hook for
interpolated strings. The gaps are catalogued below; `prepare-grammar.py` is the
single source of the transform, so each is reproducible rather than hand-patched.

Two of these were expensive to find, and both share a shape worth stating up
front: **the grammar generated cleanly and mis-parsed anyway.** Neither the
reserved-keyword nor the angle-bracket problem is visible in the grammar text —
they surface only by parsing real C# and comparing against the language.

### Dead-rule pruning: analysis upstream, deletion in the prep

Tokenizing Roslyn's lexical wrapper rules orphans 84 character-level helpers, and
they must be removed **before** literals are harvested — otherwise their
single-character literals become tokens that shadow `DEC_INT_LIT` and
`IDENTIFIER`, silently breaking every parse while generation stays clean. That
ordering is why the generator's own `--prune-unreachable` cannot do the job alone:
it runs inside codegen, after harvesting, so pruning there still emits the 78 junk
tokens (259 vs 181) and still mis-lexes.

The split is therefore **analysis upstream, edit locally**. The prep no longer
walks the grammar itself; it runs the generator as a reachability query and
deletes exactly the rules reported:

```
antlr4-rust-gen <parser>.g4 --entry-rule compilation_unit
```

`G4S078` is already a dry run — it needs neither `--prune-unreachable` nor a
lexer nor an `--out-dir`, so the probe costs ~0.4 s and writes nothing. Iterated
to a fixpoint, because removing a rule can orphan helpers only it called.

This replaced a hand-rolled walker that scanned `\b[a-z_]\w*\b` over
comment-stripped text. Both produced byte-identical output on this grammar, but
the regex version could not distinguish a rule reference from a word inside an
action, a label, or an argument list — and it did produce a false positive
elsewhere (it wrongly called Kotlin's `script` unreachable). The generator walks
the real AST, so correctness now comes from one implementation rather than an
agreement between two.

### Reserved vs. contextual keywords

Roslyn spells every keyword as an inline literal, and the prep harvests those
literals into named tokens. That is correct for *reserved* keywords and wrong for
*contextual* ones: `var`, `record`, `from`, `get`, `and`, `required`, `_`, … are
ordinary identifiers wherever they have no special meaning, but a harvested token
wins the equal-length lexer match, so `var x = 1;` stopped parsing — `var` was
absent from the expected-token set entirely.

The remedy is the standard ANTLR one, and the same shape `grammars-v4`'s C#
grammar uses: widen `identifier_token` to accept all 42 contextual keywords back.

Two second-order lessons came out of this:

- **The blast radius is much wider than the keyword.** `var` appears in most
  idiomatic modern C#, so one wrong token classification presented as broken
  support for raw strings, ranges, `using` declarations, and unbound generics
  simultaneously. Seven probe failures had one cause.
- **`record_keyword` must *not* use the widened rule.** With
  `record_keyword : {pred}? identifier_token`, `partial struct S { }` predicts
  the record path (`record_keyword` = `partial`), and a predicate cannot prune a
  path ANTLR has already committed to — it surfaces as a hard error. It uses bare
  `IDENTIFIER` instead; `record` always lexes that way, so nothing is lost.

### `out _` and unbounded error recovery

`_` was initially excluded from the widening on the mistaken belief that it still
reached `IDENTIFIER`. It does not, so `F(out _)` could not parse — and the
consequences were far out of proportion to the gap. In `JsonObject.cs` a single
`out _` put the parser into error recovery, and recovery then accumulated
diagnostics without bound: **>4.29 × 10⁹** entries in the runtime's diagnostic
arena, **15.5 GB** peak RSS on a 406-line file, ending in either
`parser.rs:1847 diagnostic sequence arena fits in u32` or a stack overflow
depending on which resource ran out first. Fixing the one token removed all three
corpus crashes.

### Angle brackets vs. shift operators

The harvester minted `>>`, `>>>`, `>>=`, and `>>>=` as single tokens from
`binary_expression`, which made `List<List<int>>` unparseable — the final `>>`
lexed as one right-shift token that `type_argument_list`'s `'>'` can never match.
Roslyn has no such problem because its published grammar encodes no operator
precedence at all (that lives in the hand-written parser).

The prep emits only `'>'` and rebuilds the operators in the *parser* behind
`token_index_adjacent` adjacency predicates, exactly as the vendored C#7 grammar
does. `token_index_adjacent` compares only the last two consumed tokens, so a
three-piece operator carries the predicate at each junction.

### Optional type-body braces (performance)

Roslyn writes every type body as `'{'? member_declaration* '}'?` — both braces
independently optional — because its parser builds a complete declaration node
even for unterminated source. That is right for a node model and pathological for
a parsing grammar: after each member, prediction must weigh "another member"
against "the type ended without a `}`", recursively outward.

| members in one type | as-published | balanced pair |
|---|---|---|
| 32 | 2.28 s | 0.23 s |
| 64 | 6.55 s | 0.21 s |
| 128 | 22.54 s | **0.37 s** (61×) |

Rewriting to `('{' member_declaration* '}')?` keeps what the optionality is for
(a body-less `record R(int X);`) and drops only the half-present case; verified
behaviour-identical on the brace-less forms and on nested types.

### Smaller gaps

- **Accessor bodies.** `accessor_declaration` had no bare `;` alternative, so
  every auto-property (`{ get; set; }`) failed — 128 corpus files.
- **Parameter modifiers.** `ParameterSyntax.Modifiers` is an untyped
  `SyntaxList<SyntaxToken>` with no `<Kind>` children, so the generator emitted
  the declaration-modifier list, which lacks `out`, `in`, `params`, and `this`.
- **Auto-property initializers.** `Syntax.xml` wraps the property body in a
  `<Choice>` of `AccessorList` vs `(ExpressionBody | Initializer) Semicolon`.
  That drives the `SyntaxFactory` overload set, not the parser: `{ get; } = true;`
  is valid C# 6 and appears in 17 corpus files. The one case where the generator
  transcribes the model faithfully and the *model* is stricter than the language.
- **Binary integer literals.** Roslyn's `integer_literal_token` lists only
  decimal and hexadecimal; `0b1010` (C# 7.0) is absent.
- **Char-literal escapes.** A char literal holds exactly one character, so unlike
  a string it has no closure to absorb a mis-sized escape — `'\ud800'` needs the
  escape forms spelled out.
- **Verbatim interpolated strings.** `$@"…"` needs its own lexer mode: a
  backslash is literal there and `""` is the escaped quote, so one text rule
  cannot serve both flavours.

### Known remaining limitation: directive-split expressions

Of the 14 files still reporting errors, the clearest class is a preprocessor
directive splitting a single expression:

```csharp
if (
#if NET9_0
    !dict.TryAdd(propertyName, value)
#else
    !dict.TryAdd(propertyName, value, out int index)
#endif
    )
```

mehen deliberately routes directives to a channel without evaluating them, so
both branches reach the parser and cannot both be one expression. That is the
intended trade: evaluating `#if` would mean picking a symbol set, and metrics for
a subset of the code is worse than approximate metrics for all of it.

### Roslyn's "omitted" syntax nodes

Two rules are genuinely empty productions modelling a blank slot:
`omitted_type_argument` (the unbound generic `Dictionary<,>`) and
`omitted_array_size_expression` (the multi-dimensional `int[,]`). ANTLR forbids
an empty rule inside a closure, so the rules must go — but simply deleting their
alternatives **loses real syntax**: the `','` in
`'[' (expression (',' expression)*)? ']'` then has nothing to match on either
side, and `int[,]` / `Dictionary<,>` stop parsing. The prep instead makes the
list elements optional at the two use sites, which is exactly what an empty node
expressed there. `repro/roslyn-csharp-perf/fixtures/omitted-nodes.cs` is the
regression test; `run.sh` asserts it parses with zero errors.

### The `record` contextual keyword

Roslyn's `Syntax.xml` declares the record keyword as
`<ContextualKind Name="RecordKeyword"/>`, and its grammar generator reads only
`<Kind>` children of a `<Field>`. That is the **only** `<ContextualKind>` in the
whole file (versus 1018 plain `<Kind>`), so it is the single field that hits the
blind spot: the published grammar has **no `'record'` literal at all** and falls
back to the catch-all `syntax_token`, which accepts every identifier, keyword,
literal, operator, and punctuation token.

The cost was severe — `class` became viable as both `class_declaration` and
`record_declaration`, so full-context prediction carried the impossible record
path across every member boundary:

| members in one class | as-published | record restored |
|---|---|---|
| 4 | 188 ms | 27 ms |
| 12 | 2 166 ms | 204 ms |
| 24 | 12 160 ms | **423 ms** |

One real 953-line file (`JsonDocument.Parse.cs`) took **272 s**; the whole
321-file library timed out past 600 s. Restoring the keyword brought that to
~3 m 50 s, and the balanced-brace fix above took it the rest of the way to the
~208 s / 5.1 s-worst-file figures at the top of this section.

The prep restores it as a *contextual* keyword, not a reserved token — `record`
is legal as an ordinary name (`int record = 1;`), and reserving it silently
mis-parses `record R(int X);` as two enum members plus a parenthesized
expression with zero reported errors. `patterns.toml` lowers the restriction to
a pure SemIR comparison, so no typed hook is needed.

Diagnosed by the antlr-rust-runtime team on
[`antlr-rust-runtime#248`](https://github.com/ophi-dev/antlr-rust-runtime/issues/248);
`repro/roslyn-csharp-perf/` at the repo root reproduces both variants.

## Semantic helpers — no hooks anywhere

The vendored grammar is **unmodified** — every repair above is applied by
`prepare-grammar.py` on the way to the derived pair, never by editing
`CSharp.Generated.g4`.

Roslyn's grammar declares no `superClass` and calls no host-language helpers (it
is generated from a syntax model, not hand-written for a parser generator), so
unlike the `grammars-v4` grammar there is no base class to port. The two semantic
surfaces the derived grammar does use are both introduced by the transform, and
both lower to **pure SemIR patterns** in the derived `patterns.toml` — no parser
hook object exists:

- `IsRecordKeyword` — restores the `<ContextualKind>` the upstream generator
  drops, as a `token_text` comparison.
- `IsRightShift` and friends — the angle-bracket adjacency checks
  (`token_index_adjacent`).

The **lexer** is hand-written, because Roslyn publishes none — but its state
lives in the grammar too, in `@lexer::members`, and lowers through the same
`patterns.toml`. A `}` cannot know from the grammar alone whether it closes an
interpolation hole or a nested block:

```csharp
$"a{ new[]{ 1, 2 }.Length }b"
//        ^^^^^^^^^  must NOT end the hole
```

Telling them apart needs a brace depth per open hole and a *conditional* mode pop
— and SemIR has no conditional and no mode-changing action (its seven statements
all touch member state). The grammar therefore splits the `}` into two
predicate-gated rules over the same character, each carrying its own
*unconditional* command, so **rule selection** supplies the condition:

```antlr
INTERP_NESTED_CLOSE : {nestDepth > 0}?       '}' { nestDepth--; }
INTERP_HOLE_CLOSE   : {holeStack.Count > 0}? '}' -> popMode
```

Order is load-bearing in two ways. Between those rules, the lowering DSL has only
`not` and truthiness — no comparisons, no `&&` — so the deeper case must come
first: reaching the second rule already proves `nestDepth == 0`. And both must
precede the unguarded `RBRACE` fallback, which is why `prepare-grammar.py` keeps
`{`, `}` and `:` out of the harvested literals block (`HOLE_SENSITIVE_LITERALS`)
and lets the hand-written file define them.

Stack-valued member state is runtime 0.20.1+, from
[`antlr-rust-runtime#206`](https://github.com/ophi-dev/antlr-rust-runtime/issues/206) —
filed for exactly this grammar shape.

Generation runs with `--sem-unknown error --require-full-semantics`, so any *new*
helper appearing in a future grammar update fails `cargo xtask antlr generate
csharp` instead of silently degrading parse fidelity. `tests/hooks.rs` pins the
observable consequences — nested braces in holes, format clauses, verbatim
backslashes, and nested interpolated strings restoring the enclosing depth.

## Toolchain

| Tool | Version | Why |
|---|---|---|
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.25.0` | Codegen; also computes rule reachability for the transform |
| [`uv`](https://docs.astral.sh/uv/) | any recent | Runs `prepare-grammar.py`; the script's PEP 723 block pins the interpreter |

Unlike the other ANTLR targets, C# needs `uv`: its grammar is derived rather than
directly generatable. `xtask` probes for it and only errors on the C# target, so a
missing `uv` does not block Kotlin/Java regeneration.

Regenerate with:

```bash
cargo install antlr-rust-runtime --version 0.25.0 --features codegen --bin antlr4-rust-gen --force
cargo run -p xtask -- antlr generate csharp
```
