// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-csharp-parser` — ANTLR-generated C# lexer and parser.
//!
//! This crate holds the machine-generated C# lexer/parser derived from
//! [Roslyn's own published grammar][roslyn] (`CSharp.Generated.g4`, vendored in
//! `grammar/`) running on the [`antlr4_runtime`] Rust runtime. It carries no
//! hand-written Rust beyond this module docs, no mehen-specific logic, and no
//! dependency on `mehen-core`, so it can be consumed on its own — e.g.
//! `mehen-csharp-parser = { git = "https://github.com/ophi-dev/mehen", tag = "…" }`
//! — the same way this repo consumes the ruff/oxc/sqruff parser crates.
//!
//! The [`mehen-csharp`](https://docs.rs/mehen-csharp) analyzer crate depends
//! on this one and walks the resulting [`antlr4_runtime::ParseTree`] to
//! compute metrics.
//!
//! ## Regenerating — never hand-edit
//!
//! The generated modules are produced by `cargo xtask antlr generate csharp`
//! and checked in verbatim (see `src/generated/README.md` and
//! `grammar/PROVENANCE.md`). `cargo xtask antlr check-generated` guards against
//! drift in CI when the toolchain is available.
//!
//! Generation has an extra step here: Roslyn publishes a *reference* grammar
//! that ANTLR rejects as-is, so `grammar/prepare-grammar.py` derives a
//! generatable lexer/parser pair from it first. Those derived `.g4` files are
//! build artifacts (gitignored); the vendored `CSharp.Generated.g4` is the
//! source of truth. The prep needs [`uv`](https://docs.astral.sh/uv/) in
//! addition to `antlr4-rust-gen`.
//!
//! ## No hooks
//!
//! Neither recognizer needs a hand-written hook object: every semantic
//! coordinate lowers to pure SemIR through the derived `patterns.toml`.
//!
//! That includes the awkward one. Interpolated strings need their own lexer
//! modes, and the `}` closing a hole is lexically identical to the one closing a
//! nested block — so the decision needs a brace depth per hole and a
//! *conditional* mode pop, which SemIR has no action for. The grammar instead
//! keeps the depth in `@lexer::members` and splits the `}` into two
//! predicate-gated rules, ordered so that rule selection supplies the condition
//! and each alternative carries an unconditional command. See
//! `grammar/lexer-tokens.g4.in`.
//!
//! ## Quickstart
//!
//! ```no_run
//! use mehen_csharp_parser::c_sharp_parser::CSharpParser;
//! use mehen_csharp_parser::c_sharp_lexer::CSharpLexer;
//! use antlr4_runtime::{CommonTokenStream, InputStream};
//! // `number_of_syntax_errors` is a `Parser`-trait method, so the trait
//! // must be in scope to call it.
//! use antlr4_runtime::Parser;
//!
//! # fn main() -> Result<(), antlr4_runtime::AntlrError> {
//! let input = InputStream::new("class C {}\n");
//! let lexer = CSharpLexer::new(input);
//! let tokens = CommonTokenStream::new(lexer);
//! let mut parser = CSharpParser::new(tokens);
//! let result = parser.compilation_unit()?;
//! let errors = parser.number_of_syntax_errors();
//! let parsed = parser.into_parsed_file(result);
//! let _ = (errors, parsed.tree());
//! # Ok(())
//! # }
//! ```
//!
//! [roslyn]: https://github.com/dotnet/roslyn/blob/main/src/Compilers/CSharp/Portable/Generated/CSharp.Generated.g4

#![forbid(unsafe_code)]

/// Re-export of the ANTLR v4 Rust runtime the generated modules were built
/// against, so downstream crates can name the runtime types (`ParseTree`,
/// `Node`, `TokenView`, …) without pinning the runtime version themselves.
pub use antlr4_runtime;

/// ANTLR-generated C# lexer.
///
/// Regenerate with `cargo xtask antlr generate csharp` — never hand-edit.
#[path = "generated/c_sharp_lexer.rs"]
pub mod c_sharp_lexer;

/// ANTLR-generated C# parser.
///
/// Regenerate with `cargo xtask antlr generate csharp` — never hand-edit.
#[path = "generated/c_sharp_parser.rs"]
pub mod c_sharp_parser;
