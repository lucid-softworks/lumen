use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lumen_host::{ops, CallbackQueue, CompletionSender, Ctx, OpDecl, TaskRegistry, Value};

pub const TLS_OPS: &[OpDecl] = ops![
    "connect" (7) => op_connect,
    "read" (3) => op_read,
    "write" (4) => op_write,
    "close" (1) => op_close,
    "listen" (5) => op_listen,
    "accept" (3) => op_accept,
    "closeServer" (1) => op_close_server,
];

struct Entry {
    stream: Arc<Mutex<lumen_tls::TlsStream>>,
    closed: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct TlsRegistry {
    next: u64,
    sockets: std::collections::HashMap<u64, Entry>,
    next_listener: u64,
    listeners: std::collections::HashMap<u64, ListenerEntry>,
}

struct ListenerEntry {
    listener: Arc<TcpListener>,
    closed: Arc<AtomicBool>,
    local: SocketAddr,
    certificate: Arc<Vec<u8>>,
    private_key: Arc<Vec<u8>>,
    alpn: Arc<Vec<String>>,
}

struct Connected {
    stream: lumen_tls::TlsStream,
    local: SocketAddr,
    peer: SocketAddr,
    protocol: String,
    cipher: String,
    alpn: String,
}

fn callbacks(ctx: &mut Ctx, resolve: Option<&Value>, reject: Option<&Value>) -> Result<(Value, Value), Value> {
    match (resolve, reject) {
        (Some(resolve), Some(reject)) if resolve.is_callable() && reject.is_callable() => Ok((resolve.clone(), reject.clone())),
        _ => Err(ctx.make_error("TypeError", "TLS op expects resolve and reject callbacks")),
    }
}

fn completions(ctx: &mut Ctx) -> CompletionSender {
    ctx.op_state().get::<CompletionSender>().expect("runtime installs completion sender").clone()
}

fn op_connect(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let host = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let port = args.get(1).and_then(Value::as_num_opt).unwrap_or(443.0) as u16;
    let servername = ctx.coerce_string(args.get(2).unwrap_or(&Value::Undefined))?.to_string();
    let alpn: Vec<String> = ctx.coerce_string(args.get(3).unwrap_or(&Value::Undefined))?.to_string()
        .split(',').filter(|value| !value.is_empty()).map(str::to_string).collect();
    let verify_peer = !matches!(args.get(4), Some(Value::Bool(false)));
    let (resolve, reject) = callbacks(ctx, args.get(5), args.get(6))?;
    let task = ctx.host_mut::<TaskRegistry>().expect("task registry").register(resolve, Some(reject), decode_connect);
    completions(ctx).run_blocking(task, move || {
        let result = (|| -> Result<Connected, String> {
            let addresses: Vec<_> = (host.as_str(), port).to_socket_addrs().map_err(|error| format!("getaddrinfo {host}: {error}"))?.collect();
            let mut last_error = None;
            for address in addresses {
                match TcpStream::connect_timeout(&address, Duration::from_secs(30)) {
                    Ok(tcp) => {
                        tcp.set_read_timeout(Some(Duration::from_millis(100))).ok();
                        tcp.set_write_timeout(Some(Duration::from_secs(30))).ok();
                        let local = tcp.local_addr().map_err(|error| error.to_string())?;
                        let peer = tcp.peer_addr().map_err(|error| error.to_string())?;
                        let stream = lumen_tls::TlsStream::connect_with_options(tcp, &servername, &alpn, verify_peer)?;
                        let protocol = stream.protocol();
                        let cipher = stream.cipher();
                        let alpn = stream.alpn_protocol();
                        return Ok(Connected { stream, local, peer, protocol, cipher, alpn });
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            Err(format!("connect {host}:{port}: {}", last_error.map_or_else(|| "no address".into(), |error| error.to_string())))
        })();
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_connect(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let connected = *payload.downcast::<Result<Connected, String>>().expect("TLS connect payload");
    register_connected(ctx, connected.map_err(|message| ctx.make_error("Error", message))?)
}

fn register_connected(ctx: &mut Ctx, connected: Connected) -> Result<Vec<Value>, Value> {
    let Connected { stream, local, peer, protocol, cipher, alpn } = connected;
    let registry = ctx.host_mut::<TlsRegistry>().expect("TLS registry");
    let id = registry.next;
    registry.next += 1;
    registry.sockets.insert(id, Entry { stream: Arc::new(Mutex::new(stream)), closed: Arc::new(AtomicBool::new(false)) });
    Ok(vec![
        Value::Num(id as f64), Value::from_string(local.ip().to_string()), Value::Num(local.port() as f64),
        Value::from_string(peer.ip().to_string()), Value::Num(peer.port() as f64),
        Value::from_string(protocol), Value::from_string(cipher), Value::from_string(alpn),
    ])
}

fn op_listen(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let host = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let port = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u16;
    let certificate = ctx.typed_array_bytes(args.get(2).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "TLS server certificate must be bytes"))?;
    let private_key = ctx.typed_array_bytes(args.get(3).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "TLS server private key must be bytes"))?;
    let alpn: Vec<String> = ctx.coerce_string(args.get(4).unwrap_or(&Value::Undefined))?.to_string()
        .split(',').filter(|value| !value.is_empty()).map(str::to_string).collect();
    let listener = TcpListener::bind((if host.is_empty() { "0.0.0.0" } else { &host }, port))
        .map_err(|error| ctx.make_error("Error", format!("TLS listen: {error}")))?;
    let local = listener.local_addr().map_err(|error| ctx.make_error("Error", error.to_string()))?;
    let registry = ctx.host_mut::<TlsRegistry>().expect("TLS registry");
    let id = registry.next_listener;
    registry.next_listener += 1;
    registry.listeners.insert(id, ListenerEntry {
        listener: Arc::new(listener), closed: Arc::new(AtomicBool::new(false)), local,
        certificate: Arc::new(certificate), private_key: Arc::new(private_key),
        alpn: Arc::new(alpn),
    });
    let result = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&result, "serverId", Value::Num(id as f64));
    let _ = ctx.set_member(&result, "address", Value::from_string(local.ip().to_string()));
    let _ = ctx.set_member(&result, "port", Value::Num(local.port() as f64));
    Ok(result)
}

enum AcceptResult { Connected(Connected), Closed, Error(String) }

fn op_accept(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64;
    let (resolve, reject) = callbacks(ctx, args.get(1), args.get(2))?;
    let found = ctx.host_mut::<TlsRegistry>().and_then(|registry| registry.listeners.get(&id)).map(|entry| (
        entry.listener.clone(), entry.closed.clone(), entry.certificate.clone(), entry.private_key.clone(), entry.alpn.clone()
    ));
    let Some((listener, closed, certificate, private_key, alpn)) = found else {
        CallbackQueue::enqueue(ctx.op_state(), resolve, vec![Value::Null]);
        return Ok(Value::Undefined);
    };
    let task = ctx.host_mut::<TaskRegistry>().expect("task registry").register(resolve, Some(reject), decode_accept);
    completions(ctx).run_blocking(task, move || {
        let result = match listener.accept() {
            Ok((tcp, peer)) if !closed.load(Ordering::SeqCst) => {
                let local = tcp.local_addr().unwrap_or_else(|_| listener.local_addr().unwrap());
                tcp.set_read_timeout(Some(Duration::from_millis(100))).ok();
                match lumen_tls::TlsStream::accept_with_alpn(tcp, &certificate, &private_key, &alpn) {
                    Ok(stream) => {
                        let protocol = stream.protocol();
                        let cipher = stream.cipher();
                        let alpn = stream.alpn_protocol();
                        AcceptResult::Connected(Connected { stream, local, peer, protocol, cipher, alpn })
                    }
                    Err(error) => AcceptResult::Error(error),
                }
            }
            _ => AcceptResult::Closed,
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_accept(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<AcceptResult>().expect("TLS accept payload") {
        AcceptResult::Connected(connected) => register_connected(ctx, connected),
        AcceptResult::Closed => Ok(vec![Value::Null]),
        AcceptResult::Error(message) => Err(ctx.make_error("Error", message)),
    }
}

fn op_close_server(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64;
    let entry = ctx.host_mut::<TlsRegistry>().and_then(|registry| registry.listeners.remove(&id));
    if let Some(entry) = entry {
        entry.closed.store(true, Ordering::SeqCst);
        let address = if entry.local.ip().is_unspecified() {
            SocketAddr::new(if entry.local.is_ipv6() { IpAddr::V6(Ipv6Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) }, entry.local.port())
        } else { entry.local };
        let _ = TcpStream::connect_timeout(&address, Duration::from_millis(100));
    }
    Ok(Value::Undefined)
}

fn op_read(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64;
    let (resolve, reject) = callbacks(ctx, args.get(1), args.get(2))?;
    let found = ctx.host_mut::<TlsRegistry>().and_then(|registry| registry.sockets.get(&id))
        .map(|entry| (entry.stream.clone(), entry.closed.clone()));
    let Some((stream, closed)) = found else {
        CallbackQueue::enqueue(ctx.op_state(), resolve, vec![Value::Null]);
        return Ok(Value::Undefined);
    };
    let task = ctx.host_mut::<TaskRegistry>().expect("task registry").register(resolve, Some(reject), decode_read);
    completions(ctx).run_blocking(task, move || {
        let result = loop {
            if closed.load(Ordering::SeqCst) { break Ok(Vec::new()); }
            let mut bytes = vec![0u8; 65536];
            match stream.lock().unwrap().read(&mut bytes) {
                Ok(0) => break Ok(Vec::new()),
                Ok(length) => { bytes.truncate(length); break Ok(bytes); }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => std::thread::yield_now(),
                Err(error) => break Err(error.to_string()),
            }
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<Vec<u8>, String>>().expect("TLS read payload") {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]),
        Ok(bytes) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        Err(message) => Err(ctx.make_error("Error", message)),
    }
}

fn op_write(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64;
    let bytes = ctx.typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "TLS write expects bytes"))?;
    let (resolve, reject) = callbacks(ctx, args.get(2), args.get(3))?;
    let stream = ctx.host_mut::<TlsRegistry>().and_then(|registry| registry.sockets.get(&id)).map(|entry| entry.stream.clone());
    let Some(stream) = stream else { return Err(ctx.make_error("Error", "TLS socket is closed")); };
    let task = ctx.host_mut::<TaskRegistry>().expect("task registry").register(resolve, Some(reject), decode_write);
    completions(ctx).run_blocking(task, move || {
        let result = stream.lock().unwrap().write_all(&bytes).map(|()| bytes.len()).map_err(|error| error.to_string());
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_write(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<Result<usize, String>>().expect("TLS write payload") {
        Ok(length) => Ok(vec![Value::Num(length as f64)]),
        Err(message) => Err(ctx.make_error("Error", message)),
    }
}

fn op_close(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64;
    if let Some(entry) = ctx.host_mut::<TlsRegistry>().and_then(|registry| registry.sockets.remove(&id)) {
        entry.closed.store(true, Ordering::SeqCst);
    }
    Ok(Value::Undefined)
}
