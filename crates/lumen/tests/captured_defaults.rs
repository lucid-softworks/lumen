//! Defaulted parameters captured by closures across every execution tier.
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
fn fresh_defaults_and_explicit_arguments_keep_identity() {
    check(
        r#"
        function object(x={}) { return () => x; }
        function array(x=[]) { return () => x; }
        function number(x=7) { return () => ++x; }
        const supplied={};
        for(let i=0;i<300;i++) {
            const a=object(), b=object(undefined);
            assert(a()===a() && a()!==b() && object(supplied)()===supplied);
            assert(object(null)()===null && object(false)()===false);
            assert(array()()!==array()());
            const n=number(); assert(n()===8 && n()===9);
        }
    "#,
    );
}

#[test]
fn later_defaults_and_body_initializers_observe_initialized_capture() {
    check(
        r#"
        function later(x={}, y=x) { return [() => x,y]; }
        function hoisted(x={}) { function read() { return x; } return read; }
        function overwrite(x={}) { const read=()=>x; x={changed:true}; return read; }
        for(let i=0;i<300;i++) {
            const pair=later(); assert(pair[0]()===pair[1]);
            const obj={}; assert(hoisted(obj)()===obj && typeof hoisted()()==='object');
            assert(overwrite()().changed);
        }
    "#,
    );
}

#[test]
fn hoist_conflicts_and_effectful_defaults_preserve_order() {
    check(
        r#"
        function conflict(x={}) { function x() { return 9; } return () => x; }
        let log='';
        function effect(x=(log+='a',{}), y=(log+='b',x)) { return () => y; }
        for(let i=0;i<100;i++) {
            assert(conflict()()()===9);
            log=''; assert(typeof effect()()==='object' && log==='ab');
        }
    "#,
    );
}
