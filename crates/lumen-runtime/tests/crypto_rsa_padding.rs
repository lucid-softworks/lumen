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
fn rsa_no_padding_supports_public_and_private_transforms() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      const c = require("node:crypto");
      const b64u = n => {
        let hex = n.toString(16); if (hex.length % 2) hex = "0" + hex;
        return Buffer.from(hex, "hex").toString("base64url");
      };
      // Textbook RSA parameters: p=61, q=53, n=3233, e=17, d=2753.
      const privateKey = c.createPrivateKey({ key: {
        kty: "RSA", n: b64u(3233n), e: b64u(17n), d: b64u(2753n),
        p: b64u(61n), q: b64u(53n), dp: b64u(53n), dq: b64u(49n), qi: b64u(38n),
      }, format: "jwk" });
      const publicKey = c.createPublicKey(privateKey);
      const options = key => ({ key, padding: c.constants.RSA_NO_PADDING });
      const message = Buffer.from([0, 42]);
      const encrypted = c.publicEncrypt(options(publicKey), message);
      const signed = c.privateEncrypt(options(privateKey), message);
      console.log("roundtrip", c.privateDecrypt(options(privateKey), encrypted).equals(message),
                  c.publicDecrypt(options(publicKey), signed).equals(message));
      try { c.publicEncrypt(options(publicKey), Buffer.from([42])); }
      catch (error) { console.log("size", error.message.includes("same size")); }
    "#;

    match rt.eval(source).expect("raw RSA script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let text = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), ["roundtrip true true", "size true"]);
}
