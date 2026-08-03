//! Time `compilation_unit` over one or more C# files.
//!
//! Usage: time-parse [--require-clean] <file.cs>...
//! Prints one line per file: elapsed ms, recovered syntax-error count, path.
//!
//! `--require-clean` additionally exits 1 if any file reported a recovered error
//! or hard-failed, so `run.sh` can assert a parse regression rather than only
//! printing one. Without it the exit status is 0 whatever the counts, which is
//! what a timing run wants.

use antlr4_runtime::{CommonTokenStream, InputStream, Parser};
use roslyn_csharp_perf::c_sharp_lexer::CSharpLexer;
use roslyn_csharp_perf::c_sharp_parser::CSharpParser;
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let require_clean = args.iter().any(|arg| arg == "--require-clean");
    args.retain(|arg| arg != "--require-clean");
    if args.is_empty() {
        eprintln!("usage: time-parse [--require-clean] <file.cs>...");
        std::process::exit(2);
    }
    let mut unclean = 0usize;
    for path in &args {
        let src = std::fs::read_to_string(path).expect("readable source file");
        let started = Instant::now();
        let lexer = CSharpLexer::new(InputStream::new(&src));
        let mut parser = CSharpParser::new(CommonTokenStream::new(lexer));
        parser.remove_error_listeners();
        let errors = match parser.compilation_unit() {
            Ok(tree) => {
                let n = parser.number_of_syntax_errors();
                let _ = parser.into_parsed_file(tree);
                n
            }
            // A hard failure still costs the time we are measuring.
            Err(_) => usize::MAX,
        };
        let ms = started.elapsed().as_millis();
        if errors != 0 {
            unclean += 1;
        }
        let errors = if errors == usize::MAX {
            "hard-fail".to_string()
        } else {
            errors.to_string()
        };
        println!("{ms:>8} ms  {errors:>9} errs  {path}");
    }
    if require_clean && unclean > 0 {
        eprintln!("error: {unclean} file(s) did not parse cleanly");
        std::process::exit(1);
    }
}
