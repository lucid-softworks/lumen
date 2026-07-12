//! Server-Sent Events transport (WHATWG HTML §9.2) over HTTP or verified HTTPS — a streaming HTTP
//! GET whose `text/event-stream` body is delivered chunk-by-chunk to `js/eventsource.js`, which
//! runs the line parser and reconnection logic. The fetch client fully buffers responses, so an
//! endless event stream can't ride on it; this is a dedicated streaming reader (the same
//! connect→read re-arm shape as the WebSocket client).
//!
//! ## What's intentionally missing (v1)
//! - **CORS / credentials** — server runtime, no origin model (matches fetch here).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_host::{Ctx, SpawnHandle, TaskRegistry, Value};

use crate::url;

const READ_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_HEADER_BYTES: usize = 64 << 10;
/// One streamed body read; the SSE parser reassembles across chunks, so this is just a buffer.
const CHUNK: usize = 16 << 10;
const MAX_REDIRECTS: u8 = 5;

#[derive(Default)]
pub(crate) struct SseRegistry {
    next: u64,
    conns: HashMap<u64, SseEntry>,
}

struct SseEntry {
    closed: Arc<AtomicBool>,
    dispatch: Value,
    dead: bool,
}

/// The connect task result: the open stream + the response status, or an error.
struct ConnectResult {
    id: u64,
    outcome: Result<Box<dyn SseStream>, ConnectError>,
}

enum ConnectError {
    /// A non-network failure that must NOT reconnect (bad scheme, wrong content-type, 204,
    /// a 4xx/5xx that isn't retriable): `(message, reconnect=false)`.
    Fatal(String),
    /// A network-level failure the client may retry after its reconnection delay.
    Retriable(String),
}

/// A streamed body read: `Some(bytes)` (possibly empty on a spurious wakeup) or end-of-stream.
struct ReadResult {
    id: u64,
    outcome: std::io::Result<Vec<u8>>,
    /// Handed back to re-arm the next read (None on EOF/error).
    reader: Option<StreamReader>,
}

struct StreamReader {
    stream: Box<dyn SseStream>,
    closed: Arc<AtomicBool>,
}

trait SseStream: Read + Write + Send {}
impl<T: Read + Write + Send> SseStream for T {}

fn sse_registry(ctx: &mut Ctx) -> &mut SseRegistry {
    ctx.host_mut::<SseRegistry>().expect("web installs SseRegistry")
}

/// `__sse.connect(url, lastEventId, dispatch)` → id. Opens the stream on the pool; the JS side
/// then receives `("open")`, `("chunk", u8array)`, `("fatal", message)` (no reconnect), or
/// `("drop", message)` (reconnect per the retry interval).
pub(crate) fn op_sse_connect(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let last_event_id = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let dispatch = match args.get(2) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "connect: dispatch must be a function")),
    };

    let u = url::parse(&target, None).map_err(|e| ctx.make_error("SyntaxError", e))?;
    match u.scheme.as_str() {
        "http" | "https" => {}
        other => {
            return Err(ctx.make_error("SyntaxError", format!("EventSource: unsupported scheme '{other}'")))
        }
    }

    let id = {
        let reg = sse_registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.conns.insert(
            id,
            SseEntry {
                closed: Arc::new(AtomicBool::new(false)),
                dispatch: dispatch.clone(),
                dead: false,
            },
        );
        id
    };

    let task = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(dispatch, None, decode_connect);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    spawn.spawn_blocking(task, move || {
        Box::new(ConnectResult {
            id,
            outcome: open_stream(&target, &last_event_id),
        })
    });
    Ok(Value::Num(id as f64))
}

/// GET `target` with the SSE request headers, following redirects, and validate the response is
/// a `text/event-stream` 200 — returning the still-open stream positioned at the body start.
fn open_stream(target: &str, last_event_id: &str) -> Result<Box<dyn SseStream>, ConnectError> {
    let mut target = target.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let u = url::parse(&target, None).map_err(ConnectError::Fatal)?;
        if u.scheme != "http" && u.scheme != "https" {
            return Err(ConnectError::Fatal(format!("unsupported scheme '{}'", u.scheme)));
        }
        let port = u.port.unwrap_or(if u.scheme == "https" { 443 } else { 80 });
        let host = u.host.trim_matches(['[', ']']);
        let tcp = TcpStream::connect((host, port))
            .map_err(|e| ConnectError::Retriable(format!("connect: {e}")))?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(READ_TIMEOUT)).ok();
        let mut stream: Box<dyn SseStream> = if u.scheme == "https" {
            Box::new(lumen_tls::TlsStream::connect(tcp, host).map_err(ConnectError::Retriable)?)
        } else { Box::new(tcp) };

        let host_header = if u.port.is_some() && u.port != Some(80) {
            format!("{}:{}", u.host, port)
        } else {
            u.host.clone()
        };
        let mut path = if u.path.is_empty() { "/".to_string() } else { u.path.clone() };
        path.push_str(&u.query);
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nAccept: text/event-stream\r\n\
             Cache-Control: no-cache\r\nConnection: keep-alive\r\n"
        );
        if !last_event_id.is_empty() {
            req.push_str(&format!("Last-Event-ID: {last_event_id}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes())
            .map_err(|e| ConnectError::Retriable(format!("request write: {e}")))?;

        // Read the response head byte-wise (a BufReader would swallow body bytes).
        let mut head = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > MAX_HEADER_BYTES {
                return Err(ConnectError::Fatal("response head too large".into()));
            }
            match stream.read_exact(&mut byte) {
                Ok(()) => head.push(byte[0]),
                Err(e) => return Err(ConnectError::Retriable(format!("head read: {e}"))),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut location = None;
        let mut content_type = String::new();
        for line in lines {
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("location") {
                location = Some(v.to_string());
            } else if k.eq_ignore_ascii_case("content-type") {
                content_type = v.to_ascii_lowercase();
            }
        }

        match status {
            200 => {
                if !content_type.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/event-stream") {
                    return Err(ConnectError::Fatal(format!(
                        "EventSource response Content-Type is '{content_type}', not text/event-stream"
                    )));
                }
                return Ok(stream);
            }
            301 | 302 | 303 | 307 | 308 => {
                let Some(loc) = location else {
                    return Err(ConnectError::Fatal("redirect without Location".into()));
                };
                target = url::parse(&loc, Some(&u.href()))
                    .map_err(ConnectError::Fatal)?
                    .href();
            }
            // 204 No Content and 205 tell the client to STOP (no reconnect).
            204 | 205 => return Err(ConnectError::Fatal(format!("server returned {status} (stop)"))),
            _ => return Err(ConnectError::Fatal(format!("server returned HTTP {status}"))),
        }
    }
    Err(ConnectError::Fatal("too many redirects".into()))
}

fn decode_connect(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let ConnectResult { id, outcome } = *payload.downcast::<ConnectResult>().expect("sse payload");
    match outcome {
        Ok(stream) => {
            let closed = match sse_registry(ctx).conns.get(&id) {
                Some(e) if !e.dead => Arc::clone(&e.closed),
                _ => return Ok(vec![Value::from_string("fatal".into()), Value::from_string("closed".into())]),
            };
            arm_read(ctx, id, StreamReader { stream, closed });
            Ok(vec![Value::from_string("open".into())])
        }
        Err(ConnectError::Fatal(msg)) => {
            sse_registry(ctx).conns.remove(&id);
            Ok(vec![Value::from_string("fatal".into()), Value::from_string(msg)])
        }
        Err(ConnectError::Retriable(msg)) => {
            sse_registry(ctx).conns.remove(&id);
            Ok(vec![Value::from_string("drop".into()), Value::from_string(msg)])
        }
    }
}

fn arm_read(ctx: &mut Ctx, id: u64, reader: StreamReader) {
    let dispatch = match sse_registry(ctx).conns.get(&id) {
        Some(e) if !e.dead => e.dispatch.clone(),
        _ => return,
    };
    let task = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(dispatch, None, decode_read);
    let spawn = ctx
        .op_state()
        .get::<SpawnHandle>()
        .expect("runtime installs the spawn handle")
        .clone();
    spawn.spawn_blocking(task, move || {
        let mut reader = reader;
        let mut buf = vec![0u8; CHUNK];
        let outcome = if reader.closed.load(Ordering::SeqCst) {
            Ok(Vec::new()) // treated as EOF below
        } else {
            reader.stream.read(&mut buf).map(|n| buf[..n].to_vec())
        };
        // EOF (0 bytes) or a closed flag ends the loop → the JS side reconnects.
        let keep = matches!(&outcome, Ok(b) if !b.is_empty()) && !reader.closed.load(Ordering::SeqCst);
        Box::new(ReadResult {
            id,
            outcome,
            reader: keep.then_some(reader),
        })
    });
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let ReadResult { id, outcome, reader } = *payload.downcast::<ReadResult>().expect("sse payload");
    // A close() while this read was in flight: swallow and stop.
    let closed_now = sse_registry(ctx)
        .conns
        .get(&id)
        .map(|e| e.dead || e.closed.load(Ordering::SeqCst))
        .unwrap_or(true);
    match outcome {
        Ok(bytes) if !bytes.is_empty() && !closed_now => {
            let arr = ctx.make_uint8array(&bytes)?;
            if let Some(r) = reader {
                arm_read(ctx, id, r);
            }
            Ok(vec![Value::from_string("chunk".into()), arr])
        }
        Ok(_) => {
            // EOF or closed: end this connection. A user close() is final; a server-side EOF
            // reconnects (the JS side decides based on its readyState).
            sse_registry(ctx).conns.remove(&id);
            if closed_now {
                Ok(vec![Value::from_string("closed".into())])
            } else {
                Ok(vec![Value::from_string("drop".into()), Value::from_string("stream ended".into())])
            }
        }
        Err(e) => {
            sse_registry(ctx).conns.remove(&id);
            if closed_now {
                Ok(vec![Value::from_string("closed".into())])
            } else {
                Ok(vec![Value::from_string("drop".into()), Value::from_string(e.to_string())])
            }
        }
    }
}

/// `__sse.close(id)` — flip the closed flag; the reader loop tears down and reports `closed`.
pub(crate) fn op_sse_close(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "close: bad connection id")),
    };
    if let Some(e) = sse_registry(ctx).conns.get_mut(&id) {
        e.dead = true;
        e.closed.store(true, Ordering::SeqCst);
    }
    Ok(Value::Undefined)
}

// ---- test support -------------------------------------------------------------------------------

/// A minimal SSE server for this workspace's tests and benchmarks (NOT part of the runtime):
/// serves a canned `text/event-stream` body, honoring `Last-Event-ID` for the reconnect case.
#[doc(hidden)]
#[allow(dead_code)]
pub mod testing {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// What the SSE server should do.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Mode {
        /// Serve three events (one named, one with an id) then close the stream.
        Events,
        /// First connection: one event with `id: 5`, then drop. Second connection (with
        /// Last-Event-ID: 5): serve a "resumed" event echoing the id, then close.
        Reconnect,
        /// Respond 200 with the WRONG content-type (text/plain) — a fatal error, no reconnect.
        WrongContentType,
        /// Respond 204 — tells the client to stop (no reconnect).
        NoContent,
    }

    /// Spawn the server; returns its port. Serves `conns` connections then exits.
    pub fn spawn(mode: Mode, conns: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut n = 0usize;
            for _ in 0..conns {
                let Ok((stream, _)) = listener.accept() else { return };
                serve_one(stream, mode, n);
                n += 1;
            }
        });
        port
    }

    fn serve_one(stream: std::net::TcpStream, mode: Mode, conn_index: usize) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if (&stream).read_exact(&mut byte).is_err() {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let last_event_id = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("last-event-id").then(|| v.trim().to_string())
        });

        let write = |body: &str, ctype: &str, status: &str| {
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nCache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n{body}"
            );
            let _ = (&stream).write_all(resp.as_bytes());
        };

        match mode {
            Mode::WrongContentType => write("nope", "text/plain", "200 OK"),
            Mode::NoContent => {
                let _ = (&stream).write_all(
                    b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
                );
            }
            Mode::Events => write(
                "data: hello\n\nevent: tick\ndata: 42\n\nid: 9\ndata: line one\ndata: line two\n\n",
                "text/event-stream",
                "200 OK",
            ),
            Mode::Reconnect => {
                if conn_index == 0 {
                    // A short `retry:` so the reconnect (and the test) doesn't wait the default
                    // 3s; then one event with an id, then drop the connection (no clean close).
                    let _ = (&stream).write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                          Connection: close\r\n\r\nretry: 10\nid: 5\ndata: first\n\n",
                    );
                    // Close mid-stream by dropping `stream` here.
                } else {
                    let body = format!("data: resumed-from-{}\n\n", last_event_id.unwrap_or_default());
                    write(&body, "text/event-stream", "200 OK");
                }
            }
        }
    }
}
