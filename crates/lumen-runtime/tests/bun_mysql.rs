use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
fn packet(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0; 4]; stream.read_exact(&mut header).unwrap();
    let length = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
    let mut payload = vec![0; length]; stream.read_exact(&mut payload).unwrap(); (header[3], payload)
}
fn send(stream: &mut impl Write, sequence: u8, payload: &[u8]) {
    let length = payload.len(); stream.write_all(&[length as u8, (length >> 8) as u8, (length >> 16) as u8, sequence]).unwrap(); stream.write_all(payload).unwrap();
}
fn nul(value: &str) -> Vec<u8> { let mut out = value.as_bytes().to_vec(); out.push(0); out }
fn field(value: &str) -> Vec<u8> { let mut out = vec![value.len() as u8]; out.extend(value.as_bytes()); out }
fn column(name: &str, kind: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for value in ["def", "database", "users", "users", name, name] { out.extend(field(value)); }
    out.extend([12, 45, 0, 0xff, 0xff, 0xff, 0, kind, 0, 0, 0, 0, 0]); out
}

#[test]
fn mysql_adapter_authenticates_escapes_queries_and_decodes_rows() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut greeting = vec![10]; greeting.extend(nul("8.0.36-lumen")); greeting.extend(1u32.to_le_bytes());
        greeting.extend(b"12345678"); greeting.push(0); greeting.extend(0xffffu16.to_le_bytes()); greeting.push(45);
        greeting.extend(2u16.to_le_bytes()); greeting.extend(0xffffu16.to_le_bytes()); greeting.push(21); greeting.extend([0; 10]);
        greeting.extend(b"abcdefghijklm"); greeting.extend(nul("mysql_native_password")); send(&mut stream, 0, &greeting);
        let (sequence, login) = packet(&mut stream); assert_eq!(sequence, 1);
        assert!(login.windows(5).any(|bytes| bytes == b"user\0")); assert!(login.windows(9).any(|bytes| bytes == b"database\0"));
        send(&mut stream, 2, &[0, 0, 0, 2, 0, 0, 0]);

        let (sequence, query) = packet(&mut stream); assert_eq!(sequence, 0); assert_eq!(query[0], 3);
        assert_eq!(String::from_utf8(query[1..].to_vec()).unwrap(), "SELECT 42 AS id, 'O\\'Brien' AS name");
        send(&mut stream, 1, &[2]); send(&mut stream, 2, &column("id", 3)); send(&mut stream, 3, &column("name", 253));
        send(&mut stream, 4, &[0xfe, 0, 0, 2, 0]); send(&mut stream, 5, &[2, b'4', b'2', 5, b'A', b'l', b'i', b'c', b'e']);
        send(&mut stream, 6, &[0xfe, 0, 0, 2, 0]);
        let (_, query) = packet(&mut stream); assert_eq!(String::from_utf8(query[1..].to_vec()).unwrap(), "UPDATE users SET name = 'Bob' WHERE note = '?' AND id = 42");
        send(&mut stream, 1, &[0, 1, 0, 2, 0, 0, 0]);
        assert_eq!(packet(&mut stream).1, [1]);
    });
    let mut runtime = Runtime::new(); let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut { out: Box::new(out.clone()), err: Box::new(Captured::default()) });
    let source = format!(r#"
      (async () => {{
        const sql = Bun.SQL("mysql://user:password@127.0.0.1:{port}/database");
        const rows = await sql`SELECT ${{42}} AS id, ${{"O'Brien"}} AS name`;
        console.log(sql.options.adapter, JSON.stringify(rows[0]), rows.command, rows.count);
        const changed = await sql.unsafe("UPDATE users SET name = ? WHERE note = '?' AND id = ?", ["Bob", 42]);
        console.log("changed", changed.count);
        await sql.close();
      }})();
    "#);
    match runtime.eval(&source).unwrap() { Completion::Value(_) => {}, Completion::Throw { name, message } => panic!("uncaught {name}: {message}") }
    peer.join().unwrap();
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().trim(), "mysql {\"id\":42,\"name\":\"Alice\"} SELECT 1\nchanged 1");
}
