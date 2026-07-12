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
fn repl_evaluates_streamed_commands_and_multiline_input() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
      const repl = require("node:repl"), { Readable } = require("node:stream");
      const input = new Readable(), chunks = [];
      const output = { write(value) { chunks.push(String(value)); return true; } };
      const server = repl.start({ prompt: "L> ", input, output, ignoreUndefined: true });
      server.defineCommand("echo", { help: "echo text", action(value) { this._write("echo:" + value + "\n"); this.displayPrompt(); } });
      server.on("close", () => {
        console.log("shape", server instanceof repl.REPLServer, server.getPrompt(), repl.writer(3));
        console.log("output", JSON.stringify(chunks.join("")));
      });
      input.push("1 + 2\n");
      input.push("({\n");
      input.push("answer: 42\n");
      input.push("})\n");
      input.push(".echo hello\n");
      input.push("undefined\n");
      input.push(".exit\n");
      input.push(null);
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        [
            "shape true L>  3",
            "output \"L> 3\\nL> ... ... { answer: 42 }\\nL> echo:hello\\nL> L> \"",
        ]
    );
}
