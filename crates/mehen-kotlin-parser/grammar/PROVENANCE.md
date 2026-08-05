# Kotlin ANTLR grammar — provenance

These `.g4` files are the **source of truth** for the Kotlin analyzer's parser.
They are vendored from upstream with one small local patch (see "Local
patches" below); the generated Rust modules in `../src/generated/` are
produced from them by `cargo xtask antlr generate kotlin`.

## Source

| Field | Value |
|---|---|
| Upstream | [`Kotlin/kotlin-spec`](https://github.com/Kotlin/kotlin-spec) — the official Kotlin language specification grammar |
| Path | `grammar/src/main/antlr/{KotlinLexer,KotlinParser,UnicodeClasses}.g4` |
| Branch | `release` |
| Commit | `2f7aa0524ec27e788dfacd550f144809f2e0254c` |

`KotlinLexer.g4` `import`s `UnicodeClasses`, so all three files must stay together.

## Local patches

These divergences from upstream are intentional and **must be re-applied if
the grammar is re-vendored**. Each is marked with a `MEHEN LOCAL PATCH`
comment in the `.g4` file.

- **`KotlinLexer.g4` — `RCURL` mode pop.** Upstream guards the `}` mode pop
  with a Java embedded action (`{ if (!_modeStack.isEmpty()) { popMode(); } }`),
  which the ANTLR Rust target cannot translate — the generated Rust lexer
  never popped the mode, breaking string-template interpolation
  (`"x ${foo()} y"`). Replaced with the target-portable `-> popMode` lexer
  command (the runtime's `pop_mode()` is a safe no-op on an empty mode stack,
  matching the guarded behavior). The grammar's own comment invites this
  replacement.

## Toolchain

| Tool | Version |
|---|---|
| Rust runtime + codegen | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.29.0` |

## Regenerating

Never hand-edit the files in `../src/generated/`. To regenerate after bumping the
grammar or the runtime:

```bash
cargo xtask antlr generate kotlin
```

That command configures `antlr_rust_codegen::Builder` with the equivalent of:

```rust
Builder::new()
    .grammar("KotlinLexer.g4")
    .grammar("KotlinParser.g4")
    .out_dir("../src/generated")
```

The analyzer selects between the generated `kotlinFile`
(`KotlinParser::kotlin_file()`) and `script` (`KotlinParser::script()`) entry
rules, matching the generated parser's entry-rule documentation.
