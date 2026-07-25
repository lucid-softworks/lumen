//! Target-neutral control-flow and stack analysis for optimizing JIT regions.
//!
//! The baseline emitters deliberately remain architecture-specific.  This module owns the
//! shared facts an optimizing tier needs: exact settled stack depths, basic blocks, normal-flow
//! dominance, and natural loops.  Exception handlers are represented as independent roots, not
//! ordinary predecessors -- a catch is entered with the saved try-entry depth plus the thrown
//! value, and pretending `PushHandler` branches to it would make dominance unsound.

// The IR is landing incrementally: CFG/stack analysis and selected region lowerings are live,
// while representation variants and verifier accessors are consumed by the next stages.
#![allow(dead_code)]

use crate::bytecode::{Chunk, Op, UpdKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct BlockId(pub(crate) u32);

impl BlockId {
    #[inline]
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) successors: Vec<BlockId>,
    pub(crate) predecessors: Vec<BlockId>,
    pub(crate) stack_in: Option<usize>,
    pub(crate) stack_out: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct HandlerRoot {
    pub(crate) push_pc: usize,
    pub(crate) target: BlockId,
    pub(crate) stack_depth: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct NaturalLoop {
    pub(crate) header: BlockId,
    pub(crate) latches: Vec<BlockId>,
    /// Sorted block ids.  A bitset is used while discovering the loop, then discarded.
    pub(crate) blocks: Vec<BlockId>,
    pub(crate) exits: Vec<(BlockId, BlockId)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BuildError {
    Empty,
    BadTarget {
        pc: usize,
        target: usize,
    },
    MissingStackEffect {
        pc: usize,
    },
    StackUnderflow {
        pc: usize,
        depth: usize,
        pops: usize,
    },
    StackMismatch {
        pc: usize,
        expected: usize,
        actual: usize,
    },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Empty => write!(f, "empty bytecode"),
            BuildError::BadTarget { pc, target } => {
                write!(f, "pc {pc} has invalid target {target}")
            }
            BuildError::MissingStackEffect { pc } => {
                write!(f, "pc {pc} has no stack-effect description")
            }
            BuildError::StackUnderflow { pc, depth, pops } => {
                write!(f, "pc {pc} pops {pops} values from depth {depth}")
            }
            BuildError::StackMismatch {
                pc,
                expected,
                actual,
            } => write!(
                f,
                "pc {pc} is reached at stack depths {expected} and {actual}"
            ),
        }
    }
}

/// A target-neutral CFG.  The graph is compile-time scratch and is never retained by `JitCode`.
#[derive(Debug)]
pub(crate) struct Cfg {
    blocks: Vec<Block>,
    pc_block: Vec<Option<BlockId>>,
    pc_depth: Vec<Option<usize>>,
    rpo: Vec<BlockId>,
    dominators: Vec<Vec<u64>>,
    loops: Vec<NaturalLoop>,
    handler_roots: Vec<HandlerRoot>,
    max_settled_stack: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ValueId(u32);

impl ValueId {
    #[inline]
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[inline]
fn push_value(values: &mut Vec<ValueData>, def: ValueDef, rep: Rep) -> ValueId {
    let id = ValueId(values.len() as u32);
    values.push(ValueData { def, rep });
    id
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FrameLoc {
    Local(u16),
    Stack(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rep {
    Tagged,
    F64,
    I32,
    Bool,
    Nullish,
    Object,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ValueDef {
    RegionInput(FrameLoc),
    This,
    BlockParam { block: BlockId, loc: FrameLoc },
    OpResult { pc: usize, result: u8 },
    Undefined { pc: usize },
    Empty { pc: usize },
}

#[derive(Clone, Debug)]
pub(crate) struct ValueData {
    pub(crate) def: ValueDef,
    pub(crate) rep: Rep,
}

#[derive(Clone, Debug)]
pub(crate) enum InstKind {
    Generic,
    CheckLocal(u16),
    StoreLocal(u16),
    UpdateLocal(u16, UpdKind),
    ResetSlots { start: u16, count: u16 },
    Clone,
    Dup,
    Dup2,
    Branch,
    InlineGuard,
}

#[derive(Clone, Debug)]
pub(crate) struct Inst {
    pub(crate) pc: usize,
    pub(crate) kind: InstKind,
    pub(crate) inputs: Vec<ValueId>,
    pub(crate) outputs: Vec<ValueId>,
}

#[derive(Clone, Debug)]
pub(crate) struct IrEdge {
    pub(crate) target: BlockId,
    /// Locals followed by operand-stack values in target block-parameter order.
    pub(crate) args: Vec<ValueId>,
}

#[derive(Clone, Debug)]
pub(crate) struct IrBlock {
    pub(crate) cfg_block: BlockId,
    pub(crate) params: Vec<(FrameLoc, ValueId)>,
    pub(crate) insts: Vec<Inst>,
    pub(crate) successors: Vec<IrEdge>,
}

#[derive(Clone, Debug)]
pub(crate) struct SideExit {
    pub(crate) from: BlockId,
    pub(crate) resume_pc: usize,
    pub(crate) locals: Vec<ValueId>,
    pub(crate) stack: Vec<ValueId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IrError {
    NoLoop,
    TooLarge,
    Handler,
    IrreducibleEntry { block: BlockId },
    BadLocal { pc: usize, slot: usize },
    StackUnderflow { pc: usize },
    StackMismatch { block: BlockId },
    MissingBlock(BlockId),
    BadValue(ValueId),
    BadEdgeArgs { from: BlockId, to: BlockId },
}

/// Stack/local SSA for one natural loop.  It is deliberately architecture-neutral and keeps
/// ownership semantics explicit in `Clone`/`Dup` instructions.  Lowerers may borrow an object
/// value while its owning frame location stays live, but side-exit reconstruction must clone
/// every duplicate logical location rather than treating SSA aliasing as Rust ownership.
#[derive(Clone, Debug)]
pub(crate) struct RegionIr {
    pub(crate) header: BlockId,
    pub(crate) blocks: Vec<IrBlock>,
    pub(crate) values: Vec<ValueData>,
    pub(crate) exits: Vec<SideExit>,
    pub(crate) this_value: ValueId,
    pub(crate) n_slots: usize,
}

impl RegionIr {
    pub(crate) fn build_loop(chunk: &Chunk, cfg: &Cfg, head: usize) -> Result<Self, IrError> {
        let lp = cfg.loop_at_header(head).ok_or(IrError::NoLoop)?;
        let (_, n_slots) = chunk.jit_frame();
        Self::build_with(
            chunk.jit_ops(),
            cfg,
            lp,
            n_slots,
            |pc| chunk.jit_stack_effect(pc),
            |target| chunk.jit_inline_target(target).argc as usize,
            |k| {
                if chunk.jit_const_num(k).is_some() {
                    Rep::F64
                } else {
                    Rep::Tagged
                }
            },
        )
    }

    fn build_with(
        ops: &[Op],
        cfg: &Cfg,
        lp: &NaturalLoop,
        n_slots: usize,
        mut effect: impl FnMut(usize) -> Option<(usize, usize)>,
        mut inline_argc: impl FnMut(u32) -> usize,
        mut const_rep: impl FnMut(u32) -> Rep,
    ) -> Result<Self, IrError> {
        if lp.blocks.len() > 128
            || lp
                .blocks
                .iter()
                .map(|id| {
                    let b = &cfg.blocks[id.index()];
                    b.end - b.start
                })
                .sum::<usize>()
                > 1024
        {
            return Err(IrError::TooLarge);
        }
        if ops
            .iter()
            .any(|op| matches!(op, Op::PushHandler(_) | Op::PopHandler))
        {
            // Until exceptional SSA exists, even a syntactically outside try region is kept on
            // the baseline tier.  This is intentionally conservative and easy to relax later.
            return Err(IrError::Handler);
        }

        let mut values = Vec::new();
        let this_value = push_value(&mut values, ValueDef::This, Rep::Tagged);

        let selected = |id: BlockId| lp.blocks.contains(&id);
        for &id in &lp.blocks {
            if id != lp.header
                && cfg.blocks[id.index()]
                    .predecessors
                    .iter()
                    .any(|pred| {
                        cfg.blocks[pred.index()].stack_in.is_some() && !selected(*pred)
                    })
            {
                return Err(IrError::IrreducibleEntry { block: id });
            }
        }

        // Allocate every block parameter before translating any block.  Backedges can therefore
        // refer to header parameters without iterative graph construction.  Trivial parameters
        // are canonicalized after all incoming edge arguments are known.
        let mut block_params: Vec<Option<Vec<(FrameLoc, ValueId)>>> = vec![None; cfg.blocks.len()];
        for &id in &lp.blocks {
            let depth = cfg.blocks[id.index()]
                .stack_in
                .ok_or(IrError::StackMismatch { block: id })?;
            let mut params = Vec::with_capacity(n_slots + depth);
            for slot in 0..n_slots {
                let loc = FrameLoc::Local(slot as u16);
                let def = if id == lp.header {
                    ValueDef::RegionInput(loc)
                } else {
                    ValueDef::BlockParam { block: id, loc }
                };
                params.push((loc, push_value(&mut values, def, Rep::Tagged)));
            }
            for pos in 0..depth {
                let loc = FrameLoc::Stack(pos as u16);
                let def = if id == lp.header {
                    ValueDef::RegionInput(loc)
                } else {
                    ValueDef::BlockParam { block: id, loc }
                };
                params.push((loc, push_value(&mut values, def, Rep::Tagged)));
            }
            block_params[id.index()] = Some(params);
        }

        struct EndState {
            locals: Vec<ValueId>,
            stack: Vec<ValueId>,
        }
        let mut end_states: Vec<Option<EndState>> = (0..cfg.blocks.len()).map(|_| None).collect();
        let mut ir_blocks = Vec::with_capacity(lp.blocks.len());

        // Stable RPO makes dumps deterministic.  Natural-loop membership, not numeric block-id
        // order, decides inclusion (nested blocks can be interleaved in bytecode order).
        let mut order: Vec<BlockId> = cfg.rpo.iter().copied().filter(|id| selected(*id)).collect();
        for &id in &lp.blocks {
            if !order.contains(&id) {
                order.push(id);
            }
        }

        for id in order {
            let params = block_params[id.index()]
                .clone()
                .ok_or(IrError::MissingBlock(id))?;
            let mut locals: Vec<ValueId> = params[..n_slots].iter().map(|(_, id)| *id).collect();
            let mut stack: Vec<ValueId> = params[n_slots..].iter().map(|(_, id)| *id).collect();
            let mut insts = Vec::new();
            let block = &cfg.blocks[id.index()];

            for pc in block.start..block.end {
                let op = &ops[pc];
                let (pops, pushes) = effect(pc).ok_or(IrError::StackUnderflow { pc })?;
                if stack.len() < pops {
                    return Err(IrError::StackUnderflow { pc });
                }

                let local = |slot: u16, locals: &[ValueId]| -> Result<ValueId, IrError> {
                    locals.get(slot as usize).copied().ok_or(IrError::BadLocal {
                        pc,
                        slot: slot as usize,
                    })
                };
                match *op {
                    Op::LoadLocal(slot) => {
                        let input = local(slot, &locals)?;
                        let rep = values[input.index()].rep;
                        let out =
                            push_value(&mut values, ValueDef::OpResult { pc, result: 0 }, rep);
                        insts.push(Inst {
                            pc,
                            kind: InstKind::CheckLocal(slot),
                            inputs: vec![input],
                            outputs: vec![out],
                        });
                        stack.push(out);
                    }
                    Op::StoreLocal(slot) => {
                        let value = stack.pop().ok_or(IrError::StackUnderflow { pc })?;
                        let dst = locals.get_mut(slot as usize).ok_or(IrError::BadLocal {
                            pc,
                            slot: slot as usize,
                        })?;
                        *dst = value;
                        insts.push(Inst {
                            pc,
                            kind: InstKind::StoreLocal(slot),
                            inputs: vec![value],
                            outputs: Vec::new(),
                        });
                    }
                    Op::Tdz(slot) => {
                        let value = push_value(&mut values, ValueDef::Empty { pc }, Rep::Tagged);
                        let dst = locals.get_mut(slot as usize).ok_or(IrError::BadLocal {
                            pc,
                            slot: slot as usize,
                        })?;
                        *dst = value;
                        insts.push(Inst {
                            pc,
                            kind: InstKind::StoreLocal(slot),
                            inputs: Vec::new(),
                            outputs: vec![value],
                        });
                    }
                    Op::ResetSlots(start, count) => {
                        let mut outputs = Vec::with_capacity(count as usize);
                        for slot in start as usize..start as usize + count as usize {
                            let dst = locals.get_mut(slot).ok_or(IrError::BadLocal { pc, slot })?;
                            let value =
                                push_value(&mut values, ValueDef::Undefined { pc }, Rep::Nullish);
                            *dst = value;
                            outputs.push(value);
                        }
                        insts.push(Inst {
                            pc,
                            kind: InstKind::ResetSlots { start, count },
                            inputs: Vec::new(),
                            outputs,
                        });
                    }
                    Op::UpdateLocal(slot, kind) => {
                        let old = local(slot, &locals)?;
                        let old_num = push_value(
                            &mut values,
                            ValueDef::OpResult { pc, result: 0 },
                            Rep::Tagged,
                        );
                        let new_num = push_value(
                            &mut values,
                            ValueDef::OpResult { pc, result: 1 },
                            Rep::Tagged,
                        );
                        locals[slot as usize] = new_num;
                        let mut outputs = vec![old_num, new_num];
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(new_num),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(old_num),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {
                                outputs.remove(0);
                            }
                        }
                        insts.push(Inst {
                            pc,
                            kind: InstKind::UpdateLocal(slot, kind),
                            inputs: vec![old],
                            outputs,
                        });
                    }
                    Op::Dup => {
                        let value = stack.pop().ok_or(IrError::StackUnderflow { pc })?;
                        stack.push(value);
                        stack.push(value);
                        insts.push(Inst {
                            pc,
                            kind: InstKind::Dup,
                            inputs: vec![value],
                            outputs: Vec::new(),
                        });
                    }
                    Op::Dup2 => {
                        let at = stack
                            .len()
                            .checked_sub(2)
                            .ok_or(IrError::StackUnderflow { pc })?;
                        let pair = [stack[at], stack[at + 1]];
                        stack.extend_from_slice(&pair);
                        insts.push(Inst {
                            pc,
                            kind: InstKind::Dup2,
                            inputs: pair.to_vec(),
                            outputs: Vec::new(),
                        });
                    }
                    Op::JumpIfFalsePeek(_)
                    | Op::JumpIfTruePeek(_)
                    | Op::JumpIfNotNullishPeek(_) => {
                        let value = *stack.last().ok_or(IrError::StackUnderflow { pc })?;
                        insts.push(Inst {
                            pc,
                            kind: InstKind::Branch,
                            inputs: vec![value],
                            outputs: Vec::new(),
                        });
                    }
                    Op::DestructureGuard => {
                        let value = *stack.last().ok_or(IrError::StackUnderflow { pc })?;
                        insts.push(Inst {
                            pc,
                            kind: InstKind::Generic,
                            inputs: vec![value],
                            outputs: Vec::new(),
                        });
                    }
                    Op::InlineGuard(target, _) => {
                        let width = inline_argc(target) + 2;
                        let at = stack
                            .len()
                            .checked_sub(width)
                            .ok_or(IrError::StackUnderflow { pc })?;
                        insts.push(Inst {
                            pc,
                            kind: InstKind::InlineGuard,
                            inputs: stack[at..].to_vec(),
                            outputs: Vec::new(),
                        });
                    }
                    Op::LoadThis => {
                        let out = push_value(
                            &mut values,
                            ValueDef::OpResult { pc, result: 0 },
                            Rep::Tagged,
                        );
                        insts.push(Inst {
                            pc,
                            kind: InstKind::Clone,
                            inputs: vec![this_value],
                            outputs: vec![out],
                        });
                        stack.push(out);
                    }
                    _ => {
                        let at = stack
                            .len()
                            .checked_sub(pops)
                            .ok_or(IrError::StackUnderflow { pc })?;
                        let mut inputs = stack[at..].to_vec();
                        stack.truncate(at);
                        match *op {
                            Op::GetPropThis(..) | Op::SetPropThisDrop(..) => {
                                inputs.insert(0, this_value);
                            }
                            Op::GetPropLocal(slot, ..)
                            | Op::SetPropLocalDrop(slot, ..)
                            | Op::GetElemLocal(slot)
                            | Op::SetElemLocal(slot)
                            | Op::SetElemLocalDrop(slot)
                            | Op::ToPropKeyLocal(slot) => {
                                inputs.insert(0, local(slot, &locals)?);
                            }
                            _ => {}
                        }
                        let mut outputs = Vec::with_capacity(pushes);
                        for result in 0..pushes {
                            let rep = match *op {
                                Op::Const(k) => const_rep(k),
                                Op::Undef => Rep::Nullish,
                                Op::Lt
                                | Op::Gt
                                | Op::Le
                                | Op::Ge
                                | Op::EqEq
                                | Op::NotEq
                                | Op::StrictEq
                                | Op::StrictNotEq
                                | Op::Not
                                | Op::InstanceOf(_) => Rep::Bool,
                                _ => Rep::Tagged,
                            };
                            let out = if matches!(op, Op::Undef) {
                                push_value(&mut values, ValueDef::Undefined { pc }, rep)
                            } else {
                                push_value(
                                    &mut values,
                                    ValueDef::OpResult {
                                        pc,
                                        result: result as u8,
                                    },
                                    rep,
                                )
                            };
                            outputs.push(out);
                            stack.push(out);
                        }
                        let kind = if matches!(
                            op,
                            Op::Jump(_)
                                | Op::JumpIfFalse(_)
                                | Op::Return
                                | Op::ReturnUndef
                                | Op::Throw
                        ) {
                            InstKind::Branch
                        } else {
                            InstKind::Generic
                        };
                        insts.push(Inst {
                            pc,
                            kind,
                            inputs,
                            outputs,
                        });
                    }
                }

                let expected = cfg.pc_depth[pc + 1];
                // A control transfer's fallthrough may be unreachable; wherever the next pc is
                // reachable, the simulated stack must agree with the shared verifier.
                if expected.is_some() && !ends_block(op) && expected != Some(stack.len()) {
                    return Err(IrError::StackMismatch { block: id });
                }
            }
            end_states[id.index()] = Some(EndState {
                locals: locals.clone(),
                stack: stack.clone(),
            });
            ir_blocks.push(IrBlock {
                cfg_block: id,
                params,
                insts,
                successors: Vec::new(),
            });
        }

        let mut exits = Vec::new();
        for block in &mut ir_blocks {
            let id = block.cfg_block;
            let state = end_states[id.index()]
                .as_ref()
                .ok_or(IrError::MissingBlock(id))?;
            for &succ in &cfg.blocks[id.index()].successors {
                if selected(succ) {
                    let mut args = state.locals.clone();
                    args.extend_from_slice(&state.stack);
                    block.successors.push(IrEdge { target: succ, args });
                } else {
                    exits.push(SideExit {
                        from: id,
                        resume_pc: cfg.blocks[succ.index()].start,
                        locals: state.locals.clone(),
                        stack: state.stack.clone(),
                    });
                }
            }
        }

        let mut ir = RegionIr {
            header: lp.header,
            blocks: ir_blocks,
            values,
            exits,
            this_value,
            n_slots,
        };
        ir.remove_trivial_params();
        ir.verify(cfg)?;
        Ok(ir)
    }

    fn remove_trivial_params(&mut self) {
        let mut replacement: Vec<ValueId> = (0..self.values.len())
            .map(|idx| ValueId(idx as u32))
            .collect();
        let canonical = |mut id: ValueId, replacement: &[ValueId]| {
            while replacement[id.index()] != id {
                id = replacement[id.index()];
            }
            id
        };

        // Non-header parameters with one effective incoming value are aliases, not phis.  A few
        // rounds handle chains of single-predecessor blocks without retaining redundant nodes.
        for _ in 0..self.blocks.len().max(1) {
            let mut changed = false;
            for block in &self.blocks {
                if block.cfg_block == self.header {
                    continue;
                }
                for (param_idx, &(_, param)) in block.params.iter().enumerate() {
                    let mut incoming = self
                        .blocks
                        .iter()
                        .flat_map(|pred| &pred.successors)
                        .filter(|edge| edge.target == block.cfg_block)
                        .filter_map(|edge| edge.args.get(param_idx).copied())
                        .map(|id| canonical(id, &replacement));
                    let Some(first) = incoming.next() else {
                        continue;
                    };
                    if incoming.all(|id| id == first) && canonical(param, &replacement) != first {
                        replacement[param.index()] = first;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for block in &mut self.blocks {
            for inst in &mut block.insts {
                for id in &mut inst.inputs {
                    *id = canonical(*id, &replacement);
                }
            }
            for edge in &mut block.successors {
                for id in &mut edge.args {
                    *id = canonical(*id, &replacement);
                }
            }
        }
        for exit in &mut self.exits {
            for id in exit.locals.iter_mut().chain(&mut exit.stack) {
                *id = canonical(*id, &replacement);
            }
        }
    }

    pub(crate) fn verify(&self, cfg: &Cfg) -> Result<(), IrError> {
        let block = |id: BlockId| {
            self.blocks
                .iter()
                .find(|block| block.cfg_block == id)
                .ok_or(IrError::MissingBlock(id))
        };
        for b in &self.blocks {
            let expected_params = self.n_slots
                + cfg.blocks[b.cfg_block.index()]
                    .stack_in
                    .ok_or(IrError::StackMismatch { block: b.cfg_block })?;
            if b.params.len() != expected_params {
                return Err(IrError::StackMismatch { block: b.cfg_block });
            }
            for inst in &b.insts {
                for &id in inst.inputs.iter().chain(&inst.outputs) {
                    if id.index() >= self.values.len() {
                        return Err(IrError::BadValue(id));
                    }
                }
            }
            for edge in &b.successors {
                let target = block(edge.target)?;
                if edge.args.len() != target.params.len() {
                    return Err(IrError::BadEdgeArgs {
                        from: b.cfg_block,
                        to: edge.target,
                    });
                }
                if edge.args.iter().any(|id| id.index() >= self.values.len()) {
                    return Err(IrError::BadEdgeArgs {
                        from: b.cfg_block,
                        to: edge.target,
                    });
                }
            }
        }
        for exit in &self.exits {
            if exit.locals.len() != self.n_slots
                || exit
                    .locals
                    .iter()
                    .chain(&exit.stack)
                    .any(|id| id.index() >= self.values.len())
            {
                return Err(IrError::BadValue(
                    exit.locals.first().copied().unwrap_or(self.this_value),
                ));
            }
        }
        Ok(())
    }
}

impl Cfg {
    pub(crate) fn build(chunk: &Chunk) -> Result<Cfg, BuildError> {
        let ops = chunk.jit_ops();
        Self::build_with(ops, |pc| chunk.jit_stack_effect(pc))
    }

    fn build_with(
        ops: &[Op],
        mut effect: impl FnMut(usize) -> Option<(usize, usize)>,
    ) -> Result<Cfg, BuildError> {
        if ops.is_empty() {
            return Err(BuildError::Empty);
        }

        let (pc_depth, max_settled_stack, handler_depths) = analyze_stack(ops, &mut effect)?;

        // Leaders: entry, branch targets, the instruction after a control transfer, and both
        // sides of handler-state changes.  Splitting at Push/PopHandler lets a future region
        // selector reject active-handler ranges without reconstructing lexical bytecode state.
        let mut leader = vec![false; ops.len() + 1];
        leader[0] = true;
        leader[ops.len()] = true;
        for (pc, op) in ops.iter().enumerate() {
            if let Some(target) = jump_target(op) {
                validate_target(ops.len(), pc, target)?;
                leader[target] = true;
            }
            if ends_block(op) || matches!(op, Op::PushHandler(_) | Op::PopHandler) {
                leader[pc + 1] = true;
            }
        }

        let starts: Vec<usize> = leader
            .iter()
            .enumerate()
            .filter_map(|(pc, yes)| (*yes).then_some(pc))
            .collect();
        let mut blocks = Vec::with_capacity(starts.len().saturating_sub(1));
        let mut pc_block = vec![None; ops.len() + 1];
        for pair in starts.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            if start == end || start == ops.len() {
                continue;
            }
            let id = BlockId(blocks.len() as u32);
            for slot in &mut pc_block[start..end] {
                *slot = Some(id);
            }
            blocks.push(Block {
                start,
                end,
                successors: Vec::new(),
                predecessors: Vec::new(),
                stack_in: pc_depth[start],
                stack_out: None,
            });
        }

        for bidx in 0..blocks.len() {
            let end = blocks[bidx].end;
            let last_pc = end - 1;
            let mut succ_pcs = Vec::with_capacity(2);
            normal_successor_pcs(&ops[last_pc], last_pc, ops.len(), &mut succ_pcs);
            for pc in succ_pcs {
                validate_target(ops.len(), last_pc, pc)?;
                if pc == ops.len() {
                    continue;
                }
                let to = pc_block[pc].expect("every in-range leader belongs to a block");
                if !blocks[bidx].successors.contains(&to) {
                    blocks[bidx].successors.push(to);
                }
            }
            if let Some(mut depth) = blocks[bidx].stack_in {
                for pc in blocks[bidx].start..end {
                    let (pops, pushes) = effect(pc).ok_or(BuildError::MissingStackEffect { pc })?;
                    if depth < pops {
                        return Err(BuildError::StackUnderflow { pc, depth, pops });
                    }
                    depth = depth - pops + pushes;
                }
                blocks[bidx].stack_out = Some(depth);
            }
        }

        for from in 0..blocks.len() {
            let succs = blocks[from].successors.clone();
            for to in succs {
                let pred = BlockId(from as u32);
                if !blocks[to.index()].predecessors.contains(&pred) {
                    blocks[to.index()].predecessors.push(pred);
                }
            }
        }

        let handler_roots = handler_depths
            .into_iter()
            .filter_map(|(push_pc, target_pc, stack_depth)| {
                pc_block[target_pc].map(|target| HandlerRoot {
                    push_pc,
                    target,
                    stack_depth,
                })
            })
            .collect::<Vec<_>>();

        let roots = graph_roots(&blocks, &handler_roots, pc_block[0]);
        let rpo = reverse_postorder(&blocks, &roots);
        let dominators = compute_dominators(&blocks, &rpo, &roots);
        let loops = discover_loops(&blocks, &rpo, &dominators);

        Ok(Cfg {
            blocks,
            pc_block,
            pc_depth,
            rpo,
            dominators,
            loops,
            handler_roots,
            max_settled_stack,
        })
    }

    #[inline]
    pub(crate) fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    #[inline]
    pub(crate) fn loops(&self) -> &[NaturalLoop] {
        &self.loops
    }

    #[inline]
    pub(crate) fn handler_roots(&self) -> &[HandlerRoot] {
        &self.handler_roots
    }

    #[inline]
    pub(crate) fn rpo(&self) -> &[BlockId] {
        &self.rpo
    }

    #[inline]
    pub(crate) fn block_at(&self, pc: usize) -> Option<BlockId> {
        self.pc_block.get(pc).copied().flatten()
    }

    #[inline]
    pub(crate) fn stack_depth_at(&self, pc: usize) -> Option<usize> {
        self.pc_depth.get(pc).copied().flatten()
    }

    /// Maximum depth after a bytecode instruction settles.  Machine-code helpers occasionally
    /// need one additional temporary word; callers should allocate [`Cfg::jit_stack_capacity`].
    #[inline]
    pub(crate) fn max_settled_stack(&self) -> usize {
        self.max_settled_stack
    }

    #[inline]
    pub(crate) fn jit_stack_capacity(&self) -> usize {
        self.max_settled_stack + 1
    }

    pub(crate) fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        bit_contains(&self.dominators[b.index()], a.index())
    }

    pub(crate) fn loop_at_header(&self, pc: usize) -> Option<&NaturalLoop> {
        let block = self.block_at(pc)?;
        if self.blocks[block.index()].start != pc {
            return None;
        }
        self.loops.iter().find(|lp| lp.header == block)
    }

    /// Return the unique unconditional backedge pc for the old branch-free loop emitter.
    /// Forward/internal branches deliberately make this return `None`; they belong to the new
    /// region lowering rather than being accidentally accepted by the linear emitter.
    pub(crate) fn linear_loop_latch(&self, ops: &[Op], head: usize) -> Option<usize> {
        let lp = self.loop_at_header(head)?;
        if lp.latches.len() != 1 {
            return None;
        }
        let latch = &self.blocks[lp.latches[0].index()];
        let latch_pc = latch.end.checked_sub(1)?;
        if !matches!(ops.get(latch_pc), Some(Op::Jump(t)) if *t as usize == head) {
            return None;
        }

        let mut covered = vec![false; latch_pc.checked_sub(head)? + 1];
        for &bid in &lp.blocks {
            let b = &self.blocks[bid.index()];
            if b.start < head || b.end > latch_pc + 1 {
                return None;
            }
            if bid != lp.header && b.predecessors.iter().any(|pred| !lp.blocks.contains(pred)) {
                return None; // an external edge needs canonical state at an interior pc
            }
            covered[b.start - head..b.end - head].fill(true);
            for &succ in &b.successors {
                if !lp.blocks.contains(&succ) {
                    continue;
                }
                let target = self.blocks[succ.index()].start;
                let is_fallthrough = target == b.end;
                let is_backedge = bid == lp.latches[0] && succ == lp.header;
                if !is_fallthrough && !is_backedge {
                    return None;
                }
            }
        }
        covered.iter().all(|yes| *yes).then_some(latch_pc)
    }
}

fn validate_target(len: usize, pc: usize, target: usize) -> Result<(), BuildError> {
    if target <= len {
        Ok(())
    } else {
        Err(BuildError::BadTarget { pc, target })
    }
}

pub(crate) fn jump_target(op: &Op) -> Option<usize> {
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfFalsePeek(t)
        | Op::JumpIfTruePeek(t)
        | Op::JumpIfNotNullishPeek(t)
        | Op::InlineGuard(_, t)
        | Op::PushHandler(t) => Some(*t as usize),
        _ => None,
    }
}

fn ends_block(op: &Op) -> bool {
    matches!(
        op,
        Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::JumpIfFalsePeek(_)
            | Op::JumpIfTruePeek(_)
            | Op::JumpIfNotNullishPeek(_)
            | Op::InlineGuard(..)
            | Op::Return
            | Op::ReturnUndef
            | Op::Throw
            | Op::IterAbortL(_)
            | Op::Await
    )
}

fn normal_successor_pcs(op: &Op, pc: usize, len: usize, out: &mut Vec<usize>) {
    match op {
        Op::Jump(t) => out.push(*t as usize),
        Op::JumpIfFalse(t)
        | Op::JumpIfFalsePeek(t)
        | Op::JumpIfTruePeek(t)
        | Op::JumpIfNotNullishPeek(t)
        | Op::InlineGuard(_, t) => {
            out.push(*t as usize);
            if pc + 1 <= len {
                out.push(pc + 1);
            }
        }
        Op::Return | Op::ReturnUndef | Op::Throw | Op::IterAbortL(_) | Op::Await => {}
        _ if pc + 1 <= len => out.push(pc + 1),
        _ => {}
    }
}

type HandlerDepth = (usize, usize, usize); // push pc, target pc, catch-entry depth

fn analyze_stack(
    ops: &[Op],
    effect: &mut impl FnMut(usize) -> Option<(usize, usize)>,
) -> Result<(Vec<Option<usize>>, usize, Vec<HandlerDepth>), BuildError> {
    let mut depth = vec![None; ops.len() + 1];
    let mut work = vec![(0usize, 0usize)];
    let mut max = 0usize;
    let mut handlers = Vec::new();
    while let Some((pc, incoming)) = work.pop() {
        validate_target(ops.len(), pc.min(ops.len()), pc)?;
        match depth[pc] {
            Some(previous) if previous == incoming => continue,
            Some(previous) => {
                return Err(BuildError::StackMismatch {
                    pc,
                    expected: previous,
                    actual: incoming,
                });
            }
            None => depth[pc] = Some(incoming),
        }
        max = max.max(incoming);
        if pc == ops.len() {
            continue;
        }
        let (pops, pushes) = effect(pc).ok_or(BuildError::MissingStackEffect { pc })?;
        if incoming < pops {
            return Err(BuildError::StackUnderflow {
                pc,
                depth: incoming,
                pops,
            });
        }
        let next = incoming - pops + pushes;
        max = max.max(next);
        match &ops[pc] {
            Op::Jump(t) => {
                let target = *t as usize;
                validate_target(ops.len(), pc, target)?;
                work.push((target, next));
            }
            Op::JumpIfFalse(t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t)
            | Op::InlineGuard(_, t) => {
                let target = *t as usize;
                validate_target(ops.len(), pc, target)?;
                work.push((target, next));
                work.push((pc + 1, next));
            }
            Op::Return | Op::ReturnUndef | Op::Throw | Op::IterAbortL(_) | Op::Await => {}
            Op::PushHandler(t) => {
                let target = *t as usize;
                validate_target(ops.len(), pc, target)?;
                let catch_depth = incoming + 1;
                max = max.max(catch_depth);
                handlers.push((pc, target, catch_depth));
                work.push((target, catch_depth));
                work.push((pc + 1, next));
            }
            _ => work.push((pc + 1, next)),
        }
    }
    Ok((depth, max, handlers))
}

fn graph_roots(blocks: &[Block], handlers: &[HandlerRoot], entry: Option<BlockId>) -> Vec<BlockId> {
    let mut roots = Vec::new();
    if let Some(entry) = entry {
        roots.push(entry);
    }
    for h in handlers {
        if !roots.contains(&h.target) {
            roots.push(h.target);
        }
    }
    // Defensive: reachable orphan blocks can arise after an abrupt predecessor.  Treat them as
    // roots rather than inventing dominance through unreachable code.
    for (idx, b) in blocks.iter().enumerate() {
        let id = BlockId(idx as u32);
        if b.stack_in.is_some() && b.predecessors.is_empty() && !roots.contains(&id) {
            roots.push(id);
        }
    }
    roots
}

fn reverse_postorder(blocks: &[Block], roots: &[BlockId]) -> Vec<BlockId> {
    fn visit(id: BlockId, blocks: &[Block], seen: &mut [bool], post: &mut Vec<BlockId>) {
        if std::mem::replace(&mut seen[id.index()], true) {
            return;
        }
        for &succ in &blocks[id.index()].successors {
            if blocks[succ.index()].stack_in.is_some() {
                visit(succ, blocks, seen, post);
            }
        }
        post.push(id);
    }

    let mut seen = vec![false; blocks.len()];
    let mut post = Vec::new();
    for &root in roots {
        visit(root, blocks, &mut seen, &mut post);
    }
    post.reverse();
    post
}

fn bit_words(n: usize) -> usize {
    n.div_ceil(64)
}

fn bit_set(bits: &mut [u64], bit: usize) {
    bits[bit / 64] |= 1u64 << (bit % 64);
}

fn bit_contains(bits: &[u64], bit: usize) -> bool {
    bits.get(bit / 64)
        .is_some_and(|word| word & (1u64 << (bit % 64)) != 0)
}

fn compute_dominators(blocks: &[Block], rpo: &[BlockId], roots: &[BlockId]) -> Vec<Vec<u64>> {
    let words = bit_words(blocks.len());
    let mut all = vec![0u64; words];
    for &id in rpo {
        bit_set(&mut all, id.index());
    }
    let mut dom = vec![vec![0u64; words]; blocks.len()];
    for &id in rpo {
        if roots.contains(&id) {
            bit_set(&mut dom[id.index()], id.index());
        } else {
            dom[id.index()].clone_from(&all);
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for &id in rpo {
            if roots.contains(&id) {
                continue;
            }
            let preds: Vec<BlockId> = blocks[id.index()]
                .predecessors
                .iter()
                .copied()
                .filter(|p| blocks[p.index()].stack_in.is_some())
                .collect();
            let mut next = if let Some(first) = preds.first() {
                dom[first.index()].clone()
            } else {
                vec![0u64; words]
            };
            for pred in &preds[1..] {
                for (dst, src) in next.iter_mut().zip(&dom[pred.index()]) {
                    *dst &= *src;
                }
            }
            bit_set(&mut next, id.index());
            if next != dom[id.index()] {
                dom[id.index()] = next;
                changed = true;
            }
        }
    }
    dom
}

fn discover_loops(blocks: &[Block], rpo: &[BlockId], dom: &[Vec<u64>]) -> Vec<NaturalLoop> {
    let mut loops: Vec<NaturalLoop> = Vec::new();
    for &tail in rpo {
        for &header in &blocks[tail.index()].successors {
            if !bit_contains(&dom[tail.index()], header.index()) {
                continue;
            }
            let pos = loops.iter().position(|lp| lp.header == header);
            let idx = match pos {
                Some(idx) => idx,
                None => {
                    loops.push(NaturalLoop {
                        header,
                        latches: Vec::new(),
                        blocks: Vec::new(),
                        exits: Vec::new(),
                    });
                    loops.len() - 1
                }
            };
            if !loops[idx].latches.contains(&tail) {
                loops[idx].latches.push(tail);
            }
        }
    }

    for lp in &mut loops {
        let mut members = vec![false; blocks.len()];
        members[lp.header.index()] = true;
        let mut work = lp.latches.clone();
        while let Some(id) = work.pop() {
            if std::mem::replace(&mut members[id.index()], true) {
                continue;
            }
            for &pred in &blocks[id.index()].predecessors {
                if blocks[pred.index()].stack_in.is_some()
                    && bit_contains(&dom[pred.index()], lp.header.index())
                    && !members[pred.index()]
                {
                    work.push(pred);
                }
            }
        }
        lp.blocks = members
            .iter()
            .enumerate()
            .filter_map(|(idx, yes)| yes.then_some(BlockId(idx as u32)))
            .collect();
        for &from in &lp.blocks {
            for &to in &blocks[from.index()].successors {
                if !members[to.index()] && !lp.exits.contains(&(from, to)) {
                    lp.exits.push((from, to));
                }
            }
        }
        lp.latches.sort_unstable();
    }
    loops.sort_by_key(|lp| blocks[lp.header.index()].start);
    loops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(op: &Op) -> Option<(usize, usize)> {
        Some(match op {
            Op::Const(_) | Op::Undef | Op::LoadLocal(_) => (0, 1),
            Op::Dup => (1, 2),
            Op::Dup2 => (2, 4),
            Op::StoreLocal(_) | Op::Pop | Op::Return | Op::Throw => (1, 0),
            Op::Lt | Op::Add => (2, 1),
            Op::Jump(_) | Op::InlineGuard(..) | Op::PushHandler(_) | Op::PopHandler => (0, 0),
            Op::ResetSlots(..) | Op::Tdz(_) => (0, 0),
            Op::JumpIfFalse(_) => (1, 0),
            Op::JumpIfFalsePeek(_) | Op::JumpIfTruePeek(_) | Op::JumpIfNotNullishPeek(_) => (1, 1),
            Op::ReturnUndef => (0, 0),
            _ => return None,
        })
    }

    fn cfg(ops: &[Op]) -> Result<Cfg, BuildError> {
        Cfg::build_with(ops, |pc| effect(&ops[pc]))
    }

    #[test]
    fn cfg_straight_line_and_capacity_headroom() {
        let g = cfg(&[
            Op::Const(0),
            Op::Const(1),
            Op::Add,
            Op::Pop,
            Op::ReturnUndef,
        ])
        .unwrap();
        assert_eq!(g.blocks.len(), 1);
        assert_eq!(g.max_settled_stack(), 2);
        assert_eq!(g.jit_stack_capacity(), 3);
        assert_eq!(g.stack_depth_at(3), Some(1));
    }

    #[test]
    fn cfg_diamond_has_precise_edges_and_dominance() {
        let ops = [
            Op::Undef,
            Op::JumpIfFalse(4),
            Op::Const(0),
            Op::Jump(5),
            Op::Const(1),
            Op::Pop,
            Op::ReturnUndef,
        ];
        let g = cfg(&ops).unwrap();
        let entry = g.block_at(0).unwrap();
        let yes = g.block_at(2).unwrap();
        let no = g.block_at(4).unwrap();
        let join = g.block_at(5).unwrap();
        assert_eq!(g.blocks[entry.index()].successors.len(), 2);
        assert!(g.blocks[entry.index()].successors.contains(&yes));
        assert!(g.blocks[entry.index()].successors.contains(&no));
        assert_eq!(g.blocks[join.index()].predecessors.len(), 2);
        assert!(g.dominates(entry, join));
        assert!(!g.dominates(yes, join));
        assert!(!g.dominates(no, join));
    }

    #[test]
    fn cfg_loop_finds_backedge_and_natural_loop() {
        let ops = [
            Op::LoadLocal(0),
            Op::JumpIfFalse(6),
            Op::Const(0),
            Op::StoreLocal(0),
            Op::Jump(0),
            Op::ReturnUndef, // unreachable and deliberately outside the natural loop
            Op::ReturnUndef,
        ];
        let g = cfg(&ops).unwrap();
        let lp = g.loop_at_header(0).expect("loop header");
        assert_eq!(lp.latches.len(), 1);
        assert_eq!(g.blocks[lp.header.index()].start, 0);
        assert_eq!(g.linear_loop_latch(&ops, 0), Some(4));
        assert_eq!(lp.exits.len(), 1);
    }

    #[test]
    fn cfg_internal_diamond_is_a_region_not_a_linear_loop() {
        let ops = [
            Op::LoadLocal(0),
            Op::JumpIfFalse(8),
            Op::LoadLocal(1),
            Op::JumpIfFalse(6),
            Op::Const(0),
            Op::StoreLocal(2),
            Op::Const(1),
            Op::StoreLocal(2),
            Op::Jump(0),
        ];
        let g = cfg(&ops).unwrap();
        assert!(g.loop_at_header(0).is_some());
        assert_eq!(g.linear_loop_latch(&ops, 0), None);
    }

    #[test]
    fn cfg_rejects_invalid_target_underflow_and_mismatched_join() {
        assert_eq!(
            cfg(&[Op::Jump(2)]).unwrap_err(),
            BuildError::BadTarget { pc: 0, target: 2 }
        );
        assert_eq!(
            cfg(&[Op::Pop, Op::ReturnUndef]).unwrap_err(),
            BuildError::StackUnderflow {
                pc: 0,
                depth: 0,
                pops: 1
            }
        );
        let mismatch = [
            Op::Undef,
            Op::JumpIfFalse(4),
            Op::Undef,
            Op::Jump(5),
            Op::Jump(5),
            Op::ReturnUndef,
        ];
        assert!(matches!(
            cfg(&mismatch),
            Err(BuildError::StackMismatch { pc: 5, .. })
        ));
    }

    #[test]
    fn cfg_unreachable_underflow_does_not_poison_graph() {
        let g = cfg(&[Op::Jump(2), Op::Pop, Op::ReturnUndef]).unwrap();
        assert_eq!(g.stack_depth_at(1), None);
        assert_eq!(g.stack_depth_at(2), Some(0));
    }

    #[test]
    fn cfg_peek_branch_preserves_condition_on_both_edges() {
        let g = cfg(&[
            Op::Undef,
            Op::JumpIfFalsePeek(3),
            Op::Jump(3),
            Op::Pop,
            Op::ReturnUndef,
        ])
        .unwrap();
        assert_eq!(g.stack_depth_at(2), Some(1));
        assert_eq!(g.stack_depth_at(3), Some(1));
    }

    #[test]
    fn cfg_inline_guard_retains_the_operand_window() {
        let g = cfg(&[
            Op::Undef,
            Op::Undef,
            Op::InlineGuard(0, 4),
            Op::Jump(4),
            Op::Pop,
            Op::Pop,
            Op::ReturnUndef,
        ])
        .unwrap();
        assert_eq!(g.stack_depth_at(3), Some(2));
        assert_eq!(g.stack_depth_at(4), Some(2));
    }

    #[test]
    fn cfg_catch_is_an_independent_root_with_exception_depth() {
        let ops = [
            Op::PushHandler(4),
            Op::Undef,
            Op::Pop,
            Op::ReturnUndef,
            Op::Pop,
            Op::ReturnUndef,
        ];
        let g = cfg(&ops).unwrap();
        assert_eq!(g.handler_roots.len(), 1);
        let h = &g.handler_roots[0];
        assert_eq!(h.push_pc, 0);
        assert_eq!(h.stack_depth, 1);
        assert_eq!(g.blocks[h.target.index()].start, 4);
        let entry = g.block_at(0).unwrap();
        assert!(!g.dominates(entry, h.target));
    }

    fn region<'a>(ops: &'a [Op], head: usize, n_slots: usize) -> (Cfg, RegionIr) {
        let g = cfg(ops).unwrap();
        let lp = g.loop_at_header(head).unwrap();
        let ir = RegionIr::build_with(
            ops,
            &g,
            lp,
            n_slots,
            |pc| effect(&ops[pc]),
            |_| 0,
            |_| Rep::F64,
        )
        .unwrap();
        (g, ir)
    }

    #[test]
    fn ssa_loop_has_header_inputs_and_backedge_values() {
        let ops = [
            Op::Const(0),
            Op::StoreLocal(1),
            Op::LoadLocal(1),
            Op::LoadLocal(0),
            Op::Lt,
            Op::JumpIfFalse(11),
            Op::LoadLocal(1),
            Op::Const(1),
            Op::Add,
            Op::StoreLocal(1),
            Op::Jump(2),
            Op::LoadLocal(1),
            Op::Return,
        ];
        let (_g, ir) = region(&ops, 2, 2);
        let header = ir.blocks.iter().find(|b| b.cfg_block == ir.header).unwrap();
        let (_, local1) = header
            .params
            .iter()
            .find(|(loc, _)| *loc == FrameLoc::Local(1))
            .unwrap();
        assert!(matches!(
            ir.values[local1.index()].def,
            ValueDef::RegionInput(FrameLoc::Local(1))
        ));
        let backedge = ir
            .blocks
            .iter()
            .flat_map(|b| &b.successors)
            .find(|edge| edge.target == ir.header)
            .unwrap();
        assert_ne!(backedge.args[1], *local1);
        assert!(matches!(
            ir.values[backedge.args[1].index()].def,
            ValueDef::OpResult { pc: 8, .. }
        ));
        let exit = ir.exits.iter().find(|exit| exit.resume_pc == 11).unwrap();
        assert!(exit.stack.is_empty(), "JumpIfFalse must snapshot post-pop");
    }

    #[test]
    fn ssa_loop_ignores_unreachable_predecessor_into_join() {
        // Compiler padding after an unconditional jump can still have a syntactic edge into a
        // live loop block. It has no settled stack depth and therefore is not a region entry.
        let ops = [
            Op::LoadLocal(0),
            Op::JumpIfFalse(8),
            Op::Jump(5),
            Op::Undef,
            Op::Jump(5),
            Op::LoadLocal(1),
            Op::Pop,
            Op::Jump(0),
            Op::ReturnUndef,
        ];
        let (g, ir) = region(&ops, 0, 2);
        assert_eq!(g.stack_depth_at(3), None);
        assert!(ir.blocks.iter().any(|b| g.blocks[b.cfg_block.index()].start == 5));
    }

    #[test]
    fn ssa_diamond_keeps_a_real_local_phi() {
        let ops = [
            Op::LoadLocal(0),
            Op::JumpIfFalse(10),
            Op::LoadLocal(1),
            Op::JumpIfFalse(7),
            Op::Const(0),
            Op::StoreLocal(2),
            Op::Jump(9),
            Op::Const(1),
            Op::StoreLocal(2),
            Op::Jump(0),
            Op::ReturnUndef,
        ];
        let (_g, ir) = region(&ops, 0, 3);
        let join = ir
            .blocks
            .iter()
            .find(|block| {
                block
                    .insts
                    .iter()
                    .any(|inst| inst.pc == 9 && matches!(inst.kind, InstKind::Branch))
            })
            .unwrap();
        let (_, phi) = join
            .params
            .iter()
            .find(|(loc, _)| *loc == FrameLoc::Local(2))
            .unwrap();
        assert!(matches!(
            ir.values[phi.index()].def,
            ValueDef::BlockParam {
                loc: FrameLoc::Local(2),
                ..
            }
        ));
        let incoming: Vec<ValueId> = ir
            .blocks
            .iter()
            .flat_map(|block| &block.successors)
            .filter(|edge| edge.target == join.cfg_block)
            .map(|edge| edge.args[2])
            .collect();
        assert_eq!(incoming.len(), 2);
        assert_ne!(incoming[0], incoming[1]);
    }

    #[test]
    fn ssa_dup_aliases_but_records_ownership_operation() {
        let ops = [Op::LoadLocal(0), Op::Dup, Op::Pop, Op::Pop, Op::Jump(0)];
        let (_g, ir) = region(&ops, 0, 1);
        let dup = ir
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .find(|inst| matches!(inst.kind, InstKind::Dup))
            .unwrap();
        assert_eq!(dup.inputs.len(), 1);
        assert!(dup.outputs.is_empty());
    }

    #[test]
    fn ssa_reset_slots_redefines_only_requested_range() {
        let ops = [Op::ResetSlots(1, 2), Op::Jump(0)];
        let (_g, ir) = region(&ops, 0, 4);
        let backedge = ir.blocks[0]
            .successors
            .iter()
            .find(|edge| edge.target == ir.header)
            .unwrap();
        assert!(matches!(
            ir.values[backedge.args[1].index()].def,
            ValueDef::Undefined { pc: 0 }
        ));
        assert!(matches!(
            ir.values[backedge.args[2].index()].def,
            ValueDef::Undefined { pc: 0 }
        ));
        assert!(matches!(
            ir.values[backedge.args[0].index()].def,
            ValueDef::RegionInput(FrameLoc::Local(0))
        ));
        assert!(matches!(
            ir.values[backedge.args[3].index()].def,
            ValueDef::RegionInput(FrameLoc::Local(3))
        ));
    }
}
