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
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.18.0` |

Regenerate with:

```bash
cargo install antlr-rust-runtime --version 0.18.0 --features codegen --bin antlr4-rust-gen --force
cargo run -p xtask -- antlr generate csharp
```
