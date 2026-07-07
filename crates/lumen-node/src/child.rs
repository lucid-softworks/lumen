//! `node:child_process` over `std::process::Command` — spawning real OS subprocesses (no native
//! addon needed; the child is a separate process lumen just pipes to). Long-running children stream
//! via one-shot `read`/`write`/`wait` ops that run on *dedicated* threads (CompletionSender), since
//! child stdio can block for an unbounded time and must not pin a shared pool worker; plus a
//! synchronous `execSync` for the `*Sync` APIs.
//!
//! Handles live in a [`ChildRegistry`] in OpState, wrapped in `Arc<Mutex<>>` so the worker threads
//! can read/write them. `kill` sends SIGKILL (std can't send arbitrary signals).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use lumen_host::{ops, CompletionSender, Ctx, OpDecl, TaskRegistry, Value};

/// A readable child stream (stdout or stderr), boxed to a common type.
type ReadStream = Arc<Mutex<Option<Box<dyn Read + Send>>>>;

struct ChildProc {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdout: ReadStream,
    stderr: ReadStream,
}

#[derive(Default)]
pub struct ChildRegistry {
    next: u32,
    procs: HashMap<u32, ChildProc>,
}

pub const CHILD_OPS: &[OpDecl] = ops![
    "spawn" (5) => op_spawn,
    "read" (4) => op_read,
    "write" (4) => op_write,
    "wait" (3) => op_wait,
    "kill" (2) => op_kill,
    "closeStdin" (1) => op_close_stdin,
    "execSync" (4) => op_exec_sync,
];

// ---- helpers ----------------------------------------------------------------------------------

fn read_string_array(ctx: &mut Ctx, v: &Value) -> Result<Vec<String>, Value> {
    let mut out = Vec::new();
    if v.as_obj().is_none() {
        return Ok(out);
    }
    let len = match ctx.get_member(v, "length") {
        Ok(Value::Num(n)) => n as usize,
        _ => return Ok(out),
    };
    for i in 0..len {
        let el = ctx.get_member(v, &i.to_string()).unwrap_or(Value::Undefined);
        out.push(ctx.coerce_string(&el)?.to_string());
    }
    Ok(out)
}

/// `["pipe"|"inherit"|"ignore", ...]` → the Stdio for one fd.
fn stdio_for(name: &str) -> Stdio {
    match name {
        "inherit" => Stdio::inherit(),
        "ignore" => Stdio::null(),
        _ => Stdio::piped(),
    }
}

fn opt_string(ctx: &mut Ctx, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Str(s)) => Some(s.to_string()),
        Some(v) if !matches!(v, Value::Undefined | Value::Null) => {
            ctx.coerce_string(v).ok().map(|s| s.to_string())
        }
        _ => None,
    }
}

fn build_command(
    ctx: &mut Ctx,
    cmd: &str,
    args: &[Value],
) -> Result<(Command, [String; 3]), Value> {
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let cwd = opt_string(ctx, args.get(2));
    let env_pairs = args.get(3).cloned().unwrap_or(Value::Undefined);
    let stdio = read_string_array(ctx, args.get(4).unwrap_or(&Value::Undefined))?;

    let mut command = Command::new(cmd);
    command.args(&arg_list);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // env: an array of [k, v] pairs *replaces* the environment (Node semantics); absent = inherit.
    if env_pairs.as_obj().is_some() {
        if let Ok(Value::Num(n)) = ctx.get_member(&env_pairs, "length") {
            command.env_clear();
            for i in 0..(n as usize) {
                let pair = ctx.get_member(&env_pairs, &i.to_string()).unwrap_or(Value::Undefined);
                let k = ctx.get_member(&pair, "0").unwrap_or(Value::Undefined);
                let v = ctx.get_member(&pair, "1").unwrap_or(Value::Undefined);
                command.env(ctx.coerce_string(&k)?.to_string(), ctx.coerce_string(&v)?.to_string());
            }
        }
    }
    let s = |i: usize| stdio.get(i).cloned().unwrap_or_else(|| "pipe".to_string());
    Ok((command, [s(0), s(1), s(2)]))
}

// ---- ops --------------------------------------------------------------------------------------

/// `(cmd, argsArray, cwd, envPairsOrNull, stdioArray) -> { childId, pid }`.
fn op_spawn(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let cmd = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let (mut command, stdio) = build_command(ctx, &cmd, args)?;
    command
        .stdin(stdio_for(&stdio[0]))
        .stdout(stdio_for(&stdio[1]))
        .stderr(stdio_for(&stdio[2]));

    let mut child = command
        .spawn()
        .map_err(|e| ctx.make_error("Error", format!("spawn {cmd}: {e}")))?;
    let pid = child.id();
    let stdout: Option<Box<dyn Read + Send>> = child.stdout.take().map(|s| Box::new(s) as _);
    let stderr: Option<Box<dyn Read + Send>> = child.stderr.take().map(|s| Box::new(s) as _);
    let proc = ChildProc {
        stdin: Arc::new(Mutex::new(child.stdin.take())),
        stdout: Arc::new(Mutex::new(stdout)),
        stderr: Arc::new(Mutex::new(stderr)),
        child: Arc::new(Mutex::new(child)),
    };
    let reg = ctx.host_mut::<ChildRegistry>().expect("child registry installed");
    let id = reg.next;
    reg.next += 1;
    reg.procs.insert(id, proc);

    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "childId", Value::Num(id as f64));
    let _ = ctx.set_member(&o, "pid", Value::Num(pid as f64));
    Ok(o)
}

fn take_resolve_reject(ctx: &mut Ctx, res: Option<&Value>, rej: Option<&Value>) -> Result<(Value, Value), Value> {
    match (res, rej) {
        (Some(r), Some(j)) if r.is_callable() && j.is_callable() => Ok((r.clone(), j.clone())),
        _ => Err(ctx.make_error("TypeError", "child op expects (resolve, reject)")),
    }
}

/// Child stdio blocks for an unbounded time, so it runs on dedicated threads (via CompletionSender)
/// rather than the shared pool — otherwise a long-lived child (e.g. an esbuild service) would pin
/// pool workers for its whole lifetime and starve everything else.
fn completions(ctx: &mut Ctx) -> CompletionSender {
    ctx.op_state()
        .get::<CompletionSender>()
        .expect("runtime installs the completion sender")
        .clone()
}

/// `(childId, which, resolve, reject)` — read a chunk from stdout (which=1) or stderr (which=2).
/// Resolves with a Uint8Array, or `null` at EOF.
fn op_read(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let which = args.get(1).and_then(Value::as_num_opt).unwrap_or(1.0) as u32;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(2), args.get(3))?;

    let handle = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| if which == 2 { p.stderr.clone() } else { p.stdout.clone() })
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process"))?;

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("registry")
        .register(resolve, Some(reject), decode_read);
    completions(ctx).run_blocking(id, move || {
        let mut guard = handle.lock().expect("stream lock");
        let result: Result<Vec<u8>, String> = match guard.as_mut() {
            Some(stream) => {
                let mut buf = vec![0u8; 65536];
                match stream.read(&mut buf) {
                    Ok(0) => Ok(Vec::new()),
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(e) => Err(format!("read: {e}")),
                }
            }
            None => Ok(Vec::new()),
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<Vec<u8>, String>>().expect("read payload") {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]), // EOF
        Ok(bytes) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `(childId, bytes, resolve, reject)` — write to the child's stdin.
fn op_write(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let data = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "child write expects bytes"))?;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(2), args.get(3))?;

    let handle = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| p.stdin.clone())
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process"))?;

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("registry")
        .register(resolve, Some(reject), decode_ok);
    completions(ctx).run_blocking(id, move || {
        let mut guard = handle.lock().expect("stdin lock");
        let result: Result<(), String> = match guard.as_mut() {
            Some(stdin) => stdin.write_all(&data).and_then(|()| stdin.flush()).map_err(|e| format!("write: {e}")),
            None => Err("child stdin is closed".to_string()),
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_ok(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<(), String>>().expect("ok payload") {
        Ok(()) => Ok(vec![]),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `(childId, resolve, reject)` — wait for exit; resolves with the exit code (or null if signaled).
fn op_wait(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(1), args.get(2))?;
    let handle = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| p.child.clone())
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process"))?;

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("registry")
        .register(resolve, Some(reject), decode_exit);
    completions(ctx).run_blocking(id, move || {
        // Poll try_wait(), releasing the child lock between checks, so `kill()` can acquire it (a
        // blocking wait() would hold the lock for the child's whole life and deadlock kill).
        let result: Result<Option<i32>, String> = loop {
            {
                match handle.lock().expect("child lock").try_wait() {
                    Ok(Some(status)) => break Ok(status.code()),
                    Ok(None) => {}
                    Err(e) => break Err(e.to_string()),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_exit(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<Option<i32>, String>>().expect("exit payload") {
        Ok(Some(code)) => Ok(vec![Value::Num(code as f64)]),
        Ok(None) => Ok(vec![Value::Null]), // terminated by signal
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

fn op_kill(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let killed = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| p.child.lock().expect("child lock").kill().is_ok())
        .unwrap_or(false);
    Ok(Value::Bool(killed))
}

fn op_close_stdin(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    if let Some(p) = ctx.host_mut::<ChildRegistry>().and_then(|r| r.procs.get(&child_id)) {
        p.stdin.lock().expect("stdin lock").take(); // drop → EOF to the child
    }
    Ok(Value::Undefined)
}

/// `(cmd, argsArray, inputBytesOrNull, cwd) -> { stdout, stderr, status }`. Synchronous: spawns,
/// writes optional stdin, waits, and captures output — for `execFileSync`/`spawnSync`/`execSync`.
fn op_exec_sync(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let cmd = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let input = ctx.typed_array_bytes(args.get(2).unwrap_or(&Value::Undefined));
    let cwd = opt_string(ctx, args.get(3));

    let mut command = Command::new(&cmd);
    command.args(&arg_list).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .map_err(|e| ctx.make_error("Error", format!("spawn {cmd}: {e}")))?;
    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&input); // dropping stdin here signals EOF
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| ctx.make_error("Error", format!("exec {cmd}: {e}")))?;

    let o = Value::Obj(ctx.new_object());
    let stdout = ctx.make_uint8array(&output.stdout)?;
    let stderr = ctx.make_uint8array(&output.stderr)?;
    let _ = ctx.set_member(&o, "stdout", stdout);
    let _ = ctx.set_member(&o, "stderr", stderr);
    let _ = ctx.set_member(&o, "status", output.status.code().map(|c| Value::Num(c as f64)).unwrap_or(Value::Null));
    Ok(o)
}
