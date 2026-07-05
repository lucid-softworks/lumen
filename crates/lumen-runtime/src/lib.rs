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
use std::time::Instant;

use lumen_host::{
    install, CallbackQueue, Engine, TaskCompletion, TaskDecoder, TaskRegistry, ThreadPool, Value,
};

mod console;
mod process;

pub use console::ConsoleOut;
pub use lumen_host::{Completion, Ctx};

/// Workers for blocking work. libuv's default; revisit when async fs lands and has numbers.
const POOL_SIZE: usize = 4;

pub struct Runtime {
    engine: Engine,
    pool: ThreadPool,
    completions: mpsc::Receiver<TaskCompletion>,
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
        let pool = ThreadPool::new(POOL_SIZE, tx);
        let mut engine = Engine::new();
        install(
            &mut engine,
            &[
                lumen_timers::extension(),
                console::extension(),
                process::extension(),
            ],
        );
        engine.ctx().op_state().put(pool.handle());
        engine.ctx().op_state().put(TaskRegistry::default());
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
        Runtime {
            engine,
            pool,
            completions: rx,
        }
    }

    /// The engine, for embedder access beyond script evaluation (defining globals, etc.).
    pub fn engine(&mut self) -> &mut Engine {
        &mut self.engine
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
        let id = registry.register(callback, decode);
        self.pool.spawn_blocking(id, work);
    }

    /// Run the loop until nothing is pending: no microtasks, no queued callbacks, no live
    /// timers, no in-flight tasks.
    pub fn run_to_completion(&mut self) {
        loop {
            // Run everything already runnable. Each JS entry is followed by a microtask
            // checkpoint, matching the "after every macrotask" model.
            self.engine.run_microtasks();
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
        let tasks_pending = state.get::<TaskRegistry>().is_some_and(|r| !r.is_empty());
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

    /// Settle one completed task: decode the payload and run its JS callback.
    fn dispatch(&mut self, done: TaskCompletion) {
        let entry = self
            .engine
            .ctx()
            .host_mut::<TaskRegistry>()
            .and_then(|r| r.take(done.task));
        let Some((callback, decode)) = entry else {
            return; // cancelled while in flight
        };
        match decode(self.engine.ctx(), done.result) {
            Ok(args) => self.fire(&callback, &args),
            Err(e) => self.report_uncaught(&e),
        }
        self.engine.run_microtasks();
    }

    /// One JS callback entry: call, report an uncaught throw, then the microtask checkpoint.
    fn fire(&mut self, callback: &Value, args: &[Value]) {
        if let Err(e) = self.engine.call_function(callback, Value::Undefined, args) {
            self.report_uncaught(&e);
        }
        self.engine.run_microtasks();
    }

    /// An exception escaped a loop-fired callback. Node prints and keeps the loop alive (we
    /// don't model `process.on('uncaughtException')`/exit semantics yet).
    fn report_uncaught(&mut self, error: &Value) {
        let text = console::describe_error(self.engine.ctx(), error);
        console::write_err_line(self.engine.ctx(), format!("Uncaught {text}"));
    }
}

#[cfg(test)]
mod tests;
