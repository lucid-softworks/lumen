//! lumen-host — the substrate shared by every op crate and the runtime.
//!
//! An *op crate* (timers, fs, ...) exports one [`Extension`]: a named bundle of native
//! functions plus host-state initialization. A runtime is assembled by [`install`]ing a list
//! of extensions into an [`Engine`]. Rust state never lives in the native fns themselves
//! (they are bare `fn` pointers): it lives in [`OpState`], reached through the `&mut Ctx`
//! argument every native fn receives.
//!
//! Two scheduling primitives serve every async op (this mirrors libuv's own fs strategy —
//! regular files are not pollable, so async fs is threadpool + completion, not readiness):
//! - [`ThreadPool::spawn_blocking`]: run blocking work off-thread; its result comes back to
//!   the loop thread as a [`TaskCompletion`] over `mpsc`.
//! - [`CallbackQueue`]: loop-thread-local queue of JS callbacks to fire on the next turn
//!   (JS values are `!Send`, so they never cross threads; off-thread work refers to its
//!   callback by [`TaskId`]).

use std::any::Any;
use std::collections::VecDeque;
use std::sync::mpsc;

pub use lumen::bytecode::Tier;
pub use lumen::embed::{Ctx, NativeClosure, NativeFn, OpState, ResourceId, ResourceTable, Value};
pub use lumen::{Completion, Engine, ParseError};

/// DEFLATE/zlib/gzip codec (std-only), shared by web CompressionStream and node:zlib.
pub mod deflate;

/// One native op: a named native function with its JS arity.
#[derive(Clone, Copy)]
pub struct OpDecl {
    pub name: &'static str,
    pub len: usize,
    pub f: NativeFn,
}

/// Declarative op-declaration table: `ops!["add" (2) => add_impl, ...]`. Uniform by design so
/// op registration stays a table, never hand-written glue (deno's `#[op2]` lesson).
#[macro_export]
macro_rules! ops {
    ($($name:literal ($len:expr) => $f:expr),* $(,)?) => {
        &[$($crate::OpDecl { name: $name, len: $len, f: $f }),*]
    };
}

/// A bundle of native ops + host-state init, exported one per op crate. Composing a runtime
/// is `install(&mut engine, &[timers::extension(), fs::extension(), ...])`.
pub struct Extension {
    pub name: &'static str,
    /// Installed as `globalThis.<name>` functions (e.g. `setTimeout`).
    pub globals: &'static [OpDecl],
    /// Installed as `globalThis.<ns>.<name>` namespace methods (e.g. a `__lumen_fs` ops
    /// object that a JS shim wraps into the public API).
    pub namespaces: &'static [(&'static str, &'static [OpDecl])],
    /// Installs this extension's state (timer heap, fd table, ...) into [`OpState`].
    pub state_init: Option<fn(&mut OpState)>,
    /// JS glue evaluated after this extension's ops are installed — the promise-returning
    /// public API is JS wrapping raw callback ops (e.g. `fs.promises` over `__fs_async`).
    /// A parse/throw here is a bug in the extension: `install` panics with its name.
    pub js_init: Option<&'static str>,
    /// A build-time snapshot of `js_init`'s parsed AST (see `Engine::eval_snapshot`). When
    /// present, `install` decodes it instead of re-lexing/parsing `js_init` every boot — the
    /// dominant cold-start cost. A decode failure (version skew) falls back to `js_init`, so it
    /// is a pure optimization. `js_init` must still be set (the fallback source).
    pub js_init_snapshot: Option<&'static [u8]>,
}

impl Extension {
    /// An empty extension named `name`; fill in the fields that apply.
    pub const fn new(name: &'static str) -> Extension {
        Extension {
            name,
            globals: &[],
            namespaces: &[],
            state_init: None,
            js_init: None,
            js_init_snapshot: None,
        }
    }
}

/// Install extensions into an engine: state first (an op may fire during install), then ops,
/// then JS glue.
pub fn install(engine: &mut Engine, extensions: &[Extension]) {
    for ext in extensions {
        if let Some(init) = ext.state_init {
            init(engine.ctx().op_state());
        }
        for op in ext.globals {
            engine.define_global(op.name, op.len, op.f);
        }
        for (ns, ops) in ext.namespaces {
            let table: Vec<(&str, usize, NativeFn)> =
                ops.iter().map(|o| (o.name, o.len, o.f)).collect();
            engine.define_namespace(ns, &table);
        }
        if let Some(src) = ext.js_init {
            // Prefer the precompiled snapshot (skips lex+parse); on a decode failure fall back to
            // parsing the source, so the snapshot can never change behavior — only speed.
            let completion = ext
                .js_init_snapshot
                .and_then(|bytes| engine.eval_snapshot(bytes, false).ok())
                .map(Ok)
                .unwrap_or_else(|| engine.eval(src, false));
            match completion {
                Ok(Completion::Value(_)) => {}
                Ok(Completion::Throw { name, message }) => {
                    panic!("extension '{}' js_init threw {name}: {message}", ext.name)
                }
                Err(e) => panic!(
                    "extension '{}' js_init: SyntaxError: {}",
                    ext.name, e.message
                ),
            }
        }
    }
}

/// Identifies an in-flight async task. The op that spawns work registers `TaskId -> JS
/// callback/promise` in its own [`OpState`] slot; the completion carries the id back so the
/// loop thread can look the JS value up (JS values themselves are `!Send`).
pub type TaskId = u64;

/// What off-thread work sends back to the loop thread. `result` is whatever `Send` payload
/// the spawning op chose; that op downcasts it when the loop hands the completion over.
pub struct TaskCompletion {
    pub task: TaskId,
    pub result: Box<dyn Any + Send>,
}

/// JS callbacks queued (on the loop thread) to run on the next loop turn — the
/// `enqueue_callback` primitive. Lives in [`OpState`]; the runtime drains it each turn.
#[derive(Default)]
pub struct CallbackQueue {
    pub queue: VecDeque<(Value, Vec<Value>)>,
}

impl CallbackQueue {
    /// Queue `callback(args...)` for the next loop turn.
    pub fn enqueue(state: &mut OpState, callback: Value, args: Vec<Value>) {
        if !state.has::<CallbackQueue>() {
            state.put(CallbackQueue::default());
        }
        state
            .get_mut::<CallbackQueue>()
            .expect("just installed")
            .queue
            .push_back((callback, args));
    }
}

struct Task {
    id: TaskId,
    work: Box<dyn FnOnce() -> Box<dyn Any + Send> + Send>,
}

/// A fixed pool of std worker threads running blocking work (`std::fs`, blocking
/// `std::net`); completions come back over the `mpsc` channel given at construction. This is
/// the whole async-I/O story until (if ever) a hand-rolled readiness reactor on raw platform
/// syscalls is explicitly authorized — never via a crate.
pub struct ThreadPool {
    work_tx: Option<mpsc::Sender<Task>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl ThreadPool {
    /// `size` worker threads sending [`TaskCompletion`]s to `completions` (the loop thread
    /// holds the receiving end).
    pub fn new(size: usize, completions: mpsc::Sender<TaskCompletion>) -> ThreadPool {
        let (work_tx, work_rx) = mpsc::channel::<Task>();
        // std's mpsc receiver is single-consumer: share it across workers behind a mutex.
        let work_rx = std::sync::Arc::new(std::sync::Mutex::new(work_rx));
        let workers = (0..size.max(1))
            .map(|_| {
                let work_rx = std::sync::Arc::clone(&work_rx);
                let completions = completions.clone();
                std::thread::spawn(move || loop {
                    let task = match work_rx.lock().expect("worker queue poisoned").recv() {
                        Ok(t) => t,
                        Err(_) => return, // pool dropped: no more work
                    };
                    let result = (task.work)();
                    // The loop shutting down first is fine; the result just has nowhere to go.
                    let _ = completions.send(TaskCompletion {
                        task: task.id,
                        result,
                    });
                })
            })
            .collect();
        ThreadPool {
            work_tx: Some(work_tx),
            workers,
        }
    }

    /// Run `work` on a pool thread; its return value comes back to the loop as a
    /// [`TaskCompletion`] tagged with `id`.
    pub fn spawn_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        self.work_tx
            .as_ref()
            .expect("pool shut down")
            .send(Task {
                id,
                work: Box::new(work),
            })
            .expect("worker threads gone");
    }

    /// A cloneable spawn handle. The runtime puts one in [`OpState`], which is how a native fn
    /// (holding only `&mut Ctx`) reaches the pool.
    pub fn handle(&self) -> SpawnHandle {
        SpawnHandle {
            work_tx: self.work_tx.clone().expect("pool shut down"),
        }
    }
}

/// [`ThreadPool::spawn_blocking`] as an [`OpState`]-storable handle, so op crates can spawn
/// blocking work from inside a native fn.
#[derive(Clone)]
pub struct SpawnHandle {
    work_tx: mpsc::Sender<Task>,
}

impl SpawnHandle {
    pub fn spawn_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        self.work_tx
            .send(Task {
                id,
                work: Box::new(work),
            })
            .expect("worker threads gone");
    }
}

/// Sends [`TaskCompletion`]s straight to the loop from a *dedicated* thread, bypassing the fixed
/// [`ThreadPool`]. For work that blocks for an unbounded time — a subprocess's stdout read, waiting
/// on a child to exit — where occupying a shared pool worker for the whole duration would starve
/// everything else. The runtime stores one in [`OpState`]. `run_blocking` spawns a fresh thread per
/// call; blocked threads cost only memory, not a pool slot.
#[derive(Clone)]
pub struct CompletionSender {
    tx: mpsc::Sender<TaskCompletion>,
}

impl CompletionSender {
    pub fn new(tx: mpsc::Sender<TaskCompletion>) -> CompletionSender {
        CompletionSender { tx }
    }
    /// Run `work` on a new dedicated thread; its result comes back to the loop as a
    /// [`TaskCompletion`] tagged with `id` (settled through the [`TaskRegistry`], like pool work).
    pub fn run_blocking(
        &self,
        id: TaskId,
        work: impl FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = work();
            let _ = tx.send(TaskCompletion { task: id, result });
        });
    }
}

/// Turns a completed task's `Send` payload back into JS callback arguments, on the loop
/// thread. `Err` is a JS value to report as an uncaught exception (later: a rejection).
pub type TaskDecoder = fn(&mut Ctx, Box<dyn Any + Send>) -> Result<Vec<Value>, Value>;

/// In-flight async tasks: `TaskId -> (JS callback, payload decoder)`. Lives in [`OpState`];
/// the op that spawns work registers here, the loop settles from [`TaskCompletion`]s. The
/// event loop stays alive while this is non-empty.
#[derive(Default)]
pub struct TaskRegistry {
    next: TaskId,
    map: std::collections::HashMap<TaskId, TaskEntry>,
}

/// How to settle one in-flight task: success callback, optional failure callback (a promise's
/// reject — when absent, a decode error is reported as an uncaught exception), and the
/// payload decoder.
pub struct TaskEntry {
    pub on_ok: Value,
    pub on_err: Option<Value>,
    pub decode: TaskDecoder,
    /// An `unref`'d task still settles when it completes, but does not by itself keep the event
    /// loop alive (Node's `child.unref()` — e.g. esbuild's persistent service child).
    pub unref: bool,
}

impl TaskRegistry {
    /// Reserve an id for work about to be spawned, remembering how to settle it.
    pub fn register(&mut self, on_ok: Value, on_err: Option<Value>, decode: TaskDecoder) -> TaskId {
        let id = self.next;
        self.next += 1;
        self.map.insert(
            id,
            TaskEntry {
                on_ok,
                on_err,
                decode,
                unref: false,
            },
        );
        id
    }
    /// Claim a completed task's settlement entry (a missing id means it was cancelled).
    pub fn take(&mut self, id: TaskId) -> Option<TaskEntry> {
        self.map.remove(&id)
    }
    /// Mark a pending task as `unref`'d (see [`TaskEntry::unref`]).
    pub fn set_unref(&mut self, id: TaskId) {
        if let Some(e) = self.map.get_mut(&id) {
            e.unref = true;
        }
    }
    /// Re-`ref` a pending task so it keeps the loop alive again (Node's `handle.ref()`).
    pub fn set_ref(&mut self, id: TaskId) {
        if let Some(e) = self.map.get_mut(&id) {
            e.unref = false;
        }
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// Whether any *ref*'d (loop-keeping) task is pending. Unref'd tasks are ignored — they
    /// settle if they complete but must not hold the process open.
    pub fn has_ref_pending(&self) -> bool {
        self.map.values().any(|e| !e.unref)
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        // Closing the work channel ends each worker's recv loop; join so no worker outlives
        // the runtime that owns the completion receiver.
        self.work_tx.take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests;
