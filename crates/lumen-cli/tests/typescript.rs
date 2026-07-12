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

#[cfg(unix)]
fn fake_typecheck_project(exit_code: i32) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "lumen-typecheck-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.cts");
    let compiler = dir.join("tsc");
    let log = dir.join("tsc.args");
    std::fs::write(&entry, "const answer: number = 42; console.log(answer);\n").unwrap();
    std::fs::write(
        &compiler,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CHECK_LOG\"\nexit {exit_code}\n"),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&compiler).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&compiler, permissions).unwrap();
    (dir, entry, log)
}

#[cfg(unix)]
#[test]
fn typecheck_runs_compiler_before_script() {
    let (dir, entry, log) = fake_typecheck_project(0);
    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg("--typecheck")
        .arg(&entry)
        .env("LUMEN_TSC", dir.join("tsc"))
        .env("CHECK_LOG", &log)
        .output()
        .expect("run checked TypeScript entry");
    let args = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "42");
    assert_eq!(args.lines().collect::<Vec<_>>(), ["--noEmit", entry.to_string_lossy().as_ref()]);
}

#[cfg(unix)]
#[test]
fn typecheck_failure_prevents_execution() {
    let (dir, entry, log) = fake_typecheck_project(2);
    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg("--typecheck")
        .arg(&entry)
        .env("LUMEN_TSC", dir.join("tsc"))
        .env("CHECK_LOG", &log)
        .output()
        .expect("run rejected TypeScript entry");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "script unexpectedly ran");
    assert!(String::from_utf8_lossy(&output.stderr).contains("TypeScript typecheck failed"));
}
