# Generated ANTLR modules — DO NOT EDIT

`c_sharp_lexer.rs`, `c_sharp_parser.rs`, `decisions.json`, and `semantics.json`
are generated from the vendored grammar in `../../grammar/` by
`cargo xtask antlr generate csharp`. They are checked in (like the tree-sitter
`grammar.rs` kind enums), so a normal `cargo build` uses them without compiling
xtask's `antlr-rust-codegen` dependency. All four artifacts are drift-checked by
`cargo xtask antlr check-generated`.

Regenerate — never hand-edit — via `cargo xtask antlr generate csharp`. See
`../../grammar/PROVENANCE.md` for the exact grammar commit and runtime/codegen
versions. `cargo xtask antlr check-generated` guards against drift in CI.

(This crate has no hand-written recognizer support at all: every semantic
coordinate lowers to pure SemIR through the derived `patterns.toml`, so there is
no `hooks.rs` and both recognizers are constructed with plain `::new`.)
