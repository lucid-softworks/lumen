use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn functions_keep_exact_source_after_the_callers_buffer_is_reused() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for cached in [false, true] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let mut text = include_str!("fixtures/shared-function-source.js").to_owned();
            let result = if cached {
                let mut bytes = lumen::compile_snapshot(&text).unwrap();
                let result = engine.eval_snapshot(&bytes, false);
                bytes.fill(0xff);
                result
            } else {
                engine.eval(&text, false)
            };
            match result.unwrap() {
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
                Completion::Value(_) => {}
            }
            text.clear();
            text.push_str("caller buffer has been reused");
            drop(text);
            match engine.eval("verifySavedSources()", false).unwrap() {
                Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}, {cached}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }
}
