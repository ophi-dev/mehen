# C# ANTLR grammar — provenance

These `.g4` files are the **source of truth** for the C# analyzer's parser.
They are vendored verbatim from upstream (no local patches — see "Local
patches" below); the generated Rust modules in `../src/generated/` are
produced from them by `cargo run -p xtask -- antlr generate csharp`.

## Source

| Field | Value |
|---|---|
| Upstream | [`antlr/grammars-v4`](https://github.com/antlr/grammars-v4) — the community-maintained ANTLR v4 grammar collection |
| Path | `csharp/v7/{CSharpLexer,CSharpParser}.g4` |
| Branch | `master` |
| Commit | `284602b3f23ca54dc30778204ab7ae9e969145e9` (the revision `ophi-dev/antlr-rust-runtime`'s Rust-vs-Go C# AST parity CI pins) |

The grammar targets C# 7 with later additions from upstream contributors. It
is the same revision the ANTLR Rust runtime validates against the Go target on
real C# corpora (byte-identical ASTs), so parses through this crate carry that
parity evidence.

## Language-version coverage (known limitation)

This grammar is **C# 7-era**, so several mainstream post-7 constructs do not
parse. mehen recovers from parse errors by design — an affected file still
yields metrics, but the constructs below are reported as `csharp.syntax_error`
diagnostics and the metrics around them are approximations.

Measured on 321 files of `dotnet/runtime`'s `System.Text.Json`: 93 parsed
cleanly, 228 produced at least one diagnostic. Verified construct-by-construct:

| Construct | Version | Parses? |
|---|---|---|
| Nullable reference types (`string?`) | C# 8 | ✅ |
| Null-coalescing assignment (`??=`) | C# 8 | ✅ |
| Tuple deconstruction (`var (a, b) = …`) | C# 7 | ✅ |
| File-scoped namespace (`namespace N;`) | C# 10 | ✅ |
| `switch` *expression* (`v switch { … }`) | C# 8 | ❌ |
| `is not` / negated patterns | C# 9 | ❌ |
| Logical + relational patterns (`is int i and > 5`) | C# 9 | ❌ |
| `record` declarations | C# 9 | ❌ |

The dominant failure by far is C# 9 pattern syntax (`is not null`, `and`/`or`,
relational patterns), which appears throughout modern .NET code.

### Why not the `v8-spec` grammar

Upstream also ships `csharp/v8-spec`. It is **not** an easy upgrade and does not
solve the main problem:

- It stops at C# 8, so it does **not** add the C# 9 pattern forms that cause
  most of the failures here.
- Its `superClass` surface is ~35 helpers (vs. this grammar's 14), including a
  **symbol-table-driven scope stack** (`EnterTypeScope`, `ExitCurrentScope`,
  `IsClassTypeName`, `IsDelegateTypeName`, `IsTypeParameterName`, …). Those
  predicates resolve identifiers against declarations seen so far, so porting
  them means implementing a semantic model, not lookahead checks — and mehen's
  `--sem-unknown error` policy means every one must be implemented exactly or
  generation fails.

### Status of the Roslyn-grammar migration

`prepare-roslyn-grammar.py` (this directory) derives a generatable grammar pair
from [`dotnet/roslyn`'s `CSharp.Generated.g4`](https://github.com/dotnet/roslyn/blob/main/src/Compilers/CSharp/Portable/Generated/CSharp.Generated.g4).
Since runtime 0.21.0 accepted mutual left recursion (upstream #221), that path
works: generation is clean, the parser compiles, and **13/13** modern-C# probes
parse. On the 321-file `System.Text.Json` corpus it reaches **109 clean vs 93**
for this C#7 grammar.

It is not yet wired in. Two things remain, both in the preparation, not the
runtime:

1. **Interpolated strings.** The prep's `INTERPOLATION` lexer mode is never
   entered — `$"` is harvested as an ordinary literal instead of a
   mode-pushing token — so any file containing `$"…{expr}…"` fails. This is the
   main cause of the remaining 212 error files.

2. **Optional body braces cost O(n²).** Roslyn spells every type body as
   `'{'? member_declaration* '}'?` (optional, for error recovery), which makes
   member boundaries ambiguous. Measured on a synthetic class:

   | members | as-published | braces required |
   |---|---|---|
   | 4 | 0.28 s | 0.05 s |
   | 8 | 0.92 s | 0.05 s |
   | 12 | 2.29 s | 0.06 s |
   | 18 | 6.54 s | 0.07 s |

   One 700-line real file (`JsonDocument.Parse.cs`) took **272 s**. Requiring
   the braces makes it flat (~93× faster at 18 members) and takes the whole
   corpus from >600 s to **61 s**, with all 13 probes still passing. Whether to
   apply that patch is a deliberate trade: it gives up Roslyn's error-recovery
   tolerance for malformed bodies, which a metrics tool arguably wants to keep.

Note also that this grammar is **permissive by design** — it models Roslyn's
syntax nodes (including error-recovery nodes such as `incomplete_member`), not
the exact accepted language — and it encodes **no operator precedence**, since
real C# precedence lives in Roslyn's hand-written parser. Neither matters for
mehen's token-level metrics, but both would matter for a validating parser.

## Local patches

**None.** The grammar is vendored unmodified. Its `superClass` helpers are
routed to exact implementations rather than dropped or patched out:

- `CSharpLexer.g4` declares `options { superClass = CSharpLexerBase; }` and
  calls ten stateful helpers: interpolated-string bookkeeping
  (`OnInterpolatedRegularStringStart`, `OnOpenBrace`, `OnColon`, …) and two
  predicates (`IsRegularCharInside`, `IsVerbatiumDoubleQuoteInside`).
  `patterns.toml` lowers all of them to **typed hooks**, and
  `../src/hooks.rs` ports the reference base class exactly
  (`CSharpLexerBase`, installed via `CSharpLexer::with_typed_hooks`). The
  port is taken from `tools/parse-bench/rust-support/csharp_lexer_base.rs`
  in [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime),
  where the runtime's CI validates it byte-identical against the Go
  `CSharpLexerBase.go` on real corpora. Beyond the grammar actions, the
  hooks implement the preprocessor state machine (`#define`/`#if`/`#elif`
  evaluation and inactive-section skipping via the lexer lifecycle
  callbacks), matching the Go/Java targets.
- `CSharpParser.g4` declares `options { superClass = CSharpParserBase; }`
  and calls four pure predicates (`IsLocalVariableDeclaration`,
  `IsRightArrow`, `IsRightShift`, `IsRightShiftAssignment`). `patterns.toml`
  lowers these to **inline SemIR patterns** (`ctx_rule_text` comparison and
  token-adjacency checks) — the parser needs no hook object.

Generation runs with `--sem-unknown error --require-full-semantics`, so any
*new* helper appearing in a future grammar update fails
`cargo xtask antlr generate csharp` instead of silently degrading parse
fidelity; the same policy makes a hook-less lexer (`CSharpLexer::new`) fail
loud rather than mis-lex.

## Toolchain

| Tool | Version |
|---|---|
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.21.0` |

Regenerate with:

```bash
cargo install antlr-rust-runtime --version 0.21.0 --features codegen --bin antlr4-rust-gen --force
cargo run -p xtask -- antlr generate csharp
```
