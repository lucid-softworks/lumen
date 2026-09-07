//! Borrowed collection reads shared by native methods and guarded JIT calls.
use crate::builtins::collection_data::{CollectionData, CollectionKind};
use crate::interpreter::Interp;
use crate::value::{NativeFn, Value};
use std::rc::Rc;

pub(crate) const MAP_GET: u8 = 13;
pub(crate) const MAP_HAS: u8 = 14;
pub(crate) const SET_HAS: u8 = 15;

pub(crate) fn intrinsic(native: usize) -> u8 {
    for (method, id) in [
        (map_get as NativeFn, MAP_GET),
        (map_has, MAP_HAS),
        (set_has, SET_HAS),
    ] {
        if native == method as *const () as usize {
            return id;
        }
    }
    0
}

fn data<'a>(
    i: &'a Interp,
    this: &Value,
    kind: CollectionKind,
) -> Result<&'a CollectionData, Value> {
    let err = || i.make_error("TypeError", "method called on an incompatible receiver");
    let object = this.as_obj().ok_or_else(err)?;
    let data = i
        .map_data
        .get(&(Rc::as_ptr(object) as usize))
        .ok_or_else(err)?;
    if data.kind() != kind {
        return Err(err());
    }
    Ok(data)
}

fn read(i: &Interp, this: &Value, key: &Value, id: u8) -> Result<Value, Value> {
    let entries = data(
        i,
        this,
        if id == SET_HAS {
            CollectionKind::Set
        } else {
            CollectionKind::Map
        },
    )?;
    Ok(if id == MAP_GET {
        entries.lookup(key).cloned().unwrap_or(Value::Undefined)
    } else {
        Value::Bool(entries.contains(key))
    })
}

pub(super) fn map_get(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    read(i, &this, args.first().unwrap_or(&Value::Undefined), MAP_GET)
}

pub(super) fn map_has(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    read(i, &this, args.first().unwrap_or(&Value::Undefined), MAP_HAS)
}

pub(super) fn set_has(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    read(i, &this, args.first().unwrap_or(&Value::Undefined), SET_HAS)
}

#[inline]
pub(crate) fn read_intrinsic(
    i: &Interp,
    this: &Value,
    key: &Value,
    id: u8,
) -> Result<Value, Value> {
    #[cfg(test)]
    HITS.with(|hits| hits.set(hits.get() + 1));
    read(i, this, key, id)
}

#[cfg(test)]
thread_local! {
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::HITS.with(|hits| hits.set(0));
            let script = format!("function assert(x) {{ if(!x) throw new Error('assertion'); }} function drive() {{ {source} }} drive(); 'passed'");
            let result = engine.eval(&script, false).unwrap();
            match result {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            #[cfg(all(
                target_arch = "aarch64",
                any(target_os = "macos", target_os = "linux", target_os = "windows")
            ))]
            if tier == Tier::Jit {
                assert!(
                    super::HITS.with(|hits| hits.get()) > 0,
                    "native collection reads not exercised"
                );
            }
        }
    }

    #[test]
    fn warmed_reads_preserve_keys_aliasing_and_live_mutations() {
        check(
            r#"
            const keys=[undefined,null,true,false,0,-0,NaN,Infinity,-2,1.5,1n,'key',Symbol('key'),{}];
            const m=new Map(), s=new Set();
            function get(c,k) {return c.get(k);}
            function has(c,k) {return c.has(k);}
            for(let i=0;i<1000;i++) {
                const k=keys[i%keys.length];
                m.set(k,m); s.add(k);
                assert(get(m,k)===m && has(m,k) && has(s,k));
                m.set(k,k); assert(Object.is(get(m,k),k));
                m.delete(k); s.delete(k);
                assert(get(m,k)===undefined && !has(m,k) && !has(s,k));
            }
            m.set(undefined,17); assert(m.get()===17 && m.has());
            m.set('x',m); m.clear(); assert(get(m,'x')===undefined);
            assert(get(new Map([[m,m]]),m)===m);
        "#,
        );
    }

    #[test]
    fn warmed_calls_observe_method_replacement_and_receiver_brands() {
        check(
            r#"
            const m=new Map([[1,2]]),s=new Set([1]);
            function get(c,k) {return c.get(k);}
            function has(c,k) {return c.has(k);}
            function throws(f) {let yes=false;try {f();}catch(e){yes=e instanceof TypeError;}assert(yes);}
            for(let i=0;i<1000;i++) assert(get(m,1)===2 && has(m,1) && has(s,1));
            m.get=function(k){return k+8;}; assert(get(m,1)===9); delete m.get;
            m.has=function(){return false;}; assert(!has(m,1)); delete m.has;
            s.has=function(){return false;}; assert(!has(s,1)); delete s.has;
            assert(get(m,1)===2 && has(m,1) && has(s,1));
            throws(()=>get(new Proxy(m,{}),1));
            throws(()=>get({get:Map.prototype.get},1));
            throws(()=>has({has:Set.prototype.has},1));
            s.get=Map.prototype.get; throws(()=>get(s,1));
            m.has=Set.prototype.has; throws(()=>has(m,1)); delete m.has;
            const weak=new WeakMap();weak.get=Map.prototype.get;throws(()=>get(weak,1));
        "#,
        );
    }

    #[test]
    fn foreign_methods_keep_their_error_realm_after_warmup() {
        check(
            r#"
            const m=new Map([[1,2]]);
            function get(c,k){return c.get(k);}
            for(let i=0;i<1000;i++) assert(get(m,1)===2);
            const realm=$262.createRealm();
            m.get=realm.global.Map.prototype.get;
            for(let i=0;i<300;i++) assert(get(m,1)===2);
            let threw=false;
            try {get({get:m.get},1);}catch(e){threw=e instanceof realm.global.TypeError;}
            assert(threw);
            delete m.get; assert(get(m,1)===2);
        "#,
        );
    }

    #[test]
    fn ordinary_properties_do_not_change_the_brand_after_warmup() {
        check(
            r#"
            const m=new Map([[1,2]]);
            function get(c,k){return c.get(k);}
            for(let i=0;i<1000;i++) assert(get(m,1)===2);
            m.__ck='Set';
            assert(get(m,1)===2);
            m.__ck='Map';assert(get(m,1)===2);
        "#,
        );
    }
}
