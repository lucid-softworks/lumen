use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn plugin_transforms_commonjs_loads() {
    let path = std::env::temp_dir().join(format!("lumen-plugin-{}.js", std::process::id()));
    let redirected = std::env::temp_dir().join(format!("lumen-redirected-{}.js", std::process::id()));
    std::fs::write(&path, "module.exports = 'original';").unwrap();
    std::fs::write(&redirected, "module.exports = 'redirected';").unwrap();
    let mut runtime = Runtime::new(); let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut { out: Box::new(out.clone()), err: Box::new(Captured::default()) });
    let source = format!(r#"
      Bun.plugin({{ name: "test", setup(build) {{
        build.onLoad({{ filter: /lumen-plugin-/ }}, args => ({{ contents: `module.exports = "loaded:${{args.namespace}}"`, loader: "js" }}));
        build.onResolve({{ filter: /^virtual-target$/ }}, () => ({{ path: {redirected:?} }}));
      }} }});
      console.log(require({path:?}));
      console.log(require("virtual-target"));
    "#, path = path.to_string_lossy(), redirected = redirected.to_string_lossy());
    match runtime.eval(&source).unwrap() { Completion::Value(_) => {}, Completion::Throw { name, message } => panic!("uncaught {name}: {message}") }
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(redirected);
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().trim(), "loaded:file\nredirected");
}
