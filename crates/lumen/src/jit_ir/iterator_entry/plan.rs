//! Symbolic entry stack and deferred-effect planning.
use super::{Binary, Branch, Candidate, DeferredStore, Expr, Literal, Obligation, Reject};
use crate::bytecode::Op;
struct Builder {
    plan: Candidate,
    stack: Vec<usize>,
}

impl Builder {
    fn push(&mut self, expr: Expr) -> usize {
        let id = self.plan.expressions.len();
        self.plan.expressions.push(expr);
        self.stack.push(id);
        id
    }
    fn pop(&mut self, pc: usize) -> Result<usize, Reject> {
        self.stack.pop().ok_or(Reject {
            pc,
            reason: "stack underflow",
        })
    }
    fn own(&mut self, receiver: usize, name: String, cache: u32) {
        let value = self.push(Expr::Own {
            receiver,
            name,
            cache,
        });
        self.plan.obligations.push(Obligation::OwnData { value });
        if let Some(store) = &self.plan.store {
            self.plan
                .obligations
                .push(Obligation::ForwardStoredEntryOrProveDisjoint {
                    read: value,
                    store_pc: store.pc,
                });
        }
    }
    fn binary(&mut self, pc: usize, op: Binary) -> Result<(), Reject> {
        let right = self.pop(pc)?;
        let left = self.pop(pc)?;
        let value = self.push(Expr::Binary { op, left, right });
        self.plan
            .obligations
            .push(Obligation::BinaryNumericOperands { expression: value });
        Ok(())
    }
}

pub(super) fn analyze_ops(
    ops: &[Op],
    name: &dyn Fn(u32) -> Option<String>,
    literal: &dyn Fn(u32) -> Option<Literal>,
) -> Result<Candidate, Reject> {
    let mut b = initial_builder();
    for (pc, op) in ops.iter().take(64).enumerate() {
        let reject = |reason| Reject { pc, reason };
        if read_step(&mut b, pc, *op, name, literal)? {
            if b.stack.len() > 16 {
                return Err(reject("operand limit"));
            }
            continue;
        }
        match *op {
            Op::JumpIfFalse(target) | Op::JumpIfFalsePeek(target) => {
                if target as usize <= pc || target as usize >= ops.len() {
                    return Err(reject("backedge or invalid branch"));
                }
                let retained = matches!(op, Op::JumpIfFalsePeek(_));
                let condition = if retained {
                    *b.stack.last().ok_or(reject("stack underflow"))?
                } else {
                    b.pop(pc)?
                };
                b.plan.branches.push(Branch {
                    pc,
                    condition,
                    required_truthy: true,
                    cold_target: target as usize,
                    retains_condition: retained,
                });
                b.plan
                    .obligations
                    .push(Obligation::Truthiness { value: condition });
            }
            Op::SetPropDrop(n, cache) => {
                if b.plan.store.is_some() {
                    return Err(reject("multiple stores"));
                }
                let value = b.pop(pc)?;
                let receiver = b.pop(pc)?;
                b.plan.store = Some(DeferredStore {
                    pc,
                    receiver,
                    name: name(n).ok_or(reject("missing store name"))?,
                    cache,
                    value,
                });
                b.plan
                    .obligations
                    .push(Obligation::WritableOrdinaryNonIndexOldNumber { store_pc: pc });
                b.plan.obligations.push(Obligation::NumberValue { value });
            }
            Op::MakeObject(start, count, _) => {
                return finish(b, pc, ops, start, count, name);
            }
            _ => return Err(reject("unsupported entry operation")),
        }
        if b.stack.len() > 16 {
            return Err(reject("operand limit"));
        }
    }
    Err(Reject {
        pc: ops.len().min(64),
        reason: "no bounded result return",
    })
}

fn read_step(
    b: &mut Builder,
    pc: usize,
    op: Op,
    name: &dyn Fn(u32) -> Option<String>,
    literal: &dyn Fn(u32) -> Option<Literal>,
) -> Result<bool, Reject> {
    let reject = |reason| Reject { pc, reason };
    match op {
        Op::Const(n) => {
            b.push(Expr::Constant(
                literal(n).ok_or(reject("unsupported literal"))?,
            ));
        }
        Op::Undef => {
            b.push(Expr::Constant(Literal::Undefined));
        }
        Op::LoadName(n, cache) => {
            let value = b.push(Expr::Name {
                name: name(n).ok_or(reject("missing name"))?,
                cache,
            });
            b.plan
                .obligations
                .push(Obligation::NameResolution { value });
        }
        Op::LoadThis => {
            b.push(Expr::This);
        }
        Op::Tdz(slot) => b.plan.tdz_locals.push(slot),
        Op::Dup => {
            let value = *b.stack.last().ok_or(reject("stack underflow"))?;
            b.stack.push(value);
        }
        Op::Pop => {
            b.pop(pc)?;
        }
        Op::GetProp(n, cache) => {
            let receiver = b.pop(pc)?;
            b.own(receiver, name(n).ok_or(reject("missing property"))?, cache);
        }
        Op::GetPropThis(n, cache) => {
            let receiver = b.push(Expr::This);
            b.pop(pc)?;
            b.own(receiver, name(n).ok_or(reject("missing property"))?, cache);
        }
        Op::GetElem => {
            let index = b.pop(pc)?;
            let receiver = b.pop(pc)?;
            let read = b.push(Expr::Dense { receiver, index });
            b.plan
                .obligations
                .push(Obligation::DenseArrayDisjointFromStore {
                    read,
                    store_pc: b.plan.store.as_ref().map(|s| s.pc),
                });
        }
        Op::Add => b.binary(pc, Binary::Add)?,
        Op::Sub => b.binary(pc, Binary::Sub)?,
        Op::Mul => b.binary(pc, Binary::Mul)?,
        Op::Div => b.binary(pc, Binary::Div)?,
        Op::Lt => b.binary(pc, Binary::Lt)?,
        Op::Gt => b.binary(pc, Binary::Gt)?,
        Op::Le => b.binary(pc, Binary::Le)?,
        Op::Ge => b.binary(pc, Binary::Ge)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn finish(
    mut b: Builder,
    pc: usize,
    ops: &[Op],
    start: u32,
    count: u16,
    name: &dyn Fn(u32) -> Option<String>,
) -> Result<Candidate, Reject> {
    let reject = |reason| Reject { pc, reason };
    if count != 2
        || b.stack.len() != 2
        || !matches!(ops.get(pc + 1), Some(Op::Return))
        || pc + 1 >= 64
    {
        return Err(reject("not a direct two-field result return"));
    }
    let keys = [name(start), start.checked_add(1).and_then(name)];
    let (done, value) = match (keys[0].as_deref(), keys[1].as_deref()) {
        (Some("done"), Some("value")) => (b.stack[0], b.stack[1]),
        (Some("value"), Some("done")) => (b.stack[1], b.stack[0]),
        _ => return Err(reject("unsupported result keys")),
    };
    b.plan.done = done;
    b.plan.value = value;
    b.plan.return_pc = pc + 1;
    if !matches!(
        b.plan.expressions[done],
        Expr::Constant(Literal::Boolean(false))
    ) {
        return Err(reject("done must be literal false"));
    }
    b.plan
        .obligations
        .push(Obligation::Truthiness { value: done });
    Ok(b.plan)
}

fn initial_builder() -> Builder {
    Builder {
        stack: Vec::new(),
        plan: Candidate {
            expressions: Vec::new(),
            branches: Vec::new(),
            store: None,
            obligations: vec![
                Obligation::CalleeEntryAndEnvironment,
                Obligation::NormalCallDepthAndGcSafepointBeforeProbes,
                Obligation::AllGuardsAndYieldOwnerBeforeCommit,
            ],
            done: 0,
            value: 0,
            return_pc: 0,
            tdz_locals: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn dump_ops() -> Vec<Op> {
        use Op::*;
        let mut ops = vec![
            LoadName(0, 0),
            GetProp(1, 0),
            LoadName(2, 1),
            Lt,
            JumpIfFalse(435),
            Tdz(0),
            LoadName(0, 2),
            GetProp(3, 4),
            GetProp(4, 8),
            Const(0),
            Gt,
            JumpIfFalsePeek(19),
            Pop,
            LoadName(0, 3),
            GetProp(5, 12),
            LoadName(0, 4),
            GetProp(3, 16),
            GetProp(4, 20),
            Lt,
            JumpIfFalse(36),
            LoadName(0, 5),
            LoadName(0, 6),
            GetProp(5, 24),
            Const(1),
            Add,
            SetPropDrop(5, 28),
            LoadName(0, 7),
            GetProp(3, 32),
            LoadName(0, 8),
            GetProp(5, 36),
            Const(2),
            Sub,
            GetElem,
            Const(3),
            MakeObject(6, 2, 0),
            Return,
        ];
        ops.resize(483, ReturnUndef);
        ops
    }

    fn inspect(ops: &[Op]) -> Result<Candidate, Reject> {
        let names = [
            "owner", "position", "limit", "items", "length", "cursor", "value", "done",
        ];
        let constants = [
            Literal::Number(0f64.to_bits()),
            Literal::Number(1f64.to_bits()),
            Literal::Number(1f64.to_bits()),
            Literal::Boolean(false),
        ];
        analyze_ops(ops, &|n| names.get(n as usize).map(|s| (*s).into()), &|n| {
            constants.get(n as usize).copied()
        })
    }

    #[test]
    fn actual_dump_preserves_peek_stack_order_and_pending_alias_obligations() {
        let plan = inspect(&dump_ops()).unwrap();
        assert_eq!(plan.return_pc, 35);
        assert_eq!(plan.tdz_locals, vec![0]);
        assert_eq!(
            plan.branches
                .iter()
                .map(|b| (b.pc, b.cold_target, b.retains_condition))
                .collect::<Vec<_>>(),
            vec![(4, 435, false), (11, 19, true), (19, 36, false)]
        );
        assert!(plan.branches.iter().all(|b| b.required_truthy));
        assert_eq!(plan.store.as_ref().unwrap().pc, 25);
        let forwarding: Vec<_> = plan
            .obligations
            .iter()
            .filter_map(|o| match o {
                Obligation::ForwardStoredEntryOrProveDisjoint { read, store_pc } => {
                    Some((*read, *store_pc))
                }
                _ => None,
            })
            .collect();
        assert_eq!(forwarding.len(), 2); // items and cursor after the deferred store
        assert!(forwarding.iter().all(|(_, pc)| *pc == 25));
        assert!(matches!(plan.expressions[plan.value], Expr::Dense { .. }));
        let arithmetic: Vec<_> = plan
            .expressions
            .iter()
            .filter_map(|e| match e {
                Expr::Binary { op, .. } => Some(*op),
                _ => None,
            })
            .collect();
        assert!(arithmetic.ends_with(&[Binary::Add, Binary::Sub]));
        assert!(plan
            .obligations
            .contains(&Obligation::NormalCallDepthAndGcSafepointBeforeProbes));
    }

    #[test]
    fn rejects_effects_bad_control_stack_and_record_contracts() {
        for (pc, op, reason) in [
            (26, Op::Call(0, 0), "unsupported entry operation"),
            (29, Op::SetPropDrop(5, 0), "multiple stores"),
            (4, Op::JumpIfFalse(0), "backedge or invalid branch"),
            (0, Op::Pop, "stack underflow"),
            (26, Op::PushHandler(100), "unsupported entry operation"),
            (26, Op::MakeClosure(0, 0), "unsupported entry operation"),
            (34, Op::MakeObject(0, 2, 0), "unsupported result keys"),
            (33, Op::Const(0), "done must be literal false"),
        ] {
            let mut ops = dump_ops();
            ops[pc] = op;
            assert_eq!(inspect(&ops).unwrap_err().reason, reason, "pc {pc}");
        }
        assert_eq!(
            inspect(&vec![Op::Tdz(0); 65]).unwrap_err().reason,
            "no bounded result return"
        );
    }

    #[test]
    fn differently_named_post_store_reads_still_require_runtime_alias_checks() {
        let mut ops = dump_ops();
        ops[29] = Op::GetProp(1, 36);
        let plan = inspect(&ops).unwrap();
        assert_eq!(
            plan.obligations
                .iter()
                .filter(|o| matches!(o, Obligation::ForwardStoredEntryOrProveDisjoint { .. }))
                .count(),
            2
        );
    }
}
