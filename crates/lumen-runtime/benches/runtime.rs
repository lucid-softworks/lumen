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

fn main() {
    let mut b = Bench::new();

    b.run("runtime: startup (Runtime::new)", || {
        black_box(Runtime::new());
    });

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
