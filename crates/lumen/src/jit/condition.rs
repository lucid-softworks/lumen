//! Inline lowering for scalar, consuming branch conditions.

use super::{asm::Asm, emit_cond, COND_POP_TRUTHY, C_EQ, C_LS, C_NE};
use crate::bytecode::Op;

/// Whether a straight-line predecessor produces a Number or BigInt (with the latter retaining
/// the helper fallback). Restricting the larger scalar template to these sites avoids increasing
/// every Bool branch's generated footprint. `Add` is intentionally absent because it may produce
/// a string; comparison and `Not` results already use the compact Bool path.
pub(super) fn produces_numeric_scalar(op: &Op) -> bool {
    matches!(
        op,
        Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Neg
            | Op::Plus
            | Op::BitNot
    )
}

/// Consume the top operand and branch to `false_target` when it is falsy.
///
/// Undefined, Empty, Null, Bool, and Number have no destructor, so their exact JavaScript
/// truthiness can be resolved before moving the stack pointer. Refcounted values and BigInt keep
/// the canonical helper path, which owns and drops the consumed operand and handles HTMLDDA.
pub(super) fn emit_pop_false(
    a: &mut Asm,
    false_target: usize,
    unwind: usize,
    scalar_fast_path: bool,
) {
    let slow = a.new_label();
    let done = a.new_label();

    // Keep the established Bool fast path first and unchanged: comparisons produce Bool
    // conditions, so they should not pay for the additional scalar coverage.
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 3);
    if !scalar_fast_path {
        a.b_cond(C_NE, slow);
        a.ldurb(9, 20, -15);
        a.sub_imm(20, 20, 16);
        a.cbz(9, false, false_target);
        a.b(done);
        a.bind(slow);
        emit_cond(a, COND_POP_TRUTHY, unwind);
        a.cbz(1, false, false_target);
        a.bind(done);
        return;
    }

    let non_bool = a.new_label();
    let scalar = a.new_label();
    let false_value = a.new_label();
    let number_value = a.new_label();
    a.b_cond(C_NE, non_bool);
    a.ldurb(1, 20, -15);
    a.sub_imm(20, 20, 16);
    a.cbz(1, false, false_target);
    a.b(done);

    a.bind(non_bool);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_EQ, number_value);
    a.cmp_imm_w(9, 2);
    a.b_cond(C_LS, false_value);
    a.b(slow);

    a.bind(false_value);
    a.movz(1, 0, 0);
    a.b(scalar);

    a.bind(number_value);
    a.ldur_d(0, 20, -8);
    a.fcmp_zero(0);
    a.cset_w(11, C_EQ); // +/-0 is falsy
    a.cset_w(12, super::C_VS); // NaN is falsy
    a.logic_w(1, 11, 11, 12);
    a.movz(12, 1, 0);
    a.logic_w(2, 1, 11, 12); // invert falsy to truthy

    a.bind(scalar);
    a.sub_imm(20, 20, 16);
    a.cbz(1, false, false_target);
    a.b(done);

    a.bind(slow);
    emit_cond(a, COND_POP_TRUTHY, unwind);
    a.cbz(1, false, false_target);
    a.bind(done);
}

#[cfg(test)]
mod tests {
    use super::{emit_pop_false, produces_numeric_scalar};
    use crate::bytecode::Op;
    use crate::{Completion, Engine};

    #[test]
    fn scalar_conditions_preserve_truthiness_and_helper_fallbacks() {
        let source = r#"
            function flag(value) { if (value) return 'T'; return 'F'; }
            function divFlag(left, right) { if (left / right) return 'T'; return 'F'; }
            function bitFlag(value) { if (value & 1n) return 'T'; return 'F'; }
            var values = [undefined, null, false, true, 0, -0, NaN, 1, -2, Infinity,
                          0n, 1n, '', 'x', Symbol(), {}, $262.IsHTMLDDA];
            for (var warm = 0; warm < 500; warm++) {
                for (var i = 0; i < values.length; i++) flag(values[i]);
                divFlag(warm & 1, 1);
                bitFlag(BigInt(warm & 3));
            }
            values.map(flag).join('') + '|' +
                [[0,1],[-0,1],[0,0],[1,1],[-2,1],[1,0]].map(x => divFlag(x[0],x[1])).join('') +
                '|' + [0n,1n,2n,3n].map(bitFlag).join('')
        "#;
        let result = Engine::new().eval(source, false).expect("parse");
        assert!(
            matches!(result, Completion::Value(value) if value == "FFFTFFFTTTFTFTTTF|FFFTTT|FTFT")
        );
    }

    #[test]
    fn only_numeric_producers_select_the_larger_template() {
        assert!(produces_numeric_scalar(&Op::BitAnd));
        assert!(produces_numeric_scalar(&Op::Div));
        assert!(produces_numeric_scalar(&Op::Neg));
        assert!(!produces_numeric_scalar(&Op::Add));
        assert!(!produces_numeric_scalar(&Op::StrictEq));
        assert!(!produces_numeric_scalar(&Op::LoadLocal(0)));
    }

    #[test]
    fn emitted_scalar_branch_consumes_exactly_one_operand() {
        use crate::jit::{asm::Asm, sys};
        use crate::value::Value;

        let mut a = Asm::new();
        let false_target = a.new_label();
        let unreachable_unwind = a.new_label();
        let exit = a.new_label();
        a.stp_pre(20, 30, -16);
        a.mov(20, 0);
        emit_pop_false(&mut a, false_target, unreachable_unwind, true);
        a.mov(0, 20);
        a.b(exit);
        a.bind(false_target);
        a.movz(0, 0, 0);
        a.b(exit);
        a.bind(unreachable_unwind);
        a.movz(0, 2, 0);
        a.bind(exit);
        a.ldp_post(20, 30, 16);
        a.ret();
        let words = a.finish();
        unsafe {
            let code = sys::alloc_exec(words.as_ptr().cast(), words.len() * 4);
            assert!(!code.is_null());
            let call: unsafe extern "C" fn(*mut Value) -> *mut Value = std::mem::transmute(code);
            for value in [
                Value::Undefined,
                Value::Empty,
                Value::Null,
                Value::Bool(false),
                Value::Num(0.0),
                Value::Num(-0.0),
                Value::Num(f64::NAN),
            ] {
                let mut stack = [value];
                assert!(call(stack.as_mut_ptr().add(1)).is_null());
            }
            for value in [
                Value::Bool(true),
                Value::Num(1.0),
                Value::Num(-2.0),
                Value::Num(f64::INFINITY),
            ] {
                let mut stack = [value];
                assert_eq!(call(stack.as_mut_ptr().add(1)), stack.as_mut_ptr());
            }
            sys::free_exec(code, words.len() * 4);
        }
    }
}
