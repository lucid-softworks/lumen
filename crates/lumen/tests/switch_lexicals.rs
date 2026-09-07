//! Case-block lexical environments through all three execution tiers.
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
fn discriminant_shadowing_fallthrough_and_default() {
    check(
        r#"
        function f(x) {
            let y=0;
            switch(y) { case 0: let y=7; case 1: return y+x; }
        }
        function g(x) {
            switch(x) { case 1: return 1; default: let y=3; case 2: return y; }
        }
        for(let i=0;i<300;i++) {
            assert(f(i)===i+7 && g(0)===3 && g(1)===1);
            try { g(2); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
        }
    "#,
    );
}

#[test]
fn case_tests_see_tdz_and_const_writes_throw() {
    check(
        r#"
        function f() { let x=0; switch(x) { case x: const x=1; return x; } }
        function g() { switch(0) { case 0: const x=1; x=2; return x; } }
        for(let i=0;i<300;i++) {
            try { f(); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
            try { g(); assert(false); } catch(e) { assert(e instanceof TypeError); }
        }
    "#,
    );
}

#[test]
fn reentry_resets_tdz_and_continue_targets_outer_loop() {
    check(
        r#"
        function f() {
            let sum=0;
            for(let i=0;i<4;i++) {
                switch(i) { case 0: let x=7; sum+=x; continue; case 1: return x; }
            }
            return sum;
        }
        function g() {
            let sum=0;
            outer: for(let i=0;i<4;i++) {
                switch(i) { case 0: let x=7; sum+=x; continue outer; default: break; }
                sum+=i;
            }
            return sum;
        }
        for(let i=0;i<300;i++) {
            try { f(); assert(false); } catch(e) { assert(e instanceof ReferenceError); }
            assert(g()===13);
        }
    "#,
    );
}

#[test]
fn escaped_case_binding_remains_live() {
    check(
        r#"
        function make(x) { switch(x) { case 0: let y=7; return () => ++y; default: return () => 0; } }
        for(let i=0;i<300;i++) { const f=make(0); assert(f()===8 && f()===9 && make(1)()===0); }
    "#,
    );
}
