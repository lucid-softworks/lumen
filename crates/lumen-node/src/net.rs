//! Real TCP (`node:net`) and UDP (`node:dgram`) sockets on `std::net`, exposed to JS as the
//! `__net` and `__udp` op namespaces (the JS glue in `js/net.js` / `js/dgram.js` wraps them into
//! the public Socket/Server/dgram.Socket surface).
//!
//! ## How it runs on the loop
//! The engine is single-threaded and `!Send`, so JS callbacks never leave the loop thread. Every
//! blocking socket call runs off-thread and comes back as a [`TaskCompletion`], settled through
//! the [`TaskRegistry`] exactly like `node:child_process` (see `child.rs`, the direct model):
//! - **reads / accept / recv** block for an unbounded time, so they run on *dedicated* threads
//!   ([`CompletionSender::run_blocking`]) — never a shared pool worker. The JS glue re-arms the
//!   next read/accept/recv after each completion, which is what keeps the loop alive while a
//!   socket or server is open (an idle server's pending `accept` is its keep-alive handle).
//! - **connect / write / send** also run on dedicated threads (they can block on a slow peer or a
//!   DNS lookup); the promise settles when they finish.
//!
//! Live handles live in [`NetRegistry`] / [`DgramRegistry`] in `OpState`, keyed by the id handed
//! to JS. A `TcpStream`/`UdpSocket` is shared as an `Arc` so one thread can read while another
//! writes (both `Read`/`Write` are implemented for `&TcpStream`); JS callbacks are never moved
//! into a worker closure.
//!
//! ## What's real vs. std-limited
//! Plain TCP and UDP are fully real (loopback + cross-runtime verified against the Node oracle):
//! connect/listen/accept/read/write/half-close/close, error codes (ECONNREFUSED, EADDRINUSE, …),
//! `setNoDelay`, addresses; UDP bind/send/recv-with-source, broadcast, TTL, and IPv4 multicast
//! membership/loopback/TTL. `setKeepAlive` uses `setsockopt(SO_KEEPALIVE)` on unix (std exposes no
//! keepalive). Genuinely std-impossible bits throw honestly from JS: `setMulticastInterface`
//! (needs `IP_MULTICAST_IF`) and IPv6 multicast TTL (no std setter). `backlog` and dgram
//! `reuseAddr` are accepted but inert (std binds without exposing either).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs,
    UdpSocket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_host::{ops, CallbackQueue, CompletionSender, Ctx, OpDecl, TaskId, TaskRegistry, Value};

/// A UDP recv poll interval: the recv thread wakes this often to notice `close()` and exit (std
/// gives no other way to interrupt a blocked `recv_from`).
const UDP_POLL: Duration = Duration::from_millis(200);

// ---- op tables --------------------------------------------------------------------------------

pub const NET_OPS: &[OpDecl] = ops![
    "connect" (4) => op_connect,
    "read" (3) => op_read,
    "write" (4) => op_write,
    "endWritable" (1) => op_end_writable,
    "close" (1) => op_close,
    "setNoDelay" (2) => op_set_no_delay,
    "setKeepAlive" (3) => op_set_keep_alive,
    "address" (1) => op_address,
    "socketRef" (2) => op_socket_ref,
    "listen" (3) => op_listen,
    "accept" (3) => op_accept,
    "closeServer" (1) => op_close_server,
    "serverAddress" (1) => op_server_address,
    "serverRef" (2) => op_server_ref,
];

pub const UDP_OPS: &[OpDecl] = ops![
    "bind" (4) => op_udp_bind,
    "recv" (3) => op_udp_recv,
    "send" (6) => op_udp_send,
    "close" (1) => op_udp_close,
    "address" (1) => op_udp_address,
    "setBroadcast" (2) => op_udp_set_broadcast,
    "setTTL" (2) => op_udp_set_ttl,
    "setMulticastTTL" (2) => op_udp_set_multicast_ttl,
    "setMulticastLoopback" (2) => op_udp_set_multicast_loop,
    "addMembership" (3) => op_udp_add_membership,
    "dropMembership" (3) => op_udp_drop_membership,
    "getBufferSize" (2) => op_udp_get_buffer_size,
    "setBufferSize" (3) => op_udp_set_buffer_size,
    "udpRef" (2) => op_udp_ref,
];

// ---- registries ---------------------------------------------------------------------------------

struct SockEntry {
    stream: Arc<TcpStream>,
    /// The socket was `unref`'d: its pending read must not by itself keep the loop alive.
    unref: bool,
    /// The in-flight read task, so `ref`/`unref` can retroactively toggle it.
    pending: Option<TaskId>,
}

struct ServerEntry {
    listener: Arc<TcpListener>,
    closed: Arc<AtomicBool>,
    local_addr: SocketAddr,
    unref: bool,
    pending: Option<TaskId>,
}

#[derive(Default)]
pub struct NetRegistry {
    next_socket: u64,
    sockets: HashMap<u64, SockEntry>,
    next_server: u64,
    servers: HashMap<u64, ServerEntry>,
}

struct UdpEntry {
    socket: Arc<UdpSocket>,
    closed: Arc<AtomicBool>,
    kind6: bool,
    unref: bool,
    pending: Option<TaskId>,
}

#[derive(Default)]
pub struct DgramRegistry {
    next: u64,
    sockets: HashMap<u64, UdpEntry>,
}

// ---- shared error/value helpers -----------------------------------------------------------------

/// A `Send` socket error carried back to the loop thread, with the errno `code` Node users switch
/// on and the syscall context Node attaches.
struct NetErr {
    code: &'static str,
    syscall: &'static str,
    message: String,
    address: Option<String>,
    port: Option<u16>,
}

fn io_code(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => "ECONNREFUSED",
        ConnectionReset => "ECONNRESET",
        ConnectionAborted => "ECONNABORTED",
        NotConnected => "ENOTCONN",
        AddrInUse => "EADDRINUSE",
        AddrNotAvailable => "EADDRNOTAVAIL",
        BrokenPipe => "EPIPE",
        TimedOut => "ETIMEDOUT",
        PermissionDenied => "EACCES",
        _ => match e.raw_os_error() {
            Some(32) => "EPIPE",
            Some(54) | Some(104) => "ECONNRESET",
            _ => "UNKNOWN",
        },
    }
}

fn net_err(syscall: &'static str, e: &std::io::Error, addr: Option<(String, u16)>) -> NetErr {
    let (address, port) = match addr {
        Some((a, p)) => (Some(a), Some(p)),
        None => (None, None),
    };
    NetErr {
        code: io_code(e),
        syscall,
        message: e.to_string(),
        address,
        port,
    }
}

/// Build the JS Error a rejected socket op throws: message shaped like Node's
/// (`connect ECONNREFUSED 127.0.0.1:80`) with `code`/`syscall`/`address`/`port`.
fn net_error_value(ctx: &mut Ctx, e: &NetErr) -> Value {
    let mut msg = format!("{} {}", e.syscall, e.code);
    if let (Some(a), Some(p)) = (&e.address, e.port) {
        msg.push(' ');
        msg.push_str(a);
        msg.push(':');
        msg.push_str(&p.to_string());
    } else if e.code == "UNKNOWN" {
        msg = format!("{}: {}", e.syscall, e.message);
    }
    let err = ctx.make_error("Error", msg);
    let _ = ctx.set_member(&err, "code", Value::str(e.code));
    let _ = ctx.set_member(&err, "syscall", Value::str(e.syscall));
    if let Some(a) = &e.address {
        let _ = ctx.set_member(&err, "address", Value::from_string(a.clone()));
    }
    if let Some(p) = e.port {
        let _ = ctx.set_member(&err, "port", Value::Num(p as f64));
    }
    err
}

fn family_of(addr: &SocketAddr) -> &'static str {
    if addr.is_ipv6() {
        "IPv6"
    } else {
        "IPv4"
    }
}

/// `{ address, family, port }` — the shape of Node's `socket.address()` / `server.address()`.
fn addr_object(ctx: &mut Ctx, addr: &SocketAddr) -> Value {
    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "address", Value::from_string(addr.ip().to_string()));
    let _ = ctx.set_member(&o, "family", Value::str(family_of(addr)));
    let _ = ctx.set_member(&o, "port", Value::Num(addr.port() as f64));
    o
}

fn completions(ctx: &mut Ctx) -> CompletionSender {
    ctx.op_state()
        .get::<CompletionSender>()
        .expect("runtime installs the completion sender")
        .clone()
}

fn take_resolve_reject(
    ctx: &mut Ctx,
    res: Option<&Value>,
    rej: Option<&Value>,
) -> Result<(Value, Value), Value> {
    match (res, rej) {
        (Some(r), Some(j)) if r.is_callable() && j.is_callable() => Ok((r.clone(), j.clone())),
        _ => Err(ctx.make_error("TypeError", "net op expects (resolve, reject)")),
    }
}

fn enqueue(ctx: &mut Ctx, cb: Value, args: Vec<Value>) {
    CallbackQueue::enqueue(ctx.op_state(), cb, args);
}

fn arg_u64(args: &[Value], i: usize) -> u64 {
    args.get(i).and_then(Value::as_num_opt).unwrap_or(0.0) as u64
}

// ---- TCP: register a connected stream (accept/connect share this) ------------------------------

/// Insert a freshly connected `TcpStream` into the registry and produce the JS descriptor
/// `[socketId, localAddress, localPort, remoteAddress, remotePort, family]`.
fn register_stream(ctx: &mut Ctx, stream: TcpStream) -> Result<Vec<Value>, Value> {
    let local = stream
        .local_addr()
        .map_err(|e| ctx.make_error("Error", format!("local_addr: {e}")))?;
    let peer = stream
        .peer_addr()
        .map_err(|e| ctx.make_error("Error", format!("peer_addr: {e}")))?;
    let reg = ctx.host_mut::<NetRegistry>().expect("net registry installed");
    let id = reg.next_socket;
    reg.next_socket += 1;
    reg.sockets.insert(
        id,
        SockEntry {
            stream: Arc::new(stream),
            unref: false,
            pending: None,
        },
    );
    Ok(vec![
        Value::Num(id as f64),
        Value::from_string(local.ip().to_string()),
        Value::Num(local.port() as f64),
        Value::from_string(peer.ip().to_string()),
        Value::Num(peer.port() as f64),
        Value::str(family_of(&peer)),
    ])
}

// ---- TCP client ops -----------------------------------------------------------------------------

/// `(host, port, resolve, reject)` — resolve the host, connect, and settle with the socket
/// descriptor (see [`register_stream`]) or reject with an errno-tagged error.
fn op_connect(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let host = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let port = arg_u64(args, 1) as u16;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(2), args.get(3))?;

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("registry")
        .register(resolve, Some(reject), decode_connect);
    completions(ctx).run_blocking(id, move || {
        let result: Result<TcpStream, NetErr> = (|| {
            let addrs: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
                Ok(it) => it.collect(),
                Err(e) => {
                    return Err(NetErr {
                        code: "ENOTFOUND",
                        syscall: "getaddrinfo",
                        message: e.to_string(),
                        address: Some(host.clone()),
                        port: Some(port),
                    })
                }
            };
            let mut last = None;
            for addr in addrs {
                match TcpStream::connect(addr) {
                    Ok(s) => return Ok(s),
                    Err(e) => last = Some(e),
                }
            }
            Err(net_err(
                "connect",
                &last.unwrap_or_else(|| std::io::Error::other("no address")),
                Some((host.clone(), port)),
            ))
        })();
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_connect(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<TcpStream, NetErr>>().expect("connect payload") {
        Ok(stream) => register_stream(ctx, stream),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

/// `(socketId, resolve, reject)` — read one chunk; resolves with a Uint8Array, or `null` at EOF.
fn op_read(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let (resolve, reject) = take_resolve_reject(ctx, args.get(1), args.get(2))?;

    let found = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| (e.stream.clone(), e.unref));
    let (stream, unref) = match found {
        Some(v) => v,
        None => {
            enqueue(ctx, resolve, vec![Value::Null]); // gone → treat as EOF
            return Ok(Value::Undefined);
        }
    };

    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
    let id = reg.register(resolve, Some(reject), decode_read);
    if unref {
        reg.set_unref(id);
    }
    if let Some(e) = ctx.host_mut::<NetRegistry>().and_then(|r| r.sockets.get_mut(&sid)) {
        e.pending = Some(id);
    }
    completions(ctx).run_blocking(id, move || {
        let mut buf = vec![0u8; 65536];
        let mut s: &TcpStream = &stream;
        let result: Result<Vec<u8>, NetErr> = match s.read(&mut buf) {
            Ok(0) => Ok(Vec::new()),
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(e) => Err(net_err("read", &e, None)),
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<Vec<u8>, NetErr>>().expect("read payload") {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]),
        Ok(bytes) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

/// `(socketId, bytes, resolve, reject)` — write all bytes; resolves when flushed.
fn op_write(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let data = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "net write expects bytes"))?;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(2), args.get(3))?;

    let stream = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.stream.clone());
    let Some(stream) = stream else {
        let err = net_error_value(
            ctx,
            &NetErr {
                code: "EPIPE",
                syscall: "write",
                message: "This socket has been ended by the other party".to_string(),
                address: None,
                port: None,
            },
        );
        enqueue(ctx, reject, vec![err]);
        return Ok(Value::Undefined);
    };

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("registry")
        .register(resolve, Some(reject), decode_write);
    completions(ctx).run_blocking(id, move || {
        let mut s: &TcpStream = &stream;
        let result: Result<(), NetErr> = s
            .write_all(&data)
            .and_then(|()| s.flush())
            .map_err(|e| net_err("write", &e, None));
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_write(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<(), NetErr>>().expect("write payload") {
        Ok(()) => Ok(vec![]),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

/// `(socketId)` — half-close: shut the write half so the peer sees EOF; our read half stays open.
fn op_end_writable(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    if let Some(e) = ctx.host_mut::<NetRegistry>().and_then(|r| r.sockets.get(&sid)) {
        let _ = e.stream.shutdown(Shutdown::Write);
    }
    Ok(Value::Undefined)
}

/// `(socketId)` — full close: shut both halves (waking a blocked read) and drop the handle.
fn op_close(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    if let Some(reg) = ctx.host_mut::<NetRegistry>() {
        if let Some(e) = reg.sockets.remove(&sid) {
            let _ = e.stream.shutdown(Shutdown::Both);
        }
    }
    Ok(Value::Undefined)
}

/// `(socketId, on)` — `socket.setNoDelay`. Returns whether it took effect.
fn op_set_no_delay(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let on = !matches!(args.get(1), Some(Value::Bool(false)));
    let ok = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.stream.set_nodelay(on).is_ok())
        .unwrap_or(false);
    Ok(Value::Bool(ok))
}

/// `(socketId, on, initialDelayMs)` — `socket.setKeepAlive`, via `setsockopt(SO_KEEPALIVE)` on
/// unix (std exposes no keepalive). A no-op returning `false` elsewhere.
fn op_set_keep_alive(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let on = matches!(args.get(1), Some(Value::Bool(true)));
    let delay_ms = args.get(2).and_then(Value::as_num_opt).unwrap_or(0.0);
    let stream = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.stream.clone());
    let Some(stream) = stream else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(set_keep_alive(&stream, on, (delay_ms / 1000.0) as i32)))
}

/// `(socketId)` — `socket.address()`; `null` if the socket is gone.
fn op_address(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let addr = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .and_then(|e| e.stream.local_addr().ok());
    Ok(match addr {
        Some(a) => addr_object(ctx, &a),
        None => Value::Null,
    })
}

/// `(socketId, unref)` — `socket.ref()`/`unref()`; toggles whether the pending read keeps the
/// loop alive.
fn op_socket_ref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let unref = matches!(args.get(1), Some(Value::Bool(true)));
    let pending = ctx.host_mut::<NetRegistry>().and_then(|r| {
        r.sockets.get_mut(&sid).map(|e| {
            e.unref = unref;
            e.pending
        })
    });
    if let (Some(Some(id)), Some(reg)) = (pending, ctx.host_mut::<TaskRegistry>()) {
        if unref {
            reg.set_unref(id);
        } else {
            reg.set_ref(id);
        }
    }
    Ok(Value::Undefined)
}

// ---- TCP server ops -----------------------------------------------------------------------------

/// `(host, port, backlog)` — bind a listener (synchronous, like `std`); returns
/// `{ serverId, address, port, family }` or throws an errno-tagged error (`EADDRINUSE`, …). The
/// `backlog` is accepted but inert (std uses its default and exposes no setter).
fn op_listen(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let mut host = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    if host.is_empty() {
        host = "0.0.0.0".to_string();
    }
    let port = arg_u64(args, 1) as u16;

    let listener = match TcpListener::bind((host.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            let err = net_err("listen", &e, Some((host, port)));
            return Err(net_error_value(ctx, &err));
        }
    };
    let local_addr = listener
        .local_addr()
        .map_err(|e| ctx.make_error("Error", format!("local_addr: {e}")))?;

    let reg = ctx.host_mut::<NetRegistry>().expect("net registry installed");
    let id = reg.next_server;
    reg.next_server += 1;
    reg.servers.insert(
        id,
        ServerEntry {
            listener: Arc::new(listener),
            closed: Arc::new(AtomicBool::new(false)),
            local_addr,
            unref: false,
            pending: None,
        },
    );

    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "serverId", Value::Num(id as f64));
    let _ = ctx.set_member(&o, "address", Value::from_string(local_addr.ip().to_string()));
    let _ = ctx.set_member(&o, "port", Value::Num(local_addr.port() as f64));
    let _ = ctx.set_member(&o, "family", Value::str(family_of(&local_addr)));
    Ok(o)
}

/// `(serverId, resolve, reject)` — accept one connection; resolves with a socket descriptor (see
/// [`register_stream`]) or `null` once the server is closed.
fn op_accept(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let (resolve, reject) = take_resolve_reject(ctx, args.get(1), args.get(2))?;

    let found = ctx.host_mut::<NetRegistry>().and_then(|r| {
        r.servers
            .get(&sid)
            .map(|e| (e.listener.clone(), e.closed.clone(), e.unref))
    });
    let (listener, closed, unref) = match found {
        Some(v) => v,
        None => {
            enqueue(ctx, resolve, vec![Value::Null]);
            return Ok(Value::Undefined);
        }
    };

    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
    let id = reg.register(resolve, Some(reject), decode_accept);
    if unref {
        reg.set_unref(id);
    }
    if let Some(e) = ctx.host_mut::<NetRegistry>().and_then(|r| r.servers.get_mut(&sid)) {
        e.pending = Some(id);
    }
    completions(ctx).run_blocking(id, move || {
        let result: AcceptResult = match listener.accept() {
            Ok((stream, _peer)) => {
                if closed.load(Ordering::SeqCst) {
                    AcceptResult::Closed // woken by closeServer()'s throwaway connect
                } else {
                    AcceptResult::Conn(stream)
                }
            }
            Err(_) => AcceptResult::Closed,
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

enum AcceptResult {
    Conn(TcpStream),
    Closed,
}

fn decode_accept(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<AcceptResult>().expect("accept payload") {
        AcceptResult::Conn(stream) => register_stream(ctx, stream),
        AcceptResult::Closed => Ok(vec![Value::Null]),
    }
}

/// `(serverId)` — stop the listener: flag it closed and poke it with a throwaway connection so the
/// blocked `accept()` wakes and reports closed.
fn op_close_server(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let target = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.servers.remove(&sid))
        .map(|e| (e.closed, e.local_addr));
    if let Some((closed, local_addr)) = target {
        closed.store(true, Ordering::SeqCst);
        let wake = if local_addr.ip().is_unspecified() {
            let ip = if local_addr.is_ipv6() {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            };
            SocketAddr::new(ip, local_addr.port())
        } else {
            local_addr
        };
        let _ = TcpStream::connect(wake);
    }
    Ok(Value::Undefined)
}

/// `(serverId)` — `server.address()`; `null` if the server is gone.
fn op_server_address(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let addr = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.servers.get(&sid))
        .map(|e| e.local_addr);
    Ok(match addr {
        Some(a) => addr_object(ctx, &a),
        None => Value::Null,
    })
}

/// `(serverId, unref)` — `server.ref()`/`unref()`.
fn op_server_ref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let unref = matches!(args.get(1), Some(Value::Bool(true)));
    let pending = ctx.host_mut::<NetRegistry>().and_then(|r| {
        r.servers.get_mut(&sid).map(|e| {
            e.unref = unref;
            e.pending
        })
    });
    if let (Some(Some(id)), Some(reg)) = (pending, ctx.host_mut::<TaskRegistry>()) {
        if unref {
            reg.set_unref(id);
        } else {
            reg.set_ref(id);
        }
    }
    Ok(Value::Undefined)
}

// ---- UDP ops ------------------------------------------------------------------------------------

/// `(type, host, port, resolve, reject)` — bind a UDP socket (`type` = "udp4"|"udp6");
/// resolves with `{ socketId, address, port, family }` or rejects with an errno-tagged error.
fn op_udp_bind(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let kind = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let kind6 = kind == "udp6";
    let host = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let host = if !host.is_empty() {
        host
    } else if kind6 {
        "::".to_string()
    } else {
        "0.0.0.0".to_string()
    };
    let port = arg_u64(args, 2) as u16;

    let socket = match UdpSocket::bind((host.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            let err = net_err("bind", &e, Some((host, port)));
            return Err(net_error_value(ctx, &err));
        }
    };
    // Bounded read timeout so the recv thread can notice close() and exit.
    let _ = socket.set_read_timeout(Some(UDP_POLL));
    let local = socket
        .local_addr()
        .map_err(|e| ctx.make_error("Error", format!("local_addr: {e}")))?;

    let reg = ctx.host_mut::<DgramRegistry>().expect("dgram registry installed");
    let id = reg.next;
    reg.next += 1;
    reg.sockets.insert(
        id,
        UdpEntry {
            socket: Arc::new(socket),
            closed: Arc::new(AtomicBool::new(false)),
            kind6,
            unref: false,
            pending: None,
        },
    );

    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "socketId", Value::Num(id as f64));
    let _ = ctx.set_member(&o, "address", Value::from_string(local.ip().to_string()));
    let _ = ctx.set_member(&o, "port", Value::Num(local.port() as f64));
    let _ = ctx.set_member(&o, "family", Value::str(family_of(&local)));
    Ok(o)
}

/// `(socketId, resolve, reject)` — receive one datagram; resolves with
/// `{ data, address, port, family, size }` or `null` once the socket is closed.
fn op_udp_recv(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let (resolve, reject) = take_resolve_reject(ctx, args.get(1), args.get(2))?;

    let found = ctx.host_mut::<DgramRegistry>().and_then(|r| {
        r.sockets
            .get(&sid)
            .map(|e| (e.socket.clone(), e.closed.clone(), e.unref))
    });
    let (socket, closed, unref) = match found {
        Some(v) => v,
        None => {
            enqueue(ctx, resolve, vec![Value::Null]);
            return Ok(Value::Undefined);
        }
    };

    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
    let id = reg.register(resolve, Some(reject), decode_recv);
    if unref {
        reg.set_unref(id);
    }
    if let Some(e) = ctx.host_mut::<DgramRegistry>().and_then(|r| r.sockets.get_mut(&sid)) {
        e.pending = Some(id);
    }
    completions(ctx).run_blocking(id, move || {
        let mut buf = vec![0u8; 65536];
        let result: RecvResult = loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if closed.load(Ordering::SeqCst) {
                        break RecvResult::Closed;
                    }
                    buf.truncate(n);
                    break RecvResult::Msg(buf, from);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if closed.load(Ordering::SeqCst) {
                        break RecvResult::Closed;
                    }
                }
                Err(e) => break RecvResult::Err(net_err("recv", &e, None)),
            }
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

enum RecvResult {
    Msg(Vec<u8>, SocketAddr),
    Closed,
    Err(NetErr),
}

fn decode_recv(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<RecvResult>().expect("recv payload") {
        RecvResult::Closed => Ok(vec![Value::Null]),
        RecvResult::Err(e) => Err(net_error_value(ctx, &e)),
        RecvResult::Msg(bytes, from) => {
            let size = bytes.len();
            let data = ctx.make_uint8array(&bytes)?;
            let o = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&o, "data", data);
            let _ = ctx.set_member(&o, "address", Value::from_string(from.ip().to_string()));
            let _ = ctx.set_member(&o, "port", Value::Num(from.port() as f64));
            let _ = ctx.set_member(&o, "family", Value::str(family_of(&from)));
            let _ = ctx.set_member(&o, "size", Value::Num(size as f64));
            Ok(vec![o])
        }
    }
}

/// `(socketId, bytes, port, address, resolve, reject)` — send to `address:port`; resolves with the
/// byte count. JS pre-slices the buffer, so `bytes` is already the exact payload.
fn op_udp_send(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let data = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "dgram send expects bytes"))?;
    let port = arg_u64(args, 2) as u16;
    let address = ctx
        .coerce_string(args.get(3).unwrap_or(&Value::Undefined))?
        .to_string();
    let (resolve, reject) = take_resolve_reject(ctx, args.get(4), args.get(5))?;

    let socket = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.socket.clone());
    let Some(socket) = socket else {
        let err = net_error_value(
            ctx,
            &NetErr {
                code: "ERR_SOCKET_DGRAM_NOT_RUNNING",
                syscall: "send",
                message: "Not running".to_string(),
                address: None,
                port: None,
            },
        );
        enqueue(ctx, reject, vec![err]);
        return Ok(Value::Undefined);
    };

    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("registry")
        .register(resolve, Some(reject), decode_send);
    completions(ctx).run_blocking(id, move || {
        let result: Result<usize, NetErr> = (address.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .ok_or_else(|| NetErr {
                code: "ENOTFOUND",
                syscall: "getaddrinfo",
                message: format!("getaddrinfo ENOTFOUND {address}"),
                address: Some(address.clone()),
                port: Some(port),
            })
            .and_then(|addr| {
                socket
                    .send_to(&data, addr)
                    .map_err(|e| net_err("send", &e, Some((address.clone(), port))))
            });
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_send(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<usize, NetErr>>().expect("send payload") {
        Ok(n) => Ok(vec![Value::Num(n as f64)]),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

/// `(socketId)` — close: flag it so the recv thread exits at its next poll, and drop the handle.
fn op_udp_close(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    if let Some(reg) = ctx.host_mut::<DgramRegistry>() {
        if let Some(e) = reg.sockets.remove(&sid) {
            e.closed.store(true, Ordering::SeqCst);
        }
    }
    Ok(Value::Undefined)
}

fn op_udp_address(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let addr = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .and_then(|e| e.socket.local_addr().ok());
    Ok(match addr {
        Some(a) => addr_object(ctx, &a),
        None => Value::Null,
    })
}

/// Look a UDP socket up and run `f` on it, mapping an error to an errno-tagged JS throw.
fn with_udp(
    ctx: &mut Ctx,
    sid: u64,
    syscall: &'static str,
    f: impl FnOnce(&Arc<UdpSocket>, bool) -> std::io::Result<()>,
) -> Result<Value, Value> {
    let found = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| (e.socket.clone(), e.kind6));
    let Some((socket, kind6)) = found else {
        return Err(ctx.make_error("Error", "dgram: unknown socket"));
    };
    match f(&socket, kind6) {
        Ok(()) => Ok(Value::Undefined),
        Err(e) => {
            let err = net_err(syscall, &e, None);
            Err(net_error_value(ctx, &err))
        }
    }
}

fn op_udp_set_broadcast(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let on = matches!(args.get(1), Some(Value::Bool(true)));
    with_udp(ctx, sid, "setBroadcast", |s, _| s.set_broadcast(on))
}

fn op_udp_set_ttl(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let ttl = args.get(1).and_then(Value::as_num_opt).unwrap_or(1.0) as u32;
    // std's set_ttl sets IP_TTL, which is invalid on an IPv6 socket; Node sets
    // IPV6_UNICAST_HOPS there, so do the same via setsockopt.
    with_udp(ctx, sid, "setTTL", |s, kind6| {
        if kind6 {
            set_ipv6_unicast_hops(s, ttl as i32)
        } else {
            s.set_ttl(ttl)
        }
    })
}

/// IPv4 multicast TTL is real; IPv6 has no std setter, so `udp6` throws honestly.
fn op_udp_set_multicast_ttl(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let ttl = args.get(1).and_then(Value::as_num_opt).unwrap_or(1.0) as u32;
    let kind6 = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.kind6)
        .unwrap_or(false);
    if kind6 {
        return Err(ctx.make_error(
            "Error",
            "dgram.setMulticastTTL is not supported for udp6 in lumen (std exposes no IPv6 multicast-hops setter)",
        ));
    }
    with_udp(ctx, sid, "setMulticastTTL", |s, _| s.set_multicast_ttl_v4(ttl))
}

fn op_udp_set_multicast_loop(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let on = matches!(args.get(1), Some(Value::Bool(true)));
    with_udp(ctx, sid, "setMulticastLoopback", |s, kind6| {
        if kind6 {
            s.set_multicast_loop_v6(on)
        } else {
            s.set_multicast_loop_v4(on)
        }
    })
}

fn parse_v4(s: &str) -> Result<Ipv4Addr, ()> {
    s.parse::<Ipv4Addr>().map_err(|_| ())
}
fn parse_v6(s: &str) -> Result<Ipv6Addr, ()> {
    s.parse::<Ipv6Addr>().map_err(|_| ())
}

/// `(socketId, multicastAddress, interface)` — join a multicast group. For udp4 `interface` is an
/// IPv4 address (default `0.0.0.0`); for udp6 it is an interface index (default 0).
fn op_udp_add_membership(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    udp_membership(ctx, args, true)
}
fn op_udp_drop_membership(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    udp_membership(ctx, args, false)
}

fn udp_membership(ctx: &mut Ctx, args: &[Value], join: bool) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let mcast = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let iface = match args.get(2) {
        Some(Value::Undefined) | Some(Value::Null) | None => String::new(),
        Some(v) => ctx.coerce_string(v)?.to_string(),
    };
    let kind6 = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.kind6)
        .unwrap_or(false);
    let syscall = if join { "addMembership" } else { "dropMembership" };

    if kind6 {
        let group = parse_v6(&mcast)
            .map_err(|_| ctx.make_error("TypeError", format!("Invalid multicast address: {mcast}")))?;
        let idx = iface.parse::<u32>().unwrap_or(0);
        return with_udp(ctx, sid, syscall, |s, _| {
            if join {
                s.join_multicast_v6(&group, idx)
            } else {
                s.leave_multicast_v6(&group, idx)
            }
        });
    }
    let group = parse_v4(&mcast)
        .map_err(|_| ctx.make_error("TypeError", format!("Invalid multicast address: {mcast}")))?;
    let iface_addr = if iface.is_empty() {
        Ipv4Addr::UNSPECIFIED
    } else {
        parse_v4(&iface)
            .map_err(|_| ctx.make_error("TypeError", format!("Invalid interface address: {iface}")))?
    };
    with_udp(ctx, sid, syscall, |s, _| {
        if join {
            s.join_multicast_v4(&group, &iface_addr)
        } else {
            s.leave_multicast_v4(&group, &iface_addr)
        }
    })
}

/// `(socketId, unref)` — `dgram.ref()`/`unref()`.
fn op_udp_ref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let unref = matches!(args.get(1), Some(Value::Bool(true)));
    let pending = ctx.host_mut::<DgramRegistry>().and_then(|r| {
        r.sockets.get_mut(&sid).map(|e| {
            e.unref = unref;
            e.pending
        })
    });
    if let (Some(Some(id)), Some(reg)) = (pending, ctx.host_mut::<TaskRegistry>()) {
        if unref {
            reg.set_unref(id);
        } else {
            reg.set_ref(id);
        }
    }
    Ok(Value::Undefined)
}

fn op_udp_get_buffer_size(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let receive = matches!(args.get(1), Some(Value::Bool(true)));
    let socket = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|registry| registry.sockets.get(&sid))
        .map(|entry| entry.socket.clone())
        .ok_or_else(|| ctx.make_error("Error", "dgram: unknown socket"))?;
    socket_buffer_size(&socket, receive)
        .map(|size| Value::Num(size as f64))
        .map_err(|error| net_error_value(ctx, &net_err("getsockopt", &error, None)))
}

fn op_udp_set_buffer_size(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let sid = arg_u64(args, 0);
    let receive = matches!(args.get(1), Some(Value::Bool(true)));
    let size = arg_u64(args, 2).min(i32::MAX as u64) as i32;
    let socket = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|registry| registry.sockets.get(&sid))
        .map(|entry| entry.socket.clone())
        .ok_or_else(|| ctx.make_error("Error", "dgram: unknown socket"))?;
    set_socket_buffer_size(&socket, receive, size)
        .map(|()| Value::Undefined)
        .map_err(|error| net_error_value(ctx, &net_err("setsockopt", &error, None)))
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn socket_buffer_size(socket: &UdpSocket, receive: bool) -> std::io::Result<i32> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_RCVBUF: i32 = 8;
    #[cfg(target_os = "linux")]
    const SO_SNDBUF: i32 = 7;
    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_os = "macos")]
    const SO_RCVBUF: i32 = 0x1002;
    #[cfg(target_os = "macos")]
    const SO_SNDBUF: i32 = 0x1001;

    extern "C" {
        fn getsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *mut std::os::raw::c_void,
            length: *mut u32,
        ) -> i32;
    }
    let mut value = 0i32;
    let mut length = std::mem::size_of::<i32>() as u32;
    // SAFETY: the socket fd is live and both output pointers reference initialized writable values.
    let result = unsafe {
        getsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            if receive { SO_RCVBUF } else { SO_SNDBUF },
            &mut value as *mut i32 as *mut _,
            &mut length,
        )
    };
    if result == 0 { Ok(value) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn set_socket_buffer_size(socket: &UdpSocket, receive: bool, size: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_RCVBUF: i32 = 8;
    #[cfg(target_os = "linux")]
    const SO_SNDBUF: i32 = 7;
    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_os = "macos")]
    const SO_RCVBUF: i32 = 0x1002;
    #[cfg(target_os = "macos")]
    const SO_SNDBUF: i32 = 0x1001;

    extern "C" {
        fn setsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *const std::os::raw::c_void,
            length: u32,
        ) -> i32;
    }
    // SAFETY: the socket fd is live and the input pointer references a valid i32.
    let result = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            if receive { SO_RCVBUF } else { SO_SNDBUF },
            &size as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(all(unix, any(target_os = "macos", target_os = "linux"))))]
fn socket_buffer_size(_socket: &UdpSocket, _receive: bool) -> std::io::Result<i32> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "socket buffer sizes are unavailable"))
}

#[cfg(not(all(unix, any(target_os = "macos", target_os = "linux"))))]
fn set_socket_buffer_size(_socket: &UdpSocket, _receive: bool, _size: i32) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "socket buffer sizes are unavailable"))
}

// ---- IPV6_UNICAST_HOPS via setsockopt (std exposes no IPv6 hop-limit setter) --------------------

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn set_ipv6_unicast_hops(socket: &UdpSocket, hops: i32) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    const IPPROTO_IPV6: i32 = 41;
    #[cfg(target_os = "macos")]
    const IPV6_UNICAST_HOPS: i32 = 4;
    #[cfg(target_os = "linux")]
    const IPV6_UNICAST_HOPS: i32 = 16;

    extern "C" {
        fn setsockopt(
            sockfd: i32,
            level: i32,
            optname: i32,
            optval: *const std::os::raw::c_void,
            optlen: u32,
        ) -> i32;
    }

    // SAFETY: the fd is a live socket owned by `socket`; optval points to a valid i32.
    let rc = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            IPV6_UNICAST_HOPS,
            &hops as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_ipv6_unicast_hops(_socket: &UdpSocket, _hops: i32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "setTTL for udp6 is not supported in lumen on this platform (no IPV6_UNICAST_HOPS access)",
    ))
}

// ---- SO_KEEPALIVE via setsockopt (std exposes no keepalive) -------------------------------------

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn set_keep_alive(stream: &TcpStream, on: bool, idle_secs: i32) -> bool {
    use std::os::unix::io::AsRawFd;

    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_KEEPALIVE: i32 = 9;
    #[cfg(target_os = "linux")]
    const TCP_KEEPIDLE: i32 = 4;
    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_os = "macos")]
    const SO_KEEPALIVE: i32 = 0x0008;
    #[cfg(target_os = "macos")]
    const TCP_KEEPALIVE: i32 = 0x10;
    const IPPROTO_TCP: i32 = 6;

    extern "C" {
        fn setsockopt(
            sockfd: i32,
            level: i32,
            optname: i32,
            optval: *const std::os::raw::c_void,
            optlen: u32,
        ) -> i32;
    }

    let fd = stream.as_raw_fd();
    let flag: i32 = if on { 1 } else { 0 };
    // SAFETY: fd is a live socket owned by `stream`; optval points to a valid i32 of optlen bytes.
    let rc = unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_KEEPALIVE,
            &flag as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if rc != 0 {
        return false;
    }
    if on && idle_secs > 0 {
        #[cfg(target_os = "linux")]
        let idle_opt = TCP_KEEPIDLE;
        #[cfg(target_os = "macos")]
        let idle_opt = TCP_KEEPALIVE;
        // SAFETY: as above; failure to tune the idle interval is non-fatal (keepalive is still on).
        unsafe {
            setsockopt(
                fd,
                IPPROTO_TCP,
                idle_opt,
                &idle_secs as *const i32 as *const _,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
    true
}

#[cfg(not(all(unix, any(target_os = "macos", target_os = "linux"))))]
fn set_keep_alive(_stream: &TcpStream, _on: bool, _idle_secs: i32) -> bool {
    false
}
