//! Web Workers (HTML §10.2) — realm-per-thread with structured messaging.
//!
//! A `Worker` is a dedicated OS thread running its OWN [`Runtime`] (a fresh realm: its own global,
//! intrinsics, event loop, and thread pool). Because engine `Value`s are `!Send`, messages cross
//! the thread boundary as bytes: the sender serializes with the JS structured-clone wire format
//! (`__serializeForClone`, see `lumen-web/src/js/serialize.js`) and the receiver deserializes in
//! its own realm. Two `mpsc` channels carry those bytes; each side arms a blocking "inbox" task
//! (the WebSocket-reader re-arm pattern) that delivers each message to JS and re-arms the next.
//!
//! ## What's intentionally v1
//! - **Cooperative terminate**: `terminate()` (and the worker's `close()`) set a shared stop flag
//!   the worker loop polls; a worker stuck in a long *synchronous* JS task finishes it first (no
//!   preemption — the engine has no interrupt point). Pending timers are dropped on stop.
//! - **No `SharedArrayBuffer` sharing, no transfer list** (messages are always copied).
//! - Reuses the full [`Runtime`] per worker (own 4-thread pool). Heavy but correct.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use lumen_host::{ops, CompletionSender, Ctx, Extension, TaskRegistry, Value};

use crate::Runtime;

// ---- cross-thread messages (all Send) ---------------------------------------------------------

enum ToWorker {
    Data(Vec<u8>),
}

enum ToMain {
    Message(Vec<u8>),
    Error(String),
    Exited,
}

// ---- main side --------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WorkerRegistry {
    next: u64,
    workers: HashMap<u64, WorkerEntry>,
}

struct WorkerEntry {
    to_worker: Sender<ToWorker>,
    dispatch: Value,
    stop: Arc<AtomicBool>,
}

fn registry(ctx: &mut Ctx) -> &mut WorkerRegistry {
    ctx.host_mut::<WorkerRegistry>().expect("worker registry installed")
}

/// The worker→main inbox, carried by value through each completion so the next receive re-arms.
struct MainInbox {
    id: u64,
    rx: Receiver<ToMain>,
}

struct MainInboxResult {
    id: u64,
    event: ToMain,
    inbox: Option<MainInbox>,
}

/// `__worker.spawn(path, isModule, dispatch)` → id. Spawns the worker thread and arms the
/// worker→main inbox. `dispatch(kind, ...)` receives `("message", u8array)`, `("error", string)`,
/// or `("exit")`.
pub(crate) fn op_worker_spawn(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let path = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let is_module = matches!(args.get(1), Some(Value::Bool(true)));
    let dispatch = match args.get(2) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "spawn: dispatch must be a function")),
    };

    let (to_worker_tx, to_worker_rx) = channel::<ToWorker>();
    let (to_main_tx, to_main_rx) = channel::<ToMain>();
    let stop = Arc::new(AtomicBool::new(false));

    let id = {
        let reg = registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.workers.insert(
            id,
            WorkerEntry {
                to_worker: to_worker_tx,
                dispatch: dispatch.clone(),
                stop: Arc::clone(&stop),
            },
        );
        id
    };

    let worker_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name(format!("lumen-worker-{id}"))
        .spawn(move || run_worker(path, is_module, to_worker_rx, to_main_tx, worker_stop))
        .expect("spawn worker thread");

    arm_main_inbox(ctx, MainInbox { id, rx: to_main_rx });
    Ok(Value::Num(id as f64))
}

/// `__worker.post(id, u8array)` — enqueue an already-serialized message for the worker.
pub(crate) fn op_worker_post(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "post: bad worker id")),
    };
    let bytes = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "post: expected a Uint8Array"))?;
    if let Some(w) = registry(ctx).workers.get(&id) {
        let _ = w.to_worker.send(ToWorker::Data(bytes));
    }
    Ok(Value::Undefined)
}

/// `__worker.terminate(id)` — set the shared stop flag and drop the worker's inbox sender (so a
/// blocked receive unblocks). The worker loop exits at its next poll; it then posts `Exited`.
pub(crate) fn op_worker_terminate(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "terminate: bad worker id")),
    };
    if let Some(w) = registry(ctx).workers.remove(&id) {
        w.stop.store(true, Ordering::SeqCst);
        drop(w); // drops `to_worker`, unblocking the worker's inbox receive
    }
    Ok(Value::Undefined)
}

fn arm_main_inbox(ctx: &mut Ctx, inbox: MainInbox) {
    let id = inbox.id;
    let dispatch = match registry(ctx).workers.get(&id) {
        Some(w) => w.dispatch.clone(),
        None => return, // already terminated
    };
    let task = ctx
        .host_mut::<TaskRegistry>()
        .expect("task registry installed")
        .register(dispatch, None, decode_main_inbox);
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let event = inbox.rx.recv().unwrap_or(ToMain::Exited);
        let done = matches!(event, ToMain::Exited);
        Box::new(MainInboxResult {
            id,
            event,
            inbox: (!done).then_some(inbox),
        })
    });
}

fn decode_main_inbox(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let MainInboxResult { id, event, inbox } = *payload.downcast::<MainInboxResult>().expect("main inbox payload");
    if let Some(inbox) = inbox {
        arm_main_inbox(ctx, inbox);
    }
    match event {
        ToMain::Message(bytes) => {
            let arr = ctx.make_uint8array(&bytes)?;
            Ok(vec![Value::from_string("message".into()), arr])
        }
        ToMain::Error(msg) => Ok(vec![Value::from_string("error".into()), Value::from_string(msg)]),
        ToMain::Exited => {
            registry(ctx).workers.remove(&id);
            Ok(vec![Value::from_string("exit".into())])
        }
    }
}

// ---- worker side ------------------------------------------------------------------------------

/// Per-worker state living in the worker Runtime's `OpState`: how to reach the main thread and the
/// shared stop flag `self.close()` / the main's `terminate()` set.
struct WorkerSelf {
    to_main: Sender<ToMain>,
    stop: Arc<AtomicBool>,
}

/// The worker thread's whole lifecycle: build a realm, run the entry, then pump the loop until
/// stopped, bridging messages both ways.
fn run_worker(
    path: String,
    is_module: bool,
    to_worker_rx: Receiver<ToWorker>,
    to_main_tx: Sender<ToMain>,
    stop: Arc<AtomicBool>,
) {
    let mut rt = Runtime::new();
    lumen_host::install(rt.engine(), &[worker_scope_extension()]);
    rt.engine().ctx().op_state().put(WorkerSelf {
        to_main: to_main_tx.clone(),
        stop: Arc::clone(&stop),
    });

    // The DedicatedWorkerGlobalScope surface (postMessage/onmessage/close/addEventListener) —
    // evaluated only in worker realms, so it lives here rather than in the shared web glue.
    if rt.engine().eval(WORKER_SCOPE_JS, false).is_err() {
        let _ = to_main_tx.send(ToMain::Error("worker scope bootstrap failed".into()));
        let _ = to_main_tx.send(ToMain::Exited);
        return;
    }

    match std::fs::read_to_string(&path) {
        Ok(source) => {
            if let Err(e) = rt.eval_worker_entry(&source, &path, is_module) {
                let _ = to_main_tx.send(ToMain::Error(e));
            }
        }
        Err(e) => {
            let _ = to_main_tx.send(ToMain::Error(format!("cannot load worker script {path}: {e}")));
        }
    }

    // Arm the main→worker inbox, then pump the loop (stays alive until stopped).
    arm_worker_inbox(rt.engine().ctx(), WorkerInbox { rx: to_worker_rx });
    rt.run_worker_loop(&stop);

    let _ = to_main_tx.send(ToMain::Exited);
}

struct WorkerInbox {
    rx: Receiver<ToWorker>,
}

struct WorkerInboxResult {
    bytes: Option<Vec<u8>>,
    inbox: Option<WorkerInbox>,
}

fn arm_worker_inbox(ctx: &mut Ctx, inbox: WorkerInbox) {
    // The JS dispatcher is a stable global installed by WORKER_SCOPE_JS.
    let global = ctx.global_this();
    let dispatch = match ctx.get_member(&global, "__workerDispatchMessage") {
        Ok(v) if v.is_callable() => v,
        _ => return,
    };
    let task = ctx
        .host_mut::<TaskRegistry>()
        .expect("task registry installed")
        .register(dispatch, None, decode_worker_inbox);
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let (bytes, keep) = match inbox.rx.recv() {
            Ok(ToWorker::Data(b)) => (Some(b), true),
            Err(_) => (None, false), // main dropped the sender (terminate)
        };
        Box::new(WorkerInboxResult {
            bytes,
            inbox: keep.then_some(inbox),
        })
    });
}

fn decode_worker_inbox(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let WorkerInboxResult { bytes, inbox } = *payload.downcast::<WorkerInboxResult>().expect("worker inbox payload");
    if let Some(inbox) = inbox {
        arm_worker_inbox(ctx, inbox);
    }
    match bytes {
        Some(b) => {
            let arr = ctx.make_uint8array(&b)?;
            Ok(vec![arr])
        }
        // Channel closed (terminate): fire nothing; the loop exits via the stop flag.
        None => Ok(vec![Value::Bool(false)]),
    }
}

/// `__wself.post(u8array)` — send an already-serialized message to the main thread.
fn op_wself_post(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let bytes = ctx
        .typed_array_bytes(args.first().unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "post: expected a Uint8Array"))?;
    if let Some(w) = ctx.host_mut::<WorkerSelf>() {
        let _ = w.to_main.send(ToMain::Message(bytes));
    }
    Ok(Value::Undefined)
}

/// `__wself.close()` — request the worker loop to stop.
fn op_wself_close(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> {
    if let Some(w) = ctx.host_mut::<WorkerSelf>() {
        w.stop.store(true, Ordering::SeqCst);
    }
    Ok(Value::Undefined)
}

/// `__wself.report(message)` — forward a worker-side uncaught error to the main thread (wired to
/// the worker's global `onerror` in the bootstrap).
fn op_wself_report(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let msg = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    if let Some(w) = ctx.host_mut::<WorkerSelf>() {
        let _ = w.to_main.send(ToMain::Error(msg));
    }
    Ok(Value::Undefined)
}

fn worker_scope_extension() -> Extension {
    Extension {
        name: "worker-scope",
        globals: &[],
        namespaces: &[(
            "__wself",
            ops![
                "post" (1) => op_wself_post,
                "close" (0) => op_wself_close,
                "report" (1) => op_wself_report,
            ],
        )],
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
    }
}

/// The main-thread `Worker` class + `__worker` op capture — the runtime extension's js_init.
pub(crate) fn extension() -> Extension {
    Extension {
        name: "worker",
        globals: &[],
        namespaces: &[(
            "__worker",
            ops![
                "spawn" (3) => op_worker_spawn,
                "post" (2) => op_worker_post,
                "terminate" (1) => op_worker_terminate,
            ],
        )],
        state_init: Some(|state| state.put(WorkerRegistry::default())),
        js_init: Some(WORKER_JS),
        js_init_snapshot: None,
    }
}

/// The main-side `Worker` class. Captures `__worker` (removing it from global scope) and drives
/// the message bridge through the structured-clone wire format.
const WORKER_JS: &str = r#"
(() => {
  "use strict";
  const __worker = globalThis.__worker;
  delete globalThis.__worker;
  const serialize = globalThis.__serializeForClone;
  const deserialize = globalThis.__deserializeClone;

  class Worker extends EventTarget {
    #id;
    #terminated = false;
    constructor(scriptURL, options = {}) {
      super();
      if (arguments.length === 0) throw new TypeError("Worker requires a scriptURL");
      options = options && typeof options === "object" ? options : {};
      const isModule = options.type === "module";
      let path = String(scriptURL);
      if (path.startsWith("file://")) path = path.slice(7);
      this.#id = __worker.spawn(path, isModule, (kind, ...args) => this.#onEvent(kind, args));
    }
    postMessage(message, _transfer) {
      if (this.#terminated) return;
      let bytes;
      try { bytes = serialize(message); }
      catch (e) { throw e; } // DataCloneError surfaces to the caller
      __worker.post(this.#id, bytes);
    }
    terminate() {
      if (this.#terminated) return;
      this.#terminated = true;
      __worker.terminate(this.#id);
    }
    #fire(type, event) {
      const h = this["on" + type];
      if (typeof h === "function") { try { h.call(this, event); } catch (e) { reportError(e); } }
      this.dispatchEvent(event);
    }
    #onEvent(kind, args) {
      if (kind === "message") {
        let data;
        try { data = deserialize(args[0]); }
        catch { this.#fire("messageerror", new MessageEvent("messageerror", {})); return; }
        this.#fire("message", new MessageEvent("message", { data }));
      } else if (kind === "error") {
        this.#fire("error", new ErrorEvent("error", { message: args[0] }));
      } else if (kind === "exit") {
        this.#terminated = true;
      }
    }
  }
  for (const name of ["message", "messageerror", "error"]) {
    Object.defineProperty(Worker.prototype, "on" + name, {
      configurable: true, enumerable: true, writable: true, value: null,
    });
  }
  globalThis.Worker = Worker;
})();
"#;

/// The DedicatedWorkerGlobalScope surface, evaluated inside each worker realm (see `run_worker`).
const WORKER_SCOPE_JS: &str = r#"
(() => {
  "use strict";
  const post = __wself.post;
  const closeSelf = __wself.close;
  const report = __wself.report;
  delete globalThis.__wself;
  const serialize = globalThis.__serializeForClone;
  const deserialize = globalThis.__deserializeClone;

  // The global scope acts as an EventTarget for message/messageerror/error.
  const target = new EventTarget();
  globalThis.addEventListener = target.addEventListener.bind(target);
  globalThis.removeEventListener = target.removeEventListener.bind(target);
  globalThis.dispatchEvent = target.dispatchEvent.bind(target);

  globalThis.postMessage = (message, _transfer) => { post(serialize(message)); };
  globalThis.close = () => closeSelf();
  globalThis.onmessage = null;
  globalThis.onmessageerror = null;

  const fire = (type, event) => {
    const h = globalThis["on" + type];
    if (typeof h === "function") { try { h.call(globalThis, event); } catch (e) { reportError(e); } }
    target.dispatchEvent(event);
  };

  globalThis.__workerDispatchMessage = (bytes) => {
    if (bytes === false) return; // channel-closed sentinel
    let data;
    try { data = deserialize(bytes); }
    catch { fire("messageerror", new MessageEvent("messageerror", {})); return; }
    fire("message", new MessageEvent("message", { data }));
  };

  // A worker-side uncaught error propagates to the parent's Worker.onerror.
  globalThis.onerror = (message) => { report(String(message)); return true; };
})();
"#;
