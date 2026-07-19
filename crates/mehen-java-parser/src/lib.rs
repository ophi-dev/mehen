// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-java-parser` — ANTLR-generated Java lexer and parser.
//!
//! This crate holds **only** the machine-generated Java lexer/parser produced
//! from the community-maintained grammars-v4 Java grammar
//! (`antlr/grammars-v4`, vendored in `grammar/`) running on the
//! [`antlr4_runtime`] Rust runtime. It carries no mehen-specific logic and no
//! dependency on `mehen-core`, so it can be consumed on its own — e.g.
//! `mehen-java-parser = { git = "https://github.com/ophi-dev/mehen", tag = "…" }`
//! — the same way this repo consumes the ruff/oxc/sqruff parser crates.
//!
//! The [`mehen-java`](https://docs.rs/mehen-java) analyzer crate depends on
//! this one and walks the resulting [`antlr4_runtime::ParseTree`] to compute
//! metrics.
//!
//! ## Regenerating — never hand-edit
//!
//! The modules are produced by `cargo xtask antlr generate java` from the
//! vendored grammar and checked in verbatim (see `src/generated/README.md`
//! and `grammar/PROVENANCE.md`). `cargo xtask antlr check-generated` guards
//! against drift in CI when the toolchain is available.
//!
//! ## Quickstart
//!
//! ```no_run
//! use mehen_java_parser::java_parser::{self, JavaParser};
//! use mehen_java_parser::java_lexer::JavaLexer;
//! // `number_of_syntax_errors` is a `Parser`-trait method, so the trait
//! // must be in scope to call it.
//! use antlr4_runtime::Parser;
//!
//! # fn main() -> Result<(), antlr4_runtime::AntlrError> {
//! // One-call setup: build lexer + token stream + parser and run an entry
//! // rule. `parse_with_parser` keeps the parser so you can read diagnostics.
//! let out = java_parser::parse_with_parser(
//!     "class C {}\n",
//!     JavaLexer::new,
//!     JavaParser::compilation_unit,
//! )?;
//! let errors = out.parser.number_of_syntax_errors();
//! let parsed = out.parser.into_parsed_file(out.result);
//! let _ = (errors, parsed.tree());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

/// Re-export of the ANTLR v4 Rust runtime the generated modules were built
/// against, so downstream crates can name the runtime types (`ParseTree`,
/// `Node`, `TokenView`, …) without pinning the runtime version themselves.
pub use antlr4_runtime;

/// ANTLR-generated Java lexer.
///
/// Regenerate with `cargo xtask antlr generate java` — never hand-edit.
#[path = "generated/java_lexer.rs"]
pub mod java_lexer;

/// ANTLR-generated Java parser.
///
/// Regenerate with `cargo xtask antlr generate java` — never hand-edit.
#[path = "generated/java_parser.rs"]
pub mod java_parser;
