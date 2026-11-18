use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn numeric_element_stores_keep_properties_and_mirrors_coherent() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine.eval(r#"
            function assert(v) { if (!v) throw new Error('numeric store'); }
            function put(a,k,v) { return a[k]=v; }
            function drop(a,k,v) { a[k]=v; }
            function read(a,k) { return a[k]; }
            const a=new Array(80);
            for(let j=0;j<a.length;j++) a[j]=j;
            for(let j=0;j<500;j++) {
                const k=j%80, v=j/3;
                assert(put(a,k,v)===v);
                assert(Object.getOwnPropertyDescriptor(a,String(k)).value===v);
                assert(read(a,k)===v);
                drop(a,k,-v);
                assert(Object.getOwnPropertyDescriptor(a,String(k)).value===-v);
                assert(read(a,k)===-v);
            }
            for(const v of [-0,Infinity,-Infinity,NaN,2**50]) {
                assert(Object.is(put(a,-0,v),v));
                assert(Object.is(read(a,0),v));
                assert(Object.is(Object.getOwnPropertyDescriptor(a,'0').value,v));
            }
            const object={answer:42}; assert(put(a,0,object)===object && a[0]===object);
            assert(put(a,0,3)===3);
            Object.defineProperty(a,'1',{writable:false});
            const old=a[1]; put(a,1,99); assert(a[1]===old);
            let caught=false; try { (function(a){'use strict';a[1]=42;})(a); }
            catch(e) { caught=e instanceof TypeError; } assert(caught);
            let seen=0; Object.defineProperty(a,'2',{set(v){seen=v;},configurable:true});
            assert(put(a,2,19)===19 && seen===19);
            delete a[3]; Object.defineProperty(Array.prototype,'3',{set(v){seen=v;},configurable:true});
            put(a,3,77); assert(seen===77 && !Object.hasOwn(a,'3')); delete Array.prototype[3];
            a.length=0; put(a,0,8); assert(a.length===1 && a[0]===8);
            for(const k of [-1,0.5,NaN,Infinity,4294967295,4294967296,'x']) {
                put(a,k,91); assert(a[k]===91);
            }
            const target={}; const proxy=new Proxy(target,{set(t,k,v){t[k]=v+1;return true;}});
            put(proxy,0,2); assert(target[0]===3);
            const ta=new Uint8Array(2); put(ta,0,258); assert(ta[0]===2);
            const packed=[1,2,3]; put(packed,1,9); assert(packed[1]===9);
            const frozen=Object.freeze([1,2]); put(frozen,0,8); assert(frozen[0]===1);
            'passed'
        "#, false).unwrap();
        match result {
            Completion::Value(v) => assert_eq!(v,"passed","{tier:?}"),
            Completion::Throw{name,message} => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
