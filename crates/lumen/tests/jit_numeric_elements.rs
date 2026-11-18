use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn numeric_element_reads_recheck_holes_descriptors_and_exotic_receivers() {
    let source = r#"
        function read(a,i) { return a[i]; }
        function repr(v) { return typeof v+':'+(Object.is(v,-0)?'-0':String(v)); }
        const a=new Array(40);
        for(let i=0;i<a.length;i++) a[i]=i+0.5;
        for(let i=0;i<200;i++) read(a,i%40);
        const out=[];
        for(const i of [0,-0,1,39,40,-1,1.5,NaN,Infinity,4294967294,4294967295,2**53,'3']) out.push(repr(read(a,i)));
        a[0]=-0; a[1]=NaN;
        out.push(repr(read(a,0)),repr(read(a,1)));
        delete a[2];
        Object.defineProperty(Array.prototype,'2',{get(){return 91;},configurable:true});
        const inherited=read(a,2);
        delete Array.prototype[2];
        out.push(inherited);
        a[2]=7;
        Object.defineProperty(a,'3',{get(){return this===a?99:0;},configurable:true});
        out.push(read(a,2),read(a,3));
        a[4]={marker:'alive'};
        const owner=read(a,4); a[4]=0;
        out.push(owner.marker);
        a.length=1;
        out.push(repr(read(a,4)));
        const proxy=new Proxy([12],{get(t,k){return k==='0'?77:t[k];}});
        out.push(read(proxy,0),read('abc',1),read(new String('def'),1));
        const ta=new Float64Array([3.5,4.5]);out.push(read(ta,1));
        const o={0:41};out.push(read(o,0));
        Object.defineProperty(o,'0',{get(){return 42;}});out.push(read(o,0));
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
