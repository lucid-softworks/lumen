//! WebSocket (RFC 6455) on `std::net::TcpStream`: a *client* (the upgrade sibling of the fetch
//! client in `http.rs`, driving the `WebSocket` global in `js/websocket.js`) plus a *server-side
//! adopt* (`op_ws_upgrade`), which takes a connection accepted by the HTTP server in `server.rs`,
//! answers the 101 handshake, and runs it through the same registry/read-loop with unmasked
//! outgoing frames — backing `Lumen.upgradeWebSocket` and Bun.serve's `websocket` option.
//!
//! ## How it runs on the loop
//! Same re-arm pattern as the HTTP server: `connect` runs the TCP dial + HTTP upgrade handshake
//! on a pool thread and comes back as a completion; the completion decoder stores the write half,
//! fires the socket's JS dispatch (`"open"`), and arms a reader task. Each reader task blocks
//! until ONE complete message (transparently answering pings and swallowing pongs), returns it as
//! a completion — which re-arms the next read — and carries the fragmentation-capable reader back
//! and forth by value. Sends/closes write on the loop thread under a per-socket write mutex (a
//! write timeout bounds a stalled peer; real backpressure handling is future work, matching the
//! server's v1 notes).
//!
//! ## What's intentionally missing (v1)
//! - **`wss:`** — rejected with the same guidance as fetch's `https:` (no TLS on std alone).
//! - **permessage-deflate** and other extensions (`extensions` is always `""`).
//! - **Backpressure**: `send` writes synchronously; `bufferedAmount` is 0 once `send` returns.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lumen_host::{Ctx, SpawnHandle, TaskRegistry, Value};

use crate::sha1::sha1;
use crate::url;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Bound reads/writes so a dead peer can't pin a pool worker (reads) or the loop (writes).
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Message size cap (mirrors the HTTP body cap); exceeding it fails the connection with 1009.
const MAX_MESSAGE: usize = 32 << 20;

// ---- base64 (encode only — the handshake key/accept values) -----------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The `Sec-WebSocket-Accept` value for a handshake `Sec-WebSocket-Key` (RFC 6455 §4.2.2 step
/// 5.4). Public: a WebSocket-capable server upgrade (and this crate's own tests) need it too.
pub fn websocket_accept(key: &str) -> String {
    let mut input = key.trim().to_string();
    input.push_str(GUID);
    base64(&sha1(input.as_bytes()))
}

// ---- frame codec -------------------------------------------------------------------------------

/// One decoded event from the wire, message-level (fragments already assembled).
#[derive(Debug, PartialEq)]
pub(crate) enum WsEvent {
    Text(String),
    Binary(Vec<u8>),
    /// Peer close frame: `(code, reason)`; 1005 = no code present.
    Close(u16, String),
}

/// Why a read loop ended without a clean message.
pub(crate) enum WsError {
    /// Protocol violation → fail the connection with this close code.
    Protocol(u16, &'static str),
    /// The socket died (EOF/reset/timeout).
    Io(String),
}

/// Encode one frame. Client frames are ALWAYS masked (RFC 6455 §5.3); `mask` comes from the
/// caller so the codec stays deterministic under test.
pub(crate) fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0f)); // FIN + opcode
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= 0xffff {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// Encode one UNMASKED frame — the server side of the wire (a server MUST NOT mask, RFC 6455
/// §5.1). Used by connections adopted via `op_ws_upgrade` (`Lumen.serve` → WebSocket handoff).
pub(crate) fn encode_frame_unmasked(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | (opcode & 0x0f)); // FIN + opcode
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// A close frame's payload (code + UTF-8 reason).
pub(crate) fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut p = code.to_be_bytes().to_vec();
    p.extend_from_slice(reason.as_bytes());
    p
}

struct RawFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn read_exact_buf(r: &mut impl Read, n: usize) -> Result<Vec<u8>, WsError> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).map_err(|e| WsError::Io(e.to_string()))?;
    Ok(buf)
}

fn read_raw_frame(r: &mut impl Read, max: usize) -> Result<RawFrame, WsError> {
    let head = read_exact_buf(r, 2)?;
    let fin = head[0] & 0x80 != 0;
    if head[0] & 0x70 != 0 {
        return Err(WsError::Protocol(1002, "reserved bits set (no extension negotiated)"));
    }
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7f) as usize;
    if opcode >= 0x8 && (!fin || len > 125) {
        return Err(WsError::Protocol(1002, "malformed control frame"));
    }
    if len == 126 {
        let ext = read_exact_buf(r, 2)?;
        len = u16::from_be_bytes([ext[0], ext[1]]) as usize;
    } else if len == 127 {
        let ext = read_exact_buf(r, 8)?;
        let n = u64::from_be_bytes(ext.try_into().unwrap());
        if n > max as u64 {
            return Err(WsError::Protocol(1009, "message too big"));
        }
        len = n as usize;
    }
    if len > max {
        return Err(WsError::Protocol(1009, "message too big"));
    }
    // A server MUST NOT mask (§5.1); tolerate it by unmasking rather than failing.
    let mask: Option<[u8; 4]> = if masked {
        Some(read_exact_buf(r, 4)?.try_into().unwrap())
    } else {
        None
    };
    let mut payload = read_exact_buf(r, len)?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(RawFrame { fin, opcode, payload })
}

/// The reader half: owns the read stream plus cross-message fragmentation state, and a write
/// handle for transparent ping→pong replies. Moves into each pool read task and back out
/// through its completion.
pub(crate) struct WsReader {
    stream: BufReader<TcpStream>,
    writer: Arc<Mutex<TcpStream>>,
    mask_seed: u64,
    /// true = we are the CLIENT end (mask outgoing pongs); false = server end (never mask).
    masked: bool,
}

impl WsReader {
    fn next_mask(&mut self) -> [u8; 4] {
        // Mask keys need unpredictability only against proxies (RFC 6455 §10.3); a cheap LCG
        // seeded from the handshake's CSPRNG key is fine and keeps the reader self-contained.
        self.mask_seed = self.mask_seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.mask_seed >> 24).to_be_bytes()[4..8].try_into().unwrap()
    }

    /// Block until one complete MESSAGE (or close), answering pings and skipping pongs inline.
    pub(crate) fn read_message(&mut self) -> Result<WsEvent, WsError> {
        let mut partial: Option<(u8, Vec<u8>)> = None;
        loop {
            let frame = read_raw_frame(&mut self.stream, MAX_MESSAGE)?;
            match frame.opcode {
                0x9 => {
                    // Ping → pong with the same payload (§5.5.3); masked only from the client end.
                    let pong = if self.masked {
                        let mask = self.next_mask();
                        encode_frame(0xA, &frame.payload, mask)
                    } else {
                        encode_frame_unmasked(0xA, &frame.payload)
                    };
                    let w = self.writer.lock().unwrap();
                    (&*w).write_all(&pong).map_err(|e| WsError::Io(e.to_string()))?;
                }
                0xA => {} // unsolicited pong: ignore (§5.5.3)
                0x8 => {
                    let (code, reason) = if frame.payload.len() >= 2 {
                        let code = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                        let reason = String::from_utf8(frame.payload[2..].to_vec())
                            .map_err(|_| WsError::Protocol(1007, "close reason is not UTF-8"))?;
                        (code, reason)
                    } else {
                        (1005, String::new())
                    };
                    return Ok(WsEvent::Close(code, reason));
                }
                0x1 | 0x2 => {
                    if partial.is_some() {
                        return Err(WsError::Protocol(1002, "new data frame during fragmented message"));
                    }
                    if frame.fin {
                        return finish_message(frame.opcode, frame.payload);
                    }
                    partial = Some((frame.opcode, frame.payload));
                }
                0x0 => {
                    let Some((op, mut buf)) = partial.take() else {
                        return Err(WsError::Protocol(1002, "continuation without a message"));
                    };
                    if buf.len() + frame.payload.len() > MAX_MESSAGE {
                        return Err(WsError::Protocol(1009, "message too big"));
                    }
                    buf.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return finish_message(op, buf);
                    }
                    partial = Some((op, buf));
                }
                _ => return Err(WsError::Protocol(1002, "unknown opcode")),
            }
        }
    }
}

fn finish_message(opcode: u8, payload: Vec<u8>) -> Result<WsEvent, WsError> {
    if opcode == 0x1 {
        String::from_utf8(payload)
            .map(WsEvent::Text)
            .map_err(|_| WsError::Protocol(1007, "text message is not UTF-8"))
    } else {
        Ok(WsEvent::Binary(payload))
    }
}

// ---- registry + ops ----------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WsRegistry {
    next: u64,
    socks: HashMap<u64, WsEntry>,
}

struct WsEntry {
    /// Write half; `None` until the handshake completes.
    writer: Option<Arc<Mutex<TcpStream>>>,
    /// One close frame ever goes out (ours or the echo of theirs).
    close_sent: Arc<AtomicBool>,
    /// Stops the read re-arm after close/error delivery.
    dead: bool,
    dispatch: Value,
    mask_seed: u64,
    /// true = client end (outgoing frames masked); false = a connection adopted server-side.
    masked: bool,
}

/// What the connect task sends back to the loop.
struct ConnectResult {
    id: u64,
    outcome: Result<ConnectedSocket, String>,
}

struct ConnectedSocket {
    stream: TcpStream,
    protocol: String,
}

/// What one read task sends back to the loop.
struct ReadResult {
    id: u64,
    outcome: Result<WsEvent, WsError>,
    /// Handed back for the next read task (None when the loop should stop).
    reader: Option<WsReader>,
}

fn ws_registry(ctx: &mut Ctx) -> &mut WsRegistry {
    ctx.host_mut::<WsRegistry>().expect("web installs WsRegistry")
}

/// `__ws.connect(url, protocolsJoined, dispatch)` → id. The handshake runs on the pool; the
/// socket's lifecycle then flows entirely through `dispatch(kind, ...)`:
/// `("open", protocol)`, `("text", string)`, `("binary", u8array)`,
/// `("close", code, reason, wasClean)`, `("error", message)`.
pub(crate) fn op_ws_connect(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let target = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let protocols = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let dispatch = match args.get(2) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "connect: dispatch must be a function")),
    };

    let u = url::parse(&target, None).map_err(|e| ctx.make_error("SyntaxError", e))?;
    match u.scheme.as_str() {
        "ws" => {}
        "wss" => {
            return Err(ctx.make_error(
                "Error",
                "WebSocket: wss is not supported yet (TLS cannot be implemented on std alone; \
                 ws URLs work)",
            ))
        }
        other => {
            return Err(ctx.make_error("SyntaxError", format!("WebSocket: unsupported scheme '{other}'")))
        }
    }

    // 16 random bytes for the handshake key (CSPRNG); also seeds the per-socket mask LCG.
    let key_bytes = crate::web_random_bytes(ctx, 16)?;
    let key = base64(&key_bytes);
    let mask_seed = u64::from_be_bytes(key_bytes[0..8].try_into().unwrap()) | 1;

    let id = {
        let reg = ws_registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.socks.insert(
            id,
            WsEntry {
                writer: None,
                close_sent: Arc::new(AtomicBool::new(false)),
                dead: false,
                dispatch: dispatch.clone(),
                mask_seed,
                masked: true,
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
            outcome: handshake(&u, &key, &protocols),
        })
    });
    Ok(Value::Num(id as f64))
}

/// Dial + HTTP/1.1 upgrade (RFC 6455 §4.1/§4.2). Returns the open stream and the negotiated
/// subprotocol ("" when none).
fn handshake(u: &url::Url, key: &str, protocols: &str) -> Result<ConnectedSocket, String> {
    let port = u.port.unwrap_or(80);
    let host = u.host.trim_matches(['[', ']']);
    let stream = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();

    let host_header = if u.port.is_some() && u.port != Some(80) {
        format!("{}:{}", u.host, port)
    } else {
        u.host.clone()
    };
    let mut path = if u.path.is_empty() { "/".to_string() } else { u.path.clone() };
    path.push_str(&u.query); // `query` already carries its leading '?' when present
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    if !protocols.is_empty() {
        req.push_str(&format!("Sec-WebSocket-Protocol: {protocols}\r\n"));
    }
    req.push_str("\r\n");
    (&stream).write_all(req.as_bytes()).map_err(|e| format!("handshake write: {e}"))?;

    // Read the 101 response head. BufReader over a clone would over-read into the frame stream,
    // so read byte-wise until CRLFCRLF (the head is tiny).
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 64 << 10 {
            return Err("handshake response too large".into());
        }
        (&stream)
            .read_exact(&mut byte)
            .map_err(|e| format!("handshake read: {e}"))?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if !status.starts_with("HTTP/1.1 101") && !status.starts_with("HTTP/1.0 101") {
        return Err(format!("handshake refused: {status}"));
    }
    let mut accept = None;
    let mut upgrade_ok = false;
    let mut protocol = String::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("sec-websocket-accept") {
            accept = Some(v.to_string());
        } else if k.eq_ignore_ascii_case("upgrade") {
            upgrade_ok = v.eq_ignore_ascii_case("websocket");
        } else if k.eq_ignore_ascii_case("sec-websocket-protocol") {
            protocol = v.to_string();
        }
    }
    if !upgrade_ok {
        return Err("handshake response missing 'Upgrade: websocket'".into());
    }
    if accept.as_deref() != Some(websocket_accept(key).as_str()) {
        return Err("handshake Sec-WebSocket-Accept mismatch".into());
    }
    // A subprotocol we never offered fails the connection (§4.1 step 5.6).
    if !protocol.is_empty()
        && !protocols
            .split(',')
            .map(str::trim)
            .any(|p| p.eq_ignore_ascii_case(&protocol))
    {
        return Err(format!("server selected unrequested subprotocol '{protocol}'"));
    }
    Ok(ConnectedSocket { stream, protocol })
}

fn decode_connect(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let ConnectResult { id, outcome } = *payload.downcast::<ConnectResult>().expect("ws payload");
    match outcome {
        Err(msg) => {
            if let Some(e) = ws_registry(ctx).socks.get_mut(&id) {
                e.dead = true;
            }
            ws_registry(ctx).socks.remove(&id);
            Ok(vec![Value::from_string("error".into()), Value::from_string(msg)])
        }
        Ok(ConnectedSocket { stream, protocol }) => {
            let write_half = stream.try_clone().map_err(|e| {
                ctx.make_error("Error", format!("WebSocket: socket clone failed: {e}"))
            })?;
            let writer = Arc::new(Mutex::new(write_half));
            let mask_seed = {
                let reg = ws_registry(ctx);
                let Some(entry) = reg.socks.get_mut(&id) else {
                    return Ok(vec![Value::from_string("close".into()), Value::Num(1006.0)]);
                };
                entry.writer = Some(Arc::clone(&writer));
                entry.mask_seed
            };
            let reader = WsReader {
                stream: BufReader::new(stream),
                writer,
                mask_seed,
                masked: true,
            };
            arm_read(ctx, id, reader);
            Ok(vec![Value::from_string("open".into()), Value::from_string(protocol)])
        }
    }
}

fn arm_read(ctx: &mut Ctx, id: u64, mut reader: WsReader) {
    let dispatch = match ws_registry(ctx).socks.get(&id) {
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
        let outcome = reader.read_message();
        let keep = outcome.is_ok() && !matches!(outcome, Ok(WsEvent::Close(..)));
        Box::new(ReadResult {
            id,
            outcome,
            reader: keep.then_some(reader),
        })
    });
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let ReadResult { id, outcome, reader } = *payload.downcast::<ReadResult>().expect("ws payload");
    match outcome {
        Ok(WsEvent::Text(s)) => {
            if let Some(r) = reader {
                arm_read(ctx, id, r);
            }
            Ok(vec![Value::from_string("text".into()), Value::from_string(s)])
        }
        Ok(WsEvent::Binary(b)) => {
            let arr = ctx.make_uint8array(&b)?;
            if let Some(r) = reader {
                arm_read(ctx, id, r);
            }
            Ok(vec![Value::from_string("binary".into()), arr])
        }
        Ok(WsEvent::Close(code, reason)) => {
            // Echo the close (once) so the TCP close handshake completes cleanly (§5.5.1),
            // then tear down.
            let entry = ws_registry(ctx).socks.remove(&id);
            if let Some(e) = entry {
                if let (Some(w), false) = (&e.writer, e.close_sent.swap(true, Ordering::SeqCst)) {
                    let payload = close_payload(if code == 1005 { 1000 } else { code }, "");
                    let frame = if e.masked {
                        encode_frame(0x8, &payload, [0x37, 0x11, 0x9a, 0x42])
                    } else {
                        encode_frame_unmasked(0x8, &payload)
                    };
                    if let Ok(guard) = w.lock() {
                        let _ = (&*guard).write_all(&frame);
                        let _ = guard.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
            Ok(vec![
                Value::from_string("close".into()),
                Value::Num(code as f64),
                Value::from_string(reason),
                Value::Bool(true),
            ])
        }
        Err(WsError::Protocol(code, msg)) => {
            let entry = ws_registry(ctx).socks.remove(&id);
            if let Some(e) = entry {
                if let (Some(w), false) = (&e.writer, e.close_sent.swap(true, Ordering::SeqCst)) {
                    let payload = close_payload(code, msg);
                    let frame = if e.masked {
                        encode_frame(0x8, &payload, [0x37, 0x11, 0x9a, 0x42])
                    } else {
                        encode_frame_unmasked(0x8, &payload)
                    };
                    if let Ok(guard) = w.lock() {
                        let _ = (&*guard).write_all(&frame);
                        let _ = guard.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
            Ok(vec![
                Value::from_string("fail".into()),
                Value::Num(code as f64),
                Value::from_string(msg.to_string()),
            ])
        }
        Err(WsError::Io(msg)) => {
            ws_registry(ctx).socks.remove(&id);
            Ok(vec![Value::from_string("io".into()), Value::from_string(msg)])
        }
    }
}

/// `__ws.send(id, stringOrBytes)` — encodes and writes one masked data frame on the loop thread.
pub(crate) fn op_ws_send(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "send: bad socket id")),
    };
    let (opcode, bytes) = match args.get(1) {
        Some(v) => match ctx.typed_array_bytes(v) {
            Some(b) => (0x2u8, b),
            None => (0x1u8, ctx.coerce_string(v)?.to_string().into_bytes()),
        },
        None => return Err(ctx.make_error("TypeError", "send: missing data")),
    };
    let (writer, mask) = {
        let reg = ws_registry(ctx);
        let Some(e) = reg.socks.get_mut(&id) else {
            return Ok(Value::Bool(false)); // already closed: spec drops silently
        };
        if e.close_sent.load(Ordering::SeqCst) {
            return Ok(Value::Bool(false));
        }
        e.mask_seed = e
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Server-adopted sockets never mask (RFC 6455 §5.1).
        let mask: Option<[u8; 4]> = e
            .masked
            .then(|| (e.mask_seed >> 24).to_be_bytes()[4..8].try_into().unwrap());
        match &e.writer {
            Some(w) => (Arc::clone(w), mask),
            None => return Err(ctx.make_error("Error", "send before open")),
        }
    };
    let frame = match mask {
        Some(m) => encode_frame(opcode, &bytes, m),
        None => encode_frame_unmasked(opcode, &bytes),
    };
    let guard = writer.lock().unwrap();
    (&*guard)
        .write_all(&frame)
        .map_err(|e| ctx.make_error("Error", format!("WebSocket send: {e}")))?;
    Ok(Value::Bool(true))
}

/// `__ws.close(id, code, reason)` — sends the close frame (once); the read loop then surfaces
/// the peer's echo as the close event.
pub(crate) fn op_ws_close(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = match args.first() {
        Some(Value::Num(n)) => *n as u64,
        _ => return Err(ctx.make_error("TypeError", "close: bad socket id")),
    };
    let code = match args.get(1) {
        Some(Value::Num(n)) => *n as u16,
        _ => 1000,
    };
    let reason = ctx
        .coerce_string(args.get(2).unwrap_or(&Value::Undefined))?
        .to_string();
    let (writer, masked) = {
        let reg = ws_registry(ctx);
        let Some(e) = reg.socks.get_mut(&id) else {
            return Ok(Value::Undefined);
        };
        if e.close_sent.swap(true, Ordering::SeqCst) {
            return Ok(Value::Undefined);
        }
        (e.writer.clone(), e.masked)
    };
    if let Some(w) = writer {
        let payload = close_payload(code, &reason);
        let frame = if masked {
            encode_frame(0x8, &payload, [0x1f, 0x2e, 0x3d, 0x4c])
        } else {
            encode_frame_unmasked(0x8, &payload)
        };
        let guard = w.lock().unwrap();
        let _ = (&*guard).write_all(&frame);
    }
    Ok(Value::Undefined)
}

/// `__ws.upgrade(connId, secWebSocketKey, protocol, extraHeaderPairs, dispatch)` → id.
///
/// Adopts a connection accepted by `Lumen.serve` (see server.rs: the parsed request's `TcpStream`
/// sits in the resource table under `connId`) as a SERVER-side WebSocket: writes the RFC 6455
/// 101 handshake response, then joins the same registry/read-loop machinery the client uses —
/// with `masked: false`, since a server must not mask (§5.1). `dispatch(kind, ...)` receives
/// `("text", string)`, `("binary", u8array)`, `("close", code, reason, wasClean)`,
/// `("fail", code, msg)` (protocol violation), and `("io", msg)` (socket died). There is no
/// "open" event: the connection is open the moment this op returns.
pub(crate) fn op_ws_upgrade(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let conn_id = match args.first() {
        Some(Value::Num(n)) if *n >= 0.0 => *n as u32,
        _ => return Err(ctx.make_error("TypeError", "upgrade: bad connection id")),
    };
    let key = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let protocol = ctx
        .coerce_string(args.get(2).unwrap_or(&Value::Undefined))?
        .to_string();
    let extra_headers = crate::read_header_pairs(ctx, args.get(3).unwrap_or(&Value::Undefined))?;
    let dispatch = match args.get(4) {
        Some(v) if v.is_callable() => v.clone(),
        _ => return Err(ctx.make_error("TypeError", "upgrade: dispatch must be a function")),
    };

    // Take the socket out of the resource table (same handoff as respond(); a later respond on
    // this connection now correctly fails as "already answered").
    let stream = ctx
        .resource_table()
        .close(conn_id)
        .and_then(|rc| rc.downcast::<TcpStream>().ok())
        .and_then(|rc| std::rc::Rc::try_unwrap(rc).ok());
    let Some(stream) = stream else {
        return Err(ctx.make_error("TypeError", "upgrade: unknown or already-answered connection"));
    };

    // The accept loop set a read timeout to bound header parsing; a WebSocket idles legitimately.
    stream.set_read_timeout(None).ok();
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
    stream.set_nodelay(true).ok();

    // Write the 101 upgrade response. It is small; writing on the loop thread matches how sends
    // and closes are written.
    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n",
        websocket_accept(&key)
    );
    if !protocol.is_empty() {
        resp.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    for (name, value) in &extra_headers {
        // The handshake-critical headers above must not be overridden by user extras.
        if name.eq_ignore_ascii_case("upgrade")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("sec-websocket-accept")
        {
            continue;
        }
        resp.push_str(&format!("{name}: {value}\r\n"));
    }
    resp.push_str("\r\n");
    (&stream)
        .write_all(resp.as_bytes())
        .map_err(|e| ctx.make_error("Error", format!("WebSocket upgrade: handshake write: {e}")))?;

    let write_half = stream
        .try_clone()
        .map_err(|e| ctx.make_error("Error", format!("WebSocket upgrade: socket clone failed: {e}")))?;
    let writer = Arc::new(Mutex::new(write_half));

    let id = {
        let reg = ws_registry(ctx);
        let id = reg.next;
        reg.next += 1;
        reg.socks.insert(
            id,
            WsEntry {
                writer: Some(Arc::clone(&writer)),
                close_sent: Arc::new(AtomicBool::new(false)),
                dead: false,
                dispatch,
                mask_seed: 0,
                masked: false,
            },
        );
        id
    };

    let reader = WsReader {
        stream: BufReader::new(stream),
        writer,
        mask_seed: 0,
        masked: false,
    };
    arm_read(ctx, id, reader);
    Ok(Value::Num(id as f64))
}

// ---- test support -------------------------------------------------------------------------------

/// A minimal RFC 6455 echo server for this workspace's tests and benchmarks (NOT part of the
/// runtime): accepts one connection at a time, upgrades, then echoes text/binary messages,
/// answers nothing to pongs, replies to close. `behavior` tweaks let tests exercise edges.
#[doc(hidden)]
#[allow(dead_code)]
pub mod testing {
    use super::*;
    use std::net::TcpListener;

    /// What the echo server should do beyond plain echoing.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Mode {
        /// Echo every message until the client closes.
        Echo,
        /// Send a ping (expecting the client's transparent pong), then echo.
        PingThenEcho,
        /// Send one fragmented text message ("frag" in 3 parts), then echo.
        FragmentedHello,
        /// Immediately close with (4001, "going away").
        CloseImmediately,
        /// Answer the upgrade with a WRONG Sec-WebSocket-Accept.
        BadAccept,
    }

    /// Spawn the server; returns its port. It serves `conns` connections then exits.
    pub fn spawn_echo(mode: Mode, conns: usize) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..conns {
                let Ok((stream, _)) = listener.accept() else { return };
                let _ = serve_one(stream, mode);
            }
        });
        port
    }

    fn serve_one(stream: TcpStream, mode: Mode) -> std::io::Result<()> {
        // Upgrade.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            (&stream).read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let key = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case("sec-websocket-key").then(|| v.trim().to_string())
            })
            .unwrap_or_default();
        let protocol = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("sec-websocket-protocol")
                .then(|| v.split(',').next().unwrap_or("").trim().to_string())
        });
        let accept = if mode == Mode::BadAccept {
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()
        } else {
            websocket_accept(&key)
        };
        let mut resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n"
        );
        if let Some(p) = protocol.filter(|p| !p.is_empty()) {
            resp.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
        }
        resp.push_str("\r\n");
        (&stream).write_all(resp.as_bytes())?;

        let unmasked = |op: u8, fin: bool, payload: &[u8]| {
            let mut f = vec![if fin { 0x80 | op } else { op }];
            if payload.len() < 126 {
                f.push(payload.len() as u8);
            } else {
                f.push(126);
                f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            f.extend_from_slice(payload);
            f
        };

        match mode {
            Mode::CloseImmediately => {
                let mut p = 4001u16.to_be_bytes().to_vec();
                p.extend_from_slice(b"going away");
                (&stream).write_all(&unmasked(0x8, true, &p))?;
            }
            Mode::PingThenEcho => {
                (&stream).write_all(&unmasked(0x9, true, b"marco"))?;
            }
            Mode::FragmentedHello => {
                (&stream).write_all(&unmasked(0x1, false, b"fr"))?;
                (&stream).write_all(&unmasked(0x0, false, b"agm"))?;
                (&stream).write_all(&unmasked(0x0, true, b"ent"))?;
            }
            _ => {}
        }

        // Echo loop over a buffered reader (frames from the client are masked).
        let mut r = BufReader::new(stream.try_clone()?);
        loop {
            let frame = match read_raw_frame(&mut r, MAX_MESSAGE) {
                Ok(f) => f,
                Err(_) => return Ok(()),
            };
            match frame.opcode {
                0x8 => {
                    (&stream).write_all(&unmasked(0x8, true, &frame.payload))?;
                    return Ok(());
                }
                0x9 => (&stream).write_all(&unmasked(0xA, true, &frame.payload))?,
                0xA => {
                    // A pong: the PingThenEcho handshake completed — tell the client via text.
                    (&stream).write_all(&unmasked(
                        0x1,
                        true,
                        format!("pong:{}", String::from_utf8_lossy(&frame.payload)).as_bytes(),
                    ))?;
                }
                0x1 | 0x2 if frame.fin => {
                    (&stream).write_all(&unmasked(frame.opcode, true, &frame.payload))?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        // Client-masked frames decode back to the payload (server view).
        for (op, payload) in [
            (0x1u8, b"hello".to_vec()),
            (0x2, vec![0u8, 1, 254, 255]),
            (0x1, vec![b'x'; 200]),      // 16-bit length form
            (0x1, vec![b'y'; 70_000]),   // 64-bit length form
        ] {
            let frame = encode_frame(op, &payload, [1, 2, 3, 4]);
            let raw = read_raw_frame(&mut &frame[..], MAX_MESSAGE).ok().unwrap();
            assert!(raw.fin);
            assert_eq!(raw.opcode, op);
            assert_eq!(raw.payload, payload);
        }
    }

    #[test]
    fn accept_value_matches_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn control_frame_rules() {
        // A fragmented (FIN=0) ping is a protocol error.
        let mut bad = encode_frame(0x9, b"p", [0, 0, 0, 0]);
        bad[0] &= 0x7f; // clear FIN
        assert!(matches!(
            read_raw_frame(&mut &bad[..], MAX_MESSAGE),
            Err(WsError::Protocol(1002, _))
        ));
        // Reserved bits fail (no extensions negotiated).
        let mut rsv = encode_frame(0x1, b"x", [0, 0, 0, 0]);
        rsv[0] |= 0x40;
        assert!(matches!(
            read_raw_frame(&mut &rsv[..], MAX_MESSAGE),
            Err(WsError::Protocol(1002, _))
        ));
    }
}
