use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
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
fn bun_tcp_listen_and_connect_echo() {
    let lines = run(
        r#"
        const server = Bun.listen({
            hostname: "127.0.0.1",
            port: 0,
            data: { side: "server" },
            socket: {
                open(socket) { console.log("server-open", socket.data.side); },
                data(socket, data) { socket.end(data); },
            },
        });
        server.once("listening", async () => {
            const client = await Bun.connect({
                hostname: "127.0.0.1",
                port: server.port,
                data: { side: "client" },
                socket: {
                    open(socket) {
                        console.log("client-open", socket.data.side);
                        socket.write("ping");
                    },
                    data(socket, data) {
                        console.log("tcp", data.toString());
                    },
                    end() { server.stop(); },
                },
            });
            console.log("address", client.remoteAddress, server.hostname, server.port > 0);
        });
        "#,
    );
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines.contains(&"server-open server".to_string()), "{lines:?}");
    assert!(lines.contains(&"client-open client".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"address 127.0.0.1 127.0.0.1 true".to_string()),
        "{lines:?}"
    );
    assert_eq!(lines.last().map(String::as_str), Some("tcp ping"));
}

#[test]
fn bun_udp_socket_sends_data_with_peer_metadata() {
    let lines = run(
        r#"
        (async () => {
            const receiver = await Bun.udpSocket({
                hostname: "127.0.0.1",
                port: 0,
                data: "receiver",
                socket: {
                    data(socket, data, port, address) {
                        console.log("udp", socket.data, data.toString(), port > 0, address);
                        socket.close();
                        sender.close();
                    },
                },
            });
            globalThis.sender = await Bun.udpSocket({ hostname: "127.0.0.1", port: 0 });
            console.log("sent", sender.send("pong", receiver.port, receiver.hostname));
        })();
        "#,
    );
    assert_eq!(
        lines,
        ["sent 4", "udp receiver pong true 127.0.0.1"]
    );
}
