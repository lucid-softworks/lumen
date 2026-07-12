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
fn frames_and_hpack_round_trip_and_reject_invalid_input() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const codec = globalThis.__lumenHttp2Codec;
        const frame = codec.encodeFrame(1, 5, 0x1020304, Buffer.from("hello"));
        const decoder = new codec.FrameDecoder();
        console.log("partial", decoder.push(frame.subarray(0, 7)).length);
        const decoded = decoder.push(frame.subarray(7))[0];
        console.log("frame", decoded.type, decoded.flags, decoded.streamId,
                    decoded.payload.toString());

        const integer = codec.encodeInteger(1337, 5);
        console.log("integer", integer.toString("hex"),
                    codec.decodeInteger(integer, 0, 5).join(","));

        const encoder = new codec.Hpack();
        const headers = encoder.encode({
          ":method": "GET", ":path": "/", "content-type": "text/plain", "x-test": "yes"
        });
        const values = new codec.Hpack().decode(headers);
        console.log("headers", values[":method"], values[":path"],
                    values["content-type"], values["x-test"]);

        try { new codec.FrameDecoder(4).push(frame); }
        catch (error) { console.log("frame-error", error.code); }
        try { new codec.Hpack().decode(Buffer.from([0, 0x81, 0])); }
        catch (error) { console.log("hpack-error", error.code); }
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
            "partial 0",
            "frame 1 5 16909060 hello",
            "integer 1f9a0a 1337,3",
            "headers GET / text/plain yes",
            "frame-error ERR_HTTP2_FRAME_SIZE_ERROR",
            "hpack-error ERR_HTTP2_COMPRESSION_ERROR",
        ]
    );
}
