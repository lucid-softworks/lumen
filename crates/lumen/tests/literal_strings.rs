//! Literal storage may be shared with the AST, but concatenating values must not mutate it.
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn literal_values_remain_immutable_across_calls_and_snapshot_execution() {
    let source = r#"
        function literal() { return 'original'; }
        function grow() { let s='λ💡'; for(let i=0;i<20;i++) s+='x'; return s; }
        function template() { return `head${'λ'}tail`; }
        for(let call=0;call<100;call++) {
            let a=literal(), b=a; a+=' changed';
            if(a!=='original changed' || b!=='original' || literal()!=='original') throw 'literal mutated';
            if(grow()!=='λ💡'+'x'.repeat(20)) throw 'accumulator mutated';
            let t=template(); t+='!';
            if(template()!=='headλtail') throw 'template mutated';
            let lone='\uD800'; lone+='x';
            if(lone.charCodeAt(0)!==0xD800 || lone.length!==2 || '\uD800'.length!==1) throw 'surrogate mutated';
        }
        'passed'
    "#;
    let snapshot = lumen::compile_snapshot(source).unwrap();
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for cached in [false, true] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let result = if cached {
                engine.eval_snapshot(&snapshot, false)
            } else {
                engine.eval(source, false)
            }
            .unwrap_or_else(|e| panic!("{}", e.message));
            match result {
                Completion::Value(value) => {
                    assert_eq!(value, "passed", "{tier:?}, cached={cached}")
                }
                Completion::Throw { name, message } => {
                    panic!("{tier:?}, cached={cached}: {name}: {message}")
                }
            }
        }
    }
}
