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
//! - [x] native addons: `require('./x.node')` dlopens the library and runs its N-API
//!   registration (see `napi.rs` for the implemented `napi_*` surface and `dylib.rs` for the
//!   dependency-free loader) — [ ] the full ~150-function N-API, references, threadsafe funcs
//! - [ ] ESM `import` of `node:` specifiers (this is the CommonJS surface); `require('esm')`

use std::path::Path;

use lumen_host::{ops, Ctx, Extension, SpawnHandle, Value};

mod child;
mod dns;
mod dylib;
mod napi;

/// The runtime's blocking-work spawner (threadpool), for async ops.
fn spawn_handle(ctx: &mut Ctx) -> SpawnHandle {
    ctx.op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone()
}

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
                    "loadNativeAddon" (1) => napi::op_load_addon,
                    "stat" (1) => op_stat,
                    "lstat" (1) => op_lstat,
                    "rm" (3) => op_rm,
                    "mkdir" (2) => op_mkdir,
                    "rename" (2) => op_rename,
                    "copyFile" (2) => op_copy_file,
                    "readdirTypes" (1) => op_readdir_types,
                    "readlink" (1) => op_readlink,
                    "symlink" (2) => op_symlink,
                    "access" (1) => op_access,
                    "chmod" (2) => op_chmod,
                    "mkdtemp" (1) => op_mkdtemp,
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
            (
                "__dns",
                ops![
                    "lookup" (4) => dns::op_lookup,
                    "resolve" (4) => dns::op_resolve,
                ],
            ),
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

/// Build a Stats-shaped object from real filesystem metadata (the JS glue wraps it with the
/// `isFile()`/`isDirectory()`/… methods). `kind` lets the glue answer those without re-probing.
fn stat_object(ctx: &mut Ctx, meta: &std::fs::Metadata) -> Value {
    use std::os::unix::fs::MetadataExt;
    let ms = |t: std::io::Result<std::time::SystemTime>| -> f64 {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    };
    let ft = meta.file_type();
    let kind = if ft.is_file() {
        "file"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_symlink() {
        "symlink"
    } else {
        "other"
    };
    let o = Value::Obj(ctx.new_object());
    let set = |o: &Value, k: &str, v: f64, ctx: &mut Ctx| {
        let _ = ctx.set_member(o, k, Value::Num(v));
    };
    set(&o, "size", meta.len() as f64, ctx);
    set(&o, "mode", meta.mode() as f64, ctx);
    set(&o, "mtimeMs", ms(meta.modified()), ctx);
    set(&o, "atimeMs", ms(meta.accessed()), ctx);
    // Unix has no true creation time in `Metadata::created` everywhere; `ctime` is change time.
    set(&o, "ctimeMs", meta.ctime() as f64 * 1000.0 + meta.ctime_nsec() as f64 / 1e6, ctx);
    set(&o, "birthtimeMs", ms(meta.created()), ctx);
    set(&o, "ino", meta.ino() as f64, ctx);
    set(&o, "dev", meta.dev() as f64, ctx);
    set(&o, "nlink", meta.nlink() as f64, ctx);
    set(&o, "uid", meta.uid() as f64, ctx);
    set(&o, "gid", meta.gid() as f64, ctx);
    set(&o, "rdev", meta.rdev() as f64, ctx);
    set(&o, "blksize", meta.blksize() as f64, ctx);
    set(&o, "blocks", meta.blocks() as f64, ctx);
    let _ = ctx.set_member(&o, "kind", Value::str(kind));
    o
}

/// An errno-tagged filesystem error (the `code` Node users switch on), from a `std::io::Error`.
fn fs_error(ctx: &mut Ctx, op: &str, path: &str, e: &std::io::Error) -> Value {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        _ => match e.raw_os_error() {
            Some(20) => "ENOTDIR",
            Some(21) => "EISDIR",
            Some(39) | Some(66) => "ENOTEMPTY",
            _ => "EIO",
        },
    };
    let err = ctx.make_error("Error", format!("{code}: {e}, {op} '{path}'"));
    let _ = ctx.set_member(&err, "code", Value::str(code));
    let _ = ctx.set_member(&err, "path", Value::str(path));
    let _ = ctx.set_member(&err, "syscall", Value::str(op));
    err
}

/// `stat(path)` — follow symlinks.
fn op_stat(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::metadata(&p) {
        Ok(m) => Ok(stat_object(ctx, &m)),
        Err(e) => Err(fs_error(ctx, "stat", &p, &e)),
    }
}

/// `lstat(path)` — do not follow symlinks.
fn op_lstat(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::symlink_metadata(&p) {
        Ok(m) => Ok(stat_object(ctx, &m)),
        Err(e) => Err(fs_error(ctx, "lstat", &p, &e)),
    }
}

/// `rm(path, recursive, force)` — remove a file or directory tree.
fn op_rm(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let recursive = matches!(args.get(1), Some(Value::Bool(true)));
    let force = matches!(args.get(2), Some(Value::Bool(true)));
    let path = std::path::Path::new(&p);
    let result = if path.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(Value::Undefined),
        // `force` swallows a missing path, as Node's rmSync does.
        Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "rm", &p, &e)),
    }
}

/// `mkdir(path, recursive)` — returns the first created directory (Node's recursive contract).
fn op_mkdir(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let recursive = matches!(args.get(1), Some(Value::Bool(true)));
    let result = if recursive {
        std::fs::create_dir_all(&p)
    } else {
        std::fs::create_dir(&p)
    };
    match result {
        Ok(()) => Ok(Value::from_string(p)),
        Err(e) if recursive && e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "mkdir", &p, &e)),
    }
}

/// `rename(from, to)`.
fn op_rename(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let from = arg_path(ctx, args)?;
    let to = ctx.coerce_string(args.get(1).unwrap_or(&Value::Undefined))?.to_string();
    match std::fs::rename(&from, &to) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "rename", &from, &e)),
    }
}

/// `copyFile(from, to)`.
fn op_copy_file(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let from = arg_path(ctx, args)?;
    let to = ctx.coerce_string(args.get(1).unwrap_or(&Value::Undefined))?.to_string();
    match std::fs::copy(&from, &to) {
        Ok(_) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "copyfile", &from, &e)),
    }
}

/// `readdirTypes(path)` — directory entries as `[name, kind]` pairs for `withFileTypes`.
fn op_readdir_types(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    let entries = std::fs::read_dir(&p).map_err(|e| fs_error(ctx, "scandir", &p, &e))?;
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match entry.file_type() {
            Ok(ft) if ft.is_dir() => "dir",
            Ok(ft) if ft.is_symlink() => "symlink",
            Ok(ft) if ft.is_file() => "file",
            _ => "other",
        };
        let pair = ctx.make_array(vec![Value::from_string(name), Value::str(kind)]);
        items.push(pair);
    }
    Ok(ctx.make_array(items))
}

/// `readlink(path)`.
fn op_readlink(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::read_link(&p) {
        Ok(target) => Ok(Value::from_string(target.to_string_lossy().into_owned())),
        Err(e) => Err(fs_error(ctx, "readlink", &p, &e)),
    }
}

/// `symlink(target, path)`.
fn op_symlink(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = arg_path(ctx, args)?;
    let link = ctx.coerce_string(args.get(1).unwrap_or(&Value::Undefined))?.to_string();
    match std::os::unix::fs::symlink(&target, &link) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "symlink", &link, &e)),
    }
}

/// `access(path)` — existence/readability check; rejects with ENOENT if absent.
fn op_access(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg_path(ctx, args)?;
    match std::fs::metadata(&p) {
        Ok(_) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "access", &p, &e)),
    }
}

/// `chmod(path, mode)`.
fn op_chmod(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use std::os::unix::fs::PermissionsExt;
    let p = arg_path(ctx, args)?;
    let mode = args.get(1).and_then(|v| v.as_num_opt()).unwrap_or(0.0) as u32;
    match std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => Err(fs_error(ctx, "chmod", &p, &e)),
    }
}

/// `mkdtemp(prefix)` — create a uniquely-named temp directory and return its path. Uniqueness
/// comes from the OS-assigned pid plus a monotonically bumped counter (no RNG dependency).
fn op_mkdtemp(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let prefix = arg_path(ctx, args)?;
    let pid = std::process::id();
    for _ in 0..1000 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{prefix}{:06x}{:04x}", pid, n & 0xffff);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(Value::from_string(candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fs_error(ctx, "mkdtemp", &candidate, &e)),
        }
    }
    Err(ctx.make_error("Error", format!("mkdtemp '{prefix}': exhausted candidates")))
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
