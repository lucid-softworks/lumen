//! Lexical this through the engine boundary in every tier, including delayed TDZ reads.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let script = format!(
            "function assert(x) {{ if (!x) throw new Error('assertion'); }}\n{source}\n'passed'"
        );
        match engine.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn lexical_receiver_ignores_call_bind_and_apply() {
    check(
        r#"
        function make() { return (v) => { this.x=v; return this; }; }
        const a={}, b={};
        const f=make.call(a), g=make.call(b);
        for(let i=0;i<300;i++) {
            assert(f.call(b,i)===a && a.x===i);
            assert(g.apply(a,[i+1])===b && b.x===i+1);
            assert(f.bind(b)(i)===a);
        }
        function strict() { 'use strict'; return () => this; }
        assert(strict.call(undefined)()===undefined);
        assert(strict.call(null)()===null);
        assert(strict.call(7)()===7);
    "#,
    );
}

#[test]
fn nested_arrows_forward_this_through_captured_activations() {
    check(
        r#"
        function make() { return (x) => () => [this,x]; }
        const a={}, b={}; const f=make.call(a), g=make.call(b);
        for(let i=0;i<300;i++) {
            const left=f(i), right=g(i+1);
            const l=left(), r=right();
            assert(l[0]===a && l[1]===i && r[0]===b && r[1]===i+1);
        }
    "#,
    );
}

#[test]
fn derived_this_is_checked_at_each_read_not_at_entry() {
    check(
        r#"
        class Base {}
        class Derived extends Base {
            constructor() {
                const read = (yes) => yes ? this : 17;
                const make = (x) => () => [this,x];
                const nested = make(9);
                assert(read(false)===17);
                for(let i=0;i<100;i++) {
                    try { read(true); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
                    try { nested(); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
                }
                super();
                assert(read(true)===this && nested()[0]===this && nested()[1]===9);
            }
        }
        new Derived();
    "#,
    );
}

#[test]
fn receiver_read_precedes_assignment_rhs() {
    check(
        r#"
        let ran=false;
        class Base {}
        class Derived extends Base {
            constructor() {
                const assign=() => { this.x=(ran=true); };
                try { assign(); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
                assert(!ran);
                super(); assign(); assert(this.x && ran);
            }
        }
        new Derived();
    "#,
    );
}
