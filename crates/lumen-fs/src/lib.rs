//! lumen-fs — filesystem ops.
//!
//! JS surface (a `fs` global namespace for now; the `node:fs` module spelling belongs to the
//! future lumen-node compat crate):
//! - Sync path ops: `readFileSync`, `writeFileSync`, `appendFileSync`, `existsSync`,
//!   `unlinkSync`, `mkdirSync` (recursive), `readdirSync`.
//! - Handle ops, backed by the `ResourceTable`: `openSync(path, "r"|"w"|"a") -> fd`,
//!   `readSync(fd)`, `writeSync(fd, data)`, `closeSync(fd)`. Ids are never reused, so a
//!   stale fd is a clean error.
//! - `fs.promises.readFile/writeFile`: JS glue (`js_init`) wrapping raw callback ops that
//!   spawn the same `std::fs` calls on the threadpool and settle via the `TaskRegistry`.
//!
//! Text-only for now: file contents cross as UTF-8 strings (invalid sequences decode
//! lossily). Binary contents want a `Buffer`/`Uint8Array` bridge in the embed API — deferred,
//! noted. Errors are plain `Error`s carrying the op + path + OS message (no `code`/`errno`
//! fields yet).

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Write};

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
                ],
            ),
            (
                "__fs_async",
                ops![
                    "read" (3) => op_read_async,
                    "write" (4) => op_write_async,
                ],
            ),
        ],
        state_init: None,
        js_init: Some(JS_PROMISES),
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
    let file = match mode.as_str() {
        "r" => File::open(&path),
        "w" => File::create(&path),
        "a" => std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path),
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
