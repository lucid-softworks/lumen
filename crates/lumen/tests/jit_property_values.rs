//! Warm property sites, then change value kinds and descriptors without changing the source.
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn cached_property_values_keep_ownership_and_recheck_live_objects() {
    let source = r#"
        function local(o) { return o.p; }
        function receiver() { return this.p; }
        const o={p:1};
        for(let n=0;n<200;n++) { local(o); receiver.call(o); }
        const out=[];
        const sym=Symbol('kept'), ref={marker:'alive'};
        for(const v of [undefined,null,false,true,0,-0,NaN,Infinity,-Infinity,3.5,'text','\ud83d\ude00',sym,ref,12n]) {
            o.p=v;
            const a=local(o), b=receiver.call(o);
            out.push(Object.is(a,v),Object.is(b,v));
            // Another write must not invalidate either returned owner.
            o.p=0;
            out.push(Object.is(a,v),Object.is(b,v));
        }
        let calls=0;
        Object.defineProperty(o,'p',{get(){calls++;return ref;},configurable:true});
        out.push(local(o)===ref,receiver.call(o)===ref,calls);
        delete o.p;
        const proto={p:sym};
        Object.setPrototypeOf(o,proto);
        out.push(local(o)===sym,receiver.call(o)===sym);
        function inherited(o) { return o.p; }
        const mid={}, leaf=Object.create(mid);
        Object.setPrototypeOf(mid,proto);
        for(let n=0;n<200;n++) inherited(leaf);
        out.push(inherited(leaf)===sym);
        proto.p=ref;
        out.push(inherited(leaf)===ref);
        mid.p='shadow';
        out.push(inherited(leaf));
        delete mid.p;
        Object.defineProperty(proto,'p',{get(){return this===leaf?'receiver':'wrong';}});
        out.push(inherited(leaf));
        delete proto.p;
        out.push(inherited(leaf));
        out.push(local(new Proxy({p:8},{get(t,k){return k==='p'?9:t[k];}})));
        out.join('|')
    "#;
    let mut expected = None;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(8);
        match engine.eval(source, false).unwrap() {
            Completion::Value(v) => match &expected {
                None => expected = Some(v),
                Some(expected) => assert_eq!(&v, expected, "{tier:?}"),
            },
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
