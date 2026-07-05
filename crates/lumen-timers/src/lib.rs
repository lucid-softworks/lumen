//! lumen-timers — the timer globals (`setTimeout`, `setInterval`, `clearTimeout`,
//! `clearInterval`, `setImmediate`) as an op crate.
//!
//! The ops only mutate the [`Timers`] heap in `OpState`; nothing here sleeps, spawns, or
//! fires. The runtime's event loop drives everything: it asks [`Timers::next_deadline`] how
//! long it may block, and fires [`Timers::take_due`] callbacks each turn. `setImmediate`
//! doesn't touch the heap at all — it queues on the loop's [`CallbackQueue`] for the next
//! turn.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use lumen_host::{ops, CallbackQueue, Ctx, Extension, Value};

/// The extension a runtime installs: the five timer globals plus the [`Timers`] state.
pub fn extension() -> Extension {
    Extension {
        name: "timers",
        globals: ops![
            "setTimeout" (2) => op_set_timeout,
            "setInterval" (2) => op_set_interval,
            "clearTimeout" (1) => op_clear_timer,
            "clearInterval" (1) => op_clear_timer,
            "setImmediate" (1) => op_set_immediate,
        ],
        namespaces: &[],
        state_init: Some(|state| state.put(Timers::default())),
        js_init: None,
    }
}

struct Entry {
    callback: Value,
    args: Vec<Value>,
    /// `Some(period)` for `setInterval`: reschedule after each firing.
    repeat: Option<Duration>,
}

/// The timer heap. Cancellation is lazy: `clear*` removes the entry; stale heap nodes are
/// skipped (and popped) when they surface, so `clearTimeout` is O(1).
#[derive(Default)]
pub struct Timers {
    next_id: u64,
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    entries: HashMap<u64, Entry>,
}

impl Timers {
    fn schedule(
        &mut self,
        callback: Value,
        args: Vec<Value>,
        delay: Duration,
        repeat: bool,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.insert(
            id,
            Entry {
                callback,
                args,
                repeat: repeat.then_some(delay),
            },
        );
        self.heap.push(Reverse((Instant::now() + delay, id)));
        id
    }

    fn clear(&mut self, id: u64) {
        self.entries.remove(&id);
    }

    /// Whether any live timer remains (the loop stays alive while true).
    pub fn has_pending(&self) -> bool {
        !self.entries.is_empty()
    }

    /// When the loop may sleep until. Pops cancelled heap nodes so a cleared timer can't
    /// produce a busy-wakeup loop.
    pub fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if self.entries.contains_key(&id) {
                return Some(deadline);
            }
            self.heap.pop();
        }
        None
    }

    /// Callbacks due at `now`, earliest first. Intervals are rescheduled (from their
    /// deadline, not `now`, so periods don't drift); one-shots are removed.
    pub fn take_due(&mut self, now: Instant) -> Vec<(Value, Vec<Value>)> {
        let mut due = Vec::new();
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if deadline > now {
                break;
            }
            self.heap.pop();
            let Some(entry) = self.entries.get(&id) else {
                continue; // cancelled
            };
            due.push((entry.callback.clone(), entry.args.clone()));
            match entry.repeat {
                Some(period) => self.heap.push(Reverse((deadline + period, id))),
                None => {
                    self.entries.remove(&id);
                }
            }
        }
        due
    }
}

/// WHATWG timer-initialization steps, abridged: coerce the delay (NaN/negative -> 0), stash
/// callback + extra args, return the id as a Number.
fn schedule_op(ctx: &mut Ctx, args: &[Value], repeat: bool) -> Result<Value, Value> {
    let callback = match args.first() {
        Some(cb) if cb.is_callable() => cb.clone(),
        _ => {
            let kind = if repeat { "setInterval" } else { "setTimeout" };
            return Err(ctx.make_error("TypeError", format!("{kind} expects a function")));
        }
    };
    let ms = match args.get(1) {
        Some(v) => ctx.coerce_number(v)?,
        None => 0.0,
    };
    let delay = Duration::from_millis(if ms.is_finite() && ms > 0.0 {
        ms as u64
    } else {
        0
    });
    let extra: Vec<Value> = args.iter().skip(2).cloned().collect();
    let timers = ctx.host_mut::<Timers>().expect("timers state installed");
    let id = timers.schedule(callback, extra, delay, repeat);
    Ok(Value::Num(id as f64))
}

fn op_set_timeout(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    schedule_op(ctx, args, false)
}

fn op_set_interval(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    schedule_op(ctx, args, true)
}

/// Shared by `clearTimeout`/`clearInterval` (per spec either clears either kind). Unknown or
/// non-numeric ids are ignored.
fn op_clear_timer(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if let Some(v) = args.first() {
        let id = ctx.coerce_number(v)?;
        if id.is_finite() && id >= 0.0 {
            let timers = ctx.host_mut::<Timers>().expect("timers state installed");
            timers.clear(id as u64);
        }
    }
    Ok(Value::Undefined)
}

/// Queue for the next loop turn (after microtasks, before timers get another look).
fn op_set_immediate(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let callback = match args.first() {
        Some(cb) if cb.is_callable() => cb.clone(),
        _ => return Err(ctx.make_error("TypeError", "setImmediate expects a function")),
    };
    let extra: Vec<Value> = args.iter().skip(1).cloned().collect();
    CallbackQueue::enqueue(ctx.op_state(), callback, extra);
    Ok(Value::Undefined)
}
