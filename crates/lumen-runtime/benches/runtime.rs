//! Runtime benchmarks — the web-platform surface the runtime assembles on top of the engine
//! (channel messaging, broadcast fan-out, error reporting), driven through the public
//! `Runtime::eval` front door with the event loop pumped to quiescence per iteration.
//!
//! Run with `cargo bench -p lumen-runtime` (or `scripts/run-benches.sh runtime`). Uses the same
//! std-only harness as the engine suite; no third-party bench crate.

#[path = "../../lumen/benches/support/harness.rs"]
mod harness;

use harness::{black_box, Bench};
use lumen_runtime::Runtime;

/// 100 MessageChannel round trips per iteration: postMessage → task delivery → reply.
const MESSAGE_CHANNEL_ROUND_TRIP: &str = r#"(() => {
    const { port1, port2 } = new MessageChannel();
    let n = 0;
    port1.onmessage = (e) => { if (e.data < 100) port1.postMessage(e.data + 1); else n = e.data; };
    port2.onmessage = (e) => port2.postMessage(e.data + 1);
    port2.postMessage(0);
    return n;
})()"#;

/// One sender broadcasting 50 structured payloads to 4 receivers.
const BROADCAST_FANOUT: &str = r#"(() => {
    const tx = new BroadcastChannel("bench");
    const rx = [1, 2, 3, 4].map(() => new BroadcastChannel("bench"));
    let got = 0;
    for (const r of rx) r.onmessage = (e) => { got += e.data.items.length; };
    for (let i = 0; i < 50; i++) tx.postMessage({ seq: i, items: [i, i + 1, i + 2] });
    for (const r of rx) setTimeout(() => r.close(), 0);
    setTimeout(() => tx.close(), 0);
    return got;
})()"#;

/// postMessage serialization cost: a nested ~100-node object per message, 50 messages.
const STRUCTURED_PAYLOAD: &str = r#"(() => {
    const { port1, port2 } = new MessageChannel();
    let sum = 0;
    port1.onmessage = (e) => { sum += e.data.rows.length; };
    const payload = { rows: [], meta: { tag: "bench" } };
    for (let i = 0; i < 20; i++) payload.rows.push({ id: i, name: "row-" + i, vals: [i, i * 2, i * 3] });
    for (let i = 0; i < 50; i++) port2.postMessage(payload);
    return sum;
})()"#;

/// The global-onerror hook path: 200 reportError calls with a suppressing handler installed.
const REPORT_ERROR_SUPPRESSED: &str = r#"(() => {
    let seen = 0;
    globalThis.onerror = () => { seen++; return true; };
    const err = new Error("bench");
    for (let i = 0; i < 200; i++) reportError(err);
    globalThis.onerror = null;
    return seen;
})()"#;

/// AbortSignal.any composition + abort propagation, 100 composites per iteration.
const ABORT_SIGNAL_ANY: &str = r#"(() => {
    let fired = 0;
    for (let i = 0; i < 100; i++) {
        const a = new AbortController();
        const b = new AbortController();
        const s = AbortSignal.any([a.signal, b.signal]);
        s.addEventListener("abort", () => fired++);
        a.abort("done");
    }
    return fired;
})()"#;

/// One WebSocket connect + 20 echo round trips + close, against a shared in-process echo server.
const WS_ECHO_ROUND_TRIPS: &str = r#"(() => new Promise((resolve) => {
    const ws = new WebSocket("ws://127.0.0.1:{PORT}/");
    let n = 0;
    ws.onopen = () => ws.send("ping-0");
    ws.onmessage = (e) => {
        if (++n >= 20) { ws.close(); }
        else ws.send("ping-" + n);
    };
    ws.onclose = () => resolve(n);
}))()"#;

/// Structured-clone wire encode+decode of a ~100-node object (the Worker.postMessage cost).
const CLONE_WIRE_ROUND_TRIP: &str = r#"(() => {
    const payload = { rows: [], meta: { tag: "bench", when: new Date(0) } };
    for (let i = 0; i < 20; i++) payload.rows.push({ id: i, name: "row-" + i, vals: [i, i * 2, i * 3] });
    let n = 0;
    for (let i = 0; i < 50; i++) n += __deserializeClone(__serializeForClone(payload)).rows.length;
    return n;
})()"#;

/// Spawn a worker, exchange 10 messages, terminate — measures thread+realm startup + the bridge.
const WORKER_SPAWN_ROUND_TRIP: &str = r#"(() => new Promise((resolve) => {
    const w = new Worker("{SCRIPT}", { type: "module" });
    let n = 0;
    w.onmessage = (e) => {
        if (e.data < 10) w.postMessage(e.data + 1);
        else { w.terminate(); resolve(e.data); }
    };
    w.postMessage(0);
}))"#;

fn main() {
    let mut b = Bench::new();

    b.run("runtime: startup (Runtime::new)", || {
        black_box(Runtime::new());
    });

    // Structured-clone wire format encode+decode (Worker message serialization cost).
    {
        let mut rt = Runtime::new();
        b.run("clone wire: encode+decode 50x ~100-node object", || {
            black_box(rt.eval(CLONE_WIRE_ROUND_TRIP).expect("clone bench parses"));
        });
    }

    // Worker: spawn a realm-per-thread worker, 10 message round trips, terminate — one full worker
    // lifecycle per iteration.
    {
        let dir = std::env::temp_dir().join(format!("lumen-worker-bench-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("inc.mjs");
        std::fs::write(&script, "onmessage = (e) => postMessage(e.data + 1);").unwrap();
        let src: &'static str = Box::leak(
            WORKER_SPAWN_ROUND_TRIP
                .replace("{SCRIPT}", &script.to_string_lossy())
                .into_boxed_str(),
        );
        let mut rt = Runtime::new();
        b.run("worker: spawn + 10 round trips + terminate", || {
            black_box(rt.eval(&format!("({src})()")).expect("worker bench parses"));
        });
    }

    // WebSocket echo round trips: a long-lived echo server, one socket (connect→20 echoes→close)
    // per iteration through a fresh-per-iteration eval on a persistent runtime.
    {
        let port = lumen_web::ws_testing::spawn_echo(
            lumen_web::ws_testing::Mode::Echo,
            1_000_000,
        );
        let src: &'static str =
            Box::leak(WS_ECHO_ROUND_TRIPS.replace("{PORT}", &port.to_string()).into_boxed_str());
        let mut rt = Runtime::new();
        b.run("websocket: connect + 20 echo round trips + close", || {
            black_box(rt.eval(src).expect("ws bench parses"));
        });
    }

    // EventSource: connect, receive the canned event batch, close — one connection per iteration
    // against a long-lived event-stream server (measures handshake + SSE line parsing + dispatch).
    {
        let port = lumen_web::sse_testing::spawn(lumen_web::sse_testing::Mode::Events, 1_000_000);
        let src: &'static str = Box::leak(
            r#"(() => new Promise((resolve) => {
                const es = new EventSource("http://127.0.0.1:{PORT}/");
                let n = 0;
                es.onmessage = () => { n++; };
                es.addEventListener("tick", () => { n++; });
                es.onerror = () => { es.close(); resolve(n); };
            }))()"#
                .replace("{PORT}", &port.to_string())
                .into_boxed_str(),
        );
        let mut rt = Runtime::new();
        b.run("eventsource: connect + parse 3-event stream + close", || {
            black_box(rt.eval(src).expect("sse bench parses"));
        });
    }

    let mut rt = Runtime::new();
    let mut bench_js = |b: &mut Bench, name: &str, src: &'static str| {
        b.run(name, || {
            black_box(rt.eval(src).expect("bench source parses"));
        });
    };
    bench_js(&mut b, "messaging: MessageChannel 100 round trips", MESSAGE_CHANNEL_ROUND_TRIP);
    bench_js(&mut b, "messaging: BroadcastChannel 50 msgs x 4 receivers", BROADCAST_FANOUT);
    bench_js(&mut b, "messaging: postMessage 50 structured payloads", STRUCTURED_PAYLOAD);
    bench_js(&mut b, "error reporting: 200 suppressed reportError", REPORT_ERROR_SUPPRESSED);
    bench_js(&mut b, "abort: 100 AbortSignal.any composites", ABORT_SIGNAL_ANY);

    b.report();
}
