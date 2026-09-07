//! Compiled enumeration agrees with the interpreter on ordering, callbacks and loop scopes.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if (!x) throw new Error('assertion'); }}\n{source}\n'passed'"
        );
        match e.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn own_order_prototypes_shadowing_and_primitives() {
    check(
        r#"
        function keys(obj) { let out=''; for(const k in obj) out+=k+','; return out; }
        const p={hidden:1,inherited:1}, o=Object.create(p);
        o.z=1; o[2]=1; o[1]=1; o.a=1; o[Symbol('s')]=1;
        Object.defineProperty(o,'hidden',{value:2,enumerable:false});
        for(let i=0;i<300;i++) {
            assert(keys(o)==='1,2,z,a,inherited,');
            assert(keys('😀x')==='0,1,2,' && keys(null)==='' && keys(undefined)==='');
        }
    "#,
    );
}

#[test]
fn deletion_and_proxy_callbacks_match_snapshot_semantics() {
    check(
        r#"
        function removed() {
            const o={a:1,b:2,c:3}; let out='';
            for(const k in o) { out+=k; if(k==='a') { delete o.b; o.d=4; } }
            return out;
        }
        function keys(o) { let out=''; for(const k in o) out+=k; return out; }
        for(let i=0;i<100;i++) {
            assert(removed()==='ac');
            let log='';
            const p=new Proxy({a:1,b:2},{
                ownKeys(t) { log+='K'; return ['a','b']; },
                getOwnPropertyDescriptor(t,k) { log+='D'+k; return Object.getOwnPropertyDescriptor(t,k); },
                getPrototypeOf(t) { log+='P'; return null; },
                has(t,k) { log+='H'+k; return k==='a'; }
            });
            assert(keys(p)==='a' && log==='KDaDbPHaHb');
        }
    "#,
    );
}

#[test]
fn lexical_head_tdz_capture_and_loop_control() {
    check(
        r#"
        function tdz(o) { const k=o; for(const k in k) return k; }
        function captured(o) { const out=[]; for(const k in o) out.push(()=>k); return out.map(f=>f()).join(','); }
        function flow(o) { let s=''; outer: for(let i=0;i<2;i++) {
            for(const k in o) { if(k==='a') continue; s+=k; if(k==='b') continue outer; }
        } return s; }
        function early(o) { for(let k in o) return k; return 'empty'; }
        for(let i=0;i<100;i++) {
            try { tdz({}); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
            assert(captured({a:1,b:2})==='a,b' && flow({a:1,b:2,c:3})==='bb');
            assert(early({x:1})==='x' && early({})==='empty');
        }
    "#,
    );
}

#[test]
fn no_array_iterator_protocol_and_exceptions_propagate() {
    check(
        r#"
        function keys(o) { let out=''; for(const k in o) out+=k; return out; }
        const old=Array.prototype[Symbol.iterator];
        Array.prototype[Symbol.iterator]=function(){throw new Error('unexpected iterator');};
        assert(keys({a:1,b:2})==='ab');
        Array.prototype[Symbol.iterator]=old;
        const token={};
        const p=new Proxy({}, {ownKeys(){throw token;}});
        try { keys(p); assert(false); } catch(e) { assert(e===token); }
        function body(o) { for(const k in o) throw token; }
        try { body({a:1}); assert(false); } catch(e) { assert(e===token); }
    "#,
    );
}
