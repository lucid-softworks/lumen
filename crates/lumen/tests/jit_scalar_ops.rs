//! Exercise native scalar templates after warmup, then their coercion/ownership fallbacks.
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn scalar_templates_match_interpreter_edges_and_fallbacks() {
    let source = r#"
        function arithmetic(a,b) { return [a+b,a-b,a*b,a/b,-a,+a]; }
        function updates(x) { let a=x++,b=++x,c=x--,d=--x; x++; x--; return [a,b,c,d,x]; }
        function condition(x) { if(x) return 1; return 0; }
        function logical(x) { return [x&&7,x||9,x??11]; }
        function repr(x) {
            if(typeof x==='number' && Object.is(x,-0)) return '-0';
            return typeof x+':'+String(x);
        }
        for(let n=0;n<200;n++) {
            arithmetic(n,3); updates(n); condition(n); logical(n);
        }
        const out=[];
        const numbers=[0,-0,1,-1,1.5,-1.5,NaN,Infinity,-Infinity,Number.MIN_VALUE,Number.MAX_VALUE,9007199254740992];
        for(const a of numbers) {
            for(const b of numbers) out.push(arithmetic(a,b).map(repr).join(','));
            out.push(updates(a).map(repr).join(','));
        }
        for(const a of [undefined,null,false,true,'','0','2',0n,2n,{},[],Symbol('s')]) {
            out.push(condition(a),logical(a).map(repr).join(','));
        }
        out.push(arithmetic('2',3).map(repr).join(','));
        out.push(arithmetic('2','3').map(repr).join(','));
        out.push(arithmetic('\ud83d','\ude00').map(repr).join(','));
        out.push(updates('2').map(repr).join(','));
        out.push(updates(2n).map(repr).join(','));
        let effects=[];
        const a={valueOf(){effects.push('a');return 2;}};
        const b={valueOf(){effects.push('b');return 3;}};
        out.push(arithmetic(a,b).map(repr).join(','),effects.join(''));
        try { arithmetic(Symbol(),1); } catch(e) { out.push(e.name); }
        try { arithmetic(1n,1); } catch(e) { out.push(e.name); }
        try { arithmetic(1n,2n); } catch(e) { out.push(e.name); } // unary + rejects BigInt
        for(let n=0;n<200;n++) {
            out.push(condition({value:n}),logical({value:n})[0]);
        }
        out.join('|')
    "#;
    let mut expected = None;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        match engine.eval(source, false).unwrap() {
            Completion::Value(v) => match &expected {
                None => expected = Some(v),
                Some(expected) => assert_eq!(&v, expected, "{tier:?}"),
            },
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
