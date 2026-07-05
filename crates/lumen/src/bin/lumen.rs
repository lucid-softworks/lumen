//! Minimal shell: evaluate JS files in one engine, printing console output as it appears.
//! Usage: lumen <file.js> [more.js ...]

use lumen::{Completion, Engine};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: lumen <file.js> [more.js ...]");
        std::process::exit(2);
    }
    let mut engine = Engine::new();
    for path in &args {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                std::process::exit(2);
            }
        };
        let result = engine.eval(&src, false);
        for line in engine.take_console() {
            println!("{line}");
        }
        match result {
            Ok(Completion::Value(_)) => {}
            Ok(Completion::Throw { name, message }) => {
                eprintln!("uncaught {name}: {message}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("SyntaxError in {path}: {}", e.message);
                std::process::exit(1);
            }
        }
    }
}
