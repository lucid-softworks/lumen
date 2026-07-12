#![cfg(unix)]

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
fn ownership_calls_use_real_path_and_descriptor_syscalls() {
    let directory = std::env::temp_dir().join(format!("lumen-fs-owner-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("file");
    let link = directory.join("link");
    std::fs::write(&file, b"data").unwrap();
    std::os::unix::fs::symlink(&file, &link).unwrap();

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = format!(r#"
        const fs = require("node:fs");
        const owner = fs.statSync({file:?});
        fs.chownSync({file:?}, owner.uid, owner.gid);
        const linkOwner = fs.lstatSync({link:?});
        fs.lchownSync({link:?}, linkOwner.uid, linkOwner.gid);
        const fd = fs.openSync({file:?}, "r+");
        fs.fchownSync(fd, owner.uid, owner.gid);
        fs.closeSync(fd);
        console.log("sync", fs.statSync({file:?}).uid === owner.uid);
        fs.chown({file:?}, owner.uid, owner.gid, error => console.log("callback", !error));
        fs.promises.lchown({link:?}, linkOwner.uid, linkOwner.gid).then(() => console.log("promise", true));
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}; output={:?}", String::from_utf8_lossy(&out.0.borrow())),
    }
    let _ = std::fs::remove_dir_all(directory);
    let lines: Vec<_> = String::from_utf8(out.0.borrow().clone()).unwrap().lines().map(str::to_string).collect();
    assert_eq!(lines.first().map(String::as_str), Some("sync true"));
    assert!(lines.contains(&"callback true".to_string()), "{lines:?}");
    assert!(lines.contains(&"promise true".to_string()), "{lines:?}");
}
