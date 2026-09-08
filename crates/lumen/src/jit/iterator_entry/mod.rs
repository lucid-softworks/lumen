//! Native guarded iterator prefix. The caller owns the logical call boundary.
use crate::{
    bytecode::Chunk,
    interpreter::{Env, Interp},
    jit_ir::iterator_entry::Candidate,
    value::Value,
};
use std::rc::Rc;

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod element;
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod emit;
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod names;
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod values;

pub(crate) struct Entry {
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ))]
    native: emit::Native,
}

pub(crate) fn compile(plan: &Candidate, chunk: &Rc<Chunk>, env: &Env) -> Option<Entry> {
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    ))]
    {
        emit::compile(plan, chunk, env).map(|native| Entry { native })
    }
    #[cfg(not(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux", target_os = "windows")
    )))]
    {
        let _ = (plan, chunk, env);
        None
    }
}

impl Entry {
    /// Caller has performed the normal depth/GC poll and validated live callee,
    /// selected code version, realm and no-activation eligibility. Env and iterator
    /// remain owned; no callback or GC may overlap this native invocation.
    pub(crate) unsafe fn run(
        &self,
        interp: &mut Interp,
        env: &Env,
        iterator: &Value,
    ) -> Option<Value> {
        #[cfg(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        ))]
        {
            self.native.run(interp, env, iterator)
        }
        #[cfg(not(all(
            target_arch = "aarch64",
            any(target_os = "macos", target_os = "linux", target_os = "windows")
        )))]
        {
            let _ = (interp, env, iterator);
            None
        }
    }
}

#[cfg(all(
    test,
    target_arch = "aarch64",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use super::*;
    use crate::{bytecode::Tier, jit_ir::iterator_entry, value::Callable, Engine};
    fn run(engine: &mut Engine, source: &str) {
        let completion = engine.eval(source, false).expect("valid script");
        assert!(
            matches!(completion, crate::Completion::Value(_)),
            "unexpected JavaScript completion"
        );
    }
    fn fixture(body: &str) -> (Engine, Entry, Env, Value) {
        let mut engine = Engine::new();
        engine.set_tier(Tier::Bytecode);
        engine.set_tier_threshold(0);
        let source = r#"
            function make(){let state={index:0,items:[{x:42},8]};
                return {state:state,next:function(){$BODY}};}
            var iterator=make();iterator.next();iterator.state.index=0;
        "#
        .replace("$BODY", body);
        run(&mut engine, &source);
        let global = Value::Obj(engine.interp.global.clone());
        let iterator = engine
            .interp
            .get_member(&global, "iterator")
            .unwrap_or_else(|_| panic!("iterator"));
        let next = engine
            .interp
            .get_member(&iterator, "next")
            .unwrap_or_else(|_| panic!("next"));
        let Value::Obj(next) = next else {
            panic!("object")
        };
        let (chunk, env) = {
            let object = next.borrow();
            let Callable::User(user) = &object.call else {
                panic!("user")
            };
            (
                user.func
                    .code
                    .get()
                    .and_then(|c| c.clone())
                    .expect("compiled next"),
                user.env.clone(),
            )
        };
        let plan = iterator_entry::analyze(&chunk).expect("entry plan");
        let entry = compile(&plan, &chunk, &env).expect("native code");
        (engine, entry, env, iterator)
    }
    #[test]
    fn machine_code_forwards_numeric_store_and_preserves_ieee_rounding() {
        let (mut engine, entry, env, iterator) =
            fixture("state.index=state.index+1;return {value:state.index-1,done:false};");
        run(&mut engine, "iterator.state.index=9007199254740992;");
        let value =
            unsafe { entry.run(&mut engine.interp, &env, &iterator) }.expect("native success");
        assert!(matches!(value, Value::Num(9007199254740991.0)));
        run(
            &mut engine,
            "if(iterator.state.index!==9007199254740992)throw 'rounding';iterator.state.index=NaN;",
        );
        assert!(
            matches!(unsafe{entry.run(&mut engine.interp,&env,&iterator)},Some(Value::Num(n)) if n.is_nan())
        );
        run(
            &mut engine,
            "if(!Number.isNaN(iterator.state.index))throw 'packed NaN';",
        );
    }
    #[test]
    fn machine_code_returns_owned_dense_object_and_declines_before_store() {
        let (mut engine,entry,env,iterator)=fixture("if(state.index<state.items.length){state.index=state.index+1;return {value:state.items[state.index-1],done:false};}return {done:true};");
        let value = unsafe { entry.run(&mut engine.interp, &env, &iterator) }
            .expect("native dense success");
        let Value::Obj(owner) = value else {
            panic!("object result")
        };
        run(&mut engine, "iterator.state.items=[];$262.gc();");
        assert!(matches!(
            owner.borrow().props.get("x").unwrap().value(),
            Value::Num(42.0)
        ));
        assert!(unsafe { entry.run(&mut engine.interp, &env, &iterator) }.is_none());
        run(
            &mut engine,
            "if(iterator.state.index!==1)throw 'miss changed index';",
        );
    }
    #[test]
    fn machine_code_negative_zero_and_branch_miss() {
        let (mut engine, entry, env, iterator) = fixture(
            "if(state.index<2){return {value:state.index*0,done:false};}return {done:true};",
        );
        run(&mut engine, "iterator.state.index=-1;");
        assert!(
            matches!(unsafe{entry.run(&mut engine.interp,&env,&iterator)},Some(Value::Num(n)) if n==0.0 && n.is_sign_negative())
        );
        run(&mut engine, "iterator.state.index=NaN;");
        assert!(unsafe { entry.run(&mut engine.interp, &env, &iterator) }.is_none());
    }
    #[test]
    fn generated_poststore_guards_leave_state_unchanged() {
        let body="if(state.index<state.items.length){state.index=state.index+1;return {value:state.items[state.index-1],done:false};}return {done:true};";
        for mutation in [
            "Object.defineProperty(iterator.state,'index',{writable:false});",
            "Object.defineProperty(iterator.state.items,'0',{get:function(){throw 'element getter';}});",
            "delete iterator.state.items[0];",
            "iterator.state.items=new Proxy(iterator.state.items,{get:function(){throw 'proxy getter';}});",
        ] {
            let (mut engine,entry,env,iterator)=fixture(body);
            run(&mut engine,mutation);
            assert!(unsafe{entry.run(&mut engine.interp,&env,&iterator)}.is_none(),"{mutation}");
            run(&mut engine,"if(iterator.state.index!==0)throw 'partial native store';");
        }
        // Reaching this accessor requires evaluating the read after the proposed
        // write. Its rejection must neither invoke it nor publish that write.
        let (mut engine, entry, env, iterator) =
            fixture("state.index=state.index+1;return {value:state.items,done:false};");
        run(&mut engine,"Object.defineProperty(iterator.state,'items',{get:function(){throw 'postwrite getter';}});");
        assert!(unsafe { entry.run(&mut engine.interp, &env, &iterator) }.is_none());
        run(
            &mut engine,
            "if(iterator.state.index!==0)throw 'accessor partial store';",
        );
    }

    #[test]
    fn generated_same_shape_distinct_entry_is_not_forwarded() {
        // holder supplies the same property spelling on a second object with the
        // same shape. Physical entry identity, not shape/key alone, chooses forwarding.
        let (mut engine, entry, env, iterator) =
            fixture("state.index=state.index+1;return {value:this.state.index,done:false};");
        let original = iterator.clone();
        run(
            &mut engine,
            "var alternate={state:{index:17,items:[]},next:iterator.next};",
        );
        let global = Value::Obj(engine.interp.global.clone());
        let alternate = engine
            .interp
            .get_member(&global, "alternate")
            .unwrap_or_else(|_| panic!("alternate"));
        let result = unsafe { entry.run(&mut engine.interp, &env, &alternate) };
        assert!(matches!(result, Some(Value::Num(17.0))));
        run(
            &mut engine,
            "if(iterator.state.index!==1 || alternate.state.index!==17)throw 'distinct alias';",
        );
        assert!(matches!(
            unsafe { entry.run(&mut engine.interp, &env, &original) },
            Some(Value::Num(2.0))
        ));
    }
}
