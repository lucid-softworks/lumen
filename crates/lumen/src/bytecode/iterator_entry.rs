//! Opt-in proof feedback only. No candidate is executed or treated as a queued hit.
use super::{Chunk, Op};
use crate::interpreter::{Abrupt, Interp};
use crate::value::{Callable, Object, Value};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};

mod classification;
use classification::{Outcome, Version};

struct Site {
    callee: Weak<RefCell<Object>>,
    version: Version,
    outcome: Outcome,
}

pub(super) struct Feedback {
    sites: Vec<(usize, RefCell<Site>)>,
}

impl Feedback {
    pub(super) fn for_ops(ops: &[Op]) -> Option<Box<Self>> {
        let enabled = std::env::var_os("LUMEN_ITERATOR_ENTRY_FEEDBACK").is_some();
        #[cfg(test)]
        let enabled = enabled || FORCE.with(|v| v.get());
        if !enabled {
            return None;
        }
        Some(Box::new(Self {
            sites: ops
                .iter()
                .enumerate()
                .filter(|(_, op)| matches!(op, Op::IterStepL(..)))
                .map(|(pc, _)| {
                    (
                        pc,
                        RefCell::new(Site {
                            callee: Weak::new(),
                            version: Version::Cold,
                            outcome: Outcome::Cold,
                        }),
                    )
                })
                .collect(),
        }))
    }

    fn observe(&self, pc: usize, interp: &Interp, next: &Value) {
        let Ok(index) = self.sites.binary_search_by_key(&pc, |(pc, _)| *pc) else {
            return;
        };
        let Value::Obj(object) = next else {
            record("non-user");
            return;
        };
        let mut site = self.sites[index].1.borrow_mut();
        let object_ref = object.borrow();
        let Callable::User(user) = &object_ref.call else {
            record("non-user");
            return;
        };
        if !interp.ordinary_get_ptr(Rc::as_ptr(object) as usize) || !object_ref.ic_plain.get() {
            record("nonordinary-callee");
            return;
        }
        if !same_realm(interp, &user.env) {
            record("foreign-realm");
            return;
        }
        let version = classification::version(&user.func);
        let same = site.callee.as_ptr() == Rc::as_ptr(object);
        if !same || site.version != version || matches!(site.outcome, Outcome::Cold) {
            site.callee = Rc::downgrade(object);
            site.version = version;
            site.outcome = classification::classify(&user.func);
            #[cfg(test)]
            REPLANS.with(|n| n.set(n.get() + 1));
        }
        match &site.outcome {
            Outcome::Cold => record("cold-uncompiled"),
            Outcome::Accepted(plan, label) => {
                let _ = plan.return_pc;
                record(label);
            }
            Outcome::Rejected(reason, label) => {
                let _ = reason.pc;
                record(label);
            }
            Outcome::Unsupported(reason) => record(reason),
        }
    }
}

fn same_realm(interp: &Interp, env: &crate::interpreter::Env) -> bool {
    let mut current = env.clone();
    for _ in 0..64 {
        let parent = current.borrow().parent.clone();
        match parent {
            Some(parent) => current = parent,
            None => return Rc::ptr_eq(&current, &interp.global_env),
        }
    }
    false
}

/// Original captured Values remain owned across reentrant next/done/value calls.
pub(super) fn fallback(
    chunk: &Chunk,
    pc: usize,
    interp: &mut Interp,
    iterator: Value,
    next: Value,
) -> Result<Option<Value>, Abrupt> {
    let result = interp.iterator_step(&iterator, &next)?;
    if let Some(feedback) = &chunk.iterator_entry_feedback {
        feedback.observe(pc, interp, &next);
    }
    Ok(result)
}

struct Counts(BTreeMap<String, u64>);
impl Drop for Counts {
    fn drop(&mut self) {
        for (status, count) in &self.0 {
            eprintln!("[iterator-entry-feedback] {count} {status}");
        }
    }
}
thread_local! {static COUNTS:RefCell<Counts>=const {RefCell::new(Counts(BTreeMap::new()))};}
fn record(status: &str) {
    let _ = COUNTS.try_with(|counts| {
        let mut counts = counts.borrow_mut();
        if let Some(count) = counts.0.get_mut(status) {
            *count += 1;
        } else {
            counts.0.insert(status.into(), 1);
        }
    });
}

#[cfg(test)]
thread_local! {static REPLANS:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}
#[cfg(test)]
thread_local! {static FORCE:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bytecode::Tier, Completion, Engine};

    struct Force;
    impl Force {
        fn new() -> Self {
            FORCE.with(|v| v.set(true));
            COUNTS.with(|c| c.borrow_mut().0.clear());
            Self
        }
    }
    impl Drop for Force {
        fn drop(&mut self) {
            FORCE.with(|v| v.set(false));
        }
    }

    #[test]
    fn both_dispatches_observe_captured_next_and_skip_array_yields() {
        let _force = Force::new();
        for tier in [Tier::Bytecode, Tier::Jit] {
            let mut engine = engine(tier);
            COUNTS.with(|c| c.borrow_mut().0.clear());
            let result = engine
                .eval(
                    r#"
                var steps=0;
                var original=function(){iterator.next=function(){throw 'replacement';};
                    return {value:++steps,done:steps>2};};
                iterator.next=original;iterator[Symbol.iterator]=function(){return this;};
                function drive(o){var sum=0;for(var v of o)sum+=v;return sum;}
                if(drive(iterator)!==3||steps!==3)throw 'capture';
            "#,
                    false,
                )
                .unwrap();
            assert!(matches!(result, Completion::Value(_)));
            assert_eq!(COUNTS.with(|c| c.borrow().0.values().sum::<u64>()), 3);
            let original = get(&mut engine, "original");
            let drive = get(&mut engine, "drive");
            let Value::Obj(original) = original else {
                panic!("function")
            };
            let Value::Obj(drive) = drive else {
                panic!("function")
            };
            let object = drive.borrow();
            let Callable::User(user) = &object.call else {
                panic!("user")
            };
            let chunk = user
                .func
                .code2
                .get()
                .or_else(|| user.func.code.get())
                .unwrap()
                .as_ref()
                .unwrap();
            let feedback = chunk.iterator_entry_feedback.as_ref().unwrap();
            assert_eq!(
                feedback.sites[0].1.borrow().callee.as_ptr(),
                Rc::as_ptr(&original)
            );
            drop(object);
            COUNTS.with(|c| c.borrow_mut().0.clear());
            let result = engine
                .eval("if(drive([1,2,3])!==6)throw 'array';", false)
                .unwrap();
            assert!(matches!(result, Completion::Value(_)));
            // Three intrinsic yielded steps bypass fallback; exhaustion still observes native next.
            if std::env::var_os("LUMEN_NO_ARRAY_ITERATOR_STEP").is_none() {
                assert_eq!(COUNTS.with(|c| c.borrow().0.values().sum::<u64>()), 1);
            }
        }
    }

    fn engine(tier: Tier) -> Engine {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine
            .eval(
                r#"
            function make(seed){return function(){return {value:seed,done:false};};}
            var first=make(7),second=make(11);
            var iterator={next:first};
            var thrown=function(){throw 19;},bad=function(){return 1;};
        "#,
                false,
            )
            .unwrap();
        assert!(matches!(result, Completion::Value(_)));
        engine
    }

    fn get(engine: &mut Engine, name: &str) -> Value {
        let global = Value::Obj(engine.interp.global.clone());
        engine
            .interp
            .get_member(&global, name)
            .unwrap_or_else(|_| panic!("fixture binding"))
    }

    fn consumer() -> Rc<Chunk> {
        let statements =
            crate::parser::parse_script("function consume(it){for(var v of it){break;}}", false)
                .unwrap_or_else(|_| panic!("valid consumer"));
        let crate::ast::Stmt::FuncDecl(function) = &statements[0] else {
            panic!("function")
        };
        let mut chunk = super::super::compile(function).unwrap();
        let sites = chunk
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, Op::IterStepL(..)))
            .map(|(pc, _)| {
                (
                    pc,
                    RefCell::new(Site {
                        callee: Weak::new(),
                        version: Version::Cold,
                        outcome: Outcome::Cold,
                    }),
                )
            })
            .collect();
        Rc::get_mut(&mut chunk).unwrap().iterator_entry_feedback =
            Some(Box::new(Feedback { sites }));
        chunk
    }

    #[test]
    fn fallback_observes_original_callee_and_separate_closure_instances_without_roots() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = engine(tier);
            let chunk = consumer();
            let feedback = chunk.iterator_entry_feedback.as_ref().unwrap();
            let pc = feedback.sites[0].0;
            let first = get(&mut engine, "first");
            let second = get(&mut engine, "second");
            let iterator = get(&mut engine, "iterator");
            let Value::Obj(first_obj) = &first else {
                panic!("callable")
            };
            let owners = Rc::strong_count(first_obj);
            let value = fallback(
                &chunk,
                pc,
                &mut engine.interp,
                iterator.clone(),
                first.clone(),
            )
            .unwrap_or_else(|_| panic!("successful first next"));
            assert!(matches!(value, Some(Value::Num(7.0))));
            assert_eq!(Rc::strong_count(first_obj), owners);
            assert_eq!(
                feedback.sites[0].1.borrow().callee.as_ptr(),
                Rc::as_ptr(first_obj)
            );
            let value = fallback(&chunk, pc, &mut engine.interp, iterator, second.clone())
                .unwrap_or_else(|_| panic!("successful second next"));
            assert!(matches!(value, Some(Value::Num(11.0))));
            let Value::Obj(second_obj) = &second else {
                panic!("callable")
            };
            assert_eq!(
                feedback.sites[0].1.borrow().callee.as_ptr(),
                Rc::as_ptr(second_obj)
            );
            if !matches!(tier, Tier::Interp) {
                assert!(matches!(
                    feedback.sites[0].1.borrow().outcome,
                    Outcome::Accepted(..)
                ));
            }
        }
    }

    #[test]
    fn thrown_and_nonobject_results_do_not_create_observations() {
        let mut engine = engine(Tier::Jit);
        let chunk = consumer();
        let feedback = chunk.iterator_entry_feedback.as_ref().unwrap();
        let pc = feedback.sites[0].0;
        let iterator = get(&mut engine, "iterator");
        for name in ["thrown", "bad"] {
            let next = get(&mut engine, name);
            assert!(fallback(&chunk, pc, &mut engine.interp, iterator.clone(), next).is_err());
            assert!(feedback.sites[0].1.borrow().callee.upgrade().is_none());
        }
    }

    #[test]
    fn feedback_weak_pin_does_not_keep_closure_environment_alive() {
        let mut engine = engine(Tier::Jit);
        let chunk = consumer();
        let pc = chunk.iterator_entry_feedback.as_ref().unwrap().sites[0].0;
        let first = get(&mut engine, "first");
        let iterator = get(&mut engine, "iterator");
        fallback(&chunk, pc, &mut engine.interp, iterator, first)
            .unwrap_or_else(|_| panic!("successful next"));
        let weak = chunk.iterator_entry_feedback.as_ref().unwrap().sites[0]
            .1
            .borrow()
            .callee
            .clone();
        assert!(weak.upgrade().is_some());
        let result = engine
            .eval("first=null;second=null;iterator=null;$262.gc();", false)
            .unwrap();
        assert!(matches!(result, Completion::Value(_)));
        assert!(
            weak.upgrade().is_none(),
            "feedback retained the callable graph"
        );
    }
}
