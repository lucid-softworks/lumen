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

// ---- lumen-web (WinterTC minimum common API; the runtime assembles it) ----

#[test]
fn web_encoding_and_base64() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const enc = new TextEncoder().encode("hi \u{1F600}");
        console.log(enc.length, new TextDecoder().decode(enc));
        console.log(btoa("Man"), atob("TWFu"));
        try { new TextDecoder().decode(new Uint8Array([0xff]), undefined) } catch { console.log("nonfatal-ok") }
        console.log(new TextDecoder("utf-8", { fatal: true }).constructor.name);
        "#,
    );
    assert_eq!(out.lines(), ["7 hi \u{1F600}", "TWFu Man", "TextDecoder"]);
}

#[test]
fn web_url_and_search_params() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const u = new URL("http://user@ex.com/a/b?x=1&y=2#f");
        console.log(u.protocol, u.hostname, u.pathname, u.hash);
        console.log(u.searchParams.get("x"), u.searchParams.getAll("y").length);
        u.searchParams.set("x", "9");
        console.log(u.search);
        console.log(new URL("../c", "http://ex.com/a/b/").href);
        console.log(URL.canParse("nope"), URL.canParse("http://ok.com"));
        const sp = new URLSearchParams("a=1&a=2&b=3");
        console.log([...sp.keys()].join(","), sp.toString());
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "http: ex.com /a/b #f",
            "1 1",
            "?x=9&y=2",
            "http://ex.com/a/c",
            "false true",
            "a,a,b a=1&a=2&b=3",
        ]
    );
}

#[test]
fn web_response_status_defaults() {
    // An explicit `undefined` status/statusText counts as absent (WebIDL) and takes the default,
    // rather than coercing to `Number(undefined)` → NaN / `String(undefined)` → "undefined". This
    // is the path Hono's `c.json()` hits (its internal status is left undefined).
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(new Response("x").status);
        console.log(new Response("x", { status: undefined }).status);
        console.log(new Response("x", { status: 201 }).status);
        const r = new Response("x", { status: undefined, statusText: undefined });
        console.log(JSON.stringify(r.statusText), r.ok);
        "#,
    );
    assert_eq!(out.lines(), ["200", "200", "201", "\"\" true"]);
}

#[test]
fn web_readable_stream_body() {
    // `Response`/`Request` expose their buffered body as a `ReadableStream` via `.body`, and the
    // constructors accept a stream body — so `new Response(res.body, res)` (Hono's `c.header()`
    // rebuild) round-trips the payload instead of dropping it. Also covers reader reads, async
    // iteration, a user-authored stream as a body, and `null` for an empty body.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        (async () => {
            const a = new Response('{"x":1}', { headers: { "content-type": "application/json" } });
            const rebuilt = new Response(a.body, a);            // c.header() rebuild pattern
            console.log(await rebuilt.text());
            console.log(new Response("hi").body instanceof ReadableStream, new Response(null).body);
            const rd = new Response("hello").body.getReader();
            const c = await rd.read();
            console.log(new TextDecoder().decode(c.value), (await rd.read()).done);
            let acc = "";
            for await (const ch of new Response("abc").body) acc += new TextDecoder().decode(ch);
            console.log(acc);
            const us = new ReadableStream({ start(ctrl) { ctrl.enqueue(new TextEncoder().encode("strm")); ctrl.close(); } });
            console.log(await new Response(us).text());
        })();
        "#,
    );
    assert_eq!(
        out.lines(),
        ["{\"x\":1}", "true null", "hello true", "abc", "strm"]
    );
}

#[test]
fn web_events_and_abort() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const et = new EventTarget();
        let count = 0;
        const cb = (e) => { count += e.detail; };
        et.addEventListener("ping", cb);
        et.dispatchEvent(new CustomEvent("ping", { detail: 5 }));
        et.dispatchEvent(new CustomEvent("ping", { detail: 5 }));
        et.removeEventListener("ping", cb);
        et.dispatchEvent(new CustomEvent("ping", { detail: 5 }));
        console.log("count", count);

        let onceCount = 0;
        et.addEventListener("x", () => onceCount++, { once: true });
        et.dispatchEvent(new Event("x"));
        et.dispatchEvent(new Event("x"));
        console.log("once", onceCount);

        const ac = new AbortController();
        let aborted = false;
        ac.signal.addEventListener("abort", () => { aborted = true; });
        console.log("pre", ac.signal.aborted);
        ac.abort();
        console.log("post", ac.signal.aborted, aborted, ac.signal.reason.name);
        console.log("static", AbortSignal.abort().aborted);
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "count 10",
            "once 1",
            "pre false",
            "post true true AbortError",
            "static true"
        ]
    );
}

#[test]
fn web_structured_clone() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const orig = { a: [1, 2, { deep: true }], m: new Map([["k", 1]]), d: new Date(1000) };
        orig.self = orig;
        const c = structuredClone(orig);
        console.log(c !== orig, c.a[2].deep, c.m.get("k"), c.d.getTime());
        console.log(c.self === c, c.a !== orig.a);
        try { structuredClone(() => {}); } catch (e) { console.log("fn", e.name); }
        "#,
    );
    assert_eq!(
        out.lines(),
        ["true true 1 1000", "true true", "fn DataCloneError"]
    );
}

#[test]
fn web_crypto() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const a = new Uint8Array(16), b = new Uint8Array(16);
        crypto.getRandomValues(a); crypto.getRandomValues(b);
        // Astronomically unlikely to be equal: a real randomness source.
        console.log(a.some((v, i) => v !== b[i]));
        console.log(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()));
        try { crypto.getRandomValues(new Uint8Array(70000)); } catch (e) { console.log("quota", e.name); }
        crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc")).then((d) => {
            const hex = [...new Uint8Array(d)].map((x) => x.toString(16).padStart(2, "0")).join("");
            console.log(hex);
        });
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "true",
            "true",
            "quota QuotaExceededError",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ]
    );
}

#[test]
fn web_fetch_roundtrip_over_local_http() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    // One-shot server on a background thread: read the request, reply with a JSON body.
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = br#"{"ok":true,"n":42}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-test: yes\r\ncontent-length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(body);
    });

    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            (async () => {{
                const r = await fetch("http://{addr}/data");
                console.log(r.status, r.ok, r.headers.get("x-test"));
                const j = await r.json();
                console.log(j.ok, j.n);
            }})();
            "#,
        ),
    );
    server.join().ok();
    assert_eq!(out.lines(), ["200 true yes", "true 42"]);
}

// Cold-boot cost breakdown (realm intrinsics + per-extension install), for tracking the
// startup floor. `#[ignore]`d (timing, not correctness):
//   cargo test -p lumen-runtime perf_boot_breakdown --release -- --ignored --nocapture
#[test]
#[ignore]
fn perf_boot_breakdown() {
    use std::time::Instant;

    fn median(mut xs: Vec<f64>) -> f64 {
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs[xs.len() / 2]
    }
    // Time a closure's median over N runs, in microseconds.
    fn bench(n: usize, mut f: impl FnMut() -> f64) -> f64 {
        median((0..n).map(|_| f()).collect())
    }

    let engine_us = bench(30, || {
        let t = Instant::now();
        let _e = lumen_host::Engine::new();
        t.elapsed().as_secs_f64() * 1e6
    });
    println!("Engine::new (realm intrinsics)   {engine_us:8.1} us");

    // Per-extension install (state + ops + js_init parse+eval), in the real order.
    let names = ["timers", "console", "process", "fs", "web", "node"];
    let mut totals = vec![Vec::new(); names.len()];
    for _ in 0..30 {
        let (tx, _rx) = std::sync::mpsc::channel();
        let pool = ThreadPool::new(4, tx);
        let mut engine = lumen_host::Engine::new();
        engine.ctx().op_state().put(pool.handle());
        engine.ctx().op_state().put(TaskRegistry::default());
        let exts = [
            lumen_timers::extension(),
            console::extension(),
            process::extension(),
            lumen_fs::extension(),
            lumen_web::extension(),
            lumen_node::extension(),
        ];
        for (i, ext) in exts.into_iter().enumerate() {
            let t = Instant::now();
            install(&mut engine, std::slice::from_ref(&ext));
            totals[i].push(t.elapsed().as_secs_f64() * 1e6);
        }
    }
    let mut sum = 0.0;
    for (i, name) in names.iter().enumerate() {
        let m = median(std::mem::take(&mut totals[i]));
        sum += m;
        println!("install {name:<8}                 {m:8.1} us");
    }
    println!("---\nextensions total                 {sum:8.1} us");
}

#[test]
fn web_serve_roundtrip_over_loopback() {
    // A whole HTTP server + client on one loop: Lumen.serve binds, the same runtime fetches
    // itself through the loopback socket (server accept, client request, and response write all
    // run concurrently on the threadpool), then shutdown() lets the loop go idle so eval returns.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        (async () => {
            const server = Lumen.serve(async (req) => {
                const u = new URL(req.url);
                if (u.pathname === "/json") {
                    return Response.json({ ok: true, id: u.searchParams.get("id") });
                }
                if (req.method === "POST") return new Response("got:" + (await req.text()));
                return new Response("pong\n", { headers: { "x-cta": "hi" } });
            }, { hostname: "127.0.0.1", port: 0 });

            const base = `http://127.0.0.1:${server.port}`;
            const r1 = await fetch(base + "/");
            console.log(r1.status, r1.headers.get("x-cta"), (await r1.text()).trim());

            const r2 = await fetch(base + "/json?id=42");
            const j = await r2.json();
            console.log(r2.status, j.ok, j.id);

            const r3 = await fetch(base + "/echo", { method: "POST", body: "hey" });
            console.log(r3.status, await r3.text());

            await server.shutdown();
            console.log("closed");
        })();
        "#,
    );
    assert_eq!(
        out.lines(),
        ["200 hi pong", "200 true 42", "200 got:hey", "closed"]
    );
}

// ---- lumen-node (node: compat; the runtime assembles it) ----

#[test]
fn node_path_and_os_builtins() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const path = require("node:path");
        console.log(path.join("a", "b", "..", "c"), path.extname("x.tar.gz"), path.basename("/p/q.js", ".js"));
        console.log(path.dirname("/a/b/c"), path.isAbsolute("/x"), path.isAbsolute("x"));
        const os = require("os");
        console.log(typeof os.platform(), typeof os.homedir(), os.EOL === "\n" || os.EOL === "\r\n");
        console.log(require("path") === require("node:path"));
        "#,
    );
    assert_eq!(
        out.lines(),
        ["a/c .gz q", "/a/b true false", "string string true", "true",]
    );
}

#[test]
fn node_buffer() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(Buffer.from("hé").length, Buffer.from("hé").toString("hex"));
        console.log(Buffer.from("6869", "hex").toString());
        console.log(Buffer.from("aGVsbG8=", "base64").toString());
        console.log(Buffer.alloc(3, 7).toString("hex"));
        console.log(Buffer.concat([Buffer.from("ab"), Buffer.from("cd")]).toString());
        console.log(Buffer.isBuffer(Buffer.from("x")), Buffer.isBuffer(new Uint8Array(1)));
        console.log(Buffer.from("abc").equals(Buffer.from("abc")), Buffer.from("abc").compare(Buffer.from("abd")));
        const b = Buffer.alloc(4); b.writeUInt32LE(0x01020304, 0);
        console.log(b.toString("hex"), b.readUInt32BE(0).toString(16));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "3 68c3a9",
            "hi",
            "hello",
            "070707",
            "abcd",
            "true false",
            "true -1",
            "04030201 4030201",
        ]
    );
}

#[test]
fn node_require_resolution() {
    use std::fs;
    let dir = TempDir::new("require");
    let root = dir.0.clone();
    fs::write(
        root.join("lib.js"),
        "module.exports = { v: require('./data.json').v + 1 };",
    )
    .unwrap();
    fs::write(root.join("data.json"), r#"{ "v": 41 }"#).unwrap();
    let pkg = root.join("node_modules").join("widget");
    fs::create_dir_all(pkg.join("lib")).unwrap();
    fs::write(
        pkg.join("package.json"),
        r#"{ "name": "widget", "main": "lib/main.js" }"#,
    )
    .unwrap();
    fs::write(
        pkg.join("lib").join("main.js"),
        "module.exports = () => 'widget-ok';",
    )
    .unwrap();
    fs::write(
        root.join("entry.js"),
        r#"
        const lib = require("./lib");
        const widget = require("widget");
        console.log(lib.v, widget());
        console.log(require.resolve("./lib").endsWith("lib.js"));
        // cache: requiring twice yields the same exports object
        console.log(require("./lib") === require("./lib"));
        "#,
    )
    .unwrap();

    let (mut rt, out, _err) = test_runtime();
    let entry = root.join("entry.js").to_string_lossy().into_owned();
    rt.run_main(&entry).expect("main runs");
    assert_eq!(out.lines(), ["42 widget-ok", "true", "true"]);
}

#[test]
fn node_run_main_dirname_and_module() {
    use std::fs;
    let dir = TempDir::new("main");
    let entry = dir.0.join("prog.js");
    fs::write(
        &entry,
        r#"
        const path = require("node:path");
        console.log(__filename.endsWith("prog.js"));
        console.log(__dirname === path.dirname(__filename));
        console.log(require.main.filename === __filename);
        module.exports = { ran: true };
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_main(&entry.to_string_lossy()).expect("runs");
    assert_eq!(out.lines(), ["true", "true", "true"]);
}

#[test]
fn node_fs_module_over_global_fs() {
    use std::fs;
    let dir = TempDir::new("nodefs");
    let target = dir.path("f.txt");
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const nfs = require("node:fs");
            nfs.writeFileSync({target:?}, "node fs content");
            console.log(nfs.readFileSync({target:?}, "utf8"));
            console.log(nfs.readFileSync({target:?}) instanceof Buffer);
            console.log(nfs.existsSync({target:?}), nfs.statSync({target:?}).isFile());
            "#,
        ),
    );
    let _ = fs::remove_file(&target);
    assert_eq!(out.lines(), ["node fs content", "true", "true true"]);
}

#[test]
fn node_require_missing_module_throws() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        try { require("./does-not-exist"); }
        catch (e) { console.log(e.code, e.message.includes("Cannot find module")); }
        "#,
    );
    assert_eq!(out.lines(), ["MODULE_NOT_FOUND true"]);
}

// ---- ESM (run_module: the import graph resolves against disk + node_modules) ----

#[test]
fn esm_named_default_json_and_dynamic_import() {
    use std::fs;
    let dir = TempDir::new("esm-basic");
    let root = dir.0.clone();
    fs::write(
        root.join("util.mjs"),
        "export const double = (n) => n * 2;\nexport default { tag: 'd' };",
    )
    .unwrap();
    fs::write(root.join("data.json"), r#"{ "answer": 42 }"#).unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import def, { double } from "./util.mjs";
        import data from "./data.json";
        console.log(double(21), def.tag, data.answer);
        const dyn = await import("./util.mjs");
        console.log(dyn.double(4));
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(out.lines(), ["42 d 42", "8"]);
}

#[test]
fn esm_imports_node_builtins_named_and_default() {
    use std::fs;
    let dir = TempDir::new("esm-builtin");
    let entry = dir.0.join("b.mjs");
    fs::write(
        &entry,
        r#"
        import { readFileSync, writeFileSync } from "node:fs";
        import path, { join } from "node:path";
        import os from "os";
        console.log(typeof readFileSync, typeof writeFileSync);
        console.log(path.basename("/a/b.js"), join("x", "y"));
        console.log(typeof os.platform());
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&entry.to_string_lossy()).expect("runs");
    assert_eq!(out.lines(), ["function function", "b.js x/y", "string"]);
}

#[test]
fn esm_resolves_node_modules_packages() {
    use std::fs;
    let dir = TempDir::new("esm-pkg");
    let root = dir.0.clone();
    let esm = root.join("node_modules").join("esmpkg");
    fs::create_dir_all(&esm).unwrap();
    fs::write(
        esm.join("package.json"),
        r#"{ "name":"esmpkg", "type":"module", "main":"index.js" }"#,
    )
    .unwrap();
    fs::write(esm.join("index.js"), "export const from = 'esm-pkg';").unwrap();
    let cjs = root.join("node_modules").join("cjspkg");
    fs::create_dir_all(&cjs).unwrap();
    fs::write(
        cjs.join("package.json"),
        r#"{ "name":"cjspkg", "main":"index.js" }"#,
    )
    .unwrap();
    fs::write(
        cjs.join("index.js"),
        "module.exports = { from: 'cjs-pkg' };",
    )
    .unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import { from as e } from "esmpkg";
        import cjs from "cjspkg";
        console.log(e, cjs.from);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("runs");
    assert_eq!(out.lines(), ["esm-pkg cjs-pkg"]);
}

#[test]
fn esm_prefers_exports_import_over_cjs_main() {
    // A package shaped like hono: `main` is a CJS build, but `type:module` + the exports `import`
    // condition point at an ESM build with real named exports. The bare import must resolve the
    // ESM entry (named `Hono` works), not fall through to `main` (CJS, default-only).
    use std::fs;
    let dir = TempDir::new("esm-exports");
    let root = dir.0.clone();
    let pkg = root.join("node_modules").join("dual");
    fs::create_dir_all(pkg.join("dist").join("cjs")).unwrap();
    fs::write(
        pkg.join("package.json"),
        r#"{
            "name": "dual",
            "main": "dist/cjs/index.js",
            "type": "module",
            "module": "dist/index.js",
            "exports": { ".": {
                "import": "./dist/index.js",
                "require": "./dist/cjs/index.js"
            } }
        }"#,
    )
    .unwrap();
    fs::write(
        pkg.join("dist").join("index.js"),
        "export class Widget { hi() { return 'esm'; } }\nexport const kind = 'named';",
    )
    .unwrap();
    // If resolution wrongly picked this CJS build, loading it as ESM would fail (`module` is not
    // defined) or expose no named `Widget`.
    fs::write(
        pkg.join("dist").join("cjs").join("index.js"),
        "module.exports = { Widget: null, kind: 'cjs' };",
    )
    .unwrap();
    // A bare *subpath* import of a `.js` file must inherit the package's `type:module`.
    fs::write(
        pkg.join("dist").join("named.js"),
        "export const sub = 'subpath-esm';",
    )
    .unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import { Widget, kind } from "dual";
        import { sub } from "dual/dist/named.js";
        console.log(new Widget().hi(), kind, sub);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("runs");
    assert_eq!(out.lines(), ["esm named subpath-esm"]);
}

#[test]
fn esm_top_level_await_on_timer_settles() {
    use std::fs;
    let dir = TempDir::new("esm-tla");
    let entry = dir.0.join("tla.mjs");
    fs::write(
        &entry,
        r#"
        const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
        console.log("before");
        await sleep(10);
        console.log("after");
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&entry.to_string_lossy()).expect("runs");
    assert_eq!(out.lines(), ["before", "after"]);
}

#[test]
fn esm_module_not_found_is_an_error() {
    use std::fs;
    let dir = TempDir::new("esm-missing");
    let entry = dir.0.join("bad.mjs");
    fs::write(&entry, "import x from './nope.mjs';\n").unwrap();
    let (mut rt, _out, _err) = test_runtime();
    let err = rt.run_module(&entry.to_string_lossy()).unwrap_err();
    assert!(err.contains("not found") || err.contains("nope"), "{err}");
}
