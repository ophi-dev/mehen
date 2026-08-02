# Generated ANTLR modules — DO NOT EDIT

`c_sharp_lexer.rs`, `c_sharp_parser.rs`, and the `semantics.json` sidecar are
generated from the vendored grammar in `../../grammar/` by
`cargo run -p xtask -- antlr generate csharp`. They are checked in (like the
tree-sitter `grammar.rs` kind enums) so a normal `cargo build` never needs
`antlr4-rust-gen`, and all three are drift-checked by
`cargo run -p xtask -- antlr check-generated`.

Regenerate — never hand-edit — via `cargo run -p xtask -- antlr generate csharp`.
See `../../grammar/PROVENANCE.md` for the exact grammar commit and toolchain
versions. `cargo run -p xtask -- antlr check-generated` guards against drift in
CI when the toolchain is available.

(This crate has no hand-written recognizer support at all: every semantic
coordinate lowers to pure SemIR through the derived `patterns.toml`, so there is
no `hooks.rs` and both recognizers are constructed with plain `::new`.)
