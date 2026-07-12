//! End-to-end coverage for Bun.password over the native Argon2/bcrypt worker ops.

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

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn bun_password_sync_and_async() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      (async () => {
        const Bun = require("bun");
        const fixture = "$argon2id$v=19$m=64,t=1,p=1$Qlq4y6N6W71yfwUALUE1saUFYrEf6EgHwYFtX0swMtU$3Zy/fZUAYcYmCSCCix/WJ0szZpIJI6SYjmGVcdfGtPw";
        console.log("fixture", Bun.password.verifySync("hunter2", fixture), Bun.password.verifySync("wrong", fixture));

        const bcrypt = Bun.password.hashSync("secret", { algorithm: "bcrypt", cost: 4 });
        console.log("bcrypt", bcrypt.startsWith("$2b$04$"), Bun.password.verifySync("secret", bcrypt));

        const argon = await Bun.password.hash("secret", { algorithm: "argon2id", memoryCost: 32, timeCost: 1 });
        console.log("argon", argon.startsWith("$argon2id$v=19$m=32,t=1,p=1$"), await Bun.password.verify("secret", argon));
        console.log("shape", typeof Bun.password.hash, typeof Bun.password.verifySync, typeof Bun.password.needsRehash);
      })();
    "#;

    match rt.eval(source).expect("password script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    assert_eq!(
        out.lines(),
        [
            "fixture true false",
            "bcrypt true true",
            "argon true true",
            "shape function function undefined",
        ]
    );
}
