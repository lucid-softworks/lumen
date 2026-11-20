use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn internal_data_properties_preserve_exotic_objects_and_proxy_descriptors() {
    let source = include_str!("fixtures/create-data-property.js");
    let snapshot = lumen::compile_snapshot(source).unwrap();
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for cached in [false, true] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let result = if cached {
                engine.eval_snapshot(&snapshot, false)
            } else {
                engine.eval(source, false)
            }
            .unwrap();
            match result {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}, {cached}"),
                Completion::Throw { name, message } => {
                    panic!("{tier:?}, {cached}: {name}: {message}")
                }
            }
        }
    }
}
