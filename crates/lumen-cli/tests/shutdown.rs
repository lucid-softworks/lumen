use std::process::Command;

#[test]
fn normal_exit_finishes_pending_work_and_flushes_partial_output() {
    let dir = std::env::temp_dir().join(format!("lumen-shutdown-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("entry.cjs");
    std::fs::write(
        &script,
        r#"
          const fs = require("node:fs/promises");
          process.stderr.write("pending");
          setTimeout(() => {
            fs.writeFile(__dirname + "/result.txt", "finished").then(() => {
              process.stdout.write("x".repeat(1024 * 1024));
            });
          }, 1);
        "#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg(&script)
        .output()
        .expect("run shutdown fixture");
    let written = std::fs::read_to_string(dir.join("result.txt"));
    let _ = std::fs::remove_dir_all(&dir);

    assert!(output.status.success(), "{:?}", output);
    assert_eq!(written.unwrap(), "finished");
    assert_eq!(output.stdout.len(), 1024 * 1024);
    assert!(output.stdout.iter().all(|byte| *byte == b'x'));
    assert_eq!(output.stderr, b"pending");
}
