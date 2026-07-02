//! Stackful coroutines for generators (and async functions), built on OS threads.
//!
//! lumen is a tree-walking interpreter, so suspending a generator mid-body means parking its native
//! call stack. Each generator gets its own OS thread; control is handed back and forth with a pair of
//! channels in strict ping-pong — exactly one of {driver, generator thread} runs at any instant, the
//! other parked in `recv`. The shared [`Interp`] is therefore never touched concurrently, which is
//! why shuttling a `*mut Interp` across the thread boundary (see [`InterpPtr`]) is sound in practice.
//!
//! The running generator's channels live in a thread-local [`YIELDER`], so a `yield` buried deep in
//! eval finds the right channel and nested generators (each on their own thread) need no extra
//! bookkeeping — every thread reads its own thread-local.
//!
//! Address stability: a generator never outlives the `Engine` that owns the interpreter, and that
//! `Engine` is not moved between the `eval` calls that create and drive the generator, so the
//! captured pointer stays valid for the coroutine's whole life.

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

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

/// The driver side of one generator, stored on the generator object in `Interp.generators`.
pub struct Coroutine {
    resume_tx: Sender<Resume>,
    suspend_rx: Receiver<Suspend>,
    /// Set once the body has finished (Done/Throw); further resumes are no-ops.
    pub done: bool,
    _handle: JoinHandle<()>,
}

impl Coroutine {
    /// Hand control to the generator and block until it next parks or finishes. Saves/restores the
    /// interpreter's scalar execution context (`strict`, recursion `depth`) across the handoff so the
    /// driver and the body don't clobber each other's.
    pub fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        let (saved_strict, saved_depth) = (i.strict, i.depth);
        let _ = self.resume_tx.send(signal);
        let s = self.suspend_rx.recv();
        i.strict = saved_strict;
        i.depth = saved_depth;
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
    let (gen_strict, gen_depth) = (i.strict, i.depth);
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

/// Spawn a generator coroutine over `body`, parked until its first [`Coroutine::resume`].
pub fn spawn_coroutine(interp: *mut Interp, body: SendBody) -> Coroutine {
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<Resume>();
    let (suspend_tx, suspend_rx) = std::sync::mpsc::channel::<Suspend>();
    let ptr = InterpPtr(interp);
    let handle = std::thread::Builder::new()
        // Generous stack: the tree-walker recurses up to MAX_EVAL_DEPTH (1500) frames.
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            // Capture the whole Send wrappers (not their inner non-Send fields — Rust 2021 would
            // otherwise disjoint-capture `ptr.0` / `body.0` and reject the closure as non-Send).
            let ptr = ptr;
            let body = body;
            let SendBody(body) = body;
            YIELDER.with(|y| {
                *y.borrow_mut() = Some(Yielder {
                    suspend_tx,
                    resume_rx,
                })
            });
            // Park until the first next()/return()/throw(); the body doesn't run before then.
            let first = YIELDER.with(|y| y.borrow().as_ref().unwrap().resume_rx.recv());
            let outcome = match first {
                Err(_) => return, // dropped before first drive
                Ok(Resume::Next(_)) => {
                    let interp = unsafe { &mut *ptr.0 };
                    body(interp)
                }
                Ok(Resume::Return(v)) => Suspend::Done(v),
                Ok(Resume::Throw(e)) => Suspend::Throw(e),
            };
            YIELDER.with(|y| {
                if let Some(yl) = y.borrow().as_ref() {
                    let _ = yl.suspend_tx.send(outcome);
                }
            });
        })
        .expect("spawn generator thread");
    Coroutine {
        resume_tx,
        suspend_rx,
        done: false,
        _handle: handle,
    }
}
