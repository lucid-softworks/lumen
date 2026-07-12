use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn runs_commonjs_typescript_entry_and_dependency() {
    let dir = std::env::temp_dir().join(format!(
        "lumen-typescript-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("value.ts"),
        "const value: number = 41; module.exports = value;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.cts"),
        "const value: number = require('./value'); console.log(value + 1);\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg(dir.join("main.cts"))
        .output()
        .expect("run TypeScript entry");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "42");
}
