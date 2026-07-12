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
fn bun_cookie_and_cookie_map_match_header_semantics() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const cookie = new Bun.Cookie("session=a b; Domain=EXAMPLE.COM; Max-Age=60; HttpOnly; Secure; SameSite=None");
        console.log("cookie", cookie.name, cookie.value, cookie.domain, cookie.path, cookie.maxAge,
                    cookie.secure, cookie.httpOnly, cookie.sameSite, cookie.isExpired());
        console.log("serialized", cookie.toString());
        console.log("expired", new Bun.Cookie({ name: "old", value: "x", maxAge: 0 }).isExpired());

        const map = new Bun.CookieMap("a=1; b=two%20words");
        map.set("c", "three", { httpOnly: true });
        map.set({ name: "d", value: "four", secure: true });
        console.log("map", map.size, map.get("b"), map.has("a"), JSON.stringify([...map]));
        console.log("headers", JSON.stringify(map.toSetCookieHeaders()));
        map.delete("a");
        console.log("json", JSON.stringify(map.toJSON()), map.size);
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
            "cookie session a b example.com / 60 true true none false",
            "serialized session=a%20b; Domain=example.com; Path=/; Max-Age=60; Secure; HttpOnly; SameSite=None",
            "expired true",
            "map 4 two words true [[\"a\",\"1\"],[\"b\",\"two words\"],[\"c\",\"three\"],[\"d\",\"four\"]]",
            "headers [\"c=three; Path=/; HttpOnly; SameSite=Lax\",\"d=four; Path=/; Secure; SameSite=Lax\"]",
            "json {\"b\":\"two words\",\"c\":\"three\",\"d\":\"four\"} 3",
        ]
    );
}
