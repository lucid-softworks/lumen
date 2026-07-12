use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn secrets_validate_credential_identity_without_accessing_store() {
    let mut runtime = Runtime::new(); let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut { out: Box::new(out.clone()), err: Box::new(Captured::default()) });
    let source = r#"
      (async () => {
        for (const options of [null, {}, { service: "service" }]) {
          try { await Bun.secrets.get(options); } catch (error) { console.log(error.name, error.message); }
        }
      })();
    "#;
    match runtime.eval(source).unwrap() { Completion::Value(_) => {}, Completion::Throw { name, message } => panic!("uncaught {name}: {message}") }
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().lines().count(), 3);
}
