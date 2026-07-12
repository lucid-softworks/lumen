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
fn csrf_tokens_interoperate_with_bun_and_reject_tampering() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const fixture = "AAABn1fI5Od0CO5U9mfXAcdR-7jbHCDuAAAAAAAAAACu5U1Sonwv5CNX3c5KeMX_R-OA3WsNYhUqBrFb4TXcXg";
        console.log("fixture", Bun.CSRF.verify(fixture, { secret: "interop-secret" }));
        const token = Bun.CSRF.generate("secret", { expiresIn: 0 });
        console.log("generated", token.length, Bun.CSRF.verify(token, { secret: "secret" }),
                    Bun.CSRF.verify(token, { secret: "wrong" }));
        const changed = token.slice(0, -1) + (token.endsWith("A") ? "B" : "A");
        console.log("tampered", Bun.CSRF.verify(changed, { secret: "secret" }));
        const hex = Bun.CSRF.generate("secret", { encoding: "hex", algorithm: "sha512" });
        console.log("sha512", hex.length, Bun.CSRF.verify(hex, { secret: "secret", encoding: "hex", algorithm: "sha512" }));
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let lines: Vec<_> = String::from_utf8(out.0.borrow().clone())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines,
        [
            "fixture true",
            "generated 86 true false",
            "tampered false",
            "sha512 192 true",
        ]
    );
}
