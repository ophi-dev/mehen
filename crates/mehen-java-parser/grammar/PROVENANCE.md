# Java ANTLR grammar — provenance

These `.g4` files are the **source of truth** for the Java analyzer's parser.
They are vendored verbatim from upstream (no local patches — see "Local
patches" below); the generated Rust modules in `../src/generated/` are
produced from them by `cargo run -p xtask -- antlr generate java`.

## Source

| Field | Value |
|---|---|
| Upstream | [`antlr/grammars-v4`](https://github.com/antlr/grammars-v4) — the community-maintained ANTLR v4 grammar collection |
| Path | `java/java/{JavaLexer,JavaParser}.g4` |
| Branch | `master` |
| Commit | `37146747969be81255787b80d476873ec24d2626` (last change to `java/java/JavaParser.g4`, 2025-08-31) |

`JavaLexer.g4` and `JavaParser.g4` are self-contained — neither `import`s
another grammar (unlike the Kotlin grammar's `UnicodeClasses`), so no extra
`.g4` files are vendored.

## Local patches

**None.** Unlike the Kotlin grammar (which needs a `RCURL` mode-pop patch),
the Java grammar is vendored unmodified. Its two Java-target semantic
predicates are routed to hand-written hooks, not dropped and not patched
out of the grammar:

- `JavaParser.g4` declares `options { superClass = JavaParserBase; }` and uses
  two host-language (Java) semantic predicates:
  - `{ this.IsNotIdentifierAssign() }?` (annotation `key = value`
    disambiguation), and
  - `{ this.DoLastRecordComponent() }?` (varargs record component must be
    last).

  `patterns.toml` (this directory) lowers both helper calls to **typed
  hooks**, and `../src/hooks.rs` ports the upstream
  `Java/JavaParserBase.java` semantics exactly (`JavaParserBase`, installed
  via `JavaParser::with_typed_hooks`). Generation runs with `--sem-unknown
  error --require-full-semantics`, so any *new* helper appearing in a future
  grammar update fails `cargo xtask antlr generate java` instead of silently
  degrading parse fidelity; the same policy makes a hook-less parser
  (`JavaParser::new`) fail loud at the first hooked predicate rather than
  mis-parse. The upstream `Java/JavaParserBase.java` file itself is **not**
  vendored — it is Java-only; `src/hooks.rs` is its Rust counterpart.

  The `precpred(...)` calls in the generated parser are *precedence*
  predicates for left-recursive expression rules; the runtime evaluates those
  automatically and they are unrelated to the two hooked predicates.

## Toolchain

| Tool | Version |
|---|---|
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.18.0` |

## Regenerating

Never hand-edit the files in `../src/generated/`. To regenerate after bumping
the grammar or the runtime:

```bash
cargo install antlr-rust-runtime --version 0.18.0 --features codegen --bin antlr4-rust-gen --force
cargo run -p xtask -- antlr generate java
```

That command runs:

```bash
antlr4-rust-gen JavaLexer.g4 JavaParser.g4 --out-dir ../src/generated
```

The analyzer parses via the generated `compilationUnit`
(`JavaParser::compilation_unit()`) entry rule.
