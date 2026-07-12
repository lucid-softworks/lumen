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
fn udp_socket_buffer_sizes_are_native_and_validated() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const dgram = require("node:dgram");
        const socket = dgram.createSocket("udp4");
        const initialRecv = socket.getRecvBufferSize();
        const initialSend = socket.getSendBufferSize();
        console.log("initial", initialRecv > 0, initialSend > 0);
        console.log("returns", socket.setRecvBufferSize(65536), socket.setSendBufferSize(65536));
        console.log("updated", socket.getRecvBufferSize() > 0, socket.getSendBufferSize() > 0);
        try { socket.setRecvBufferSize(0); }
        catch (error) { console.log("invalid", error.code); }
        socket.close();
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let lines: Vec<_> = String::from_utf8(out.0.borrow().clone()).unwrap().lines().map(str::to_string).collect();
    assert_eq!(
        lines,
        ["initial true true", "returns undefined undefined", "updated true true", "invalid ERR_OUT_OF_RANGE"]
    );
}
