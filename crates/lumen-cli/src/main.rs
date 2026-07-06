//! lumen-cli — run JS on the lumen runtime, node/deno style.
//!
//! Usage:
//!   lumen-cli                          repl when stdin is a terminal, else eval stdin
//!   lumen-cli repl                     repl explicitly (even when piped — for scripting it)
//!   lumen-cli <file.js> [args...]      run a script to loop quiescence
//!   lumen-cli -e '<code>'              evaluate a string
//!   --tier=interp|bytecode, --tier-threshold=N   select the engine execution tier

use std::io::{IsTerminal, Read};

use lumen_host::Completion;
use lumen_repl::Repl;
use lumen_runtime::Runtime;

fn main() {
    let mut tier = None;
    let mut threshold = None;
    let mut eval_source = None;
    let mut file = None;
    let mut force_repl = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if let Some(t) = a.strip_prefix("--tier=") {
            tier = Some(match t {
                "bytecode" => lumen_host::Tier::Bytecode,
                "interp" => lumen_host::Tier::Interp,
                other => die(2, &format!("unknown tier '{other}' (interp|bytecode)")),
            });
        } else if let Some(n) = a.strip_prefix("--tier-threshold=") {
            threshold = n.parse::<u32>().ok();
        } else if a == "-e" || a == "--eval" {
            match args.next() {
                Some(code) => eval_source = Some(code),
                None => die(2, "-e expects code"),
            }
        } else if a == "repl" && file.is_none() {
            force_repl = true;
        } else if a == "-h" || a == "--help" {
            println!(
                "usage: lumen-cli [repl | file.js [args...] | -e code] [--tier=interp|bytecode]"
            );
            return;
        } else {
            // First free arg is the script; the rest belong to it (visible via process.argv).
            file = Some(a);
            break;
        }
    }

    let mut runtime = Runtime::new();
    if let Some(t) = tier {
        runtime.engine().set_tier(t);
    }
    if let Some(n) = threshold {
        runtime.engine().set_tier_threshold(n);
    }

    if let Some(code) = eval_source {
        run_source(&mut runtime, &code);
    } else if let Some(path) = file {
        // Run as a CommonJS main module (require/module/__dirname in scope), like `node file`.
        if !std::path::Path::new(&path).is_file() {
            die(2, &format!("cannot read {path}: not a file"));
        }
        if let Err(e) = runtime.run_main(&path) {
            die(1, &format!("Uncaught {e}"));
        }
    } else if force_repl || std::io::stdin().is_terminal() {
        println!(
            "lumen {} (.help for help, .exit or Ctrl-D to quit)",
            env!("CARGO_PKG_VERSION")
        );
        let stdin = std::io::stdin();
        Repl::new(runtime).run(&mut stdin.lock(), &mut std::io::stdout());
    } else {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            die(2, "cannot read stdin");
        }
        run_source(&mut runtime, &src);
    }
}

/// Evaluate + loop to quiescence; uncaught top-level throws exit 1 (console output already
/// streamed as the script ran).
fn run_source(runtime: &mut Runtime, src: &str) {
    match runtime.eval(src) {
        Ok(Completion::Value(_)) => {}
        Ok(Completion::Throw { name, message }) => {
            if name.is_empty() {
                die(1, &format!("Uncaught {message}"));
            }
            die(1, &format!("Uncaught {name}: {message}"));
        }
        Err(e) => die(1, &format!("SyntaxError: {} (line {})", e.message, e.line)),
    }
}

fn die(code: i32, message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(code);
}
