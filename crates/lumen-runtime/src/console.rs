//! Streaming `console`: writes as it is called, replacing the engine's buffered test262
//! console. `log`/`info`/`debug` go to the out sink, `warn`/`error` to the err sink; the
//! sinks live in `OpState` so tests (and later, embedders) can capture them.

use std::io::Write;

use lumen_host::{ops, Ctx, Extension, OpState, Value};

pub(crate) fn extension() -> Extension {
    Extension {
        name: "console",
        globals: &[],
        namespaces: &[(
            "console",
            ops![
                "log" (0) => op_log,
                "info" (0) => op_log,
                "debug" (0) => op_log,
                "warn" (0) => op_warn,
                "error" (0) => op_error,
                "__reportUncaught" (1) => op_report_uncaught,
            ],
        )],
        state_init: Some(|state: &mut OpState| state.put(ConsoleOut::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}

/// Where `console` output goes. Default: the process's stdout/stderr.
pub struct ConsoleOut {
    pub out: Box<dyn Write>,
    pub err: Box<dyn Write>,
}

impl Default for ConsoleOut {
    fn default() -> Self {
        ConsoleOut {
            out: Box::new(std::io::stdout()),
            err: Box::new(std::io::stderr()),
        }
    }
}

/// Render one argument roughly the way Node does for the common cases: strings bare, symbols
/// by description, everything else through ToString. Never throws — a value whose `toString`
/// throws prints as its typeof. (A real `util.inspect` is future work.)
fn render(ctx: &mut Ctx, v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Sym(s) => match &s.description {
            Some(d) => format!("Symbol({d})"),
            None => "Symbol()".into(),
        },
        other => match ctx.coerce_string(other) {
            Ok(s) => s.to_string(),
            Err(_) => format!("[{}]", other.type_of()),
        },
    }
}

/// Space-joined arguments, one line, to the chosen sink.
fn write_line(ctx: &mut Ctx, args: &[Value], to_err: bool) -> Result<Value, Value> {
    let parts: Vec<String> = args.iter().map(|a| render(ctx, a)).collect();
    let line = parts.join(" ");
    let sinks = ctx
        .host_mut::<ConsoleOut>()
        .expect("console state installed");
    let sink = if to_err {
        &mut sinks.err
    } else {
        &mut sinks.out
    };
    // A broken pipe shouldn't take the whole runtime down; console errors are swallowed.
    let _ = writeln!(sink, "{line}");
    let _ = sink.flush();
    Ok(Value::Undefined)
}

fn op_log(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_line(ctx, args, false)
}

fn op_warn(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_line(ctx, args, true)
}

fn op_error(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_line(ctx, args, true)
}

/// `reportError`'s default report (after the global `onerror` declined to suppress): the same
/// `Uncaught <error>` line on the error sink that an uncaught loop-callback error produces.
fn op_report_uncaught(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let e = args.first().cloned().unwrap_or(Value::Undefined);
    let text = describe_error(ctx, &e);
    write_err_line(ctx, format!("Uncaught {text}"));
    Ok(Value::Undefined)
}

/// [`render`] for hosts above the runtime (the REPL prints results with it).
pub fn render_value(ctx: &mut Ctx, v: &Value) -> String {
    render(ctx, v)
}

/// `"TypeError: boom"` for an Error object, else the rendered value — for uncaught reports.
pub fn describe_error(ctx: &mut Ctx, error: &Value) -> String {
    if error.as_obj().is_some() {
        let name = ctx
            .get_member(error, "name")
            .ok()
            .filter(|v| !matches!(v, Value::Undefined))
            .map(|v| render(ctx, &v));
        let message = ctx
            .get_member(error, "message")
            .ok()
            .filter(|v| !matches!(v, Value::Undefined))
            .map(|v| render(ctx, &v));
        if let Some(name) = name {
            return match message {
                Some(m) if !m.is_empty() => format!("{name}: {m}"),
                _ => name,
            };
        }
    }
    render(ctx, error)
}

/// A line straight to the err sink (uncaught-exception reports, not `console.error`).
pub(crate) fn write_err_line(ctx: &mut Ctx, line: String) {
    let sinks = ctx
        .host_mut::<ConsoleOut>()
        .expect("console state installed");
    let _ = writeln!(sinks.err, "{line}");
    let _ = sinks.err.flush();
}
