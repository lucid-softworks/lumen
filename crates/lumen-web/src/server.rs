//! A blocking HTTP/1.1 *server* on `std::net::TcpListener`, the mirror of the fetch client in
//! `http.rs`. `Lumen.serve(handler)` (see `js/server.js`) drives a WinterCG-style fetch
//! handler: each connection is parsed into a `Request`, the handler returns a `Response`, and
//! the bytes are written back — the same `(Request) -> Response` contract Deno.serve/Bun.serve
//! and Cloudflare Workers converge on (WinterTC's Minimum Common API standardizes
//! fetch/Request/Response but not a server, so this follows that cross-runtime convention).
//!
//! ## How it runs on the loop
//! The engine is single-threaded and `!Send`, so the JS handler must run on the loop thread.
//! We reuse the runtime's only async primitive — `spawn_blocking` + a `TaskRegistry`
//! completion — with a **re-arm** pattern: one accept task runs on a pool thread, blocks in
//! `accept()`, reads+parses one request, and comes back to the loop as a completion. The
//! completion decoder ([`decode_accept`]) hands the request to JS *and* arms the next accept,
//! so the listener keeps running for the life of the process (an always-registered task is
//! also what keeps the event loop from going idle). Responses are written back on the pool
//! (blocking `write`), settled through the registry like any other async op.
//!
//! ## What's intentionally missing (v1 — cold-start / low-concurrency focus)
//! - **Concurrency**: exactly one `accept()` is in flight at a time, and it holds one of the
//!   pool's worker threads while blocked. Fine for cold-start latency and light load; a real
//!   readiness reactor (epoll/kqueue) or a dedicated listener thread is future work and would
//!   need raw syscalls (out of scope under the zero-dep policy) or a new host primitive.
//! - **Keep-alive**: every response is `Connection: close`; one request per connection.
//! - **Streaming**: request and response bodies are fully buffered (no chunked *response*
//!   output, no backpressure) — same limitation the fetch client / body streams have today.
//! - **No HTTP/2, no `Expect: 100-continue`, no trailers on responses, no `Date` header**
//!   (formatting an HTTP-date without a date library is deferred), **no TLS/https** (same
//!   STOP-AND-FLAG as the client: TLS can't be built on std alone).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_host::{Ctx, SpawnHandle, TaskRegistry, Value};

use crate::http::read_chunked;
use crate::read_header_pairs;

/// A slow or idle client must not pin a pool worker forever: bound the header/body read.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Request-line + headers size cap (a hostile client can't balloon the worker).
const MAX_HEADER_BYTES: usize = 64 << 10;
/// Request-body cap, matching the client's response cap in `http.rs`.
const MAX_BODY: u64 = 32 << 20;

/// Live servers, keyed by the id handed to JS. Lives in `OpState`; the accept decoder reads it
/// to re-arm and the `close` op flips the `closed` flag. JS values (`dispatch`) are `!Send` and
/// so only ever touched on the loop thread — never moved into a pool closure.
#[derive(Default)]
pub(crate) struct ServerRegistry {
    next: u64,
    servers: HashMap<u64, ServerEntry>,
}

struct ServerEntry {
    listener: Arc<TcpListener>,
    local_addr: SocketAddr,
    /// Flipped by `close()`; the accept loop checks it and stops re-arming.
    closed: Arc<AtomicBool>,
    /// The fixed JS `__dispatch(serverId, connId, ...)` callback (same for every accept of
    /// this server); it looks the user handler up by id.
    dispatch: Value,
}

/// What one accept task sends back to the loop (all fields `Send`).
struct AcceptTaskResult {
    server_id: u64,
    outcome: AcceptOutcome,
}

enum AcceptOutcome {
    /// A parsed request plus the still-open socket to answer on.
    Request(Accepted),
    /// The listener was shut down (or `accept()` failed): stop serving this server.
    Closed,
}

struct Accepted {
    stream: TcpStream,
    peer: SocketAddr,
    method: String,
    /// Absolute URL (`http://<host><target>`) so the JS `Request` constructor accepts it.
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

// ---- native ops -------------------------------------------------------------------------------

/// `__http_server.listen(hostname, port, dispatch)` -> `[serverId, boundPort]`. Binds, registers
/// the server, and arms the first accept. Throws if the bind fails.
pub(crate) fn op_server_listen(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let host = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let port = match args.get(1) {
        Some(Value::Num(n)) if *n >= 0.0 && *n <= 65535.0 => *n as u16,
        _ => return Err(ctx.make_error("TypeError", "listen: port must be 0..=65535")),
    };
    let dispatch = match args.get(2) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "listen: dispatch must be a function")),
    };

    let listener = TcpListener::bind((host.as_str(), port))
        .map_err(|e| ctx.make_error("Error", format!("listen {host}:{port}: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| ctx.make_error("Error", format!("local_addr: {e}")))?;
    let listener = Arc::new(listener);
    let closed = Arc::new(AtomicBool::new(false));

    let id = {
        let reg = ctx
            .host_mut::<ServerRegistry>()
            .expect("web installs ServerRegistry");
        let id = reg.next;
        reg.next += 1;
        reg.servers.insert(
            id,
            ServerEntry {
                listener: Arc::clone(&listener),
                local_addr,
                closed: Arc::clone(&closed),
                dispatch: dispatch.clone(),
            },
        );
        id
    };

    arm_accept(ctx, id, listener, closed, local_addr, dispatch);

    Ok(ctx.make_array(vec![
        Value::Num(id as f64),
        Value::Num(local_addr.port() as f64),
    ]))
}

/// `__http_server.respond(connId, status, statusText, headers, body, resolve, reject)`.
/// Serializes the response and writes it on the pool; the promise settles when the write
/// finishes (or the client hung up). The socket is taken out of the resource table here, so a
/// second respond on the same connection rejects.
pub(crate) fn op_server_respond(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let conn_id = match args.first() {
        Some(Value::Num(n)) if *n >= 0.0 => *n as u32,
        _ => return Err(ctx.make_error("TypeError", "respond: bad connection id")),
    };
    let status = match args.get(1) {
        Some(Value::Num(n)) if *n >= 100.0 && *n <= 599.0 => *n as u16,
        _ => return Err(ctx.make_error("TypeError", "respond: status must be 100..=599")),
    };
    let status_text = ctx
        .coerce_string(args.get(2).unwrap_or(&Value::Undefined))?
        .to_string();
    let headers = read_header_pairs(ctx, args.get(3).unwrap_or(&Value::Undefined))?;
    let body = match args.get(4) {
        None | Some(Value::Undefined) | Some(Value::Null) => Vec::new(),
        Some(v) => ctx.typed_array_bytes(v).unwrap_or_default(),
    };
    let (resolve, reject) = match (args.get(5), args.get(6)) {
        (Some(res), Some(rej)) if res.is_callable() && rej.is_callable() => {
            (res.clone(), rej.clone())
        }
        _ => return Err(ctx.make_error("TypeError", "respond expects (resolve, reject)")),
    };

    // Take ownership of the socket (removing it from the table); `Rc::try_unwrap` succeeds
    // because the table held the only reference.
    let stream = ctx
        .resource_table()
        .close(conn_id)
        .and_then(|rc| rc.downcast::<TcpStream>().ok())
        .and_then(|rc| std::rc::Rc::try_unwrap(rc).ok());
    let Some(stream) = stream else {
        return Err(ctx.make_error("TypeError", "respond: unknown or already-answered connection"));
    };

    let bytes = build_response(status, &status_text, &headers, &body);

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_write);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    spawn.spawn_blocking(id, move || Box::new(write_all_and_close(stream, &bytes)));

    Ok(Value::Undefined)
}

/// `__http_server.close(serverId)`. Flags the listener closed and pokes it with a throwaway
/// connection so the blocked `accept()` wakes, sees the flag, and stops re-arming.
pub(crate) fn op_server_close(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) if *n >= 0.0 => *n as u64,
        _ => return Ok(Value::Undefined),
    };
    let target = ctx
        .host_mut::<ServerRegistry>()
        .and_then(|reg| reg.servers.get(&id))
        .map(|e| (Arc::clone(&e.closed), e.local_addr));
    if let Some((closed, local_addr)) = target {
        closed.store(true, Ordering::SeqCst);
        // Connect to a concrete loopback address when bound to the wildcard, so the wake
        // actually reaches our listener.
        let wake_addr = if local_addr.ip().is_unspecified() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_addr.port())
        } else {
            local_addr
        };
        let _ = TcpStream::connect(wake_addr);
    }
    Ok(Value::Undefined)
}

/// `__http_server.version()` -> the runtime version string (backs `Lumen.version`).
pub(crate) fn op_server_version(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::from_string(env!("CARGO_PKG_VERSION").to_string()))
}

// ---- completion decoders (run on the loop thread with &mut Ctx) --------------------------------

/// Settle one accept: on a request, stash the socket, re-arm the next accept, and return the
/// args for `__dispatch(serverId, connId, method, url, headers, body, remoteHost, remotePort)`.
/// On close, return `[serverId, -1]` (JS resolves `finished`) and do not re-arm.
fn decode_accept(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let AcceptTaskResult { server_id, outcome } =
        *payload.downcast::<AcceptTaskResult>().expect("accept payload");

    let accepted = match outcome {
        AcceptOutcome::Closed => {
            if let Some(reg) = ctx.host_mut::<ServerRegistry>() {
                reg.servers.remove(&server_id);
            }
            return Ok(vec![Value::Num(server_id as f64), Value::Num(-1.0)]);
        }
        AcceptOutcome::Request(accepted) => accepted,
    };

    // Re-arm the next accept from the still-live server entry (a concurrent close removes the
    // entry, in which case this connection is the last one we serve).
    let rearm = ctx
        .host_mut::<ServerRegistry>()
        .and_then(|reg| reg.servers.get(&server_id))
        .map(|e| {
            (
                Arc::clone(&e.listener),
                Arc::clone(&e.closed),
                e.local_addr,
                e.dispatch.clone(),
            )
        });

    let peer = accepted.peer;
    let method = accepted.method;
    let url = accepted.url;
    let header_pairs = accepted.headers;
    let body = accepted.body;
    let conn_id = ctx.resource_table().add(accepted.stream);

    if let Some((listener, closed, local_addr, dispatch)) = rearm {
        arm_accept(ctx, server_id, listener, closed, local_addr, dispatch);
    }

    let headers_val = header_pairs_to_js(ctx, &header_pairs);
    let body_val = if body.is_empty() {
        Value::Undefined
    } else {
        ctx.make_uint8array(&body)?
    };
    Ok(vec![
        Value::Num(server_id as f64),
        Value::Num(conn_id as f64),
        Value::from_string(method),
        Value::from_string(url),
        headers_val,
        body_val,
        Value::from_string(peer.ip().to_string()),
        Value::Num(peer.port() as f64),
    ])
}

/// Settle a response write: `resolve()` on success, `reject(error)` if the socket write failed
/// (client hung up mid-response, etc.).
fn decode_write(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<(), String>>().expect("write payload") {
        Ok(()) => Ok(vec![]),
        Err(message) => Err(ctx.make_error("Error", message)),
    }
}

// ---- helpers ----------------------------------------------------------------------------------

/// Register a fresh accept task (its `on_ok` is `dispatch`) and spawn it on the pool.
fn arm_accept(
    ctx: &mut Ctx,
    server_id: u64,
    listener: Arc<TcpListener>,
    closed: Arc<AtomicBool>,
    local_addr: SocketAddr,
    dispatch: Value,
) {
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(dispatch, None, decode_accept);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    let fallback_host = local_addr.to_string();
    spawn.spawn_blocking(id, move || {
        Box::new(AcceptTaskResult {
            server_id,
            outcome: accept_one(&listener, &closed, &fallback_host),
        })
    });
}

/// Accept and parse exactly one good request (answering malformed ones with 400 inline and
/// moving on), or report the listener as closed.
fn accept_one(listener: &TcpListener, closed: &AtomicBool, fallback_host: &str) -> AcceptOutcome {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) => return AcceptOutcome::Closed,
        };
        if closed.load(Ordering::SeqCst) {
            return AcceptOutcome::Closed; // woken by close()'s throwaway connection
        }
        stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
        match read_request(&stream, fallback_host) {
            Ok((method, url, headers, body)) => {
                return AcceptOutcome::Request(Accepted {
                    stream,
                    peer,
                    method,
                    url,
                    headers,
                    body,
                })
            }
            Err(msg) => {
                let _ = write_simple(&stream, 400, "Bad Request", msg.as_bytes());
                // Keep serving: fall through to accept the next connection.
            }
        }
    }
}

/// Parse a request off the socket: request line, headers, and a Content-Length/chunked body.
/// Returns `(METHOD, absolute-url, headers, body)`.
#[allow(clippy::type_complexity)]
fn read_request(
    stream: &TcpStream,
    fallback_host: &str,
) -> Result<(String, String, Vec<(String, String)>, Vec<u8>), String> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    let n = reader
        .read_line(&mut request_line)
        .map_err(|e| format!("read request line: {e}"))?;
    if n == 0 {
        return Err("empty request".to_string());
    }
    let mut parts = request_line.trim_end().split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err("malformed request line".to_string());
    }

    let mut headers = Vec::new();
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let hn = reader
            .read_line(&mut line)
            .map_err(|e| format!("read headers: {e}"))?;
        if hn == 0 {
            break; // EOF before the blank line: tolerate it
        }
        header_bytes += hn;
        if header_bytes > MAX_HEADER_BYTES {
            return Err("request headers too large".to_string());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(i) = line.find(':') {
            headers.push((line[..i].to_string(), line[i + 1..].trim().to_string()));
        }
    }

    let host = header(&headers, "host").unwrap_or_else(|| fallback_host.to_string());
    let url = if target.starts_with("http://") || target.starts_with("https://") {
        target // absolute-form (proxy requests)
    } else if target.starts_with('/') {
        format!("http://{host}{target}")
    } else {
        // authority-form (CONNECT) or asterisk-form: not meaningfully routable; give a URL the
        // Request constructor still accepts.
        format!("http://{host}/{target}")
    };

    let body = read_request_body(&mut reader, &headers, &method)?;
    Ok((method.to_ascii_uppercase(), url, headers, body))
}

fn read_request_body(
    reader: &mut impl BufRead,
    headers: &[(String, String)],
    method: &str,
) -> Result<Vec<u8>, String> {
    if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
        return Ok(Vec::new());
    }
    if header(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return read_chunked(reader).map_err(|e| format!("chunked body: {e}"));
    }
    match header(headers, "content-length").and_then(|v| v.parse::<u64>().ok()) {
        Some(len) => {
            let mut body = vec![0u8; len.min(MAX_BODY) as usize];
            std::io::Read::read_exact(reader, &mut body).map_err(|e| format!("read body: {e}"))?;
            Ok(body)
        }
        // No framing on a request means no body (unlike a response, there is no read-to-EOF).
        None => Ok(Vec::new()),
    }
}

/// Serialize a response. We own `Connection`/`Transfer-Encoding` and add `Content-Length` and a
/// `Server` header when the handler didn't set them; everything else the handler chose passes
/// through verbatim.
fn build_response(
    status: u16,
    status_text: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let reason = if status_text.is_empty() {
        reason_phrase(status)
    } else {
        status_text
    };
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());

    let (mut has_content_length, mut has_server) = (false, false);
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("transfer-encoding") {
            continue; // we own connection framing
        }
        has_content_length |= k.eq_ignore_ascii_case("content-length");
        has_server |= k.eq_ignore_ascii_case("server");
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !has_content_length {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    if !has_server {
        out.extend_from_slice(
            concat!("Server: lumen/", env!("CARGO_PKG_VERSION"), "\r\n").as_bytes(),
        );
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

/// A bare status-only response used for the inline 400 path.
fn write_simple(stream: &TcpStream, status: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    let mut s = stream;
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn write_all_and_close(stream: TcpStream, bytes: &[u8]) -> Result<(), String> {
    let mut s = &stream;
    s.write_all(bytes)
        .map_err(|e| format!("response write: {e}"))?;
    s.flush().ok();
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn header_pairs_to_js(ctx: &Ctx, headers: &[(String, String)]) -> Value {
    let pairs = headers
        .iter()
        .map(|(k, v)| {
            ctx.make_array(vec![
                Value::from_string(k.clone()),
                Value::from_string(v.clone()),
            ])
        })
        .collect();
    ctx.make_array(pairs)
}

/// The default reason phrase for the common statuses; anything else gets an empty phrase (valid
/// per HTTP — clients ignore it).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}
