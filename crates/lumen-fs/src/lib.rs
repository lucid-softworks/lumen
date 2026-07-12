//! lumen-fs — filesystem ops.
//!
//! JS surface (a `fs` global namespace for now; the `node:fs` module spelling belongs to the
//! future lumen-node compat crate):
//! - Sync path ops: `readFileSync`, `writeFileSync`, `appendFileSync`, `existsSync`,
//!   `unlinkSync`, `mkdirSync` (recursive), `readdirSync`.
//! - Handle ops, backed by the `ResourceTable`: `openSync(path, "r"|"w"|"a"|"r+"|"w+"|"a+")
//!   -> fd`, `readSync(fd)`, `writeSync(fd, data)`, `closeSync(fd)`. Ids are never reused, so a
//!   stale fd is a clean error. Positional/metadata ops back node:fs's fd surface and
//!   FileHandle: `preadSync`/`pwriteSync` (seek + read/write), `fstatSync`, `ftruncateSync`,
//!   `fsyncSync`/`fdatasyncSync`, `fchmodSync`, `futimesSync`.
//! - `fs.promises.readFile/writeFile`: JS glue (`js_init`) wrapping raw callback ops that
//!   spawn the same `std::fs` calls on the threadpool and settle via the `TaskRegistry`.
//!
//! Text-only for now: file contents cross as UTF-8 strings (invalid sequences decode
//! lossily). Binary contents want a `Buffer`/`Uint8Array` bridge in the embed API — deferred,
//! noted. Errors are plain `Error`s carrying the op + path + OS message (no `code`/`errno`
//! fields yet).

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use lumen_host::{ops, Ctx, Extension, SpawnHandle, TaskRegistry, Value};

pub fn extension() -> Extension {
    Extension {
        name: "fs",
        globals: &[],
        namespaces: &[
            (
                "fs",
                ops![
                    "readFileSync" (1) => op_read_file_sync,
                    "writeFileSync" (2) => op_write_file_sync,
                    "appendFileSync" (2) => op_append_file_sync,
                    "existsSync" (1) => op_exists_sync,
                    "unlinkSync" (1) => op_unlink_sync,
                    "mkdirSync" (1) => op_mkdir_sync,
                    "readdirSync" (1) => op_readdir_sync,
                    "openSync" (2) => op_open_sync,
                    "readSync" (1) => op_read_fd_sync,
                    "writeSync" (2) => op_write_fd_sync,
                    "closeSync" (1) => op_close_sync,
                    // Positional / fd-based ops backing node:fs's fd surface and FileHandle.
                    "preadSync" (3) => op_pread_sync,
                    "pwriteSync" (3) => op_pwrite_sync,
                    "fstatSync" (1) => op_fstat_sync,
                    "ftruncateSync" (2) => op_ftruncate_sync,
                    "fsyncSync" (1) => op_fsync_sync,
                    "fdatasyncSync" (1) => op_fdatasync_sync,
                    "fchmodSync" (2) => op_fchmod_sync,
                    "fchownSync" (3) => op_fchown_sync,
                    "futimesSync" (3) => op_futimes_sync,
                ],
            ),
            (
                "__fs_async",
                ops![
                    "read" (3) => op_read_async,
                    "write" (4) => op_write_async,
                ],
            ),
            (
                "__fs_fdmeta",
                ops![
                    "openSync" (2) => op_open_sync,
                    "closeSync" (1) => op_close_sync,
                    "fchmodSync" (2) => op_fchmod_sync,
                    "fchownSync" (3) => op_fchown_sync,
                ],
            ),
        ],
        state_init: None,
        js_init: Some(JS_PROMISES),
        js_init_snapshot: None,
    }
}

/// `fs.promises` over the raw callback ops. The raw namespace is captured and removed from
/// the global scope — promise construction is exactly the glue JS is better at than Rust.
const JS_PROMISES: &str = r#"
{
    const raw = globalThis.__fs_async;
    delete globalThis.__fs_async;
    fs.promises = {
        readFile: (path) =>
            new Promise((resolve, reject) => raw.read(String(path), resolve, reject)),
        writeFile: (path, data) =>
            new Promise((resolve, reject) => raw.write(String(path), String(data), resolve, reject)),
    };
}
"#;

fn arg_string(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.get(i).unwrap_or(&Value::Undefined))?
        .to_string())
}

fn io_error(ctx: &mut Ctx, op: &str, path: &str, e: std::io::Error) -> Value {
    ctx.make_error("Error", format!("{op} '{path}': {e}"))
}

// ---- sync path ops ----

fn op_read_file_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Value::from_string(
            String::from_utf8_lossy(&bytes).into_owned(),
        )),
        Err(e) => Err(io_error(ctx, "readFileSync", &path, e)),
    }
}

fn op_write_file_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    let data = arg_string(ctx, args, 1)?;
    std::fs::write(&path, data).map_err(|e| io_error(ctx, "writeFileSync", &path, e))?;
    Ok(Value::Undefined)
}

fn op_append_file_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    let data = arg_string(ctx, args, 1)?;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut f| f.write_all(data.as_bytes()))
        .map_err(|e| io_error(ctx, "appendFileSync", &path, e))?;
    Ok(Value::Undefined)
}

fn op_exists_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    Ok(Value::Bool(std::path::Path::new(&path).exists()))
}

fn op_unlink_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    std::fs::remove_file(&path).map_err(|e| io_error(ctx, "unlinkSync", &path, e))?;
    Ok(Value::Undefined)
}

/// Recursive by default (`create_dir_all`) — Node needs `{ recursive: true }`, but the
/// non-recursive failure mode is a trap nobody wants; revisit with an options argument.
fn op_mkdir_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    std::fs::create_dir_all(&path).map_err(|e| io_error(ctx, "mkdirSync", &path, e))?;
    Ok(Value::Undefined)
}

fn op_readdir_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    let entries = std::fs::read_dir(&path).map_err(|e| io_error(ctx, "readdirSync", &path, e))?;
    let mut names: Vec<String> = entries
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    Ok(ctx.make_array(names.into_iter().map(Value::from_string).collect()))
}

// ---- handle ops (ResourceTable) ----

/// The `ResourceTable` entry for an open file. `RefCell` because reads/writes need `&mut
/// File` while the table hands out shared `Rc`s.
type FsHandle = RefCell<File>;

fn op_open_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    let mode = match args.get(1) {
        None | Some(Value::Undefined) => "r".to_string(),
        Some(v) => ctx.coerce_string(v)?.to_string(),
    };
    let mut opts = std::fs::OpenOptions::new();
    let file = match mode.as_str() {
        "r" => opts.read(true).open(&path),
        "w" => opts.write(true).create(true).truncate(true).open(&path),
        "a" => opts.append(true).create(true).open(&path),
        // Read+write variants back FileHandle and the fd read/write ops.
        "r+" => opts.read(true).write(true).open(&path),
        "w+" => opts.read(true).write(true).create(true).truncate(true).open(&path),
        "a+" => opts.read(true).append(true).create(true).open(&path),
        other => return Err(ctx.make_error("TypeError", format!("openSync: bad mode '{other}'"))),
    }
    .map_err(|e| io_error(ctx, "openSync", &path, e))?;
    let rid = ctx.resource_table().add::<FsHandle>(RefCell::new(file));
    Ok(Value::Num(rid as f64))
}

fn fd_handle(ctx: &mut Ctx, args: &[Value]) -> Result<std::rc::Rc<FsHandle>, Value> {
    let fd = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))?;
    let handle = (fd.is_finite() && fd >= 0.0)
        .then(|| ctx.resource_table().get::<FsHandle>(fd as u32))
        .flatten();
    handle.ok_or_else(|| ctx.make_error("TypeError", format!("bad file descriptor {fd}")))
}

/// Read everything from the handle's current position (sequential reads consume the file).
fn op_read_fd_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let handle = fd_handle(ctx, args)?;
    let mut bytes = Vec::new();
    handle
        .borrow_mut()
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(ctx, "readSync", "fd", e))?;
    Ok(Value::from_string(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

fn op_write_fd_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let data = arg_string(ctx, args, 1)?;
    let handle = fd_handle(ctx, args)?;
    handle
        .borrow_mut()
        .write_all(data.as_bytes())
        .map_err(|e| io_error(ctx, "writeSync", "fd", e))?;
    Ok(Value::Undefined)
}

fn op_close_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let fd = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))?;
    if fd.is_finite() && fd >= 0.0 {
        // Drop closes the file once the last Rc clone (a read/write mid-flight) is gone.
        ctx.resource_table().close(fd as u32);
    }
    Ok(Value::Undefined)
}

// ---- positional / fd-based ops (back node:fs fd surface + FileHandle) ----

/// `preadSync(fd, length, position)` — read up to `length` bytes; seek to `position` first
/// when it is a non-negative number, otherwise read from the handle's current offset. Returns
/// a `Uint8Array` (may be shorter than `length` at EOF).
fn op_pread_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let length = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))?;
    let position = args.get(2).and_then(|v| v.as_num_opt());
    let handle = fd_handle(ctx, args)?;
    let mut file = handle.borrow_mut();
    if let Some(pos) = position.filter(|p| p.is_finite() && *p >= 0.0) {
        file.seek(SeekFrom::Start(pos as u64))
            .map_err(|e| io_error(ctx, "read", "fd", e))?;
    }
    let cap = if length.is_finite() && length > 0.0 { length as usize } else { 0 };
    let mut buf = vec![0u8; cap];
    let n = file
        .read(&mut buf)
        .map_err(|e| io_error(ctx, "read", "fd", e))?;
    buf.truncate(n);
    ctx.make_uint8array(&buf)
}

/// `pwriteSync(fd, data, position)` — `data` is a string or a `Uint8Array`; seek to `position`
/// first when it is a non-negative number. Returns the number of bytes written.
fn op_pwrite_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let bytes = match args.get(1) {
        Some(v) => match ctx.typed_array_bytes(v) {
            Some(b) => b,
            None => ctx.coerce_string(v)?.as_bytes().to_vec(),
        },
        None => Vec::new(),
    };
    let position = args.get(2).and_then(|v| v.as_num_opt());
    let handle = fd_handle(ctx, args)?;
    let mut file = handle.borrow_mut();
    if let Some(pos) = position.filter(|p| p.is_finite() && *p >= 0.0) {
        file.seek(SeekFrom::Start(pos as u64))
            .map_err(|e| io_error(ctx, "write", "fd", e))?;
    }
    file.write_all(&bytes)
        .map_err(|e| io_error(ctx, "write", "fd", e))?;
    Ok(Value::Num(bytes.len() as f64))
}

/// A Node `Stats`-shaped raw object from a handle's metadata (JS `makeStats` adds the methods).
fn stat_value(ctx: &mut Ctx, meta: &std::fs::Metadata) -> Value {
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
    let set = |k: &str, v: f64, ctx: &mut Ctx| {
        let _ = ctx.set_member(&o, k, Value::Num(v));
    };
    set("size", meta.len() as f64, ctx);
    set("mtimeMs", ms(meta.modified()), ctx);
    set("atimeMs", ms(meta.accessed()), ctx);
    set("birthtimeMs", ms(meta.created()), ctx);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        set("mode", meta.mode() as f64, ctx);
        set("ctimeMs", meta.ctime() as f64 * 1000.0 + meta.ctime_nsec() as f64 / 1e6, ctx);
        set("ino", meta.ino() as f64, ctx);
        set("dev", meta.dev() as f64, ctx);
        set("nlink", meta.nlink() as f64, ctx);
        set("uid", meta.uid() as f64, ctx);
        set("gid", meta.gid() as f64, ctx);
        set("rdev", meta.rdev() as f64, ctx);
        set("blksize", meta.blksize() as f64, ctx);
        set("blocks", meta.blocks() as f64, ctx);
    }
    #[cfg(not(unix))]
    {
        let type_bits = if ft.is_dir() { 0o040000 } else { 0o100000 };
        let readonly = meta.permissions().readonly();
        set("mode", (type_bits | if readonly { 0o444 } else { 0o666 }) as f64, ctx);
        set("ctimeMs", ms(meta.modified()), ctx);
        set("ino", 0.0, ctx);
        set("dev", 0.0, ctx);
        set("nlink", 1.0, ctx);
        set("uid", 0.0, ctx);
        set("gid", 0.0, ctx);
        set("rdev", 0.0, ctx);
        set("blksize", 4096.0, ctx);
        set("blocks", ((meta.len() + 511) / 512) as f64, ctx);
    }
    let _ = ctx.set_member(&o, "kind", Value::str(kind));
    o
}

/// `fstatSync(fd)`.
fn op_fstat_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let handle = fd_handle(ctx, args)?;
    let meta = handle
        .borrow()
        .metadata()
        .map_err(|e| io_error(ctx, "fstat", "fd", e))?;
    Ok(stat_value(ctx, &meta))
}

/// `ftruncateSync(fd, len)`.
fn op_ftruncate_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let len = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))?;
    let handle = fd_handle(ctx, args)?;
    handle
        .borrow()
        .set_len(if len.is_finite() && len >= 0.0 { len as u64 } else { 0 })
        .map_err(|e| io_error(ctx, "ftruncate", "fd", e))?;
    Ok(Value::Undefined)
}

/// `fsyncSync(fd)`.
fn op_fsync_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let handle = fd_handle(ctx, args)?;
    handle
        .borrow()
        .sync_all()
        .map_err(|e| io_error(ctx, "fsync", "fd", e))?;
    Ok(Value::Undefined)
}

/// `fdatasyncSync(fd)`.
fn op_fdatasync_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let handle = fd_handle(ctx, args)?;
    handle
        .borrow()
        .sync_data()
        .map_err(|e| io_error(ctx, "fdatasync", "fd", e))?;
    Ok(Value::Undefined)
}

/// `fchmodSync(fd, mode)`.
fn op_fchmod_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mode = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))? as u32;
    let handle = fd_handle(ctx, args)?;
    #[cfg(unix)]
    let perms = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    };
    #[cfg(not(unix))]
    let perms = {
        let mut p = handle
            .borrow()
            .metadata()
            .map_err(|e| io_error(ctx, "fchmod", "fd", e))?
            .permissions();
        p.set_readonly(mode & 0o200 == 0);
        p
    };
    handle
        .borrow()
        .set_permissions(perms)
        .map_err(|e| io_error(ctx, "fchmod", "fd", e))?;
    Ok(Value::Undefined)
}

fn op_fchown_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let uid = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))? as u32;
    let gid = ctx.coerce_number(args.get(2).unwrap_or(&Value::Undefined))? as u32;
    let handle = fd_handle(ctx, args)?;
    #[cfg(unix)]
    {
        let result = std::os::unix::fs::fchown(&*handle.borrow(), Some(uid), Some(gid))
            .map(|()| Value::Undefined)
            .map_err(|error| io_error(ctx, "fchown", "fd", error));
        result
    }
    #[cfg(not(unix))]
    { let _ = (uid, gid, handle); Err(ctx.make_error("Error", "ENOSYS: fchown is not supported on this platform")) }
}

/// `futimesSync(fd, atimeSec, mtimeSec)` — set access/modification times on an open handle.
/// Backed by the BSD `futimes(2)` (present on macOS and glibc Linux); a clean error elsewhere.
fn op_futimes_sync(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let atime = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))?;
    let mtime = ctx.coerce_number(args.get(2).unwrap_or(&Value::Undefined))?;
    let handle = fd_handle(ctx, args)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::io::AsRawFd;
        let fd = handle.borrow().as_raw_fd();
        let times = [timeval_from_secs(atime), timeval_from_secs(mtime)];
        // SAFETY: `fd` is a live descriptor from the handle; `times` is a valid 2-element array.
        let rc = unsafe { futimes(fd, times.as_ptr()) };
        if rc != 0 {
            return Err(io_error(ctx, "futime", "fd", std::io::Error::last_os_error()));
        }
        Ok(Value::Undefined)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (atime, mtime, handle);
        let err = ctx.make_error("Error", "ENOSYS: futimes is not supported on this platform");
        let _ = ctx.set_member(&err, "code", Value::str("ENOSYS"));
        Err(err)
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i32,
}
#[cfg(target_os = "linux")]
#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn timeval_from_secs(secs: f64) -> Timeval {
    let s = secs.floor();
    let usec = ((secs - s) * 1_000_000.0).round();
    Timeval {
        tv_sec: s as i64,
        tv_usec: usec as _,
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
extern "C" {
    fn futimes(fd: i32, times: *const Timeval) -> i32;
}

// ---- async ops (threadpool + TaskRegistry; called only by the js_init glue) ----

/// `(path, resolve, reject)`: read off-thread, settle the promise on the loop.
fn op_read_async(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    let (resolve, reject) = settle_args(ctx, args, "__fs_async.read")?;
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_read);
    let spawn = spawn_handle(ctx);
    spawn.spawn_blocking(id, move || {
        Box::new(
            std::fs::read(&path)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|e| format!("readFile '{path}': {e}")),
        )
    });
    Ok(Value::Undefined)
}

/// `(path, data, resolve, reject)`.
fn op_write_async(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = arg_string(ctx, args, 0)?;
    let data = arg_string(ctx, args, 1)?;
    let (resolve, reject) = settle_args(ctx, &args[1..], "__fs_async.write")?;
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_write);
    let spawn = spawn_handle(ctx);
    spawn.spawn_blocking(id, move || {
        Box::new(std::fs::write(&path, data).map_err(|e| format!("writeFile '{path}': {e}")))
    });
    Ok(Value::Undefined)
}

/// The trailing `(resolve, reject)` pair the glue passes; anything else is a glue bug.
fn settle_args(ctx: &mut Ctx, args: &[Value], who: &str) -> Result<(Value, Value), Value> {
    match (args.get(1), args.get(2)) {
        (Some(res), Some(rej)) if res.is_callable() && rej.is_callable() => {
            Ok((res.clone(), rej.clone()))
        }
        _ => Err(ctx.make_error("TypeError", format!("{who} expects (resolve, reject)"))),
    }
}

fn spawn_handle(ctx: &mut Ctx) -> SpawnHandle {
    ctx.op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone()
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<String, String>>()
        .expect("read payload")
    {
        Ok(text) => Ok(vec![Value::from_string(text)]),
        Err(message) => Err(ctx.make_error("Error", message)),
    }
}

fn decode_write(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<(), String>>()
        .expect("write payload")
    {
        Ok(()) => Ok(Vec::new()),
        Err(message) => Err(ctx.make_error("Error", message)),
    }
}
