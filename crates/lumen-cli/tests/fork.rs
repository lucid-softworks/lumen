use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn child_process_fork_exchanges_messages() {
    let dir = std::env::temp_dir().join(format!(
        "lumen-fork-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let child = dir.join("child.js");
    let parent = dir.join("parent.js");

    std::fs::write(
        &child,
        r#"
          console.log("ordinary child output");
          process.on("message", message => process.send({ value: message.value + 1 }));
        "#,
    )
    .unwrap();
    std::fs::write(
        &parent,
        r#"
          const { fork } = require("node:child_process");
          const child = fork(process.argv[2], [], { silent: true });
          let stdout = "";
          child.stdout.on("data", chunk => { stdout += chunk; });
          child.on("spawn", () => child.send({ value: 41 }));
          child.on("message", message => {
            console.log("message", message.value);
            child.disconnect();
          });
          child.on("close", code => console.log("close", code, stdout.trim()));
        "#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg(&parent)
        .arg(&child)
        .output()
        .expect("run fork parent");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().lines().collect::<Vec<_>>(),
        ["message 42", "close 0 ordinary child output"]
    );
}
