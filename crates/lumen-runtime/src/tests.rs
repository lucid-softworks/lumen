use super::*;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// A console sink the test can read back after the loop runs.
#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// A runtime with console captured: (runtime, stdout sink, stderr sink).
fn test_runtime() -> (Runtime, Captured, Captured) {
    let mut rt = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    rt.engine().ctx().op_state().put(console::ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });
    (rt, out, err)
}

fn eval_ok(rt: &mut Runtime, src: &str) {
    match rt.eval(src).expect("parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

/// The Phase-2 acceptance test: setTimeout + queueMicrotask + console.log complete in the
/// right order and the loop exits by itself.
#[test]
fn acceptance_timers_microtasks_console() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log("start");
        setTimeout(() => console.log("timeout"), 10);
        setTimeout(() => console.log("timeout-late"), 20);
        queueMicrotask(() => console.log("micro"));
        Promise.resolve().then(() => console.log("promise"));
        console.log("end");
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "start",
            "end",
            "micro",
            "promise",
            "timeout",
            "timeout-late"
        ]
    );
}

#[test]
fn interval_fires_until_cleared_and_loop_exits() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        let n = 0;
        const id = setInterval(() => {
            n++;
            console.log("tick", n);
            if (n === 3) clearInterval(id);
        }, 5);
        "#,
    );
    // run_to_completion returned, so the cleared interval no longer holds the loop open.
    assert_eq!(out.lines(), ["tick 1", "tick 2", "tick 3"]);
}

#[test]
fn clear_timeout_cancels() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const id = setTimeout(() => console.log("no"), 5);
        clearTimeout(id);
        setTimeout(() => console.log("yes"), 10);
        "#,
    );
    assert_eq!(out.lines(), ["yes"]);
}

#[test]
fn next_tick_and_set_immediate_run_before_timers() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        setTimeout(() => console.log("timer"), 0);
        setImmediate(() => console.log("immediate"));
        process.nextTick((tag) => console.log("tick", tag), 42);
        "#,
    );
    assert_eq!(out.lines(), ["immediate", "tick 42", "timer"]);
}

#[test]
fn timer_args_pass_through_and_nested_timers_work() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        setTimeout((a, b) => {
            console.log("outer", a + b);
            setTimeout(() => console.log("inner"), 5);
        }, 5, 20, 22);
        "#,
    );
    assert_eq!(out.lines(), ["outer 42", "inner"]);
}

#[test]
fn uncaught_callback_error_reports_and_loop_survives() {
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        setTimeout(() => { throw new TypeError("boom") }, 5);
        setTimeout(() => console.log("still running"), 10);
        "#,
    );
    assert_eq!(out.lines(), ["still running"]);
    assert_eq!(err.lines(), ["Uncaught TypeError: boom"]);
}

#[test]
fn spawn_blocking_completion_settles_on_the_loop() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        "globalThis.onDone = (n) => console.log('got', n); 0",
    );
    let g = rt.engine().global_this();
    let cb = rt
        .engine()
        .ctx()
        .get_member(&g, "onDone")
        .map_err(|_| ())
        .expect("defined above");
    rt.spawn_blocking(
        || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            Box::new(21u64)
        },
        cb,
        |_ctx, payload| {
            let n = *payload.downcast::<u64>().expect("u64 payload");
            Ok(vec![Value::Num((n * 2) as f64)])
        },
    );
    // The in-flight task must hold the loop open until its completion arrives.
    rt.run_to_completion();
    assert_eq!(out.lines(), ["got 42"]);
}

#[test]
fn console_streams_and_renders_common_values() {
    let (mut rt, out, err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log("s", 1.5, true, null, undefined, Symbol("sym"), [1, 2], { a: 1 });
        console.warn("careful");
        console.error("bad");
        "#,
    );
    assert_eq!(
        out.lines(),
        ["s 1.5 true null undefined Symbol(sym) 1,2 [object Object]"]
    );
    assert_eq!(err.lines(), ["careful", "bad"]);
}

#[test]
fn process_basics() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(typeof process.cwd(), process.cwd().length > 0);
        console.log(Array.isArray(process.argv), typeof process.argv[0]);
        console.log(typeof process.env, typeof process.platform);
        "#,
    );
    assert_eq!(out.lines(), ["string true", "true string", "object string"]);
}

#[test]
fn async_await_settles_before_loop_exit() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
        (async () => {
            console.log("before");
            await delay(10);
            console.log("after");
        })();
        "#,
    );
    assert_eq!(out.lines(), ["before", "after"]);
}

// ---- fs (the runtime assembles lumen-fs, so its behavior tests live here) ----

/// A unique temp dir per test, cleaned up on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("lumen-fs-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        TempDir(dir)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The Phase-4 acceptance test: sync and async read/write both round-trip a file from JS.
#[test]
fn fs_sync_and_async_roundtrip() {
    let dir = TempDir::new("roundtrip");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const sync = {sync:?}, asyncPath = {async_:?};
            fs.writeFileSync(sync, "hello sync");
            console.log("sync:", fs.readFileSync(sync));
            (async () => {{
                await fs.promises.writeFile(asyncPath, "hello async");
                console.log("async:", await fs.promises.readFile(asyncPath));
            }})();
            "#,
            sync = dir.path("s.txt"),
            async_ = dir.path("a.txt"),
        ),
    );
    assert_eq!(out.lines(), ["sync: hello sync", "async: hello async"]);
}

#[test]
fn fs_exists_readdir_unlink_append() {
    let dir = TempDir::new("meta");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const d = {dir:?}, f = {file:?};
            console.log(fs.existsSync(f));
            fs.writeFileSync(f, "a");
            fs.appendFileSync(f, "b");
            console.log(fs.existsSync(f), fs.readFileSync(f));
            console.log(fs.readdirSync(d).join(","));
            fs.unlinkSync(f);
            console.log(fs.existsSync(f));
            "#,
            dir = dir.path(""),
            file = dir.path("x.txt"),
        ),
    );
    assert_eq!(out.lines(), ["false", "true ab", "x.txt", "false"]);
}

#[test]
fn fs_handles_via_resource_table() {
    let dir = TempDir::new("handles");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const f = {file:?};
            const w = fs.openSync(f, "w");
            fs.writeSync(w, "line one\n");
            fs.writeSync(w, "line two\n");
            fs.closeSync(w);
            const r = fs.openSync(f, "r");
            console.log(JSON.stringify(fs.readSync(r)));
            fs.closeSync(r);
            try {{ fs.readSync(r) }} catch (e) {{ console.log("stale:", e.constructor.name) }}
            "#,
            file = dir.path("h.txt"),
        ),
    );
    assert_eq!(
        out.lines(),
        [r#""line one\nline two\n""#, "stale: TypeError"]
    );
}

#[test]
fn fs_promise_rejection_is_catchable() {
    let dir = TempDir::new("reject");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            (async () => {{
                try {{
                    await fs.promises.readFile({missing:?});
                    console.log("unexpected success");
                }} catch (e) {{
                    console.log("caught:", e.message.includes("readFile"), e.message.includes("nope.txt"));
                }}
            }})();
            "#,
            missing = dir.path("nope.txt"),
        ),
    );
    assert_eq!(out.lines(), ["caught: true true"]);
}

#[test]
fn fs_sync_error_throws_catchable_error() {
    let dir = TempDir::new("syncerr");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            "try {{ fs.readFileSync({missing:?}) }} catch (e) {{ console.log('caught', e instanceof Error) }}",
            missing = dir.path("gone.txt"),
        ),
    );
    assert_eq!(out.lines(), ["caught true"]);
}
