//! Ordinary InlineGuard emission with opt-in, path-selected diagnostic counting.
mod counters;
mod registry;
use super::{asm::Asm, C_NE};
use crate::{bytecode::InlineTarget, value::JitLayout};
pub(super) use registry::Compilation;

/// A shared-Function guard reads only live operands and declines before any binding.
pub(super) fn emit_closure(a: &mut Asm, pc: u32, target: usize, id: Option<u64>) {
    let miss = id.map_or(target, |_| a.new_label());
    a.mov(0, 19);
    a.movz(1, pc, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (super::H_INLINE_CLOSURE * 8) as u32);
    a.blr(16);
    a.cbz(0, true, miss);
    if let Some(id) = id {
        let done = a.new_label();
        counters::emit(a, id, true);
        a.b(done);
        a.bind(miss);
        counters::emit(a, id, false);
        a.b(target);
        a.bind(done);
    }
}

/// Disabled emission has the original guards and destinations, with no counter call.
pub(super) fn emit(
    a: &mut Asm,
    layout: &JitLayout,
    it: &InlineTarget,
    target: usize,
    id: Option<u64>,
) {
    let miss = id.map_or(target, |_| a.new_label());
    // Value::Obj stores the RcBox base, not Rc::as_ptr. Probe the stored word
    // exactly as jit_layout does; a dead pin or unknown layout always misses.
    let stored = it.pin.upgrade().filter(|_| layout.valid).map(|o| {
        let some: Option<crate::value::Gc> = Some(o);
        unsafe { *(&some as *const Option<crate::value::Gc> as *const usize) }
    });
    match stored {
        None => a.b(miss),
        Some(s) => {
            if it.expected_env != 0 {
                a.ldr_imm(11, 19, 40);
                a.mov_imm64(12, it.expected_env as u64);
                a.cmp_reg_x(11, 12);
                a.b_cond(C_NE, miss);
            }
            let dm = (it.argc as i32 + 1) * 16;
            a.ldurb(9, 20, -dm);
            a.cmp_imm_w(9, 8);
            a.b_cond(C_NE, miss);
            a.ldur(9, 20, -dm + 8);
            a.mov_imm64(10, s as u64);
            a.cmp_reg_x(9, 10);
            a.b_cond(C_NE, miss);
            if it.check_this {
                a.ldurb(9, 20, -dm - 16);
                a.cmp_imm_w(9, 8);
                a.b_cond(C_NE, miss);
            }
        }
    }
    if let Some(id) = id {
        let done = a.new_label();
        counters::emit(a, id, true);
        a.b(done);
        a.bind(miss);
        counters::emit(a, id, false);
        a.b(target);
        a.bind(done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        jit::{sys, JitCode},
        value::{jit_layout, Object, Value},
    };
    use std::rc::Rc;

    fn code(it: &InlineTarget, layout: &JitLayout, id: Option<u64>) -> JitCode {
        let mut a = Asm::new();
        a.stp_pre(29, 30, -16);
        a.stp_pre(19, 20, -16);
        a.mov(19, 0);
        a.mov(20, 1);
        let miss = a.new_label();
        let done = a.new_label();
        emit(&mut a, layout, it, miss, id);
        a.movz(0, 1, 0);
        a.b(done);
        a.bind(miss);
        a.movz(0, 0, 0);
        a.bind(done);
        a.ldp_post(19, 20, 16);
        a.ldp_post(29, 30, 16);
        a.ret();
        let words = a.finish();
        let len = words.len() * 4;
        let mem = unsafe { sys::alloc_exec(words.as_ptr().cast(), len) };
        assert!(!mem.is_null());
        JitCode {
            mem,
            len,
            pc_offsets: vec![],
            max_stack: 0,
            needs_global: false,
        }
    }

    #[test]
    fn actual_guard_edges_count_once_without_changing_decisions() {
        let callee = Object::new(None);
        let other = Object::new(None);
        let layout = jit_layout(&callee);
        let target = InlineTarget {
            expected: 0,
            pin: Rc::downgrade(&callee),
            expected_env: 123,
            argc: 0,
            check_this: true,
        };
        let id = registry::unique_id();
        let instrumented = code(&target, &layout, Some(id));
        let original = code(&target, &layout, None);
        type Probe = unsafe extern "C" fn(*const u64, *const Value) -> u32;
        let run: Probe = unsafe { std::mem::transmute(instrumented.mem_ptr()) };
        let baseline: Probe = unsafe { std::mem::transmute(original.mem_ptr()) };
        let cases = [
            (
                123,
                Value::Obj(other.clone()),
                Value::Obj(callee.clone()),
                1,
            ),
            (
                124,
                Value::Obj(other.clone()),
                Value::Obj(callee.clone()),
                0,
            ),
            (123, Value::Obj(other.clone()), Value::Num(7.0), 0),
            (123, Value::Obj(other.clone()), Value::Obj(other.clone()), 0),
            (123, Value::Num(7.0), Value::Obj(callee.clone()), 0),
        ];
        for (env, receiver, function, expected) in cases {
            let context = [0, 0, 0, 0, 0, env];
            let stack = [receiver, function];
            let sp = unsafe { stack.as_ptr().add(2) };
            assert_eq!(unsafe { baseline(context.as_ptr(), sp) }, expected);
            assert_eq!(unsafe { run(context.as_ptr(), sp) }, expected);
        }
        assert_eq!(counters::totals(id), [4, 1]);
        drop(callee);
        let dead_id = registry::unique_id();
        let dead = code(&target, &layout, Some(dead_id));
        let run: Probe = unsafe { std::mem::transmute(dead.mem_ptr()) };
        // Compile-time dead pin never touches context or VM stack.
        assert_eq!(unsafe { run(std::ptr::null(), std::ptr::null()) }, 0);
        assert_eq!(counters::totals(dead_id), [1, 0]);
    }
}
