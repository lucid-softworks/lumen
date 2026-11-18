//! Early for-await exits suspend until iterator closing settles.
use lumen::{bytecode::Tier, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).expect("evaluate") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn close_does_not_drain_unrelated_microtasks() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            r#"
            var log = [];
            async function* g() { yield 1; }
            (async () => {
                for await (const x of g()) { log.push(x); break; }
                log.push('after-loop');
            })().then(() => log.push('done'), e => log.push(e.name));
            let ticks = 0;
            function tick() {
                if (++ticks < 100) Promise.resolve().then(tick);
                else log.push('ticks');
            }
            tick();
            "#,
        );
        assert_eq!(
            eval(&mut engine, "log.join()"),
            "1,after-loop,done,ticks",
            "{tier:?}"
        );
    }
}

#[test]
fn close_waits_for_later_settlement_and_preserves_completion() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for from_sync in [false, true] {
            for exit in ["break;", "return 'returned';", "throw 'body-error';"] {
                for settlement in [
                    "resolveClose({ done: true });",
                    "rejectClose('close-error');",
                    "resolveClose(1);",
                ] {
                    let mut engine = Engine::new();
                    engine.set_tier(tier);
                    engine.set_tier_threshold(0);
                    let symbol = if from_sync {
                        "iterator"
                    } else {
                        "asyncIterator"
                    };
                    let close_result = if from_sync {
                        "{ value: pending, done: true }"
                    } else {
                        "pending"
                    };
                    eval(
                        &mut engine,
                        &format!(
                            r#"
                        var log = [], resolveClose, rejectClose;
                        const pending = new Promise((resolve, reject) => {{
                            resolveClose = resolve; rejectClose = reject;
                        }});
                        const it = {{
                            [Symbol.{symbol}]() {{ return this; }},
                            next() {{ return {{ value: 1, done: false }}; }},
                            return() {{ log.push('close:' + arguments.length); return {close_result}; }}
                        }};
                        (async () => {{
                            for await (const x of it) {{ {exit} }}
                            log.push('after-loop');
                            return 'finished';
                        }})().then(v => log.push(v), e => log.push(e.name || e));
                    "#
                        ),
                    );
                    // A second host turn models a timer/event resolving the close promise,
                    // after the first turn's microtask queue has become empty.
                    assert_eq!(
                        eval(&mut engine, "log.join()"),
                        "close:0",
                        "{tier:?}, sync={from_sync}, {exit}, {settlement}"
                    );
                    eval(&mut engine, settlement);
                    let expected = if exit.starts_with("throw") {
                        "close:0,body-error"
                    } else if settlement.starts_with("reject") {
                        "close:0,close-error"
                    } else if !from_sync && settlement == "resolveClose(1);" {
                        "close:0,TypeError"
                    } else if exit.starts_with("return") {
                        "close:0,returned"
                    } else {
                        "close:0,after-loop,finished"
                    };
                    assert_eq!(
                        eval(&mut engine, "log.join()"),
                        expected,
                        "{tier:?}, sync={from_sync}, {exit}, {settlement}"
                    );
                }
            }
        }
    }
}
