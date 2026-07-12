use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn preview1_imports_read_and_write_guest_memory() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
        const { WASI } = require("node:wasi");
        const wasi = new WASI({ version: "preview1", args: ["app", "value"], env: { MODE: "test" } });
        const memory = new WebAssembly.Memory({ initial: 1 });
        wasi.initialize({ exports: { memory } });
        const imports = wasi.getImportObject().wasi_snapshot_preview1;
        const view = new DataView(memory.buffer), bytes = new Uint8Array(memory.buffer);
        console.log("sizes", imports.args_sizes_get(0, 4), view.getUint32(0, true), view.getUint32(4, true));
        imports.args_get(8, 32);
        const first = view.getUint32(8, true), second = view.getUint32(12, true);
        console.log("args", Buffer.from(bytes.subarray(first, second - 1)).toString(), Buffer.from(bytes.subarray(second, second + 5)).toString());
        imports.environ_sizes_get(64, 68); imports.environ_get(72, 80);
        console.log("env", Buffer.from(bytes.subarray(80, 89)).toString());
        imports.random_get(96, 16);
        console.log("random", bytes.subarray(96, 112).some(value => value !== 0));
        console.log("clock", imports.clock_time_get(0, 0n, 120), view.getUint32(120, true) !== 0);
        bytes.set(Buffer.from("wasi-out\n"), 160); view.setUint32(144, 160, true); view.setUint32(148, 9, true);
        console.log("write", imports.fd_write(1, 144, 1, 152), view.getUint32(152, true));
        console.log("nosys", imports.path_open());
        const exiting = new WASI({ version: "preview1", returnOnExit: true });
        console.log("exit", exiting.start({ exports: { memory, _start() { exiting.wasiImport.proc_exit(7); } } }));
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        [
            "sizes 0 2 10",
            "args app value",
            "env MODE=test",
            "random true",
            "clock 0 true",
            "wasi-out",
            "write 0 9",
            "nosys 52",
            "exit 7",
        ]
    );
}
