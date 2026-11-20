use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn method_lookup_preserves_receiver_callee_and_live_descriptors() {
    let source = include_str!("fixtures/jit-method-lookup.js");
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            match engine.eval(source, false).unwrap() {
                Completion::Value(value) => {
                    assert_eq!(value, "passed", "{tier:?}, threshold {threshold}")
                }
                Completion::Throw { name, message } => {
                    panic!("{tier:?}, threshold {threshold}: {name}: {message}")
                }
            }
        }
    }
}

#[test]
fn shared_property_probes_keep_each_live_engine_realm() {
    fn evaluate(engine: &mut Engine, source: &str) -> String {
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => value,
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }
    let mut first = Engine::new();
    let mut second = Engine::new();
    for (engine, marker) in [(&mut first, "first"), (&mut second, "second")] {
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        let source = format!(
            "String.prototype.marker = function() {{ return '{marker}'; }};\n\
             function read(value) {{ let result; for (let i = 0; i < 20; i++) result = value.marker(); return result; }}\n\
             read('word');"
        );
        assert_eq!(evaluate(engine, &source), marker);
    }
    for _ in 0..3 {
        assert_eq!(evaluate(&mut first, "read('first');"), "first");
        assert_eq!(evaluate(&mut second, "read('second');"), "second");
    }
    drop(second);
    assert_eq!(evaluate(&mut first, "read('survives');"), "first");
    assert_eq!(
        evaluate(
            &mut first,
            "String.prototype.marker = function() { return 'changed'; }; read('changed');"
        ),
        "changed"
    );
}
