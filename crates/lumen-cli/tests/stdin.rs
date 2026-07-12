use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn process_stdin_streams_piped_input() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .args([
            "-e",
            r#"const chunks=[]; process.stdin.on("data", c => chunks.push(c)); process.stdin.on("end", () => console.log(Buffer.concat(chunks).toString()))"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn lumen-cli");

    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"streamed input")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for lumen-cli");

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "streamed input\n");
}
