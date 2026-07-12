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
        console.log("badfd", imports.path_open());
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
            "badfd 8",
            "exit 7",
        ]
    );
}

#[test]
#[cfg(unix)]
fn preview1_preopens_confine_and_access_files() {
    let directory = std::env::temp_dir().join(format!("lumen-wasi-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("data.txt"), b"hello").unwrap();
    let outside = directory.with_extension("outside");
    std::fs::write(&outside, b"outside").unwrap();
    std::os::unix::fs::symlink(&outside, directory.join("escape-link")).unwrap();
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = format!(r#"
        const {{ WASI }} = require("node:wasi");
        const wasi = new WASI({{ version: "preview1", preopens: {{ "/sandbox": {directory:?} }} }});
        const memory = new WebAssembly.Memory({{ initial: 1 }});
        wasi.initialize({{ exports: {{ memory }} }});
        const i = wasi.wasiImport, b = new Uint8Array(memory.buffer), v = new DataView(memory.buffer);
        console.log("prestat", i.fd_prestat_get(3, 0), v.getUint32(4, true));
        i.fd_prestat_dir_name(3, 16, 8);
        console.log("name", Buffer.from(b.subarray(16, 24)).toString());
        b.set(Buffer.from("data.txt"), 32);
        console.log("open", i.path_open(3, 0, 32, 8, 0, 0n, 0n, 0, 48));
        const fd = v.getUint32(48, true);
        v.setUint32(56, 128, true); v.setUint32(60, 5, true);
        console.log("read", i.fd_read(fd, 56, 1, 64), Buffer.from(b.subarray(128, 133)).toString());
        console.log("seek", i.fd_seek(fd, 0n, 0, 72));
        b.set(Buffer.from("HELLO"), 144); v.setUint32(80, 144, true); v.setUint32(84, 5, true);
        console.log("write", i.fd_write(fd, 80, 1, 88), v.getUint32(88, true));
        console.log("stat", i.fd_filestat_get(fd, 160), v.getUint32(192, true));
        b.set(Buffer.from("../outside"), 240);
        console.log("escape", i.path_open(3, 0, 240, 10, 0, 0n, 0n, 0, 252));
        b.set(Buffer.from("escape-link"), 256);
        console.log("symlink", i.path_open(3, 0, 256, 11, 0, 0n, 0n, 0, 272));
        console.log("close", i.fd_close(fd), i.fd_read(fd, 56, 1, 64));
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        ["prestat 0 8", "name /sandbox", "open 0", "read 0 hello", "seek 0", "write 0 5", "stat 0 5", "escape 76", "symlink 76", "close 0 8"]
    );
    assert_eq!(std::fs::read(directory.join("data.txt")).unwrap(), b"HELLO");
    let _ = std::fs::remove_dir_all(directory);
    let _ = std::fs::remove_file(outside);
}
