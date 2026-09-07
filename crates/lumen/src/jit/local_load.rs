//! Native local loads: clone live values, transfer ownership at CFG-proven last uses.
use super::{asm::Asm, emit_exec, C_EQ, C_HI, C_LO};
use crate::value::JitLayout;

pub(super) fn emit(
    a: &mut Asm,
    slot: u16,
    pc: usize,
    layout: &JitLayout,
    last_use: bool,
    l_unwind: usize,
) {
    let rc_ok = layout.valid && layout.rc_strong_off < 256;
    let off = slot as u32 * 16;
    let slow = a.new_label();
    let done = a.new_label();
    a.ldrb_imm(9, 22, off);
    a.cmp_imm_w(9, 1); // Empty = TDZ throw → slow
    a.b_cond(C_EQ, slow);
    if last_use {
        // Transfer the slot's ownership, including BigInt, without inspecting
        // its representation. Leave Undefined so unwinding/frame cleanup is safe.
        a.ldr_imm(10, 22, off);
        a.ldr_imm(11, 22, off + 8);
        a.stur(10, 20, 0);
        a.stur(11, 20, 8);
        a.strb_imm(31, 22, off);
    } else if rc_ok {
        a.cmp_imm_w(9, 5);
        a.b_cond(C_EQ, slow);
        a.ldr_imm(10, 22, off);
        a.ldr_imm(11, 22, off + 8);
        a.stur(10, 20, 0);
        a.stur(11, 20, 8);
        let nobump = a.new_label();
        a.cmp_imm_w(9, 6);
        a.b_cond(C_LO, nobump);
        a.ldur(13, 11, layout.rc_strong_off as i32);
        a.add_imm(13, 13, 1);
        a.stur(13, 11, layout.rc_strong_off as i32);
        a.bind(nobump);
    } else {
        a.cmp_imm_w(9, 4);
        a.b_cond(C_HI, slow);
        a.ldr_imm(10, 22, off);
        a.ldr_imm(11, 22, off + 8);
        a.stur(10, 20, 0);
        a.stur(11, 20, 8);
    }
    a.add_imm(20, 20, 16);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc as u32, l_unwind);
    a.bind(done);
}
