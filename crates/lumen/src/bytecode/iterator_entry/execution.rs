//! Experimental call/result elision through the existing logical call boundary.
use super::{classification, same_realm, Chunk, Feedback, Outcome};
use crate::interpreter::{call_entry::EntryResult, Abrupt, Interp};
use crate::value::{Callable, Value};
use std::rc::Rc;

pub(super) fn enabled() -> bool {
    let enabled = std::env::var_os("LUMEN_ITERATOR_ENTRY_NATIVE").is_some();
    #[cfg(test)]
    let enabled = enabled || FORCE.with(|v| v.get());
    enabled
}

pub(super) fn step(
    chunk: &Chunk,
    pc: usize,
    interp: &mut Interp,
    iterator: Value,
    next: Value,
) -> Result<Option<Value>, Abrupt> {
    let feedback = chunk.iterator_entry_feedback.as_deref();
    let result = if let Some(feedback) = feedback.filter(|f| f.execute) {
        // Owned caller operands survive the poll. No cached heap address is borrowed yet.
        match interp.call_iterator_entry(
            next.clone(),
            iterator.clone(),
            &[],
            |i, next, iterator| {
                let value = try_cached(feedback, pc, i, next, iterator);
                feedback.record(if value.is_some() {
                    "executed"
                } else {
                    "entry-miss"
                });
                value
            },
        )? {
            EntryResult::Yielded(value) => return Ok(Some(value)),
            EntryResult::Ordinary(result) => consume(interp, result)?,
        }
    } else {
        interp.iterator_step(&iterator, &next)?
    };
    if let Some(feedback) = feedback {
        feedback.observe(pc, interp, &next);
    }
    Ok(result)
}

fn try_cached(
    feedback: &Feedback,
    pc: usize,
    interp: &mut Interp,
    next: &Value,
    iterator: &Value,
) -> Option<Value> {
    let index = feedback
        .sites
        .binary_search_by_key(&pc, |(pc, _)| *pc)
        .ok()?;
    let site = feedback.sites[index].1.try_borrow().ok()?;
    let Value::Obj(object) = next else {
        return None;
    };
    if site.callee.as_ptr() != Rc::as_ptr(object) {
        return None;
    }
    {
        let object = object.try_borrow().ok()?;
        let Callable::User(user) = &object.call else {
            return None;
        };
        if site.version != classification::version(&user.func) || !same_realm(interp, &user.env) {
            return None;
        }
    }
    let Outcome::Accepted(..) = &site.outcome else {
        return None;
    };
    let env = entry_environment(interp, next)?;
    if !matches!(iterator, Value::Obj(_)) {
        return None;
    }
    // All live callee/version/realm guards and the normal poll precede this call.
    unsafe { site.native.as_ref()?.run(interp, &env, iterator) }
}

pub(super) fn compile_entry(
    interp: &Interp,
    next: &Value,
    outcome: &Outcome,
) -> Option<crate::jit::iterator_entry::Entry> {
    let Outcome::Accepted(plan, _) = outcome else {
        return None;
    };
    let env = entry_environment(interp, next)?;
    let Value::Obj(object) = next else {
        return None;
    };
    let object = object.try_borrow().ok()?;
    let Callable::User(user) = &object.call else {
        return None;
    };
    let chunk = user
        .func
        .code2
        .get()
        .or_else(|| user.func.code.get())?
        .as_ref()?;
    crate::jit::iterator_entry::compile(plan, chunk, &env)
}

fn entry_environment(i: &Interp, next: &Value) -> Option<crate::interpreter::Env> {
    let Value::Obj(callee) = next else {
        return None;
    };
    if !i.ordinary_get_ptr(Rc::as_ptr(callee) as usize)
        || i.class_info.contains_key(&(Rc::as_ptr(callee) as usize))
    {
        return None;
    }
    let callee_ref = callee.try_borrow().ok()?;
    if !callee_ref.ic_plain.get() {
        return None;
    }
    let Callable::User(user) = &callee_ref.call else {
        return None;
    };
    let function = &user.func;
    if function.is_arrow
        || function.is_async
        || function.is_generator
        || !function.params.is_empty()
        || (function.is_fn_expr && function.name.is_some())
        || function.scan_flags() & crate::ast::SCAN_ARGUMENTS != 0
    {
        return None;
    }
    let chunk = function
        .code2
        .get()
        .or_else(|| function.code.get())?
        .as_ref()?;
    if !chunk.jit_no_activation() {
        return None;
    }
    let env = user.env.clone();
    if !same_realm(i, &env) {
        return None;
    }
    drop(callee_ref);
    Some(env)
}

/// Same IteratorComplete/IteratorValue order, after call depth has been restored.
fn consume(interp: &mut Interp, result: Value) -> Result<Option<Value>, Abrupt> {
    if !matches!(result, Value::Obj(_)) {
        return Err(interp.throw("TypeError", "iterator result is not an object"));
    }
    let done = interp.get_member(&result, "done")?;
    if interp.to_boolean(&done) {
        Ok(None)
    } else {
        Ok(Some(interp.get_member(&result, "value")?))
    }
}

#[cfg(test)]
thread_local! {static FORCE: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}

#[cfg(all(
    test,
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use super::FORCE;
    use crate::{bytecode::Tier, Completion, Engine};

    struct Enabled;
    impl Enabled {
        fn new() -> Self {
            FORCE.with(|v| v.set(true));
            super::super::FORCE.with(|v| v.set(true));
            super::super::COUNTS.with(|v| v.borrow_mut().0.clear());
            Self
        }
    }
    impl Drop for Enabled {
        fn drop(&mut self) {
            FORCE.with(|v| v.set(false));
            super::super::FORCE.with(|v| v.set(false));
        }
    }

    const SOURCE: &str = r#"
        function make(){
            const state={cursor:0,items:[{x:7},{x:8},{x:9}]};
            return {state:state,[Symbol.iterator](){return this;},next(){
                if(state.cursor<state.items.length){
                    state.cursor=state.cursor+1;
                    return {value:state.items[state.cursor-1],done:false};
                }
                return {done:true};
            }};
        }
        function drain(it){var values=[];for(var value of it)values.push(value);return values;}
    "#;

    fn run(engine: &mut Engine, source: &str) {
        let result = engine.eval(source, false).expect("valid source");
        assert!(matches!(result, Completion::Value(_)));
    }

    #[test]
    fn both_consumers_execute_entries_and_keep_yielded_objects_alive() {
        let _enabled = Enabled::new();
        for tier in [Tier::Bytecode, Tier::Jit] {
            super::super::COUNTS.with(|v| v.borrow_mut().0.clear());
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            run(&mut engine, SOURCE);
            run(&mut engine, "var it=make();var values=drain(it);if(it.state.cursor!==3)throw 'cursor';it=null;$262.gc();if(values.length!==3||values[0].x!==7||values[1].x!==8||values[2].x!==9)throw 'owners';");
            super::super::COUNTS.with(|v| {
                assert_eq!(v.borrow().0.get("executed"), Some(&2));
            });
        }
    }

    #[test]
    fn post_store_accessor_miss_runs_original_increment_and_getter_once() {
        let _enabled = Enabled::new();
        for tier in [Tier::Bytecode, Tier::Jit] {
            super::super::COUNTS.with(|v| v.borrow_mut().0.clear());
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            run(&mut engine, SOURCE);
            run(&mut engine, "var it=make(),calls=0;Object.defineProperty(it.state.items,'1',{get(){calls++;if(it.state.cursor!==2)throw 'replayed write';return {x:8};},configurable:true});var values=drain(it);if(calls!==1||it.state.cursor!==3||values.length!==3||values[1].x!==8)throw 'fallback';");
            super::super::COUNTS.with(|v| {
                assert!(v.borrow().0.get("entry-miss").copied().unwrap_or(0) >= 3);
            });
        }
    }

    #[test]
    fn native_cache_does_not_root_iterator_closure_or_environment() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        run(&mut engine, SOURCE);
        run(&mut engine, "var it=make();var values=drain(it);");
        let global = crate::value::Value::Obj(engine.interp.global.clone());
        let iterator = engine
            .interp
            .get_member(&global, "it")
            .unwrap_or_else(|_| panic!("it"));
        let next = engine
            .interp
            .get_member(&iterator, "next")
            .unwrap_or_else(|_| panic!("next"));
        let crate::value::Value::Obj(next) = next else {
            panic!("callable")
        };
        let weak = std::rc::Rc::downgrade(&next);
        let env_weak = {
            let borrowed = next.borrow();
            let crate::value::Callable::User(user) = &borrowed.call else {
                panic!("user")
            };
            std::rc::Rc::downgrade(&user.env)
        };
        drop((next, iterator));
        run(&mut engine, "it=null;values=null;$262.gc();");
        assert!(weak.upgrade().is_none(), "native site kept next alive");
        assert!(
            env_weak.upgrade().is_none(),
            "native code kept captured environment alive"
        );
    }

    #[test]
    fn selected_code_change_invalidates_executable_entry() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Bytecode);
        engine.set_tier_threshold(0);
        run(&mut engine, "var it={next(){return {value:7,done:false};},[Symbol.iterator](){return this;}};function one(it){for(var value of it)return value;}if(one(it)!==7||one(it)!==7||one(it)!==7)throw 'base';");
        super::super::COUNTS.with(|v| assert_eq!(v.borrow().0.get("executed"), Some(&2)));
        let global = crate::value::Value::Obj(engine.interp.global.clone());
        let iterator = engine
            .interp
            .get_member(&global, "it")
            .unwrap_or_else(|_| panic!("it"));
        let next = engine
            .interp
            .get_member(&iterator, "next")
            .unwrap_or_else(|_| panic!("next"));
        let crate::value::Value::Obj(next) = next else {
            panic!("function")
        };
        let function = {
            let next = next.borrow();
            let crate::value::Callable::User(user) = &next.call else {
                panic!("user")
            };
            user.func.clone()
        };
        let parsed = crate::parser::parse_script(
            "function replacement(){return {value:9,done:false};}",
            false,
        )
        .unwrap_or_else(|_| panic!("valid replacement"));
        let crate::ast::Stmt::FuncDecl(replacement) = &parsed[0] else {
            panic!("function")
        };
        let chunk = crate::bytecode::compile(replacement).expect("replacement bytecode");
        assert!(function.code2.set(Some(chunk)).is_ok());
        run(
            &mut engine,
            "if(one(it)!==9||one(it)!==9)throw 'stale native code';",
        );
        super::super::COUNTS.with(|v| assert_eq!(v.borrow().0.get("executed"), Some(&3)));
    }
}
