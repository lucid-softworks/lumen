//! Non-owning wide Value cells; original physical owners remain live until publication.
use crate::jit::{asm::Asm, C_NE};

pub(super) fn copy(a: &mut Asm, source: u32, from: u32, to: u32) {
    a.ldr_imm(9, source, from);
    a.ldr_imm(10, source, from + 8);
    a.str_imm(9, 23, to);
    a.str_imm(10, 23, to + 8);
}

pub(super) fn number(a: &mut Asm, offset: u32, register: u32, fail: usize) {
    a.ldrb_imm(9, 23, offset);
    a.cmp_imm_w(9, 4);
    a.b_cond(C_NE, fail);
    a.ldr_d_imm(register, 23, offset + 8);
}

pub(super) fn object(a: &mut Asm, source: u32, offset: u32, fail: usize) {
    a.ldrb_imm(9, source, offset);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(0, source, offset + 8);
}

pub(super) fn write_number(a: &mut Asm, offset: u32, register: u32) {
    a.movz(9, 4, 0);
    a.str_imm(9, 23, offset);
    a.str_d_imm(register, 23, offset + 8);
}

pub(super) fn write_object(a: &mut Asm, offset: u32) {
    a.movz(9, 8, 0);
    a.str_imm(9, 23, offset);
    a.str_imm(0, 23, offset + 8);
}
