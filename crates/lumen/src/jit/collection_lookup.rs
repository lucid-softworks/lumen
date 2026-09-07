//! Exact builtin call-IC hits can borrow receiver/key operands without native dispatch.
use super::{asm::Asm, C_HI, C_LO, H_COLLECTION_LOOKUP};
use crate::builtins::collection_lookup::{MAP_GET, SET_HAS};

pub(super) fn emit(a: &mut Asm, pc: usize, done: usize, unwind: usize) {
    let next = a.new_label();
    a.cmp_imm_w(9, MAP_GET as u32);
    a.b_cond(C_LO, next);
    a.cmp_imm_w(9, SET_HAS as u32);
    a.b_cond(C_HI, next);
    a.mov(0, 19);
    a.movz(1, pc as u32, 0);
    a.add_shifted(1, 1, 9, 16); // Pack the guarded intrinsic id above the source pc.
    a.mov(2, 20);
    a.ldr_imm(16, 21, (H_COLLECTION_LOOKUP * 8) as u32);
    a.blr(16);
    a.mov(20, 0);
    a.cbnz(1, false, unwind);
    a.b(done);
    a.bind(next);
}
