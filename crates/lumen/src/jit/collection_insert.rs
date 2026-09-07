//! Consume exact builtin insertion calls at their ordinary arity.
use super::{asm::Asm, C_NE, H_COLLECTION_MAP_SET, H_COLLECTION_SET_ADD};
use crate::builtins::collection_insert::{MAP_SET, SET_ADD};

pub(super) fn emit(a: &mut Asm, pc: usize, argc: usize, done: usize, unwind: usize) {
    let (id, helper) = if argc == 1 {
        (SET_ADD, H_COLLECTION_SET_ADD)
    } else {
        (MAP_SET, H_COLLECTION_MAP_SET)
    };
    let next = a.new_label();
    a.cmp_imm_w(9, id as u32);
    a.b_cond(C_NE, next);
    a.mov(0, 19);
    a.movz(1, pc as u32, 0);
    a.mov(2, 20);
    a.ldr_imm(16, 21, (helper * 8) as u32);
    a.blr(16);
    a.mov(20, 0);
    a.cbnz(1, false, unwind);
    a.b(done);
    a.bind(next);
}
