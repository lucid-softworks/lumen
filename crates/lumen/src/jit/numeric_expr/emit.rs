//! Borrow roots and intermediate objects until every guard succeeds, then publish the existing terminal’s operands.
use super::plan::{Plan, Rep, Source};
use crate::bytecode::Chunk;
use crate::jit::{asm::Asm, property_probe, C_NE};
use crate::value::{JitLayout, PACK_OBJ};

pub(super) fn emit(a: &mut Asm, chunk: &Chunk, plan: &Plan, layout: &JitLayout, labels: &[usize]) {
    let plain = a.new_label();
    for value in &plan.values {
        let register = value.register;
        let rep = value.rep.expect("fully constrained expression");
        match value.source {
            Source::Local(slot) => {
                a.add_imm(14, 22, slot as u32 * 16);
                root(a, rep, register, plain);
            }
            Source::This => {
                a.ldr_imm(14, 19, 48);
                root(a, rep, register, plain);
            }
            Source::Property {
                receiver,
                name,
                cache,
            } => {
                a.mov(11, plan.values[receiver].register);
                property_probe::own_entry_with_hint(
                    a,
                    layout,
                    chunk.jit_cache_ptr(cache),
                    chunk.jit_name(name),
                    chunk.jit_cache_preferred(cache),
                    plain,
                );
                property(a, layout, rep, register, plain);
            }
            Source::Constant(bits) => {
                a.mov_imm64(9, bits);
                a.fmov_d_x(register, 9);
            }
            Source::Arithmetic {
                left,
                right,
                operation,
            } => {
                a.f_arith(
                    operation,
                    register,
                    plan.values[left].register,
                    plan.values[right].register,
                );
            }
            Source::Negate(input) => a.fneg(register, plan.values[input].register),
        }
    }
    commit(a, plan, layout);
    #[cfg(test)]
    super::record_success(a, plan.prefix_depth);
    a.b(labels[plan.end]);
    a.bind(plain);
}

fn root(a: &mut Asm, rep: Rep, register: u32, fail: usize) {
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, if rep == Rep::Object { 8 } else { 4 });
    a.b_cond(C_NE, fail);
    match rep {
        Rep::Object => a.ldr_imm(register, 14, 8),
        Rep::Number => a.ldr_d_imm(register, 14, 8),
    }
}

fn property(a: &mut Asm, layout: &JitLayout, rep: Rep, register: u32, fail: usize) {
    match rep {
        Rep::Object => {
            a.ldur(13, 15, layout.entry_value as i32);
            a.lsr_imm(9, 13, 48);
            a.movz(16, (PACK_OBJ >> 48) as u32, 0);
            a.cmp_reg_x(9, 16);
            a.b_cond(C_NE, fail);
            a.lsl_imm(register, 13, 16);
            a.lsr_imm(register, register, 16);
        }
        Rep::Number => {
            crate::jit::emit_region_packed_number(a, 15, layout.entry_value as i32, register, fail)
        }
    }
}

fn commit(a: &mut Asm, plan: &Plan, layout: &JitLayout) {
    let offset = if let Some(destination) = plan.destination {
        let destination = plan.values[destination].register;
        a.ldur(9, destination, layout.rc_strong_off as i32);
        a.add_imm(9, 9, 1);
        a.stur(9, destination, layout.rc_strong_off as i32);
        a.movz(9, 8, 0);
        a.str_imm(9, 20, 0);
        a.str_imm(destination, 20, 8);
        16
    } else {
        0
    };
    a.movz(9, 4, 0);
    a.str_imm(9, 20, offset);
    a.str_d_imm(plan.values[plan.result].register, 20, offset + 8);
    a.add_imm(20, 20, offset + 16);
}
