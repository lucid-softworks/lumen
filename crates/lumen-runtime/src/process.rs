//! Minimal `process`: argv/env/platform snapshots, `cwd()`, `exit()`, `nextTick()`. Just
//! enough for scripts to orient themselves; the full `node:process` surface belongs to the
//! future lumen-node compat crate.

use lumen_host::{ops, CallbackQueue, Ctx, Engine, Extension, Value};

pub(crate) fn extension() -> Extension {
    Extension {
        name: "process",
        globals: &[],
        namespaces: &[(
            "process",
            ops![
                "cwd" (0) => op_cwd,
                "exit" (1) => op_exit,
                "nextTick" (1) => op_next_tick,
            ],
        )],
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
    }
}

/// The data properties (`argv`, `env`, `platform`) — snapshots taken at startup, like Node's.
/// Runs after `install` because it needs the `process` object that install created.
pub(crate) fn install_data_props(engine: &mut Engine) {
    let global = engine.global_this();
    let ctx = engine.ctx();
    let process = match ctx.get_member(&global, "process") {
        Ok(v @ Value::Obj(_)) => v,
        _ => unreachable!("install() defined the process namespace"),
    };

    let argv: Vec<Value> = std::env::args().map(Value::from_string).collect();
    let argv = ctx.make_array(argv);
    let _ = ctx.set_member(&process, "argv", argv);

    let env = ctx.new_object();
    let env = Value::Obj(env);
    for (k, v) in std::env::vars() {
        let _ = ctx.set_member(&env, &k, Value::from_string(v));
    }
    let _ = ctx.set_member(&process, "env", env);

    let platform = match std::env::consts::OS {
        "macos" => "darwin", // Node's name for it
        other => other,
    };
    let _ = ctx.set_member(&process, "platform", Value::str(platform));
}

fn op_cwd(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    match std::env::current_dir() {
        Ok(p) => Ok(Value::from_string(p.to_string_lossy().into_owned())),
        Err(e) => Err(ctx.make_error("Error", format!("cwd unavailable: {e}"))),
    }
}

fn op_exit(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let code = match args.first() {
        Some(v) => ctx.coerce_number(v)? as i32,
        None => 0,
    };
    std::process::exit(code);
}

/// Queued on the loop's callback queue: runs next turn, before timers. (Node runs nextTick
/// callbacks even sooner — between microtask checkpoints — but "before any timer" is the
/// part programs rely on; noted as a deliberate approximation.)
fn op_next_tick(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let callback = match args.first() {
        Some(cb) if cb.is_callable() => cb.clone(),
        _ => return Err(ctx.make_error("TypeError", "process.nextTick expects a function")),
    };
    let extra: Vec<Value> = args.iter().skip(1).cloned().collect();
    CallbackQueue::enqueue(ctx.op_state(), callback, extra);
    Ok(Value::Undefined)
}
