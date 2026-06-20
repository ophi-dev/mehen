# Generated ANTLR modules — DO NOT EDIT

`kotlin_lexer.rs` and `kotlin_parser.rs` are generated from the vendored
grammar in `../../grammar/` by `cargo xtask antlr generate kotlin`. They are
checked in (like the tree-sitter `grammar.rs` kind enums) so a normal
`cargo build` never needs Java or the ANTLR jar.

Regenerate — never hand-edit — via `cargo xtask antlr generate kotlin`. See
`../../grammar/PROVENANCE.md` for the exact grammar commit and toolchain
versions. `cargo xtask antlr check-generated` guards against drift in CI when
the toolchain is available.
