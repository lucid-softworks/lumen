//! Borrowed wide scalar/object homes. No ownership changes until final publication.
use crate::jit::{asm::Asm, C_EQ, C_LO, C_LS, C_NE};
use crate::value::{JitLayout, PACK_OBJ, PACK_SYM, PACK_UNDEFINED};
pub(super) fn offset(id: usize) -> u32 {
    (id * 16) as u32
}
pub(super) fn number(a: &mut Asm, id: usize, fp: u32, fail: usize) {
    a.ldrb_imm(9, 23, offset(id));
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, fail);
    a.ldr_d_imm(fp, 23, offset(id) + 8);
}
pub(super) fn object(a: &mut Asm, id: usize, reg: u32, fail: usize) {
    a.ldrb_imm(9, 23, offset(id));
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(reg, 23, offset(id) + 8);
}
pub(super) fn store(a: &mut Asm, id: usize, tag: u32, payload: u32) {
    a.movz(9, tag, 0);
    a.str_imm(9, 23, offset(id));
    a.str_imm(payload, 23, offset(id) + 8);
}
/// Packed x13 -> Number/Object wide home; unsupported primitive tags miss.
pub(super) fn packed(a: &mut Asm, id: usize, fail: usize) {
    let object = a.new_label();
    let number = a.new_label();
    let done = a.new_label();
    a.lsr_imm(9, 13, 48);
    a.movz(16, (PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_EQ, object);
    a.movz(16, (PACK_UNDEFINED >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_LO, number);
    a.movz(16, (PACK_SYM >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    a.b_cond(C_LS, fail);
    a.bind(number);
    store(a, id, 4, 13);
    a.b(done);
    a.bind(object);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    store(a, id, 8, 13);
    a.bind(done);
}
/// Names return actual wide Values; accept scalar/object tags only. Bool payload is
/// inline in its discriminant word, so preserve that full word rather than rebuilding it.
pub(super) fn wide(a: &mut Asm, id: usize, ptr: u32, fail: usize) {
    a.ldrb_imm(9, ptr, 0);
    a.cmp_imm_w(9, 1);
    a.b_cond(C_EQ, fail);
    let accepted = a.new_label();
    a.cmp_imm_w(9, 4);
    a.b_cond(C_LS, accepted);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.bind(accepted);
    a.ldr_imm(12, ptr, 0);
    a.ldr_imm(13, ptr, 8);
    a.str_imm(12, 23, offset(id));
    a.str_imm(13, 23, offset(id) + 8);
}
/// All predicates completed; clone Object owner then publish full wide Value.
pub(super) fn output(a: &mut Asm, l: &JitLayout, id: usize, fail: usize) {
    a.ldrb_imm(9, 23, offset(id));
    let scalar = a.new_label();
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, scalar);
    a.ldr_imm(13, 23, offset(id) + 8);
    a.ldur(10, 13, l.rc_strong_off as i32);
    a.mov_imm64(11, isize::MAX as u64);
    a.cmp_reg_x(10, 11);
    a.b_cond(crate::jit::C_HS, fail);
    a.add_imm(10, 10, 1);
    a.stur(10, 13, l.rc_strong_off as i32);
    a.bind(scalar);
    a.ldr_imm(12, 23, offset(id));
    a.ldr_imm(13, 23, offset(id) + 8);
    a.str_imm(12, 21, 0);
    a.str_imm(13, 21, 8);
}
