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
| ANTLR tool jar | `antlr-4.13.2-complete.jar` (from <https://www.antlr.org/download/>) |
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.5.0` (commit `ba2c065c26ba7cdd7cb1ec9e0011484f76ec31be`) |

## Regenerating

Never hand-edit the files in `../src/generated/`. To regenerate after bumping the
grammar or the runtime:

```bash
cargo xtask antlr generate kotlin
```

That command drives the same pipeline this directory was produced with:

1. `java -jar antlr-4.13.2-complete.jar -o <interp-dir> -Xexact-output-dir KotlinLexer.g4 KotlinParser.g4`
   → `<interp-dir>/KotlinLexer.interp` + `<interp-dir>/KotlinParser.interp`
2. `antlr4-rust-gen --lexer <interp-dir>/KotlinLexer.interp --parser <interp-dir>/KotlinParser.interp --out-dir ../src/generated`

The analyzer selects between the generated `kotlinFile`
(`KotlinParser::kotlin_file()`) and `script` (`KotlinParser::script()`) entry
rules, matching the generated parser's v0.5.0 entry-rule documentation.
