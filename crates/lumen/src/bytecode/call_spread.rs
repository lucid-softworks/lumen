//! Scalar replacement for an effect-free, bounded Array spread iterator walk.
use super::array_destructure::{original_next, original_values};
use crate::{
    interpreter::{Abrupt, Interp, MAX_EVAL_DEPTH},
    value::{Exotic, Gc, Value},
};
use std::rc::Rc;

const LIMIT: usize = 16;

/// Return owned values only when the complete iterator walk is proven unobservable.
///
/// `None` is a pure miss: the caller must run the existing iterator protocol. A hit
/// retains the logical native-call boundaries for Array.prototype.values and every
/// Array Iterator `next`, including the final done step.
pub(super) fn try_dense(i: &mut Interp, input: &Value) -> Result<Option<Vec<Value>>, Abrupt> {
    let Some(values) = original_values(i) else {
        return Ok(None);
    };
    let Some(next) = original_next(i) else {
        return Ok(None);
    };
    let Value::Obj(array) = input else {
        return Ok(None);
    };
    let Some(len) = array_length(array) else {
        return Ok(None);
    };
    if len > LIMIT {
        return Ok(None);
    }

    let Some(iterator_symbol) = i.iterator_sym.as_ref() else {
        return Ok(None);
    };
    let key = Interp::sym_key(iterator_symbol);
    if !matches!(plain_lookup(array, &key), Some(Some(actual)) if identity(&actual, values)) {
        return Ok(None);
    }
    let Some(iterator_proto) = i.extra_protos.get("%ArrayIteratorPrototype%") else {
        return Ok(None);
    };
    if !matches!(plain_lookup(iterator_proto, "next"), Some(Some(actual)) if identity(&actual, next))
    {
        return Ok(None);
    }

    let outputs = {
        let b = array.borrow();
        let mut outputs = Vec::with_capacity(len);
        // Validate the entire walk before cloning anything. Holes and accessors can
        // consult the prototype chain or execute JS, so they stay on the slow path.
        for index in 0..len {
            let Some(property) = b.props.get_index(index as u32) else {
                return Ok(None);
            };
            if property.accessor() {
                return Ok(None);
            }
        }
        for index in 0..len {
            outputs.push(b.props.get_index(index as u32).unwrap().value());
        }
        outputs
    };

    // The replaced protocol calls values once, then next once per element and once
    // more to observe done. Preserve recursion-limit checks and amortized GC polls.
    for _ in 0..len + 2 {
        poll_elided_native_call(i)?;
    }

    #[cfg(test)]
    SUCCESSES.with(|n| n.set(n.get() + 1));
    Ok(Some(outputs))
}

fn poll_elided_native_call(i: &mut Interp) -> Result<(), Abrupt> {
    i.depth += 1;
    if i.depth > MAX_EVAL_DEPTH {
        i.depth -= 1;
        return Err(i.throw("RangeError", "Maximum call stack size exceeded"));
    }
    let result = i.gc_check_amortized();
    i.depth -= 1;
    result
}

fn array_length(array: &Gc) -> Option<usize> {
    let b = array.borrow();
    if !matches!(b.exotic, Exotic::Array) || !b.ic_plain.get() {
        return None;
    }
    let property = b.props.get("length")?;
    if property.accessor() {
        return None;
    }
    let Value::Num(length) = property.value() else {
        return None;
    };
    if !length.is_finite() || length < 0.0 || length.fract() != 0.0 || length > u32::MAX as f64 {
        return None;
    }
    Some(length as usize)
}

fn identity(actual: &Value, expected: &Gc) -> bool {
    matches!(actual, Value::Obj(object) if Rc::ptr_eq(object, expected))
}

/// Outer None means unsafe lookup; inner None means proven absent through null.
fn plain_lookup(start: &Gc, name: &str) -> Option<Option<Value>> {
    let mut current = Some(start.clone());
    for _ in 0..8 {
        let Some(object) = current else {
            return Some(None);
        };
        let b = object.borrow();
        if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None | Exotic::Array) {
            return None;
        }
        if let Some(property) = b.props.get(name) {
            return if property.accessor() {
                None
            } else {
                Some(Some(property.value()))
            };
        }
        current = b.proto.clone();
    }
    None
}

#[cfg(test)]
thread_local! { static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, interpreter::MAX_EVAL_DEPTH, value::Value, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let compiled = tier != Tier::Interp;
            let script = format!(
                "function assert(v,n){{if(!v)throw new Error('call spread '+n);}} var compiled={compiled}; {source}; 'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            let hits = super::SUCCESSES.with(|n| n.get());
            if tier == Tier::Jit {
                assert!(
                    hits > 0,
                    "JIT should scalar-replace at least one dense spread"
                );
            } else {
                assert_eq!(hits, 0, "{tier:?} must keep its reference path");
            }
        }
    }

    #[test]
    fn dense_values_retain_logical_call_boundaries() {
        let mut engine = Engine::new();
        let input =
            engine
                .interp
                .make_array(vec![Value::Num(2.0), Value::Num(3.0), Value::Num(5.0)]);
        engine.interp.gc_tick = 0;
        let values = match super::try_dense(&mut engine.interp, &input) {
            Ok(Some(values)) => values,
            _ => panic!("dense array should pass guarded spread expansion"),
        };
        assert_eq!(values.len(), 3);
        assert_eq!(engine.interp.gc_tick, 5);
        assert_eq!(engine.interp.depth, 0);

        engine.interp.depth = MAX_EVAL_DEPTH;
        engine.interp.gc_tick = 0;
        assert!(super::try_dense(&mut engine.interp, &input).is_err());
        assert_eq!(engine.interp.depth, MAX_EVAL_DEPTH);
        assert_eq!(engine.interp.gc_tick, 0);
    }

    #[test]
    fn dense_spread_preserves_target_and_argument_semantics() {
        check(
            r#"
            var order=[];
            function first(){order.push('plain');return 1;}
            function spread(){order.push('spread');return [2,3];}
            function target(a,b,c){
                order.push('target');
                assert(this.marker===9);
                assert(a===1&&b===2&&c===3);
                return a+b+c;
            }
            var receiver={marker:9,target:target};
            function invoke(receiver){return receiver.target(first(),...spread());}
            assert(invoke(receiver)===6);
            assert(order.join(',')==='plain,spread,target');

            var shared={value:7};
            function owned(a,b){$262.gc();return a===b&&a.value===7;}
            function invokeOwned(a){return owned(...a);}
            assert(invokeOwned([shared,shared]));

            var trapped=0;
            var proxy=new Proxy(target,{apply:function(fn,self,args){trapped++;return 12;}});
            function invokeProxy(a){return proxy(...a);}
            assert(invokeProxy([1,2,3])===12&&trapped===1);

            function throws(){throw new Error('spread-target');}
            function invokeThrows(a){return throws(...a);}
            var caught='';try{invokeThrows([]);}catch(e){caught=e.message;}
            assert(caught==='spread-target');
            "#,
        );
    }

    #[test]
    fn observable_iterators_elements_proxies_and_realms_fall_back() {
        check(
            r#"
            function sum(a,b){return a+b;}
            function invoke(a){return sum(...a);}
            assert(invoke([2,3])===5,1);
            var log=[];
            var originalValues=Array.prototype[Symbol.iterator];
            Object.defineProperty(Array.prototype,Symbol.iterator,{configurable:true,writable:true,value:function(){
                log.push('iterator');var done=false;
                return {next:function(){log.push('next');if(done)return {done:true};done=true;return {done:false,value:8};}};
            }});
            var customResult=invoke([2,3]);
            assert(compiled?customResult!==5&&log.join(',')==='iterator,next,next':customResult===5&&log.length===0,2);
            Object.defineProperty(Array.prototype,Symbol.iterator,{configurable:true,writable:true,value:originalValues});

            var iteratorProto=Object.getPrototypeOf([].values());
            var originalNext=iteratorProto.next,steps=0;
            iteratorProto.next=function(){steps++;return originalNext.call(this);};
            assert(invoke([2,3])===5&&steps===(compiled?3:0),3);
            iteratorProto.next=originalNext;

            var reads=0,a=[2,3];
            Object.defineProperty(a,'0',{get:function(){reads++;return 4;}});
            assert(invoke(a)===7&&reads===1,4);
            var parent=Object.create(Array.prototype);
            Object.defineProperty(parent,'0',{get:function(){reads++;return 6;}});
            a=[,3];Object.setPrototypeOf(a,parent);
            assert(invoke(a)===9&&reads===2,5);

            var traps=0,p=new Proxy([2,3],{get:function(t,k){traps++;return t[k];}});
            assert(invoke(p)===5&&traps>0,6);
            var realm=$262.createRealm(),foreign=realm.global.Array(2,3);
            assert(invoke(foreign)===5,7);
            "#,
        );
    }
}
