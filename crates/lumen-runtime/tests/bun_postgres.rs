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

fn message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5]; stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    let mut payload = vec![0; length - 4]; stream.read_exact(&mut payload).unwrap();
    (header[0], payload)
}
fn send(stream: &mut impl Write, kind: u8, payload: &[u8]) {
    stream.write_all(&[kind]).unwrap();
    stream.write_all(&((payload.len() + 4) as u32).to_be_bytes()).unwrap();
    stream.write_all(payload).unwrap();
}
fn cstr(value: &str) -> Vec<u8> { let mut bytes = value.as_bytes().to_vec(); bytes.push(0); bytes }

#[test]
fn postgres_adapter_uses_extended_protocol_and_decodes_rows() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let mut accepted = None;
        for _ in 0..500 {
            match listener.accept() { Ok(value) => { accepted = Some(value); break; }, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(10)), Err(error) => panic!("accept: {error}") }
        }
        let (mut stream, _) = accepted.expect("postgres client did not connect");
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        let mut length = [0u8; 4]; stream.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        let mut startup = vec![0; length - 4]; stream.read_exact(&mut startup).unwrap();
        assert!(startup.windows(9).any(|value| value == b"user\0user"));
        send(&mut stream, b'R', &3u32.to_be_bytes());
        let (kind, password) = message(&mut stream);
        assert_eq!(kind, b'p'); assert_eq!(&password[..password.len() - 1], b"password");
        send(&mut stream, b'R', &0u32.to_be_bytes());
        send(&mut stream, b'Z', b"I");

        let mut query = String::new(); let mut params = Vec::new();
        loop {
            let (kind, payload) = message(&mut stream);
            if kind == b'P' {
                let start = 1; let end = payload[start..].iter().position(|byte| *byte == 0).unwrap() + start;
                query = String::from_utf8(payload[start..end].to_vec()).unwrap();
            } else if kind == b'B' {
                let mut offset = 2;
                let formats = u16::from_be_bytes(payload[offset..offset + 2].try_into().unwrap()) as usize; offset += 2 + formats * 2;
                let count = u16::from_be_bytes(payload[offset..offset + 2].try_into().unwrap()) as usize; offset += 2;
                for _ in 0..count {
                    let size = i32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()); offset += 4;
                    if size < 0 { params.push(None); } else { params.push(Some(String::from_utf8(payload[offset..offset + size as usize].to_vec()).unwrap())); offset += size as usize; }
                }
            } else if kind == b'S' { break; }
        }
        assert_eq!(query, "SELECT $1::int4 AS id, $2::text AS name, $3::bool AS active");
        assert_eq!(params, [Some("42".into()), Some("Alice".into()), Some("true".into())]);
        send(&mut stream, b'1', &[]); send(&mut stream, b'2', &[]);
        let mut description = Vec::new(); description.extend_from_slice(&3u16.to_be_bytes());
        for (name, oid, size) in [("id", 23u32, 4i16), ("name", 25, -1), ("active", 16, 1)] {
            description.extend(cstr(name)); description.extend_from_slice(&0u32.to_be_bytes()); description.extend_from_slice(&0u16.to_be_bytes());
            description.extend_from_slice(&oid.to_be_bytes()); description.extend_from_slice(&size.to_be_bytes()); description.extend_from_slice(&(-1i32).to_be_bytes()); description.extend_from_slice(&0u16.to_be_bytes());
        }
        send(&mut stream, b'T', &description);
        let mut row = Vec::new(); row.extend_from_slice(&3u16.to_be_bytes());
        for value in [b"42".as_slice(), b"Alice", b"t"] { row.extend_from_slice(&(value.len() as u32).to_be_bytes()); row.extend_from_slice(value); }
        send(&mut stream, b'D', &row); send(&mut stream, b'C', b"SELECT 1\0"); send(&mut stream, b'Z', b"I");
        assert_eq!(message(&mut stream).0, b'X');
    });

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut { out: Box::new(out.clone()), err: Box::new(Captured::default()) });
    let source = format!(r#"
      (async () => {{
        const sql = Bun.SQL("postgres://user:password@127.0.0.1:{port}/database?sslmode=disable");
        const rows = await sql`SELECT ${{42}}::int4 AS id, ${{"Alice"}}::text AS name, ${{true}}::bool AS active`;
        console.log("row", JSON.stringify(rows[0]), rows.command, rows.count);
        await sql.close();
      }})();
    "#);
    match runtime.eval(&source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    peer.join().unwrap();
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().trim(), "row {\"id\":42,\"name\":\"Alice\",\"active\":true} SELECT 1");
}

#[test]
fn postgres_adapter_authenticates_with_scram_sha_256() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut length = [0u8; 4]; stream.read_exact(&mut length).unwrap();
        let mut startup = vec![0; u32::from_be_bytes(length) as usize - 4]; stream.read_exact(&mut startup).unwrap();
        let mut sasl = 10u32.to_be_bytes().to_vec(); sasl.extend(b"SCRAM-SHA-256\0\0"); send(&mut stream, b'R', &sasl);
        let (kind, first) = message(&mut stream); assert_eq!(kind, b'p');
        let mechanism_end = first.iter().position(|byte| *byte == 0).unwrap(); assert_eq!(&first[..mechanism_end], b"SCRAM-SHA-256");
        let first_message = String::from_utf8(first[mechanism_end + 5..].to_vec()).unwrap();
        assert_eq!(first_message, "n,,n=user,r=fixed-client-nonce");
        let server_first = "r=fixed-client-nonceserver,s=c2NyYW0tdGVzdC1zYWx0,i=1";
        let mut continuation = 11u32.to_be_bytes().to_vec(); continuation.extend(server_first.as_bytes()); send(&mut stream, b'R', &continuation);
        let (kind, final_message) = message(&mut stream); assert_eq!(kind, b'p');
        assert_eq!(String::from_utf8(final_message).unwrap(), "c=biws,r=fixed-client-nonceserver,p=NiA39R0NbjqSoWjdqDtWyuNcv/Umenu8XmqVHCo1RBo=");
        let mut final_auth = 12u32.to_be_bytes().to_vec(); final_auth.extend(b"v=/znrhjluposUbXiPmX2gG7TFu1ft/gF/hw7xhWY+Orw=");
        send(&mut stream, b'R', &final_auth); send(&mut stream, b'R', &0u32.to_be_bytes()); send(&mut stream, b'Z', b"I");
        loop { if message(&mut stream).0 == b'S' { break; } }
        send(&mut stream, b'C', b"SELECT 0\0"); send(&mut stream, b'Z', b"I");
        assert_eq!(message(&mut stream).0, b'X');
    });
    let mut runtime = Runtime::new(); let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut { out: Box::new(out.clone()), err: Box::new(Captured::default()) });
    let source = format!(r#"
      (async () => {{
        require("node:crypto").randomBytes = () => ({{ toString: () => "fixed-client-nonce" }});
        const sql = Bun.SQL("postgres://user:pencil@127.0.0.1:{port}/database?sslmode=disable");
        const rows = await sql`SELECT 1`;
        console.log("scram", rows.command, rows.count);
        await sql.close();
      }})();
    "#);
    match runtime.eval(&source).unwrap() { Completion::Value(_) => {}, Completion::Throw { name, message } => panic!("uncaught {name}: {message}") }
    peer.join().unwrap();
    assert_eq!(String::from_utf8(out.0.borrow().clone()).unwrap().trim(), "scram SELECT 0");
}
