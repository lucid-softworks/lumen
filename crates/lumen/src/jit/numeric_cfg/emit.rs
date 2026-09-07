//! Fixed register homes across numeric CFG edges. All exits restore owned VM locals.
use super::super::{asm::Asm, UpdKind, C_NE};
use super::plan::{Plan, Step};

fn home(plan: &Plan, slot: u16) -> u32 {
    16 + plan.locals.iter().position(|s| *s == slot).unwrap() as u32
}

pub(super) fn flush(a: &mut Asm, plan: &Plan) {
    for &slot in &plan.dirty {
        a.str_d_imm(home(plan, slot), 22, slot as u32 * 16 + 8);
    }
}

pub(super) fn emit(
    a: &mut Asm,
    plan: &Plan,
    layout: &crate::value::JitLayout,
    pc_labels: &[usize],
) -> usize {
    let plain = a.new_label();
    let branches = super::branches::Branches::new(a, plan, pc_labels, plain);
    for &slot in &plan.locals {
        a.ldrb_imm(9, 22, slot as u32 * 16);
        a.cmp_imm_w(9, 4);
        a.b_cond(C_NE, plain);
    }
    for &slot in &plan.locals {
        a.ldr_d_imm(home(plan, slot), 22, slot as u32 * 16 + 8);
    }
    super::arrays::preamble(a, plan, layout, plain);
    #[cfg(test)]
    super::record_entry(a);
    a.movz(17, 1024, 0);
    let mut guards = Vec::new();
    a.b(branches.label(plan.head));
    for (index, block) in plan.blocks.iter().enumerate() {
        let next = plan.blocks.get(index + 1).map(|b| b.start);
        a.bind(branches.label(block.start));
        let mut depth = 0;
        for &step in &block.steps {
            match step {
                Step::GetElem { slot, pc } => {
                    let guard = a.new_label();
                    guards.push((guard, pc, depth));
                    super::arrays::read(a, plan, slot, 24 + depth - 1, guard);
                }
                Step::Compare { condition, yes, no } => {
                    branches.compare(a, block.start, condition, yes, no, next);
                    depth = 0;
                }
                Step::Jump(pc) => {
                    branches.jump(a, block.start, pc, next);
                }
                _ => value_step(a, plan, step, &mut depth),
            }
        }
        debug_assert_eq!(depth, 0);
    }
    branches.exits(a);
    for (guard, pc, depth) in guards {
        a.bind(guard);
        #[cfg(test)]
        super::record_bail(a);
        flush(a, plan);
        materialize_stack(a, depth);
        a.b(pc_labels[pc]);
    }
    plain
}

fn value_step(a: &mut Asm, plan: &Plan, step: Step, depth: &mut u32) {
    let top = 24 + *depth;
    match step {
        Step::Constant(bits) => {
            a.mov_imm64(9, bits);
            a.fmov_d_x(top, 9);
            *depth += 1;
        }
        Step::Load(s) => {
            a.fmov_d_d(top, home(plan, s));
            *depth += 1;
        }
        Step::Store(s) => {
            *depth -= 1;
            a.fmov_d_d(home(plan, s), top - 1);
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
            let reg = home(plan, s);
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

fn materialize_stack(a: &mut Asm, depth: u32) {
    a.movz(9, 4, 0);
    for index in 0..depth {
        a.str_imm(9, 20, index * 16);
        a.str_d_imm(24 + index, 20, index * 16 + 8);
    }
    a.add_imm(20, 20, depth * 16);
}
