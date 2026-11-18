//! Arrow functions retain the enclosing arguments binding across tier transitions.
use lumen::{bytecode::Tier, Completion, Engine};

fn check(source: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        // Exercise both immediate compilation and the default call-count transition.
        for immediate in [false, true] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            if immediate {
                engine.set_tier_threshold(0);
            }
            let script = format!(
                "function assert(x) {{ if (!x) throw new Error('assertion'); }}\n{source}\n'passed'"
            );
            match engine.eval(&script, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}, {immediate}"),
                Completion::Throw { name, message } => {
                    panic!("{tier:?}, immediate={immediate}: {name}: {message}")
                }
            }
        }
    }
}

#[test]
fn arrow_reads_arguments_after_loop() {
    check(
        r#"
        function f() {
            for (var i = 0; i < 1; i++) {}
            return (() => arguments[0])();
        }
        assert(f(5) === 5);
        "#,
    );
}

#[test]
fn arrow_reads_arguments_after_call_count_transition() {
    check(
        r#"
        function f() { return (() => arguments[0])(); }
        for (let i = 0; i < 300; i++) assert(f(i) === i);
        "#,
    );
}

#[test]
fn escaping_arrows_share_live_binding_per_call() {
    check(
        r#"
        function make() {
            const original = arguments;
            return {
                read: () => () => arguments,
                write: value => { arguments = value; },
                original: original
            };
        }
        for (let i = 0; i < 300; i++) {
            const a = make(i), b = make(i + 1);
            assert(a.read()() === a.original);
            assert(a.read()()[0] === i && b.read()()[0] === i + 1);
            const replacement = [42];
            a.write(replacement);
            assert(a.read()() === replacement && b.read()() === b.original);
        }
        "#,
    );
}
