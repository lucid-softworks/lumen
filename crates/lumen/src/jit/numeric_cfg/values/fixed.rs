//! Original fixed operand-register layout, retained as a same-binary control.
use super::super::plan::{Plan, Step};
use crate::jit::{asm::Asm, UpdKind};

pub(super) fn step(a: &mut Asm, plan: &Plan, step: Step, depth: &mut u32) {
    let top = 24 + *depth;
    match step {
        Step::Constant(bits) => {
            a.mov_imm64(9, bits);
            a.fmov_d_x(top, 9);
            *depth += 1;
        }
        Step::Input(index) => {
            a.fmov_d_d(top, super::super::inputs::register(index));
            *depth += 1;
        }
        Step::Load(s) => {
            a.fmov_d_d(top, plan.home(s));
            *depth += 1;
        }
        Step::Store(s) => {
            *depth -= 1;
            a.fmov_d_d(plan.home(s), top - 1);
        }
        Step::Arithmetic(op) => {
            a.f_arith(op, top - 2, top - 2, top - 1);
            *depth -= 1;
        }
        Step::Negate => a.fneg(top - 1, top - 1),
        Step::Duplicate => {
            a.fmov_d_d(top, top - 1);
            *depth += 1;
        }
        Step::Pop => *depth -= 1,
        Step::Update(s, kind) => {
            let reg = plan.home(s);
            if matches!(kind, UpdKind::PostInc | UpdKind::PostDec) {
                a.fmov_d_d(top, reg);
            }
            a.fmov_one(0);
            let sub = matches!(
                kind,
                UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
            );
            a.f_arith(sub as u32, reg, reg, 0);
            if matches!(kind, UpdKind::PreInc | UpdKind::PreDec) {
                a.fmov_d_d(top, reg);
            }
            if !matches!(kind, UpdKind::IncDiscard | UpdKind::DecDiscard) {
                *depth += 1;
            }
        }
        Step::GetElem { .. } | Step::Compare { .. } | Step::Jump(_) => unreachable!(),
    }
}
