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
fn strips_erasable_types_into_runnable_javascript() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const { stripTypeScriptTypes } = require("node:module");
        const typed = `interface Pair { left: number; right: number }
type Numeric = number | bigint;
const start: number = 40;
function add(value: number, step?: number): number { return value + (step || 0); }
const literal = { number: 1, text: "value: number" };
const increment = (value: number): number => value + literal.number;
const result: Numeric = (increment(add(start, 1)) as number) satisfies Numeric;
return result;`;
        const stripped = stripTypeScriptTypes(typed, { sourceUrl: "fixture.ts" });
        console.log("shape", stripped.split("\n").length === typed.split("\n").length + 2,
                    stripped.includes("//# sourceURL=fixture.ts;"));
        console.log("result", new Function(stripped)());
        try { stripTypeScriptTypes("enum Direction { Up, Down }"); }
        catch (error) { console.log("enum", error.code); }
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
            "shape true true",
            "result 42",
            "enum ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX",
        ]
    );
}
