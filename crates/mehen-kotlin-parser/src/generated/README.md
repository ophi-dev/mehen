# Generated ANTLR modules — DO NOT EDIT

`kotlin_lexer.rs`, `kotlin_parser.rs`, `decisions.json`, and `semantics.json`
are generated from the vendored grammar in `../../grammar/` by
`cargo xtask antlr generate kotlin`. They are checked in (like the tree-sitter
`grammar.rs` kind enums), so a normal `cargo build` uses them without compiling
xtask's `antlr-rust-codegen` dependency. All four artifacts are drift-checked by
`cargo xtask antlr check-generated`.

Regenerate — never hand-edit — via `cargo xtask antlr generate kotlin`. See
`../../grammar/PROVENANCE.md` for the exact grammar commit and runtime/codegen
versions. `cargo xtask antlr check-generated` guards against drift in CI.
