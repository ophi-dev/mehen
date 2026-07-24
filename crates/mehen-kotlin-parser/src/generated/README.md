# Generated ANTLR modules — DO NOT EDIT

`kotlin_lexer.rs`, `kotlin_parser.rs`, and the `semantics.json` sidecar are
generated from the vendored grammar in `../../grammar/` by
`cargo xtask antlr generate kotlin`. They are checked in (like the tree-sitter
`grammar.rs` kind enums) so a normal `cargo build` never needs
`antlr4-rust-gen`, and all three are drift-checked by `cargo xtask antlr
check-generated`.

Regenerate — never hand-edit — via `cargo xtask antlr generate kotlin`. See
`../../grammar/PROVENANCE.md` for the exact grammar commit and toolchain
versions. `cargo xtask antlr check-generated` guards against drift in CI when
the toolchain is available.
