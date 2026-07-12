use std::cell::RefCell;
use std::io::Write;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use lumen_runtime::{Completion, ConsoleOut, Runtime};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn x509_parses_and_verifies_openssl_certificate() {
    if Command::new("openssl").arg("version").output().is_err() { return; }
    let dir = std::env::temp_dir().join(format!(
        "lumen-x509-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let status = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "2", "-nodes"])
        .arg("-keyout").arg(&key)
        .arg("-out").arg(&cert)
        .args([
            "-subj", "/C=GB/O=Lumen Test/CN=example.test",
            "-addext", "basicConstraints=critical,CA:false",
            "-addext", "subjectAltName=DNS:example.test,DNS:*.example.org,IP:127.0.0.1,email:test@example.test",
        ])
        .output()
        .expect("run openssl")
        .status;
    assert!(status.success());

    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = format!(
        r#"
          const fs = require("node:fs"), c = require("node:crypto");
          const cert = new c.X509Certificate(fs.readFileSync({cert:?}));
          const key = c.createPrivateKey(fs.readFileSync({key:?}));
          console.log(cert.subject.replace(/\n/g, "|"));
          console.log(cert.ca, cert.verify(cert.publicKey), cert.checkPrivateKey(key), cert.checkIssued(cert));
          console.log(cert.checkHost("www.example.org"), cert.checkHost("bad.test"));
          console.log(cert.checkEmail("test@example.test"), cert.checkIP("127.0.0.1"));
          console.log(cert.fingerprint256.split(":").length, cert.toJSON().startsWith("-----BEGIN CERTIFICATE-----"));
        "#,
        cert = cert.to_string_lossy(),
        key = key.to_string_lossy(),
    );
    match rt.eval(&source).expect("X509 script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let _ = std::fs::remove_dir_all(&dir);

    let lines = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        [
            "C=GB|O=Lumen Test|CN=example.test",
            "false true true true",
            "www.example.org undefined",
            "test@example.test 127.0.0.1",
            "32 true",
        ]
    );
}
