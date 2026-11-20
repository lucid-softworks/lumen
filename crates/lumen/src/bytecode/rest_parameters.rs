//! Variadic compiled entries must materialize the same fresh array in every execution path.
use crate::{
    ast::Stmt,
    bytecode::{self, Tier},
    Completion, Engine,
};

fn expect(engine: &mut Engine, source: &str, expected: &str) {
    match engine
        .eval(source, false)
        .unwrap_or_else(|e| panic!("{}", e.message))
    {
        Completion::Value(value) => assert_eq!(value, expected),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn rest_functions_compile_and_preserve_call_semantics() {
    let source = include_str!("../../tests/fixtures/rest-parameters.js");
    let body =
        crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
    for statement in &body {
        if let Stmt::FuncDecl(function) = statement {
            if matches!(
                function.name.as_deref(),
                Some(
                    "collect"
                        | "defaults"
                        | "captured"
                        | "reassigned"
                        | "forward"
                        | "Holder"
                        | "drive"
                )
            ) {
                assert!(
                    bytecode::compile(function).is_some(),
                    "{} should compile",
                    function.name.as_deref().unwrap()
                );
            }
        }
    }
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            expect(&mut engine, source, "passed");
        }
    }
}

#[test]
fn rest_arrays_survive_async_suspension_and_capture() {
    let source = include_str!("../../tests/fixtures/rest-parameters-async.js");
    let body =
        crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
    let function = body
        .iter()
        .find_map(|s| match s {
            Stmt::FuncDecl(f) if f.name.as_deref() == Some("later") => Some(f),
            _ => None,
        })
        .unwrap();
    assert!(bytecode::compile(function).is_some());
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        engine
            .eval(source, false)
            .unwrap_or_else(|e| panic!("{}", e.message));
        expect(
            &mut engine,
            "asyncError || (asyncDone ? 'passed' : 'not finished')",
            "passed",
        );
    }
}

#[test]
fn rest_initialization_handles_large_frames_and_argument_counts() {
    let locals = (0..300)
        .map(|k| format!("let v{k}={k};"))
        .collect::<String>();
    let source = format!(
        r#"
        function large(head, ...tail) {{ {locals} return [head, tail, v299]; }}
        const args = Array.from({{length: 400}}, (_, i) => i);
        function drive() {{
            for(let i=0;i<100;i++) {{
                const empty = large(), values = large(...args);
                if(empty[0] !== undefined || empty[1].length !== 0 || empty[2] !== 299) throw 'empty';
                if(values[0] !== 0 || values[1].length !== 399 || values[1][398] !== 399 || values[2] !== 299) throw 'values';
            }}
        }}
        drive(); 'passed'
    "#
    );
    let body =
        crate::parser::parse_script(&source, false).unwrap_or_else(|e| panic!("{}", e.message));
    for statement in &body {
        if let Stmt::FuncDecl(function) = statement {
            assert!(bytecode::compile(function).is_some());
        }
    }
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        expect(&mut engine, &source, "passed");
    }
}
