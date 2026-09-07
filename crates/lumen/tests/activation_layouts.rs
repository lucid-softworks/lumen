//! Shared activation layouts preserve closure identity, hoisting and initialization.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if(!x) throw new Error('assertion'); }}\nfunction runChecks() {{ {source} }} runChecks();\n'passed'"
        );
        match e.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn large_activations_are_independent_and_updates_are_live() {
    check(
        r#"
        function make(seed) {
            let a=seed,b=2,c=3,d=4,e=5,f=6,g=7,h=8,i=9,j=10;
            return {read:()=>a+b+c+d+e+f+g+h+i+j,write:(v)=>{a=v;}};
        }
        for(let i=0;i<300;i++) {
            const a=make(1),b=make(11);
            assert(a.read()===55 && b.read()===65);
            a.write(20);
            assert(a.read()===74 && b.read()===65);
        }
    "#,
    );
}

#[test]
fn hoisted_collisions_tdz_const_and_owned_values() {
    check(
        r#"
        function make(arg) { var arg; function arg() { return 9; } return ()=>arg; }
        function tdz() {
            const read=()=>later;
            let threw=false;
            try { read(); } catch(e) { threw=e instanceof ReferenceError; }
            const later={value:7};
            return [threw,read,()=>{try {later=1;}catch(e){return e instanceof TypeError;}}];
        }
        function big(v) { let x=v; const read=()=>x; x=x+1n; return read(); }
        for(let i=0;i<300;i++) {
            assert(make(3)()()===9);
            const result=tdz();
            assert(result[0] && result[1]().value===7 && result[2]());
            assert(big(9007199254740992n)===9007199254740993n);
        }
    "#,
    );
}
