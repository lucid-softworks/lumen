use super::*;

/// The Phase-1 acceptance test: an external crate registers a native global and JS calls it.
fn host_add(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let a = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))?;
    let b = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))?;
    Ok(Value::Num(a + b))
}

fn eval_str(engine: &mut Engine, src: &str) -> String {
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn native_global_callable_from_js() {
    let mut engine = Engine::new();
    engine.define_global("hostAdd", 2, host_add);
    assert_eq!(eval_str(&mut engine, "hostAdd(2, 40)"), "42");
    // Coercion path (user valueOf) works, and a native fn is a real function object.
    assert_eq!(
        eval_str(&mut engine, "hostAdd({ valueOf: () => 1 }, '2.5')"),
        "3.5"
    );
    assert_eq!(eval_str(&mut engine, "typeof hostAdd"), "function");
    assert_eq!(eval_str(&mut engine, "hostAdd.length"), "2");
}

#[test]
fn extension_installs_state_globals_and_namespaces() {
    struct Counter(u32);
    fn bump(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
        let c = ctx.host_mut::<Counter>().expect("state_init ran");
        c.0 += 1;
        Ok(Value::Num(c.0 as f64))
    }
    static EXT: Extension = Extension {
        name: "counter",
        globals: ops!["bump" (0) => bump],
        namespaces: &[("counterNs", ops!["bumpToo" (0) => bump])],
        state_init: Some(|state| state.put(Counter(0))),
    };
    let mut engine = Engine::new();
    install(&mut engine, std::slice::from_ref(&EXT));
    assert_eq!(eval_str(&mut engine, "bump(); bump()"), "2");
    assert_eq!(eval_str(&mut engine, "counterNs.bumpToo()"), "3");
}

#[test]
fn native_fn_survives_the_bytecode_tier() {
    // The embed hooks must work on every execution tier (a native call from JIT/bytecode
    // frames goes through the same Callable::Native dispatch, but verify, don't assume).
    let mut engine = Engine::new();
    engine.set_tier(lumen::bytecode::Tier::Bytecode);
    engine.set_tier_threshold(0);
    engine.define_global("hostAdd", 2, host_add);
    assert_eq!(
        eval_str(
            &mut engine,
            "function f() { let s = 0; for (let i = 0; i < 1000; i++) s = hostAdd(s, 1); return s } f(); f()"
        ),
        "1000"
    );
}

#[test]
fn call_function_reenters_the_engine() {
    let mut engine = Engine::new();
    match engine
        .eval("globalThis.cb = (x) => x * 2; 0", false)
        .unwrap()
    {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    let g = global_this(&mut engine);
    let cb = engine
        .ctx()
        .get_member(&g, "cb")
        .map_err(|_| ())
        .expect("cb defined");
    let out = engine
        .call_function(&cb, Value::Undefined, &[Value::Num(21.0)])
        .map_err(|_| "js threw")
        .expect("call ok");
    assert!(matches!(out, Value::Num(n) if n == 42.0));
}

fn global_this(engine: &mut Engine) -> Value {
    engine.global_this()
}

#[test]
fn microtask_hooks_drive_promises() {
    let mut engine = Engine::new();
    // Queue a reaction without draining: eval() drains at the end, so queue one from a
    // native call instead... simplest honest check: after eval, nothing pending; then a
    // manually enqueued callback via call_function leaves the queue drainable by hand.
    engine
        .eval(
            "globalThis.done = 0; Promise.resolve(1).then(v => done = v)",
            false,
        )
        .unwrap();
    // Engine::eval already ran the checkpoint: the reaction fired.
    assert_eq!(eval_str(&mut engine, "done"), "1");
    assert!(!engine.has_pending_jobs());
    // Now queue a job by calling a promise-creating function through call_function (which
    // does NOT run a microtask checkpoint) and drain it step by step.
    engine
        .eval(
            "globalThis.queue = () => { Promise.resolve(2).then(v => done = v) }",
            false,
        )
        .unwrap();
    let queue = {
        let g = global_this(&mut engine);
        engine
            .ctx()
            .get_member(&g, "queue")
            .map_err(|_| ())
            .unwrap()
    };
    engine
        .call_function(&queue, Value::Undefined, &[])
        .map_err(|_| "js threw")
        .unwrap();
    assert!(engine.has_pending_jobs());
    assert!(engine.run_one_job());
    engine.run_microtasks();
    assert!(!engine.has_pending_jobs());
    assert_eq!(eval_str(&mut engine, "done"), "2");
}

#[test]
fn resource_table_roundtrip() {
    let mut table = ResourceTable::default();
    let rid = table.add(String::from("a file handle"));
    assert!(table.has(rid));
    assert_eq!(*table.get::<String>(rid).unwrap(), "a file handle");
    assert!(table.get::<u32>(rid).is_none(), "downcast is type-checked");
    assert!(table.close(rid).is_some());
    assert!(!table.has(rid));
    assert!(table.is_empty());
}

#[test]
fn threadpool_completions_arrive() {
    let (tx, rx) = std::sync::mpsc::channel();
    let pool = ThreadPool::new(4, tx);
    for id in 0..16u64 {
        pool.spawn_blocking(id, move || {
            Box::new(id * 2) as Box<dyn std::any::Any + Send>
        });
    }
    let mut seen = std::collections::HashMap::new();
    for _ in 0..16 {
        let done = rx.recv().expect("completion");
        seen.insert(done.task, *done.result.downcast::<u64>().unwrap());
    }
    assert_eq!(seen.len(), 16);
    assert!((0..16).all(|id| seen[&id] == id * 2));
    drop(pool); // joins workers; must not deadlock
}
