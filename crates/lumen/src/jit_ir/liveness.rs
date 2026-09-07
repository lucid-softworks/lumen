//! Backward local liveness over the shared CFG. A dead local load can transfer ownership
//! to the operand stack; no runtime type feedback or speculative representation is needed.
use super::Cfg;
use crate::bytecode::{Chunk, Op};

pub(crate) struct LastUses(Vec<bool>);

impl LastUses {
    pub(crate) fn build(chunk: &Chunk, cfg: &Cfg) -> Self {
        let (_, slots) = chunk.jit_frame();
        // Captured/aliased frames and exception edges need a separate escape/handler model.
        // Bound compile-time memory and fixed-point work for large generated functions.
        if !chunk.jit_no_activation()
            || slots > 128
            || chunk.jit_ops().len() > 4096
            || !cfg.handler_roots().is_empty()
            || std::env::var_os("LUMEN_JIT_NO_LAST_USES").is_some()
        {
            return Self(Vec::new());
        }
        Self::analyze(chunk.jit_ops(), cfg)
    }

    pub(crate) fn contains(&self, pc: usize) -> bool {
        self.0.get(pc).copied().unwrap_or(false)
    }

    fn analyze(ops: &[Op], cfg: &Cfg) -> Self {
        let blocks = cfg.blocks();
        let mut inputs = vec![0u128; blocks.len()];
        let mut outputs = inputs.clone();
        let effects: Vec<_> = ops.iter().map(effect).collect();
        let summaries: Vec<_> = blocks
            .iter()
            .map(|block| {
                let (mut reads, mut writes) = (0, 0);
                for &(uses, defs) in &effects[block.start..block.end] {
                    reads |= uses & !writes;
                    writes |= defs;
                }
                (reads, writes)
            })
            .collect();
        // Monotone union reaches a fixed point, including loop-carried uses and diamonds.
        // A budget exhaustion safely declines the optimization instead of using partial facts.
        for _ in 0..256 {
            let mut changed = false;
            for id in cfg.rpo().iter().rev() {
                let k = id.0 as usize;
                let out = blocks[k]
                    .successors
                    .iter()
                    .fold(0, |bits, next| bits | inputs[next.0 as usize]);
                let (reads, writes) = summaries[k];
                let input = reads | (out & !writes);
                changed |= inputs[k] != input;
                inputs[k] = input;
                outputs[k] = out;
            }
            if !changed {
                let mut last = vec![false; ops.len()];
                for (k, block) in blocks.iter().enumerate() {
                    let mut live = outputs[k];
                    for pc in (block.start..block.end).rev() {
                        if let Op::LoadLocal(slot) = ops[pc] {
                            last[pc] = live & bit(slot) == 0;
                        }
                        let (reads, writes) = effects[pc];
                        live = reads | (live & !writes);
                    }
                }
                return Self(last);
            }
        }
        Self(Vec::new())
    }
}

fn bit(slot: u16) -> u128 {
    1u128.checked_shl(slot as u32).unwrap_or(u128::MAX)
}

/// Read/write sets concern frame slots, not heap effects. Calls may mutate objects but cannot
/// inspect uncaptured slots. Unknown opcodes conservatively keep every slot live.
fn effect(op: &Op) -> (u128, u128) {
    use Op::*;
    match *op {
        LoadLocal(s)
        | GetPropLocal(s, ..)
        | SetPropLocalDrop(s, ..)
        | GetElemLocal(s)
        | SetElemLocal(s)
        | SetElemLocalDrop(s)
        | ToPropKeyLocal(s)
        | IterCloseL(s)
        | IterAbortL(s) => (bit(s), 0),
        IterStepL(a, b) => (bit(a) | bit(b), 0),
        UpdateLocal(s, _) => (bit(s), bit(s)),
        StoreLocal(s) | Tdz(s) => (0, bit(s)),
        ResetSlots(start, count) => (
            0,
            (start..start.saturating_add(count)).fold(0, |v, s| v | bit(s)),
        ),
        Const(_)
        | Undef
        | Dup
        | Dup2
        | Pop
        | LoadThis
        | LoadName(..)
        | LoadNameForCall(..)
        | StoreName(_)
        | StoreNameCached(..)
        | UpdateName(..)
        | UpdateNameCached(..)
        | GetProp(..)
        | GetPropThis(..)
        | SetProp(..)
        | SetPropDrop(..)
        | SetPropThisDrop(..)
        | ToStr
        | GetIter
        | DestructureGuard
        | DestructureArr(_)
        | DeleteProp(..)
        | DeleteElem(_)
        | CallSpread(_)
        | CallSpreadThis(_)
        | AppendProp(..)
        | GetElem
        | SetElem
        | SetElemDrop
        | UpdateProp(..)
        | UpdateElem(_)
        | ToPropKey
        | GetMethod(..)
        | GetMethodElem
        | Jump(_)
        | JumpIfFalse(_)
        | JumpIfFalsePeek(_)
        | JumpIfTruePeek(_)
        | JumpIfNotNullishPeek(_)
        | Call(..)
        | CallWithThis(..)
        | InlineGuard(..)
        | New(..)
        | MakeRegExp(..)
        | MakeArray(_)
        | MakeObject(..)
        | Throw
        | Return
        | ReturnUndef => (0, 0),
        _ if stack_operator(op) => (0, 0),
        _ => (u128::MAX, 0),
    }
}

// Operators consume operands, even when coercion re-enters JavaScript; escaped bindings
// are excluded by the frame gate above.
fn stack_operator(op: &Op) -> bool {
    use Op::*;
    matches!(
        *op,
        Add | Sub
            | Mul
            | Div
            | Mod
            | BitAnd
            | BitOr
            | BitXor
            | Shl
            | Shr
            | UShr
            | Lt
            | Gt
            | Le
            | Ge
            | EqEq
            | NotEq
            | StrictEq
            | StrictNotEq
            | InstanceOf(_)
            | GenBin(_)
            | Neg
            | Plus
            | Not
            | BitNot
            | Typeof
            | TypeofName(_)
            | Void
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(ops: &[Op]) -> LastUses {
        // These fixtures only need exact depths for loads, stores, branches and returns.
        let cfg = Cfg::build_with(ops, |pc| {
            Some(match ops[pc] {
                Op::LoadLocal(_) | Op::Undef => (0, 1),
                Op::StoreLocal(_) | Op::Pop | Op::JumpIfFalse(_) | Op::Return => (1, 0),
                _ => (0, 0),
            })
        })
        .unwrap();
        LastUses::analyze(ops, &cfg)
    }

    #[test]
    fn only_final_read_moves() {
        let uses = last(&[Op::LoadLocal(0), Op::Pop, Op::LoadLocal(0), Op::Return]);
        assert!(!uses.contains(0));
        assert!(uses.contains(2));
    }

    #[test]
    fn overwrite_ends_previous_value_lifetime() {
        let uses = last(&[
            Op::LoadLocal(0),
            Op::Pop,
            Op::Undef,
            Op::StoreLocal(0),
            Op::LoadLocal(0),
            Op::Return,
        ]);
        assert!(uses.contains(0));
        assert!(uses.contains(4));
    }

    #[test]
    fn successor_use_keeps_value_alive_on_both_paths() {
        let uses = last(&[
            Op::LoadLocal(0),
            Op::JumpIfFalse(4),
            Op::LoadLocal(0),
            Op::Return,
            Op::Undef,
            Op::Return,
        ]);
        assert!(!uses.contains(0));
        assert!(uses.contains(2));
    }

    #[test]
    fn backedge_keeps_loop_carried_value_alive() {
        let uses = last(&[
            Op::LoadLocal(0),
            Op::Pop,
            Op::LoadLocal(1),
            Op::JumpIfFalse(5),
            Op::Jump(0),
            Op::LoadLocal(0),
            Op::Return,
        ]);
        assert!(!uses.contains(0));
        assert!(!uses.contains(2));
        assert!(uses.contains(5));
    }

    #[test]
    fn implicit_slot_reads_and_unknown_ops_are_barriers() {
        assert_eq!(effect(&Op::GetPropLocal(3, 0, 0)), (8, 0));
        assert_eq!(effect(&Op::IterStepL(2, 4)), (20, 0));
        assert_eq!(effect(&Op::MakeClosure(0, 0)), (u128::MAX, 0));
        assert_eq!(effect(&Op::ResetSlots(2, 3)), (0, 28));
    }
}
