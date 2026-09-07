//! Native access to fixed captured-binding slots, guarded against structural scope changes.
use super::{asm::Asm, emit_exec, JitCtx, C_EQ, C_LO, H_DROP_AT};
use crate::value::JitLayout;

fn pointer(a: &mut Asm, layout: &JitLayout, offset: usize, slow: usize, initialized: bool) {
    a.ldr_imm(14, 19, std::mem::offset_of!(JitCtx, captured_base) as u32);
    a.cbz(14, true, slow);
    a.ldr_imm(9, 19, 40); // env_raw points at RefCell<Scope>
    a.ldr_imm(10, 9, 0); // RefCell borrow flag: do not bypass a live Rust borrow.
    a.cbnz(10, true, slow);
    a.ldr_w_imm(10, 9, layout.scope_gen as u32);
    a.cbnz(10, false, slow); // structural changes invalidate the entry-time base pointer.
    a.mov_imm64(15, offset as u64);
    a.add_shifted(14, 14, 15, 0);
    if initialized {
        a.ldrb_imm(9, 14, layout.binding_init as u32);
        a.cbz(9, false, slow);
    }
    a.add_imm(14, 14, layout.binding_value as u32);
}

pub(super) fn load(a: &mut Asm, layout: &JitLayout, offset: usize, pc: u32, unwind: usize) {
    let slow = a.new_label();
    let done = a.new_label();
    pointer(a, layout, offset, slow, true);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 5); // BigInt keeps its checked clone path.
    a.b_cond(C_EQ, slow);
    a.ldr_imm(10, 14, 0);
    a.ldr_imm(11, 14, 8);
    a.stur(10, 20, 0);
    a.stur(11, 20, 8);
    let scalar = a.new_label();
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, scalar);
    a.ldur(12, 11, layout.rc_strong_off as i32);
    a.add_imm(12, 12, 1);
    a.stur(12, 11, layout.rc_strong_off as i32);
    a.bind(scalar);
    a.add_imm(20, 20, 16);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, unwind);
    a.bind(done);
}

pub(super) fn store(
    a: &mut Asm,
    layout: &JitLayout,
    offset: usize,
    initialize: bool,
    pc: u32,
    unwind: usize,
) {
    let slow = a.new_label();
    let done = a.new_label();
    let commit = a.new_label();
    let last = a.new_label();
    pointer(a, layout, offset, slow, !initialize);
    a.ldurb(9, 20, -16);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 5);
    a.b_cond(C_EQ, slow);
    a.cmp_imm_w(9, 6);
    a.b_cond(C_LO, commit);
    a.ldr_imm(11, 14, 8);
    a.ldur(12, 11, layout.rc_strong_off as i32);
    a.cmp_imm_x(12, 1);
    a.b_cond(C_EQ, last);
    a.sub_imm(12, 12, 1);
    a.stur(12, 11, layout.rc_strong_off as i32);
    a.b(commit);
    a.bind(last);
    // Value destruction cannot execute JS or change the activation's binding layout.
    a.stp_pre(14, 15, -16);
    a.mov(0, 19);
    a.movz(1, 0, 0);
    a.mov(2, 14);
    a.ldr_imm(16, 21, (H_DROP_AT * 8) as u32);
    a.blr(16);
    a.ldp_post(14, 15, 16);
    a.bind(commit);
    if initialize {
        a.movz(9, 1, 0);
        a.sturb(
            9,
            14,
            layout.binding_init as i32 - layout.binding_value as i32,
        );
    }
    a.ldur(9, 20, -16);
    a.ldur(10, 20, -8);
    a.stur(9, 14, 0);
    a.stur(10, 14, 8);
    a.sub_imm(20, 20, 16);
    a.b(done);
    a.bind(slow);
    emit_exec(a, pc, unwind);
    a.bind(done);
}
