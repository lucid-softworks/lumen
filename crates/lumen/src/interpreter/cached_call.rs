//! Cached physical-call dispatch and the transition from closure retry to identity hits.
use super::{Abrupt, Interp, Value};
use std::rc::Rc;

impl Interp {
    /// The JIT→JIT fast call (from `bytecode::jit_exec`'s Call/CallWithThis arms): a plain,
    /// same-realm, already-JIT-compiled user function with no activation environment runs with
    /// exactly the observable effects of the layered path — recursion depth, gc_check, the
    /// `FnFrame` for `f.caller` reflection, constructing/new.target save-clear-restore, and the
    /// proper-tail-call trampoline — but skips the dispatch layers' proxy/realm/eval-marker
    /// re-checks (guarded up front) and *moves* the `argc` argument `Value`s at `args` into the
    /// callee's slots instead of clone-here-drop-there.
    ///
    /// `None` = not applicable, with NO side effects and the arguments untouched (the caller
    /// runs the generic path). `Some(r)` = handled; the arguments AND `*this_slot` have been
    /// consumed (the `this` binding moves into the callee instead of a clone-here-drop-there).
    ///
    /// The identity-cached JIT→JIT call (see [`crate::bytecode::CallIc`]): on a per-site hit,
    /// the entire dispatch guard set collapses into two pointer compares (callee identity +
    /// active-realm global), and the frame's derived state (env, chunk, machine code, strict)
    /// reads through the cached raw pointers with a single refcount bump for the env handle.
    /// `None` = miss (empty cache / different callee / realm switched), with NO side effects —
    /// the caller falls into [`Interp::call_jit_fast`], which revalidates and refills.
    ///
    /// # Safety
    /// Same contract as `call_jit_fast`: on `Some`, `args..args+argc` and `*this_slot` have been
    /// consumed.
    pub(crate) unsafe fn call_jit_cached(
        &mut self,
        caller: &crate::bytecode::Chunk,
        site_index: u32,
        callee: &Value,
        this_slot: *const Value,
        args: *mut Value,
        argc: usize,
    ) -> Option<Result<Value, Abrupt>> {
        let Value::Obj(o) = callee else { return None };
        let site = caller.call_site(site_index);
        let key = Rc::as_ptr(o) as usize;
        let genv = Rc::as_ptr(&self.global_env) as usize;
        // Probe the identity fields through the Cell without copying whole entries; only the
        // hit is copied out (nothing re-entrant runs between the probe and the copy).
        let epoch = crate::bytecode::CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        let mut hit = None;
        for e in &site.entries {
            let p = e.as_ptr();
            unsafe {
                if (*p).callee == key && (*p).global_env == genv && (*p).epoch == epoch {
                    hit = Some(*p);
                    break;
                }
            }
        }
        let ic = match hit {
            Some(ic) => ic,
            None => {
                let ic = self.fresh_call_ic(site, o, key, genv, epoch)?;
                if ic.direct & crate::bytecode::CALL_IC_NEEDS_ENV != 0 {
                    return Some(unsafe {
                        self.call_jit_env_committed(ic, ic.env, this_slot, args, argc)
                    });
                }
                if super::fresh_call::refresh_enabled() {
                    caller.refresh_call_cache(site_index, ic, o);
                }
                // Fresh no-activation calls retain the normal recompile opportunity below.
                ic
            }
        };
        if ic.native != 0 {
            let nf: crate::value::NativeFn = unsafe { std::mem::transmute(ic.native) };
            return Some(unsafe { self.call_native_committed(nf, this_slot, args, argc) });
        }
        // Inline-recompile trigger: a chunk that keeps running in machine code gets one shot at
        // splicing its own hot monomorphic callees (see `bytecode::plan_inlines`).
        {
            let chunk_ref = unsafe { &**ic.chunk };
            let runs = chunk_ref.jit_runs.get().wrapping_add(1);
            chunk_ref.jit_runs.set(runs);
            if runs == crate::bytecode::inline_recompile_at() {
                self.try_inline_recompile(ic.func, chunk_ref, ic.env);
            }
        }
        Some(unsafe { self.call_jit_committed(ic, this_slot, args, argc) })
    }
}
