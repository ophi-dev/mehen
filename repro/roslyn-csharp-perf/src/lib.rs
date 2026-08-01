//! Generated C# lexer/parser for the Roslyn-grammar performance repro.
//!
//! `src/generated/` is produced by `./run.sh`; it is not checked in (a 4.6 MB
//! parser), so build via that script rather than `cargo build` directly.
//!
//! Everything here is `pub` on purpose. `src/bin/time-parse.rs` is a separate crate
//! from this library, so a private facade could not reach the generated modules at
//! all — narrowing the surface would mean re-exporting the same names one level in,
//! which is the same surface with an extra hop. This crate is `publish = false` and
//! exists only to be measured by its own binary; there is no downstream consumer for
//! the surface to matter to. (The real parser crate, `mehen-csharp-parser`, is the
//! one with a public API worth curating.)
pub use antlr4_runtime;

#[path = "generated/c_sharp_lexer.rs"]
pub mod c_sharp_lexer;

#[path = "generated/c_sharp_parser.rs"]
pub mod c_sharp_parser;
