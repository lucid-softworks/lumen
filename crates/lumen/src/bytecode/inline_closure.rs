//! Shared-function guards for inlines that resolve names in the caller's lexical environment.
use super::{Chunk, InlineTarget, InlineWay, Op};
use crate::{
    ast::Function,
    interpreter::{Interp, Scope},
    value::{Callable, Value},
};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

pub(super) struct Guard {
    function: Weak<Function>,
    pub(super) slot: u16,
}

pub(super) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(value) = OVERRIDE.with(|v| v.get()) {
        return value;
    }
    std::env::var_os("LUMEN_JIT_INLINE_CLOSURES").is_some()
}

impl Guard {
    pub(super) fn for_way(way: &InlineWay, slot: u16) -> Self {
        Self {
            function: Rc::downgrade(&way.f),
            slot,
        }
    }

    fn matches(
        &self,
        interp: &Interp,
        target: &InlineTarget,
        callee: &Value,
        receiver: Option<&Value>,
        env: *const RefCell<Scope>,
    ) -> bool {
        if interp.fn_frames.is_empty()
            || target.check_this && !matches!(receiver, Some(Value::Obj(_)))
        {
            return false;
        }
        let Value::Obj(object) = callee else {
            return false;
        };
        let key = Rc::as_ptr(object) as usize;
        if !interp.ordinary_get_ptr(key) || interp.class_info.contains_key(&key) {
            return false;
        }
        let Ok(borrowed) = object.try_borrow() else {
            return false;
        };
        let Callable::User(user) = &borrowed.call else {
            return false;
        };
        // The weak Function pin prevents address reuse. Shared live environment identity
        // proves the splice's free-name reads see precisely the callee's bindings.
        if Rc::as_ptr(&user.func) != self.function.as_ptr()
            || Rc::as_ptr(&user.env) != env
            || !borrowed.ic_plain.get()
        {
            return false;
        }
        let Ok(scope) = user.env.try_borrow() else {
            return false;
        };
        if scope.under_with {
            return false;
        }
        #[cfg(test)]
        HITS.with(|hits| hits.set(hits.get() + 1));
        true
    }
}

impl Chunk {
    pub(super) fn inline_guard_matches(
        &self,
        interp: &Interp,
        index: u32,
        callee: &Value,
        receiver: Option<&Value>,
        env: *const RefCell<Scope>,
    ) -> bool {
        let target = &self.inline_targets[index as usize];
        if let Some(guard) = self.inline_closure(index) {
            return guard.matches(interp, target, callee, receiver, env);
        }
        matches!(callee, Value::Obj(object) if Rc::as_ptr(object) as usize == target.expected)
            && (!target.check_this || matches!(receiver, Some(Value::Obj(_))))
            && (target.expected_env == 0 || env as usize == target.expected_env)
    }
}

/// Called before changing any operands or locals; every rejection uses the original call.
/// # Safety
/// `ctx` is a live JIT activation and `sp` points past the current initialized operands.
pub(crate) unsafe extern "C" fn check_native(
    ctx: *mut crate::jit::JitCtx,
    pc: u32,
    sp: *const Value,
) -> u64 {
    let ctx = unsafe { &*ctx };
    let chunk = unsafe { &*ctx.chunk };
    let Op::InlineGuard(index, _) = chunk.jit_ops()[pc as usize] else {
        return 0;
    };
    let target = &chunk.inline_targets[index as usize];
    let callee = unsafe { sp.sub(target.argc as usize + 1) };
    let receiver = target.check_this.then(|| unsafe { &*callee.sub(1) });
    u64::from(chunk.inline_guard_matches(
        unsafe { &*ctx.interp },
        index,
        unsafe { &*callee },
        receiver,
        ctx.env_raw.cast(),
    ))
}

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Scoped override for execution tests of optimizations composed with shared inlines.
#[cfg(test)]
pub(crate) fn test_with_enabled(run: impl FnOnce()) {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            OVERRIDE.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(OVERRIDE.with(|value| value.replace(Some(true))));
    run();
}

#[cfg(test)]
mod tests {
    use super::{HITS, OVERRIDE};
    use crate::{bytecode::Tier, Completion, Engine};

    struct Enabled(Option<bool>);
    impl Enabled {
        fn new() -> Self {
            HITS.with(|hits| hits.set(0));
            Self(OVERRIDE.with(|value| value.replace(Some(true))))
        }
    }
    impl Drop for Enabled {
        fn drop(&mut self) {
            OVERRIDE.with(|value| value.set(self.0));
        }
    }
    fn eval(engine: &mut Engine, source: &str) {
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "passed"),
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
        assert!(engine.interp.fn_frames.is_empty());
    }

    fn assert_optimized(engine: &Engine) {
        use crate::value::Callable;
        let first = engine
            .interp
            .global
            .borrow()
            .props
            .get("first")
            .unwrap()
            .value();
        let invoke = first
            .as_obj()
            .unwrap()
            .borrow()
            .props
            .get("invoke")
            .unwrap()
            .value();
        let object = invoke.as_obj().unwrap().borrow();
        let Callable::User(user) = &object.call else {
            panic!("user function")
        };
        let chunk = user
            .func
            .code2
            .get()
            .and_then(Option::as_ref)
            .expect("optimized caller");
        assert!(chunk.has_inline_closures());
    }

    #[test]
    fn fresh_shared_closures_preserve_capture_and_reflected_identity() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r#"
            function inspect() { eval(''); return inspect.caller; }
            function make(seed) {
                function leaf(x) {
                    if(inspect()!==leaf) throw 'wrong live callee';
                    return seed+x;
                }
                function invoke(x) { return leaf(x); }
                return {leaf:leaf,invoke:invoke};
            }
            var first=make(3);
            function warm() { for(var n=0;n<500;n++) if(first.invoke(4)!==7) throw 'warm'; }
            warm();
            'passed'
        "#,
        );
        assert_optimized(&engine);
        for tier in [Tier::Jit, Tier::Bytecode] {
            engine.set_tier(tier);
            HITS.with(|hits| hits.set(0));
            eval(
                &mut engine,
                r#"
                for(var n=0;n<40;n++) {
                    var next=make(n);
                    if(next.invoke(4)!==n+4) throw 'fresh capture';
                }
                'passed'
            "#,
            );
            assert!(
                HITS.with(|hits| hits.get()) > 0,
                "fresh guards never accepted in {tier:?}"
            );
        }
    }

    #[test]
    fn changed_function_environment_and_callable_kind_use_the_original_call() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r#"
            function make(seed) {
                function leaf(x) {return seed+x;}
                function invoke(f,x) {return f(x);}
                return {leaf:leaf,invoke:invoke};
            }
            var first=make(3), second=make(20);
            function warm() {for(var n=0;n<500;n++) if(first.invoke(first.leaf,4)!==7) throw 'warm';}
            warm(); 'passed'
        "#,
        );
        assert_optimized(&engine);
        for tier in [Tier::Jit, Tier::Bytecode] {
            engine.set_tier(tier);
            HITS.with(|hits| hits.set(0));
            eval(
                &mut engine,
                r#"
                if(first.invoke(second.leaf,4)!==24) throw 'foreign environment';
                if(first.invoke(function(x){return 50+x;},4)!==54) throw 'foreign AST';
                var count=0;
                var proxy=new Proxy(first.leaf,{apply:function(f,t,args){count++;return 90;}});
                if(first.invoke(proxy,4)!==90 || count!==1) throw 'proxy';
                if(first.invoke(second.leaf.bind(null),4)!==24) throw 'bound';
                var rejected=false;
                try {first.invoke(class C {},4);} catch(e) {rejected=e instanceof TypeError;}
                if(!rejected) throw 'class call';
                'passed'
            "#,
            );
            assert_eq!(HITS.with(|hits| hits.get()), 0, "invalid guard accepted");
        }
    }

    #[test]
    fn fresh_nested_inlines_preserve_getters_exceptions_and_gc_lifetime() {
        use crate::value::{set_builtin, Value};
        use std::rc::Rc;
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        let collect = engine.interp.make_native("collect", 0, |interp, _, _| {
            interp.gc_collect();
            Ok(Value::Undefined)
        });
        set_builtin(&engine.interp.global, "collect", Value::Obj(collect));
        eval(
            &mut engine,
            r#"
            function make(seed) {
                function leaf(holder) {
                    if(holder.drop) {holder.fn=null; collect();}
                    if(holder.fail) throw new Error('expected');
                    return holder.value+seed;
                }
                function middle(holder) {return holder.fn(holder);}
                function invoke(holder) {return middle(holder);}
                return {leaf:leaf,middle:middle,invoke:invoke};
            }
            var first=make(3);
            function warm() {
                var holder={fn:first.leaf,value:4,drop:false,fail:false};
                for(var n=0;n<500;n++) if(first.invoke(holder)!==7) throw 'warm';
            }
            warm();
            var next=make(20);
            var holder={fn:next.leaf,drop:false,fail:false};
            Object.defineProperty(holder,'value',{get:function inspectValue(){
                if(inspectValue.caller!==holder.expected) throw 'getter caller';
                if(holder.expected.caller!==next.middle) throw 'nested caller';
                return 4;
            }});
            holder.expected=next.leaf;
            for(var n=0;n<10;n++) if(next.invoke(holder)!==24) throw 'fresh';
            holder.fail=true;
            var caught=false;
            try {next.invoke(holder);} catch(e) {
                caught=e.message==='expected' && e.stack.indexOf('leaf')>=0 && e.stack.indexOf('middle')>=0;
            }
            if(!caught || next.leaf.caller!==null || next.middle.caller!==null) throw 'unwind';
            holder.fail=false;
            if(next.invoke(holder)!==24) throw 'after catch';
            var doomed=next.leaf;
            'passed'
        "#,
        );
        assert!(HITS.with(|hits| hits.get()) > 0);
        let doomed = engine
            .interp
            .global
            .borrow()
            .props
            .get("doomed")
            .unwrap()
            .value();
        let weak = Rc::downgrade(doomed.as_obj().unwrap());
        drop(doomed);
        eval(
            &mut engine,
            r#"
            next.leaf=null; doomed=null;
            Object.defineProperty(holder,'expected',{value:null});
            var gcHolder={fn:holder.fn,drop:true,fail:false}; holder.fn=null;
            Object.defineProperty(gcHolder,'value',{get:function inspectLive(){
                if(inspectLive.caller===null || inspectLive.caller.caller!==next.middle) throw 'collected active callee';
                return 4;
            }});
            if(next.invoke(gcHolder)!==24) throw 'GC result';
            'passed'
        "#,
        );
        engine.interp.gc_collect();
        assert!(weak.upgrade().is_none(), "hidden owner survived return");
    }

    #[test]
    fn fresh_method_receiver_and_missing_arguments_match_the_generic_binding() {
        let _enabled = Enabled::new();
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r#"
            function make(seed) {
                function leaf(x,missing) {return seed+this.bias+x+(missing===undefined?0:100);}
                function invoke(holder,x) {return holder.fn(x);}
                return {leaf:leaf,invoke:invoke};
            }
            var first=make(3), holder={fn:first.leaf,bias:10};
            function warm(){for(var n=0;n<500;n++) if(first.invoke(holder,4)!==17) throw 'warm';}
            warm(); 'passed'
        "#,
        );
        assert_optimized(&engine);
        for tier in [Tier::Jit, Tier::Bytecode] {
            engine.set_tier(tier);
            HITS.with(|hits| hits.set(0));
            eval(
                &mut engine,
                r#"
                var next=make(20), holder={fn:next.leaf,bias:30};
                if(next.invoke(holder,4)!==54) throw 'fresh receiver';
                Number.prototype.fn=next.leaf; Number.prototype.bias=40;
                if(next.invoke(7,4)!==64) throw 'boxed receiver';
                'passed'
            "#,
            );
            assert_eq!(
                HITS.with(|hits| hits.get()),
                1,
                "primitive receiver entered inline"
            );
        }
    }
}
