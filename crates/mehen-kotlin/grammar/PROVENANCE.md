# Kotlin ANTLR grammar — provenance

These `.g4` files are the **source of truth** for the Kotlin analyzer's parser.
They are vendored verbatim; the generated Rust modules in `../src/generated/`
are produced from them by `cargo xtask antlr generate kotlin`.

## Source

| Field | Value |
|---|---|
| Upstream | [`Kotlin/kotlin-spec`](https://github.com/Kotlin/kotlin-spec) — the official Kotlin language specification grammar |
| Path | `grammar/src/main/antlr/{KotlinLexer,KotlinParser,UnicodeClasses}.g4` |
| Branch | `release` |
| Commit | `2f7aa0524ec27e788dfacd550f144809f2e0254c` |

`KotlinLexer.g4` `import`s `UnicodeClasses`, so all three files must stay together.

## Toolchain

| Tool | Version |
|---|---|
| ANTLR tool jar | `antlr-4.13.2-complete.jar` (from <https://www.antlr.org/download/>) |
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.4.0` (commit `6bf139715dceffdee505a3ac64ebc8d2ad0868cd`) |

## Regenerating

Never hand-edit the files in `../src/generated/`. To regenerate after bumping the
grammar or the runtime:

```bash
cargo xtask antlr generate kotlin
```

That command drives the same pipeline this directory was produced with:

1. `java -jar antlr-4.13.2-complete.jar -Xexact-output-dir KotlinLexer.g4 KotlinParser.g4`
   → `KotlinLexer.interp` + `KotlinParser.interp`
2. `antlr4-rust-gen --lexer KotlinLexer.interp --parser KotlinParser.interp --out-dir ../src/generated`

The entry rule is `kotlinFile` (`KotlinParser::kotlin_file()`).
