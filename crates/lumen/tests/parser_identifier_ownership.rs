use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn parsed_identifiers_outlive_temporary_sources_and_relexing() {
    fn value(engine: &mut Engine, source: &str) -> String {
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => value,
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            let source = String::from(
                r#"
                function preservedFunction(parameterName) {
                    const pl\u0061inName = parameterName;
                    const π = 3;
                    const object = {ordinaryName: plainName};
                    return `${(() => { const nestedName = object.ordinaryName + π; return nestedName; })()}`;
                }
                function dividedAwait() { var await = 48, divisor = 6; return await / divisor / 2; }
                function dividedYield() { var yield = 60, divisor = 5; return yield / divisor / 3; }
                function labelled() { outerLabel: for (;;) { break outerLabel; } return 11; }
                'created';
                "#,
            );
            assert_eq!(value(&mut engine, &source), "created");
            drop(source);
            assert_eq!(
                value(
                    &mut engine,
                    "[preservedFunction(9), preservedFunction.name, dividedAwait(), dividedYield(), labelled()].join(',');"
                ),
                "12,preservedFunction,4,4,11",
                "{tier:?}, threshold {threshold}"
            );
        }
    }
}
