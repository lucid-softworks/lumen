//! Guarded own-data Array Iterator state; target reads retain existing semantics.
use super::{ab, map_ptr, set_data};
use crate::{
    interpreter::Interp,
    value::{Exotic, Property, Value},
};

pub(super) fn fast(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    next::<true>(i, this, args)
}
pub(super) fn baseline(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    next::<false>(i, this, args)
}

struct State {
    target: Value,
    index: usize,
    kind: u8,
}

/// No getters/coercions here. Failure occurs before any user-visible operation.
fn own_state(this: &Value) -> Option<State> {
    let Value::Obj(o) = this else {
        return None;
    };
    let b = o.borrow();
    if !matches!(b.exotic, Exotic::None) || !b.ic_plain.get() {
        return None;
    }
    let data = |key| {
        b.props
            .get(key)
            .filter(|p| !p.accessor())
            .map(Property::value)
    };
    let target = data("__ai_target")?;
    if matches!(target, Value::Undefined) {
        return Some(State {
            target,
            index: 0,
            kind: 0,
        });
    }
    let Value::Num(index) = data("__ai_index")? else {
        return None;
    };
    let Value::Num(kind) = data("__ai_kind")? else {
        return None;
    };
    // Match the ordinary constructor state; malformed/coercible state stays checked.
    if !index.is_finite()
        || index < 0.0
        || index.fract() != 0.0
        || index >= usize::MAX as f64
        || !matches!(kind, 0.0 | 1.0 | 2.0)
    {
        return None;
    }
    Some(State {
        target,
        index: index as usize,
        kind: kind as u8,
    })
}

/// Revalidate AFTER target length getters/coercions, which may mutate this iterator.
fn write_index(i: &mut Interp, this: &Value, next: f64) -> Result<(), Value> {
    let mut written = false;
    if let Value::Obj(o) = this {
        let mut b = o.borrow_mut();
        if matches!(b.exotic, Exotic::None) && b.ic_plain.get() {
            if let Some(p) = b
                .props
                .get_mut("__ai_index")
                .filter(|p| !p.accessor() && p.writable() && matches!(p.value(), Value::Num(_)))
            {
                p.set_value(Value::Num(next));
                written = true;
                #[cfg(test)]
                WRITES.with(|n| n.set(n.get() + 1));
            }
        }
    }
    if !written {
        #[cfg(test)]
        LATE_FALLBACKS.with(|n| n.set(n.get() + 1));
        ab(i.set_member(this, "__ai_index", Value::Num(next)))?;
    }
    Ok(())
}

fn next<const FAST: bool>(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    // Brand check: the receiver must carry the Array Iterator internal slots.
    if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("__ai_kind")) {
        return Err(i.make_error(
            "TypeError",
            "Array Iterator next called on an incompatible receiver",
        ));
    }
    let state = if FAST { own_state(&this) } else { None };
    #[cfg(test)]
    if state.is_some() {
        SNAPSHOTS.with(|n| n.set(n.get() + 1));
    }
    let (target, state) = match state {
        Some(State {
            target,
            index,
            kind,
        }) => (target, Some((index, kind))),
        None => (ab(i.get_member(&this, "__ai_target"))?, None),
    };
    // An exhausted iterator clears its target so it stays done even if the source later grows.
    if matches!(target, Value::Undefined) {
        let result = i.new_object();
        set_data(&result, "value", Value::Undefined);
        set_data(&result, "done", Value::Bool(true));
        return Ok(Value::Obj(result));
    }
    let (idx, kind) = match state {
        Some(state) => state,
        None => {
            let idx_v = ab(i.get_member(&this, "__ai_index"))?;
            let idx = ab(i.to_number(&idx_v))? as usize;
            let kind_v = ab(i.get_member(&this, "__ai_kind"))?;
            let kind = ab(i.to_number(&kind_v))? as u8;
            (idx, kind)
        }
    };
    let len = target_len(i, &target)?;
    let result = i.new_object();
    if idx >= len {
        ab(i.set_member(&this, "__ai_target", Value::Undefined))?;
        set_data(&result, "value", Value::Undefined);
        set_data(&result, "done", Value::Bool(true));
        return Ok(Value::Obj(result));
    }
    if FAST {
        write_index(i, &this, (idx + 1) as f64)?;
    } else {
        ab(i.set_member(&this, "__ai_index", Value::Num((idx + 1) as f64)))?;
    }
    let elem = ab(i.get_member(&target, &idx.to_string()))?;
    let value = match kind {
        1 => Value::Num(idx as f64),
        2 => i.make_array(vec![Value::Num(idx as f64), elem]),
        _ => elem,
    };
    set_data(&result, "value", value);
    set_data(&result, "done", Value::Bool(false));
    Ok(Value::Obj(result))
}

fn target_len(i: &mut Interp, target: &Value) -> Result<usize, Value> {
    Ok(
        if let Some(info) = map_ptr(target).and_then(|p| i.typed_arrays.get(&p).copied()) {
            match i.ta_len(&info) {
                Some(l) => l,
                None => return Err(i.make_error("TypeError", "TypedArray is out of bounds")),
            }
        } else {
            match target {
                // LengthOfArrayLike through [[Get]], so a proxy target's traps are honored.
                Value::Obj(_) => {
                    let lv = ab(i.get_member(target, "length"))?;
                    let n = ab(i.to_number(&lv))?;
                    if n.is_nan() || n <= 0.0 {
                        0
                    } else {
                        n.min(9007199254740991.0) as usize
                    }
                }
                Value::Str(s) => crate::jstr::unit_len(s),
                _ => 0,
            }
        },
    )
}

#[cfg(test)]
thread_local! {
    static SNAPSHOTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LATE_FALLBACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, expected: (usize, usize, usize)) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SNAPSHOTS.with(|n| n.set(0));
            super::WRITES.with(|n| n.set(0));
            super::LATE_FALLBACKS.with(|n| n.set(0));
            // Existing AST native calls lose the strict caller flag on this path;
            // compiled callers retain it. Keep this optimization neutral to that difference.
            let expected_native_strict = tier != Tier::Interp;
            let script = format!("function assert(v){{if(!v)throw new Error('iterator state');}} var expectedNativeStrict={expected_native_strict}; var next=Object.getPrototypeOf([].values()).next; {source}; 'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            let counts = (
                super::SNAPSHOTS.with(|n| n.get()),
                super::WRITES.with(|n| n.get()),
                super::LATE_FALLBACKS.with(|n| n.get()),
            );
            let expected = if std::env::var_os("LUMEN_NO_ARRAY_ITERATOR_STATE").is_some() {
                (0, 0, 0)
            } else {
                expected
            };
            assert_eq!(counts, expected, "{tier:?}: snapshot/write/late fallback");
        }
    }

    #[test]
    fn ordinary_values_exhaustion_and_proxy_target_keep_fetch_order() {
        check(
            r#"
            var it=[10,20].values();
            assert(it.next().value===10);assert(it.next().value===20);
            assert(it.next().done);assert(it.next().done);
            var log='';var target=new Proxy({0:5,length:1},{get:function(t,k){log+=k+',';return t[k];}});
            var fake={__ai_target:target,__ai_kind:1,__ai_index:0};
            assert(next.call(fake).value===0&&log==='length,0,');
        "#,
            (5, 3, 0),
        );
    }

    #[test]
    fn length_mutations_revalidate_only_the_pending_index_write() {
        check(
            r#"
            var log='',fake={__ai_kind:0,__ai_index:0};
            fake.__ai_target={0:41,get length(){
                log+='L';Object.defineProperty(fake,'__ai_index',{set:function(v){log+='S'+v;},configurable:true});
                $262.gc();return 1;
            }};
            var r=next.call(fake);assert(r.value===41&&!r.done&&log==='LS1');
            var lengths=0;fake={__ai_kind:0,__ai_index:0};
            fake.__ai_target={0:8,get length(){lengths++;fake.__ai_index={kept:true};$262.gc();return 1;}};
            r=next.call(fake);assert(r.value===8&&lengths===1&&fake.__ai_index===1);
        "#,
            (2, 0, 2),
        );
    }

    #[test]
    fn reentrant_length_getter_preserves_both_saved_indices() {
        check(
            r#"
            var active=false,inner,lengths=0,fake={__ai_kind:0,__ai_index:0};
            fake.__ai_target={0:6,1:7,get length(){
                lengths++;if(!active){active=true;inner=next.call(fake);}return 2;
            }};
            var r=next.call(fake);
            assert(inner.value===6&&r.value===6&&lengths===2&&fake.__ai_index===1);
        "#,
            (2, 2, 0),
        );
    }

    #[test]
    fn accessor_and_coercion_fallback_preserves_order_and_early_done() {
        check(
            r#"
            var exhausted={__ai_target:undefined,get __ai_index(){throw new Error('index');},get __ai_kind(){throw new Error('kind');}};
            assert(next.call(exhausted).done);
            var log='',fake={__ai_target:[9],__ai_kind:0,__ai_index:{valueOf:function(){log+='I';fake.__ai_kind=1;return 0;}}};
            assert(next.call(fake).value===0&&log==='I');
            log='';fake={__ai_kind:0,__ai_index:0,get __ai_target(){log+='T';return [];},set __ai_target(v){assert(v===undefined);log+='C';}};
            assert(next.call(fake).done&&log==='TC');
        "#,
            (1, 0, 1),
        );
    }

    #[test]
    fn typed_array_detach_and_fixed_view_shrink_recheck_length() {
        check(
            r#"
            var ta=new Uint8Array(2);ta[0]=9;ta[1]=8;var it=Array.prototype.values.call(ta);
            assert(it.next().value===9);$262.detachArrayBuffer(ta.buffer);
            var threw=false;try{it.next();}catch(e){threw=e.name==='TypeError';}assert(threw);
            var buffer=new ArrayBuffer(4,{maxByteLength:8});ta=new Uint8Array(buffer,0,4);ta[0]=7;
            it=Array.prototype.values.call(ta);assert(it.next().value===7);buffer.resize(1);
            threw=false;try{it.next();}catch(e){threw=e.name==='TypeError';}assert(threw);
        "#,
            (4, 2, 0),
        );
    }

    #[test]
    fn proxy_receiver_retains_existing_own_brand_rejection() {
        check(
            r#"
            var traps=0;var fake={__ai_target:[1],__ai_kind:0,__ai_index:0};
            var proxy=new Proxy(fake,{get:function(t,k){traps++;return t[k];}});
            var threw=false;try{next.call(proxy);}catch(e){threw=e.name==='TypeError';}
            assert(threw&&traps===0);
        "#,
            (0, 0, 0),
        );
    }

    #[test]
    fn late_readonly_and_deleted_index_keep_strictness_and_inherited_setter() {
        check(
            r#"
            var lengths=0,fake={__ai_kind:0,__ai_index:0};
            fake.__ai_target={0:12,get length(){
                lengths++;Object.defineProperty(fake,'__ai_index',{writable:false});return 1;
            }};
            var r=next.call(fake);
            assert(r.value===12&&!r.done&&fake.__ai_index===0&&lengths===1);
            fake={__ai_kind:0,__ai_index:0};
            fake.__ai_target={0:13,get length(){
                lengths++;Object.defineProperty(fake,'__ai_index',{writable:false});return 1;
            }};
            function strictNext(){'use strict';return next.call(fake);}
            var threw=false;try{strictNext();}catch(e){threw=e.name==='TypeError';}
            assert(threw===expectedNativeStrict&&fake.__ai_index===0&&lengths===2);
            var written=-1,receiver;
            var proto={set __ai_index(v){written=v;receiver=this;}};
            fake={__ai_kind:0,__ai_index:0};Object.setPrototypeOf(fake,proto);
            fake.__ai_target={0:14,get length(){lengths++;delete fake.__ai_index;return 1;}};
            r=next.call(fake);
            assert(r.value===14&&!r.done&&written===1&&receiver===fake&&lengths===3);
            assert(!Object.prototype.hasOwnProperty.call(fake,'__ai_index'));
        "#,
            (3, 0, 3),
        );
    }
}
