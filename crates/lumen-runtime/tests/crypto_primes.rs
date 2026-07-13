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
fn constrained_prime_generation_matches_node_options() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      const c = require("node:crypto");

      const ordinary = c.generatePrimeSync(16, { add: 10n, bigint: true });
      console.log("ordinary", ordinary % 10n, c.checkPrimeSync(ordinary));

      const safe = c.generatePrimeSync(16, {
        add: Buffer.from([12]), rem: Buffer.from([11]), safe: true, bigint: true,
      });
      console.log("safe", safe % 12n, c.checkPrimeSync(safe), c.checkPrimeSync((safe - 1n) / 2n));

      const ignored = c.generatePrimeSync(8, { rem: 0n, bigint: true });
      console.log("ignored", c.checkPrimeSync(ignored));

      try {
        c.generatePrimeSync(16, { add: 10n, rem: 2n, bigint: true });
      } catch (error) {
        console.log("invalid", error.name);
      }
    "#;

    match rt.eval(source).expect("prime script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    let text = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), [
        "ordinary 1 true",
        "safe 11 true true",
        "ignored true",
        "invalid RangeError",
    ]);
}
