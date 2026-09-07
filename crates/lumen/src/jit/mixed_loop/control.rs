//! Original branch semantics over a borrowed shadow operand stack.
use super::{plan::Plan, shadow};
use crate::{
    bytecode::Chunk,
    jit::{asm::Asm, C_EQ, C_NE, C_VS},
    value::{Gc, JitLayout},
};

pub(super) fn condition(a: &mut Asm, at: u32, yes: usize, no: usize, fail: usize) {
    let boolean = a.new_label();
    let number = a.new_label();
    a.ldrb_imm(9, 23, at);
    a.cmp_imm_w(9, 3);
    a.b_cond(C_EQ, boolean);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_EQ, number);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_EQ, fail); // HTMLDDA Objects are falsy; keep Object truthiness checked.
    a.cbz(9, false, no); // Undefined
    a.cmp_imm_w(9, 2);
    a.b_cond(C_EQ, no); // Null
    a.b(fail); // Strings/BigInts remain on the checked baseline path.
    a.bind(boolean);
    a.ldrb_imm(9, 23, at + 1);
    a.cbz(9, false, no);
    a.b(yes);
    a.bind(number);
    a.ldr_d_imm(16, 23, at + 8);
    a.fmov_d_x(17, 31);
    a.fcmp(16, 17);
    a.b_cond(C_EQ, no);
    a.b_cond(C_VS, no);
    a.b(yes);
}

pub(super) fn inline_guard(
    a: &mut Asm,
    chunk: &Chunk,
    plan: &Plan,
    target: u32,
    depth: usize,
    destinations: (usize, usize),
    layout: &JitLayout,
) {
    let (yes, no) = destinations;
    let target = chunk.jit_inline_target(target);
    let owner: Option<Gc> = target.pin.upgrade();
    if owner.is_none() || !layout.valid {
        a.b(no);
        return;
    }
    let stored = unsafe { *(&owner as *const Option<Gc> as *const usize) };
    if target.expected_env != 0 {
        a.ldr_imm(9, 19, 40);
        a.mov_imm64(10, target.expected_env as u64);
        a.cmp_reg_x(9, 10);
        a.b_cond(C_NE, no);
    }
    let callee = plan.stack(depth - usize::from(target.argc) - 1);
    shadow::object(a, 23, callee, no);
    a.mov_imm64(9, stored as u64);
    a.cmp_reg_x(0, 9);
    a.b_cond(C_NE, no);
    if target.check_this {
        shadow::object(a, 23, callee - 16, no);
    }
    a.b(yes);
}
