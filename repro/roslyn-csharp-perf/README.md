# Roslyn C# grammar — member-scaling performance repro

Self-contained reproduction for
[`antlr-rust-runtime#248`](https://github.com/ophi-dev/antlr-rust-runtime/issues/248)
on `dotnet/roslyn`'s published C# grammar. Everything needed is here — no need
to re-derive a lexer or hunt for fixtures.

## Quick start

```bash
cargo install antlr-rust-runtime --version 0.21.0 \
    --features codegen --bin antlr4-rust-gen --force

./run.sh                 # as-published grammar  -> quadratic
./run.sh braces          # body braces required  -> flat (control)
```

Each run generates the parser, builds a timing harness, and prints elapsed
milliseconds for synthetic classes of 4/8/12/18/24 members.

## Measured on this repro (0.21.0, release build, M-series macOS)

| members | as-published | braces required |
|---|---|---|
| 4 | 398 ms | 10 ms |
| 8 | 1 095 ms | 15 ms |
| 12 | 2 371 ms | 68 ms |
| 18 | 6 193 ms | 29 ms |
| 24 | 13 543 ms | 38 ms |

Every input is **valid C#** and parses with **0 recovered syntax errors** in
both variants — this is not error-recovery cost. Growth is ~quadratic in
*members per type*, and flat once the braces are required.

On real code, `System.Text.Json`'s `JsonDocument.Parse.cs` (953 lines) takes
**~272 s** as-published; the whole 321-file library goes from >600 s to ~61 s
with braces required.

## What is here

| Path | What it is |
|---|---|
| `grammar/CSharpLexer.g4` | Hand-written lexer. Roslyn ships a **parser-only** grammar (terminals are inline literals plus character-level parser rules), so this supplies the terminals, comment/directive channels, and the interpolation / XML-doc modes. |
| `grammar/CSharpParser.g4` | Roslyn's grammar, mechanically prepared: 6 empty rules corrected, lexical wrappers pointed at tokens, orphaned character-level helpers pruned, 3 nullable closure members tightened. |
| `grammar/braces-required/` | Same pair, with `'{'? member_declaration* '}'?` changed to required braces in the 6 type-body sites. The control. |
| `fixtures/gen-fixture.py` | Emits a class with N members. The blow-up scales with members per type, not file length, so a generated fixture reproduces it exactly (and avoids vendoring third-party source). |
| `src/bin/time-parse.rs` | Times `compilation_unit` per file; prints ms + recovered-error count. |
| `run.sh` | Generate → build → measure, for either variant. |

## How the grammar was prepared

`crates/mehen-csharp-parser/grammar/prepare-roslyn-grammar.py` in this repo
derives `grammar/CSharpParser.g4` from a pinned upstream revision
(`dotnet/roslyn` `76234ec6a1`, 2026-06-24). Re-run it to refresh from a newer
Roslyn; see that script's docstring and
`crates/mehen-csharp-parser/grammar/PROVENANCE.md` for the rationale behind
each correction.

## Suspected cause

Roslyn spells every type body with **optional** braces, for error recovery:

```antlr
class_declaration
  : attribute_list* modifier* 'class' identifier_token …
    '{'? member_declaration* '}'? ';'?
  ;
```

With the closing brace optional and `member_declaration*` a closure, the
parser appears to consider O(n) candidate end-points for the body at each of
n members. The `braces-required` variant removes exactly that ambiguity and
the cost collapses, which is the evidence for this reading — but it is a
hypothesis about *why*, not a verified account of the prediction path.

## Known unrelated gap

Interpolated strings do not parse in this repro: the `INTERPOLATION` lexer mode
is never entered, because `$"` is harvested as an ordinary literal rather than a
mode-pushing token. That is a defect in the preparation, not a runtime issue,
and it does not affect the timings above (none of the fixtures use `$"`).
