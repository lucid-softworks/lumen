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
fn tty_streams_wrap_process_io_without_claiming_terminal_support() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
      const tty = require("node:tty"), stream = require("node:stream");
      const input = new tty.ReadStream(0), output = new tty.WriteStream(1);
      console.log("shape", input instanceof stream.Readable, output instanceof stream.Writable, tty.isatty(1));
      console.log("input", input.fd, input.isTTY, input.setRawMode(true) === input, input.isRaw);
      console.log("output", output.fd, output.isTTY, output.getColorDepth(), output.hasColors(), output.getWindowSize().join("x"));
      console.log("cursor", output.clearLine(0), output.clearScreenDown(), output.cursorTo(0), output.moveCursor(1, 1));
      output.end("tty-write\n");
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        ["shape true true false", "input 0 false true true", "output 1 false 1 false 80x24", "cursor true true true true", "tty-write"]
    );
}
