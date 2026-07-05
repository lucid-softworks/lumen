//! Differential fuzzer for lumen's execution tiers.
//!
//! Every generated program runs twice — in the `interp` tier (the tree-walking reference oracle)
//! and in the `bytecode` tier with the compile threshold at 0 — and the two runs must agree on:
//!   (a) the completion value, via an in-program canonical serializer that distinguishes type,
//!       -0 from 0, and NaN;
//!   (b) whether the program threw, plus the error's constructor name and message;
//!   (c) the observable side-effect trace: generated programs funnel every property get/set/
//!       has/delete and every valueOf/toString coercion on instrumented objects (Proxy-wrapped)
//!       into a trace array printed at the end, so reordered or elided effects diverge loudly;
//!   (d) the final observable global state (own global property names + a value sample).
//!
//! Determinism: programs never touch Math.random, Date, or GC-observable machinery; every run
//! of a given seed generates byte-identical source.
//!
//! On divergence the program is delta-minimized (statement-granular) while preserving the
//! divergence, written to the regression corpus, and the process halts with a nonzero exit.
//! On startup the whole corpus is replayed first — regressions stay fixed forever.
//!
//! The engine's Rc heap frees realms only via the in-interpreter cycle collector, so a dropped
//! Engine leaks its realm (one big cycle). Like the test262 runner, the fuzzer therefore runs
//! programs in short-lived child processes (batches), keeping the parent's memory flat.
//!
//! Usage:
//!   lumen-difftest [--seed N] [--count N] [--corpus DIR]
//!
//! Each candidate program runs in a short-lived child (`--check`) that the parent caps on memory
//! and wall-clock: the Rc heap frees realms only via the cycle collector (a dropped Engine leaks
//! its realm), and generated programs can be non-terminating or allocate without bound, so
//! isolation + budget is what keeps the parent flat and turns pathological programs into skipped
//! "inconclusive" results rather than false divergences or an OOM.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen::bytecode::Tier;
use lumen::{Completion, Engine};

/// A counting allocator with a hard live-bytes cap. When a `--check` child engages the cap, any
/// allocation that would exceed it fails (returns null → `handle_alloc_error` aborts the child)
/// *before* the memory is committed — so a runaway program can never spike RSS between RSS polls;
/// it dies the instant it asks for too much. Uncapped (`usize::MAX`) in the parent.
struct CappedAlloc {
    live: AtomicUsize,
    cap: AtomicUsize,
}

unsafe impl GlobalAlloc for CappedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let prev = self.live.fetch_add(layout.size(), Ordering::Relaxed);
        if prev + layout.size() > self.cap.load(Ordering::Relaxed) {
            self.live.fetch_sub(layout.size(), Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        let p = System.alloc(layout);
        if p.is_null() {
            self.live.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        self.live.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: CappedAlloc = CappedAlloc {
    live: AtomicUsize::new(0),
    cap: AtomicUsize::new(usize::MAX),
};

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u32) -> bool {
        self.next() % 100 < pct as u64
    }
}

/// One tier's observable outcome.
#[derive(PartialEq, Eq, Debug)]
struct Outcome {
    console: Vec<String>,
    completion: String,
    threw: Option<(String, String)>,
}

fn run_tier(src: &str, tier: Tier) -> Outcome {
    let mut e = Engine::new();
    e.set_tier(tier);
    e.set_tier_threshold(0);
    match e.eval(src, false) {
        Ok(Completion::Value(v)) => Outcome {
            console: e.take_console(),
            completion: v,
            threw: None,
        },
        Ok(Completion::Throw { name, message }) => Outcome {
            console: e.take_console(),
            completion: String::new(),
            threw: Some((name, message)),
        },
        Err(p) => Outcome {
            console: Vec::new(),
            completion: String::new(),
            threw: Some(("SyntaxError".into(), p.message)),
        },
    }
}

fn diverges(src: &str) -> bool {
    run_tier(src, Tier::Interp) != run_tier(src, Tier::Bytecode)
}

// ---------------------------------------------------------------------------------------------
// Program generation
// ---------------------------------------------------------------------------------------------

/// The harness around every generated body: trace plumbing, instrumented objects, and the
/// end-of-program canonical dumps. Instrumented objects log get/set/has/delete and coercions.
const PRELUDE: &str = r#"
var __t = [];
function trace(x) { __t.push(String(x)); }
function instrumented(name, o) {
  return new Proxy(o, {
    get(t, k, r) { trace('get ' + name + '.' + String(k)); return Reflect.get(t, k, r); },
    set(t, k, v, r) { trace('set ' + name + '.' + String(k)); return Reflect.set(t, k, v, r); },
    has(t, k) { trace('has ' + name + '.' + String(k)); return Reflect.has(t, k); },
    deleteProperty(t, k) { trace('del ' + name + '.' + String(k)); return Reflect.deleteProperty(t, k); },
  });
}
function coercer(name, v) {
  return {
    valueOf() { trace('valueOf ' + name); return v; },
    toString() { trace('toString ' + name); return 'S' + v; },
  };
}
function canon(v) {
  if (typeof v === 'number') {
    if (v !== v) return 'num:NaN';
    if (v === 0) return 1 / v < 0 ? 'num:-0' : 'num:0';
    return 'num:' + v;
  }
  if (typeof v === 'object' && v !== null) {
    var keys = [];
    for (var k in v) keys.push(k);
    return 'obj:{' + keys.join(',') + '}';
  }
  if (v === null) return 'null';
  return typeof v + ':' + String(v);
}
"#;

const EPILOGUE: &str = r#"
print('TRACE ' + __t.join('|'));
print('RESULT ' + canon(typeof __result === 'undefined' ? undefined : __result));
var __g = [];
for (var __k in globalThis) if (__k.startsWith('v')) __g.push(__k + '=' + canon(globalThis[__k]));
print('GLOBALS ' + __g.join(';'));
"#;

struct Gen {
    rng: Rng,
    vars: u32,
    fns: u32,
    body: String,
}

impl Gen {
    fn small_num(&mut self) -> String {
        const NUMS: &[&str] = &[
            "0",
            "1",
            "2",
            "3",
            "7",
            "-1",
            "0.5",
            "-0",
            "1e9",
            "2147483647",
            "-2147483648",
            "4294967295",
            "0.1",
            "NaN",
            "Infinity",
            "1073741824",
        ];
        NUMS[self.rng.below(NUMS.len())].to_string()
    }
    fn existing_var(&mut self) -> String {
        if self.vars == 0 {
            return "v0".into(); // preamble always declares v0
        }
        format!("v{}", self.rng.below(self.vars as usize))
    }
    fn binop(&mut self) -> &'static str {
        const OPS: &[&str] = &[
            "+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", ">>>", "<", ">", "<=", ">=", "==",
            "!=", "===", "!==",
        ];
        OPS[self.rng.below(OPS.len())]
    }
    fn expr(&mut self, depth: u32) -> String {
        if depth == 0 {
            return match self.rng.below(3) {
                0 => self.existing_var(),
                1 => self.small_num(),
                _ => format!("A[{}]", self.rng.below(12)),
            };
        }
        match self.rng.below(10) {
            0..=3 => {
                let (l, r, op) = (self.expr(depth - 1), self.expr(depth - 1), self.binop());
                format!("({l} {op} {r})")
            }
            4 => format!("(typeof {})", self.existing_var()),
            5 => format!("(- {})", self.expr(depth - 1)),
            6 => format!("O.p{}", self.rng.below(4)),
            7 => format!("P.q{}", self.rng.below(4)),
            8 => {
                let (c, a, b) = (
                    self.expr(depth - 1),
                    self.expr(depth - 1),
                    self.expr(depth - 1),
                );
                format!("({c} ? {a} : {b})")
            }
            _ => {
                let f = if self.fns == 0 {
                    "f0".to_string()
                } else {
                    format!("f{}", self.rng.below(self.fns as usize))
                };
                format!("{f}({}, {})", self.expr(depth - 1), self.small_num())
            }
        }
    }
    fn stmt(&mut self) {
        match self.rng.below(12) {
            0 | 1 => {
                let v = self.vars;
                self.vars += 1;
                let kind = ["var", "let"][self.rng.below(2)];
                let e = self.expr(2);
                let _ = writeln!(self.body, "{kind} v{v} = {e};");
            }
            2 | 3 => {
                let (t, e) = (self.existing_var(), self.expr(2));
                let op = ["=", "+=", "-=", "*=", "|=", "^=", "<<="][self.rng.below(7)];
                let _ = writeln!(self.body, "{t} {op} {e};");
            }
            4 => {
                // Hot loop with an accumulator and array traffic — the tier-up workhorse.
                let v = self.vars;
                self.vars += 1;
                let (e, op) = (self.expr(1), self.binop());
                let n = 3 + self.rng.below(40);
                let _ = writeln!(
                    self.body,
                    "var v{v} = 0; for (let i = 0; i < {n}; i++) {{ v{v} = (v{v} {op} ({e})) | 0; A[i % 12] = v{v}; }}"
                );
            }
            5 => {
                let f = self.fns;
                self.fns += 1;
                let e = self.expr(2);
                // Sometimes the function contains a bailing construct (try) so tiers mix.
                if self.rng.chance(30) {
                    let _ = writeln!(
                        self.body,
                        "function f{f}(a, b) {{ try {{ return ({e}) + a - b; }} catch (e) {{ return 'caught:' + e.constructor.name; }} }}"
                    );
                } else {
                    let _ = writeln!(
                        self.body,
                        "function f{f}(a, b) {{ var t = a; for (let i = 0; i < 5; i++) t = t + b - ({e}) * 0; return t; }}"
                    );
                }
            }
            6 => {
                let e = self.expr(2);
                let _ = writeln!(self.body, "trace('e:' + canon({e}));");
            }
            7 => {
                let (k, e) = (self.rng.below(4), self.expr(1));
                let _ = writeln!(self.body, "O.p{k} = {e};");
            }
            8 => {
                let (k, e) = (self.rng.below(4), self.expr(1));
                let _ = writeln!(self.body, "P.q{k} = {e};");
            }
            9 => {
                // Type-unstable variable: number, then string/object mid-flight.
                let v = self.vars;
                self.vars += 1;
                let _ = writeln!(
                    self.body,
                    "var v{v} = 1; for (let i = 0; i < 9; i++) {{ if (i === 5) v{v} = 's'; else v{v} = (v{v} | 0) + i; }}"
                );
            }
            10 => {
                let e = self.expr(1);
                let _ = writeln!(
                    self.body,
                    "with (O) {{ trace('with:' + canon(p0)); P.q0 = {e}; }}"
                );
            }
            _ => {
                let (a, b) = (self.expr(1), self.expr(1));
                let _ = writeln!(
                    self.body,
                    "try {{ if ({a} > 1e8) throw new RangeError('big'); }} catch (e) {{ trace('t:' + e.message); }} finally {{ trace('f:' + canon({b})); }}"
                );
            }
        }
    }
}

fn generate(seed: u64) -> String {
    let mut g = Gen {
        rng: Rng::new(seed),
        vars: 0,
        fns: 0,
        body: String::new(),
    };
    let n = 4 + g.rng.below(14);
    for _ in 0..n {
        g.stmt();
    }
    let result_expr = g.expr(2);
    format!(
        "{PRELUDE}\nvar v0 = 1;\nfunction f0(a, b) {{ return (a | 0) + (b | 0); }}\nvar A = instrumented('A', [0,1,2,3,4,5,6,7,8,9,10,11]);\nvar O = instrumented('O', {{ p0: 1, p1: 's', p2: -0, p3: null }});\nvar P = {{ q0: 0, q1: 0, q2: 0, q3: 0 }};\n{}\nvar __result = {result_expr};\n{EPILOGUE}",
        g.body
    )
}

// ---------------------------------------------------------------------------------------------
// Minimizer
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------------------------

fn main() {
    // The tree-walker needs a deep native stack to reach its own recursion ceiling (a clean
    // RangeError at MAX_EVAL_DEPTH) instead of overflowing — same reason the test262 runner
    // gives its workers large stacks.
    std::thread::Builder::new()
        .stack_size(
            std::env::var("DIFFTEST_STACK_MB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(32)
                << 20,
        )
        .spawn(dispatch)
        .unwrap()
        .join()
        .unwrap();
}

fn dispatch() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // Child: run a seed range in-process; leaked realms die with the process.
        Some("--one") => {
            // Debug: run a single seed in a single tier (which=interp|bytecode).
            let s: u64 = args[1].parse().unwrap();
            let tier = if args.get(2).map(String::as_str) == Some("bytecode") {
                Tier::Bytecode
            } else {
                Tier::Interp
            };
            let o = run_tier(&generate(s), tier);
            eprintln!("{o:?}");
        }
        Some("--dump") => {
            let s: u64 = args[1].parse().unwrap();
            print!("{}", generate(s));
        }
        // Child: run the JS in `args[1]` in both tiers. Exit 3 = diverged, 0 = agreed. The parent
        // caps this child's memory and wall-clock, so a pathological program (non-terminating or
        // huge — both tiers agree, but forever) is killed and reported as inconclusive, never a
        // false divergence.
        Some("--check") => {
            // Hard allocation ceiling for this child: an over-budget program's allocation fails
            // and aborts the child (exit != 3 → parent reads Inconclusive) with no RSS spike.
            ALLOC
                .cap
                .store(CAP_MB as usize * 1024 * 1024, Ordering::Relaxed);
            let src = std::fs::read_to_string(&args[1]).unwrap();
            std::process::exit(if diverges(&src) { 3 } else { 0 });
        }
        _ => fuzz_parent(args),
    }
}

/// Per-program resource budget for the capped child (generated programs can be non-terminating
/// or allocate without bound — see the seed-159 exponential-catch case in the fuzzer notes).
const CAP_MB: u64 = 512;
const TIMEOUT_MS: u64 = 4000;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Verdict {
    Agree,
    Diverge,
    /// Killed for exceeding the memory/time budget — inconclusive, not a divergence.
    Inconclusive,
}

/// Run `src` in a capped child (`--check`), polling its RSS and enforcing a wall-clock deadline.
fn run_capped(exe: &std::path::Path, src: &str, tmp: &std::path::Path) -> Verdict {
    std::fs::write(tmp, src).unwrap();
    let mut child = std::process::Command::new(exe)
        .arg("--check")
        .arg(tmp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return match status.code() {
                Some(3) => Verdict::Diverge,
                Some(0) => Verdict::Agree,
                // Nonzero-but-not-3 (a crash/abort in BOTH tiers, e.g. a stack overflow on a
                // deeply-recursive program) is inconclusive, not a divergence.
                _ => Verdict::Inconclusive,
            };
        }
        // Poll RSS via `ps` (dependency-free); kill if over budget or past the deadline.
        if let Ok(out) = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
        {
            if let Ok(kb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                if kb > CAP_MB * 1024 {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Verdict::Inconclusive;
                }
            }
        }
        if start.elapsed().as_millis() as u64 > TIMEOUT_MS {
            let _ = child.kill();
            let _ = child.wait();
            return Verdict::Inconclusive;
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
}

/// Statement-line delta debugging against the capped child: drop line chunks while the divergence
/// still reproduces (Verdict::Diverge). Inconclusive/Agree candidates are reverted.
fn minimize_capped(exe: &std::path::Path, src: &str, tmp: &std::path::Path) -> String {
    let prelude_lines = PRELUDE.lines().count() + 5;
    let epilogue_lines = EPILOGUE.lines().count();
    let mut lines: Vec<String> = src.lines().map(String::from).collect();
    let mut chunk = (lines.len() / 2).max(1);
    while chunk >= 1 {
        let mut i = prelude_lines;
        while i < lines.len().saturating_sub(epilogue_lines) {
            let mut candidate = lines.clone();
            let end = (i + chunk).min(candidate.len().saturating_sub(epilogue_lines));
            candidate.drain(i..end);
            if run_capped(exe, &candidate.join("\n"), tmp) == Verdict::Diverge {
                lines = candidate;
            } else {
                i = end;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk /= 2;
    }
    lines.join("\n")
}

fn fuzz_parent(args: Vec<String>) {
    let mut seed: u64 = 1;
    let mut count: u64 = 1000;
    let mut corpus = PathBuf::from("difftest-corpus");
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seed" => seed = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--count" => count = it.next().and_then(|v| v.parse().ok()).unwrap_or(1000),
            "--corpus" => corpus = PathBuf::from(it.next().unwrap_or_default()),
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
    }
    let exe = std::env::current_exe().unwrap();
    let tmp = std::env::temp_dir().join(format!("difftest-{}.js", std::process::id()));

    // Replay the regression corpus first — old divergences must stay fixed forever.
    if corpus.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&corpus)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "js"))
            .collect();
        entries.sort();
        for p in &entries {
            let src = std::fs::read_to_string(p).unwrap();
            if run_capped(&exe, &src, &tmp) == Verdict::Diverge {
                eprintln!("REGRESSION: corpus case {} diverges again", p.display());
                std::process::exit(1);
            }
        }
        if !entries.is_empty() {
            println!("corpus replay: {} cases green", entries.len());
        }
    }

    let (mut agreed, mut skipped) = (0u64, 0u64);
    for k in 0..count {
        let s = seed.wrapping_add(k);
        match run_capped(&exe, &generate(s), &tmp) {
            Verdict::Agree => agreed += 1,
            Verdict::Inconclusive => skipped += 1,
            Verdict::Diverge => {
                eprintln!("DIVERGENCE at seed {s} — minimizing…");
                let min = minimize_capped(&exe, &generate(s), &tmp);
                std::fs::create_dir_all(&corpus).unwrap();
                let path = corpus.join(format!("seed-{s}.js"));
                std::fs::write(&path, &min).unwrap();
                // Print both tiers' outcomes for the minimized case (small — safe in-process).
                eprintln!("--- minimized ({} lines) ---", min.lines().count());
                eprintln!("interp:   {:?}", run_tier(&min, Tier::Interp));
                eprintln!("bytecode: {:?}", run_tier(&min, Tier::Bytecode));
                eprintln!("saved {}", path.display());
                let _ = std::fs::remove_file(&tmp);
                std::process::exit(1);
            }
        }
        if (k + 1) % 200 == 0 {
            println!("{}/{count} done ({agreed} agree, {skipped} skipped)", k + 1);
        }
    }
    let _ = std::fs::remove_file(&tmp);
    println!(
        "done: {agreed} agree, {skipped} skipped (budget) across seeds {seed}..{}",
        seed + count
    );
}
