use std::cell::RefCell;
use std::io::Write;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn server_handles_multiplexed_node_requests() {
    if Command::new("node").arg("--version").output().is_err() { return; }
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let source = format!(r#"
          const http2 = require("node:http2");
          const client = http2.connect("http://127.0.0.1:{port}");
          let complete = 0;
          for (const [path, body] of [["/one", ""], ["/two", "payload"]]) {{
            const request = client.request({{ ":method": body ? "POST" : "GET", ":path": path }});
            let response = "";
            request.on("data", chunk => response += chunk);
            request.on("end", () => {{ console.log(path, response); if (++complete === 2) client.close(); }});
            request.end(body);
          }}
        "#);
        Command::new("node").args(["-e", &source]).output().unwrap()
    });

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = format!(r#"
        const http2 = require("node:http2");
        let requests = 0, streams = 0;
        const server = http2.createServer((request, response) => {{
          let body = "";
          request.on("data", chunk => body += chunk);
          request.on("end", () => {{
            response.setHeader("x-lumen", "yes");
            response.end(request.url + ":" + body.toUpperCase());
            if (++requests === 2) server.close();
          }});
        }});
        server.on("stream", () => {{ streams++; if (streams === 2) console.log("streams", streams); }});
        server.listen({port}, "127.0.0.1");
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let child = client.join().unwrap();
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
    let child_lines: Vec<_> = String::from_utf8(child.stdout).unwrap().lines().map(str::to_string).collect();
    assert!(child_lines.contains(&"/one /one:".to_string()), "{child_lines:?}");
    assert!(child_lines.contains(&"/two /two:PAYLOAD".to_string()), "{child_lines:?}");
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().trim(), "streams 2");
}

#[test]
fn secure_server_negotiates_h2_with_node_client() {
    if Command::new("node").arg("--version").output().is_err()
        || Command::new("openssl").arg("version").output().is_err()
    {
        return;
    }
    let directory = std::env::temp_dir().join(format!("lumen-http2-server-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let generated = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "1", "-nodes"])
        .arg("-keyout").arg(&key).arg("-out").arg(&certificate)
        .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost"])
        .output().unwrap();
    assert!(generated.status.success());
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let source = format!(r#"
          const http2 = require("node:http2");
          const client = http2.connect("https://localhost:{port}", {{ rejectUnauthorized: false }});
          client.on("connect", () => console.log("alpn", client.socket.alpnProtocol));
          const request = client.request({{ ":path": "/secure" }});
          let body = "";
          request.on("data", chunk => body += chunk);
          request.on("end", () => {{ console.log("body", body); client.close(); }});
          request.end();
        "#);
        Command::new("node").args(["-e", &source]).output().unwrap()
    });

    let mut runtime = Runtime::new();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(Captured::default()), err: Box::new(Captured::default()),
    });
    let source = format!(r#"
        const fs = require("node:fs"), http2 = require("node:http2");
        const server = http2.createSecureServer({{
          cert: fs.readFileSync({certificate:?}), key: fs.readFileSync({key:?})
        }}, (request, response) => {{ response.end("secure:" + request.url); server.close(); }});
        server.listen({port}, "127.0.0.1");
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let child = client.join().unwrap();
    let _ = std::fs::remove_dir_all(directory);
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
    assert_eq!(String::from_utf8(child.stdout).unwrap().lines().collect::<Vec<_>>(), ["alpn h2", "body secure:/secure"]);
}
