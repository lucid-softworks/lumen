//! Supported CFG paths through a natural loop; unsupported effects become exact exits.
use crate::{
    bytecode::{Chunk, Op},
    jit_ir::{Cfg, RegionIr},
};
use std::collections::{BTreeSet, VecDeque};

pub(super) struct Plan {
    pub head: usize,
    pub pcs: Vec<usize>,
    pub exits: Vec<usize>,
    pub slots: usize,
    pub depth: usize,
}

impl Plan {
    pub fn stack(&self, index: usize) -> u32 {
        ((self.slots + index) * 16) as u32
    }
    pub fn control(&self) -> u32 {
        self.stack(self.depth)
    }
    pub fn frame_bytes(&self) -> u32 {
        self.control() + 32
    }
}

pub(super) fn build(chunk: &Chunk, cfg: &Cfg, head: usize) -> Option<Plan> {
    let region = cfg.loop_at_header(head)?;
    let (_, slots) = chunk.jit_frame();
    if slots > 16 || cfg.max_settled_stack() > 8 || cfg.stack_depth_at(head)? != 0 {
        return None;
    }
    let ir = RegionIr::build_loop(chunk, cfg, head).ok()?;
    if region.blocks.len() > 64 || !has_object_work(chunk, &ir) {
        return None;
    }
    let allowed: BTreeSet<_> = region
        .blocks
        .iter()
        .flat_map(|id| {
            let block = &cfg.blocks()[id.0 as usize];
            block.start..block.end
        })
        .collect();
    if allowed.len() > 256 {
        return None;
    }
    let mut queue = VecDeque::from([head]);
    let mut pcs = BTreeSet::new();
    let mut exits = BTreeSet::new();
    let mut backedge = false;
    while let Some(pc) = queue.pop_front() {
        if pcs.contains(&pc) || exits.contains(&pc) {
            continue;
        }
        if !allowed.contains(&pc)
            || !supported(chunk, chunk.jit_ops()[pc])
            || matches!(chunk.jit_ops()[pc], Op::JumpIfFalse(t) | Op::InlineGuard(_,t) if t as usize <= pc)
        {
            exits.insert(pc);
            continue;
        }
        pcs.insert(pc);
        for next in successors(chunk.jit_ops()[pc], pc) {
            if next == head && pc >= head {
                backedge = true;
            }
            queue.push_back(next);
        }
    }
    if !backedge || exits.is_empty() || exits.iter().any(|&pc| pc >= chunk.jit_ops().len()) {
        return None;
    }
    Some(Plan {
        head,
        pcs: pcs.into_iter().collect(),
        exits: exits.into_iter().collect(),
        slots,
        depth: cfg.max_settled_stack(),
    })
}

fn has_object_work(chunk: &Chunk, ir: &RegionIr) -> bool {
    // Follow SSA uses rather than assuming that every array element is an Object.
    let mut needed = std::collections::HashSet::new();
    for block in &ir.blocks {
        for inst in &block.insts {
            if matches!(chunk.jit_ops()[inst.pc], Op::GetMethod(..)) {
                return true;
            }
            if matches!(
                chunk.jit_ops()[inst.pc],
                Op::GetProp(..)
                    | Op::GetPropLocal(..)
                    | Op::SetProp(..)
                    | Op::SetPropDrop(..)
                    | Op::SetPropLocalDrop(..)
            ) {
                if let Some(value) = inst.inputs.first() {
                    needed.insert(*value);
                }
            }
        }
    }
    loop {
        let old = needed.len();
        for block in &ir.blocks {
            for inst in &block.insts {
                if matches!(chunk.jit_ops()[inst.pc], Op::LoadLocal(_) | Op::LoadThis)
                    && inst.outputs.iter().any(|v| needed.contains(v))
                {
                    needed.extend(inst.inputs.iter().copied());
                }
            }
            for edge in &block.successors {
                let target = ir
                    .blocks
                    .iter()
                    .find(|b| b.cfg_block == edge.target)
                    .unwrap();
                for ((_, parameter), value) in target.params.iter().zip(&edge.args) {
                    if needed.contains(parameter) {
                        needed.insert(*value);
                    }
                }
            }
        }
        if needed.len() == old {
            break;
        }
    }
    ir.blocks.iter().flat_map(|b| &b.insts).any(|inst| {
        matches!(chunk.jit_ops()[inst.pc], Op::GetElem | Op::GetElemLocal(_))
            && inst.outputs.iter().any(|v| needed.contains(v))
    })
}

fn supported(chunk: &Chunk, op: Op) -> bool {
    match op {
        Op::Const(k) => chunk.jit_const_num(k).is_some(),
        Op::LoadLocal(_)
        | Op::StoreLocal(_)
        | Op::LoadThis
        | Op::LoadName(..)
        | Op::GetProp(..)
        | Op::GetPropThis(..)
        | Op::GetPropLocal(..)
        | Op::GetMethod(..)
        | Op::GetElem
        | Op::GetElemLocal(_)
        | Op::SetProp(..)
        | Op::SetPropDrop(..)
        | Op::SetPropThisDrop(..)
        | Op::SetPropLocalDrop(..)
        | Op::UpdateLocal(..)
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Neg
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge
        | Op::EqEq
        | Op::StrictEq
        | Op::NotEq
        | Op::StrictNotEq
        | Op::Undef
        | Op::Pop
        | Op::Dup
        | Op::Dup2
        | Op::InlineGuard(..)
        | Op::Jump(_)
        | Op::JumpIfFalse(_) => true,
        _ => false,
    }
}

pub(super) fn successors(op: Op, pc: usize) -> Vec<usize> {
    match op {
        Op::Jump(target) => vec![target as usize],
        Op::JumpIfFalse(target) | Op::InlineGuard(_, target) => vec![pc + 1, target as usize],
        _ => vec![pc + 1],
    }
}
