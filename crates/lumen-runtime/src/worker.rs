//! Workers — realm-per-thread with structured messaging. Backs BOTH the Web `Worker` (HTML §10.2)
//! and `node:worker_threads` (the node glue's Worker class drives the same ops in "node mode").
//!
//! A `Worker` is a dedicated OS thread running its OWN [`Runtime`] (a fresh realm: its own global,
//! intrinsics, event loop, and thread pool). Because engine `Value`s are `!Send`, messages cross
//! the thread boundary as bytes: the sender serializes with the JS structured-clone wire format
//! (`__serializeForClone`, see `lumen-web/src/js/serialize.js`) and the receiver deserializes in
//! its own realm. Two `mpsc` channels carry those bytes; each side arms a blocking "inbox" task
//! (the WebSocket-reader re-arm pattern) that delivers each message to JS and re-arms the next.
//!
//! ## Node mode (`spawn` options `{ node: true, ... }`)
//! - The worker realm gets the `node:worker_threads` bootstrap (parentPort, workerData, threadId,
//!   patched process.argv/env/exit) instead of the DedicatedWorkerGlobalScope surface.
//! - `online` is announced before the entry runs; `exit` carries a real exit code (0 natural,
//!   `process.exit(code)`'s code, 1 on terminate/uncaught — Node's codes).
//! - The worker→main inbox is *unref'd* until `parentPort` gains a `'message'` listener, so a
//!   worker with nothing pending exits naturally with code 0 (Node's port-ref semantics).
//! - A non-`.mjs` file entry runs through the node CJS loader as the realm's main module
//!   (`require.main === module`, `__filename`/`__dirname` in scope), like Node workers.
//!
//! ## What's intentionally v1
//! - **Cooperative terminate**: `terminate()` (and the worker's `close()`/`process.exit()`) set a
//!   shared stop flag the worker loop polls; a worker stuck in a long *synchronous* JS task
//!   finishes it first (no preemption — the engine has no interrupt point). Pending timers are
//!   dropped on stop. Likewise `process.exit()` in a worker stops at the next loop poll rather
//!   than instantly.
//! - **No `SharedArrayBuffer` sharing, no transfer list** (messages are always copied).
//! - Reuses the full [`Runtime`] per worker (own 4-thread pool). Heavy but correct.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use lumen_host::{ops, CompletionSender, Ctx, Extension, TaskId, TaskRegistry, Value};

use crate::Runtime;

/// Node-visible thread ids: the main thread is 0, workers count up from 1 (process-wide, so ids
/// stay unique even for workers spawned from workers).
static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

// ---- cross-thread messages (all Send) ---------------------------------------------------------

enum ToWorker {
    Data(Vec<u8>),
}

enum ToMain {
    Online,
    Message(Vec<u8>),
    Error(String),
    Exited(i32),
}

// ---- main side --------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WorkerRegistry {
    next: u64,
    workers: HashMap<u64, WorkerEntry>,
}

struct WorkerEntry {
    /// `None` once terminated (dropping the sender unblocks the worker's inbox receive).
    to_worker: Option<Sender<ToWorker>>,
    dispatch: Value,
    stop: Arc<AtomicBool>,
    /// Whether this worker's main-side inbox keeps the main loop alive (`worker.unref()` clears).
    keep_alive: bool,
    /// The currently armed inbox task, so `setRef` can re-mark it in flight.
    inbox_task: Option<TaskId>,
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

/// What the spawned thread needs to build and run the worker realm.
struct WorkerSpec {
    /// The entry: a path, or the source itself when `is_eval`.
    entry: String,
    is_module: bool,
    is_node: bool,
    is_eval: bool,
    /// Structured-clone bytes of `{ workerData, argv, env, envData, entry }` (node mode).
    init: Option<Vec<u8>>,
    thread_id: u64,
}

/// `__worker.spawn(path, isModule, dispatch, opts?)` → `{ id, threadId }`. Spawns the worker
/// thread and arms the worker→main inbox. `dispatch(kind, ...)` receives `("online")`,
/// `("message", u8array)`, `("error", string)`, or `("exit", code)`. `opts` (node mode):
/// `{ node: true, eval: bool, init: Uint8Array }`.
pub(crate) fn op_worker_spawn(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let entry = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let is_module = matches!(args.get(1), Some(Value::Bool(true)));
    let dispatch = match args.get(2) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "spawn: dispatch must be a function")),
    };
    let opts = args.get(3).cloned().unwrap_or(Value::Undefined);
    let has_opts = opts.as_obj().is_some();
    let opt_bool = |ctx: &mut Ctx, name: &str| -> bool {
        has_opts && matches!(ctx.get_member(&opts, name), Ok(Value::Bool(true)))
    };
    let is_node = opt_bool(ctx, "node");
    let is_eval = opt_bool(ctx, "eval");
    let init = if has_opts {
        ctx.get_member(&opts, "init")
            .ok()
            .and_then(|v| ctx.typed_array_bytes(&v))
    } else {
        None
    };

    let (to_worker_tx, to_worker_rx) = channel::<ToWorker>();
    let (to_main_tx, to_main_rx) = channel::<ToMain>();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_id = NEXT_THREAD_ID.fetch_add(1, Ordering::SeqCst);

    let id = {
        let reg = registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.workers.insert(
            id,
            WorkerEntry {
                to_worker: Some(to_worker_tx),
                dispatch: dispatch.clone(),
                stop: Arc::clone(&stop),
                keep_alive: true,
                inbox_task: None,
            },
        );
        id
    };

    let spec = WorkerSpec {
        entry,
        is_module,
        is_node,
        is_eval,
        init,
        thread_id,
    };
    let worker_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name(format!("lumen-worker-{thread_id}"))
        .spawn(move || run_worker(spec, to_worker_rx, to_main_tx, worker_stop))
        .expect("spawn worker thread");

    arm_main_inbox(ctx, MainInbox { id, rx: to_main_rx });
    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "id", Value::Num(id as f64));
    let _ = ctx.set_member(&o, "threadId", Value::Num(thread_id as f64));
    Ok(o)
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
        if let Some(tx) = &w.to_worker {
            let _ = tx.send(ToWorker::Data(bytes));
        }
    }
    Ok(Value::Undefined)
}

/// `__worker.terminate(id)` — set the shared stop flag and drop the worker's inbox sender (so a
/// blocked receive unblocks). The worker loop exits at its next poll and posts `Exited`, which is
/// still delivered (the registry entry lives until then) so `'exit'` fires with the code.
pub(crate) fn op_worker_terminate(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "terminate: bad worker id")),
    };
    if let Some(w) = registry(ctx).workers.get_mut(&id) {
        w.stop.store(true, Ordering::SeqCst);
        w.to_worker = None; // drops the sender, unblocking the worker's inbox receive
    }
    Ok(Value::Undefined)
}

/// `__worker.setRef(id, keep)` — `worker.ref()/unref()`: whether this worker's inbox keeps the
/// main loop alive. Applies to the in-flight inbox task and every re-arm after it.
pub(crate) fn op_worker_set_ref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "setRef: bad worker id")),
    };
    let keep = matches!(args.get(1), Some(Value::Bool(true)));
    let task = match registry(ctx).workers.get_mut(&id) {
        Some(w) => {
            w.keep_alive = keep;
            w.inbox_task
        }
        None => return Ok(Value::Undefined),
    };
    if let (Some(task), Some(reg)) = (task, ctx.host_mut::<TaskRegistry>()) {
        if keep {
            reg.set_ref(task);
        } else {
            reg.set_unref(task);
        }
    }
    Ok(Value::Undefined)
}

fn arm_main_inbox(ctx: &mut Ctx, inbox: MainInbox) {
    let id = inbox.id;
    let (dispatch, keep_alive) = match registry(ctx).workers.get(&id) {
        Some(w) => (w.dispatch.clone(), w.keep_alive),
        None => return, // already gone
    };
    let reg = ctx.host_mut::<TaskRegistry>().expect("task registry installed");
    let task = reg.register(dispatch, None, decode_main_inbox);
    if !keep_alive {
        reg.set_unref(task);
    }
    if let Some(w) = registry(ctx).workers.get_mut(&id) {
        w.inbox_task = Some(task);
    }
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("completion sender installed")
        .clone();
    sender.run_blocking(task, move || {
        let event = inbox.rx.recv().unwrap_or(ToMain::Exited(1));
        let done = matches!(event, ToMain::Exited(_));
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
        ToMain::Online => Ok(vec![Value::from_string("online".into())]),
        ToMain::Message(bytes) => {
            let arr = ctx.make_uint8array(&bytes)?;
            Ok(vec![Value::from_string("message".into()), arr])
        }
        ToMain::Error(msg) => Ok(vec![Value::from_string("error".into()), Value::from_string(msg)]),
        ToMain::Exited(code) => {
            registry(ctx).workers.remove(&id);
            Ok(vec![Value::from_string("exit".into()), Value::Num(code as f64)])
        }
    }
}

// ---- worker side ------------------------------------------------------------------------------

/// Per-worker state living in the worker Runtime's `OpState`: how to reach the main thread, the
/// shared stop flag `self.close()` / `process.exit()` / the main's `terminate()` set, the exit
/// code, and the inbox ref bookkeeping (Node's parentPort ref semantics).
struct WorkerSelf {
    to_main: Sender<ToMain>,
    stop: Arc<AtomicBool>,
    exit_code: Option<i32>,
    /// Whether the armed inbox keeps the worker loop alive. Web workers: always (a worker waits
    /// for messages until closed). Node workers: only while parentPort has a 'message' listener.
    keep_alive: bool,
    inbox_task: Option<TaskId>,
}

/// The worker thread's whole lifecycle: build a realm, run the entry, then pump the loop until
/// stopped (or, node mode, idle), bridging messages both ways.
fn run_worker(
    spec: WorkerSpec,
    to_worker_rx: Receiver<ToWorker>,
    to_main_tx: Sender<ToMain>,
    stop: Arc<AtomicBool>,
) {
    let mut rt = Runtime::new();
    lumen_host::install(rt.engine(), &[worker_scope_extension()]);
    rt.engine().ctx().op_state().put(WorkerSelf {
        to_main: to_main_tx.clone(),
        stop: Arc::clone(&stop),
        exit_code: None,
        keep_alive: !spec.is_node,
        inbox_task: None,
    });

    // The per-realm bootstrap: DedicatedWorkerGlobalScope for web workers, the worker_threads
    // wiring (parentPort/workerData/threadId/process patches) for node workers. Evaluated only in
    // worker realms, so it lives here rather than in the shared glue.
    let boot = if spec.is_node {
        let ctx = rt.engine().ctx();
        let global = ctx.global_this();
        let _ = ctx.set_member(&global, "__lumenWorkerThreadId", Value::Num(spec.thread_id as f64));
        if let Some(bytes) = &spec.init {
            match ctx.make_uint8array(bytes) {
                Ok(arr) => {
                    let _ = ctx.set_member(&global, "__lumenWorkerInit", arr);
                }
                Err(_) => {
                    let _ = to_main_tx.send(ToMain::Error("worker init payload failed".into()));
                    let _ = to_main_tx.send(ToMain::Exited(1));
                    return;
                }
            }
        }
        NODE_WORKER_SCOPE_JS
    } else {
        WORKER_SCOPE_JS
    };
    if rt.engine().eval(boot, false).is_err() {
        let _ = to_main_tx.send(ToMain::Error("worker scope bootstrap failed".into()));
        let _ = to_main_tx.send(ToMain::Exited(1));
        return;
    }

    let _ = to_main_tx.send(ToMain::Online);

    let entry_result = if spec.is_eval {
        // `eval: true` — the entry IS the source, run as a classic script (global `require` in
        // scope, like Node's eval workers). Imports resolve against the cwd.
        let base = std::env::current_dir()
            .map(|d| d.join("[worker eval]").to_string_lossy().into_owned())
            .unwrap_or_else(|_| "[worker eval]".to_string());
        rt.eval_worker_entry(&spec.entry, &base, false)
    } else if spec.is_node && !spec.is_module {
        // A node CJS worker file is the realm's main module (require.main, __filename, ...).
        rt.start_main(&spec.entry)
    } else {
        match std::fs::read_to_string(&spec.entry) {
            Ok(source) => rt.eval_worker_entry(&source, &spec.entry, spec.is_module),
            Err(e) => Err(format!("cannot load worker script {}: {e}", spec.entry)),
        }
    };
    if let Err(e) = entry_result {
        let _ = to_main_tx.send(ToMain::Error(e));
        if spec.is_node {
            // Node: an entry that throws kills the worker ('error', then 'exit' with code 1).
            let _ = to_main_tx.send(ToMain::Exited(1));
            return;
        }
    }

    // Arm the main→worker inbox, then pump the loop until stopped — or, in node mode with the
    // inbox unref'd, until nothing else is pending (a natural exit).
    arm_worker_inbox(rt.engine().ctx(), WorkerInbox { rx: to_worker_rx });
    rt.run_worker_loop(&stop);

    let code = rt
        .engine()
        .ctx()
        .op_state()
        .get::<WorkerSelf>()
        .and_then(|w| w.exit_code)
        .unwrap_or(if stop.load(Ordering::SeqCst) { 1 } else { 0 });
    let _ = to_main_tx.send(ToMain::Exited(code));
}

struct WorkerInbox {
    rx: Receiver<ToWorker>,
}

struct WorkerInboxResult {
    bytes: Option<Vec<u8>>,
    inbox: Option<WorkerInbox>,
}

fn arm_worker_inbox(ctx: &mut Ctx, inbox: WorkerInbox) {
    // The JS dispatcher is a stable global installed by the scope bootstrap.
    let global = ctx.global_this();
    let dispatch = match ctx.get_member(&global, "__workerDispatchMessage") {
        Ok(v) if v.is_callable() => v,
        _ => return,
    };
    let keep_alive = ctx
        .op_state()
        .get::<WorkerSelf>()
        .map(|w| w.keep_alive)
        .unwrap_or(true);
    let reg = ctx.host_mut::<TaskRegistry>().expect("task registry installed");
    let task = reg.register(dispatch, None, decode_worker_inbox);
    if !keep_alive {
        reg.set_unref(task);
    }
    if let Some(w) = ctx.host_mut::<WorkerSelf>() {
        w.inbox_task = Some(task);
    }
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

/// `__wself.exit(code)` — node `process.exit(code)` inside a worker: record the code and stop the
/// loop (cooperatively — the current synchronous JS runs to its end first; see the module docs).
fn op_wself_exit(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let code = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as i32;
    if let Some(w) = ctx.host_mut::<WorkerSelf>() {
        w.exit_code = Some(code);
        w.stop.store(true, Ordering::SeqCst);
    }
    Ok(Value::Undefined)
}

/// `__wself.setRef(keep)` — whether the armed inbox keeps this worker alive (parentPort ref
/// semantics: on while a 'message' listener exists, off otherwise).
fn op_wself_set_ref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let keep = matches!(args.first(), Some(Value::Bool(true)));
    let task = match ctx.host_mut::<WorkerSelf>() {
        Some(w) => {
            w.keep_alive = keep;
            w.inbox_task
        }
        None => None,
    };
    if let (Some(task), Some(reg)) = (task, ctx.host_mut::<TaskRegistry>()) {
        if keep {
            reg.set_ref(task);
        } else {
            reg.set_unref(task);
        }
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
                "exit" (1) => op_wself_exit,
                "setRef" (1) => op_wself_set_ref,
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
                "spawn" (4) => op_worker_spawn,
                "post" (2) => op_worker_post,
                "terminate" (1) => op_worker_terminate,
                "setRef" (2) => op_worker_set_ref,
            ],
        )],
        state_init: Some(|state| state.put(WorkerRegistry::default())),
        js_init: Some(WORKER_JS),
        js_init_snapshot: None,
    }
}

/// The main-side web `Worker` class. Captures `__worker` (removing it from global scope, but
/// stashing a hidden handle for the node glue's worker_threads Worker, whose js_init ran earlier
/// and grabs the ops lazily on first use) and drives the message bridge through the
/// structured-clone wire format.
const WORKER_JS: &str = r#"
(() => {
  "use strict";
  const __worker = globalThis.__worker;
  delete globalThis.__worker;
  // node:worker_threads (lumen-node glue) drives the same ops; hand them over via a hidden global.
  Object.defineProperty(globalThis, "__lumenWorkerOps", {
    value: __worker, configurable: true, enumerable: false, writable: false,
  });
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
      this.#id = __worker.spawn(path, isModule, (kind, ...args) => this.#onEvent(kind, args)).id;
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
      // "online" is a node-mode event; the web Worker has no counterpart and ignores it.
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

/// The DedicatedWorkerGlobalScope surface, evaluated inside each *web* worker realm (see
/// `run_worker`).
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

/// The node worker bootstrap, evaluated inside each *node* worker realm: hands the `__wself` ops,
/// the thread id, and the init payload to the hook the node glue installed
/// (`__lumenInitWorkerThread`, see lumen-node/src/js/worker_threads.js), which wires parentPort/
/// workerData/process and returns the inbox dispatcher.
const NODE_WORKER_SCOPE_JS: &str = r#"
(() => {
  "use strict";
  const wself = globalThis.__wself;
  delete globalThis.__wself;
  const threadId = globalThis.__lumenWorkerThreadId;
  delete globalThis.__lumenWorkerThreadId;
  const initBytes = globalThis.__lumenWorkerInit;
  delete globalThis.__lumenWorkerInit;
  const hook = globalThis.__lumenInitWorkerThread;
  if (typeof hook !== "function") throw new Error("node worker glue is not installed");
  globalThis.__workerDispatchMessage = hook(wself, threadId, initBytes);
})();
"#;
