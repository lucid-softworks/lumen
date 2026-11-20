use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn whitespace_and_comments_preserve_asi_and_function_source() {
    let source = include_str!("fixtures/lexer-trivia.js");
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
