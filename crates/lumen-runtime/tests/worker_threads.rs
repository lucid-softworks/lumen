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
fn worker_threads_exchange_structured_messages() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
        const { Worker } = require("node:worker_threads");
        const worker = new Worker(`
            const { parentPort, workerData, isMainThread, threadId } = require("node:worker_threads");
            parentPort.on("message", message => {
                parentPort.postMessage({
                    answer: message.value + workerData.offset,
                    worker: !isMainThread && threadId > 0,
                });
                parentPort.close();
            });
        `, { eval: true, workerData: { offset: 2 } });
        worker.on("online", () => worker.postMessage({ value: 40 }));
        worker.on("message", message => console.log("message", JSON.stringify(message)));
        worker.on("exit", code => console.log("exit", code));
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    assert_eq!(
        out.lines(),
        ["message {\"answer\":42,\"worker\":true}", "exit 0"]
    );
}
