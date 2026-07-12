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
fn p256_ecdsa_matches_bun_and_supports_both_encodings() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      const c = require("node:crypto");
      const jwk = {
        kty: "EC", crv: "P-256",
        x: "axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY",
        y: "T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU",
        d: "AQ",
      };
      const privateKey = c.createPrivateKey({ key: jwk, format: "jwk" });
      const publicKey = c.createPublicKey(privateKey);
      const message = Buffer.from("oracle");
      const bunSignature = Buffer.from("3046022100d7e4ea3d2e968f527f30914afcaa41e8992d88a03c05391275526a82b3e45906022100d118438050653b3c452ff262464404f77eb0cd525adc42e03b5e2f06e88b2369", "hex");
      console.log("bun", c.verify("sha256", message, publicKey, bunSignature));

      for (const dsaEncoding of ["der", "ieee-p1363"]) {
        const signature = c.sign("sha256", message, { key: privateKey, dsaEncoding });
        console.log(dsaEncoding, signature.length, c.verify("sha256", message, { key: publicKey, dsaEncoding }, signature),
                    c.verify("sha256", Buffer.from("wrong"), { key: publicKey, dsaEncoding }, signature));
      }

      const generated = c.generateKeyPairSync("ec", { namedCurve: "prime256v1" });
      const signature = c.createSign("sha384").update("streamed").sign(generated.privateKey);
      console.log("generated", c.createVerify("sha384").update("streamed").verify(generated.publicKey, signature));
    "#;

    match rt.eval(source).expect("ECDSA script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    let lines = String::from_utf8(out.0.borrow().clone()).unwrap();
    let lines: Vec<_> = lines.lines().collect();
    assert_eq!(lines[0], "bun true");
    assert!(lines[1].starts_with("der ") && lines[1].ends_with(" true false"), "{}", lines[1]);
    assert_eq!(lines[2], "ieee-p1363 64 true false");
    assert_eq!(lines[3], "generated true");
}
