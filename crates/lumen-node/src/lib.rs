//! lumen-node — `node:` builtin compatibility.
//!
//! The interesting parts are JS (see `src/js/`): the CommonJS `require` implementation with
//! `node_modules` resolution and `package.json` `main`/`exports`, `Buffer`, `node:path`, and
//! `node:fs`/`node:os` shims. Rust backs only what JS can't reach: filesystem *classification*
//! for the resolver (is-file / is-dir, distinct from the fs ops' read/write) and OS facts.
//!
//! Scope, stated honestly (checklist — [ ] = not yet):
//! - [x] CommonJS `require`: core modules, relative/absolute, `node_modules` walk, the module
//!   wrapper via `new Function`, `require.cache`/`require.resolve`/`require.main`, `.js`/
//!   `.json`/`.cjs`
//! - [x] `package.json` `main`; a practical `exports` subset (string, `"."` key,
//!   `require`/`node`/`default` conditions) — [ ] subpath patterns, full conditional exports
//! - [x] `node:path` (posix + win32), `node:os`, `node:fs` (sync + callback + `.promises`)
//! - [x] `Buffer` (from/alloc/concat, utf8·hex·base64·latin1·ascii, slice/write/compare, the
//!   common read/write-int accessors) — [ ] every codec + accessor variant
//! - [x] `global`, `__dirname`/`__filename` (per module)
//! - [ ] ESM `import` of `node:` specifiers (this is the CommonJS surface); `require('esm')`;
//!   native addons

use std::path::Path;

use lumen_host::{ops, Ctx, Extension, Value};

pub fn extension() -> Extension {
    Extension {
        name: "node",
        globals: &[],
        namespaces: &[
            (
                "__node",
                ops![
                    "isFile" (1) => op_is_file,
                    "isDir" (1) => op_is_dir,
                    "readText" (1) => op_read_text,
                    "realpath" (1) => op_realpath,
                ],
            ),
            (
                "__os",
                ops![
                    "info" (0) => op_os_info,
                    "hostname" (0) => op_hostname,
                ],
            ),
        ],
        state_init: None,
        js_init: Some(JS_GLUE),
    }
}

const JS_GLUE: &str = concat!(
    "(() => {\n",
    include_str!("js/preamble.js"),
    include_str!("js/buffer.js"),
    include_str!("js/path.js"),
    include_str!("js/os.js"),
    include_str!("js/fs.js"),
    include_str!("js/module.js"),
    "\n})();"
);

fn arg_path(ctx: &mut Ctx, args: &[Value]) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string())
}

fn op_is_file(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    Ok(Value::Bool(Path::new(&p).is_file()))
}

fn op_is_dir(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    Ok(Value::Bool(Path::new(&p).is_dir()))
}

/// Read a module/JSON source as text; a miss is an error the resolver turns into
/// MODULE_NOT_FOUND context.
fn op_read_text(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(Value::from_string(s)),
        Err(e) => Err(ctx.make_error("Error", format!("cannot read '{p}': {e}"))),
    }
}

/// Canonicalize (resolve symlinks) for the module cache key; falls back to the input when the
/// path doesn't exist yet (matching how the JS resolver probes candidates).
fn op_realpath(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::canonicalize(&p) {
        Ok(c) => Ok(Value::from_string(c.to_string_lossy().into_owned())),
        Err(_) => Ok(Value::from_string(p)),
    }
}

/// One object of OS facts the JS `os` shim reads (snapshotted like Node's are). `hostname`
/// is separate because it can do I/O.
fn op_os_info(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let os_type = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        "windows" => "Windows_NT",
        other => other,
    };
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let tmpdir = std::env::temp_dir().to_string_lossy().into_owned();
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let release = os_release();

    let obj = Value::Obj(ctx.new_object());
    for (k, v) in [
        ("platform", platform.to_string()),
        ("arch", arch.to_string()),
        ("type", os_type.to_string()),
        ("homedir", home),
        ("tmpdir", tmpdir),
        ("release", release),
        (
            "endianness",
            if cfg!(target_endian = "big") {
                "BE"
            } else {
                "LE"
            }
            .to_string(),
        ),
    ] {
        let _ = ctx.set_member(&obj, k, Value::from_string(v));
    }
    let _ = ctx.set_member(&obj, "cpus", Value::Num(cpus as f64));
    Ok(obj)
}

fn os_release() -> String {
    // std exposes no uname(); read the one file Linux publishes it in, else leave it blank
    // rather than invent a version.
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Best-effort hostname without a syscall or crate: env, then the file Linux writes it to,
/// then a safe default.
fn op_hostname(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let name = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string());
    Ok(Value::from_string(name))
}
