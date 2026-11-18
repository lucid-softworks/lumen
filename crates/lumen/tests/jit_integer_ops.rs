use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn integer_templates_match_to_int32_and_shift_boundaries() {
    let source = r#"
        function bits(a,b) { return [a&b,a|b,a^b,a<<b,a>>b,a>>>b,~a]; }
        for(let n=0;n<200;n++) bits(n,n+1);
        const values=[0,-0,0.1,-0.1,1.9,-1.9,31,32,33,-33,
            2147483647,2147483648,-2147483648,-2147483649,4294967295,4294967296,
            4294967297,4294967297.9,-4294967297.9,2**52,2**53-1,2**53,
            2**63-1024,2**63,-(2**63),-(2**63)+1024,2**64,
            NaN,Infinity,-Infinity,Number.MIN_VALUE,Number.MAX_VALUE];
        const out=[];
        for(const a of values) for(const b of values) out.push(bits(a,b).join(','));
        if(bits(-1,0)[5]!==4294967295 || bits(4294967297.9,0)[1]!==1 || bits(-1,31)[5]!==1) throw 'bad uint32';
        for(const v of [undefined,null,false,true,'3.9',{},[]]) out.push(bits(v,3).join(','));
        let effects='';
        const a={valueOf(){effects+='a';return -1.5;}},b={valueOf(){effects+='b';return 33.7;}};
        out.push(bits(a,b).join(','),effects);
        for(const v of [Symbol('s'),1n]) {
            try { bits(v,2); } catch(e) { out.push(e.name); }
        }
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
