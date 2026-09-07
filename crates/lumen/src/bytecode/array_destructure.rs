//! Scalar replacement of an effect-free, bounded Array binding iterator walk.
use crate::{
    interpreter::Interp,
    value::{Exotic, Gc, Value},
};
use std::rc::Rc;

// extra_protos is already rooted, saved and restored with the active realm. These
// private entries retain original method identities even after JS replaces properties.
const VALUES: &str = "%DestructureArrayValuesIntrinsic%";
const NEXT: &str = "%DestructureArrayNextIntrinsic%";
const LIMIT: usize = 16;

pub(super) fn original_next(i: &Interp) -> Option<&Gc> {
    i.extra_protos.get(NEXT)
}

pub(crate) fn remember_next(i: &mut Interp, proto: &Gc) {
    if let Some(Value::Obj(next)) = proto.borrow().props.get("next").map(|p| p.value()) {
        i.extra_protos.insert(NEXT, next);
    }
}

/// Realm initialization only; disabled realms never enter the scalar replacement.
pub(crate) fn remember_values(i: &mut Interp, values: &Value) {
    if std::env::var_os("LUMEN_NO_ARRAY_DESTRUCTURE").is_none() {
        if let Value::Obj(values) = values {
            i.extra_protos.insert(VALUES, values.clone());
        }
    }
}

/// None is a pure miss: the caller still owns the original input, and must run
/// the complete existing DestructureArr opcode, including its IteratorClose.
pub(super) fn try_dense(i: &Interp, input: &Value, count: u16) -> Option<Vec<Value>> {
    let count = usize::from(count);
    if count > LIMIT {
        return None;
    }
    let values = i.extra_protos.get(VALUES)?;
    let next = i.extra_protos.get(NEXT)?;
    let Value::Obj(array) = input else {
        return None;
    };
    let len = array_length(array)?;
    let key = Interp::sym_key(i.iterator_sym.as_ref()?);
    if !identity(plain_lookup(array, &key)??, values) {
        return None;
    }
    let proto = i.extra_protos.get("%ArrayIteratorPrototype%")?;
    if !identity(plain_lookup(proto, "next")??, next) {
        return None;
    }
    // Exactly count==len still closes: no subsequent exhausted step occurred.
    if count <= len
        && !matches!(
            plain_lookup(proto, "return")?,
            None | Some(Value::Undefined | Value::Null)
        )
    {
        return None;
    }
    let b = array.borrow();
    let yielded = count.min(len);
    // Guard every read before cloning outputs; holes/inherited/accessor elements
    // fall back, since any one can execute JS and invalidate earlier proofs.
    for index in 0..yielded {
        if b.props.get_index(index as u32)?.accessor() {
            return None;
        }
    }
    let mut outputs = Vec::with_capacity(count);
    for index in 0..yielded {
        outputs.push(b.props.get_index(index as u32)?.value());
    }
    outputs.resize(count, Value::Undefined);
    #[cfg(test)]
    SUCCESSES.with(|n| n.set(n.get() + 1));
    Some(outputs)
}

fn array_length(array: &Gc) -> Option<usize> {
    let b = array.borrow();
    if !matches!(b.exotic, Exotic::Array) || !b.ic_plain.get() {
        return None;
    }
    let p = b.props.get("length")?;
    if p.accessor() {
        return None;
    }
    let Value::Num(n) = p.value() else {
        return None;
    };
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
        return None;
    }
    Some(n as usize)
}

fn identity(actual: Value, expected: &Gc) -> bool {
    matches!(actual, Value::Obj(o) if Rc::ptr_eq(&o, expected))
}

/// Outer None means unsafe lookup; inner None means proven absent through null.
fn plain_lookup(start: &Gc, name: &str) -> Option<Option<Value>> {
    let mut current = Some(start.clone());
    for _ in 0..8 {
        let Some(object) = current else {
            return Some(None);
        };
        let b = object.borrow();
        // All callers use named non-index keys (symbol iterator, next, return).
        // Arrays, including Array.prototype, have ordinary own lookup for these.
        if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None | Exotic::Array) {
            return None;
        }
        if let Some(p) = b.props.get(name) {
            return if p.accessor() {
                None
            } else {
                Some(Some(p.value()))
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
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, expected: usize) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let source = format!("function assert(v){{if(!v)throw new Error('dense destructure');}} {source}; 'passed'");
            match engine.eval(&source, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if tier != Tier::Interp && std::env::var_os("LUMEN_NO_ARRAY_DESTRUCTURE").is_none() {
                assert_eq!(
                    super::SUCCESSES.with(|n| n.get()),
                    expected,
                    "{tier:?}: actual scalar replacements"
                );
            } else {
                assert_eq!(super::SUCCESSES.with(|n| n.get()), 0);
            }
        }
    }

    #[test]
    fn zero_short_exact_and_long_patterns_copy_owned_outputs() {
        check(
            r#"
            function empty(a){var []=a;return 1;}
            function one(a){var [x]=a;return x;}
            function pair(a){var [x,y]=a;return x===undefined?0:y===undefined?x:x+y;}
            function three(a){var [x,y,z]=a;return z;}
            assert(empty([])===1&&empty([1])===1);
            assert(one([])===undefined&&one([3])===3&&one([3,4])===3);
            assert(pair([])===0&&pair([2])===2&&pair([2,3])===5&&pair([2,3,4])===5);
            assert(three([1,2])===undefined&&three([1,2,3])===3);
            function owned(a){var [x,y]=a;$262.gc();return x===y&&x.value===9;}
            var object={value:9};assert(owned([object,object]));
        "#,
            12,
        );
    }

    #[test]
    fn custom_next_iterator_and_return_keep_protocol_observations() {
        check(
            r#"
            function pair(a){var [x,y]=a;return x+y;}
            function three(a){var [x,y,z]=a;return z;}
            assert(pair([2,3])===5);
            var proto=Object.getPrototypeOf([].values()),originalNext=proto.next,steps=0,closed=0;
            proto.next=function(){steps++;return {done:false,value:7};};
            proto.return=function(){closed++;return {};};
            assert(pair([2,3])===14&&steps===2&&closed===1);
            proto.next=originalNext;
            assert(pair([2,3])===5&&closed===2);
            assert(three([2,3])===undefined&&closed===2);
            delete proto.return;
            var originalValues=Array.prototype.values,originalIterator=Array.prototype[Symbol.iterator];
            function custom(){return {next:function(){return {done:false,value:4};},return:function(){closed++;return {};}};}
            Array.prototype.values=custom;Array.prototype[Symbol.iterator]=custom;
            assert(pair([2,3])===8&&closed===3);
            Array.prototype.values=originalValues;Array.prototype[Symbol.iterator]=originalIterator;
            var getters=0,a=[2,3];Object.defineProperty(a,'0',{get:function(){getters++;return 5;}});
            assert(pair(a)===8&&getters===1);
            var proxy=new Proxy([2,3],{get:function(t,k){return t[k];}});assert(pair(proxy)===5);
        "#,
            2,
        );
    }

    #[test]
    fn getters_holes_and_empty_close_are_never_suppressed() {
        check(
            r#"
            function empty(a){var []=a;return 1;}
            function pair(a){var [x,y]=a;return x+y;}
            function three(a){var [x,y,z]=a;return z;}
            assert(pair([2,3])===5);
            var proto=Object.getPrototypeOf([].values()),closed=0;
            Object.defineProperty(proto,'return',{get:function(){closed++;return undefined;},configurable:true});
            assert(empty([])===1&&closed===1);
            assert(pair([2,3])===5&&closed===2);
            assert(three([2,3])===undefined&&closed===2);
            delete proto.return;
            var oldNext=proto.next,reads=0;
            Object.defineProperty(proto,'next',{get:function(){reads++;return oldNext;},configurable:true});
            assert(empty([])===1&&reads===1);
            Object.defineProperty(proto,'next',{value:oldNext,writable:true,configurable:true});
            var a=[2,3],iterator=Array.prototype[Symbol.iterator];
            Object.defineProperty(a,Symbol.iterator,{get:function(){reads++;return iterator;}});
            assert(pair(a)===5&&reads===2);
            a=[,3];var parent=Object.create(Array.prototype);
            Object.defineProperty(parent,'0',{get:function(){reads++;return 5;}});Object.setPrototypeOf(a,parent);
            assert(pair(a)===8&&reads===3);
        "#,
            2,
        );
    }
    #[test]
    fn foreign_intrinsics_and_invalid_close_keep_original_identity() {
        check(
            r#"
            function pair(a){var [x,y]=a;return x+y;}
            assert(pair([2,3])===5);
            var realm=$262.createRealm(),foreign=realm.global.Array(2,3);
            assert(pair(foreign)===5);
            var localIterator=Array.prototype[Symbol.iterator];
            Array.prototype[Symbol.iterator]=realm.global.Array.prototype[Symbol.iterator];
            assert(pair([2,3])===5);
            Array.prototype[Symbol.iterator]=localIterator;
            $262.gc();assert(pair([2,3])===5);
            var proto=Object.getPrototypeOf([].values()),closed=0;
            proto.return=function(){closed++;return 0;};
            var threw=false;try{pair([2,3]);}catch(e){threw=e.name==='TypeError';}
            assert(threw&&closed===1);delete proto.return;
            "#,
            2,
        );
    }

    #[test]
    fn explicit_undefined_and_skipped_elements_stay_distinct_from_holes() {
        check(
            r#"
            function pair(a){var [x,y]=a;return x===undefined&&y===3;}
            function skip(a){var [x,,z]=a;return x+z;}
            assert(pair([undefined,3]));assert(pair([,3]));
            assert(skip([2,99,3])===5);
            var reads=0,a=[2,99,3];
            Object.defineProperty(a,'1',{get:function(){reads++;return 99;}});
            assert(skip(a)===5&&reads===1);
            "#,
            2,
        );
    }
    #[test]
    fn element_getters_can_change_later_values_and_close_during_gc() {
        check(
            r#"
            var log='',proto=Object.getPrototypeOf([].values());
            function pair(a){var [x,y]=a;log+='body,';$262.gc();return x.v+y.v;}
            assert(pair([{v:2},{v:3}])===5);log='';
            var a=[0,0];Object.defineProperty(a,'0',{get:function(){
                log+='first,';
                Object.defineProperty(a,'1',{get:function(){log+='second,';$262.gc();return {v:7};},configurable:true});
                Object.defineProperty(proto,'return',{get:function(){
                    log+='return-get,';return function(){log+='return-call,';$262.gc();return {};};
                },configurable:true});
                $262.gc();return {v:5};
            }});
            assert(pair(a)===12&&log==='first,second,return-get,return-call,body,');
            delete proto.return;log='';assert(pair([{v:2},{v:3}])===5);
            "#,
            2,
        );
    }
}
