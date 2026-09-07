//! Bypass a temporary method owner when an adjacent zero-argument inline guard succeeds.
use super::{asm::Asm, C_NE};
use crate::bytecode::{Chunk, Op};
use crate::value::{Gc, JitLayout, PACK_OBJ};

pub(super) struct Guard {
    packed_callee: u64,
    expected_env: usize,
    check_this: bool,
    continuation: usize,
}

pub(super) fn plan(
    chunk: &Chunk,
    pc: usize,
    labels: &[usize],
    layout: &JitLayout,
) -> Option<Guard> {
    if !layout.valid
        || layout.entry_accessor != layout.entry_value + 8
        || std::env::var_os("LUMEN_JIT_NO_INLINE_METHOD").is_some()
    {
        return None;
    }
    let [Op::GetMethod(..), Op::InlineGuard(target, _), Op::Pop] =
        chunk.jit_ops().get(pc..pc + 3)?
    else {
        return None;
    };
    let target = chunk.jit_inline_target(*target);
    if target.argc != 0 {
        return None;
    }
    let owner: Option<Gc> = Some(target.pin.upgrade()?);
    // Same probed stored-Rc representation used by the ordinary InlineGuard emitter.
    let stored = unsafe { *(&owner as *const Option<Gc> as *const usize) };
    Some(Guard {
        packed_callee: PACK_OBJ | stored as u64,
        expected_env: target.expected_env,
        check_this: target.check_this,
        continuation: *labels.get(pc + 3)?,
    })
}

/// Called after the ordinary live property/prototype and data-descriptor guards. No owners
/// have changed. A miss falls through to the original decoder, guard and Pop templates; a hit
/// has exactly their final stack (only the receiver), without cloning and dropping the method.
pub(super) fn emit(a: &mut Asm, guard: &Guard, value_offset: i32) {
    let ordinary = a.new_label();
    if guard.expected_env != 0 {
        a.ldr_imm(9, 19, 40);
        a.mov_imm64(12, guard.expected_env as u64);
        a.cmp_reg_x(9, 12);
        a.b_cond(C_NE, ordinary);
    }
    if guard.check_this {
        a.ldurb(9, 20, -16);
        a.cmp_imm_w(9, 8);
        a.b_cond(C_NE, ordinary);
    }
    a.ldur(9, 15, value_offset);
    a.mov_imm64(12, guard.packed_callee);
    a.cmp_reg_x(9, 12);
    a.b_cond(C_NE, ordinary);
    #[cfg(test)]
    record_success(a);
    a.b(guard.continuation);
    a.bind(ordinary);
}

#[cfg(test)]
thread_local! {
    static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_success(a: &mut Asm) {
    // Engine and installed chunks are !Send; emitted code runs on this TLS cell's thread.
    a.mov_imm64(9, SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(12, 9, 0);
    a.add_imm(12, 12, 1);
    a.str_imm(12, 9, 0);
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let script = format!(
                "function assert(v){{if(!v)throw new Error('inline method');}} {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                assert!(
                    super::SUCCESSES.with(|n| n.get() > 0),
                    "fusion never executed"
                );
            }
        }
    }

    #[test]
    fn live_method_replacement_accessors_and_prototypes() {
        check(
            r#"
            function first(){return this.value;}
            function second(){return this.value+1;}
            function methodSite(o){return o.method();}
            function invoke(o){return methodSite(o);}
            var proto={method:first}, o=Object.create(proto); o.value=7;
            for(var i=0;i<600;i++)assert(invoke(o)===7);
            proto.method=second; assert(invoke(o)===8);
            o.method=first; assert(invoke(o)===7);
            var reads=0;
            Object.defineProperty(o,'method',{configurable:true,get:function(){reads++;return second;}});
            assert(invoke(o)===8 && reads===1);
            delete o.method;
            Object.setPrototypeOf(o,{method:first}); assert(invoke(o)===7);
            var proxy=new Proxy(o,{get:function(t,k,r){reads++;return Reflect.get(t,k,r);}});
            assert(invoke(proxy)===7 && reads===3);
        "#,
        );
    }

    #[test]
    fn closures_and_primitive_receivers_keep_their_environment_and_this() {
        check(
            r#"
            function factory(n){return function(){return n;};}
            function methodSite(o){return o.method();}
            function invoke(o){return methodSite(o);}
            function first(){return this.value;}
            var a={method:first,value:3}, b={method:factory(9)};
            for(var i=0;i<600;i++)assert(invoke(a)===3);
            a.method=factory(3); assert(invoke(a)===3);
            assert(invoke(b)===9);
            a.method=b.method; assert(invoke(a)===9);
            function strictValue(){'use strict';return this;}
            String.prototype.method=strictValue;
            assert(invoke('text')==='text');
            function boxedValue(){return typeof this;}
            String.prototype.method=boxedValue;
            assert(invoke('text')==='object');
            delete String.prototype.method;
        "#,
        );
    }

    #[test]
    fn inlined_method_remains_live_through_collection_after_its_property_is_deleted() {
        check(
            r#"
            function method(){
                if(this.drop){delete this.method; $262.gc();}
                return this.value;
            }
            function methodSite(o){return o.method();}
            function invoke(o){return methodSite(o);}
            var o={method:method,value:17,drop:false};
            for(var i=0;i<600;i++)assert(invoke(o)===17);
            method=null; o.drop=true; assert(invoke(o)===17);
            assert(!Object.prototype.hasOwnProperty.call(o,'method'));
        "#,
        );
    }
}
