//! test262 conformance runner for the `lumen` engine.
//!
//! Walks a tc39/test262 checkout, parses each test's YAML frontmatter, assembles the harness +
//! test source, runs it through lumen, and checks the outcome against the test's `negative`
//! expectation. Prints a per-category score + a top-failure histogram and writes
//! `test262-report/summary.json`.
//!
//! ## Crash resilience
//! A tree-walking interpreter can stack-overflow on adversarial input, and a stack overflow aborts
//! the whole OS process — `catch_unwind` cannot stop it. So the runner does **process isolation**:
//! the parent re-execs itself as short-lived `--worker` child processes over chunks of the test
//! list. If a child dies (overflow / OOM / abort), the parent records the one test it died on as a
//! crash-fail and respawns a worker for the rest of that chunk. One bad test costs one result, not
//! the whole run. (Per-test `catch_unwind` still handles ordinary panics inside a worker.)
//!
//! Usage:
//!   test262-runner [PATH ...]        # PATH is relative to <root>/test, e.g. language/expressions
//!   TEST262=/path/to/test262 test262-runner built-ins/Array
//!
//! With no PATH it runs `language/expressions` + `language/statements` (a fast, meaningful slice).

mod frontmatter;
mod report;

use frontmatter::{Frontmatter, Phase};
use lumen::{Completion, Engine};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Clone, Copy)]
struct Tally {
    pass: u32,
    fail: u32,
    skip: u32,
}

/// One test outcome.
enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

/// Tests per worker child process. Small keeps the blast radius of a crash tiny AND recycles the
/// worker often — since lumen has no GC, exiting the process is what reclaims any leaked memory.
const CHUNK: usize = 40;

/// Max concurrent worker processes. lumen's per-op allocation caps keep each worker bounded (tens
/// of MB in practice), so this can track core count; we still cap it so a huge box doesn't spawn an
/// absurd number of children.
const MAX_WORKERS: usize = 16;

/// Per-worker address-space ceiling (passed to `ulimit -v`, in KiB) so a runaway allocation makes
/// `malloc` fail (the worker aborts and is recorded as a crash) instead of eating all RAM. Enforced
/// on Linux; macOS may not honor RLIMIT_AS, so lumen's in-engine caps are the primary defense.
const WORKER_AS_LIMIT_KIB: u64 = 2 * 1024 * 1024; // 2 GiB

/// Wall-clock budget for one worker (a whole chunk). A normal chunk finishes in well under a
/// second; this only fires for a genuinely pathological test (e.g. an O(n²) `s += x` loop run a
/// million times, or an infinite `while (true) {}`). On timeout the parent kills the worker, marks
/// the test it was stuck on as a timeout-fail, and re-enqueues the rest — same path as a crash.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(6);

struct Harness {
    /// `assert.js` + `sta.js`, prepended to every non-raw test.
    base: String,
    /// Lazily-read `harness/<name>` include files.
    cache: Mutex<std::collections::HashMap<String, String>>,
    dir: PathBuf,
}

impl Harness {
    fn build(root: &Path) -> Harness {
        let h = root.join("harness");
        let assert = std::fs::read_to_string(h.join("assert.js")).unwrap_or_default();
        let sta = std::fs::read_to_string(h.join("sta.js")).unwrap_or_default();
        Harness {
            base: format!("{assert}\n{sta}\n"),
            cache: Mutex::new(std::collections::HashMap::new()),
            dir: h,
        }
    }
    fn include(&self, name: &str) -> String {
        if let Some(s) = self.cache.lock().unwrap().get(name) {
            return s.clone();
        }
        let text = std::fs::read_to_string(self.dir.join(name)).unwrap_or_default();
        self.cache
            .lock()
            .unwrap()
            .insert(name.to_string(), text.clone());
        text
    }
}

fn main() {
    // Worker mode: `--worker <root> <paths_file> <out_file> <lo> <hi>`. Runs tests [lo,hi) from
    // paths_file, writing one `idx\tKIND\treason` line per finished test (flushed immediately, so a
    // crash leaves every completed result on disk).
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s == "--worker").unwrap_or(false) {
        run_worker(&argv);
        return;
    }

    std::panic::set_hook(Box::new(|_| {}));

    let root = match find_root() {
        Some(r) => r,
        None => {
            eprintln!(
                "error: test262 checkout not found.\n\
                 Run scripts/test262-clone.sh, or set TEST262=/path/to/test262."
            );
            std::process::exit(2);
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let targets: Vec<String> = if args.is_empty() {
        vec!["language/expressions".into(), "language/statements".into()]
    } else {
        args.iter()
            .map(|a| a.trim_start_matches("test/").to_string())
            .collect()
    };

    let mut files = Vec::new();
    for t in &targets {
        collect(&root.join("test").join(t), &mut files);
    }
    files.sort();
    println!(
        "test262: {} files under {}",
        files.len(),
        targets.join(", ")
    );

    let results = run_isolated(&root, &files);
    report_results(&results, &targets);
}

// ---------------------------------------------------------------------------------------------
// Parent: process-isolated execution
// ---------------------------------------------------------------------------------------------

fn run_isolated(root: &Path, files: &[PathBuf]) -> Vec<(String, Outcome)> {
    if files.is_empty() {
        return Vec::new();
    }
    // Write the absolute path list once; workers read their slice by index.
    let paths_file = unique_tmp("paths");
    {
        let mut f = std::fs::File::create(&paths_file).expect("create paths file");
        for p in files {
            writeln!(f, "{}", p.display()).unwrap();
        }
    }

    // Shared work queue of [lo, hi) ranges and a slot per test for its result.
    let queue: Arc<Mutex<VecDeque<(usize, usize)>>> = Arc::new(Mutex::new(
        (0..files.len())
            .step_by(CHUNK)
            .map(|lo| (lo, (lo + CHUNK).min(files.len())))
            .collect(),
    ));
    let slots: Arc<Vec<Mutex<Option<Outcome>>>> =
        Arc::new((0..files.len()).map(|_| Mutex::new(None)).collect());
    let self_exe = std::env::current_exe().expect("current exe");
    let root_s = root.to_string_lossy().to_string();
    let paths_s = paths_file.to_string_lossy().to_string();

    let total = files.len();
    let done = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_WORKERS);
    eprintln!("running {total} tests across {nproc} worker processes...");
    let handles: Vec<_> = (0..nproc)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let slots = Arc::clone(&slots);
            let done = Arc::clone(&done);
            let self_exe = self_exe.clone();
            let root_s = root_s.clone();
            let paths_s = paths_s.clone();
            std::thread::spawn(move || {
                worker_loop(
                    &queue, &slots, &self_exe, &root_s, &paths_s, &done, total, started,
                );
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    let _ = std::fs::remove_file(&paths_file);

    // Drain the slots into (rel, outcome). Anything still empty never got scheduled (shouldn't
    // happen) — count it as a crash so totals stay honest.
    files
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let rel = rel_path(root, p);
            let outcome = slots[i]
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Outcome::Fail("not executed".into()));
            (rel, outcome)
        })
        .collect()
}

/// One parent thread: pull a chunk, run it in a child process, record results, and on a child
/// crash re-enqueue the remainder past the offending test.
#[allow(clippy::too_many_arguments)]
fn worker_loop(
    queue: &Mutex<VecDeque<(usize, usize)>>,
    slots: &[Mutex<Option<Outcome>>],
    self_exe: &Path,
    root: &str,
    paths_file: &str,
    done: &AtomicUsize,
    total: usize,
    started: Instant,
) {
    loop {
        let (lo, hi) = match queue.lock().unwrap().pop_front() {
            Some(c) => c,
            None => return,
        };
        let out_file = unique_tmp("out");
        // Launch the worker under `ulimit -v` so a runaway allocation hits the address-space limit
        // (malloc fails → worker aborts → recorded as a crash) rather than exhausting system RAM.
        let worker_cmd = format!(
            "ulimit -v {limit} 2>/dev/null; exec {exe} --worker {root} {paths} {out} {lo} {hi}",
            limit = WORKER_AS_LIMIT_KIB,
            exe = shell_quote(&self_exe.to_string_lossy()),
            root = shell_quote(root),
            paths = shell_quote(paths_file),
            out = shell_quote(&out_file.to_string_lossy()),
        );
        // Spawn the worker and wait with a deadline; kill it if it blows the time budget.
        let mut timed_out = false;
        let status = match std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&worker_cmd)
            .spawn()
        {
            Ok(mut child) => {
                // The deadline is per-*test*, not per-chunk: it resets whenever the worker writes a
                // new result (the out file grows). So a chunk of merely-slow tests keeps running as
                // long as each finishes within CHUNK_TIMEOUT; only a genuinely stuck (e.g. infinite-
                // loop) test, which makes no progress for the whole window, is killed.
                let mut deadline = Instant::now() + CHUNK_TIMEOUT;
                let mut last_len = 0u64;
                loop {
                    match child.try_wait() {
                        Ok(Some(st)) => break Ok(st),
                        Ok(None) => {
                            let len = std::fs::metadata(&out_file).map(|m| m.len()).unwrap_or(0);
                            if len > last_len {
                                last_len = len;
                                deadline = Instant::now() + CHUNK_TIMEOUT;
                            }
                            if Instant::now() >= deadline {
                                let _ = child.kill();
                                let _ = child.wait();
                                timed_out = true;
                                break Err(std::io::Error::other("timed out"));
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(e) => break Err(e),
                    }
                }
            }
            Err(e) => Err(e),
        };

        let mut recorded = std::collections::HashSet::new();
        if let Ok(text) = std::fs::read_to_string(&out_file) {
            for line in text.lines() {
                if let Some((idx, outcome)) = decode_line(line) {
                    if idx < slots.len() {
                        *slots[idx].lock().unwrap() = Some(outcome);
                        recorded.insert(idx);
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&out_file);

        // First index in the chunk with no recorded result = the test the child died on.
        let mut finalized = recorded.len();
        if let Some(crashed) = (lo..hi).find(|i| !recorded.contains(i)) {
            let why = if timed_out {
                "timed out (no progress — pathologically slow / infinite test)".to_string()
            } else {
                match status {
                    Ok(s) => format!("worker died ({s}) — likely stack overflow"),
                    Err(e) => format!("worker spawn failed: {e}"),
                }
            };
            *slots[crashed].lock().unwrap() = Some(Outcome::Fail(why));
            finalized += 1;
            if crashed + 1 < hi {
                queue.lock().unwrap().push_back((crashed + 1, hi));
            }
        }

        // Live progress to stderr (~every 2%), so a redirected run still shows it is advancing.
        let prev = done.fetch_add(finalized, Ordering::Relaxed);
        let now = prev + finalized;
        let step = (total / 50).max(1);
        if now / step != prev / step || now == total {
            let pct = 100.0 * now as f64 / total as f64;
            let secs = started.elapsed().as_secs_f64();
            let rate = if secs > 0.0 { now as f64 / secs } else { 0.0 };
            let eta = if rate > 0.0 {
                (total - now) as f64 / rate
            } else {
                0.0
            };
            eprintln!("  progress: {now}/{total} ({pct:.0}%)  {rate:.0} tests/s  eta {eta:.0}s",);
        }
    }
}

fn run_worker(argv: &[String]) {
    // Run on a thread with a large stack: a tree-walker (and especially generators/async, which
    // add frames) needs headroom for legitimately deep — but depth-bounded — recursion. The child's
    // 8 MiB main-thread stack is not enough.
    let argv: Vec<String> = argv.to_vec();
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || run_worker_inner(&argv))
        .expect("spawn worker thread")
        .join()
        .expect("worker thread");
}

fn run_worker_inner(argv: &[String]) {
    std::panic::set_hook(Box::new(|_| {}));
    let root = PathBuf::from(&argv[2]);
    let paths_file = &argv[3];
    let out_file = &argv[4];
    let lo: usize = argv[5].parse().unwrap_or(0);
    let hi: usize = argv[6].parse().unwrap_or(0);

    let harness = Harness::build(&root);
    let paths: Vec<String> = std::fs::read_to_string(paths_file)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    let mut out = std::fs::File::create(out_file).expect("create worker out");

    for idx in lo..hi {
        let Some(path) = paths.get(idx) else { break };
        let outcome = run_one(Path::new(path), &harness);
        let (kind, reason) = match outcome {
            Outcome::Pass => ('P', String::new()),
            Outcome::Fail(r) => ('F', r),
            Outcome::Skip(r) => ('S', r),
        };
        let reason = reason.replace(['\t', '\n', '\r'], " ");
        // Flush per line so a stack overflow on the NEXT test still leaves this result on disk.
        let _ = writeln!(out, "{idx}\t{kind}\t{reason}");
        let _ = out.flush();
    }
}

fn decode_line(line: &str) -> Option<(usize, Outcome)> {
    let mut parts = line.splitn(3, '\t');
    let idx: usize = parts.next()?.parse().ok()?;
    let kind = parts.next()?;
    let reason = parts.next().unwrap_or("").to_string();
    let outcome = match kind {
        "P" => Outcome::Pass,
        "S" => Outcome::Skip(reason),
        _ => Outcome::Fail(reason),
    };
    Some((idx, outcome))
}

/// POSIX single-quote a string so it survives `/bin/sh -c` as one argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn unique_tmp(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("t262_{}_{tag}_{n}", std::process::id()))
}

// ---------------------------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------------------------

fn report_results(results: &[(String, Outcome)], targets: &[String]) {
    let mut by_cat: std::collections::BTreeMap<String, Tally> = std::collections::BTreeMap::new();
    let mut skip_reasons: std::collections::BTreeMap<String, u32> = Default::default();
    let mut fail_buckets: std::collections::HashMap<String, u32> = Default::default();
    let mut total = Tally::default();
    for (rel, outcome) in results {
        let t = by_cat.entry(category(rel)).or_default();
        match outcome {
            Outcome::Pass => {
                t.pass += 1;
                total.pass += 1;
            }
            Outcome::Fail(why) => {
                t.fail += 1;
                total.fail += 1;
                *fail_buckets.entry(bucket(why)).or_default() += 1;
            }
            Outcome::Skip(reason) => {
                t.skip += 1;
                total.skip += 1;
                *skip_reasons.entry(reason.clone()).or_default() += 1;
            }
        }
    }

    println!(
        "\n{:<48} {:>7} {:>7} {:>7}",
        "category", "pass", "fail", "skip"
    );
    println!("{}", "-".repeat(72));
    for (cat, t) in &by_cat {
        println!("{cat:<48} {:>7} {:>7} {:>7}", t.pass, t.fail, t.skip);
    }
    println!("{}", "-".repeat(72));
    println!(
        "{:<48} {:>7} {:>7} {:>7}",
        "TOTAL", total.pass, total.fail, total.skip
    );
    let ran = total.pass + total.fail;
    let pct = if ran > 0 {
        100.0 * total.pass as f64 / ran as f64
    } else {
        0.0
    };
    println!(
        "\npass rate (excl. skipped): {pct:.1}%  ({}/{})",
        total.pass, ran
    );
    if !skip_reasons.is_empty() {
        let parts: Vec<String> = skip_reasons
            .iter()
            .map(|(r, n)| format!("{r}={n}"))
            .collect();
        println!("skips: {}", parts.join(", "));
    }

    let mut buckets: Vec<(String, u32)> = fail_buckets.into_iter().collect();
    buckets.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    if !buckets.is_empty() {
        println!("\ntop failure reasons:");
        for (reason, n) in buckets.iter().take(25) {
            println!("  {n:>5}  {reason}");
        }
    }

    if std::env::var("T262_SAMPLES").is_ok() {
        let grep = std::env::var("T262_GREP").ok();
        let cap: usize = std::env::var("T262_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
        println!("\nsample failures:");
        for (rel, outcome) in results
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Fail(_)))
            .filter(|(_, o)| match (&grep, o) { (Some(g), Outcome::Fail(why)) => why.contains(g.as_str()), _ => true })
            .take(cap)
        {
            if let Outcome::Fail(why) = outcome {
                println!("  {rel}\n      {why}");
            }
        }
    }

    if let Err(e) = report::write(&by_cat, total, targets) {
        eprintln!("warning: could not write report: {e}");
    }
}

/// Normalise a failure message into a coarse bucket (strip quoted/identifier specifics) so the
/// histogram groups like causes together.
fn bucket(why: &str) -> String {
    let mut out = String::new();
    let mut chars = why.chars().peekable();
    let mut words = 0;
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' || c == '«' {
            out.push('X');
            for d in chars.by_ref() {
                if d == '\'' || d == '"' || d == '»' {
                    break;
                }
            }
            continue;
        }
        if c == ' ' {
            words += 1;
            if words >= 7 {
                break;
            }
        }
        if c.is_ascii_digit() {
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn category(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.len() {
        0 => "?".into(),
        1 => parts[0].into(),
        _ => format!("{}/{}", parts[0], parts[1]),
    }
}

fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root.join("test"))
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

// ---------------------------------------------------------------------------------------------
// Running a single test (used inside a worker process)
// ---------------------------------------------------------------------------------------------

/// Isolate ordinary panics inside lumen as a `Fail`. (A stack overflow still aborts the worker —
/// the parent handles that as a crash.)
fn run_one(path: &Path, harness: &Harness) -> Outcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_one_inner(path, harness)
    }))
    .unwrap_or_else(|_| Outcome::Fail("panicked".into()))
}

fn run_one_inner(path: &Path, harness: &Harness) -> Outcome {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Outcome::Skip("unreadable".into()),
    };
    let fm = Frontmatter::parse(&src);

    if fm.has_flag("module") {
        return run_module(path, &src, harness, &fm);
    }
    let is_async = fm.has_flag("async");

    if fm.has_flag("raw") {
        return if is_async {
            check_async(&src, false, &fm, path)
        } else {
            check(&src, false, &fm, path)
        };
    }

    let mut preamble = harness.base.clone();
    // Async tests report completion via `$DONE`, defined in the implicitly-included doneprintHandle.
    if is_async && !fm.includes.iter().any(|i| i == "doneprintHandle.js") {
        preamble.push_str(&harness.include("doneprintHandle.js"));
        preamble.push('\n');
    }
    for inc in &fm.includes {
        preamble.push_str(&harness.include(inc));
        preamble.push('\n');
    }
    let program = format!("{preamble}\n{src}");
    let judge = |p: &str, strict: bool| {
        if is_async {
            check_async(p, strict, &fm, path)
        } else {
            check(p, strict, &fm, path)
        }
    };

    let mut ran = false;
    if !fm.has_flag("onlyStrict") {
        ran = true;
        if let r @ (Outcome::Fail(_) | Outcome::Skip(_)) = judge(&program, false) {
            return r;
        }
    }
    if !fm.has_flag("noStrict") {
        ran = true;
        let strict = format!("\"use strict\";\n{program}");
        if let r @ (Outcome::Fail(_) | Outcome::Skip(_)) = judge(&strict, true) {
            return r;
        }
    }
    if ran {
        Outcome::Pass
    } else {
        Outcome::Skip("no-variant".into())
    }
}

/// A fresh engine wired with a filesystem module loader (for dynamic `import()`), resolving
/// specifiers relative to the importing file (defaulting to `path` for top-level script imports).
fn engine_for(path: &Path) -> Engine {
    let mut engine = Engine::new();
    engine.set_module_loader(|spec: &str, referrer: &str| {
        let base = Path::new(referrer).parent()?;
        let resolved = normalize_path(&base.join(spec));
        let text = std::fs::read_to_string(&resolved).ok()?;
        Some((resolved.to_string_lossy().into_owned(), text))
    });
    engine.set_import_base(&path.to_string_lossy());
    engine
}

/// Run an `async`-flagged test, judging by the `$DONE` completion message printed to the console.
fn check_async(program: &str, strict: bool, _fm: &Frontmatter, path: &Path) -> Outcome {
    let mut engine = engine_for(path);
    match engine.eval(program, strict) {
        Err(e) => return Outcome::Fail(format!("unexpected SyntaxError: {}", e.message)),
        Ok(Completion::Throw { name, message }) => {
            return Outcome::Fail(format!("unexpected throw {name}: {message}"));
        }
        Ok(Completion::Value(_)) => {}
    }
    let console = engine.take_console();
    if console.iter().any(|l| l == "Test262:AsyncTestComplete") {
        Outcome::Pass
    } else if let Some(fail) = console
        .iter()
        .find(|l| l.starts_with("Test262:AsyncTestFailure:"))
    {
        Outcome::Fail(format!(
            "async: {}",
            &fail["Test262:AsyncTestFailure:".len()..]
        ))
    } else {
        Outcome::Fail("async test did not signal completion".into())
    }
}

/// Normalize a path, collapsing `.`/`..` segments (without touching the filesystem).
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

/// Run a `module`-flagged test: evaluate the harness as a script (for the global helpers), then
/// load the test as an ES module with a filesystem loader resolving relative specifiers.
fn run_module(path: &Path, src: &str, harness: &Harness, fm: &Frontmatter) -> Outcome {
    if fm.has_flag("async") {
        return Outcome::Skip("async".into());
    }
    let mut engine = engine_for(path);
    let mut preamble = harness.base.clone();
    for inc in &fm.includes {
        preamble.push_str(&harness.include(inc));
        preamble.push('\n');
    }
    if let Ok(Completion::Throw { name, message }) = engine.eval(&preamble, false) {
        return Outcome::Fail(format!("harness threw {name}: {message}"));
    }
    let key = path.to_string_lossy().into_owned();
    let loader = |spec: &str, referrer: &str| -> Option<(String, String)> {
        let base = Path::new(referrer).parent()?;
        let resolved = normalize_path(&base.join(spec));
        let text = std::fs::read_to_string(&resolved).ok()?;
        Some((resolved.to_string_lossy().into_owned(), text))
    };
    let result = engine.eval_module(src, &key, loader);
    judge_module(result, fm)
}

fn judge_module(result: Result<Completion, lumen::ParseError>, fm: &Frontmatter) -> Outcome {
    match (&fm.negative, result) {
        (None, Ok(Completion::Value(_))) => Outcome::Pass,
        (None, Ok(Completion::Throw { name, message })) => {
            Outcome::Fail(format!("unexpected throw {name}: {message}"))
        }
        (None, Err(e)) => Outcome::Fail(format!("unexpected SyntaxError: {}", e.message)),
        // A parse/early/resolution error surfaces as a SyntaxError (thrown or at parse time).
        (Some(neg), Err(_))
            if matches!(neg.phase, Phase::Parse | Phase::Early | Phase::Resolution) =>
        {
            if neg.error_type == "SyntaxError" {
                Outcome::Pass
            } else {
                Outcome::Fail(format!("parse error but expected {}", neg.error_type))
            }
        }
        (Some(neg), Ok(Completion::Throw { name, .. })) => {
            if name == neg.error_type {
                Outcome::Pass
            } else {
                Outcome::Fail(format!("expected {}, threw {name}", neg.error_type))
            }
        }
        (Some(neg), Ok(Completion::Value(_))) => Outcome::Fail(format!(
            "expected {} but completed normally",
            neg.error_type
        )),
        (Some(neg), Err(e)) => Outcome::Fail(format!(
            "expected runtime {} but parse failed: {}",
            neg.error_type, e.message
        )),
    }
}

/// Run one assembled program and judge it against the negative expectation.
fn check(program: &str, strict: bool, fm: &Frontmatter, path: &Path) -> Outcome {
    let result = engine_for(path).eval(program, strict);
    match (&fm.negative, result) {
        (None, Ok(Completion::Value(_))) => Outcome::Pass,
        (None, Ok(Completion::Throw { name, message })) => {
            Outcome::Fail(format!("unexpected throw {name}: {message}"))
        }
        (None, Err(e)) => Outcome::Fail(format!("unexpected SyntaxError: {}", e.message)),

        (Some(neg), Err(_))
            if matches!(neg.phase, Phase::Parse | Phase::Early | Phase::Resolution) =>
        {
            if neg.error_type == "SyntaxError" {
                Outcome::Pass
            } else {
                Outcome::Fail(format!("parse error but expected {}", neg.error_type))
            }
        }
        (Some(neg), Ok(Completion::Throw { name, .. }))
            if matches!(neg.phase, Phase::Parse | Phase::Early | Phase::Resolution) =>
        {
            if name == neg.error_type {
                Outcome::Pass
            } else {
                Outcome::Fail(format!(
                    "expected {} at {:?}, threw {name}",
                    neg.error_type, neg.phase
                ))
            }
        }

        (Some(neg), Ok(Completion::Throw { name, .. })) => {
            if name == neg.error_type {
                Outcome::Pass
            } else {
                Outcome::Fail(format!("expected {}, threw {name}", neg.error_type))
            }
        }
        (Some(neg), Ok(Completion::Value(_))) => Outcome::Fail(format!(
            "expected {} but completed normally",
            neg.error_type
        )),
        (Some(neg), Err(e)) => Outcome::Fail(format!(
            "expected runtime {} but parse failed: {}",
            neg.error_type, e.message
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Test discovery
// ---------------------------------------------------------------------------------------------

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.is_file() {
        if is_test_file(dir) {
            out.push(dir.to_path_buf());
        }
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "intl402" | "staging") {
                continue;
            }
            collect(&path, out);
        } else if is_test_file(&path) {
            out.push(path);
        }
    }
}

fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".js") && !name.ends_with("_FIXTURE.js")
}

fn find_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TEST262") {
        let p = PathBuf::from(p);
        if p.join("test").is_dir() {
            return Some(p);
        }
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("test262");
        if candidate.join("test").is_dir() && candidate.join("harness").is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}
