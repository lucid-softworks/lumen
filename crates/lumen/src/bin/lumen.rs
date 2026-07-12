//! Minimal shell: evaluate JS files in one engine, printing console output as it appears.
//! Usage: lumen [--module] [--tier=interp|bytecode|jit] [--tier-threshold=N] [file.js ...]
//!        lumen -h | --help       print the help menu (all flags and env vars)
//!        lumen -v | --version    print the version
//!
//! With no files, reads from stdin: an interactive REPL when stdin is a terminal (one realm
//! for the whole session, incomplete input continues on the next line), otherwise the whole
//! stream is evaluated as one script.
//!
//! With `--module` each file is evaluated as an ES module; relative import specifiers resolve
//! against the importing file on disk (what a test262 host like test262.fyi expects).

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use lumen::{Completion, Engine};

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL_ALLOC: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;

fn main() {
    let mut module = false;
    let mut interactive = false;
    let mut tier = None;
    let mut threshold = None;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| {
            if a == "-h" || a == "--help" {
                print_help();
                std::process::exit(0);
            }
            if a == "-v" || a == "--version" {
                // The nightly workflow sets LUMEN_VERSION_SUFFIX to "-nightly (<short sha>)".
                println!(
                    "lumen {}{}",
                    env!("CARGO_PKG_VERSION"),
                    option_env!("LUMEN_VERSION_SUFFIX").unwrap_or("")
                );
                std::process::exit(0);
            }
            if a == "--module" {
                module = true;
                false
            } else if a == "-i" || a == "--interactive" {
                interactive = true;
                false
            } else if let Some(t) = a.strip_prefix("--tier=") {
                tier = Some(match t {
                    "bytecode" => lumen::bytecode::Tier::Bytecode,
                    "jit" => lumen::bytecode::Tier::Jit,
                    "interp" => lumen::bytecode::Tier::Interp,
                    other => {
                        eprintln!("error: unknown tier '{other}' (interp|bytecode|jit)");
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
    let mut engine = Engine::new();
    if let Some(t) = tier {
        engine.set_tier(t);
    }
    if let Some(n) = threshold {
        engine.set_tier_threshold(n);
    }
    if args.is_empty() {
        if module {
            eprintln!("error: --module requires a file");
            std::process::exit(2);
        }
        if interactive || std::io::stdin().is_terminal() {
            repl(&mut engine);
        } else {
            eval_stdin(&mut engine);
        }
        return;
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
            let key = normalize_path(Path::new(path))
                .to_string_lossy()
                .into_owned();
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

/// Print the `--help` menu: every flag the shell parses plus the runtime env vars it honours.
/// Keep this in sync with the argument loop in `main` and the tier env vars read by the engine.
fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    print!(
        "\
lumen {version} — a JavaScript engine

Usage:
  lumen [options] [file.js ...]   evaluate each file in one engine
  lumen [options] < script.js     evaluate stdin as one script
  lumen [-i] [options]            interactive REPL (default when stdin is a tty)

Options:
  -h, --help              Print this help and exit
  -v, --version           Print the version and exit
      --module            Evaluate each file as an ES module (relative imports
                          resolve against the importing file on disk)
  -i, --interactive       Force the REPL even when stdin is not a terminal
      --tier=TIER         Execution tier: interp | bytecode | jit
                          (default: jit; falls back to the bytecode VM where the
                          JIT is unavailable)
      --tier-threshold=N  Calls before an eligible function tiers up to a
                          compiled tier; 0 = immediately (default: 8)

Environment:
  LUMEN_TIER=TIER         Default execution tier (--tier overrides it)
  LUMEN_TIER_THRESHOLD=N  Default tier-up threshold (--tier-threshold overrides it)

Diagnostics (env, unstable):
  LUMEN_TIER_LOG=1        Report the AST construct a compile bail came from
  LUMEN_JIT_DUMP=SUBSTR   Dump the op stream of JIT'd chunks whose slot names match
  LUMEN_JIT_CODEDUMP=SUB  Dump the finished machine code of matching chunks (hex)
  LUMEN_JIT_OPSTAT=1      Tally ops that reach the JIT slow path (top at exit; =2 pinpoints sites)
  LUMEN_JIT_CALLSTAT=1    Tally calls that reach the inline-cache call helper
  LUMEN_JIT_LOOPLOG=1     Trace JIT loop back-edge compilation
"
    );
}

/// Interactive prompt: one engine (realm) for the whole session. Input that parses but for
/// running out of source (`at_eof`) continues on a `..` line; Ctrl-D exits.
fn repl(engine: &mut Engine) {
    println!("lumen {} (Ctrl-D to exit)", env!("CARGO_PKG_VERSION"));
    let stdin = std::io::stdin();
    let mut buffer = String::new();
    loop {
        print!("{}", if buffer.is_empty() { "> " } else { ".. " });
        std::io::stdout().flush().ok();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: {e}");
                return;
            }
        }
        buffer.push_str(&line);
        if buffer.trim().is_empty() {
            buffer.clear();
            continue;
        }
        match engine.eval(&buffer, false) {
            Err(e) if e.at_eof => continue, // incomplete input: keep reading
            result => {
                buffer.clear();
                report(engine, result, true);
            }
        }
    }
}

/// Non-terminal stdin: evaluate the whole stream as one script (e.g. `echo 1+1 | lumen`).
fn eval_stdin(engine: &mut Engine) {
    let mut src = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut src) {
        eprintln!("error: cannot read stdin: {e}");
        std::process::exit(2);
    }
    let result = engine.eval(&src, false);
    let ok = matches!(result, Ok(Completion::Value(_)));
    report(engine, result, false);
    if !ok {
        std::process::exit(1);
    }
}

/// Print buffered console output, then the completion: the result value (REPL only), an
/// `Uncaught ...` line for a throw, or the SyntaxError.
fn report(engine: &mut Engine, result: Result<Completion, lumen::ParseError>, echo_value: bool) {
    for line in engine.take_console() {
        println!("{line}");
    }
    match result {
        Ok(Completion::Value(v)) => {
            if echo_value && !v.is_empty() {
                println!("{v}");
            }
        }
        Ok(Completion::Throw { name, message }) => {
            if name.is_empty() {
                eprintln!("Uncaught {message}");
            } else {
                eprintln!("Uncaught {name}: {message}");
            }
        }
        Err(e) => eprintln!("SyntaxError: {}", e.message),
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
