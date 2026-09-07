//! Ownership transfers must preserve values across calls, branches, backedges and unwinding.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if (!x) throw new Error('assertion failed'); }}
             {source}\n'passed'"
        );
        match engine.eval(&script, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn returns_and_arguments_keep_every_value_representation() {
    check(
        r#"
        function identity(x) { return x; }
        function forward(x) { return identity(x); }
        const values=[undefined,null,true,false,-0,NaN,12.5,123456789012345678901n,
                      'hello 😀',Symbol('symbol'),{answer:42}];
        for(let i=0;i<300;i++) for(const v of values) assert(Object.is(forward(v),v));
        function fresh() { let x={answer:42}; return identity(x); }
        for(let i=0;i<300;i++) assert(fresh().answer===42);
    "#,
    );
}

#[test]
fn branches_loops_and_overwrites_preserve_live_values() {
    check(
        r#"
        function branch(x,b) { if(b) return x; return x.answer; }
        function loop(x,n) { let sum=0; while(n-->0) sum+=x.answer; return sum; }
        function overwrite(x) { let y=identity(x); x={answer:9}; return y.answer+x.answer; }
        function identity(x) { return x; }
        for(let i=0;i<300;i++) {
            let x={answer:7};
            assert(branch(x,true)===x && branch(x,false)===7);
            assert(loop(x,5)===35 && overwrite(x)===16);
        }
    "#,
    );
}

#[test]
fn throws_captures_and_tdz_keep_original_semantics() {
    check(
        r#"
        function fail(x) { throw x; }
        function caught(x) { try { fail(x); } catch(e) { return e===x; } }
        function captured(x) { const f=()=>x; const y=x; return f()===y; }
        function tdz() { return x; let x=1; }
        for(let i=0;i<300;i++) {
            assert(caught({}) && captured({}));
            try { tdz(); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
            const token={};
            try { fail(token); assert(false); } catch(e) { assert(e===token); }
        }
    "#,
    );
}
