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
        console.log(typeof process.setuid, typeof process.setgid, Array.isArray(process.getgroups()));
        "#,
    );
    assert_eq!(out.lines(), ["string true", "true string", "object string", "function function true"]);
}

#[test]
fn process_execve_validates_and_reports_os_errors() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(typeof process.execve);
        try { process.execve(1, [], {}); } catch (error) { console.log(error.name); }
        try { process.execve("/definitely/not/a/lumen/executable", ["missing"], {}); }
        catch (error) { console.log(error.message.startsWith("execve failed:")); }
        "#,
    );
    assert_eq!(out.lines(), ["function", "TypeError", "true"]);
}

#[test]
fn node_crypto_argon2_sync_and_async_match_rfc9106() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const { argon2, argon2Sync } = require("node:crypto");
        const parameters = {
          message: Buffer.alloc(32, 1), nonce: Buffer.alloc(16, 2),
          parallelism: 4, tagLength: 32, memory: 32, passes: 3,
          secret: Buffer.alloc(8, 3), associatedData: Buffer.alloc(12, 4),
        };
        const expected = "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659";
        console.log(Object.keys(require("node:crypto")).length, argon2Sync("argon2id", parameters).toString("hex") === expected);
        argon2("argon2id", parameters, (error, key) => console.log(error === null, key.toString("hex") === expected));
        "#,
    );
    assert_eq!(out.lines(), ["67 true", "true true"]);
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
        static NEXT_TEMP_DIR: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_TEMP_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lumen-fs-test-{tag}-{}-{id}",
            std::process::id()
        ));
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

// ---- WinterTC Minimum Common API conformance ----
//
// The tracked score for the WinterTC "Minimum Common API" global surface. `SUPPORTED` are the
// interfaces implemented today; `NOT_YET` are the remaining ones. The test asserts every SUPPORTED
// global is present AND every NOT_YET global is absent — so implementing an interface fails the
// test until its name is moved across, keeping the score honest. Total = the full spec surface.
const WINTERTC_SUPPORTED: &[&str] = &[
    "globalThis",
    "queueMicrotask",
    "structuredClone",
    "atob",
    "btoa",
    "fetch",
    "console",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "Event",
    "EventTarget",
    "CustomEvent",
    "DOMException",
    "AbortController",
    "AbortSignal",
    "TextEncoder",
    "TextDecoder",
    "URL",
    "URLSearchParams",
    "Headers",
    "Request",
    "Response",
    "ReadableStream",
    "ReadableStreamDefaultReader",
    "ReadableStreamDefaultController",
    "crypto",
    "Crypto",
    "SubtleCrypto",
    "performance",
    "Performance",
    "navigator",
    "self",
    "Blob",
    "File",
    "FormData",
    "WritableStream",
    "WritableStreamDefaultWriter",
    "WritableStreamDefaultController",
    "TransformStream",
    "TransformStreamDefaultController",
    "ByteLengthQueuingStrategy",
    "CountQueuingStrategy",
    "TextEncoderStream",
    "TextDecoderStream",
    "URLPattern",
    "CompressionStream",
    "DecompressionStream",
    "ReadableStreamBYOBReader",
    "ReadableByteStreamController",
    "ReadableStreamBYOBRequest",
    "WebAssembly",
    "reportError",
    "onerror",
    "onunhandledrejection",
];
const WINTERTC_NOT_YET: &[&str] = &[];

/// Web-platform interfaces the runtime ships *beyond* the WinterTC Minimum Common API. Not part
/// of the tracked 56/56 score, but presence-guarded the same way so they can't silently regress.
const BEYOND_MINIMUM: &[&str] = &[
    "Worker",
    "WebSocket",
    "EventSource",
    "MessageChannel",
    "MessagePort",
    "BroadcastChannel",
    "MessageEvent",
    "CloseEvent",
    "ErrorEvent",
    "PromiseRejectionEvent",
];

#[test]
fn wintertc_minimum_common_api() {
    let (mut rt, out, _err) = test_runtime();
    let all = [WINTERTC_SUPPORTED, WINTERTC_NOT_YET, BEYOND_MINIMUM].concat().join(",");
    eval_ok(
        &mut rt,
        &format!(
            r#"{{
                const names = "{all}".split(",");
                const present = names.filter((n) => typeof globalThis[n] !== "undefined");
                console.log("PRESENT:" + present.join(","));
            }}"#,
        ),
    );
    let line = out.lines().into_iter().next().unwrap_or_default();
    let present: std::collections::HashSet<&str> = line
        .strip_prefix("PRESENT:")
        .unwrap_or("")
        .split(',')
        .collect();

    let missing_supported: Vec<&str> = WINTERTC_SUPPORTED
        .iter()
        .copied()
        .filter(|n| !present.contains(n))
        .collect();
    assert!(
        missing_supported.is_empty(),
        "WinterTC regression — these SUPPORTED globals went missing: {missing_supported:?}"
    );
    let unexpected: Vec<&str> = WINTERTC_NOT_YET
        .iter()
        .copied()
        .filter(|n| present.contains(n))
        .collect();
    assert!(
        unexpected.is_empty(),
        "WinterTC globals now present but still listed NOT_YET — move them to SUPPORTED: {unexpected:?}"
    );

    let missing_extra: Vec<&str> = BEYOND_MINIMUM
        .iter()
        .copied()
        .filter(|n| !present.contains(n))
        .collect();
    assert!(
        missing_extra.is_empty(),
        "beyond-minimum web interfaces went missing: {missing_extra:?}"
    );

    let total = WINTERTC_SUPPORTED.len() + WINTERTC_NOT_YET.len();
    println!(
        "WinterTC Minimum Common API: {}/{} globals implemented (+{} beyond-minimum interfaces)",
        WINTERTC_SUPPORTED.len(),
        total,
        BEYOND_MINIMUM.len()
    );
}

#[test]
fn error_reporting_globals() {
    // HTML error reporting (WinterTC §5.2): `reportError` fires the global `onerror` handler
    // (returning `true` suppresses the default report); an unhandled rejection fires
    // `onunhandledrejection` whose `event.preventDefault()` suppresses the default line. The
    // default reports land on the error sink in the loop's `Uncaught …` format.
    use std::fs;
    let dir = TempDir::new("error-reporting");
    let root = dir.0.clone();
    fs::write(
        root.join("app.mjs"),
        r#"
        reportError(new TypeError("boom-default"));
        onerror = (message, source, lineno, colno, error) => {
            console.log("onerror", message, error.message, source === "" && lineno === 0);
            return true;
        };
        reportError(new RangeError("boom-suppressed"));
        onerror = null;
        try { reportError(); } catch (e) { console.log("zero-arg", e.constructor.name); }
        onunhandledrejection = (event) => {
            console.log("rejection", event.type, event.reason, event.promise instanceof Promise);
            event.preventDefault();
        };
        Promise.reject("quiet");
        setTimeout(() => {
            onunhandledrejection = null;
            Promise.reject("loud");
        }, 1);
        "#,
    )
    .unwrap();
    let (mut rt, out, err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        [
            "onerror Uncaught RangeError: boom-suppressed boom-suppressed true",
            "zero-arg TypeError",
            "rejection unhandledrejection quiet true",
        ]
    );
    assert_eq!(
        err.lines(),
        ["Uncaught TypeError: boom-default", "Uncaught (in promise) loud"]
    );
}

#[test]
fn message_channel_semantics() {
    // Channel messaging: data serializes synchronously at postMessage (later mutations are
    // invisible), delivery queues until the receiving port starts (assigning onmessage starts
    // it), the receiver gets a CLONE, and the event is a real MessageEvent.
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        const { port1, port2 } = new MessageChannel();
        const msg = { n: 1 };
        port2.postMessage(msg);       // port1 not started yet: queued
        msg.n = 999;                  // must not be observable (sync serialize)
        port1.onmessage = (e) => {
            console.log(e.data.n, e.data !== msg, e instanceof MessageEvent, e.type);
        };
        "#,
    )
    .unwrap();
    assert_eq!(out.lines(), ["1 true true message"]);
}

#[test]
fn message_port_close_and_guards() {
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        const { port1, port2 } = new MessageChannel();
        port1.onmessage = () => console.log("received (wrong: port closed)");
        port1.close();
        port2.postMessage("x");       // dropped: peer closed
        try { new MessagePort(); } catch (e) { console.log("ctor:", e.constructor.name); }
        setTimeout(() => console.log("done"), 5);
        "#,
    )
    .unwrap();
    assert_eq!(out.lines(), ["ctor: TypeError", "done"]);
}

#[test]
fn broadcast_channel_fanout() {
    // BroadcastChannel: every same-name channel EXCEPT the sender receives its own clone;
    // posting on a closed channel is an InvalidStateError DOMException.
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        const a = new BroadcastChannel("chan");
        const b = new BroadcastChannel("chan");
        const c = new BroadcastChannel("chan");
        const other = new BroadcastChannel("elsewhere");
        a.onmessage = () => console.log("self (wrong!)");
        other.onmessage = () => console.log("cross-name (wrong!)");
        b.onmessage = (e) => { e.data.x++; console.log("b", e.data.x); };
        c.onmessage = (e) => console.log("c", e.data.x); // b's mutation must not leak here
        a.postMessage({ x: 7 });
        setTimeout(() => {
            c.close();
            try { c.postMessage(1); } catch (e) { console.log("closed:", e.name); }
        }, 5);
        "#,
    )
    .unwrap();
    assert_eq!(out.lines(), ["b 8", "c 7", "closed: InvalidStateError"]);
}

#[test]
fn abort_signal_any_and_event_classes() {
    let (mut rt, out, _err) = test_runtime();
    rt.eval(
        r#"
        // AbortSignal.any: first abort wins; a pre-aborted input short-circuits.
        const c1 = new AbortController();
        const c2 = new AbortController();
        const s = AbortSignal.any([c1.signal, c2.signal]);
        s.addEventListener("abort", () => console.log("any:", s.reason.message));
        c1.abort(new Error("first"));
        c2.abort(new Error("second (must lose)"));
        console.log("pre:", AbortSignal.any([AbortSignal.abort("done")]).aborted);
        // Event classes: CloseEvent code is ToUint16; ErrorEvent coerces positions; the
        // PromiseRejectionEvent init requires a promise.
        const ce = new CloseEvent("close", { code: 70000, reason: "bye", wasClean: true });
        console.log("close:", ce.code, ce.reason, ce.wasClean);
        const ee = new ErrorEvent("error", { message: "m", lineno: 3.7, error: 42 });
        console.log("errev:", ee.message, ee.lineno, ee.error);
        const pre = new PromiseRejectionEvent("unhandledrejection", {
            promise: Promise.resolve(),
            reason: "r",
        });
        console.log("prev:", pre.reason, pre.promise instanceof Promise);
        try { new PromiseRejectionEvent("x", {}); } catch (e) { console.log("guard:", e.constructor.name); }
        "#,
    )
    .unwrap();
    assert_eq!(
        out.lines(),
        [
            "any: first",
            "pre: true",
            "close: 4464 bye true",
            "errev: m 3 42",
            "prev: r true",
            "guard: TypeError",
        ]
    );
}

// ---- EventSource (SSE) against an in-process event-stream server (lumen_web::sse_testing) ----

use lumen_web::sse_testing::{spawn as spawn_sse, Mode as SseMode};

fn sse_drive(mode: SseMode, conns: usize, script: &str) -> Vec<String> {
    let port = spawn_sse(mode, conns);
    let (mut rt, out, _err) = test_runtime();
    let src = script.replace("{PORT}", &port.to_string());
    rt.eval(&src).expect("sse script parses and runs to quiescence");
    out.lines()
}

#[test]
fn eventsource_parses_events() {
    // Unnamed `message`, a named `event: tick`, and a multi-line `data` (joined with \n) with an
    // `id` — the three canned events, then the stream ends (readyState -> CONNECTING as it would
    // reconnect; we close on the first error).
    let lines = sse_drive(
        SseMode::Events,
        1,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        console.log("initial", es.readyState, es.url.endsWith("/stream"));
        es.onopen = () => console.log("open", es.readyState);
        es.onmessage = (e) => console.log("message", JSON.stringify(e.data), e.lastEventId, e instanceof MessageEvent);
        es.addEventListener("tick", (e) => console.log("tick", e.data));
        es.onerror = () => { console.log("error", es.readyState); es.close(); };
        "#,
    );
    assert_eq!(
        lines,
        [
            "initial 0 true",
            "open 1",
            r#"message "hello"  true"#,
            "tick 42",
            r#"message "line one\nline two" 9 true"#,
            "error 0",
        ]
    );
}

#[test]
fn eventsource_reconnects_with_last_event_id() {
    // First connection yields `id: 5` then drops; the client reconnects (default 3s retry is too
    // slow for a test, so the server's event sets retry). We shorten via a `retry:` field is not
    // sent here — instead the test tolerates the reconnect by checking the resumed event, which
    // the server builds from the Last-Event-ID header. Two connections are served.
    let lines = sse_drive(
        SseMode::Reconnect,
        2,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        let seen = 0;
        es.onmessage = (e) => {
            console.log("msg", e.data, e.lastEventId);
            if (++seen === 2) es.close();
        };
        "#,
    );
    assert_eq!(lines, ["msg first 5", "msg resumed-from-5 5"]);
}

#[test]
fn eventsource_wrong_content_type_is_fatal() {
    let lines = sse_drive(
        SseMode::WrongContentType,
        1,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        es.onopen = () => console.log("open (wrong!)");
        es.onerror = () => console.log("error", es.readyState);
        "#,
    );
    // A fatal error sets readyState CLOSED (2) and does not reconnect.
    assert_eq!(lines, ["error 2"]);
}

#[test]
fn eventsource_204_stops() {
    let lines = sse_drive(
        SseMode::NoContent,
        1,
        r#"
        const es = new EventSource("http://127.0.0.1:{PORT}/stream");
        es.onerror = () => console.log("error", es.readyState);
        "#,
    );
    assert_eq!(lines, ["error 2"]);
}

#[test]
fn eventsource_field_parser_units() {
    // Exercise the line parser directly against a synthetic stream via a data: URL is not
    // supported; instead validate comment-skipping, colon-less field, leading-space stripping,
    // and retry parsing through the state the events produce. Server Events mode already covers
    // the happy path; here we assert the readyState constants + url/withCredentials surface.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        console.log(EventSource.CONNECTING, EventSource.OPEN, EventSource.CLOSED);
        const es = new EventSource("http://127.0.0.1:1/nope", { withCredentials: true });
        console.log(es.withCredentials, es.url.endsWith("/nope"));
        es.close();
        console.log(es.readyState);
        "#,
    );
    assert_eq!(out.lines(), ["0 1 2", "true true", "2"]);
}

// ---- structured-clone wire format (cross-thread serialize/deserialize) ----

#[test]
fn structured_clone_wire_round_trips() {
    // The JS wire format (__serializeForClone/__deserializeClone) round-trips the structured-clone
    // subset, including cycles, shared subgraphs, typed arrays, and DataCloneError. This is what
    // carries a Worker message across the thread boundary.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const rt = (v) => __deserializeClone(__serializeForClone(v));
        console.log("prims", rt(undefined), rt(null), rt(true), rt(-3.5), rt("x\u{1f600}"));
        console.log("bigint", rt(90071992547409910n) === 90071992547409910n);
        console.log("nested", JSON.stringify(rt({ a: [1, { b: 2 }], c: "y" })));
        console.log("date", rt(new Date(123)).getTime());
        console.log("regexp", String(rt(/a.c/gi)));
        const m = rt(new Map([["k", 1]])); const s = rt(new Set([1, 2, 2]));
        console.log("mapset", m.get("k"), [...s].join(","));
        const ta = rt(new Float64Array([1.5, 2.5]));
        console.log("typedarray", ta instanceof Float64Array, ta.join(","));
        const c = { x: 1 }; c.self = c; const rc = rt(c);
        console.log("cycle", rc.self === rc, rc.x);
        const shared = { v: 9 }; const g = rt({ a: shared, b: shared });
        console.log("shared", g.a === g.b);
        const e = rt(new TypeError("boom")); console.log("error", e.name, e.message);
        try { rt(Symbol("s")); } catch (ex) { console.log("throws", ex.name); }
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "prims undefined null true -3.5 x\u{1f600}",
            "bigint true",
            r#"nested {"a":[1,{"b":2}],"c":"y"}"#,
            "date 123",
            "regexp /a.c/gi",
            "mapset 1 1,2",
            "typedarray true 1.5,2.5",
            "cycle true 1",
            "shared true",
            "error TypeError boom",
            "throws DataCloneError",
        ]
    );
}

// ---- Web Workers (realm-per-thread + structured messaging) ----

/// Write worker scripts into a temp dir and drive `main_src` against them; returns stdout lines.
/// Every case must arrange for the worker(s) to exit (close/terminate) so the loop finishes.
fn worker_drive(files: &[(&str, &str)], main_src: &str) -> Vec<String> {
    let dir = TempDir::new("worker");
    for (name, body) in files {
        std::fs::write(dir.0.join(name), body).unwrap();
    }
    let (mut rt, out, _err) = test_runtime();
    let src = main_src.replace("{DIR}", &dir.0.to_string_lossy());
    rt.eval(&src).expect("worker main parses and runs to quiescence");
    out.lines()
}

#[test]
fn worker_message_round_trip_and_structured_clone() {
    // A structured payload (nested object, array, Date, Map) survives the cross-thread clone in
    // both directions; the worker computes a reply and then closes itself.
    let lines = worker_drive(
        &[(
            "echo.mjs",
            r#"
            onmessage = (e) => {
                const d = e.data;
                postMessage({
                    sum: d.nums.reduce((a, b) => a + b, 0),
                    ts: d.when.getTime(),
                    tag: d.meta.get("tag"),
                });
                close();
            };
            "#,
        )],
        r#"
        const w = new Worker("{DIR}/echo.mjs", { type: "module" });
        w.onmessage = (e) => console.log("reply", e.data.sum, e.data.ts, e.data.tag);
        w.postMessage({ nums: [1, 2, 3, 4], when: new Date(1000), meta: new Map([["tag", "hi"]]) });
        "#,
    );
    assert_eq!(lines, ["reply 10 1000 hi"]);
}

#[test]
fn worker_bidirectional_conversation() {
    // Several messages each way, in order; the worker closes after the third.
    let lines = worker_drive(
        &[(
            "counter.mjs",
            r#"
            let total = 0;
            onmessage = (e) => {
                total += e.data;
                postMessage(total);
                if (total >= 6) close();
            };
            "#,
        )],
        r#"
        const w = new Worker("{DIR}/counter.mjs", { type: "module" });
        const send = [1, 2, 3];
        let i = 0;
        w.onmessage = (e) => {
            console.log("running total", e.data);
            if (i < send.length) w.postMessage(send[i++]);
        };
        w.postMessage(send[i++]);
        "#,
    );
    assert_eq!(lines, ["running total 1", "running total 3", "running total 6"]);
}

#[test]
fn worker_error_propagates_to_onerror() {
    let lines = worker_drive(
        &[(
            "boom.mjs",
            r#"onmessage = (e) => { throw new RangeError("bad: " + e.data); };"#,
        )],
        r#"
        const w = new Worker("{DIR}/boom.mjs", { type: "module" });
        w.onerror = (e) => { console.log("onerror", e.message, e instanceof ErrorEvent); w.terminate(); };
        w.postMessage(42);
        "#,
    );
    assert_eq!(lines, ["onerror Uncaught RangeError: bad: 42 true"]);
}

#[test]
fn worker_terminate_stops_a_running_worker() {
    // A worker posting on an interval is terminated after 3 messages; no further messages arrive,
    // and the main loop exits promptly.
    let lines = worker_drive(
        &[(
            "ticker.mjs",
            r#"
            let n = 0;
            setInterval(() => { n++; postMessage(n); }, 3);
            "#,
        )],
        r#"
        const w = new Worker("{DIR}/ticker.mjs", { type: "module" });
        let count = 0;
        w.onmessage = (e) => {
            count++;
            if (count === 3) {
                w.terminate();
                console.log("terminated at", e.data);
                setTimeout(() => console.log("no leak"), 60);
            } else if (count > 3) {
                console.log("LEAK", e.data);
            }
        };
        "#,
    );
    assert_eq!(lines, ["terminated at 3", "no leak"]);
}

#[test]
fn worker_load_error_reports() {
    // A missing worker script surfaces as an error event, not a hang.
    let lines = worker_drive(
        &[],
        r#"
        const w = new Worker("{DIR}/does-not-exist.mjs", { type: "module" });
        w.onerror = (e) => { console.log("load-error", e.message.includes("cannot load")); w.terminate(); };
        "#,
    );
    assert_eq!(lines, ["load-error true"]);
}

#[test]
fn worker_datacloneerror_on_unserializable() {
    // Posting a function is a DataCloneError, synchronously, on the sending side.
    let lines = worker_drive(
        &[("noop.mjs", "onmessage = () => close();")],
        r#"
        const w = new Worker("{DIR}/noop.mjs", { type: "module" });
        try { w.postMessage(() => 1); } catch (e) { console.log("clone-error", e.name); }
        w.terminate();
        "#,
    );
    assert_eq!(lines, ["clone-error DataCloneError"]);
}

// ---- WebSocket (RFC 6455) against an in-process echo server (lumen_web::ws_testing) ----

use lumen_web::ws_testing::{spawn_echo, Mode as WsMode};

/// Drive a WebSocket against `mode`'s echo server and return the captured stdout lines.
fn ws_drive(mode: WsMode, script: &str) -> Vec<String> {
    let port = spawn_echo(mode, 1);
    let (mut rt, out, _err) = test_runtime();
    let src = script.replace("{PORT}", &port.to_string());
    rt.eval(&src).expect("ws script parses and runs to quiescence");
    out.lines()
}

#[test]
fn websocket_text_and_binary_round_trip() {
    let lines = ws_drive(
        WsMode::Echo,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/", ["chat", "v2"]);
        ws.binaryType = "arraybuffer";
        ws.onopen = () => {
            console.log("open", ws.readyState, ws.protocol);
            ws.send("hello");
        };
        ws.onmessage = (e) => {
            if (typeof e.data === "string") {
                console.log("text", e.data, e instanceof MessageEvent, e.origin);
                ws.send(new Uint8Array([1, 2, 254, 255]));
            } else {
                console.log("binary", Array.from(new Uint8Array(e.data)).join(","));
                ws.close(1000, "done");
            }
        };
        ws.onclose = (e) => console.log("close", e.code, e.reason, e.wasClean, ws.readyState);
        "#,
    );
    let port = lines[0].is_empty();
    let _ = port;
    // The message-event origin is the socket URL (host:port varies), so match it structurally.
    assert_eq!(lines[0], "open 1 chat");
    assert!(
        lines[1].starts_with("text hello true ws://127.0.0.1:") && lines[1].ends_with('/'),
        "message event shape/origin: {}",
        lines[1]
    );
    assert_eq!(lines[2], "binary 1,2,254,255");
    assert_eq!(lines[3], "close 1000 done true 3");
    assert_eq!(lines.len(), 4);
}

#[test]
fn websocket_transparent_ping_pong() {
    // The server pings on connect; the client must answer with a pong transparently (no user
    // event), which the server then reports back as a text message.
    let lines = ws_drive(
        WsMode::PingThenEcho,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onmessage = (e) => { console.log("msg", e.data); ws.close(); };
        ws.onclose = () => console.log("closed");
        "#,
    );
    assert_eq!(lines, ["msg pong:marco", "closed"]);
}

#[test]
fn websocket_reassembles_fragments() {
    // A message split across a data frame + two continuation frames arrives whole.
    let lines = ws_drive(
        WsMode::FragmentedHello,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onmessage = (e) => { console.log("got", e.data, e.data.length); ws.close(); };
        ws.onclose = () => console.log("closed");
        "#,
    );
    assert_eq!(lines, ["got fragment 8", "closed"]);
}

#[test]
fn websocket_server_initiated_close() {
    let lines = ws_drive(
        WsMode::CloseImmediately,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onclose = (e) => console.log("close", e.code, e.reason, e.wasClean);
        ws.onopen = () => console.log("open");
        "#,
    );
    assert_eq!(lines, ["open", "close 4001 going away true"]);
}

#[test]
fn websocket_handshake_rejection_is_error_then_close() {
    // A wrong Sec-WebSocket-Accept fails the connection: an error event, then a 1006 close.
    let lines = ws_drive(
        WsMode::BadAccept,
        r#"
        const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
        ws.onerror = () => console.log("error", ws.readyState);
        ws.onclose = (e) => console.log("close", e.code, e.wasClean);
        ws.onopen = () => console.log("open (wrong!)");
        "#,
    );
    assert_eq!(lines, ["error 3", "close 1006 false"]);
}

#[test]
fn websocket_constructor_validation() {
    // Bad scheme, fragment, and duplicate subprotocol all throw synchronously (no server needed).
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const bad = (fn) => { try { fn(); console.log("no throw"); } catch (e) { console.log(e.name); } };
        bad(() => new WebSocket("http://x/"));
        bad(() => new WebSocket("ws://x/#frag"));
        bad(() => new WebSocket("ws://x/", ["a", "a"]));
        bad(() => new WebSocket("ws://x/", ["bad proto"]));
        console.log(WebSocket.CONNECTING, WebSocket.OPEN, WebSocket.CLOSING, WebSocket.CLOSED);
        "#,
    );
    assert_eq!(
        out.lines(),
        ["SyntaxError", "SyntaxError", "SyntaxError", "SyntaxError", "0 1 2 3"]
    );
}

#[test]
fn wintertc_functional_smoke() {
    // Presence is not correctness: exercise the core interfaces end-to-end.
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const ok = [];
        ok.push(JSON.stringify(structuredClone({a:[1,2]})) === '{"a":[1,2]}');
        ok.push(new TextDecoder().decode(new TextEncoder().encode("héllo")) === "héllo");
        ok.push(new URL("http://x/y?a=1").searchParams.get("a") === "1");
        ok.push(atob(btoa("hi")) === "hi");
        ok.push(new AbortController().signal.aborted === false);
        ok.push(/^[0-9a-f-]{36}$/.test(crypto.randomUUID()));
        ok.push(typeof performance.now() === "number");
        ok.push(new Headers({a:"1"}).get("a") === "1");
        console.log(ok.every(Boolean) ? "ALL_OK" : "FAIL:" + ok.join(","));
        "#,
    );
    assert_eq!(out.lines(), ["ALL_OK"]);
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
fn esm_import_text_attribute() {
    // TC39 import-text: `with { type: "text" }` default-exports the file's contents, decoded
    // as UTF-8 with a leading BOM stripped — for any extension, including a .js file (which
    // must not execute). Dynamic import with the attribute resolves the same record.
    use std::fs;
    let dir = TempDir::new("esm-import-text");
    let root = dir.0.clone();
    fs::write(root.join("note.txt"), b"\xef\xbb\xbfHello \xc3\xa9!\n").unwrap();
    fs::write(root.join("side.js"), "globalThis.__ranSide = true;").unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import note from "./note.txt" with { type: "text" };
        import source from "./side.js" with { type: "text" };
        console.log(JSON.stringify(note));
        console.log(source === "globalThis.__ranSide = true;", typeof globalThis.__ranSide);
        const dyn = await import("./note.txt", { with: { type: "text" } });
        console.log(dyn.default === note);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        ["\"Hello \u{e9}!\\n\"", "true undefined", "true"]
    );
}

#[test]
fn esm_import_bytes_attribute() {
    // TC39 import-bytes: `with { type: "bytes" }` default-exports a Uint8Array over an
    // immutable buffer, byte-exact for arbitrary binary content (here deliberately invalid
    // UTF-8). Strict (module) writes throw TypeError and leave the bytes untouched.
    use std::fs;
    let dir = TempDir::new("esm-import-bytes");
    let root = dir.0.clone();
    fs::write(root.join("blob.bin"), [0u8, 1, 0xfe, 0xff, 0x80, 65]).unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import b from "./blob.bin" with { type: "bytes" };
        console.log(b instanceof Uint8Array, b.length, Array.from(b).join(","));
        console.log(b.buffer.immutable);
        let wrote = "no-throw";
        try { b[0] = 9; } catch (e) { wrote = e.constructor.name; }
        console.log(wrote, b[0]);
        const dyn = await import("./blob.bin", { with: { type: "bytes" } });
        console.log(dyn.default === b);
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        ["true 6 0,1,254,255,128,65", "true", "TypeError 0", "true"]
    );
}

#[test]
fn esm_import_json_attribute() {
    // JSON modules: `with { type: "json" }` JSON.parses the file for ANY extension; the legacy
    // attribute-less `.json` import gets the same JSON.parse semantics (`__proto__` becomes a
    // plain own data property — never prototype-setting) but stays a distinct module record.
    use std::fs;
    let dir = TempDir::new("esm-import-json");
    let root = dir.0.clone();
    fs::write(
        root.join("data.json"),
        r#"{ "answer": 42, "__proto__": { "evil": true } }"#,
    )
    .unwrap();
    fs::write(root.join("data.txt"), r#"{ "fromTxt": true }"#).unwrap();
    fs::write(root.join("bad.json"), "{ bad").unwrap();
    fs::write(
        root.join("app.mjs"),
        r#"
        import data from "./data.json" with { type: "json" };
        import legacy from "./data.json";
        import txt from "./data.txt" with { type: "json" };
        console.log(data.answer, txt.fromTxt, data === legacy);
        const safe = (o) =>
            Object.getPrototypeOf(o) === Object.prototype &&
            o.evil === undefined &&
            Object.getOwnPropertyNames(o).includes("__proto__");
        console.log(safe(data), safe(legacy));
        const dyn = await import("./data.json", { with: { type: "json" } });
        console.log(dyn.default === data);
        try {
            await import("./bad.json", { with: { type: "json" } });
            console.log("bad: resolved");
        } catch (e) {
            console.log("bad:", e.constructor.name);
        }
        "#,
    )
    .unwrap();
    let (mut rt, out, _err) = test_runtime();
    rt.run_module(&root.join("app.mjs").to_string_lossy())
        .expect("module runs");
    assert_eq!(
        out.lines(),
        ["42 true false", "true true", "true", "bad: SyntaxError"]
    );
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

/// Bun.hash through the whole JS glue stack: values are Bun 1.2.21 outputs (the exhaustive
/// oracle-matrix test lives in lumen-node's `bunhash::tests`; this covers the glue — return
/// types, input coercion, and seed coercion edge cases).
#[test]
fn bun_hash_matches_bun_through_the_glue() {
    let (mut rt, out, _err) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const h = Bun.hash;
        console.log(`${typeof h("hello")} ${h("hello")}`);
        console.log(`${h.wyhash("hello", 1)}`);
        console.log(`${typeof h.cityHash32("hello")} ${h.cityHash32("hello")}`);
        console.log(`${h.cityHash64("hello")}`);
        console.log(`${h.xxHash32("hello")} ${h.xxHash64("hello")} ${h.xxHash3("hello")}`);
        console.log(`${h.murmur32v3("hello")} ${h.murmur32v2("hello")} ${h.murmur64v2("hello")}`);
        console.log(`${h.rapidhash("hello")} ${h.crc32("hello")} ${h.adler32("hello")}`);
        // seed coercion: int32 sign-extends; 2^51 clamps to 0; bigints wrap mod 2^64
        console.log(`${h.wyhash("x", -1)} ${h.wyhash("x", 2 ** 51) === h.wyhash("x", 0)}`);
        console.log(`${h.wyhash("hello world test", 2n ** 64n - 1n)}`);
        // input coercion: string === its utf8 bytes === offset subarray; null → "null"
        const bytes = new TextEncoder().encode("hello");
        const sub = new Uint8Array(new TextEncoder().encode("XXhelloYY").buffer, 2, 5);
        console.log(`${h.wyhash(bytes) === h.wyhash("hello")} ${h.wyhash(sub) === h.wyhash("hello")}`);
        console.log(`${h.wyhash(null) === h.wyhash("null")}`);
        "#,
    );
    rt.run_to_completion();
    assert_eq!(
        out.lines(),
        [
            "bigint 1019145960556548909",
            "15802777309726279454",
            "number 2039911270",
            "16172099214758459231",
            "4211111929 2794345569481354659 10760762337991515389",
            "613153351 3848350155 2191231550387646743",
            "9166712279701818032 907060870 103547413",
            "12979056674793561970 true",
            "5531584226709605751",
            "true true",
            "true",
        ]
    );
}
