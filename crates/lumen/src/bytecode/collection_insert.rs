//! Transfer native call operands into collection storage without clone/drop round trips.
use crate::builtins::collection_insert;
use crate::interpreter::{Abrupt, MAX_EVAL_DEPTH};
use crate::jit::{JitCtx, SpFlag};
use crate::value::Value;

/// # Safety
/// An exact builtin/realm call-IC hit proved Map.set with two arguments; all four operands
/// below `sp` are initialized and consumed, including on errors.
pub(crate) unsafe extern "C" fn map_set(ctx: *mut JitCtx, pc: u32, sp: *mut Value) -> SpFlag {
    unsafe { insert::<false>(ctx, pc, sp) }
}

/// # Safety
/// As for `map_set`, but the builtin is Set.add and there are three operands.
pub(crate) unsafe extern "C" fn set_add(ctx: *mut JitCtx, pc: u32, sp: *mut Value) -> SpFlag {
    unsafe { insert::<true>(ctx, pc, sp) }
}

unsafe fn insert<const SET: bool>(ctx: *mut JitCtx, pc: u32, sp: *mut Value) -> SpFlag {
    let ctx = unsafe { &mut *ctx };
    let interp = unsafe { &mut *ctx.interp };
    unsafe { &*ctx.chunk }.record_inline_location(interp, pc as usize);
    let base = unsafe { sp.sub(if SET { 3 } else { 4 }) };
    let mut args_moved = false;
    let mut receiver_moved = false;
    interp.depth += 1;
    let result = if interp.depth > MAX_EVAL_DEPTH {
        Err(interp.throw("RangeError", "Maximum call stack size exceeded"))
    } else {
        interp.gc_check_amortized().and_then(|()| {
            // Brand checking and insertion cannot enter JS. No constructor-state change or
            // pending-tail drain is observable, and GC has finished before moving arguments.
            args_moved = true;
            let key = unsafe { base.add(2).read() };
            let result = if SET {
                collection_insert::set_add_owned(interp, unsafe { &*base }, key)
            } else {
                collection_insert::map_set_owned(interp, unsafe { &*base }, key, unsafe {
                    base.add(3).read()
                })
            };
            result.map_err(Abrupt::Throw).map(|()| {
                receiver_moved = true;
                #[cfg(test)]
                HITS.with(|hits| {
                    let mut counts = hits.get();
                    counts[SET as usize] += 1;
                    hits.set(counts);
                });
                unsafe { base.read() }
            })
        })
    };
    interp.depth -= 1;
    unsafe {
        if !receiver_moved {
            std::ptr::drop_in_place(base);
        }
        std::ptr::drop_in_place(base.add(1));
        if !args_moved {
            std::ptr::drop_in_place(base.add(2));
            if !SET {
                std::ptr::drop_in_place(base.add(3));
            }
        }
    }
    match result {
        Ok(value) => {
            unsafe { base.write(value) };
            SpFlag {
                sp: unsafe { base.add(1) },
                flag: 0,
            }
        }
        Err(error) => {
            ctx.error = Some(error);
            SpFlag { sp: base, flag: 1 }
        }
    }
}

#[cfg(test)]
thread_local! {
    static HITS: std::cell::Cell<[usize; 2]> = const { std::cell::Cell::new([0, 0]) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::HITS.with(|hits| hits.set([0, 0]));
            let script=format!("function assert(x) {{ if(!x) throw new Error('assertion'); }} function drive() {{ {source} }} drive(); 'passed'");
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            #[cfg(all(
                target_arch = "aarch64",
                any(target_os = "macos", target_os = "linux", target_os = "windows")
            ))]
            if tier == Tier::Jit {
                assert!(
                    super::HITS.with(|hits| hits.get().iter().all(|n| *n > 0)),
                    "both native insertion helpers must run"
                );
            }
        }
    }

    #[test]
    fn moved_arguments_preserve_aliases_keys_and_insertion_order() {
        check(
            r#"
            const m=new Map(),s=new Set();
            const keys=[undefined,null,true,0,-0,NaN,Infinity,1.5,1n,'key',Symbol('key'),{}];
            function put(k,v){assert(m.set(k,v)===m);assert(s.add(k)===s);}
            for(let i=0;i<1000;i++) {
                const k=keys[i%keys.length];put(k,m);assert(m.get(k)===m && s.has(k));
                put(k,k);assert(Object.is(m.get(k),k));
                m.delete(k);s.delete(k);
            }
            m.clear();s.clear();put(-0,-0);
            assert(1/m.keys().next().value===Infinity && 1/m.get(0)===-Infinity);
            assert(1/s.values().next().value===Infinity);
            m.clear();s.clear();put('a',1);put('b',2);
            const mi=m.keys(),si=s.values();assert(mi.next().value==='a' && si.next().value==='a');
            m.delete('b');s.delete('b');put('c',3);put('b',4);
            assert(mi.next().value==='c' && mi.next().value==='b');
            assert(si.next().value==='c' && si.next().value==='b');
            const value={};assert(new Map().set(value,value).get(value)===value);
            assert(new Set().add(value).has(value));
        "#,
        );
    }

    #[test]
    fn warmed_insertions_keep_overrides_brand_errors_and_missing_arguments() {
        check(
            r#"
            const m=new Map(),s=new Set();
            function put(c,k,v){return c.set(k,v);}
            function add(c,k){return c.add(k);}
            function throws(f){let yes=false;try{f();}catch(e){yes=e instanceof TypeError;}assert(yes);}
            for(let i=0;i<1000;i++){assert(put(m,i,m)===m);assert(add(s,i)===s);}
            m.set=function(k,v){return v;};assert(put(m,1,7)===7);delete m.set;
            s.add=function(k){return k;};assert(add(s,7)===7);delete s.add;
            throws(()=>put(new Proxy(m,{}),{},{}));throws(()=>add(new Proxy(s,{}),{}));
            throws(()=>put({set:Map.prototype.set}, {},{}));
            throws(()=>add({add:Set.prototype.add},{}));
            m.__ck='Set';assert(put(m,1,2)===m);m.__ck='Map';
            s.__ck='Map';assert(add(s,1)===s);s.__ck='Set';
            assert(put(m,1,2)===m && m.get(1)===2);assert(add(s,1)===s);
            m.set();assert(m.has(undefined) && m.get(undefined)===undefined);
            m.set('missing');assert(m.has('missing') && m.get('missing')===undefined);
            s.add();assert(s.has(undefined));
            const weak=new WeakMap();weak.set=Map.prototype.set;throws(()=>put(weak,{},{}));
            const ws=new WeakSet();ws.add=Set.prototype.add;throws(()=>add(ws,{}));
        "#,
        );
    }

    #[test]
    fn foreign_insertions_keep_error_realms() {
        check(
            r#"
            const m=new Map(),s=new Set();
            function put(c,k,v){return c.set(k,v);}
            function add(c,k){return c.add(k);}
            for(let i=0;i<1000;i++){assert(put(m,i,m)===m);assert(add(s,i)===s);}
            const r=$262.createRealm();m.set=r.global.Map.prototype.set;s.add=r.global.Set.prototype.add;
            for(let i=0;i<300;i++){assert(put(m,i,m)===m);assert(add(s,i)===s);}
            let a=false,b=false;
            try{put({set:m.set},m,m);}catch(e){a=e instanceof r.global.TypeError;}
            try{add({add:s.add},s);}catch(e){b=e instanceof r.global.TypeError;}
            assert(a && b);
            delete m.set;delete s.add;assert(put(m,1,2)===m && add(s,1)===s);
        "#,
        );
    }
}
