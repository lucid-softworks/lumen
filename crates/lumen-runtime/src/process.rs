//! Minimal `process`: argv/env/platform snapshots, `cwd()`, `exit()`, `nextTick()`, `hrtime()`,
//! and `stdout`/`stderr` writable streams. Enough for scripts to orient themselves and for the
//! Node ecosystem (morgan et al.) to log and time; the fuller `node:process` surface is layered
//! on in lumen-node.

use std::io::Write;
use std::time::Instant;

use lumen_host::{ops, CallbackQueue, Ctx, Engine, Extension, OpState, Value};

use crate::console::ConsoleOut;

/// Process-start monotonic reference for `hrtime()`.
struct ProcStart(Instant);

pub(crate) fn extension() -> Extension {
    Extension {
        name: "process",
        globals: &[],
        namespaces: &[
            (
                "process",
                ops![
                    "cwd" (0) => op_cwd,
                    "exit" (1) => op_exit,
                    "nextTick" (1) => op_next_tick,
                ],
            ),
            // Internal primitives the js_init below wraps into process.stdout/stderr/hrtime, then
            // deletes the namespace (the capture-and-delete pattern the op crates use).
            (
                "__proc",
                ops![
                    "writeStdout" (1) => op_write_stdout,
                    "writeStderr" (1) => op_write_stderr,
                    "hrtime" (0) => op_hrtime,
                ],
            ),
        ],
        state_init: Some(|state: &mut OpState| state.put(ProcStart(Instant::now()))),
        js_init: Some(JS_INIT),
        js_init_snapshot: None,
    }
}

/// Shapes `process.stdout`/`stderr` (over the raw write ops) and `process.hrtime` (over the
/// monotonic op), and stamps `version`/`versions`. We report a Node version string because the
/// ecosystem branches on it for feature detection; `versions.lumen` records the real engine.
const JS_INIT: &str = r#"(() => {
  const proc = globalThis.__proc;
  delete globalThis.__proc;
  const process = globalThis.process;

  const makeStream = (writeFn, fd) => ({
    write(chunk) { writeFn(chunk); return true; },
    end(chunk) { if (chunk != null) writeFn(chunk); return this; },
    isTTY: false,
    fd,
    columns: 80,
    rows: 24,
    // morgan/debug attach listeners; accept and ignore them (nothing emits).
    on() { return this; },
    once() { return this; },
    removeListener() { return this; },
    cork() {},
    uncork() {},
  });
  Object.defineProperty(process, "stdout", { value: makeStream(proc.writeStdout, 1), enumerable: true, configurable: true });
  Object.defineProperty(process, "stderr", { value: makeStream(proc.writeStderr, 2), enumerable: true, configurable: true });

  const raw = proc.hrtime;
  const hrtime = (prev) => {
    const t = raw();
    if (prev) {
      let s = t[0] - prev[0], n = t[1] - prev[1];
      if (n < 0) { s -= 1; n += 1e9; }
      return [s, n];
    }
    return t;
  };
  hrtime.bigint = () => { const t = raw(); return BigInt(t[0]) * 1000000000n + BigInt(t[1]); };
  process.hrtime = hrtime;

  process.version = "v20.11.0";
  process.versions = { node: "20.11.0", lumen: "0.1.1", v8: "0.0.0" };
})();"#;

/// The data properties (`argv`, `env`, `platform`) — snapshots taken at startup, like Node's.
/// Runs after `install` because it needs the `process` object that install created.
pub(crate) fn install_data_props(engine: &mut Engine) {
    let global = engine.global_this();
    let ctx = engine.ctx();
    let process = match ctx.get_member(&global, "process") {
        Ok(v @ Value::Obj(_)) => v,
        _ => unreachable!("install() defined the process namespace"),
    };

    let argv: Vec<Value> = std::env::args().map(Value::from_string).collect();
    let argv = ctx.make_array(argv);
    let _ = ctx.set_member(&process, "argv", argv);

    let env = ctx.new_object();
    let env = Value::Obj(env);
    for (k, v) in std::env::vars() {
        let _ = ctx.set_member(&env, &k, Value::from_string(v));
    }
    let _ = ctx.set_member(&process, "env", env);

    let platform = match std::env::consts::OS {
        "macos" => "darwin", // Node's name for it
        other => other,
    };
    let _ = ctx.set_member(&process, "platform", Value::str(platform));

    let _ = ctx.set_member(&process, "pid", Value::Num(std::process::id() as f64));
    // Node's architecture names, not Rust's (native addons resolve their platform binary by these).
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let _ = ctx.set_member(&process, "arch", Value::str(arch));
}

/// `(chunk)` — write raw bytes to stdout (no trailing newline, unlike `console.log`). A typed
/// array is written as-is; anything else is coerced to a string. Backs `process.stdout.write`.
fn op_write_stdout(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_raw(ctx, args, false)
}
fn op_write_stderr(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_raw(ctx, args, true)
}
fn write_raw(ctx: &mut Ctx, args: &[Value], to_err: bool) -> Result<Value, Value> {
    let arg = args.first().unwrap_or(&Value::Undefined);
    let bytes = match ctx.typed_array_bytes(arg) {
        Some(b) => b,
        None => ctx.coerce_string(arg)?.as_bytes().to_vec(),
    };
    let sinks = ctx.host_mut::<ConsoleOut>().expect("console state installed");
    let sink = if to_err { &mut sinks.err } else { &mut sinks.out };
    // A broken pipe shouldn't crash the runtime (matches console's behavior).
    let _ = sink.write_all(&bytes);
    let _ = sink.flush();
    Ok(Value::Undefined)
}

/// `[seconds, nanoseconds]` elapsed since process start, from a monotonic clock (Node's
/// `process.hrtime()` contract). The js_init wrapper handles the optional `prev` diff + `.bigint`.
fn op_hrtime(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let start = ctx.host_mut::<ProcStart>().expect("process start installed").0;
    let e = start.elapsed();
    Ok(ctx.make_array(vec![
        Value::Num(e.as_secs() as f64),
        Value::Num(e.subsec_nanos() as f64),
    ]))
}

fn op_cwd(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    match std::env::current_dir() {
        Ok(p) => Ok(Value::from_string(p.to_string_lossy().into_owned())),
        Err(e) => Err(ctx.make_error("Error", format!("cwd unavailable: {e}"))),
    }
}

fn op_exit(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let code = match args.first() {
        Some(v) => ctx.coerce_number(v)? as i32,
        None => 0,
    };
    std::process::exit(code);
}

/// Queued on the loop's callback queue: runs next turn, before timers. (Node runs nextTick
/// callbacks even sooner — between microtask checkpoints — but "before any timer" is the
/// part programs rely on; noted as a deliberate approximation.)
fn op_next_tick(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let callback = match args.first() {
        Some(cb) if cb.is_callable() => cb.clone(),
        _ => return Err(ctx.make_error("TypeError", "process.nextTick expects a function")),
    };
    let extra: Vec<Value> = args.iter().skip(1).cloned().collect();
    CallbackQueue::enqueue(ctx.op_state(), callback, extra);
    Ok(Value::Undefined)
}
