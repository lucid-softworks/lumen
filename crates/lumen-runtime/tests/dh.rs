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
fn finite_field_dh_matches_node_vector_and_modp14() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      const c = require("node:crypto");
      const a = c.createDiffieHellman(Buffer.from([23]), 5);
      const b = c.createDiffieHellman(Buffer.from([23]), 5);
      a.setPrivateKey(Buffer.from([6]));
      b.setPrivateKey(Buffer.from([15]));
      console.log("small", a.getPublicKey("hex"), b.getPublicKey("hex"),
                  a.computeSecret(b.getPublicKey()).toString("hex"),
                  b.computeSecret(a.getPublicKey()).toString("hex"));

      const x = c.getDiffieHellman("modp14");
      const y = c.createDiffieHellmanGroup("modp14");
      x.setPrivateKey(Buffer.from([6]));
      y.setPrivateKey(Buffer.from([15]));
      console.log("modp14", x.getPrime("hex").length, x.getGenerator("hex"),
                  x.computeSecret(y.getPublicKey()).equals(y.computeSecret(x.getPublicKey())));
    "#;

    match rt.eval(source).expect("DH script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    let lines = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(lines.lines().collect::<Vec<_>>(), ["small 08 13 02 02", "modp14 512 02 true"]);
}
