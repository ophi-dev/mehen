//! Generated C# lexer/parser for the Roslyn-grammar performance repro.
//!
//! `src/generated/` is produced by `./run.sh`; it is not checked in (a 4.6 MB
//! parser), so build via that script rather than `cargo build` directly.
pub use antlr4_runtime;

#[path = "generated/c_sharp_lexer.rs"]
pub mod c_sharp_lexer;

#[path = "generated/c_sharp_parser.rs"]
pub mod c_sharp_parser;
