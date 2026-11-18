//! Evaluation order, array extensibility, and nested async parameter regressions.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let script = format!(
            "'use strict';\nfunction assert(x) {{ if (!x) throw new Error('assertion'); }}\n{source}\n'passed'"
        );
        match engine.eval(&script, false).unwrap() {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}

#[test]
fn tail_call_errors_evaluate_receivers_callees_and_arguments_once() {
    check(
        r#"
        let log = '';
        function nil() { log += 'r'; return null; }
        function bad() { log += 'c'; return 0; }
        function arg() { log += 'a'; return 1; }
        const obj = { get f() { log += 'g'; return 0; } };
        function throws(fn, expected) {
            log = '';
            let threw = false;
            try { fn(); } catch (e) { threw = true; assert(e instanceof TypeError); }
            assert(threw && log === expected);
        }
        throws(() => nil().f(arg()), 'r');
        throws(() => bad()(arg()), 'ca');
        throws(() => obj.f(arg()), 'ga');
        throws(() => nil().f`${arg()}`, 'r');
        throws(() => bad()`${arg()}`, 'ca');
        throws(() => obj.f`${arg()}`, 'ga');
        const sentinel = {};
        function fail() { log += 'a'; throw sentinel; }
        log = '';
        try { (() => bad()(fail()))(); } catch (e) { assert(e === sentinel); }
        assert(log === 'ca');
        log = '';
        try { (() => bad()`${fail()}`)(); } catch (e) { assert(e === sentinel); }
        assert(log === 'ca');
    "#,
    );
}

#[test]
fn tail_optional_chains_short_circuit_once() {
    check(
        r#"
        let n = 0;
        function nil() { n++; return null; }
        function fail() { throw new Error('argument evaluated'); }
        const member = () => nil()?.f(fail());
        const call = () => nil()?.()(fail());
        const nested = () => nil()?.f.g(fail());
        assert(member() === undefined && n === 1);
        assert(call() === undefined && n === 2);
        assert(nested() === undefined && n === 3);
    "#,
    );
}

#[test]
fn push_respects_non_extensible_arrays() {
    check(
        r#"
        for (const a of [[], [1], new Array(3)]) {
            const len = a.length;
            Object.preventExtensions(a);
            assert(a.push() === len);
            let threw = false;
            try { a.push(2, 3); } catch (e) { threw = true; assert(e instanceof TypeError); }
            assert(threw && a.length === len && !Object.hasOwn(a, len));
        }
        const ordinary = [1];
        assert(ordinary.push(2, 3) === 3 && ordinary.join() === '1,2,3');
    "#,
    );
}

#[test]
fn nested_async_arrow_bodies_leave_parameter_context() {
    check(
        r#"
        const g = async (p = async (q) => await q) => 1;
        const h = async (p = async q => await q) => 1;
        const block = async (p = async () => { await null; }) => 1;
        const plain = (p = async q => await q) => p;
        assert(typeof g === 'function' && typeof h === 'function');
        assert(typeof block === 'function' && typeof plain() === 'function');
        for (const source of [
            'async (p = await 1) => 1',
            'async (p = async q => await q, r = await 1) => 1',
            'async (p = async (q = await 1) => q) => 1'
        ]) {
            let threw = false;
            try { eval(source); } catch (e) { threw = true; assert(e instanceof SyntaxError); }
            assert(threw);
        }
    "#,
    );
}
