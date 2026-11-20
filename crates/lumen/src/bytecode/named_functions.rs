//! A named expression may use compiled calls only when its private self binding is unobservable.
use crate::{
    ast::{Expr, Pattern, Stmt},
    bytecode::{self, Tier},
    Completion, Engine,
};

#[test]
fn named_expressions_compile_only_without_self_references() {
    let source = include_str!("../../tests/fixtures/named-functions.js");
    let body =
        crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
    let (mut accepted, mut refused) = (0, 0);
    for statement in &body {
        if let Stmt::VarDecl { decls, .. } = statement {
            for (pattern, value) in decls {
                if let (Pattern::Ident(name), Some(Expr::Func(function))) = (pattern, value) {
                    if name.starts_with("compiled_") {
                        assert!(
                            bytecode::compile(function).is_some(),
                            "{name} should compile"
                        );
                        accepted += 1;
                    } else if name.starts_with("fallback_") {
                        assert!(
                            bytecode::compile(function).is_none(),
                            "{name} needs its self environment"
                        );
                        refused += 1;
                    }
                }
            }
        }
    }
    assert_eq!((accepted, refused), (7, 9));
    let snapshot = crate::compile_snapshot(source).unwrap();
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            for cached in [false, true] {
                let mut engine = Engine::new();
                engine.set_tier(tier);
                engine.set_tier_threshold(threshold);
                let result = if cached {
                    engine.eval_snapshot(&snapshot, false)
                } else {
                    engine.eval(source, false)
                }
                .unwrap_or_else(|e| panic!("{}", e.message));
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
fn named_async_expressions_preserve_capture_and_self_binding() {
    let source = include_str!("../../tests/fixtures/named-functions-async.js");
    let body =
        crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
    let mut checked = 0;
    for statement in &body {
        if let Stmt::VarDecl { decls, .. } = statement {
            for (pattern, value) in decls {
                if let (Pattern::Ident(name), Some(Expr::Func(function))) = (pattern, value) {
                    if name == "compiled_async" || name == "fallback_async" {
                        assert_eq!(
                            bytecode::compile(function).is_some(),
                            name == "compiled_async",
                            "{name}"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 2);
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        engine
            .eval(source, false)
            .unwrap_or_else(|e| panic!("{}", e.message));
        let result = engine
            .eval(
                "asyncError || (asyncDone ? 'passed' : 'not finished')",
                false,
            )
            .unwrap_or_else(|e| panic!("{}", e.message));
        match result {
            Completion::Value(value) => assert_eq!(value, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
