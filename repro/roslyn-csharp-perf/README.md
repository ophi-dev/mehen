# Roslyn C# grammar — member-scaling performance repro

Reproduces a ~quadratic-in-members-per-type parse cost on `dotnet/roslyn`'s
published C# grammar, and the fix. Self-contained: no need to re-derive a lexer
or hunt for fixtures.

Context: [`antlr-rust-runtime#248`](https://github.com/ophi-dev/antlr-rust-runtime/issues/248)
(closed — the cause is in the grammar, not the runtime).

## Quick start

```bash
cargo install antlr-rust-runtime --version 0.21.0 \
    --features codegen --bin antlr4-rust-gen --force

./run.sh slow     # as-published: record keyword is the catch-all `syntax_token`
./run.sh fixed    # record contextual keyword restored (default)
```

## Root cause

Roslyn's `Syntax.xml` declares the record keyword as a **contextual** kind:

```xml
<Field Name="Keyword" Type="SyntaxToken" Override="true">
  <ContextualKind Name="RecordKeyword"/>
</Field>
```

Its grammar generator reads only `<Kind>` children of a `<Field>`, never
`<ContextualKind>` — and this is the **only** `<ContextualKind>` in all of
`Syntax.xml` (versus 1018 plain `<Kind>`), so it is the single field that hits
that blind spot. The published grammar therefore contains **no `'record'`
literal at all** and falls back to the catch-all `syntax_token`:

```antlr
record_declaration
  : attribute_list* modifier* syntax_token ('class' | 'struct')? … ;

syntax_token
  : character_literal_token | identifier_token | keyword
  | numeric_literal_token | operator_token | punctuation_token
  | string_literal_token ;
```

Because `syntax_token` accepts `keyword`, a `class` token is viable as **both**
`class_declaration` and `record_declaration`. Full-context prediction carries
that impossible record path across every member boundary, which is what scales
quadratically in members per type.

Credit for this diagnosis goes to the antlr-rust-runtime team on #248. My
original hypothesis — Roslyn's optional body braces — was wrong: requiring the
braces only shortens the ambiguity window and masks the real cause.

## Measured (0.21.0, release build, M-series macOS)

| members | `slow` (as-published) | `fixed` (record restored) |
|---|---|---|
| 4 | 188 ms | 27 ms |
| 8 | 757 ms | 67 ms |
| 12 | 2 166 ms | 204 ms |
| 18 | 5 645 ms | 267 ms |
| 24 | 12 160 ms | **423 ms** |

All inputs are valid C# parsing with **0 recovered syntax errors** in both
variants, and **both keep the body braces optional** — the difference is the
record keyword alone.

On real code (`dotnet/runtime` `System.Text.Json`, 321 files): the whole library
went from **>600 s (timed out)** to **~3 m 50 s**, with the worst single file
dropping from **272 s** to under 6 s. Note that 52 files still exceed 1 s, so
some residual cost remains beyond this fix.

## The fix

`record` is a *contextual* keyword — legal as an ordinary name (`int record = 1;`)
— so it must not become a reserved token. Reserving it silently mis-parses
`record R(int X);` as two enum members plus a parenthesized expression, with zero
reported errors. Instead the declaration position is restricted to an identifier
whose text is `record`:

```antlr
record_keyword
  : {this.IsRecordKeyword()}? identifier_token
  ;
```

`patterns.toml` lowers that predicate to a pure SemIR comparison
(`cmp(eq, token_text(1), str("record"))` → `LookaheadTextEquals`), so **no typed
hook is needed** and the grammar still generates under
`--sem-unknown error --require-full-semantics`.

Verified: `record R(int X);` produces a real `record_declaration`; `record` still
works as a variable, field, and method name; 13/13 modern-C# probes pass.

## What is here

| Path | What it is |
|---|---|
| `grammar/` | The prepared pair plus `patterns.toml`. Roslyn ships a **parser-only** grammar, so the lexer (terminals, comment/directive channels, interpolation and XML-doc modes) is supplied here. |
| `grammar/unnarrowed-record/` | Same, with the record fix reverted — the `slow` control. |
| `fixtures/gen-fixture.py` | Emits a class with N members. The cost scales with members per type, not file length, so a generated fixture reproduces it exactly and avoids vendoring third-party source. |
| `src/bin/time-parse.rs` | Times `compilation_unit` per file; prints ms + recovered-error count. |
| `run.sh` | Generate → build → measure, either variant. |

The grammar is derived by
`crates/mehen-csharp-parser/grammar/prepare-roslyn-grammar.py` from a pinned
upstream revision (`dotnet/roslyn` `76234ec6a1`, 2026-06-24). See that script and
`crates/mehen-csharp-parser/grammar/PROVENANCE.md` for every correction and why.

## Known unrelated gap

Interpolated strings do not parse here: `$"` is harvested as an ordinary literal,
so the `INTERPOLATION` lexer mode is never entered. That is a defect in the
preparation and does not affect these timings (no fixture uses `$"`).
