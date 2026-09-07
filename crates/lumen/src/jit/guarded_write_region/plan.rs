//! Bounded acyclic branches ending in exactly one field write on every path.
use super::values::{Builder, Expression};
use crate::{
    bytecode::{Chunk, Op},
    jit_ir::Cfg,
};

pub(super) enum Node {
    Branch {
        values: Expression,
        comparison: Op,
        yes: Box<Node>,
        no: Box<Node>,
    },
    Store {
        values: Expression,
        name: u32,
        cache: u32,
        continuation: usize,
    },
}

pub(super) struct Plan {
    pub root: Node,
    pub pcs: Vec<usize>,
    pub join: usize,
    pub prefix_depth: usize,
}

pub(super) fn build(chunk: &Chunk, cfg: &Cfg, start: usize) -> Option<Plan> {
    let prefix = cfg.stack_depth_at(start)?;
    let mut scan = Scan {
        chunk,
        cfg,
        prefix,
        first: start,
        pcs: Vec::new(),
        branches: 0,
    };
    let root = scan.node(start, 0)?;
    if scan.branches == 0 {
        return None;
    }
    let join = root.join()?;
    if join <= start || cfg.stack_depth_at(join)? != prefix {
        return None;
    }
    Some(Plan {
        root,
        pcs: scan.pcs,
        join,
        prefix_depth: prefix,
    })
}

impl Node {
    fn join(&self) -> Option<usize> {
        match self {
            Self::Store { continuation, .. } => Some(*continuation),
            Self::Branch { yes, no, .. } => {
                let join = yes.join()?;
                (no.join()? == join).then_some(join)
            }
        }
    }
}

struct Scan<'a> {
    chunk: &'a Chunk,
    cfg: &'a Cfg,
    prefix: usize,
    first: usize,
    pcs: Vec<usize>,
    branches: usize,
}

impl Scan<'_> {
    fn visit(&mut self, pc: usize) -> Option<Op> {
        if pc < self.first || pc - self.first >= 64 || self.pcs.len() >= 64 {
            return None;
        }
        self.pcs.push(pc);
        self.chunk.jit_ops().get(pc).copied()
    }

    fn node(&mut self, mut pc: usize, depth: usize) -> Option<Node> {
        if depth >= 4 || self.cfg.stack_depth_at(pc)? != self.prefix {
            return None;
        }
        let mut values = Builder::default();
        loop {
            let op = self.visit(pc)?;
            match op {
                Op::EqEq
                | Op::StrictEq
                | Op::NotEq
                | Op::StrictNotEq
                | Op::Lt
                | Op::Le
                | Op::Gt
                | Op::Ge => {
                    let Op::JumpIfFalse(other) = self.visit(pc + 1)? else {
                        return None;
                    };
                    if other as usize <= pc + 1 {
                        return None;
                    }
                    let values = values.comparison()?;
                    self.branches += 1;
                    let yes = Box::new(self.node(pc + 2, depth + 1)?);
                    let no = Box::new(self.node(other as usize, depth + 1)?);
                    return Some(Node::Branch {
                        values,
                        comparison: op,
                        yes,
                        no,
                    });
                }
                Op::SetPropDrop(name, cache)
                | Op::SetPropThisDrop(name, cache)
                | Op::SetPropLocalDrop(_, name, cache) => {
                    let values = values.store(op)?;
                    let continuation = self.continuation(pc + 1)?;
                    return Some(Node::Store {
                        values,
                        name,
                        cache,
                        continuation,
                    });
                }
                _ => values.step(self.chunk, op)?,
            }
            pc += 1;
        }
    }

    fn continuation(&mut self, mut pc: usize) -> Option<usize> {
        for _ in 0..4 {
            if self.cfg.stack_depth_at(pc)? != self.prefix {
                return None;
            }
            match self.chunk.jit_ops().get(pc)? {
                Op::Jump(target) if *target as usize > pc => {
                    let target = *target as usize;
                    self.visit(pc)?;
                    pc = target;
                }
                Op::Jump(_) => return None,
                _ => return Some(pc),
            }
        }
        None
    }
}
