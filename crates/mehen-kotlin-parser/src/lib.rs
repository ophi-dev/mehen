// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-kotlin-parser` — ANTLR-generated Kotlin lexer and parser.
//!
//! This crate holds **only** the machine-generated Kotlin lexer/parser
//! produced from the official Kotlin specification ANTLR grammar
//! (`Kotlin/kotlin-spec`, vendored in `grammar/`) running on the
//! [`antlr4_runtime`] Rust runtime. It carries no mehen-specific logic and
//! no dependency on `mehen-core`, so it can be consumed on its own — e.g.
//! `mehen-kotlin-parser = { git = "https://github.com/ophi-dev/mehen", tag = "…" }`
//! — the same way this repo consumes the ruff/oxc/sqruff parser crates.
//!
//! (Linked to the repository, not docs.rs: the analyzer crates are
//! `publish = false`, so they have no docs.rs page to link to.)
//!
//! The [`mehen-kotlin`](https://github.com/ophi-dev/mehen/tree/main/crates/mehen-kotlin) analyzer crate depends
//! on this one and walks the resulting [`antlr4_runtime::ParseTree`] to
//! compute metrics.
//!
//! ## Regenerating — never hand-edit
//!
//! The modules are produced by `cargo xtask antlr generate kotlin` from the
//! vendored grammar and checked in verbatim (see `src/generated/README.md`
//! and `grammar/PROVENANCE.md`). `cargo xtask antlr check-generated` guards
//! against drift in CI when the toolchain is available.
//!
//! ## Quickstart
//!
//! ```no_run
//! use mehen_kotlin_parser::kotlin_parser::{self, KotlinParser};
//! use mehen_kotlin_parser::kotlin_lexer::KotlinLexer;
//! // `number_of_syntax_errors` is a `Parser`-trait method, so the trait
//! // must be in scope to call it.
//! use antlr4_runtime::Parser;
//!
//! # fn main() -> Result<(), antlr4_runtime::AntlrError> {
//! // One-call setup: build lexer + token stream + parser and run an entry
//! // rule. `parse_with_parser` keeps the parser so you can read diagnostics.
//! let out = kotlin_parser::parse_with_parser(
//!     "fun main() {}\n",
//!     KotlinLexer::new,
//!     KotlinParser::kotlin_file,
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

/// ANTLR-generated Kotlin lexer.
///
/// Regenerate with `cargo xtask antlr generate kotlin` — never hand-edit.
#[path = "generated/kotlin_lexer.rs"]
pub mod kotlin_lexer;

/// ANTLR-generated Kotlin parser.
///
/// Regenerate with `cargo xtask antlr generate kotlin` — never hand-edit.
#[path = "generated/kotlin_parser.rs"]
pub mod kotlin_parser;
