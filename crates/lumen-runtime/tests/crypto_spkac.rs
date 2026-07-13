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
fn certificate_parses_and_verifies_openssl_spkac() {
    if Command::new("openssl").arg("version").output().is_err() { return; }
    let directory = std::env::temp_dir().join(format!(
        "lumen-node-spkac-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let key = directory.join("key.pem");
    let spkac = directory.join("spkac.txt");
    let generated = Command::new("openssl")
        .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:1024", "-out"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(generated.status.success());
    let generated = Command::new("openssl")
        .args(["spkac", "-key"])
        .arg(&key)
        .args(["-challenge", "lumen-challenge", "-out"])
        .arg(&spkac)
        .output()
        .unwrap();
    assert!(generated.status.success());

    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = format!(r#"
      const fs = require("node:fs"), {{ Certificate }} = require("node:crypto");
      const encoded = fs.readFileSync({spkac:?}, "utf8").trim();
      const value = encoded.slice(encoded.indexOf("=") + 1);
      const instance = Certificate();
      console.log("static", Certificate.verifySpkac(value), Certificate.exportChallenge(value).toString());
      console.log("instance", instance.verifySpkac(Buffer.from(value)), instance.exportPublicKey(value).toString().startsWith("-----BEGIN PUBLIC KEY-----"));
      const corrupt = Buffer.from(value, "base64");
      corrupt[corrupt.length - 1] ^= 1;
      console.log("invalid", Certificate.verifySpkac(corrupt), Certificate.exportChallenge("invalid").length);
    "#);
    match rt.eval(&source).expect("SPKAC script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let _ = std::fs::remove_dir_all(directory);

    let text = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), [
        "static true lumen-challenge",
        "instance true true",
        "invalid false 0",
    ]);
}
