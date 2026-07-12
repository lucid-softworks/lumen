use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::rc::Rc;
use std::process::{Command, Stdio};

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

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn run(source: &str) -> Vec<String> {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    out.lines()
}

#[test]
fn redis_client_pipelines_and_decodes_resp_values() {
    let lines = run(
        r#"
        const net = require("node:net");
        let requests = "";
        let replied = false;
        const server = net.createServer(socket => {
            socket.on("data", chunk => {
                requests += chunk.toString();
                if (replied || !requests.includes("LRANGE")) return;
                replied = true;
                socket.write("+PONG\r\n:42\r\n$-1\r\n*2\r\n$3\r\none\r\n");
                setTimeout(() => socket.write("$3\r\ntwo\r\n"), 5);
            });
        });
        server.listen(0, "127.0.0.1", async () => {
            const redis = new Bun.RedisClient(`redis://127.0.0.1:${server.address().port}`);
            redis.onconnect = () => console.log("connected", redis.connected);
            const values = await Promise.all([
                redis.send("PING", ["x"]),
                redis.send("INCR", ["n"]),
                redis.send("GET", ["missing"]),
                redis.send("LRANGE", ["items"]),
            ]);
            console.log("values", JSON.stringify(values));
            redis.onclose = () => { console.log("closed", redis.connected); server.close(); };
            redis.close();
        });
        "#,
    );
    assert_eq!(
        lines,
        [
            "connected true",
            "values [\"PONG\",42,null,[\"one\",\"two\"]]",
            "closed false",
        ]
    );
}

#[test]
fn redis_client_preserves_binary_bulk_strings_and_rejects_errors() {
    let lines = run(
        r#"
        const net = require("node:net");
        let count = 0;
        const server = net.createServer(socket => socket.on("data", () => {
            count++;
            socket.write(count === 1 ? Buffer.from([36, 51, 13, 10, 0, 255, 65, 13, 10]) : "-WRONGTYPE bad value\r\n");
        }));
        server.listen(0, "127.0.0.1", async () => {
            const redis = new Bun.RedisClient(`redis://127.0.0.1:${server.address().port}`);
            const value = await redis.getBuffer("binary");
            console.log("binary", Buffer.isBuffer(value), value.length, value[0], value[1], value[2]);
            try { await redis.send("GET", ["bad"]); }
            catch (error) { console.log("error", error.code, error.message); }
            redis.close();
            server.close();
        });
        "#,
    );
    assert_eq!(
        lines,
        ["binary true 3 0 255 65", "error WRONGTYPE WRONGTYPE bad value"]
    );
}

#[test]
fn redis_convenience_methods_encode_arguments_and_convert_results() {
    let lines = run(
        r#"
        const net = require("node:net");
        let input = "";
        const server = net.createServer(socket => socket.on("data", chunk => {
            input += chunk.toString();
            if (!input.includes("HGETALL")) return;
            socket.write(":1\r\n:0\r\n*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n");
        }));
        server.listen(0, "127.0.0.1", async () => {
            const redis = new Bun.RedisClient(`redis://127.0.0.1:${server.address().port}`);
            const values = await Promise.all([
                redis.exists("present"), redis.sismember("set", "absent"), redis.hgetall("hash")
            ]);
            console.log("converted", JSON.stringify(values));
            console.log("encoded", input.includes("EXISTS"), input.includes("SISMEMBER"), input.includes("HGETALL"));
            redis.close();
            server.close();
        });
        "#,
    );
    assert_eq!(
        lines,
        ["converted [true,false,{\"a\":\"1\",\"b\":\"2\"}]", "encoded true true true"]
    );
}

#[test]
fn redis_client_connects_over_tls() {
    if Command::new("openssl").arg("version").output().is_err()
        || Command::new("node").arg("--version").output().is_err()
    {
        return;
    }
    let directory = std::env::temp_dir().join(format!("lumen-redis-tls-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let generated = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "1", "-nodes"])
        .arg("-keyout").arg(&key).arg("-out").arg(&certificate)
        .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost"])
        .output().unwrap();
    assert!(generated.status.success());
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let server_source = format!(r#"
      const fs = require("node:fs"), tls = require("node:tls");
      const server = tls.createServer({{
        cert: fs.readFileSync({certificate:?}), key: fs.readFileSync({key:?})
      }}, socket => socket.once("data", () => socket.end("+PONG\r\n")));
      server.listen({port}, "127.0.0.1", () => console.log("ready"));
      server.on("secureConnection", socket => socket.on("close", () => server.close()));
    "#);
    let mut server = Command::new("node").args(["-e", &server_source])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut ready = String::new();
    BufReader::new(server.stdout.take().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "ready");
    let lines = run(&format!(r#"
        (async () => {{
          const redis = new Bun.RedisClient("rediss://localhost:{port}", {{ tls: {{ rejectUnauthorized: false }} }});
          console.log("secure", await redis.ping());
          redis.close();
        }})();
    "#));
    let status = server.wait().unwrap();
    let _ = std::fs::remove_dir_all(directory);
    assert!(status.success());
    assert_eq!(lines, ["secure PONG"]);
}
