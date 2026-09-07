//! Fixed register homes across numeric CFG edges. All exits restore owned VM locals.
use super::super::{asm::Asm, C_NE};
use super::plan::{Plan, Step};

pub(super) fn flush(a: &mut Asm, plan: &Plan) {
    for &slot in &plan.dirty {
        a.str_d_imm(plan.home(slot), 22, slot as u32 * 16 + 8);
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
        a.ldr_d_imm(plan.home(slot), 22, slot as u32 * 16 + 8);
    }
    super::arrays::preamble(a, plan, layout, plain);
    #[cfg(test)]
    super::record_entry(a);
    a.movz(17, 1024, 0);
    let mut guards = Vec::new();
    let allocated = std::env::var_os("LUMEN_JIT_NO_CFG_REGALLOC").is_none();
    a.b(branches.label(plan.head));
    for (index, block) in plan.blocks.iter().enumerate() {
        let next = plan.blocks.get(index + 1).map(|b| b.start);
        a.bind(branches.label(block.start));
        let mut values = super::values::Values::new(allocated);
        for (step_index, &step) in block.steps.iter().enumerate() {
            let following = block.steps.get(step_index + 1).copied();
            match step {
                Step::GetElem { slot, pc } => {
                    let guard = a.new_label();
                    guards.push((guard, pc, values.snapshot()));
                    values.read(a, plan, slot, guard, following);
                }
                Step::Compare { condition, yes, no } => {
                    let (lhs, rhs) = values.comparison();
                    a.fcmp(lhs, rhs);
                    branches.compare(a, block.start, condition, yes, no, next);
                }
                Step::Jump(pc) => {
                    branches.jump(a, block.start, pc, next);
                }
                _ => values.step(a, plan, step, following),
            }
        }
        debug_assert!(values.is_empty());
    }
    branches.exits(a);
    for (guard, pc, registers) in guards {
        a.bind(guard);
        #[cfg(test)]
        super::record_bail(a);
        flush(a, plan);
        materialize_stack(a, &registers);
        a.b(pc_labels[pc]);
    }
    plain
}

fn materialize_stack(a: &mut Asm, registers: &[u32]) {
    a.movz(9, 4, 0);
    for (index, &register) in registers.iter().enumerate() {
        a.str_imm(9, 20, index as u32 * 16);
        a.str_d_imm(register, 20, index as u32 * 16 + 8);
    }
    a.add_imm(20, 20, registers.len() as u32 * 16);
}
