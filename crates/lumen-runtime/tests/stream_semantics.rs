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
fn streams_account_for_exact_reads_backpressure_and_corking() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
      const { Readable, Writable, Duplex } = require("node:stream");
      const readable = new Readable({ highWaterMark: 5 });
      console.log("push", readable.push(Buffer.from("abc")), readable.push(Buffer.from("def")), readable.readableLength, readable.readableHighWaterMark);
      console.log("read", readable.read(4).toString(), readable.readableLength, readable.read(2).toString(), readable.readableLength);

      const first = { id: 1 }, objects = new Readable({ objectMode: true, highWaterMark: 2 });
      console.log("objects", objects.push(first), objects.push({ id: 2 }), objects.readableLength, objects.read() === first, objects.readableObjectMode);

      const writes = [];
      const writable = new Writable({ highWaterMark: 3, write(chunk, encoding, callback) { writes.push(chunk.toString()); queueMicrotask(callback); } });
      writable.on("drain", () => console.log("drain", writable.writableLength));
      writable.on("finish", () => console.log("finish", writes.join("")));
      console.log("pressure", writable.write("ab"), writable.write("cd"), writable.writableLength, writable.writableHighWaterMark);
      writable.end();

      const batches = [];
      const batched = new Writable({
        write(_chunk, _encoding, callback) { callback(); },
        writev(chunks, callback) { batches.push(chunks.map(value => value.chunk.toString()).join("+")); callback(); }
      });
      batched.cork(); batched.write("one"); batched.write("two"); batched.end();
      batched.on("finish", () => console.log("batch", batches.length, batches[0]));

      const duplex = new Duplex({ objectMode: true, read() {}, write(_chunk, _encoding, callback) { callback(); } });
      console.log("duplex", duplex.readableObjectMode, duplex.writableObjectMode);
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        [
            "push true false 6 5",
            "read abcd 2 ef 0",
            "objects true false 2 true true",
            "pressure true false 4 3",
            "duplex true true",
            "drain 2",
            "batch 1 one+two",
            "finish abcd",
        ]
    );
}
