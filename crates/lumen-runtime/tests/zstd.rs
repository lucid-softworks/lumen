//! End-to-end coverage for Node and Bun Zstandard APIs.

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
fn node_and_bun_zstd_surfaces() {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
      (async () => {
        const z = require("node:zlib");
        const Bun = require("bun");
        const input = Buffer.from("abc123".repeat(3000));

        const encoded = z.zstdCompressSync(input);
        console.log("node-sync", z.zstdDecompressSync(encoded).equals(input));

        const reference = Buffer.from("28b52ffd6050457500003061626331323301004746f65204", "hex");
        console.log("reference", z.zstdDecompressSync(reference).equals(input));

        const callbackEncoded = await new Promise((resolve, reject) =>
          z.zstdCompress(input, (err, value) => err ? reject(err) : resolve(value))
        );
        console.log("node-callback", z.zstdDecompressSync(callbackEncoded).equals(input));

        const streamEncoded = await new Promise((resolve, reject) => {
          const stream = z.createZstdCompress();
          const chunks = [];
          stream.on("data", chunk => chunks.push(chunk));
          stream.on("error", reject);
          stream.on("end", () => resolve(Buffer.concat(chunks)));
          stream.end(input);
        });
        console.log("node-stream", z.zstdDecompressSync(streamEncoded).equals(input));

        const bunEncoded = Bun.zstdCompressSync(input);
        console.log("bun-sync", Buffer.from(Bun.zstdDecompressSync(bunEncoded)).equals(input));
        const bunAsync = await Bun.zstdCompress(input);
        console.log("bun-async", Buffer.from(await Bun.zstdDecompress(bunAsync)).equals(input));
      })();
    "#;

    match rt.eval(source).expect("Zstd script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    assert_eq!(
        out.lines(),
        [
            "node-sync true",
            "reference true",
            "node-callback true",
            "node-stream true",
            "bun-sync true",
            "bun-async true",
        ]
    );
}
