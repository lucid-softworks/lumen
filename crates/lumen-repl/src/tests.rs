use super::*;
use lumen_runtime::ConsoleOut;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

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
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).expect("utf8")
    }
}

fn test_repl() -> (Repl, Captured) {
    let mut runtime = Runtime::new();
    let console = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(console.clone()),
        err: Box::new(console.clone()),
    });
    (Repl::new(runtime), console)
}

fn done(repl: &mut Repl, line: &str) -> String {
    match repl.feed(line) {
        Step::Done(s) => s,
        other => panic!("expected Done for {line:?}, got {other:?}"),
    }
}

/// The Phase-3 acceptance criteria, verbatim from the plan.
#[test]
fn acceptance() {
    let (mut repl, console) = test_repl();
    assert_eq!(done(&mut repl, "1 + 1"), "2");
    assert_eq!(done(&mut repl, "await Promise.resolve(42)"), "42");
    // The timer fires (its console output streams) before feed() returns => before the next
    // prompt would render.
    assert_eq!(
        done(
            &mut repl,
            r#"setTimeout(() => console.log("hi"), 10); "queued""#
        ),
        "'queued'"
    );
    assert_eq!(console.text(), "hi\n");
}

#[test]
fn realm_and_last_value_persist() {
    let (mut repl, _console) = test_repl();
    assert_eq!(done(&mut repl, "const x = 10"), "undefined");
    assert_eq!(done(&mut repl, "x * 2"), "20");
    assert_eq!(done(&mut repl, "_ + 1"), "21");
}

#[test]
fn multiline_continuation() {
    let (mut repl, _console) = test_repl();
    assert!(!repl.continuing());
    assert_eq!(repl.feed("function fact(n) {"), Step::More);
    assert!(repl.continuing());
    assert_eq!(
        repl.feed("  return n < 2 ? 1 : n * fact(n - 1)"),
        Step::More
    );
    assert_eq!(done(&mut repl, "}"), "undefined");
    assert_eq!(done(&mut repl, "fact(5)"), "120");
}

#[test]
fn syntax_error_reports_and_recovers() {
    let (mut repl, _console) = test_repl();
    assert!(done(&mut repl, "let let = 1").starts_with("SyntaxError:"));
    assert!(!repl.continuing(), "buffer cleared after error");
    assert_eq!(done(&mut repl, "2 + 2"), "4");
}

#[test]
fn uncaught_throw_reports_and_session_survives() {
    let (mut repl, _console) = test_repl();
    assert_eq!(
        done(&mut repl, r#"throw new TypeError("boom")"#),
        "Uncaught TypeError: boom"
    );
    assert_eq!(done(&mut repl, "1"), "1");
}

#[test]
fn await_rejection_reports_uncaught() {
    let (mut repl, _console) = test_repl();
    assert_eq!(
        done(&mut repl, r#"await Promise.reject(new RangeError("nope"))"#),
        "Uncaught RangeError: nope"
    );
}

#[test]
fn await_with_timer_backed_promise() {
    let (mut repl, _console) = test_repl();
    assert_eq!(
        done(
            &mut repl,
            "await new Promise((resolve) => setTimeout(() => resolve('slept'), 10))"
        ),
        "'slept'"
    );
}

#[test]
fn never_settling_await_is_reported_not_hung() {
    let (mut repl, _console) = test_repl();
    assert_eq!(
        done(&mut repl, "await new Promise(() => {})"),
        "[promise never settled]"
    );
}

#[test]
fn strings_are_quoted_only_at_the_top_level() {
    let (mut repl, console) = test_repl();
    assert_eq!(done(&mut repl, r#""hi""#), "'hi'");
    assert_eq!(done(&mut repl, r#"console.log("hi")"#), "undefined");
    assert_eq!(console.text(), "hi\n");
}

#[test]
fn dot_commands() {
    let (mut repl, _console) = test_repl();
    assert!(done(&mut repl, ".help").contains(".exit"));
    assert!(done(&mut repl, ".nope").contains("unknown command"));
    assert_eq!(repl.feed(".exit"), Step::Exit);
}

#[test]
fn dot_exit_inside_continuation_is_code_not_command() {
    let (mut repl, _console) = test_repl();
    assert_eq!(repl.feed("const s = `"), Step::More);
    assert_eq!(repl.feed(".exit"), Step::More); // template literal line, not a command
    assert_eq!(done(&mut repl, "`"), "undefined");
    assert_eq!(done(&mut repl, "s.includes('.exit')"), "true");
}

#[test]
fn run_driver_end_to_end() {
    let (repl, console) = test_repl();
    let script = b"1 + 1\nsetTimeout(() => console.log('later'), 5)\n.exit\n" as &[u8];
    let mut input = std::io::BufReader::new(script);
    let mut out = Vec::new();
    repl.run(&mut input, &mut out);
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("> 2\n"), "prompt + result: {out:?}");
    assert_eq!(
        console.text(),
        "later\n",
        "timer settled before next prompt"
    );
}
