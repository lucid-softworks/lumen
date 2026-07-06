//! Stackful coroutines for generators (and async functions), built on OS threads.
//!
//! lumen is a tree-walking interpreter, so suspending a generator mid-body means parking its native
//! call stack. Each live coroutine runs on an OS thread; control is handed back and forth with a
//! pair of channels in strict ping-pong — exactly one of {driver, coroutine thread} runs at any
//! instant, the other parked in `recv`. The shared [`Interp`] is therefore never touched
//! concurrently, which is why shuttling a `*mut Interp` across the thread boundary (see
//! [`InterpPtr`]) is sound in practice.
//!
//! Worker threads are **pooled**: spawning a fresh thread per async call / generator was ~30µs
//! (measured), dominating async cost, so finished workers return to an idle pool and are handed the
//! next coroutine instead. The pool grows to the high-water mark of concurrently *live* coroutines
//! and is reused across calls; only the ~6µs per-suspend channel handoff remains.
//!
//! The running coroutine's channels live in a thread-local [`YIELDER`], so a `yield` buried deep in
//! eval finds the right channel and nested coroutines (each on their own worker) need no extra
//! bookkeeping — every thread reads its own thread-local.
//!
//! Address stability: a coroutine never outlives the `Engine` that owns the interpreter, and that
//! `Engine` is not moved between the `eval` calls that create and drive the coroutine, so the
//! captured pointer stays valid for the coroutine's whole life.

use std::cell::RefCell;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use crate::interpreter::Interp;
use crate::value::Value;

/// Driver → generator: resume the body.
pub enum Resume {
    /// `next(v)` — the `yield` expression evaluates to `v`.
    Next(Value),
    /// `return(v)` — inject a return completion at the suspended `yield`.
    Return(Value),
    /// `throw(e)` — inject a throw at the suspended `yield`.
    Throw(Value),
}

// `Resume`/`Suspend` carry `Value`s (which hold non-`Send` `Rc`s). Transferring them across the
// channel is sound because of the strict ping-pong: a value is produced on one side only after the
// other side has parked, so it is never touched on two threads at once.
unsafe impl Send for Resume {}
unsafe impl Send for Suspend {}

/// Generator → driver: the body parked or finished.
pub enum Suspend {
    /// `yield v` — parked, produced `v` (a generator value).
    Yield(Value),
    /// `await v` — parked waiting for `v` to settle (async functions/generators).
    Await(Value),
    /// The body ran to completion / `return v`.
    Done(Value),
    /// The body threw `e` and it escaped.
    Throw(Value),
}

/// A `*mut Interp` carried to the generator thread. Sound only under the strict ping-pong handoff:
/// when the generator thread dereferences it the driver is parked (not touching the interpreter),
/// and vice versa, so the two `&mut` reborrows are never *used* concurrently.
pub struct InterpPtr(pub *mut Interp);
unsafe impl Send for InterpPtr {}

/// The generator body, boxed. It captures `Rc`s (the function + its scope) so it is not really
/// `Send`; the strict handoff makes moving it to the worker thread sound.
pub struct SendBody(pub Box<dyn FnOnce(&mut Interp) -> Suspend>);
unsafe impl Send for SendBody {}

/// The generator-thread side of the channels, kept in the worker thread's TLS.
struct Yielder {
    suspend_tx: Sender<Suspend>,
    resume_rx: Receiver<Resume>,
}

thread_local! {
    static YIELDER: RefCell<Option<Yielder>> = const { RefCell::new(None) };
    /// Set on the coroutine thread when the body is an *async* generator, so `yield` knows to
    /// `Await` its operand (AsyncGeneratorYield) before suspending.
    static ASYNC_GEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread is executing a generator body (so `yield` is legal here).
pub fn in_coroutine() -> bool {
    YIELDER.with(|y| y.borrow().is_some())
}

/// Mark the running coroutine thread as an async generator body.
pub fn set_async_gen(v: bool) {
    ASYNC_GEN.with(|c| c.set(v));
}

/// Whether the running coroutine is an async generator (its `yield` awaits the operand).
pub fn in_async_gen() -> bool {
    ASYNC_GEN.with(|c| c.get())
}

/// The driver side of one coroutine, stored on the generator object in `Interp.generators`. Either
/// an OS-thread-backed coroutine (generators, and async bodies the bytecode compiler declined) or a
/// bytecode [`VmCoro`](crate::bytecode::VmCoro) (async bodies that compile) — both drive the same
/// way, so `drive_async`/`drive_generator` are agnostic.
pub enum Coroutine {
    Thread(ThreadCoro),
    Vm(crate::bytecode::VmCoro),
}

impl Coroutine {
    #[inline]
    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        match self {
            Coroutine::Thread(c) => c.resume(i, signal),
            Coroutine::Vm(c) => c.resume(i, signal),
        }
    }
    /// Whether the body has finished (further resumes are no-ops).
    #[inline]
    pub fn done(&self) -> bool {
        match self {
            Coroutine::Thread(c) => c.done,
            Coroutine::Vm(c) => c.done,
        }
    }
    /// Whether the first resume has happened (distinguishes suspendedStart from a suspended yield).
    #[inline]
    pub fn started(&self) -> bool {
        match self {
            Coroutine::Thread(c) => c.started,
            Coroutine::Vm(c) => c.started,
        }
    }
}

/// An OS-thread-backed coroutine (a pooled worker runs the body; see [`spawn_coroutine`]).
pub struct ThreadCoro {
    resume_tx: Sender<Resume>,
    suspend_rx: Receiver<Suspend>,
    /// Set once the body has finished (Done/Throw); further resumes are no-ops.
    pub done: bool,
    /// Set on the first resume — distinguishes "suspendedStart" from a suspended yield.
    pub started: bool,
}

impl ThreadCoro {
    /// Hand control to the generator and block until it next parks or finishes. Saves/restores the
    /// interpreter's scalar execution context (`strict`, recursion `depth`, tail-call eligibility
    /// `tco_ok`) across the handoff so the driver and the body don't clobber each other's. `tco_ok`
    /// matters because a coroutine body executes outside `Interp::call`'s tail-call trampoline: if a
    /// leaked `tco_ok == true` reached an `async`/generator body, its `return f(...)` would be parked
    /// as a pending tail call that nothing ever runs, and the body would resolve to `undefined`.
    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        self.started = true;
        let (saved_strict, saved_depth, saved_tco) = (i.strict, i.depth, i.tco_ok);
        let _ = self.resume_tx.send(signal);
        let s = self.suspend_rx.recv();
        i.strict = saved_strict;
        i.depth = saved_depth;
        i.tco_ok = saved_tco;
        match s {
            Ok(s) => {
                if matches!(s, Suspend::Done(_) | Suspend::Throw(_)) {
                    self.done = true;
                }
                s
            }
            // The worker died (panicked) — treat as a finished generator.
            Err(_) => {
                self.done = true;
                Suspend::Done(Value::Undefined)
            }
        }
    }
}

/// Park the running coroutine, hand `msg` (a `Yield` or `Await`) to the driver, and block until
/// resumed. Restores the body's scalar context (which the driver mutated while it ran).
fn park(i: &mut Interp, msg: Suspend) -> Resume {
    let (gen_strict, gen_depth, gen_tco) = (i.strict, i.depth, i.tco_ok);
    let resumed = YIELDER.with(|y| {
        let b = y.borrow();
        let yl = b.as_ref().expect("suspend outside a coroutine");
        let _ = yl.suspend_tx.send(msg);
        yl.resume_rx.recv()
    });
    match resumed {
        Ok(r) => {
            i.strict = gen_strict;
            i.depth = gen_depth;
            i.tco_ok = gen_tco;
            r
        }
        // `recv` errors only when the driver's `resume_tx` was dropped — i.e. the `Coroutine` (and
        // usually the owning `Engine`) is being torn down. Resuming the body here would run JS
        // (`finally` blocks, iterator-close) that dereferences the shared `*mut Interp` from this
        // thread while the main thread tears it down — a data race that surfaced as random
        // `RefCell already borrowed` panics / SIGSEGV. Instead, never touch the interpreter again:
        // park forever, holding only the captured `Rc`s. The detached thread is reaped at process
        // exit (the generator never outlives its `Engine`).
        Err(_) => loop {
            std::thread::park();
        },
    }
}

/// `yield value` — park producing a generator value.
pub fn coroutine_yield(i: &mut Interp, value: Value) -> Resume {
    park(i, Suspend::Yield(value))
}

/// `await value` — park waiting for `value` to settle.
pub fn coroutine_await(i: &mut Interp, value: Value) -> Resume {
    park(i, Suspend::Await(value))
}

/// Thrown as a JS `Error` when a coroutine cannot start (wasm32 has no OS threads, so
/// `std::thread::Builder::spawn` reports `Unsupported` there).
pub const UNSUPPORTED_MSG: &str =
    "generators and async functions require OS threads, which this WebAssembly build does not have";

/// One unit of work for a pooled worker: the interpreter pointer, the body to run, and this
/// coroutine's channel ends. All fields are `Send` (via the `unsafe impl`s above / channel `Send`),
/// so `Job` is `Send`; moving the captured `Rc`s to the worker is sound for the same ping-pong
/// reason `spawn` was — the driver stops touching them the instant it sends.
struct Job {
    ptr: InterpPtr,
    body: SendBody,
    resume_rx: Receiver<Resume>,
    suspend_tx: Sender<Suspend>,
}

/// Idle worker threads waiting for their next coroutine. Guarded by a plain `Mutex`: under the
/// strict ping-pong exactly one thread touches the interpreter at a time, so contention is
/// near-zero (a worker only pushes itself back *after* handing its final value to the driver).
static IDLE: Mutex<Vec<Sender<Job>>> = Mutex::new(Vec::new());

/// Grab an idle worker, or start a new one. `Err` when the platform cannot spawn threads (wasm32).
fn get_worker() -> std::io::Result<Sender<Job>> {
    if let Some(tx) = IDLE.lock().unwrap().pop() {
        return Ok(tx);
    }
    let (job_tx, job_rx) = channel::<Job>();
    let self_tx = job_tx.clone();
    std::thread::Builder::new()
        // Generous stack: the tree-walker recurses up to MAX_EVAL_DEPTH (1500) frames.
        .stack_size(64 * 1024 * 1024)
        .spawn(move || worker_loop(job_rx, self_tx))?;
    Ok(job_tx)
}

/// A pooled worker: run one coroutine to completion, return to the idle pool, repeat. A worker that
/// parks forever (its `Engine` was torn down while it was suspended; see `park`) simply never comes
/// back — the same leak-at-teardown as the pre-pool one-thread-per-coroutine design.
fn worker_loop(job_rx: Receiver<Job>, self_tx: Sender<Job>) {
    while let Ok(job) = job_rx.recv() {
        run_job(job);
        IDLE.lock().unwrap().push(self_tx.clone());
    }
}

/// Set up this thread's coroutine TLS, run the body from its first drive to completion, and hand
/// the outcome to the driver. Resets the per-thread coroutine state a reused worker would otherwise
/// inherit from its previous job.
fn run_job(job: Job) {
    let Job {
        ptr,
        body,
        resume_rx,
        suspend_tx,
    } = job;
    let SendBody(body) = body;
    // A plain async body never sets ASYNC_GEN, so it must not inherit a previous async-generator
    // job's `true`.
    ASYNC_GEN.with(|c| c.set(false));
    YIELDER.with(|y| {
        *y.borrow_mut() = Some(Yielder {
            suspend_tx: suspend_tx.clone(),
            resume_rx,
        })
    });
    // Park until the first next()/return()/throw(); the body doesn't run before then.
    let first = YIELDER.with(|y| y.borrow().as_ref().unwrap().resume_rx.recv());
    let outcome = match first {
        Err(_) => {
            YIELDER.with(|y| *y.borrow_mut() = None);
            return; // dropped before first drive
        }
        Ok(Resume::Next(_)) => {
            let interp = unsafe { &mut *ptr.0 };
            body(interp)
        }
        Ok(Resume::Return(v)) => Suspend::Done(v),
        Ok(Resume::Throw(e)) => Suspend::Throw(e),
    };
    let _ = suspend_tx.send(outcome);
    // Clear the TLS so the next job starts clean and `in_coroutine()` reads false between jobs.
    YIELDER.with(|y| *y.borrow_mut() = None);
}

/// Spawn a coroutine over `body` on a pooled worker, parked until its first [`Coroutine::resume`].
/// `Err` when the platform cannot spawn threads (wasm32).
pub fn spawn_coroutine(interp: *mut Interp, body: SendBody) -> std::io::Result<Coroutine> {
    let (resume_tx, resume_rx) = channel::<Resume>();
    let (suspend_tx, suspend_rx) = channel::<Suspend>();
    let worker = get_worker()?;
    let job = Job {
        ptr: InterpPtr(interp),
        body,
        resume_rx,
        suspend_tx,
    };
    // The worker is idle in `job_rx.recv()`; hand it this coroutine. A send failure means the worker
    // vanished — surface it like a failed spawn rather than wedging.
    worker.send(job).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "coroutine worker unavailable")
    })?;
    Ok(Coroutine::Thread(ThreadCoro {
        resume_tx,
        suspend_rx,
        done: false,
        started: false,
    }))
}
