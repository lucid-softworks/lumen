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

mod child;
// The N-API `.node` loader consumes `DynLib::path` (diagnostics); allow until that stage lands.
#[allow(dead_code)]
mod dylib;

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
                    "readBytes" (1) => op_read_bytes,
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
            (
                "__zlib",
                ops![
                    "deflate" (1) => op_zlib_deflate,
                    "inflate" (1) => op_zlib_inflate,
                    "deflateRaw" (1) => op_zlib_deflate_raw,
                    "inflateRaw" (1) => op_zlib_inflate_raw,
                    "gzip" (1) => op_zlib_gzip,
                    "gunzip" (1) => op_zlib_gunzip,
                ],
            ),
            ("__child", child::CHILD_OPS),
        ],
        state_init: Some(|state: &mut lumen_host::OpState| state.put(child::ChildRegistry::default())),
        js_init: Some(JS_GLUE),
        js_init_snapshot: Some(JS_GLUE_SNAPSHOT),
    }
}

// Assembled by build.rs from src/js/*.js (single source of truth). JS_GLUE is the fallback
// source; JS_GLUE_SNAPSHOT is its precompiled AST, decoded at boot to skip re-parsing.
const JS_GLUE: &str = include_str!(concat!(env!("OUT_DIR"), "/node_glue.js"));
const JS_GLUE_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node_glue.snap"));

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

/// Read a file as raw bytes (a Uint8Array), for `fs.readFileSync` without an encoding — the text
/// path corrupts binary. Errors carry the errno `code` Node users switch on.
fn op_read_bytes(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::read(&p) {
        Ok(bytes) => ctx.make_uint8array(&bytes),
        Err(e) => {
            let err = ctx.make_error("Error", format!("cannot read '{p}': {e}"));
            let code = match e.kind() {
                std::io::ErrorKind::NotFound => "ENOENT",
                std::io::ErrorKind::PermissionDenied => "EACCES",
                _ => "EIO",
            };
            let _ = ctx.set_member(&err, "code", Value::str(code));
            Err(err)
        }
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

// ---- node:zlib, over the shared DEFLATE codec ----

fn zlib_compress_op(
    ctx: &mut Ctx,
    args: &[Value],
    codec: fn(&[u8]) -> Vec<u8>,
) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "zlib expects a Buffer/TypedArray"));
    };
    ctx.make_uint8array(&codec(&bytes))
}

fn zlib_decompress_op(
    ctx: &mut Ctx,
    args: &[Value],
    codec: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "zlib expects a Buffer/TypedArray"));
    };
    match codec(&bytes) {
        Ok(out) => ctx.make_uint8array(&out),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

fn op_zlib_deflate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::deflate::zlib_compress)
}
fn op_zlib_inflate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::deflate::zlib_decompress)
}
fn op_zlib_deflate_raw(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::deflate::deflate)
}
fn op_zlib_inflate_raw(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::deflate::inflate)
}
fn op_zlib_gzip(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_compress_op(ctx, a, lumen_host::deflate::gzip_compress)
}
fn op_zlib_gunzip(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    zlib_decompress_op(ctx, a, lumen_host::deflate::gzip_decompress)
}
