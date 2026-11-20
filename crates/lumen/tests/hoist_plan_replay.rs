use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn hoisted_bindings_remain_separate_across_calls_and_suspended_activations() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            for source in [
                include_str!("fixtures/hoist-plan-replay-async.js"),
                "if (!asyncDone || asyncError) throw Error(asyncError || 'unfinished'); 'passed'",
            ] {
                match engine
                    .eval(source, false)
                    .expect("evaluate hoist regression")
                {
                    Completion::Value(_) => {}
                    Completion::Throw { name, message } => {
                        panic!("{tier:?}, threshold {threshold}: {name}: {message}");
                    }
                }
            }
        }
    }
}
