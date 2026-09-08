//! Iterator-only prepared call boundary; ordinary Interp::call remains unchanged.
use super::{Abrupt, Interp, MAX_EVAL_DEPTH};
use crate::value::Value;

pub(crate) enum EntryResult {
    Ordinary(Value),
    Yielded(Value),
}

impl Interp {
    /// Run an optional guarded entry inside the normal logical-call depth/GC boundary.
    /// `attempt` must not invoke JS or a safepoint. It must acquire fresh rooted state after
    /// this boundary's poll, leave everything unchanged on None, and have no possible miss
    /// after its first observable write. Some carries an owned yielded value, not a record.
    pub(crate) fn call_iterator_entry(
        &mut self,
        callee: Value,
        this: Value,
        args: &[Value],
        attempt: impl FnOnce(&mut Self, &Value, &Value) -> Option<Value>,
    ) -> Result<EntryResult, Abrupt> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return Err(self.throw("RangeError", "Maximum call stack size exceeded"));
        }
        if let Err(error) = self.gc_check_amortized() {
            self.depth -= 1;
            return Err(error);
        }
        let mut result = match attempt(self, &callee, &this) {
            Some(value) => Ok(EntryResult::Yielded(value)),
            None => self
                .call_inner(callee, this, args)
                .map(EntryResult::Ordinary),
        };
        // Retain the original proper-tail trampoline, including a fresh poll per redispatch.
        // A pending tail supersedes either initial outcome and returns an ordinary result.
        while result.is_ok() {
            match self.pending_tail.take() {
                Some(tail) => {
                    let (callee, this, args) = *tail;
                    if let Err(error) = self.gc_check_amortized() {
                        result = Err(error);
                        break;
                    }
                    result = self
                        .call_inner(callee, this, &args)
                        .map(EntryResult::Ordinary);
                }
                None => break,
            }
        }
        self.depth -= 1;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryResult, MAX_EVAL_DEPTH};
    use crate::{value::Value, Engine};
    use std::cell::Cell;

    #[test]
    fn normal_miss_and_hit_share_one_depth_and_poll_boundary() {
        let mut engine = Engine::new();
        let callee = Value::Obj(engine.interp.make_native("probe", 0, |interp, _, _| {
            assert_eq!(interp.depth, 1);
            assert_eq!(interp.gc_tick, 1);
            Ok(Value::Num(7.0))
        }));
        engine.interp.gc_tick = 0;
        assert!(matches!(
            engine.interp.call(callee.clone(), Value::Undefined, &[]),
            Ok(Value::Num(7.0))
        ));
        assert_eq!((engine.interp.depth, engine.interp.gc_tick), (0, 1));
        engine.interp.gc_tick = 0;
        let missed = engine.interp.call_iterator_entry(
            callee.clone(),
            Value::Undefined,
            &[],
            |interp, _, _| {
                assert_eq!((interp.depth, interp.gc_tick), (1, 1));
                None
            },
        );
        assert!(matches!(missed, Ok(EntryResult::Ordinary(Value::Num(7.0)))));
        assert_eq!((engine.interp.depth, engine.interp.gc_tick), (0, 1));
        engine.interp.gc_tick = 0;
        let hit =
            engine
                .interp
                .call_iterator_entry(callee, Value::Undefined, &[], |interp, _, _| {
                    assert_eq!((interp.depth, interp.gc_tick), (1, 1));
                    Some(Value::Num(9.0))
                });
        assert!(matches!(hit, Ok(EntryResult::Yielded(Value::Num(9.0)))));
        assert_eq!((engine.interp.depth, engine.interp.gc_tick), (0, 1));
    }

    #[test]
    fn overflow_skips_attempt_and_poll_and_restores_depth() {
        let mut engine = Engine::new();
        engine.interp.depth = MAX_EVAL_DEPTH;
        engine.interp.gc_tick = 0;
        let attempted = Cell::new(false);
        let result = engine.interp.call_iterator_entry(
            Value::Undefined,
            Value::Undefined,
            &[],
            |_, _, _| {
                attempted.set(true);
                Some(Value::Undefined)
            },
        );
        assert!(result.is_err());
        assert!(!attempted.get());
        assert_eq!(engine.interp.depth, MAX_EVAL_DEPTH);
        assert_eq!(engine.interp.gc_tick, 0);
    }

    #[test]
    fn ordinary_error_restores_depth_and_pending_tail_supersedes_yield() {
        let mut engine = Engine::new();
        engine.interp.gc_tick = 0;
        let failed = engine.interp.call_iterator_entry(
            Value::Undefined,
            Value::Undefined,
            &[],
            |_, _, _| None,
        );
        assert!(failed.is_err());
        assert_eq!(engine.interp.depth, 0);
        let tail = Value::Obj(engine.interp.make_native("tail", 0, |interp, _, _| {
            assert_eq!(interp.depth, 1);
            assert_eq!(interp.gc_tick, 2);
            Ok(Value::Num(13.0))
        }));
        engine.interp.gc_tick = 0;
        engine.interp.pending_tail = Some(Box::new((tail, Value::Undefined, vec![])));
        let result = engine.interp.call_iterator_entry(
            Value::Undefined,
            Value::Undefined,
            &[],
            |_, _, _| Some(Value::Num(9.0)),
        );
        assert!(matches!(
            result,
            Ok(EntryResult::Ordinary(Value::Num(13.0)))
        ));
        assert_eq!((engine.interp.depth, engine.interp.gc_tick), (0, 2));
    }
}
