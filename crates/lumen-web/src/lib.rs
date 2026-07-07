//! lumen-web — the WinterTC "Minimum Common Web Platform API", incrementally.
//!
//! Pure-JS pieces ship as `js_init` glue (see `src/js/`); Rust backs parsing, crypto, and the
//! network. Conformance checklist against the WinterTC minimum common API:
//!
//! - [x] `console`, timers, `queueMicrotask` (lumen-runtime/lumen-timers)
//! - [x] `DOMException`, `Event`, `CustomEvent`, `EventTarget`, `AbortController`,
//!   `AbortSignal` (incl. `abort()`/`timeout()` statics) — flat target, no capture phase
//! - [x] `TextEncoder` / `TextDecoder` (utf-8 only; `fatal` supported)
//! - [x] `atob` / `btoa`
//! - [x] `structuredClone` (objects/arrays/cycles, Date, RegExp, Map, Set, Error,
//!   ArrayBuffer, typed arrays; no transfer list)
//! - [x] `URL` / `URLSearchParams` (see url.rs for the parser's declared subset — no IDNA)
//! - [x] `performance.now()` (+`timeOrigin`), `navigator.userAgent`
//! - [x] `crypto.getRandomValues` / `crypto.randomUUID` (`/dev/urandom` via std::fs — no
//!   syscalls, no crates), `crypto.subtle.digest` (SHA-256 only)
//! - [x] `fetch` / `Headers` / `Request` / `Response` — **http only**: TLS cannot be built on
//!   std and is not implemented (STOP-AND-FLAG); https rejects with a clear error
//! - [~] `Lumen.serve` — an HTTP/1.1 *server* (not a WinterTC API; follows the cross-runtime
//!   `serve((request) => Response)` convention of Deno/Bun/Workers). v1 is single-accept,
//!   `Connection: close`, buffered bodies, http only — see `server.rs` for what's deferred.
//! - [~] Streams: `ReadableStream` (default reader, async iteration, `getReader`/`cancel`/
//!   `values`) backing Request/Response `.body` over the buffered bytes; no BYOB/byte streams,
//!   `tee()`, piping, `WritableStream`, or `TransformStream` yet. Bodies remain buffered, so a
//!   stream used as a body must produce its data synchronously.
//! - [ ] `Blob` / `File` / `FormData`, `URLPattern`, `TextEncoderStream`/`TextDecoderStream`,
//!   `crypto.subtle` beyond digest, `WebSocket`, compression streams

use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::time::Instant;

use lumen_host::{ops, Ctx, Extension, OpState, SpawnHandle, TaskRegistry, Value};

mod http;
mod server;
mod sha256;
mod url;
// The decoder parses the whole binary format; the MVP interpreter doesn't consume every field yet
// (reserved value-type data, mutability flags, etc.), and a few opcode matches read cleaner as
// explicit lists than ranges.
#[allow(dead_code, clippy::manual_range_patterns)]
mod wasm;
mod wasm_ops;

pub fn extension() -> Extension {
    Extension {
        name: "web",
        globals: &[],
        namespaces: &[
            (
                "__perf",
                ops!["now" (0) => op_perf_now, "timeOrigin" (0) => op_time_origin],
            ),
            (
                "__encoding",
                ops!["encode" (1) => op_encode, "decode" (2) => op_decode],
            ),
            ("__url", ops!["parse" (2) => op_url_parse]),
            ("__http", ops!["request" (6) => op_http_request]),
            (
                "__http_server",
                ops![
                    "listen" (3) => server::op_server_listen,
                    "respond" (7) => server::op_server_respond,
                    "close" (1) => server::op_server_close,
                    "version" (0) => server::op_server_version,
                ],
            ),
            (
                "__crypto",
                ops![
                    "fill" (1) => op_random_fill,
                    "uuid" (0) => op_uuid,
                    "sha256" (1) => op_sha256,
                ],
            ),
            (
                "__compress",
                ops![
                    "deflate" (1) => op_deflate,
                    "inflate" (1) => op_inflate,
                    "deflateRaw" (1) => op_deflate_raw,
                    "inflateRaw" (1) => op_inflate_raw,
                    "gzip" (1) => op_gzip,
                    "gunzip" (1) => op_gunzip,
                ],
            ),
            (
                "__wasm",
                ops![
                    "validate" (1) => wasm_ops::op_validate,
                    "compile" (1) => wasm_ops::op_compile,
                    "moduleExports" (1) => wasm_ops::op_module_exports,
                    "moduleImports" (1) => wasm_ops::op_module_imports,
                    "allocMemory" (2) => wasm_ops::op_alloc_memory,
                    "allocTable" (2) => wasm_ops::op_alloc_table,
                    "allocGlobal" (3) => wasm_ops::op_alloc_global,
                    "instantiate" (2) => wasm_ops::op_instantiate,
                    "call" (2) => wasm_ops::op_call,
                    "memBytes" (1) => wasm_ops::op_mem_bytes,
                    "memWrite" (3) => wasm_ops::op_mem_write,
                    "memGrow" (2) => wasm_ops::op_mem_grow,
                    "tableGet" (2) => wasm_ops::op_table_get,
                    "tableSet" (3) => wasm_ops::op_table_set,
                    "tableSize" (1) => wasm_ops::op_table_size,
                    "globalGet" (1) => wasm_ops::op_global_get,
                    "globalSet" (2) => wasm_ops::op_global_set,
                ],
            ),
        ],
        state_init: Some(|state: &mut OpState| {
            state.put(WebState::default());
            state.put(server::ServerRegistry::default());
            state.put(wasm_ops::WasmStore::default());
        }),
        js_init: Some(JS_GLUE),
        js_init_snapshot: Some(JS_GLUE_SNAPSHOT),
    }
}

/// One IIFE (preamble captures and deletes the raw `__*` namespaces, the rest defines the
/// standard classes over them), assembled by `build.rs` from `src/js/*.js` — the single source
/// of truth. `JS_GLUE` is the fallback source; `JS_GLUE_SNAPSHOT` is its precompiled AST, decoded
/// at boot to skip re-parsing (see `lumen_host::install` / `Engine::eval_snapshot`).
const JS_GLUE: &str = include_str!(concat!(env!("OUT_DIR"), "/web_glue.js"));
const JS_GLUE_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web_glue.snap"));

#[derive(Default)]
struct WebState {
    /// `performance.now()`'s monotonic zero point and the wall-clock time (`timeOrigin`, Unix ms)
    /// captured at the same instant — set together on first access.
    start: Option<Instant>,
    time_origin_ms: f64,
    /// Cached `/dev/urandom` handle (macOS/Linux; the only randomness std can reach without
    /// syscalls or crates).
    urandom: Option<RefCell<File>>,
}

impl WebState {
    /// The monotonic clock's zero point, initializing it (and the paired `timeOrigin`) on first use.
    fn clock_start(&mut self) -> Instant {
        if self.start.is_none() {
            self.start = Some(Instant::now());
            self.time_origin_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
        }
        self.start.unwrap()
    }
}

fn op_perf_now(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let state = ctx.host_mut::<WebState>().expect("web state installed");
    let start = state.clock_start();
    Ok(Value::Num(start.elapsed().as_secs_f64() * 1000.0))
}

/// `performance.timeOrigin`: Unix-epoch milliseconds at the monotonic clock's zero point.
fn op_time_origin(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let state = ctx.host_mut::<WebState>().expect("web state installed");
    state.clock_start();
    Ok(Value::Num(state.time_origin_ms))
}

// ---- encoding ----

fn op_encode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let s = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?;
    let bytes = s.as_bytes().to_vec();
    ctx.make_uint8array(&bytes)
}

/// `(u8array, fatal)`; the glue has already converted ArrayBuffer inputs to views.
fn op_decode(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "TextDecoder.decode expects a BufferSource"));
    };
    let fatal = matches!(args.get(1), Some(Value::Bool(true)));
    if fatal {
        match String::from_utf8(bytes) {
            Ok(s) => Ok(Value::from_string(s)),
            Err(_) => Err(ctx.make_error("TypeError", "TextDecoder: invalid utf-8 (fatal)")),
        }
    } else {
        Ok(Value::from_string(
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    }
}

// ---- url ----

/// `(input, base?)` -> component object. Throws TypeError, as the URL constructor must.
fn op_url_parse(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let input = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let base = match args.get(1) {
        None | Some(Value::Undefined) => None,
        Some(v) => Some(ctx.coerce_string(v)?.to_string()),
    };
    let u = url::parse(&input, base.as_deref())
        .map_err(|e| ctx.make_error("TypeError", format!("URL: {e}")))?;
    let obj = Value::Obj(ctx.new_object());
    let port = u.port.map(|p| p.to_string()).unwrap_or_default();
    let href = u.href();
    let origin = u.origin();
    for (k, v) in [
        ("scheme", u.scheme),
        ("username", u.username),
        ("password", u.password),
        ("host", u.host),
        ("port", port),
        ("path", u.path),
        ("query", u.query),
        ("fragment", u.fragment),
        ("href", href),
        ("origin", origin),
    ] {
        let _ = ctx.set_member(&obj, k, Value::from_string(v));
    }
    Ok(obj)
}

// ---- crypto ----

fn random_bytes(ctx: &mut Ctx, n: usize) -> Result<Vec<u8>, Value> {
    let state = ctx.host_mut::<WebState>().expect("web state installed");
    if state.urandom.is_none() {
        match File::open("/dev/urandom") {
            Ok(f) => state.urandom = Some(RefCell::new(f)),
            Err(e) => {
                return Err(ctx.make_error("Error", format!("no randomness source: {e}")));
            }
        }
    }
    let mut buf = vec![0u8; n];
    let ok = {
        let state = ctx.host_mut::<WebState>().expect("just set");
        let f = state.urandom.as_ref().expect("just set");
        f.borrow_mut().read_exact(&mut buf).is_ok()
    };
    if !ok {
        return Err(ctx.make_error("Error", "randomness source read failed"));
    }
    Ok(buf)
}

/// Fill the given typed array in place (the glue enforces the 65536-byte quota + returns it).
fn op_random_fill(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    let Some(existing) = ctx.typed_array_bytes(&v) else {
        return Err(ctx.make_error("TypeError", "getRandomValues expects a typed array"));
    };
    let bytes = random_bytes(ctx, existing.len())?;
    ctx.typed_array_set_bytes(&v, &bytes);
    Ok(Value::Undefined)
}

fn op_uuid(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let mut b = random_bytes(ctx, 16)?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    let s = h.join("");
    Ok(Value::from_string(format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )))
}

fn op_sha256(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "digest expects a BufferSource"));
    };
    let digest = sha256::sha256(&bytes);
    ctx.make_uint8array(&digest)
}

// ---- compression (DEFLATE/zlib/gzip, backing CompressionStream/DecompressionStream) ----

fn compress_op(
    ctx: &mut Ctx,
    args: &[Value],
    codec: fn(&[u8]) -> Vec<u8>,
) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "compression expects a BufferSource"));
    };
    ctx.make_uint8array(&codec(&bytes))
}

fn decompress_op(
    ctx: &mut Ctx,
    args: &[Value],
    codec: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Value, Value> {
    let v = args.first().unwrap_or(&Value::Undefined);
    let Some(bytes) = ctx.typed_array_bytes(v) else {
        return Err(ctx.make_error("TypeError", "decompression expects a BufferSource"));
    };
    match codec(&bytes) {
        Ok(out) => ctx.make_uint8array(&out),
        Err(e) => Err(ctx.make_error("TypeError", e)),
    }
}

fn op_deflate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    compress_op(ctx, a, lumen_host::deflate::zlib_compress)
}
fn op_inflate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    decompress_op(ctx, a, lumen_host::deflate::zlib_decompress)
}
fn op_deflate_raw(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    compress_op(ctx, a, lumen_host::deflate::deflate)
}
fn op_inflate_raw(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    decompress_op(ctx, a, lumen_host::deflate::inflate)
}
fn op_gzip(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    compress_op(ctx, a, lumen_host::deflate::gzip_compress)
}
fn op_gunzip(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    decompress_op(ctx, a, lumen_host::deflate::gzip_decompress)
}

// ---- fetch ----

/// `(method, url, headerPairs, bodyOrUndefined, resolve, reject)`: one HTTP request on the
/// threadpool, settled through the TaskRegistry like every async op.
fn op_http_request(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let method = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let target = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let headers = read_header_pairs(ctx, args.get(2).unwrap_or(&Value::Undefined))?;
    let body = match args.get(3) {
        None | Some(Value::Undefined) | Some(Value::Null) => None,
        Some(v) => match ctx.typed_array_bytes(v) {
            Some(bytes) => Some(bytes),
            None => Some(ctx.coerce_string(v)?.as_bytes().to_vec()),
        },
    };
    let (resolve, reject) = match (args.get(4), args.get(5)) {
        (Some(res), Some(rej)) if res.is_callable() && rej.is_callable() => {
            (res.clone(), rej.clone())
        }
        _ => return Err(ctx.make_error("TypeError", "__http.request expects (resolve, reject)")),
    };
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_http);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    spawn.spawn_blocking(id, move || {
        Box::new(http::request(&method, &target, &headers, body.as_deref()))
    });
    Ok(Value::Undefined)
}

/// A JS `[[k, v], ...]` array into Rust pairs, via the curated member API.
pub(crate) fn read_header_pairs(ctx: &mut Ctx, v: &Value) -> Result<Vec<(String, String)>, Value> {
    let mut out = Vec::new();
    if v.as_obj().is_none() {
        return Ok(out);
    }
    let len = ctx
        .get_member(v, "length")
        .map_err(|_| ctx.make_error("TypeError", "__http.request: headers must be an array"))?;
    let Value::Num(len) = len else {
        return Ok(out);
    };
    for i in 0..(len as usize) {
        let pair = ctx
            .get_member(v, &i.to_string())
            .unwrap_or(Value::Undefined);
        let k = ctx.get_member(&pair, "0").unwrap_or(Value::Undefined);
        let val = ctx.get_member(&pair, "1").unwrap_or(Value::Undefined);
        out.push((
            ctx.coerce_string(&k)?.to_string(),
            ctx.coerce_string(&val)?.to_string(),
        ));
    }
    Ok(out)
}

/// Build the raw-response object the JS glue wraps into a `Response`.
fn decode_http(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let result = *payload
        .downcast::<Result<http::HttpResponse, String>>()
        .expect("http payload");
    let response = match result {
        Ok(r) => r,
        Err(message) => return Err(ctx.make_error("TypeError", message)),
    };
    let obj = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&obj, "status", Value::Num(response.status as f64));
    let _ = ctx.set_member(&obj, "statusText", Value::from_string(response.status_text));
    let _ = ctx.set_member(&obj, "url", Value::from_string(response.url));
    let pairs: Vec<Value> = response
        .headers
        .into_iter()
        .map(|(k, v)| ctx.make_array(vec![Value::from_string(k), Value::from_string(v)]))
        .collect();
    let headers = ctx.make_array(pairs);
    let _ = ctx.set_member(&obj, "headers", headers);
    let body = ctx.make_uint8array(&response.body)?;
    let _ = ctx.set_member(&obj, "body", body);
    Ok(vec![obj])
}
