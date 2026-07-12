use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use lumen_runtime::{Completion, ConsoleOut, Runtime};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

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
fn filesystem_router_matches_nextjs_pages_and_reloads() {
    let dir = std::env::temp_dir().join(format!(
        "lumen-router-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(dir.join("blog")).unwrap();
    for file in ["index.tsx", "settings.tsx", "blog/index.tsx", "blog/[slug].tsx", "[...all].tsx", "[[...optional]].tsx"] {
        std::fs::write(dir.join(file), "").unwrap();
    }

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let canonical_dir = std::fs::canonicalize(&dir).unwrap();
    let directory = canonical_dir.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let source = format!(
        r#"
        globalThis.router = new Bun.FileSystemRouter({{
            style: "nextjs", dir: "{directory}", origin: "https://example.test", assetPrefix: "/assets/"
        }});
        for (const input of ["/", "/blog", "/blog/hello?q=a%20b&q=c", "/one/two"]) {{
            const route = router.match(input);
            console.log("route", route.kind, route.name, JSON.stringify(route.params), JSON.stringify(route.query), route.src);
        }}
        console.log("before", router.match("/new").name);
        "#
    );
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    std::fs::write(dir.join("new.tsx"), "").unwrap();
    match runtime.eval("router.reload(); console.log('after', router.match('/new').name, Object.keys(router.routes).includes('/new'));").expect("reload source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    let lines: Vec<_> = String::from_utf8(out.0.borrow().clone()).unwrap().lines().map(str::to_string).collect();
    assert_eq!(lines[0], "route exact / {} {} https://example.test/assets/index.tsx");
    assert_eq!(lines[1], "route exact /blog {} {} https://example.test/assets/blog/index.tsx");
    assert_eq!(lines[2], "route dynamic /blog/[slug] {\"slug\":\"hello\"} {\"q\":[\"a b\",\"c\"],\"slug\":\"hello\"} https://example.test/assets/blog/[slug].tsx");
    assert_eq!(lines[3], "route catch-all /[...all] {\"all\":\"one/two\"} {\"all\":\"one/two\"} https://example.test/assets/[...all].tsx");
    assert_eq!(lines[4..], ["before /[...all]", "after /new true"]);
}
