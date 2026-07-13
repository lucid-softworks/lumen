//! Minimal `process`: argv/env/platform snapshots, `cwd()`, `exit()`, `nextTick()`, `hrtime()`,
//! and `stdout`/`stderr` writable streams. Enough for scripts to orient themselves and for the
//! Node ecosystem (morgan et al.) to log and time; the fuller `node:process` surface is layered
//! on in lumen-node.

use std::io::{Read, Write};
use std::time::Instant;

use lumen_host::{
    ops, CallbackQueue, CompletionSender, Ctx, Engine, Extension, OpState, TaskRegistry, Value,
};

use crate::console::ConsoleOut;

/// Process-start monotonic reference for `hrtime()`.
struct ProcStart(Instant);

pub(crate) fn extension() -> Extension {
    Extension {
        name: "process",
        globals: &[],
        namespaces: &[
            (
                "process",
                ops![
                    "cwd" (0) => op_cwd,
                    "exit" (1) => op_exit,
                    "nextTick" (1) => op_next_tick,
                ],
            ),
            // Internal primitives the js_init below wraps into process.stdout/stderr/hrtime, then
            // deletes the namespace (the capture-and-delete pattern the op crates use).
            (
                "__proc",
                ops![
                    "writeStdout" (1) => op_write_stdout,
                    "writeStderr" (1) => op_write_stderr,
                    "readStdin" (2) => op_read_stdin,
                    "hrtime" (0) => op_hrtime,
                    "chdir" (1) => op_chdir,
                    "abort" (0) => op_abort,
                    "kill" (2) => op_kill,
                    "umask" (1) => op_umask,
                    "getuid" (0) => op_getuid,
                    "geteuid" (0) => op_geteuid,
                    "getgid" (0) => op_getgid,
                    "getegid" (0) => op_getegid,
                    "getppid" (0) => op_getppid,
                    "execve" (3) => op_execve,
                    "setuid" (1) => op_setuid,
                    "seteuid" (1) => op_seteuid,
                    "setgid" (1) => op_setgid,
                    "setegid" (1) => op_setegid,
                    "getgroups" (0) => op_getgroups,
                    "setgroups" (1) => op_setgroups,
                    "initgroups" (2) => op_initgroups,
                    "metrics" (0) => op_metrics,
                ],
            ),
        ],
        state_init: Some(|state: &mut OpState| state.put(ProcStart(Instant::now()))),
        js_init: Some(JS_INIT),
        js_init_snapshot: None,
    }
}

/// Shapes `process.stdout`/`stderr` (over the raw write ops) and `process.hrtime` (over the
/// monotonic op), and stamps `version`/`versions`. We report a Node version string because the
/// ecosystem branches on it for feature detection; `versions.lumen` records the real engine.
const JS_INIT: &str = r#"(() => {
  const proc = globalThis.__proc;
  const readStdin = proc.readStdin;
  const rawExecve = proc.execve;
  const metrics = proc.metrics;
  delete globalThis.__proc;
  const process = globalThis.process;

  const makeStream = (writeFn, fd) => ({
    write(chunk) { writeFn(chunk); return true; },
    end(chunk) { if (chunk != null) writeFn(chunk); return this; },
    isTTY: false,
    fd,
    columns: 80,
    rows: 24,
    // morgan/debug attach listeners; accept and ignore them (nothing emits).
    on() { return this; },
    once() { return this; },
    removeListener() { return this; },
    cork() {},
    uncork() {},
  });
  Object.defineProperty(process, "stdout", { value: makeStream(proc.writeStdout, 1), enumerable: true, configurable: true });
  Object.defineProperty(process, "stderr", { value: makeStream(proc.writeStderr, 2), enumerable: true, configurable: true });
  Object.defineProperty(process, "_readStdin", { value: readStdin, configurable: true });
  Object.defineProperty(process, "_nativeMetrics", { value: metrics, configurable: true });

  const raw = proc.hrtime;
  const hrtime = (prev) => {
    const t = raw();
    if (prev) {
      let s = t[0] - prev[0], n = t[1] - prev[1];
      if (n < 0) { s -= 1; n += 1e9; }
      return [s, n];
    }
    return t;
  };
  hrtime.bigint = () => { const t = raw(); return BigInt(t[0]) * 1000000000n + BigInt(t[1]); };
  process.hrtime = hrtime;

  // Seconds (fractional) since process start, from the same monotonic clock hrtime uses.
  process.uptime = () => { const t = raw(); return t[0] + t[1] / 1e9; };

  process.version = "v20.11.0";
  process.versions = { node: "20.11.0", lumen: "0.1.1", v8: "0.0.0" };

  // Real OS-identity / control surface over the native ops. These need the (about-to-be-deleted)
  // `__proc` namespace, so they are wired here rather than in the lumen-node JS glue.
  process.chdir = proc.chdir;
  process.abort = proc.abort;
  process.umask = proc.umask;
  process.ppid = proc.getppid();
  process.execve = (file, args, env) => {
    if (typeof file !== "string") throw new TypeError('The "file" argument must be of type string');
    if (!Array.isArray(args)) throw new TypeError('The "args" argument must be an Array');
    if (env === null || typeof env !== "object" || Array.isArray(env)) throw new TypeError('The "env" argument must be an object');
    const argv = args.map(value => String(value));
    const entries = Object.keys(env).filter(key => env[key] !== undefined).map(key => `${key}=${String(env[key])}`);
    if ([file, ...argv, ...entries].some(value => value.includes("\0"))) throw new TypeError("execve arguments may not contain null bytes");
    return rawExecve(file, argv.join("\0"), entries.join("\0"));
  };
  // getuid/getgid/geteuid/getegid are POSIX-only; the ops return undefined off unix (where Node
  // omits these entirely). We keep them defined but honest — `undefined` when the OS can't answer.
  const uid = proc.getuid(), gid = proc.getgid();
  if (uid !== undefined) {
    process.getuid = proc.getuid;
    process.geteuid = proc.geteuid;
    process.getgid = proc.getgid;
    process.getegid = proc.getegid;
    process.setuid = proc.setuid;
    process.seteuid = proc.seteuid;
    process.setgid = proc.setgid;
    process.setegid = proc.setegid;
    process.getgroups = proc.getgroups;
    process.setgroups = groups => {
      if (!Array.isArray(groups)) throw new TypeError('The "groups" argument must be an Array');
      return proc.setgroups(groups.map(group => Number(group)).join(","));
    };
    process.initgroups = (user, extraGroup) => proc.initgroups(String(user), Number(extraGroup));
  }
  // Portable signal numbers (identical on Linux/macOS); named signals outside this set fall back
  // to SIGTERM's number so `process.kill(pid)` still delivers a terminating signal.
  const SIGNALS = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGKILL: 9, SIGALRM: 14, SIGTERM: 15 };
  const rawKill = proc.kill;
  process.kill = (pid, sig = "SIGTERM") => {
    const n = typeof sig === "number" ? sig : (SIGNALS[sig] ?? 15);
    rawKill(pid | 0, n);
    return true;
  };
})();"#;

/// The data properties (`argv`, `env`, `platform`) — snapshots taken at startup, like Node's.
/// Runs after `install` because it needs the `process` object that install created.
pub(crate) fn install_data_props(engine: &mut Engine) {
    let global = engine.global_this();
    let ctx = engine.ctx();
    let process = match ctx.get_member(&global, "process") {
        Ok(v @ Value::Obj(_)) => v,
        _ => unreachable!("install() defined the process namespace"),
    };

    let argv0_str = std::env::args().next().unwrap_or_else(|| "lumen".to_string());
    let argv: Vec<Value> = std::env::args().map(Value::from_string).collect();
    let argv = ctx.make_array(argv);
    let _ = ctx.set_member(&process, "argv", argv);

    // argv0/execPath mirror argv[0] (the running binary), like Node. These are set here rather
    // than in the JS glue because the glue runs before this data-prop pass, so argv isn't ready
    // there yet. `title` defaults to the executable's basename (settable afterwards).
    let _ = ctx.set_member(&process, "argv0", Value::from_string(argv0_str.clone()));
    let _ = ctx.set_member(&process, "execPath", Value::from_string(argv0_str.clone()));
    let title = argv0_str
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("lumen")
        .to_string();
    let _ = ctx.set_member(&process, "title", Value::from_string(title));

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

    let _ = ctx.set_member(&process, "pid", Value::Num(std::process::id() as f64));
    // Node's architecture names, not Rust's (native addons resolve their platform binary by these).
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let _ = ctx.set_member(&process, "arch", Value::str(arch));
}

/// `(chunk)` — write raw bytes to stdout (no trailing newline, unlike `console.log`). A typed
/// array is written as-is; anything else is coerced to a string. Backs `process.stdout.write`.
fn op_write_stdout(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_raw(ctx, args, false)
}
fn op_write_stderr(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    write_raw(ctx, args, true)
}

/// `(resolve, reject)` — read one chunk from the process's stdin without blocking the loop.
fn op_read_stdin(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let (resolve, reject) = match (args.first(), args.get(1)) {
        (Some(resolve), Some(reject)) if resolve.is_callable() && reject.is_callable() => {
            (resolve.clone(), reject.clone())
        }
        _ => return Err(ctx.make_error("TypeError", "readStdin expects resolve and reject functions")),
    };
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs task registry")
        .register(resolve, Some(reject), decode_stdin_read);
    let sender = ctx
        .op_state()
        .get::<CompletionSender>()
        .expect("runtime installs completion sender")
        .clone();
    sender.run_blocking(id, || {
        let mut buf = vec![0u8; 65_536];
        let result = std::io::stdin()
            .read(&mut buf)
            .map(|n| {
                buf.truncate(n);
                buf
            })
            .map_err(|e| format!("stdin read: {e}"));
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_stdin_read(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<u8>, String>>()
        .expect("stdin read payload")
    {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]),
        Ok(bytes) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        Err(message) => Err(ctx.make_error("Error", message)),
    }
}

fn write_raw(ctx: &mut Ctx, args: &[Value], to_err: bool) -> Result<Value, Value> {
    let arg = args.first().unwrap_or(&Value::Undefined);
    let bytes = match ctx.typed_array_bytes(arg) {
        Some(b) => b,
        None => ctx.coerce_string(arg)?.as_bytes().to_vec(),
    };
    let sinks = ctx.host_mut::<ConsoleOut>().expect("console state installed");
    let sink = if to_err { &mut sinks.err } else { &mut sinks.out };
    // A broken pipe shouldn't crash the runtime (matches console's behavior).
    let _ = sink.write_all(&bytes);
    let _ = sink.flush();
    Ok(Value::Undefined)
}

/// `[seconds, nanoseconds]` elapsed since process start, from a monotonic clock (Node's
/// `process.hrtime()` contract). The js_init wrapper handles the optional `prev` diff + `.bigint`.
fn op_hrtime(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let start = ctx.host_mut::<ProcStart>().expect("process start installed").0;
    let e = start.elapsed();
    Ok(ctx.make_array(vec![
        Value::Num(e.as_secs() as f64),
        Value::Num(e.subsec_nanos() as f64),
    ]))
}

/// Real per-process CPU, resident-memory, and kernel resource counters from `getrusage(2)`.
/// The compact array keeps the native boundary cheap; lumen-node assigns Node's public names.
#[cfg(unix)]
fn op_metrics(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let mut usage = std::mem::MaybeUninit::<ffi::RUsage>::zeroed();
    if unsafe { ffi::getrusage(ffi::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(ctx.make_error(
            "Error",
            format!("getrusage failed: {}", std::io::Error::last_os_error()),
        ));
    }
    let usage = unsafe { usage.assume_init() };
    let micros = |time: ffi::TimeVal| time.sec * 1_000_000 + time.usec;
    #[cfg(target_os = "macos")]
    let (rss_bytes, max_rss_kib) = (usage.max_rss, usage.max_rss / 1024);
    #[cfg(not(target_os = "macos"))]
    let (rss_bytes, max_rss_kib) = (usage.max_rss * 1024, usage.max_rss);
    Ok(ctx.make_array(
        [
            rss_bytes,
            max_rss_kib,
            micros(usage.user),
            micros(usage.system),
            usage.minor_faults,
            usage.major_faults,
            usage.swaps,
            usage.block_inputs,
            usage.block_outputs,
            usage.messages_sent,
            usage.messages_received,
            usage.signals,
            usage.voluntary_switches,
            usage.involuntary_switches,
        ]
        .into_iter()
        .map(|value| Value::Num(value as f64))
        .collect(),
    ))
}

#[cfg(not(unix))]
fn op_metrics(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(ctx.make_array(vec![Value::Num(0.0); 14]))
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

/// `(dir)` — change the process working directory (`std::env::set_current_dir`). Cross-platform,
/// no FFI. Throws with the OS error on failure (matching `process.chdir`).
fn op_chdir(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let dir = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    match std::env::set_current_dir(&dir) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(ctx.make_error("Error", format!("ENOENT: chdir '{dir}': {e}"))),
    }
}

/// `process.abort()` — terminate immediately (SIGABRT, core dump where enabled). `std::process::abort`
/// is the real thing; never returns.
fn op_abort(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    std::process::abort();
}

/// `(pid, signal)` — deliver `signal` to `pid` via libc `kill(2)`. Real on unix; a no-op that
/// reports failure elsewhere (lumen ships unix-first). The JS wrapper maps signal names to numbers.
#[cfg(unix)]
fn op_kill(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let pid = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as i32;
    let sig = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))? as i32;
    let rc = unsafe { ffi::kill(pid, sig) };
    if rc == 0 {
        Ok(Value::Undefined)
    } else {
        Err(ctx.make_error("Error", format!("kill {pid} failed (errno set)")))
    }
}
#[cfg(not(unix))]
fn op_kill(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Err(ctx.make_error("Error", "process.kill is not supported on this platform"))
}

/// `([mask])` — read (no-arg, via the standard read-then-restore) or set the file-mode creation
/// mask through libc `umask(2)`. Returns the previous mask. Unix-only; returns 0 off unix.
#[cfg(unix)]
fn op_umask(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let prev = match args.first() {
        Some(v) if !matches!(v, Value::Undefined) => {
            let m = ctx.coerce_number(v)? as u32;
            unsafe { ffi::umask(m) }
        }
        // No argument: umask(2) has no pure read, so set-to-0-then-restore is the canonical idiom
        // (this is exactly why Node deprecated the read form).
        _ => {
            let cur = unsafe { ffi::umask(0) };
            unsafe { ffi::umask(cur) };
            cur
        }
    };
    Ok(Value::Num((prev & 0o7777) as f64))
}
#[cfg(not(unix))]
fn op_umask(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(0.0))
}

// Process-identity getters. POSIX-only; off unix they return `undefined` and the JS_INIT wiring
// leaves `process.getuid` &c. undefined, matching Node (which does not define them on Windows).
#[cfg(unix)]
fn op_getuid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(unsafe { ffi::getuid() } as f64))
}
#[cfg(unix)]
fn op_geteuid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(unsafe { ffi::geteuid() } as f64))
}
#[cfg(unix)]
fn op_getgid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(unsafe { ffi::getgid() } as f64))
}
#[cfg(unix)]
fn op_getegid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(unsafe { ffi::getegid() } as f64))
}
#[cfg(unix)]
fn op_getppid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Num(unsafe { ffi::getppid() } as f64))
}

#[cfg(unix)]
fn op_execve(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    use std::ffi::CString;
    use std::os::raw::c_char;

    let path = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let argv = ctx.coerce_string(args.get(1).unwrap_or(&Value::Undefined))?.to_string();
    let env = ctx.coerce_string(args.get(2).unwrap_or(&Value::Undefined))?.to_string();
    let path = CString::new(path).map_err(|_| ctx.make_error("TypeError", "execve path contains a null byte"))?;
    let argv = argv.split('\0').filter(|value| !value.is_empty()).map(CString::new).collect::<Result<Vec<_>, _>>()
        .map_err(|_| ctx.make_error("TypeError", "execve argument contains a null byte"))?;
    let env = env.split('\0').filter(|value| !value.is_empty()).map(CString::new).collect::<Result<Vec<_>, _>>()
        .map_err(|_| ctx.make_error("TypeError", "execve environment contains a null byte"))?;
    let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|value| value.as_ptr()).collect(); argv_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const c_char> = env.iter().map(|value| value.as_ptr()).collect(); env_ptrs.push(std::ptr::null());
    unsafe { ffi::execve(path.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()); }
    Err(ctx.make_error("Error", format!("execve failed: {}", std::io::Error::last_os_error())))
}

#[cfg(unix)]
fn identity_result(ctx: &mut Ctx, rc: i32, name: &str) -> Result<Value, Value> {
    if rc == 0 { Ok(Value::Undefined) } else { Err(ctx.make_error("Error", format!("{name} failed: {}", std::io::Error::last_os_error()))) }
}
#[cfg(unix)]
fn op_setuid(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> { let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as u32; identity_result(ctx, unsafe { ffi::setuid(id) }, "setuid") }
#[cfg(unix)]
fn op_seteuid(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> { let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as u32; identity_result(ctx, unsafe { ffi::seteuid(id) }, "seteuid") }
#[cfg(unix)]
fn op_setgid(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> { let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as u32; identity_result(ctx, unsafe { ffi::setgid(id) }, "setgid") }
#[cfg(unix)]
fn op_setegid(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> { let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as u32; identity_result(ctx, unsafe { ffi::setegid(id) }, "setegid") }
#[cfg(unix)]
fn op_getgroups(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> {
    let count = unsafe { ffi::getgroups(0, std::ptr::null_mut()) };
    if count < 0 { return Err(ctx.make_error("Error", format!("getgroups failed: {}", std::io::Error::last_os_error()))); }
    let mut groups = vec![0u32; count as usize];
    let count = unsafe { ffi::getgroups(count, groups.as_mut_ptr()) };
    if count < 0 { return Err(ctx.make_error("Error", format!("getgroups failed: {}", std::io::Error::last_os_error()))); }
    groups.truncate(count as usize); Ok(ctx.make_array(groups.into_iter().map(|id| Value::Num(id as f64)).collect()))
}
#[cfg(unix)]
fn op_setgroups(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let text = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let groups = if text.is_empty() { Vec::new() } else { text.split(',').map(str::parse::<u32>).collect::<Result<Vec<_>, _>>().map_err(|_| ctx.make_error("TypeError", "group ids must be numbers"))? };
    identity_result(ctx, unsafe { ffi::setgroups(groups.len(), groups.as_ptr()) }, "setgroups")
}
#[cfg(unix)]
fn op_initgroups(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    use std::ffi::CString;
    let user = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let user = CString::new(user).map_err(|_| ctx.make_error("TypeError", "user contains a null byte"))?;
    let group = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))? as u32;
    identity_result(ctx, unsafe { ffi::initgroups(user.as_ptr(), group) }, "initgroups")
}
#[cfg(not(unix))]
fn op_setuid(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Err(ctx.make_error("Error", "setuid is not supported on this platform")) }
#[cfg(not(unix))]
fn op_seteuid(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Err(ctx.make_error("Error", "seteuid is not supported on this platform")) }
#[cfg(not(unix))]
fn op_setgid(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Err(ctx.make_error("Error", "setgid is not supported on this platform")) }
#[cfg(not(unix))]
fn op_setegid(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Err(ctx.make_error("Error", "setegid is not supported on this platform")) }
#[cfg(not(unix))]
fn op_getgroups(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Ok(ctx.make_array(Vec::new())) }
#[cfg(not(unix))]
fn op_setgroups(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Err(ctx.make_error("Error", "setgroups is not supported on this platform")) }
#[cfg(not(unix))]
fn op_initgroups(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> { Err(ctx.make_error("Error", "initgroups is not supported on this platform")) }
#[cfg(not(unix))]
fn op_execve(ctx: &mut Ctx, _t: Value, _args: &[Value]) -> Result<Value, Value> {
    Err(ctx.make_error("Error", "process.execve is not supported on this platform"))
}
#[cfg(not(unix))]
fn op_getuid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Undefined)
}
#[cfg(not(unix))]
fn op_geteuid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Undefined)
}
#[cfg(not(unix))]
fn op_getgid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Undefined)
}
#[cfg(not(unix))]
fn op_getegid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Undefined)
}
#[cfg(not(unix))]
fn op_getppid(_ctx: &mut Ctx, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    // getppid is meaningless without the unix parent model; 0 is the honest "unknown".
    Ok(Value::Num(0.0))
}

/// Raw libc identity/control calls. Same category as `dylib.rs`'s dlopen FFI: these symbols live
/// in libc, which std already links into every Rust program, so no third-party dependency is added.
#[cfg(unix)]
mod ffi {
    use std::os::raw::{c_char, c_int, c_long, c_uint};

    pub const RUSAGE_SELF: c_int = 0;

    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct TimeVal {
        pub sec: c_long,
        pub usec: c_long,
    }

    #[repr(C)]
    pub struct RUsage {
        pub user: TimeVal,
        pub system: TimeVal,
        pub max_rss: c_long,
        pub shared_memory: c_long,
        pub unshared_data: c_long,
        pub unshared_stack: c_long,
        pub minor_faults: c_long,
        pub major_faults: c_long,
        pub swaps: c_long,
        pub block_inputs: c_long,
        pub block_outputs: c_long,
        pub messages_sent: c_long,
        pub messages_received: c_long,
        pub signals: c_long,
        pub voluntary_switches: c_long,
        pub involuntary_switches: c_long,
    }

    extern "C" {
        pub fn getrusage(who: c_int, usage: *mut RUsage) -> c_int;
        pub fn getuid() -> c_uint;
        pub fn geteuid() -> c_uint;
        pub fn getgid() -> c_uint;
        pub fn getegid() -> c_uint;
        pub fn getppid() -> c_int;
        pub fn kill(pid: c_int, sig: c_int) -> c_int;
        // mode_t is 16-bit on macOS / 32-bit on Linux; c_uint is ABI-safe for both (small values,
        // masked on the way out). Passing/returning through a register truncates harmlessly.
        pub fn umask(mask: c_uint) -> c_uint;
        pub fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int;
        pub fn setuid(uid: c_uint) -> c_int;
        pub fn seteuid(uid: c_uint) -> c_int;
        pub fn setgid(gid: c_uint) -> c_int;
        pub fn setegid(gid: c_uint) -> c_int;
        pub fn getgroups(size: c_int, groups: *mut c_uint) -> c_int;
        pub fn setgroups(size: usize, groups: *const c_uint) -> c_int;
        pub fn initgroups(user: *const c_char, group: c_uint) -> c_int;
    }
}
