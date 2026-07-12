use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

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
fn p256_ecdh_matches_node_vector() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      const c = require("node:crypto");
      const scalar = n => Buffer.from(n.toString(16).padStart(64, "0"), "hex");
      const b64u = bytes => Buffer.from(bytes).toString("base64").replace(/=/g, "").replace(/\+/g, "-").replace(/\//g, "_");
      const jwk = (ecdh, d) => {
        const point = ecdh.getPublicKey();
        return { kty: "EC", crv: "P-256", x: b64u(point.subarray(1, 33)), y: b64u(point.subarray(33)), d: b64u(scalar(d)) };
      };

      const a = c.createECDH("prime256v1");
      const b = c.createECDH("prime256v1");
      a.setPrivateKey(scalar(1n));
      b.setPrivateKey(scalar(2n));
      const expected = "7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978";
      console.log("secret", a.computeSecret(b.getPublicKey()).toString("hex") === expected,
                  b.computeSecret(a.getPublicKey()).toString("hex") === expected);

      const compressed = a.getPublicKey(null, "compressed");
      console.log("convert", compressed.toString("hex") === "036b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
                  c.ECDH.convertKey(compressed, "prime256v1", null, "hex", "uncompressed") === a.getPublicKey("hex"));

      const privateKey = c.createPrivateKey({ key: jwk(a, 1n), format: "jwk" });
      const publicKey = c.createPublicKey(c.createPrivateKey({ key: jwk(b, 2n), format: "jwk" }));
      console.log("keyobjects", c.diffieHellman({ privateKey, publicKey }).toString("hex") === expected);
    "#;

    match rt.eval(source).expect("ECDH script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    let text = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), ["secret true true", "convert true true", "keyobjects true"]);
}
