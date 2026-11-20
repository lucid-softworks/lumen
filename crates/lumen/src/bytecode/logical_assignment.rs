use crate::{
    ast::Stmt,
    bytecode::{self, Tier},
    Completion, Engine,
};

pub(super) fn exercise(source: &str, accepted: usize, refused: usize, async_result: bool) {
    let body =
        crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
    let (mut compiled, mut fallback) = (0, 0);
    for statement in &body {
        if let Stmt::FuncDecl(function) = statement {
            let name = function.name.as_deref().unwrap_or("");
            if name.starts_with("compiled_") {
                assert!(bytecode::compile(function).is_some(), "{name} must compile");
                compiled += 1;
            } else if name.starts_with("fallback_") {
                assert!(
                    bytecode::compile(function).is_none(),
                    "{name} must retain fallback"
                );
                fallback += 1;
            }
        }
    }
    assert_eq!((compiled, fallback), (accepted, refused));
    let snapshot = crate::compile_snapshot(source).unwrap();
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            for cached in [false, true] {
                let mut engine = Engine::new();
                engine.set_tier(tier);
                engine.set_tier_threshold(threshold);
                let mut result = if cached {
                    engine.eval_snapshot(&snapshot, false)
                } else {
                    engine.eval(source, false)
                }
                .unwrap_or_else(|e| panic!("{}", e.message));
                if async_result {
                    if let Completion::Throw { name, message } = result {
                        panic!("{name}: {message}");
                    }
                    result = engine
                        .eval(
                            "logicalAsyncError || (logicalAsyncDone ? 'passed' : 'unfinished')",
                            false,
                        )
                        .unwrap_or_else(|e| panic!("{}", e.message));
                }
                match result {
                    Completion::Value(value) => assert_eq!(
                        value, "passed",
                        "{tier:?}, threshold {threshold}, cached {cached}"
                    ),
                    Completion::Throw { name, message } => panic!(
                        "{tier:?}, threshold {threshold}, cached {cached}: {name}: {message}"
                    ),
                }
            }
        }
    }
}

#[test]
fn logical_assignments_preserve_short_circuit_and_reference_order() {
    exercise(
        include_str!("../../tests/fixtures/logical-assignment.js"),
        12,
        2,
        false,
    );
}

#[test]
fn logical_assignments_preserve_references_across_await() {
    exercise(
        include_str!("../../tests/fixtures/logical-assignment-async.js"),
        2,
        0,
        true,
    );
}
