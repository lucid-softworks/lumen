//! Native loop control with borrowed wide locals; every exit publishes complete VM owners.
use super::{compare_branch, control, operations, plan::Plan, shadow, stats::Stats};
use crate::{
    bytecode::{Chunk, Op},
    jit::{asm::Asm, local_exit},
    jit_ir::Cfg,
    value::JitLayout,
};

pub(super) fn emit(
    a: &mut Asm,
    chunk: &Chunk,
    cfg: &Cfg,
    plan: &Plan,
    layout: &JitLayout,
    baseline: &[usize],
) {
    let stats = Stats::new(chunk, plan.head);
    let labels: Vec<_> = plan.pcs.iter().map(|&pc| (pc, a.new_label())).collect();
    let exits: Vec<_> = plan.exits.iter().map(|&pc| (pc, a.new_label())).collect();
    let destination = |pc| {
        labels
            .iter()
            .chain(&exits)
            .find(|(p, _)| *p == pc)
            .unwrap()
            .1
    };
    let mut state = LoopState {
        guards: Vec::new(),
        stats: &stats,
    };
    initialize_frame(a, plan);
    stats.entry(a);
    a.b(destination(plan.head));
    let compare = std::env::var_os("LUMEN_JIT_NO_MIXED_LOOP_COMPARE").is_none();
    let mut consumed = None;
    for &(pc, label) in &labels {
        a.bind(label);
        if consumed == Some(pc) {
            consumed = None;
            continue;
        }
        let op = chunk.jit_ops()[pc];
        let depth = cfg.stack_depth_at(pc).unwrap();
        let fail = if operations::can_fail(op) {
            let fail = a.new_label();
            state.guards.push((pc, fail));
            fail
        } else {
            label
        };
        if let Some(pair) = compare
            .then(|| compare_branch::select(chunk.jit_ops(), cfg, plan, pc))
            .flatten()
        {
            compare_branch::emit(
                a,
                plan,
                &pair,
                fail,
                destination(pair.yes),
                destination(pair.no),
            );
            consumed = Some(pair.branch);
            continue;
        }
        emit_step(
            a,
            chunk,
            plan,
            layout,
            (pc, op, depth, fail),
            &destination,
            &mut state,
        );
    }
    publish_exits(
        a,
        cfg,
        plan,
        baseline,
        exits.iter().chain(&state.guards).copied(),
        &stats,
    );
}

fn initialize_frame(a: &mut Asm, plan: &Plan) {
    a.sub_imm(31, 31, plan.frame_bytes());
    a.stp_off(23, 24, 0);
    a.add_imm(23, 31, 16);
    for slot in 0..plan.slots {
        shadow::copy(a, 22, slot as u32 * 16, slot as u32 * 16);
    }
    a.movz(24, 1024, 0);
    #[cfg(test)]
    {
        a.str_imm(31, 23, plan.control());
        super::record_entry(a);
    }
}

fn publish_exits(
    a: &mut Asm,
    cfg: &Cfg,
    plan: &Plan,
    baseline: &[usize],
    exits: impl Iterator<Item = (usize, usize)>,
    stats: &Stats,
) {
    for (pc, label) in exits {
        a.bind(label);
        stats.exit(a, pc);
        #[cfg(test)]
        super::record_exit(a, plan.control());
        assert!(local_exit::emit_shadow(
            a,
            plan.slots,
            cfg.stack_depth_at(pc).unwrap()
        ));
        a.ldp_off(23, 24, 0);
        a.add_imm(31, 31, plan.frame_bytes());
        a.b(baseline[pc]);
    }
}

struct LoopState<'a> {
    guards: Vec<(usize, usize)>,
    stats: &'a Stats,
}

fn emit_step(
    a: &mut Asm,
    chunk: &Chunk,
    plan: &Plan,
    layout: &JitLayout,
    (pc, op, depth, fail): (usize, Op, usize, usize),
    destination: &impl Fn(usize) -> usize,
    state: &mut LoopState<'_>,
) {
    match op {
        Op::Jump(target) => {
            let target = target as usize;
            if target <= pc {
                state.stats.backward_jump(a);
                #[cfg(test)]
                super::record_iteration(a);
                let budget = a.new_label();
                state.guards.push((target, budget));
                a.sub_imm(24, 24, 1);
                a.cbz(24, false, budget);
            }
            a.b(destination(target));
        }
        Op::JumpIfFalse(target) => control::condition(
            a,
            plan.stack(depth - 1),
            destination(pc + 1),
            destination(target as usize),
            fail,
        ),
        Op::InlineGuard(target, no) => control::inline_guard(
            a,
            chunk,
            plan,
            target,
            depth,
            (destination(pc + 1), destination(no as usize)),
            layout,
        ),
        _ => {
            operations::emit(a, chunk, plan, layout, op, depth, fail);
            a.b(destination(pc + 1));
        }
    }
}
