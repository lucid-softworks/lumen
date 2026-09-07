//! Publish borrowed typed operands above an unchanged, owned VM stack prefix.
use crate::jit::asm::Asm;
use crate::value::JitLayout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit) enum Operand {
    Object(u32),
    Number(u32),
}

/// `max_depth` must be the original CFG maximum covered by the allocated VM stack.
/// The caller establishes that x20 points just above `prefix_depth` owned Values.
pub(in crate::jit) fn supported(
    layout: &JitLayout,
    operands: &[Operand],
    prefix_depth: usize,
    max_depth: usize,
) -> bool {
    layout.valid
        && layout.rc_strong_off < 256
        && prefix_depth
            .checked_add(operands.len())
            .is_some_and(|depth| depth <= max_depth)
        // ADD x20 uses an unshifted imm12. This also bounds every scaled STR offset.
        && operands.len() <= 4095 / 16
        && operands.iter().all(|value| match value {
            Operand::Object(register) => *register < 8,
            Operand::Number(register) => (16..32).contains(register),
        })
}

/// Clone each logical Object occurrence, publish ordered wide Values, then advance x20.
/// Returns false without emission if preflight fails. Clobbers x9 and x20 only.
///
/// All objects must remain rooted by unchanged physical owners throughout this sequence.
/// Destination memory must be unused stack storage: this does not drop overwritten owners.
/// No helper/GC may run until publication completes. No locals or frame metadata are changed;
/// the caller must restore those separately before resuming a checked original instruction.
pub(in crate::jit) fn emit(
    a: &mut Asm,
    layout: &JitLayout,
    operands: &[Operand],
    prefix_depth: usize,
    max_depth: usize,
) -> bool {
    if !supported(layout, operands, prefix_depth, max_depth) {
        return false;
    }
    for &value in operands {
        if let Operand::Object(register) = value {
            crate::jit::emit_region_clone_rc(a, register, layout.rc_strong_off as i32);
        }
    }
    for (index, &value) in operands.iter().enumerate() {
        let offset = index as u32 * 16;
        a.movz(
            9,
            if matches!(value, Operand::Object(_)) {
                8
            } else {
                4
            },
            0,
        );
        a.str_imm(9, 20, offset);
        match value {
            Operand::Object(register) => a.str_imm(register, 20, offset + 8),
            Operand::Number(register) => a.str_d_imm(register, 20, offset + 8),
        }
    }
    if !operands.is_empty() {
        a.add_imm(20, 20, operands.len() as u32 * 16);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{emit, supported, Operand};
    use crate::jit::{asm::Asm, sys};
    use crate::value::{jit_layout, Gc, Object, Value};
    use std::{mem::MaybeUninit, rc::Rc};

    #[test]
    fn preflight_rejects_capacity_overflow_and_scratch_homes_without_emission() {
        let object = Object::new(None);
        let mut layout = jit_layout(&object);
        let values = [Operand::Object(0), Operand::Number(16)];
        assert!(supported(&layout, &values, 3, 5));
        let mut a = Asm::new();
        assert!(!emit(&mut a, &layout, &values, 3, 4));
        assert!(!emit(&mut a, &layout, &values, usize::MAX, usize::MAX));
        assert!(!emit(&mut a, &layout, &[Operand::Object(8)], 0, 1));
        assert!(!emit(&mut a, &layout, &[Operand::Number(15)], 0, 1));
        assert!(!emit(&mut a, &layout, &[Operand::Number(32)], 0, 1));
        assert!(!emit(
            &mut a,
            &layout,
            &vec![Operand::Number(16); 256],
            0,
            256
        ));
        layout.rc_strong_off = 256;
        assert!(!emit(&mut a, &layout, &values, 0, 2));
        assert!(a.finish().is_empty());
    }

    #[test]
    fn native_publication_creates_duplicate_owners_and_preserves_prefix() {
        let object = Object::new(None);
        let weak = Rc::downgrade(&object);
        let layout = jit_layout(&object);
        let mut a = Asm::new();
        // ABI arguments: x0=borrowed stored Rc, x1=first unused stack position.
        a.stp_pre(20, 30, -16);
        a.mov(20, 1);
        a.mov_imm64(9, (-0.0f64).to_bits());
        a.fmov_d_x(16, 9);
        assert!(emit(
            &mut a,
            &layout,
            &[Operand::Object(0), Operand::Number(16), Operand::Object(0)],
            1,
            4,
        ));
        a.mov(0, 20);
        a.ldp_post(20, 30, 16);
        a.ret();
        let words = a.finish();
        let mut stack: [MaybeUninit<Value>; 4] = std::array::from_fn(|_| MaybeUninit::uninit());
        stack[0].write(Value::Obj(object.clone()));
        let base = stack.as_mut_ptr().cast::<Value>();
        // jit_layout validates the stored Rc representation, not Rc::as_ptr's body address.
        let stored = unsafe { *(&object as *const Gc).cast::<usize>() };
        unsafe {
            let code = sys::alloc_exec(words.as_ptr().cast(), words.len() * 4);
            assert!(!code.is_null());
            let call: unsafe extern "C" fn(usize, *mut Value) -> *mut Value =
                std::mem::transmute(code);
            let end = call(stored, base.add(1));
            sys::free_exec(code, words.len() * 4);
            assert_eq!(end, base.add(4));
        }
        assert_eq!(Rc::strong_count(&object), 4);
        let [prefix, first, number, second] = stack.map(|value| unsafe { value.assume_init() });
        assert!(matches!(&prefix, Value::Obj(value) if Rc::ptr_eq(value, &object)));
        assert!(matches!(number, Value::Num(value) if value.to_bits() == (-0.0f64).to_bits()));
        drop(prefix);
        drop(object);
        assert_eq!(weak.strong_count(), 2);
        drop(first);
        assert_eq!(weak.strong_count(), 1);
        drop(second);
        assert!(weak.upgrade().is_none());
    }
}
