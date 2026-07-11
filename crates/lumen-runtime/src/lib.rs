//! lumen-runtime — the event loop that turns the lumen engine into a runtime.
//!
//! One [`Runtime`] = one engine (one realm) + the installed extensions (timers, console,
//! process) + a threadpool for blocking work. The engine is `!Send`, so the thread that
//! creates the runtime owns it; everything asynchronous funnels back to that thread as
//! either a queued JS callback or an mpsc [`TaskCompletion`].
//!
//! A loop turn, in order (see [`Runtime::run_to_completion`]):
//! 1. drain **microtasks** (promise reactions),
//! 2. run queued **callbacks** (`process.nextTick`, `setImmediate`),
//! 3. fire due **timers**,
//! 4. dispatch ready **completions** from the threadpool,
//! then block on the completion channel until the next timer deadline (or indefinitely if
//! only completions remain). The loop exits when nothing is pending anywhere. A readiness
//! reactor (epoll/kqueue) would need raw syscalls and stays out unless explicitly authorized;
//! threadpool + completions is libuv's own fs strategy and covers everything we host today.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use lumen_host::{
    install, CallbackQueue, CompletionSender, Engine, TaskCompletion, TaskDecoder, TaskRegistry,
    ThreadPool, Value,
};

mod console;
mod esm;
mod jsx;
mod process;
mod worker;

pub use console::{describe_error, render_value, ConsoleOut};
pub use lumen_host::{Completion, Ctx};

/// Workers for blocking work. libuv's default; revisit when async fs lands and has numbers.
const POOL_SIZE: usize = 4;

pub struct Runtime {
    engine: Engine,
    pool: ThreadPool,
    completions: mpsc::Receiver<TaskCompletion>,
    /// The error-reporting shims (see `Runtime::new`): `(error) -> suppressed` for the global
    /// `onerror` convention and `(promise, reason) -> suppressed` for `onunhandledrejection`.
    fire_error: Value,
    fire_rejection: Value,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// An engine with the runtime globals installed: timers, streaming `console`, minimal
    /// `process`, `queueMicrotask`.
    pub fn new() -> Runtime {
        let (tx, rx) = mpsc::channel();
        let pool = ThreadPool::new(POOL_SIZE, tx.clone());
        let mut engine = Engine::new();
        // Substrate first: fs's js_init runs during install and its ops need these.
        engine.ctx().op_state().put(pool.handle());
        // Dedicated-thread completions for unbounded-blocking work (child stdio) that must not
        // occupy a shared pool worker.
        engine.ctx().op_state().put(CompletionSender::new(tx));
        engine.ctx().op_state().put(TaskRegistry::default());
        install(
            &mut engine,
            &[
                lumen_timers::extension(),
                console::extension(),
                process::extension(),
                lumen_fs::extension(),
                lumen_web::extension(),
                // Last: node's glue wraps the fs global, Buffer uses TextEncoder (web), and
                // require() calls process.cwd().
                lumen_node::extension(),
                worker::extension(),
            ],
        );
        process::install_data_props(&mut engine);
        // queueMicrotask, via the promise queue the engine already has. Close enough to spec
        // for v0 (a thrown callback error becomes an unhandled rejection, not a reported
        // exception); a native microtask hook can replace it if that gap ever matters.
        engine
            .eval(
                "globalThis.queueMicrotask = (cb) => {
                    if (typeof cb !== 'function')
                        throw new TypeError('queueMicrotask expects a function');
                    Promise.resolve().then(cb);
                };",
                false,
            )
            .expect("shim parses");
        // The HTML error-reporting globals (WinterTC Minimum Common API §5.2): `onerror` /
        // `onunhandledrejection` global event-handler properties and `reportError`. The fire
        // helpers return whether the default report is suppressed (`onerror` returning `true`;
        // `unhandledrejection`'s `event.preventDefault()`); the loop's uncaught/rejection
        // reporting consults them through handles grabbed (and then unglobaled) below. A throw
        // inside a handler never re-enters it — the original error still default-reports.
        engine
            .eval(
                r#"
                globalThis.onerror = null;
                globalThis.onunhandledrejection = null;
                globalThis.__lumen_fire_error = function (error) {
                    const h = globalThis.onerror;
                    if (typeof h !== 'function') return false;
                    let message = '';
                    try {
                        message =
                            error instanceof Error
                                ? `Uncaught ${error.name}: ${error.message}`
                                : `Uncaught ${String(error)}`;
                    } catch {}
                    try {
                        return h.call(globalThis, message, '', 0, 0, error) === true;
                    } catch {
                        return false;
                    }
                };
                globalThis.__lumen_fire_rejection = function (promise, reason) {
                    const h = globalThis.onunhandledrejection;
                    if (typeof h !== 'function') return false;
                    let prevented = false;
                    const event = {
                        type: 'unhandledrejection',
                        promise,
                        reason,
                        cancelable: true,
                        preventDefault() { prevented = true; },
                        get defaultPrevented() { return prevented; },
                    };
                    try {
                        h.call(globalThis, event);
                    } catch {}
                    return prevented;
                };
                {
                    const fire = globalThis.__lumen_fire_error;
                    const report = console.__reportUncaught;
                    delete console.__reportUncaught;
                    globalThis.reportError = function reportError(e) {
                        if (arguments.length === 0)
                            throw new TypeError('reportError requires at least 1 argument');
                        if (!fire(e)) report(e);
                    };
                }
                "#,
                false,
            )
            .expect("error-reporting shim parses");
        let global = engine.global_this();
        let fire_error = engine
            .ctx()
            .get_member(&global, "__lumen_fire_error")
            .unwrap_or_else(|_| panic!("error-reporting shim installed"));
        let fire_rejection = engine
            .ctx()
            .get_member(&global, "__lumen_fire_rejection")
            .unwrap_or_else(|_| panic!("error-reporting shim installed"));
        engine
            .eval(
                "delete globalThis.__lumen_fire_error; delete globalThis.__lumen_fire_rejection;",
                false,
            )
            .expect("shim cleanup");
        Runtime {
            engine,
            pool,
            completions: rx,
            fire_error,
            fire_rejection,
        }
    }

    /// The engine, for embedder access beyond script evaluation (defining globals, etc.).
    pub fn engine(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Run `path` as a CommonJS program entry (`require.main === module`, with `__dirname`/
    /// `__filename`/`require` in scope), then loop to quiescence. `Err` is the rendered
    /// uncaught error. This is what the CLI uses for `lumen-cli file.js`.
    pub fn run_main(&mut self, path: &str) -> Result<(), String> {
        // A CJS script can still dynamic-`import()`: give the engine the ESM loader and resolve
        // bare relative specifiers against the entry file.
        let loader = esm::make_loader(self.builtin_modules());
        self.engine.set_module_loader_attrs(loader);
        if let Ok(abs) = std::fs::canonicalize(path) {
            self.engine.set_import_base(&abs.to_string_lossy());
        }
        let global = self.engine.global_this();
        let run_main = self
            .engine
            .ctx()
            .get_member(&global, "__runMain")
            .map_err(|_| "node runtime not installed".to_string())?;
        let result = self.engine.call_function(
            &run_main,
            Value::Undefined,
            &[Value::from_string(path.to_string())],
        );
        self.run_to_completion();
        result
            .map(|_| ())
            .map_err(|e| describe_error(self.engine.ctx(), &e))
    }

    /// Run `path` as an ES module: its `import` graph resolves against disk + `node_modules`
    /// (and the `node:` builtins), then the loop runs to quiescence so top-level `await`,
    /// timers, and I/O settle. `Err` is the rendered uncaught error.
    pub fn run_module(&mut self, path: &str) -> Result<(), String> {
        let source =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        // A `.jsx` entry is lowered to plain JS before the engine parses it.
        let source = if path.ends_with(".jsx") {
            jsx::transform(&source).map_err(|e| format!("JSX transform failed for {path}: {e}"))?
        } else {
            source
        };
        let key = std::fs::canonicalize(path)
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .into_owned();
        let loader = esm::make_loader(self.builtin_modules());
        let result = self.engine.eval_module_attrs(&source, &key, loader);
        self.run_to_completion();
        match result {
            Ok(Completion::Value(_)) => Ok(()),
            Ok(Completion::Throw { name, message }) => Err(if name.is_empty() {
                message
            } else {
                format!("{name}: {message}")
            }),
            Err(e) => Err(format!("SyntaxError: {} (line {})", e.message, e.line)),
        }
    }

    /// Evaluate a Worker entry `source` (a module when `is_module`, else a classic script) WITHOUT
    /// running the loop — the caller arms the message inbox first, then pumps the loop itself so
    /// the worker stays alive for messages. `base` seeds relative-import resolution. `Err` is the
    /// rendered load/parse/top-level error.
    pub fn eval_worker_entry(&mut self, source: &str, base: &str, is_module: bool) -> Result<(), String> {
        let loader = esm::make_loader(self.builtin_modules());
        self.engine.set_module_loader_attrs(loader);
        self.engine.set_import_base(base);
        let result = if is_module {
            let loader = esm::make_loader(self.builtin_modules());
            self.engine.eval_module_attrs(source, base, loader)
        } else {
            self.engine.eval(source, false)
        };
        // Drain the microtask checkpoint from top-level code, but not the macrotask loop.
        self.engine.run_microtasks();
        match result {
            Ok(Completion::Value(_)) => Ok(()),
            Ok(Completion::Throw { name, message }) => {
                Err(if name.is_empty() { message } else { format!("{name}: {message}") })
            }
            Err(e) => Err(format!("SyntaxError: {} (line {})", e.message, e.line)),
        }
    }

    /// Like [`run_to_completion`](Runtime::run_to_completion), but for a Worker: it does NOT exit
    /// when idle (the armed message inbox keeps it alive), and it returns promptly when `stop` is
    /// set — a cooperative terminate that also drops any still-pending timers. The blocking wait
    /// polls `stop` on a short interval so a `terminate()` from another thread is noticed even
    /// with no message or timer due.
    pub fn run_worker_loop(&mut self, stop: &std::sync::atomic::AtomicBool) {
        use std::sync::atomic::Ordering;
        let poll = Duration::from_millis(50);
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            self.engine.run_microtasks();
            self.report_unhandled_rejections();
            loop {
                let mut progressed = false;
                for (cb, args) in self.take_queued_callbacks() {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                for (cb, args) in self.take_due_timers() {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                while let Ok(done) = self.completions.try_recv() {
                    progressed = true;
                    self.dispatch(done);
                }
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if !progressed {
                    break;
                }
            }
            if stop.load(Ordering::SeqCst) || self.idle() {
                return;
            }
            let wait = match self.next_timer_deadline() {
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline <= now {
                        continue;
                    }
                    (deadline - now).min(poll)
                }
                None => poll,
            };
            if let Ok(done) = self.completions.recv_timeout(wait) {
                self.dispatch(done);
            }
        }
    }

    /// Pull the JS-precomputed synthetic ESM source for each `node:` builtin out of the engine
    /// (the loader can't enumerate a builtin's exports from Rust; see the node module glue).
    fn builtin_modules(&mut self) -> esm::BuiltinModules {
        let global = self.engine.global_this();
        let ctx = self.engine.ctx();
        let mut map = std::collections::HashMap::new();
        let names = ctx
            .get_member(&global, "__builtinNames")
            .ok()
            .and_then(|v| ctx.coerce_string(&v).ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let sources = ctx.get_member(&global, "__esmBuiltinSources").ok();
        if let Some(sources) = sources {
            for name in names.split(',').filter(|s| !s.is_empty()) {
                let key = format!("node:{name}");
                if let Ok(src) = ctx.get_member(&sources, &key) {
                    if let Ok(src) = ctx.coerce_string(&src) {
                        map.insert(key, src.to_string());
                    }
                }
            }
        }
        esm::BuiltinModules(map)
    }

    /// Evaluate a script, then run the event loop until quiescent — timers fired, spawned
    /// work completed, promise queue empty.
    pub fn eval(&mut self, src: &str) -> Result<Completion, lumen_host::ParseError> {
        let result = self.engine.eval(src, false);
        self.run_to_completion();
        result
    }

    /// Spawn blocking work on the pool; when it finishes, `decode` turns its payload into
    /// arguments and `callback` runs on the loop thread. This is the pattern async fs ops
    /// will use from inside native fns (via the `SpawnHandle`/`TaskRegistry` in `OpState`).
    pub fn spawn_blocking(
        &mut self,
        work: impl FnOnce() -> Box<dyn std::any::Any + Send> + Send + 'static,
        callback: Value,
        decode: TaskDecoder,
    ) {
        let registry = self
            .engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .expect("installed in new()");
        let id = registry.register(callback, None, decode);
        self.pool.spawn_blocking(id, work);
    }

    /// Run the loop until nothing is pending: no microtasks, no queued callbacks, no live
    /// timers, no in-flight tasks.
    pub fn run_to_completion(&mut self) {
        loop {
            // Run everything already runnable. Each JS entry is followed by a microtask
            // checkpoint, matching the "after every macrotask" model.
            self.engine.run_microtasks();
            self.report_unhandled_rejections();
            loop {
                let mut progressed = false;
                for (cb, args) in self.take_queued_callbacks() {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                for (cb, args) in self.take_due_timers() {
                    progressed = true;
                    self.fire(&cb, &args);
                }
                while let Ok(done) = self.completions.try_recv() {
                    progressed = true;
                    self.dispatch(done);
                }
                if !progressed {
                    break;
                }
            }

            if self.idle() {
                return;
            }

            // Blocked: only a timer deadline or a task completion can make progress now.
            match self.next_timer_deadline() {
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline > now {
                        if let Ok(done) = self.completions.recv_timeout(deadline - now) {
                            self.dispatch(done);
                        }
                    }
                }
                None => match self.completions.recv() {
                    Ok(done) => self.dispatch(done),
                    // The pool is gone (unreachable while `self.pool` lives); nothing can
                    // ever complete, so pending tasks are abandoned rather than spun on.
                    Err(_) => return,
                },
            }
        }
    }

    fn idle(&mut self) -> bool {
        if self.engine.has_pending_jobs() {
            return false;
        }
        let state = self.engine.ctx().op_state();
        let callbacks_queued = state
            .get::<CallbackQueue>()
            .is_some_and(|q| !q.queue.is_empty());
        let timers_pending = state
            .get::<lumen_timers::Timers>()
            .is_some_and(|t| t.has_pending());
        let tasks_pending = state.get::<TaskRegistry>().is_some_and(|r| r.has_ref_pending());
        !callbacks_queued && !timers_pending && !tasks_pending
    }

    fn take_queued_callbacks(&mut self) -> Vec<(Value, Vec<Value>)> {
        match self.engine.ctx().host_mut::<CallbackQueue>() {
            Some(q) => std::mem::take(&mut q.queue).into(),
            None => Vec::new(),
        }
    }

    fn take_due_timers(&mut self) -> Vec<(Value, Vec<Value>)> {
        let now = Instant::now();
        match self.engine.ctx().host_mut::<lumen_timers::Timers>() {
            Some(t) => t.take_due(now),
            None => Vec::new(),
        }
    }

    fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.engine
            .ctx()
            .host_mut::<lumen_timers::Timers>()?
            .next_deadline()
    }

    /// Settle one completed task: decode the payload, then run the success callback — or the
    /// failure one (a promise's reject) when the decoder says the work failed.
    fn dispatch(&mut self, done: TaskCompletion) {
        let entry = self
            .engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .and_then(|r| r.take(done.task));
        let Some(entry) = entry else {
            return; // cancelled while in flight
        };
        match (entry.decode)(self.engine.ctx(), done.result) {
            Ok(args) => self.fire(&entry.on_ok, &args),
            Err(e) => match &entry.on_err {
                Some(reject) => self.fire(reject, std::slice::from_ref(&e)),
                None => self.report_uncaught(&e),
            },
        }
        self.engine.run_microtasks();
        self.report_unhandled_rejections();
    }

    /// One JS callback entry: call, report an uncaught throw, then the microtask checkpoint.
    fn fire(&mut self, callback: &Value, args: &[Value]) {
        if let Err(e) = self.engine.call_function(callback, Value::Undefined, args) {
            self.report_uncaught(&e);
        }
        self.engine.run_microtasks();
        self.report_unhandled_rejections();
    }

    /// An exception escaped a loop-fired callback. Node prints and keeps the loop alive (we
    /// don't model `process.on('uncaughtException')`/exit semantics yet).
    fn report_uncaught(&mut self, error: &Value) {
        // HTML "report an exception": the global `onerror` handler runs first; returning `true`
        // suppresses the default report.
        let fire = self.fire_error.clone();
        if let Ok(Value::Bool(true)) =
            self.engine
                .call_function(&fire, Value::Undefined, std::slice::from_ref(error))
        {
            return;
        }
        let text = console::describe_error(self.engine.ctx(), error);
        console::write_err_line(self.engine.ctx(), format!("Uncaught {text}"));
    }

    /// Report promises rejected without a handler (Node prints `Uncaught (in promise) …`). Called
    /// after each microtask checkpoint; a rejection handled in the same checkpoint won't appear.
    fn report_unhandled_rejections(&mut self) {
        for (promise, reason) in self.engine.take_unhandled_rejections_full() {
            // The global `onunhandledrejection` handler runs first; `event.preventDefault()`
            // suppresses the default report.
            let fire = self.fire_rejection.clone();
            if let Ok(Value::Bool(true)) =
                self.engine
                    .call_function(&fire, Value::Undefined, &[promise, reason.clone()])
            {
                continue;
            }
            let text = console::describe_error(self.engine.ctx(), &reason);
            console::write_err_line(self.engine.ctx(), format!("Uncaught (in promise) {text}"));
        }
    }
}

#[cfg(test)]
mod crypto_asym_tests;
#[cfg(test)]
mod tests;
