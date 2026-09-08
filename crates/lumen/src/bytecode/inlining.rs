//! Guarded call splicing, argument binding and the callee namespace.
use super::{
    call_cache_seeds, name_cache_seeds, property_cache_seeds, Bail, CResult, Compiler,
    InlinePlanEntry, InlineTarget, InlineWay, Op,
};
use crate::ast::{Function, HoistOp, Pattern};
use std::rc::Rc;

impl Compiler {
    pub(super) fn try_emit_inline(
        &mut self,
        entry: &InlinePlanEntry,
        argc: u16,
        cc: u32,
        has_this: bool,
    ) -> CResult {
        if argc > 8 {
            return Err(Bail); // the JIT guard peeks the callee with a ±256-byte unscaled load
        }
        // Per-way gates: a plain `Call` site has no `this` beneath the callee (a this-using
        // callee needs the generic binding, and the guard's receiver peek would read past the
        // operands); a caller binding (slot or captured) would shadow a global free name.
        let ways: Vec<&InlineWay> = entry
            .ways
            .iter()
            .filter(|w| {
                (has_this || !w.uses_this)
                    && (w.expected_env != 0
                        || !w.free_names.iter().any(|name| {
                            self.lookup(name).is_some() || self.env_names.contains_key(&**name)
                        }))
            })
            .collect();
        if ways.is_empty() {
            return Err(Bail);
        }
        // Each way: identity guard → bind → spliced body → jump to the shared join; a guard
        // mismatch falls to the next way, the last one to the generic call.
        let mut end_jumps: Vec<usize> = Vec::new();
        let mut pending_guard: Option<usize> = None;
        for w in &ways {
            if let Some(g) = pending_guard.take() {
                self.patch(g); // previous way's mismatch lands on this way's guard
            }
            let guard = self.emit_inline_way(w, argc, has_this, &mut end_jumps)?;
            pending_guard = Some(guard);
        }
        // ---- join: every way's result jumps here; the last mismatch runs the generic call.
        self.patch(pending_guard.take().expect("at least one way"));
        if has_this {
            self.emit(Op::CallWithThis(argc, cc));
        } else {
            self.emit(Op::Call(argc, cc));
        }
        for j in end_jumps {
            self.patch(j);
        }
        Ok(())
    }

    /// One guarded splice: emits the identity guard (returned unpatched — the caller chains it
    /// to the next way or the generic call), the frame binds, and the body; the result-carrying
    /// exits are appended to `end_jumps`.
    fn emit_inline_way(
        &mut self,
        w: &InlineWay,
        argc: u16,
        has_this: bool,
        end_jumps: &mut Vec<usize>,
    ) -> Result<usize, Bail> {
        let f = &w.f;
        let t = self.inline_targets.len() as u32;
        let callee_slot = (w.shared_lexical && super::inline_closure::enabled()).then(|| {
            let slot = self.fresh_slot("(inline callee)");
            self.inline_frames
                .closure(t, super::inline_closure::Guard::for_way(w, slot));
            slot
        });
        self.inline_targets.push(InlineTarget {
            expected: Rc::as_ptr(&w.obj) as usize,
            pin: Rc::downgrade(&w.obj),
            expected_env: w.expected_env,
            argc,
            check_this: has_this && w.check_this,
        });
        let guard = self.emit(Op::InlineGuard(t, 0));

        // ---- bind the callee frame into fresh caller slots ----
        let n_params = f.params.len();
        for _ in n_params..argc as usize {
            self.emit(Op::Pop); // surplus arguments (evaluated; excess drops from the top)
        }
        for _ in argc as usize..n_params {
            self.emit(Op::Undef); // missing arguments
        }
        let mut param_slots: Vec<u16> = Vec::with_capacity(n_params);
        for p in &f.params {
            let Pattern::Ident(name) = &p.pattern else {
                return Err(Bail);
            };
            if p.default.is_some() || p.rest {
                return Err(Bail);
            }
            param_slots.push(self.fresh_slot(name));
        }
        for &s in param_slots.iter().rev() {
            self.emit(Op::StoreLocal(s));
        }
        self.emit(callee_slot.map_or(Op::Pop, Op::StoreLocal));
        let this_slot = if !has_this {
            None // a plain Call site: nothing beneath the callee
        } else if w.uses_this {
            let s = self.fresh_slot("(inline this)");
            self.emit(Op::StoreLocal(s));
            Some(s)
        } else {
            self.emit(Op::Pop);
            None
        };

        let returns = self.emit_inline_body(w, &param_slots, this_slot)?;
        if let Some(slot) = callee_slot {
            // Every successful return joins before releasing the hidden owner. Exceptions
            // release it with the ordinary physical frame or a subsequent overwritten slot.
            for jump in returns {
                self.patch(jump);
            }
            self.emit(Op::ResetSlots(slot, 1));
        } else {
            end_jumps.extend(returns);
        }
        end_jumps.push(self.emit(Op::Jump(0)));
        Ok(guard)
    }

    /// Compile a spliced callee body: mirrors `compile_inner`'s hoist + lexical + statement
    /// sequence, with explicit per-execution resets replacing the fresh frame's zeroed slots.
    fn inline_body(&mut self, f: &Function) -> CResult {
        // Hoisted vars start undefined on EVERY pass through the site; fused-reset runs are
        // emitted per contiguous slot range (fresh slots are consecutive, so usually one op).
        let mut resets: Vec<u16> = Vec::new();
        for op in crate::interpreter::collect_hoist_ops(&f.body, f.is_strict, &[]) {
            match op {
                HoistOp::Var(name) => {
                    if self.lookup(&name).is_none() {
                        let slot = self.fresh_slot(&name);
                        self.scope_bind(&name, slot, false);
                        resets.push(slot);
                    }
                }
                HoistOp::VarForce(name) => {
                    let slot = match self.lookup(&name) {
                        Some((s, _)) => s,
                        None => {
                            let s = self.fresh_slot(&name);
                            self.scope_bind(&name, s, false);
                            s
                        }
                    };
                    if !resets.contains(&slot) {
                        resets.push(slot);
                    }
                }
                HoistOp::Fn(..) | HoistOp::AnnexB(..) => return Err(Bail),
            }
        }
        resets.sort_unstable();
        let mut k = 0;
        while k < resets.len() {
            let start = resets[k];
            let mut count = 1u16;
            while k + (count as usize) < resets.len() && resets[k + count as usize] == start + count
            {
                count += 1;
            }
            self.emit(Op::ResetSlots(start, count));
            k += count as usize;
        }
        let empty = std::collections::HashSet::new();
        self.declare_body_lexicals(&f.body, &empty)?;
        for stmt in &f.body {
            self.stmt(stmt)?;
        }
        self.emit(Op::Undef); // implicit return value
        Ok(())
    }
    fn seed_inline_caches(&mut self, f: &Function) {
        let hot_chunk = f.code.get().and_then(Option::as_ref);
        self.cache_seed_stack.push((
            hot_chunk
                .map(|chunk| property_cache_seeds(chunk))
                .unwrap_or_default(),
            0,
        ));
        self.name_seed_stack.push((
            hot_chunk
                .map(|chunk| name_cache_seeds(chunk))
                .unwrap_or_default(),
            0,
        ));
        self.call_seed_stack.push((
            hot_chunk
                .map(|chunk| call_cache_seeds(chunk))
                .unwrap_or_default(),
            0,
        ));
    }

    fn emit_inline_body(
        &mut self,
        w: &InlineWay,
        param_slots: &[u16],
        this_slot: Option<u16>,
    ) -> Result<Vec<usize>, Bail> {
        let f = &w.f;
        let t = self.inline_targets.len() as u32 - 1;
        // ---- compile the body under the callee's (empty) namespace ----
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_env_names = std::mem::take(&mut self.env_names);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_labels = std::mem::take(&mut self.pending_labels);
        let saved_try = std::mem::replace(&mut self.try_depth, 0);
        let saved_this = std::mem::replace(&mut self.inline_this, this_slot);
        let saved_returns = std::mem::take(&mut self.inline_returns);
        self.inline_depth += 1;
        self.plan_stack.push((w.nested.clone(), 0));
        self.seed_inline_caches(f);
        self.scopes.push(Vec::new());
        for (k, p) in f.params.iter().enumerate() {
            let Pattern::Ident(name) = &p.pattern else {
                unreachable!()
            };
            self.scope_bind(name, param_slots[k], false);
        }
        let frame_context = self.inline_frames.enter(t, f.is_strict, self.ops.len());
        let r = self.inline_body(f);
        self.inline_frames.leave(frame_context);
        self.call_seed_stack.pop();
        self.name_seed_stack.pop();
        self.cache_seed_stack.pop();
        self.plan_stack.pop();
        self.inline_depth -= 1;
        let returns = std::mem::replace(&mut self.inline_returns, saved_returns);
        self.inline_this = saved_this;
        self.try_depth = saved_try;
        self.pending_labels = saved_labels;
        self.loops = saved_loops;
        self.env_names = saved_env_names;
        self.scopes = saved_scopes;
        r?;

        Ok(returns)
    }
}
