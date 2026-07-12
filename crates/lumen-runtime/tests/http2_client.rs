use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
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
fn client_multiplexes_requests_against_node_server() {
    if Command::new("node").arg("--version").output().is_err() { return; }
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let server_source = format!(r#"
        const http2 = require("node:http2");
        const server = http2.createServer();
        server.on("stream", (stream, headers) => {{
          let body = "";
          stream.on("data", chunk => body += chunk);
          stream.on("end", () => {{
            stream.respond({{ ":status": 200, "x-path": headers[":path"] }});
            stream.end(body ? body.toUpperCase() : headers[":path"]);
          }});
        }});
        server.listen({port}, "127.0.0.1", () => console.log("ready"));
        server.on("session", session => session.on("close", () => server.close()));
    "#);
    let mut child = Command::new("node")
        .args(["-e", &server_source])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "ready");

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = format!(r#"
        const http2 = require("node:http2");
        const client = http2.connect("http://127.0.0.1:{port}");
        let completed = 0;
        function request(headers, body) {{
          const req = client.request(headers);
          let data = "";
          req.on("response", response => console.log("response", response[":status"], response["x-path"]));
          req.on("data", chunk => data += chunk);
          req.on("end", () => {{
            console.log("body", data);
            if (++completed === 2) client.ping(Buffer.from("12345678"), (error, duration, payload) => {{
              console.log("ping", !error, typeof duration, payload.toString());
              client.close();
            }});
          }});
          req.end(body);
        }}
        request({{ ":method": "GET", ":path": "/first" }}, "");
        request({{ ":method": "POST", ":path": "/second" }}, "hello");
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let output = String::from_utf8(out.0.borrow().clone()).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    let lines: Vec<_> = output.lines().collect();
    assert!(lines.contains(&"response 200 /first"), "{lines:?}");
    assert!(lines.contains(&"response 200 /second"), "{lines:?}");
    assert!(lines.contains(&"body /first"), "{lines:?}");
    assert!(lines.contains(&"body HELLO"), "{lines:?}");
    assert!(lines.contains(&"ping true number 12345678"), "{lines:?}");
}
