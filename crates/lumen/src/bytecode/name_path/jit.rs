//! A non-throwing cache probe that leaves native local slots and operands untouched on misses.
use crate::bytecode::{jit_exec, jit_opstat, Op};
use crate::interpreter::Scope;
use crate::jit::{JitCtx, SpFlag};
use crate::value::Value;
use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

/// # Safety
/// `ctx` is the active generated frame and `sp` has space for the opcode's declared outputs.
/// Its env is kept alive by the frame or the pinned direct callee, as in `jit_exec_inner`.
pub(crate) unsafe extern "C" fn load_cached(
    ctx: *mut JitCtx,
    pc: u32,
    mut sp: *mut Value,
) -> SpFlag {
    let frame = unsafe { &*ctx };
    let chunk = unsafe { &*frame.chunk };
    let (cache, for_call) = match chunk.ops[pc as usize] {
        Op::LoadName(_, cache) => (cache, false),
        Op::LoadNameForCall(_, cache) => (cache, true),
        _ => return unsafe { jit_exec(ctx, pc, sp) },
    };
    let env = ManuallyDrop::new(unsafe { Rc::from_raw(frame.env_raw.cast::<RefCell<Scope>>()) });
    let Some(value) = chunk.name_path_hit(unsafe { &*frame.interp }, &env, cache) else {
        return unsafe { jit_exec(ctx, pc, sp) };
    };
    #[cfg(test)]
    HITS.with(|hits| hits.set(hits.get() + 1));
    // A guarded path never passes through a with object or executes JS. A call therefore
    // receives Undefined as its reference receiver; ordinary invocation does this conversion.
    unsafe { jit_opstat(&mut *ctx, pc) };
    unsafe {
        if for_call {
            sp.write(Value::Undefined);
            sp = sp.add(1);
        }
        sp.write(value);
        SpFlag {
            sp: sp.add(1),
            flag: 0,
        }
    }
}

#[cfg(test)]
thread_local! {
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    #[test]
    fn exact_scope_cache_keeps_owned_values_alive_across_collection() {
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        super::super::EXACT_HITS.with(|hits| hits.set(0));
        let result = engine
            .eval(
                r#"
                const make=Function('return function(seed){eval("var deep=seed");return function(){eval("var middle=1");return function(){eval("var inner=2");return function(n){return [deep,n]}}() }()}')();
                const object={value:7}, read=make(object);
                for(let i=0;i<500;i++) {
                    const result=read(i);
                    if(result[0]!==object || result[1]!==i) throw 'stale exact path';
                    if((i%50)===0)$262.gc();
                }
                'passed'
            "#,
                false,
            )
            .unwrap();
        assert!(matches!(result, Completion::Value(v) if v == "passed"));
        #[cfg(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        ))]
        assert!(
            super::super::EXACT_HITS.with(|hits| hits.get()) > 0,
            "exact scope path was not exercised"
        );
    }

    #[test]
    fn native_cache_probe_handles_owned_values_calls_and_getter_invalidation() {
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        super::HITS.with(|hits| hits.set(0));
        let result = engine.eval(r#"
            let shared;
            function make(local) { return () => [shared, target(local)]; }
            globalThis.target=function(n) { 'use strict'; if(this!==undefined) throw 'receiver'; return n; };
            function drive() {
                const values=[undefined,null,true,7,1n,'hello',Symbol('x'),{x:1}];
                for(let i=0;i<500;i++) {
                    shared=values[i%values.length];
                    const result=make(i)();
                    if(result[0]!==shared || result[1]!==i) throw 'stale value';
                }
                const reader=make(19), original=target;
                let getters=0;
                Object.defineProperty(globalThis,'target',{configurable:true,get(){getters++;return original;}});
                for(let i=0;i<30;i++) if(reader()[1]!==19) throw 'getter result';
                if(getters!==30) throw 'getter count';
                delete globalThis.target;
                let threw=false;
                try {reader();} catch(e) {threw=e instanceof ReferenceError;}
                if(!threw) throw 'missing name';
            }
            drive(); 'passed'
        "#, false).unwrap();
        assert!(matches!(result, Completion::Value(v) if v == "passed"));
        #[cfg(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        ))]
        assert!(
            super::HITS.with(|hits| hits.get()) > 0,
            "native probe was not exercised"
        );
    }
}
