# Generated ANTLR modules — DO NOT EDIT

`java_lexer.rs` and `java_parser.rs` are generated from the vendored
grammar in `../../grammar/` by `cargo run -p xtask -- antlr generate java`.
They are checked in (like the tree-sitter `grammar.rs` kind enums) so a
normal `cargo build` never needs Java or the ANTLR jar.

Regenerate — never hand-edit — via `cargo run -p xtask -- antlr generate java`.
See `../../grammar/PROVENANCE.md` for the exact grammar commit and toolchain
versions. `cargo run -p xtask -- antlr check-generated` guards against drift in
CI when the toolchain is available.
