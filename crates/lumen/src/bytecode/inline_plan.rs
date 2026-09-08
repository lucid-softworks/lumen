//! Admission planning for guarded speculative user-call inlining.
pub(super) mod compiled;
mod diagnostics;
use super::{CallIc, Chunk, InlinePlanEntry, InlineWay, Op};
use crate::ast::Function;
use std::rc::Rc;

const INLINE_MAX_DEPTH: u32 = 3;
const INLINE_MAX_WAYS: usize = 4;

macro_rules! skip {
    ($log:expr,$idx:expr,$why:expr) => {{
        if $log {
            eprintln!("[tier] inline skip site {}: {}", $idx, $why);
        }
        continue;
    }};
}

/// Build the speculative-inline plan for a hot chunk: for each monomorphic, filled call site,
/// the callee qualifies when it is a plain same-strictness function whose compiled body is
/// small, needs no activation environment, permits guarded global/shared free-name reads, and hides no control-flow
/// the splice can't reproduce (handlers, closures). The plan keys are the sites' `CallIc`
/// indices, which equal the second compile's caller-level site ordinals (same AST, same
/// emission order).
pub(crate) fn plan_inlines(
    chunk: &Chunk,
    caller: &Function,
    global_env: &crate::interpreter::Env,
    caller_env: *const std::cell::RefCell<crate::interpreter::Scope>,
) -> crate::fasthash::FastMap<u32, InlinePlanEntry> {
    // Bound the *whole* optimized body rather than stopping after one arbitrary nesting level.
    // OO hot loops tend to be call chains (dispatcher -> virtual method -> small scheduler
    // helper); a depth-one cap leaves the most valuable dispatch intact.  The shared source-op
    // budget prevents four-way polymorphic sites from growing exponentially.  It is deliberately
    // conservative: an inline property op can expand to substantially more machine code than a
    // simple arithmetic op.
    const INLINE_SOURCE_OP_BUDGET: usize = 320;
    let limit = std::env::var("LUMEN_INLINE_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(INLINE_SOURCE_OP_BUDGET);
    let mut budget = limit.saturating_sub(chunk.ops.len());
    plan_inlines_at(chunk, caller, global_env, caller_env, 0, &mut budget)
}

fn plan_inlines_at(
    chunk: &Chunk,
    caller: &Function,
    global_env: &crate::interpreter::Env,
    caller_env: *const std::cell::RefCell<crate::interpreter::Scope>,
    depth: u32,
    budget: &mut usize,
) -> crate::fasthash::FastMap<u32, InlinePlanEntry> {
    let max_ops = std::env::var("LUMEN_INLINE_MAX_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96);
    let mut planner = Planner {
        chunk,
        caller,
        global_env,
        caller_env,
        depth,
        budget,
        max_ops,
        log: std::env::var_os("LUMEN_TIER_LOG").is_some(),
        admission: std::env::var_os("LUMEN_INLINE_ADMISSION").is_some(),
    };
    let mut plan = crate::fasthash::FastMap::default();
    for (idx, site) in chunk.call_caches.iter().enumerate() {
        let ways = planner.ways(idx, site);
        if !ways.is_empty() {
            plan.insert(idx as u32, InlinePlanEntry { ways });
        }
    }
    plan
}
struct Planner<'a> {
    chunk: &'a Chunk,
    caller: &'a Function,
    global_env: &'a crate::interpreter::Env,
    caller_env: *const std::cell::RefCell<crate::interpreter::Scope>,
    depth: u32,
    budget: &'a mut usize,
    max_ops: usize,
    log: bool,
    admission: bool,
}
impl Planner<'_> {
    fn ways(&mut self, idx: usize, site: &super::CallSite) -> Vec<InlineWay> {
        let (chunk, caller, global_env, caller_env, depth, max_ops) = (
            self.chunk,
            self.caller,
            self.global_env,
            self.caller_env,
            self.depth,
            self.max_ops,
        );
        let budget = &mut *self.budget;
        let log = self.log;
        let pins = chunk.call_pins.borrow();
        let filled = filled(site);
        let mut ways: Vec<InlineWay> = Vec::new();
        for (way, ic) in filled.iter().take(INLINE_MAX_WAYS) {
            let Some(weak) = pins.get(&ic.callee) else {
                skip!(log, idx, "no pin")
            };
            let Some(obj) = weak.upgrade() else {
                skip!(log, idx, "dead callee")
            };
            let b = obj.borrow();
            let crate::value::Callable::User(user) = &b.call else {
                continue;
            };
            let global_closure = Rc::ptr_eq(&user.env, global_env);
            let callee_env = Rc::as_ptr(&user.env);
            let shared_closure = !caller_env.is_null() && callee_env == caller_env;
            let f = &user.func;
            let callee_chunk = match eligible(f, caller, chunk, ic, max_ops) {
                Ok(chunk) => chunk,
                Err(why) => skip!(log, idx, why),
            };
            let free_names = free_names(callee_chunk);
            let record = diagnostics::Record {
                caller,
                chunk,
                callee: f,
                callee_chunk,
                depth,
                site: idx,
                way: *way,
                callee_object: ic.callee,
                caller_env: caller_env as usize,
                callee_env: callee_env as usize,
                budget: *budget,
                free_names: &free_names,
            };
            if !free_names.is_empty() && !global_closure && !shared_closure {
                diagnostics::rejected(self.admission, record);
                skip!(log, idx, "free names in a non-global closure");
            }
            let inline_cost = callee_chunk.ops.len();
            if inline_cost > *budget {
                skip!(log, idx, "optimized-body budget");
            }
            *budget -= inline_cost;
            let uses_this = callee_chunk.uses_this();
            let nested = if depth < INLINE_MAX_DEPTH && *budget > 0 {
                plan_inlines_at(callee_chunk, f, global_env, callee_env, depth + 1, budget)
            } else {
                Default::default()
            };
            record.accepted(self.admission, global_closure, shared_closure, *budget);
            let f = f.clone();
            drop(b);
            ways.push(InlineWay {
                check_this: uses_this && !f.is_strict,
                uses_this,
                f,
                obj,
                free_names,
                expected_env: expected_env(shared_closure, callee_env as usize),
                shared_lexical: shared_closure && !global_closure,
                nested,
            });
        }
        ways
    }
}

fn eligible<'a>(
    f: &'a Function,
    caller: &Function,
    chunk: &Chunk,
    ic: &CallIc,
    max_ops: usize,
) -> Result<&'a Rc<Chunk>, &'static str> {
    if f.is_arrow || f.is_async || f.is_generator || f.is_strict != caller.is_strict {
        return Err("arrow/strictness");
    }
    if f.params.iter().any(|p| {
        p.rest || p.default.is_some() || !matches!(p.pattern, crate::ast::Pattern::Ident(_))
    }) {
        return Err("param shape");
    }
    let Some(Some(callee_chunk)) = f.code.get() else {
        return Err("callee not compiled");
    };
    if std::ptr::eq(&**callee_chunk, chunk) {
        return Err("self-recursion");
    }
    if callee_chunk.ops.len() > max_ops
        || callee_chunk.n_slots > 32
        || !callee_chunk.jit_no_activation()
        || callee_chunk.env_this
        || ic.n_params > 8
    {
        return Err("callee size/shape");
    }
    // The splice runs under the caller's frame: no handler regions to relocate, no inner
    // closures, no name writes. Free-name READS are allowed for global-closure callees —
    // the compiler re-resolves them at the splice site and refuses shadowed ones.
    if callee_chunk.ops.iter().any(|op| {
        matches!(
            op,
            Op::PushHandler(_)
                | Op::MakeClosure(..)
                | Op::StoreName(_)
                | Op::StoreNameCached(..)
                | Op::UpdateName(..)
                | Op::UpdateNameCached(..)
        )
    }) {
        return Err("callee ops (handlers/closures/name writes)");
    }
    Ok(callee_chunk)
}

fn free_names(callee_chunk: &Chunk) -> Vec<Rc<str>> {
    let mut free_names: Vec<Rc<str>> = Vec::new();
    for op in callee_chunk.ops.iter() {
        if let Op::LoadName(n, _) | Op::LoadNameForCall(n, _) = op {
            let name = callee_chunk.names[*n as usize].clone();
            if !free_names.contains(&name) {
                free_names.push(name);
            }
        }
    }
    free_names
}

fn filled(site: &super::CallSite) -> Vec<(usize, CallIc)> {
    let mut filled: Vec<_> = site
        .entries
        .iter()
        .enumerate()
        .map(|(way, e)| (way, e.get()))
        .filter(|(_, c)| c.callee != 0)
        .collect();
    // Preserve existing adjacent-only dedup and PIC order exactly.
    filled.dedup_by_key(|(_, c)| c.callee);
    filled
}

fn expected_env(shared: bool, address: usize) -> usize {
    if shared {
        address
    } else {
        0
    }
}
