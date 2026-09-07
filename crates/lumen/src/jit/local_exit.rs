//! Publish bounded borrowed wide locals and operands before releasing displaced owners.
use crate::{jit::asm::Asm, value::Value};

/// Publish a borrowed wide shadow frame; x22=locals, x23=shadow, x20=empty VM stack base.
/// Shadow locals precede `depth` operands. Caller validates original CFG stack capacity,
/// initializes every shadow Value, and retains physical owners/the unchanged Object graph.
/// Shadow copies are NEVER dropped. Numeric-only heap writes cannot sever borrowed edges.
///
/// All new owners are cloned and published before any old owner drops. No GC/JS/re-entry is
/// allowed inside this transaction. On return x20 is the published stack top; caller-saved
/// homes are dead. x19,x21,x22,x23 are ABI-preserved. Caller saved LR in its JIT prologue.
pub(in crate::jit) fn emit_shadow(a: &mut Asm, n_slots: usize, depth: usize) -> bool {
    if crate::jit::PACKED_LOCAL_SLOTS || n_slots > 16 || depth > 8 {
        return false;
    }
    a.mov(0, 22);
    a.mov(1, 23);
    a.mov(2, 20);
    a.movz(3, n_slots as u32, 0);
    a.movz(4, depth as u32, 0);
    a.mov_imm64(16, publish_shadow as *const () as usize as u64);
    a.blr(16);
    a.mov(20, 0);
    true
}

unsafe extern "C" fn publish_shadow(
    locals: *mut Value,
    shadow: *const Value,
    stack: *mut Value,
    n_slots: usize,
    depth: usize,
) -> *mut Value {
    debug_assert!(n_slots <= 16 && depth <= 8);
    let mut owned: [Value; 24] = std::array::from_fn(|_| Value::Undefined);
    for (index, new) in owned.iter_mut().enumerate().take(n_slots + depth) {
        *new = unsafe { (&*shadow.add(index)).clone() };
    }
    let mut displaced: [Value; 16] = std::array::from_fn(|_| Value::Undefined);
    for (index, old) in displaced.iter_mut().enumerate().take(n_slots) {
        *old = unsafe { locals.add(index).replace(std::mem::take(&mut owned[index])) };
    }
    for index in 0..depth {
        unsafe {
            stack
                .add(index)
                .write(std::mem::take(&mut owned[n_slots + index]))
        };
    }
    // Complete both local and operand ownership before recursive destruction.
    drop(displaced);
    unsafe { stack.add(depth) }
}

#[cfg(test)]
mod tests {
    use crate::{
        jit::{asm::Asm, sys},
        value::{Object, Value},
    };
    use std::rc::Rc;

    #[test]
    fn native_shadow_publishes_aliases_tags_stack_and_destroys_stale_graph() {
        let a = Object::new(None);
        let b = Object::new(None);
        let child = Object::new(None);
        let child_weak = Rc::downgrade(&child);
        let old = Object::new(Some(child));
        let old_weak = Rc::downgrade(&old);
        let mut locals: [Value; 12] = std::array::from_fn(|_| Value::Undefined);
        locals[0] = Value::Obj(b.clone());
        locals[1] = Value::Obj(a.clone());
        locals[2] = Value::Obj(a.clone()); // Self-assignment must preserve ownership.
        locals[11] = Value::Obj(old);
        let mut shadow: Vec<Value> = (0..9)
            .map(|index| Value::Obj(if index % 2 == 0 { a.clone() } else { b.clone() }))
            .collect();
        shadow.extend([Value::Empty, Value::Undefined, Value::Num(-0.0)]);
        shadow.extend([
            Value::Obj(a.clone()),
            Value::Str("stack string".into()),
            Value::Num(7.0),
        ]);
        let mut stack: [std::mem::MaybeUninit<Value>; 4] =
            std::array::from_fn(|_| std::mem::MaybeUninit::uninit());
        stack[0].write(Value::Num(123.0)); // Outside the supplied empty-frame stack base.
        let base = stack.as_mut_ptr().cast::<Value>();
        let mut a64 = Asm::new();
        a64.stp_pre(20, 22, -32);
        a64.stp_off(23, 30, 16);
        a64.mov(22, 0);
        a64.mov(23, 1);
        a64.mov(20, 2);
        assert!(super::emit_shadow(&mut a64, 12, 3));
        a64.mov(0, 20);
        a64.ldp_off(23, 30, 16);
        a64.ldp_post(20, 22, 32);
        a64.ret();
        let words = a64.finish();
        unsafe {
            let mem = sys::alloc_exec(words.as_ptr().cast(), words.len() * 4);
            assert!(!mem.is_null());
            let call: unsafe extern "C" fn(*mut Value, *const Value, *mut Value) -> *mut Value =
                std::mem::transmute(mem);
            let end = call(locals.as_mut_ptr(), shadow.as_ptr(), base.add(1));
            sys::free_exec(mem, words.len() * 4);
            assert_eq!(end, base.add(4));
        }
        assert!(old_weak.upgrade().is_none() && child_weak.upgrade().is_none());
        assert!(matches!(&locals[9], Value::Empty));
        assert!(matches!(&locals[10], Value::Undefined));
        assert!(matches!(&locals[11], Value::Num(v) if v.to_bits() == (-0.0f64).to_bits()));
        let published = stack.map(|value| unsafe { value.assume_init() });
        assert!(matches!(&published[0], Value::Num(123.0)));
        assert!(matches!(&published[1], Value::Obj(v) if Rc::ptr_eq(v, &a)));
        assert!(matches!(&published[2], Value::Str(v) if v.as_str() == "stack string"));
        assert!(matches!(&published[3], Value::Num(7.0)));
        drop(shadow);
        assert_eq!(Rc::strong_count(&a), 7); // root + five locals + operand
        assert_eq!(Rc::strong_count(&b), 5);
        drop(locals);
        drop(published);
        assert_eq!(Rc::strong_count(&a), 1);
        assert_eq!(Rc::strong_count(&b), 1);
    }

    #[test]
    fn shadow_preflight_rejects_limits_without_emission() {
        let mut a = Asm::new();
        assert!(!super::emit_shadow(&mut a, 17, 0));
        assert!(!super::emit_shadow(&mut a, 0, 9));
        assert!(a.finish().is_empty());
    }
}
