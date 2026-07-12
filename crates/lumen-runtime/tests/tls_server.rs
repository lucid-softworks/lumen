use std::cell::RefCell;
use std::io::Write;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(buffer); Ok(buffer.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn tls_server_accepts_verified_client_and_exchanges_data() {
    if Command::new("openssl").arg("version").output().is_err() { return; }
    let directory = std::env::temp_dir().join(format!("lumen-node-tls-server-{}-{}", std::process::id(), NEXT_DIR.fetch_add(1, Ordering::Relaxed)));
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

    let client_cert = certificate.clone();
    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(750));
        let mut child = Command::new("openssl")
            .args(["s_client", "-quiet", "-connect", &format!("127.0.0.1:{port}"), "-servername", "localhost", "-CAfile"])
            .arg(client_cert).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(b"ping").unwrap();
        child.wait_with_output().unwrap()
    });

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut { out: Box::new(out.clone()), err: Box::new(Captured::default()) });
    let source = format!(r#"
        const fs = require("node:fs"), tls = require("node:tls");
        const server = tls.createServer({{ cert: fs.readFileSync({certificate:?}), key: fs.readFileSync({key:?}) }}, function(socket) {{
            socket.on("data", function(data) {{ console.log("server", data.toString(), socket.authorized); socket.end("pong"); server.close(); }});
        }});
        server.listen({port}, "127.0.0.1");
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let client = client.join().unwrap();
    let _ = std::fs::remove_dir_all(directory);
    assert!(client.status.success());
    assert_eq!(client.stdout, b"pong");
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().trim(), "server ping true");
}
