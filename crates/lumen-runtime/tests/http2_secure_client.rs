use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::rc::Rc;

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
fn secure_client_negotiates_h2_with_node_server() {
    if Command::new("node").arg("--version").output().is_err()
        || Command::new("openssl").arg("version").output().is_err()
    {
        return;
    }
    let directory = std::env::temp_dir().join(format!("lumen-http2-secure-{}", std::process::id()));
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
    let server_source = format!(r#"
        const fs = require("node:fs"), http2 = require("node:http2");
        const server = http2.createSecureServer({{
          cert: fs.readFileSync({certificate:?}), key: fs.readFileSync({key:?})
        }});
        server.on("stream", stream => {{ stream.respond({{ ":status": 200 }}); stream.end("secure"); }});
        server.listen({port}, "127.0.0.1", () => console.log("ready"));
        server.on("session", session => session.on("close", () => server.close()));
    "#);
    let mut child = Command::new("node")
        .args(["-e", &server_source])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn().unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "ready");

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = format!(r#"
        const http2 = require("node:http2");
        const client = http2.connect("https://localhost:{port}", {{ rejectUnauthorized: false }});
        client.on("connect", (session, socket) => console.log("connected", session.encrypted, socket.alpnProtocol, socket.authorized));
        const request = client.request({{ ":path": "/" }});
        let body = "";
        request.on("data", chunk => body += chunk);
        request.on("end", () => {{ console.log("body", body); client.close(); }});
        request.end();
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let status = child.wait().unwrap();
    let _ = std::fs::remove_dir_all(directory);
    assert!(status.success());
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        ["connected true h2 false", "body secure"]
    );
}
