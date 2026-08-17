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
//! (Linked to the repository, not docs.rs: the analyzer crates are
//! `publish = false`, so they have no docs.rs page to link to.)
//!
//! The [`mehen-java`](https://github.com/ophi-dev/mehen/tree/main/crates/mehen-java) analyzer crate depends on
//! this one and walks the resulting [`antlr4_runtime::ParseTree`] to compute
//! metrics.
//!
//! ## Regenerating — never hand-edit
//!
//! The modules are produced by `cargo xtask antlr generate java` from the
//! vendored grammar and checked in verbatim (see `src/generated/README.md`
//! and `grammar/PROVENANCE.md`). `cargo xtask antlr check-generated` guards
//! against drift in CI.
//!
//! ## Semantic predicates — construct with hooks
//!
//! The grammar declares `superClass = JavaParserBase` and calls two of its
//! predicates. [`hooks::JavaParserBase`] is the exact Rust port; install it
//! with `JavaParser::with_typed_hooks` (as the quickstart below does). The
//! modules are generated under `--sem-unknown error`, so a parser built
//! *without* hooks (`JavaParser::new`) fails loud with
//! [`antlr4_runtime::AntlrError::Unsupported`] the moment an input reaches
//! either predicate — it never silently mis-parses.
//!
//! ## Quickstart
//!
//! The hand-rolled lexer/stream/parser setup below is deliberate: the
//! generated one-call drivers (`java_parser::parse_with_parser`, …) always
//! construct the parser hook-less via `JavaParser::new`, so this grammar
//! cannot use them until the entry points accept hooks
//! (<https://github.com/ophi-dev/antlr-rust-runtime/issues/349>).
//!
//! ```no_run
//! use mehen_java_parser::hooks::JavaParserBase;
//! use mehen_java_parser::java_parser::{self, JavaParser};
//! use mehen_java_parser::java_lexer::JavaLexer;
//! use antlr4_runtime::{CommonTokenStream, InputStream};
//! // `number_of_syntax_errors` is a `Parser`-trait method, so the trait
//! // must be in scope to call it.
//! use antlr4_runtime::Parser;
//!
//! # fn main() -> Result<(), antlr4_runtime::AntlrError> {
//! let lexer = JavaLexer::new(InputStream::new("class C {}\n"));
//! let tokens = CommonTokenStream::new(lexer);
//! // `with_typed_hooks` installs the JavaParserBase predicate port.
//! let mut parser = JavaParser::with_typed_hooks(tokens, JavaParserBase);
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
