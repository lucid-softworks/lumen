//! Discover numeric regions from CFG edges, without matching a fixed instruction sequence.
use super::inputs::Input;
use crate::bytecode::{Chunk, Op, UpdKind};
use crate::jit_ir::{Cfg, RegionIr};

#[derive(Clone, Copy, Debug)]
pub(super) enum Step {
    Constant(u64),
    Load(u16),
    Input(u8),
    GetElem {
        slot: u16,
        pc: usize,
    },
    Store(u16),
    Update(u16, UpdKind),
    Arithmetic(u32),
    Negate,
    Duplicate,
    Pop,
    Compare {
        condition: u32,
        yes: usize,
        no: usize,
    },
    Jump(usize),
}

pub(super) struct Block {
    pub start: usize,
    pub end: usize,
    pub steps: Vec<Step>,
}

pub(super) struct Plan {
    pub head: usize,
    pub blocks: Vec<Block>,
    pub locals: Vec<u16>,
    pub dirty: Vec<u16>,
    pub receivers: Vec<u16>,
    pub inputs: Vec<Input>,
    pub exits: Vec<usize>,
}

impl Plan {
    pub(super) fn home(&self, slot: u16) -> u32 {
        16 + self.locals.iter().position(|s| *s == slot).unwrap() as u32
    }
}

pub(super) fn build(chunk: &Chunk, cfg: &Cfg, head: usize) -> Option<Plan> {
    if !cfg.handler_roots().is_empty() || cfg.linear_loop_latch(chunk.jit_ops(), head).is_some() {
        return None;
    }
    let region = cfg.loop_at_header(head)?;
    if region.blocks.len() > 32 || cfg.stack_depth_at(head) != Some(0) {
        return None;
    }
    RegionIr::build_loop(chunk, cfg, head).ok()?;
    let mut plan = Plan {
        head,
        blocks: Vec::new(),
        locals: Vec::new(),
        dirty: Vec::new(),
        receivers: Vec::new(),
        inputs: Vec::new(),
        exits: Vec::new(),
    };
    let mut size = 0;
    for id in &region.blocks {
        let block = &cfg.blocks()[id.0 as usize];
        size += block.end - block.start;
        if size > 256
            || block.start < head
            || block.stack_in != Some(0)
            || block.stack_out != Some(0)
        {
            return None;
        }
        let steps = translate(chunk, cfg, block.start, block.end, &mut plan)?;
        plan.blocks.push(Block {
            start: block.start,
            end: block.end,
            steps,
        });
    }
    if plan.locals.is_empty() || plan.locals.len() > 8 {
        return None;
    }
    if plan.receivers.len() > 4 || plan.receivers.iter().any(|s| plan.locals.contains(s)) {
        return None;
    }
    if plan
        .inputs
        .iter()
        .filter_map(Input::receiver)
        .any(|s| plan.locals.contains(&s) || s as usize * 16 + 8 >= 4096)
    {
        return None;
    }
    for (_, target) in &region.exits {
        let pc = cfg.blocks()[target.0 as usize].start;
        if cfg.stack_depth_at(pc) != Some(0) {
            return None;
        }
        if !plan.exits.contains(&pc) {
            plan.exits.push(pc);
        }
    }
    (!plan.exits.is_empty()).then_some(plan)
}

fn local(plan: &mut Plan, slot: u16, write: bool) -> Option<()> {
    if slot as usize * 16 + 8 >= 4096 {
        return None;
    }
    if !plan.locals.contains(&slot) {
        plan.locals.push(slot);
    }
    if write && !plan.dirty.contains(&slot) {
        plan.dirty.push(slot);
    }
    Some(())
}

fn translate(
    chunk: &Chunk,
    cfg: &Cfg,
    start: usize,
    end: usize,
    plan: &mut Plan,
) -> Option<Vec<Step>> {
    let ops = chunk.jit_ops();
    let mut steps = Vec::new();
    let mut pc = start;
    while pc < end {
        if cfg.stack_depth_at(pc)? > 8 {
            return None;
        }
        let step = if let Some(source) = Input::decode(chunk, ops[pc]) {
            input(plan, source)?
        } else {
            match ops[pc] {
                Op::Const(k) => Step::Constant(chunk.jit_const_num(k)?),
                Op::LoadLocal(s) => {
                    local(plan, s, false)?;
                    Step::Load(s)
                }
                Op::GetElemLocal(slot) => {
                    if slot as usize * 16 + 8 >= 4096 {
                        return None;
                    }
                    if !plan.receivers.contains(&slot) {
                        plan.receivers.push(slot);
                    }
                    Step::GetElem { slot, pc }
                }
                Op::StoreLocal(s) => {
                    local(plan, s, true)?;
                    Step::Store(s)
                }
                Op::UpdateLocal(s, k) => {
                    local(plan, s, true)?;
                    Step::Update(s, k)
                }
                Op::Add => Step::Arithmetic(0),
                Op::Sub => Step::Arithmetic(1),
                Op::Mul => Step::Arithmetic(2),
                Op::Div => Step::Arithmetic(3),
                Op::Neg => Step::Negate,
                Op::Dup => Step::Duplicate,
                Op::Pop => Step::Pop,
                Op::Jump(target) => Step::Jump(target as usize),
                op => {
                    let condition = false_condition(op)?;
                    let Op::JumpIfFalse(no) = *ops.get(pc + 1)? else {
                        return None;
                    };
                    if pc + 2 != end || cfg.stack_depth_at(pc) != Some(2) {
                        return None;
                    }
                    pc += 1;
                    Step::Compare {
                        condition,
                        yes: pc + 1,
                        no: no as usize,
                    }
                }
            }
        };
        steps.push(step);
        pc += 1;
    }
    if cfg.stack_depth_at(end.saturating_sub(1))? > 8 {
        return None;
    }
    if !matches!(steps.last(), Some(Step::Compare { .. } | Step::Jump(_))) {
        steps.push(Step::Jump(end));
    }
    Some(steps)
}

fn input(plan: &mut Plan, input: Input) -> Option<Step> {
    let index = if let Some(index) = plan.inputs.iter().position(|old| old.same_source(&input)) {
        index
    } else {
        if plan.inputs.len() == 6 {
            return None;
        }
        plan.inputs.push(input);
        plan.inputs.len() - 1
    };
    Some(Step::Input(index as u8))
}

fn false_condition(op: Op) -> Option<u32> {
    // Ordered floating-point comparisons must send NaN to the false edge.
    Some(match op {
        Op::Lt => 5,
        Op::Gt => 13,
        Op::Le => 8,
        Op::Ge => 11,
        Op::StrictEq | Op::EqEq => 1,
        Op::StrictNotEq | Op::NotEq => 0,
        _ => return None,
    })
}
