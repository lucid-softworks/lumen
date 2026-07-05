//! Minimal shell: evaluate JS files in one engine, printing console output as it appears.
//! Usage: lumen [--module] [--tier=interp|bytecode] [--tier-threshold=N] <file.js> [more.js ...]
//!
//! With `--module` each file is evaluated as an ES module; relative import specifiers resolve
//! against the importing file on disk (what a test262 host like test262.fyi expects).

use std::path::{Path, PathBuf};

use lumen::{Completion, Engine};

fn main() {
    let mut module = false;
    let mut tier = None;
    let mut threshold = None;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| {
            if a == "--module" {
                module = true;
                false
            } else if let Some(t) = a.strip_prefix("--tier=") {
                tier = Some(match t {
                    "bytecode" => lumen::bytecode::Tier::Bytecode,
                    "interp" => lumen::bytecode::Tier::Interp,
                    other => {
                        eprintln!("error: unknown tier '{other}' (interp|bytecode)");
                        std::process::exit(2);
                    }
                });
                false
            } else if let Some(n) = a.strip_prefix("--tier-threshold=") {
                threshold = n.parse::<u32>().ok();
                false
            } else {
                true
            }
        })
        .collect();
    if args.is_empty() {
        eprintln!("usage: lumen [--module] <file.js> [more.js ...]");
        std::process::exit(2);
    }
    let mut engine = Engine::new();
    if let Some(t) = tier {
        engine.set_tier(t);
    }
    if let Some(n) = threshold {
        engine.set_tier_threshold(n);
    }
    for path in &args {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                std::process::exit(2);
            }
        };
        let result = if module {
            let key = normalize_path(Path::new(path)).to_string_lossy().into_owned();
            engine.eval_module(&src, &key, load_module)
        } else {
            engine.eval(&src, false)
        };
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

/// Resolve `spec` against the importing module's path and read it off disk. Non-UTF-8 sources are
/// decoded latin-1 style (test262 `type: "bytes"` fixtures).
fn load_module(spec: &str, referrer: &str) -> Option<(String, String)> {
    let base = Path::new(referrer).parent()?;
    let resolved = normalize_path(&base.join(spec));
    let bytes = std::fs::read(&resolved).ok()?;
    let text: String = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    };
    Some((resolved.to_string_lossy().into_owned(), text))
}

/// Lexically resolve `.` / `..` so a module that imports itself via a roundabout specifier maps to
/// the same registration key.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}
