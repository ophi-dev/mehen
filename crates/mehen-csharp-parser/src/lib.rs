// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-csharp-parser` — ANTLR-generated C# lexer and parser.
//!
//! This crate holds the machine-generated C# lexer/parser produced from the
//! community-maintained grammars-v4 C# grammar (`antlr/grammars-v4`, vendored
//! in `grammar/`) running on the [`antlr4_runtime`] Rust runtime, plus the
//! hand-written [`hooks::CSharpLexerBase`] state machine the grammar's
//! `superClass` requires. It carries no mehen-specific logic and no
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
//! from the vendored grammar and checked in verbatim (see
//! `src/generated/README.md` and `grammar/PROVENANCE.md`). `cargo xtask antlr
//! check-generated` guards against drift in CI when the toolchain is
//! available. (`src/hooks.rs` is hand-written and excluded from that rule.)
//!
//! ## Semantic helpers — construct the lexer with hooks
//!
//! The grammar's `CSharpLexerBase` is stateful (interpolated strings,
//! preprocessor directives); [`hooks::CSharpLexerBase`] is the exact Rust
//! port and **must** be installed with `CSharpLexer::with_typed_hooks` (as
//! the quickstart below does). The modules are generated under
//! `--sem-unknown error`, so a hook-less lexer fails loud rather than
//! mis-lexing. The parser side needs no hooks: `CSharpParserBase`'s four
//! predicates lower to inline patterns (see `grammar/patterns.toml`).
//!
//! ## Quickstart
//!
//! ```no_run
//! use mehen_csharp_parser::hooks::CSharpLexerBase;
//! use mehen_csharp_parser::c_sharp_parser::CSharpParser;
//! use mehen_csharp_parser::c_sharp_lexer::CSharpLexer;
//! use antlr4_runtime::{CommonTokenStream, InputStream};
//! // `number_of_syntax_errors` is a `Parser`-trait method, so the trait
//! // must be in scope to call it.
//! use antlr4_runtime::Parser;
//!
//! # fn main() -> Result<(), antlr4_runtime::AntlrError> {
//! let input = InputStream::new("class C {}\n");
//! // `with_typed_hooks` installs the CSharpLexerBase state machine.
//! let lexer = CSharpLexer::with_typed_hooks(input, CSharpLexerBase::default());
//! let tokens = CommonTokenStream::new(lexer);
//! let mut parser = CSharpParser::new(tokens);
//! let result = parser.compilation_unit()?;
//! let errors = parser.number_of_syntax_errors();
//! let parsed = parser.into_parsed_file(result);
//! let _ = (errors, parsed.tree());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

/// Re-export of the ANTLR v4 Rust runtime the generated modules were built
/// against, so downstream crates can name the runtime types (`ParseTree`,
/// `Node`, `TokenView`, …) without pinning the runtime version themselves.
pub use antlr4_runtime;

pub mod hooks;

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
