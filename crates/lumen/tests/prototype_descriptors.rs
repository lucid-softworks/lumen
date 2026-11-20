use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn cached_stores_observe_prototype_descriptor_changes() {
    let source = include_str!("fixtures/prototype-descriptors.js");
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
