//! Elide a yielded Array Iterator result while retaining the real iterator for close.
use crate::{
    interpreter::Interp,
    value::{Exotic, Value},
};
use std::rc::Rc;

const ENABLED_NEXT: &str = "%ArrayIteratorStepIntrinsic%";

/// Independent realm-time selection; Array destructuring may be disabled separately.
pub(crate) fn install(i: &mut Interp) {
    if std::env::var_os("LUMEN_NO_ARRAY_ITERATOR_STEP").is_none() {
        if let Some(next) = super::array_destructure::original_next(i).cloned() {
            i.extra_protos.insert(ENABLED_NEXT, next);
        }
    }
}

/// Some is a yielded owned value, including Undefined. None has no visible effects:
/// execute the original iterator_step. Exhaustion deliberately remains on that path.
pub(super) fn try_yield(i: &Interp, iterator: &Value, captured_next: &Value) -> Option<Value> {
    let expected = i.extra_protos.get(ENABLED_NEXT)?;
    if !matches!(captured_next, Value::Obj(actual) if Rc::ptr_eq(actual, expected)) {
        return None;
    }
    let Value::Obj(object) = iterator else {
        return None;
    };
    let (target, index) = {
        let b = object.borrow();
        if !matches!(b.exotic, Exotic::None) || !b.ic_plain.get() {
            return None;
        }
        let kind = b.props.get("__ai_kind")?;
        if kind.accessor() || !matches!(kind.value(), Value::Num(0.0)) {
            return None;
        }
        let p = b.props.get("__ai_index")?;
        if p.accessor() || !p.writable() {
            return None;
        }
        let Value::Num(index) = p.value() else {
            return None;
        };
        if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index >= u32::MAX as f64 {
            return None;
        }
        let p = b.props.get("__ai_target")?;
        if p.accessor() {
            return None;
        }
        let Value::Obj(target) = p.value() else {
            return None;
        };
        (target, index as u32)
    };
    let value = own_array_value(&target, index)?;
    // All preflight is pure; no user code or GC occurred. Recheck on the mutable
    // borrow anyway, and never fall back after replacing this Number with Number.
    {
        let mut b = object.borrow_mut();
        if !matches!(b.exotic, Exotic::None) || !b.ic_plain.get() {
            return None;
        }
        let p = b.props.get_mut("__ai_index")?;
        if p.accessor() || !p.writable() || !matches!(p.value(), Value::Num(_)) {
            return None;
        }
        p.set_value(Value::Num(f64::from(index) + 1.0));
    }
    #[cfg(test)]
    SUCCESSES.with(|n| n.set(n.get() + 1));
    Some(value)
}

fn own_array_value(target: &crate::value::Gc, index: u32) -> Option<Value> {
    let b = target.borrow();
    if !matches!(b.exotic, Exotic::Array) || !b.ic_plain.get() {
        return None;
    }
    let length = b.props.get("length")?;
    if length.accessor() {
        return None;
    }
    let Value::Num(length) = length.value() else {
        return None;
    };
    if !length.is_finite()
        || length.fract() != 0.0
        || length > u32::MAX as f64
        || f64::from(index) >= length
    {
        return None;
    }
    let element = b.props.get_index(index)?;
    if element.accessor() {
        return None;
    }
    let value = element.value();
    if matches!(value, Value::Empty) {
        return None;
    }
    Some(value)
}

#[cfg(test)]
thread_local! { static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, hits: usize) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let source = format!("function assert(v){{if(!v)throw new Error('iterator yielded step');}} {source}; 'passed'");
            match engine.eval(&source, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            let expected = if tier == Tier::Interp
                || std::env::var_os("LUMEN_NO_ARRAY_ITERATOR_STEP").is_some()
            {
                0
            } else {
                hits
            };
            assert_eq!(
                super::SUCCESSES.with(|n| n.get()),
                expected,
                "{tier:?}: yielded successes"
            );
        }
    }

    #[test]
    fn dense_values_keep_captured_next_after_prototype_replacement() {
        check(
            r#"
            var proto=Object.getPrototypeOf([].values()),saved=proto.next,custom=0;
            function scan(a){var sum=0;for(var x of a){
                sum+=x;proto.next=function(){custom++;return {done:true};};
            }return sum;}
            assert(scan([1,2,3])===6&&custom===0);proto.next=saved;
        "#,
            3,
        );
    }

    #[test]
    fn body_growth_shrink_holes_and_proxy_use_each_steps_live_state() {
        check(
            r#"
            function changing(a){var sum=0;for(var x of a){sum+=x;if(x===1)a.push(4);if(x===2)a.length=2;}return sum;}
            assert(changing([1,2,3])===3);
            function sum(a){var total=0;for(var x of a)total+=x;return total;}
            var a=[,2],reads=0,parent=Object.create(Array.prototype);
            Object.defineProperty(parent,'0',{get:function(){reads++;return 5;}});Object.setPrototypeOf(a,parent);
            assert(sum(a)===7&&reads===1);
            var traps=0,proxy=new Proxy([3,4],{get:function(t,k){traps++;return t[k];}});
            assert(sum(proxy)===7&&traps>0);
        "#,
            3,
        );
    }

    #[test]
    fn close_and_abort_observe_real_iterator_and_new_return_method() {
        check(
            r#"
            var iter,source,closed=0,seen;
            function scan(fail){for(var x of source){
                iter.return=function(){closed++;seen=this;return {};};
                if(fail)throw 'original';break;
            }}
            iter=[1,2].values();source={ [Symbol.iterator]:function(){return iter;} };
            scan(false);assert(closed===1&&seen===iter&&iter.__ai_index===1);
            iter=[3,4].values();var error='';try{scan(true);}catch(e){error=e;}
            assert(error==='original'&&closed===2&&seen===iter&&iter.__ai_index===1);
        "#,
            2,
        );
    }

    #[test]
    fn readonly_state_and_length_getter_fallback_do_not_replay_steps() {
        check(
            r#"
            var iter=[1,2,3].values(),source={ [Symbol.iterator]:function(){return iter;} };
            function scan(a){var sum=0,n=0;for(var x of a){
                sum+=x;n++;if(n===1)Object.defineProperty(iter,'__ai_index',{writable:false});if(n===2)break;
            }return sum;}
            assert(scan(source)===3&&iter.__ai_index===1);
            var lengths=0,target={0:7,get length(){lengths++;return 1;}};
            iter=Array.prototype.values.call(target);
            function drain(a){var sum=0;for(var x of a)sum+=x;return sum;}
            assert(drain(source)===7&&lengths===2);
        "#,
            1,
        );
    }

    #[test]
    fn undefined_yields_and_aliased_objects_survive_gc_before_exhaustion() {
        check(
            r#"
            function scan(a){var count=0,seen;for(var x of a){
                count++;$262.gc();
                if(count===1)assert(x===undefined);
                else if(count===2){seen=x;assert(x.value===9);}
                else assert(x===seen&&x.value===9);
            }return count;}
            var object={value:9};assert(scan([undefined,object,object])===3);
        "#,
            3,
        );
    }

    #[test]
    fn body_installed_element_getter_falls_back_once_then_resumes_fast_steps() {
        check(
            r#"
            var reads=0;
            function scan(a){var sum=0;for(var x of a){
                sum+=x;if(x===1)Object.defineProperty(a,'1',{get:function(){reads++;$262.gc();return 2;},configurable:true});
            }return sum;}
            assert(scan([1,2,3,4])===10&&reads===1);
        "#,
            3,
        );
    }

    #[test]
    fn foreign_intrinsic_captured_next_uses_the_checked_path() {
        check(
            r#"
            var other=$262.createRealm().global;
            var foreignNext=other.eval('Object.getPrototypeOf([].values()).next');
            var localNext=Object.getPrototypeOf([].values()).next;assert(foreignNext!==localNext);
            var iterator=[3,4].values();iterator.next=foreignNext;
            var source={ [Symbol.iterator]:function(){return iterator;} };
            function scan(a){var sum=0;for(var x of a)sum+=x;return sum;}
            assert(scan(source)===7&&iterator.__ai_index===2);
        "#,
            0,
        );
    }

    #[test]
    fn configurable_index_setter_runs_once_then_data_state_resumes_fast() {
        check(
            r#"
            var originalNext=Object.getPrototypeOf([].values()).next;
            var iterator={__ai_kind:0,__ai_index:0,__ai_target:[1,2,3],next:originalNext};
            var source={ [Symbol.iterator]:function(){return iterator;} },backing=1,sets=0,gets=0;
            function scan(a){var sum=0;for(var x of a){
                sum+=x;
                if(x===1)Object.defineProperty(iterator,'__ai_index',{
                    get:function(){gets++;return backing;},
                    set:function(v){sets++;backing=v;},configurable:true
                });
                if(x===2)Object.defineProperty(iterator,'__ai_index',{value:backing,writable:true,configurable:true});
            }return sum;}
            assert(scan(source)===6&&sets===1&&gets===1&&iterator.__ai_index===3);
        "#,
            2,
        );
    }
}
