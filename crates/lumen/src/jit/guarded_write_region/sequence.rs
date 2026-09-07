//! Consecutive numeric writes with exact pre-op snapshots, including chained assignments.
use super::values::{Builder, Expression, Source};
use crate::{
    bytecode::{Chunk, Op},
    jit_ir::Cfg,
};

pub(super) struct Write {
    pub receiver: usize,
    pub number: usize,
    pub name: u32,
    pub cache: u32,
}

pub(super) struct Step {
    pub pc: usize,
    pub before: Vec<usize>,
    pub values: std::ops::Range<usize>,
    pub write: Option<Write>,
    pub committed: bool,
}

impl Step {
    pub(super) fn can_fail(&self, values: &Expression) -> bool {
        self.write.is_some()
            || self
                .values
                .clone()
                .any(|at| match values.values[at].source {
                    Source::Local(_)
                    | Source::This
                    | Source::Name(..)
                    | Source::Property { .. } => true,
                    Source::Constant(_) | Source::Arithmetic { .. } | Source::Negate(_) => false,
                })
    }
}

pub(super) struct Plan {
    pub values: Expression,
    pub steps: Vec<Step>,
    pub prefix: usize,
    pub join: usize,
}

pub(super) fn build(chunk: &Chunk, cfg: &Cfg, start: usize) -> Option<Plan> {
    if !cfg.handler_roots().is_empty() {
        return None;
    }
    let prefix = cfg.stack_depth_at(start)?;
    let mut values = Builder::default();
    let mut steps = Vec::new();
    let mut writes = 0;
    for pc in start..start.checked_add(32)? {
        let op = *chunk.jit_ops().get(pc)?;
        let before = values.snapshot();
        if cfg.stack_depth_at(pc)? != prefix.checked_add(before.len())? {
            return None;
        }
        let first = values.value_count();
        let committed = writes > 0;
        let write = match op {
            Op::SetProp(name, cache)
            | Op::SetPropDrop(name, cache)
            | Op::SetPropThisDrop(name, cache)
            | Op::SetPropLocalDrop(_, name, cache) => {
                let (receiver, number) = values.sequence_store(op)?;
                writes += 1;
                Some(Write {
                    receiver,
                    number,
                    name,
                    cache,
                })
            }
            _ => {
                values.step(chunk, op)?;
                None
            }
        };
        steps.push(Step {
            pc,
            before,
            values: first..values.value_count(),
            write,
            committed,
        });
        if writes >= 2 && values.snapshot().is_empty() {
            if cfg.stack_depth_at(pc + 1)? != prefix {
                return None;
            }
            return Some(Plan {
                values: values.sequence_finish()?,
                steps,
                prefix,
                join: pc + 1,
            });
        }
    }
    None
}
