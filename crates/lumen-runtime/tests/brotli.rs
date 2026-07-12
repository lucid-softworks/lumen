//! End-to-end coverage for the `node:zlib` Brotli sync, callback, and stream surfaces.

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
fn node_zlib_brotli_surfaces() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
        const z = require("node:zlib");
        const input = Buffer.from("abc123".repeat(1000));

        const encoded = z.brotliCompressSync(input);
        console.log("sync", z.brotliDecompressSync(encoded).equals(input), encoded.length > 0);

        const reference = Buffer.from("1b6f17000476c0e62e73216e8b03c90340770c", "hex");
        console.log("reference", z.brotliDecompressSync(reference).equals(input));

        z.brotliCompress(input, (err, asyncEncoded) => {
          if (err) throw err;
          z.brotliDecompress(asyncEncoded, (decodeErr, decoded) => {
            if (decodeErr) throw decodeErr;
            console.log("callback", decoded.equals(input));
          });
        });

        const stream = z.createBrotliCompress();
        const chunks = [];
        stream.on("data", chunk => chunks.push(chunk));
        stream.on("end", () => {
          const decompress = z.createBrotliDecompress();
          const decoded = [];
          decompress.on("data", chunk => decoded.push(chunk));
          decompress.on("end", () => console.log("stream", Buffer.concat(decoded).equals(input)));
          decompress.end(Buffer.concat(chunks));
        });
        stream.end(input);
    "#;

    match rt.eval(source).expect("Brotli script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    assert_eq!(
        out.lines(),
        [
            "sync true true",
            "reference true",
            "callback true",
            "stream true",
        ]
    );
}
