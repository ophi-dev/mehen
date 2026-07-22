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
predicates are handled by the generator, not by editing the grammar:

- `JavaParser.g4` declares `options { superClass = JavaParserBase; }` and uses
  two host-language (Java) semantic predicates:
  - `{ this.IsNotIdentifierAssign() }?` (annotation `key = value`
    disambiguation), and
  - `{ this.DoLastRecordComponent() }?` (varargs record component must be
    last).

  These embed Java code and cannot be translated to the Rust target. The
  `antlr4-rust-gen` generator **drops action-backed semantic predicates**
  (it emits an empty `PARSER_PREDICATES` table), so the generated Rust parser
  needs **no `JavaParserBase` superclass**. ANTLR semantic predicates only
  ever *reject* candidate parses, so dropping them widens what the grammar
  accepts — harmless for a metrics tool, which recovers from parse errors by
  design. The upstream `Java/JavaParserBase.java` helper is therefore **not**
  vendored: it is Java-only and unused by the Rust target.

  The `precpred(...)` calls that remain in the generated parser are
  *precedence* predicates for left-recursive expression rules; the runtime
  evaluates those automatically and they are unrelated to the dropped
  action-backed predicates.

## Toolchain

| Tool | Version |
|---|---|
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `745cf7ff69fc9edca42cd4e121f2d69d5e490a43` |

## Regenerating

Never hand-edit the files in `../src/generated/`. To regenerate after bumping
the grammar or the runtime:

```bash
cargo install antlr-rust-runtime --features codegen --bin antlr4-rust-gen --force
cargo run -p xtask -- antlr generate java
```

That command runs:

```bash
antlr4-rust-gen JavaLexer.g4 JavaParser.g4 --out-dir ../src/generated
```

The analyzer parses via the generated `compilationUnit`
(`JavaParser::compilation_unit()`) entry rule.
