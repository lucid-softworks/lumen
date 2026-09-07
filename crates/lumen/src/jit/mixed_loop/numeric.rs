//! Numeric guards precede all changes to shadow operands or locals.
use super::{plan::Plan, shadow};
use crate::{
    bytecode::{Op, UpdKind},
    jit::{asm::Asm, C_EQ, C_GE, C_GT, C_LS, C_MI, C_NE},
};

pub(super) fn emit(a: &mut Asm, plan: &Plan, op: Op, depth: usize, fail: usize) {
    match op {
        Op::UpdateLocal(slot, kind) => update(a, plan, slot, kind, depth, fail),
        Op::Neg => {
            let at = plan.stack(depth - 1);
            shadow::number(a, at, 16, fail);
            a.fneg(16, 16);
            shadow::write_number(a, at, 16);
        }
        _ => {
            let left = plan.stack(depth - 2);
            shadow::number(a, left, 16, fail);
            shadow::number(a, plan.stack(depth - 1), 17, fail);
            match op {
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    let operation = match op {
                        Op::Add => 0,
                        Op::Sub => 1,
                        Op::Mul => 2,
                        _ => 3,
                    };
                    a.f_arith(operation, 16, 16, 17);
                    shadow::write_number(a, left, 16);
                }
                _ => {
                    let condition = match op {
                        Op::Lt => C_MI,
                        Op::Le => C_LS,
                        Op::Gt => C_GT,
                        Op::Ge => C_GE,
                        Op::EqEq | Op::StrictEq => C_EQ,
                        Op::NotEq | Op::StrictNotEq => C_NE,
                        _ => unreachable!("planned comparison"),
                    };
                    a.fcmp(16, 17);
                    a.cset_w(9, condition);
                    a.movz(10, 3, 0);
                    a.str_imm(10, 23, left);
                    a.strb_imm(9, 23, left + 1); // Bool payload is byte one, not byte eight.
                    a.str_imm(31, 23, left + 8);
                }
            }
        }
    }
}

fn update(a: &mut Asm, plan: &Plan, slot: u16, kind: UpdKind, depth: usize, fail: usize) {
    let at = u32::from(slot) * 16;
    shadow::number(a, at, 16, fail);
    a.fmov_one(0);
    let subtract = matches!(
        kind,
        UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
    );
    a.f_arith(subtract as u32, 17, 16, 0);
    shadow::write_number(a, at, 17);
    match kind {
        UpdKind::PreInc | UpdKind::PreDec => shadow::write_number(a, plan.stack(depth), 17),
        UpdKind::PostInc | UpdKind::PostDec => shadow::write_number(a, plan.stack(depth), 16),
        _ => {}
    }
}
