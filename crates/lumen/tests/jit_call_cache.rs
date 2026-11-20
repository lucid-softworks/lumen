use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn cached_calls_preserve_dispatch_receivers_and_operand_ownership() {
    let source = include_str!("fixtures/jit-call-cache.js");
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            match engine.eval(source, false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}, {threshold}"),
                Completion::Throw { name, message } => {
                    panic!("{tier:?}, {threshold}: {name}: {message}")
                }
            }
        }
    }
}
