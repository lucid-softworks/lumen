//! End-to-end tests for `node:net` (TCP) and `node:dgram` (UDP) over the `__net`/`__udp` native
//! ops — loopback echo, half-close, error codes, and cross-checks against plain std sockets
//! standing in for a foreign peer. All sockets bind port 0 on 127.0.0.1, so runs are parallel-safe.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::Duration;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[cfg(unix)]
static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

/// A console sink the test can read back after the loop runs (same shape as the unit tests').
#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn test_runtime() -> (Runtime, Captured) {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    (rt, out)
}

fn eval_ok(rt: &mut Runtime, src: &str) {
    match rt.eval(src).expect("parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

// ---- node:net ---------------------------------------------------------------------------------

#[test]
fn net_loopback_echo_and_half_close() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        const server = net.createServer((sock) => {
            sock.on("data", (d) => sock.write(d));
            sock.on("end", () => sock.end());
        });
        server.listen(0, "127.0.0.1", () => {
            const port = server.address().port;
            const client = net.connect(port, "127.0.0.1", () => {
                console.log("connected", client.remoteAddress, client.remotePort === port,
                            client.remoteFamily, typeof client.localPort);
                client.write("hello ");
                client.write(Buffer.from("world"));
                client.end();
            });
            let got = "";
            client.on("data", (d) => { got += d.toString(); });
            client.on("end", () => console.log("echo:", got));
            client.on("close", (hadError) => {
                console.log("client close, hadError:", hadError);
                server.close(() => console.log("server closed"));
            });
        });
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "connected 127.0.0.1 true IPv4 number",
            "echo: hello world",
            "client close, hadError: false",
            "server closed",
        ]
    );
}

#[test]
fn net_binary_integrity_64k() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        const N = 65536 + 12345; // spans multiple 64k read chunks
        const payload = Buffer.alloc(N);
        for (let i = 0; i < N; i++) payload[i] = (i * 31 + 7) & 0xff;
        const server = net.createServer((sock) => { sock.pipe(sock); });
        server.listen(0, "127.0.0.1", () => {
            const c = net.connect(server.address().port, "127.0.0.1", () => { c.write(payload); c.end(); });
            const chunks = [];
            c.on("data", (d) => chunks.push(d));
            c.on("end", () => {
                const got = Buffer.concat(chunks);
                let ok = got.length === N;
                if (ok) for (let i = 0; i < N; i++) if (got[i] !== payload[i]) { ok = false; break; }
                console.log("intact:", ok, "bytesRead:", c.bytesRead === N, "bytesWritten:", c.bytesWritten === N);
                server.close();
            });
        });
        "#,
    );
    assert_eq!(out.lines(), ["intact: true bytesRead: true bytesWritten: true"]);
}

#[test]
fn net_end_reply_pattern_and_events_order() {
    // The classic `sock.on('end', () => sock.end(reply))` pattern (verified against Node).
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        const server = net.createServer((sock) => {
            let data = "";
            sock.on("data", (d) => data += d);
            sock.on("end", () => { sock.end("SRV[" + data + "]"); server.close(); });
        });
        server.listen(0, "127.0.0.1", () => {
            const c = net.connect(server.address().port, "127.0.0.1", () => { c.write("ping"); c.end(); });
            let got = "";
            c.on("data", (d) => got += d);
            c.on("end", () => console.log("reply:", got));
        });
        "#,
    );
    assert_eq!(out.lines(), ["reply: SRV[ping]"]);
}

#[test]
fn net_allow_half_open_keeps_write_side() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        const server = net.createServer({ allowHalfOpen: true }, (sock) => {
            sock.on("data", () => {});
            sock.on("end", () => {
                setTimeout(() => { sock.write("late"); sock.end(); }, 30);
            });
        });
        server.listen(0, "127.0.0.1", () => {
            const c = net.connect(server.address().port, "127.0.0.1", () => { c.write("x"); c.end(); });
            let got = "";
            c.on("data", (d) => got += d);
            c.on("end", () => { console.log("half-open got:", got); server.close(); });
        });
        "#,
    );
    assert_eq!(out.lines(), ["half-open got: late"]);
}

#[test]
fn net_econnrefused_and_eaddrinuse() {
    let (mut rt, out) = test_runtime();
    // Reserve a port nothing listens on: bind + drop (racy in theory, fine on loopback in practice).
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().unwrap().port()
    };
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const net = require("node:net");
            const c = net.connect({dead_port}, "127.0.0.1");
            c.on("error", (e) => console.log("error:", e.code, e.syscall, e.address, e.port === {dead_port}));
            c.on("close", (hadError) => console.log("close hadError:", hadError));
            const s1 = net.createServer();
            s1.listen(0, "127.0.0.1", () => {{
                const s2 = net.createServer();
                s2.on("error", (e) => {{ console.log("listen error:", e.code); s1.close(); }});
                s2.listen(s1.address().port, "127.0.0.1");
            }});
            "#,
        ),
    );
    let lines = out.lines();
    assert!(lines.contains(&"error: ECONNREFUSED connect 127.0.0.1 true".to_string()), "{lines:?}");
    assert!(lines.contains(&"close hadError: true".to_string()), "{lines:?}");
    assert!(lines.contains(&"listen error: EADDRINUSE".to_string()), "{lines:?}");
}

#[test]
fn net_lumen_client_against_std_server() {
    use std::io::{Read, Write};
    // A plain std TcpListener plays the foreign server: read to EOF, reply, close.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).expect("read");
        let reply = format!("EXT[{}]", String::from_utf8_lossy(&buf));
        s.write_all(reply.as_bytes()).expect("write");
    });
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const net = require("node:net");
            const c = net.connect({port}, "127.0.0.1", () => {{ c.write("from-lumen"); c.end(); }});
            let got = "";
            c.on("data", (d) => got += d);
            c.on("end", () => console.log("got:", got));
            "#,
        ),
    );
    server.join().expect("server thread");
    assert_eq!(out.lines(), ["got: EXT[from-lumen]"]);
}

#[test]
fn net_std_client_against_lumen_server() {
    use std::io::{Read, Write};
    // Pre-bind a port for the lumen server so the std client knows where to go.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().unwrap().port()
    };
    let client = std::thread::spawn(move || {
        // Retry until the lumen server (starting on the main thread) is listening.
        let mut stream = None;
        for _ in 0..200 {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        let mut s = stream.expect("lumen server never came up");
        s.write_all(b"from-std").expect("write");
        s.shutdown(std::net::Shutdown::Write).expect("shutdown");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).expect("read");
        String::from_utf8(buf).expect("utf8")
    });
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const net = require("node:net");
            const server = net.createServer((sock) => {{
                let data = "";
                sock.on("data", (d) => data += d);
                sock.on("end", () => {{ sock.end("LUMEN[" + data + "]"); server.close(); }});
            }});
            server.listen({port}, "127.0.0.1", () => console.log("listening:", server.address().port === {port}));
            "#,
        ),
    );
    let reply = client.join().expect("client thread");
    assert_eq!(reply, "LUMEN[from-std]");
    assert_eq!(out.lines(), ["listening: true"]);
}

#[test]
fn net_unref_does_not_hold_the_loop() {
    let (mut rt, out) = test_runtime();
    // eval() only returns when run_to_completion() finishes, so completing at all proves the
    // unref'd server did not keep the loop alive.
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        const server = net.createServer(() => {});
        server.listen(0, "127.0.0.1", () => { server.unref(); console.log("unrefd"); });
        "#,
    );
    assert_eq!(out.lines(), ["unrefd"]);
}

#[test]
fn net_address_math_still_real() {
    // The pre-existing pure surface must survive the socket rewrite.
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        console.log(net.isIP("127.0.0.1"), net.isIP("::1"), net.isIP("nope"));
        const bl = new net.BlockList();
        bl.addSubnet("10.0.0.0", 8);
        console.log(bl.check("10.1.2.3"), bl.check("11.0.0.1"));
        const sa = net.SocketAddress.parse("[::1]:8080");
        console.log(sa.family, sa.port);
        "#,
    );
    assert_eq!(out.lines(), ["4 6 0", "true false", "ipv6 8080"]);
}

#[cfg(unix)]
#[test]
fn net_unix_path_accept_and_cleanup() {
    let path = std::env::temp_dir().join(format!(
        "lumen-net-{}-{}.sock",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_file(&path);
    let peer_path = path.clone();
    let peer = std::thread::spawn(move || {
        for _ in 0..100 {
            if std::os::unix::net::UnixStream::connect(&peer_path).is_ok() { return; }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("connect to lumen Unix server at {}", peer_path.display());
    });
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(r#"
          const fs = require("node:fs"), net = require("node:net"), path = {path:?};
          const server = net.createServer(socket => {{
            console.log("accepted", socket.remoteAddress === undefined);
            socket.destroy();
            server.close(() => console.log("closed", fs.existsSync(path)));
          }});
          server.listen(path, () => {{
            console.log("listening", server.address() === path, fs.existsSync(path));
          }});
        "#),
    );
    peer.join().unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(out.lines(), [
        "listening true true",
        "accepted true",
        "closed false",
    ]);
}

#[cfg(unix)]
#[test]
fn net_unix_path_client_writes_to_std_peer() {
    let path = std::env::temp_dir().join(format!(
        "lumen-net-peer-{}-{}.sock",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ping");
    });
    let (mut rt, out) = test_runtime();
    eval_ok(&mut rt, &format!(r#"
      const net = require("node:net");
      const client = new net.Socket({{ _deferRead: true }});
      client.connect({path:?}, () => {{
        console.log("connected", client.remoteAddress === undefined);
        client.write("ping");
        setTimeout(() => client.destroy(), 20);
      }});
    "#));
    peer.join().unwrap();
    let _ = std::fs::remove_file(path);
    assert_eq!(out.lines(), ["connected true"]);
}

// ---- node:dgram -------------------------------------------------------------------------------

#[test]
fn dgram_loopback_echo_with_rinfo() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const dgram = require("node:dgram");
        const server = dgram.createSocket("udp4");
        server.on("message", (msg, rinfo) => {
            console.log("server got:", msg.toString(), rinfo.family, rinfo.size === msg.length);
            server.send(Buffer.from("pong:" + msg), rinfo.port, rinfo.address);
        });
        server.on("listening", () => {
            const a = server.address();
            console.log("listening:", a.address, a.family, typeof a.port);
            const client = dgram.createSocket("udp4");
            client.on("message", (msg) => {
                console.log("client got:", msg.toString());
                client.close(() => server.close());
            });
            client.send("ping", a.port, "127.0.0.1", (err, n) => console.log("sent:", err === null, n));
        });
        server.bind(0, "127.0.0.1");
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "listening: 127.0.0.1 IPv4 number",
            "sent: true 4",
            "server got: ping IPv4 true",
            "client got: pong:ping",
        ]
    );
}

#[test]
fn dgram_connected_mode_and_offsets() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const dgram = require("node:dgram");
        const s = dgram.createSocket("udp4");
        s.on("message", (m) => { console.log("got:", m.toString()); s.close(); });
        s.bind(0, "127.0.0.1", () => {
            const c = dgram.createSocket("udp4");
            c.connect(s.address().port, "127.0.0.1", () => {
                const r = c.remoteAddress();
                console.log("remote:", r.address, r.family, typeof r.port);
                // offset/length form on a connected socket (Node-verified)
                c.send(Buffer.from("xxHELLOxx"), 2, 5, (err) => { console.log("err:", err); c.close(); });
            });
        });
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "remote: 127.0.0.1 IPv4 number",
            "err: null",
            "got: HELLO",
        ]
    );
}

#[test]
fn dgram_against_std_udp_socket() {
    // A plain std UdpSocket plays the foreign peer.
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    let peer_port = peer.local_addr().unwrap().port();
    let echo = std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        let (n, from) = peer.recv_from(&mut buf).expect("recv");
        let reply = format!("STD[{}]", String::from_utf8_lossy(&buf[..n]));
        peer.send_to(reply.as_bytes(), from).expect("send");
    });
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const dgram = require("node:dgram");
            const c = dgram.createSocket("udp4");
            c.on("message", (m, r) => {{ console.log("got:", m.toString(), r.port === {peer_port}); c.close(); }});
            c.send("from-lumen", {peer_port}, "127.0.0.1");
            "#,
        ),
    );
    echo.join().expect("echo thread");
    assert_eq!(out.lines(), ["got: STD[from-lumen] true"]);
}

#[test]
fn dgram_errors_and_option_paths() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const dgram = require("node:dgram");
        try { dgram.createSocket("tcp"); } catch (e) { console.log("bad type:", e.code); }
        const u = dgram.createSocket("udp4");
        u.bind(0, "127.0.0.1", () => {
            u.setBroadcast(true);
            console.log("ttl:", u.setTTL(32));
            console.log("mttl:", u.setMulticastTTL(2));
            u.setMulticastLoopback(true);
            u.addMembership("224.0.0.114");
            u.dropMembership("224.0.0.114");
            console.log("mcast iface:", u.setMulticastInterface("0.0.0.0") === u);
            try { u.setTTL(0); } catch (e) { console.log("ttl range:", e.code); }
            u.close(() => {
                try { u.send("x", 9, "127.0.0.1"); }
                catch (e) { console.log("send after close:", e.code); }
            });
        });
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "bad type: ERR_SOCKET_BAD_TYPE",
            "ttl: 32",
            "mttl: 2",
            "mcast iface: true",
            "ttl range: ERR_OUT_OF_RANGE",
            "send after close: ERR_SOCKET_DGRAM_NOT_RUNNING",
        ]
    );
}

#[test]
fn dgram_udp6_and_eaddrinuse() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const dgram = require("node:dgram");
        const a = dgram.createSocket("udp6");
        a.bind(0, "::1", () => {
            const addr = a.address();
            console.log("udp6:", addr.family, addr.address);
            a.setTTL(64); // IPV6_UNICAST_HOPS, not IP_TTL
            const b = dgram.createSocket("udp6");
            b.on("error", (e) => { console.log("bind error:", e.code); a.close(); });
            b.bind(addr.port, "::1");
        });
        "#,
    );
    assert_eq!(out.lines(), ["udp6: IPv6 ::1", "bind error: EADDRINUSE"]);
}
