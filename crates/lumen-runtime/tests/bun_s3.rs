use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[test]
fn s3_presign_matches_sigv4_vector_and_validates_credentials() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
        const client = new Bun.S3Client({
          accessKeyId: "AKIDEXAMPLE",
          secretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
          bucket: "examplebucket", region: "us-east-1", endpoint: "https://s3.amazonaws.com"
        });
        const url = client.presign("test.txt", { expiresIn: 86400, date: new Date("2013-05-24T00:00:00Z") });
        const parsed = new URL(url);
        console.log("path", parsed.pathname);
        console.log("credential", parsed.searchParams.get("X-Amz-Credential"));
        console.log("signature", parsed.searchParams.get("X-Amz-Signature"));
        console.log("file", client.file("test.txt") instanceof Blob, client.file("test.txt").presign({ date: new Date("2013-05-24T00:00:00Z") }).includes("X-Amz-Signature="));
        try { new Bun.S3Client({ bucket: "missing" }); }
        catch (error) { console.log("error", error.code); }
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        [
            "path /examplebucket/test.txt",
            "credential AKIDEXAMPLE/20130524/us-east-1/s3/aws4_request",
            "signature 164d9c8fcc0c651340299e87fdf165894f9fd545952e24c3748d91df99323786",
            "file true true",
            "error ERR_S3_MISSING_CREDENTIALS",
        ]
    );
}

#[test]
fn s3_client_executes_signed_object_lifecycle() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()), err: Box::new(Captured::default()),
    });
    let source = r#"
        const objects = new Map();
        let signed = true;
        const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
          signed = signed && request.headers.get("authorization").startsWith("AWS4-HMAC-SHA256 ");
          const url = new URL(request.url), key = url.pathname;
          if (url.searchParams.get("list-type") === "2") {
            signed = signed && url.searchParams.get("prefix") === "folder/" && url.searchParams.get("max-keys") === "2";
            return new Response(`<?xml version="1.0"?><ListBucketResult><Name>bucket</Name><Prefix>folder/</Prefix><MaxKeys>2</MaxKeys><KeyCount>1</KeyCount><IsTruncated>true</IsTruncated><NextContinuationToken>next</NextContinuationToken><Contents><Key>folder/a&amp;b.txt</Key><LastModified>2025-01-07T00:19:10Z</LastModified><ETag>&quot;list-etag&quot;</ETag><Size>7</Size><StorageClass>STANDARD</StorageClass><Owner><ID>owner-id</ID><DisplayName>Owner</DisplayName></Owner></Contents><CommonPrefixes><Prefix>folder/sub/</Prefix></CommonPrefixes></ListBucketResult>`, { status: 200 });
          }
          if (request.method === "PUT") { objects.set(key, await request.text()); return new Response("", { status: 200 }); }
          if (request.method === "DELETE") { objects.delete(key); return new Response("", { status: 204 }); }
          if (!objects.has(key)) return new Response("", { status: 404 });
          const value = objects.get(key), headers = { "content-length": String(Buffer.byteLength(value)), "content-type": "text/plain", etag: '"etag"', "last-modified": "Tue, 07 Jan 2025 00:19:10 GMT" };
          return new Response(request.method === "HEAD" ? null : value, { status: 200, headers });
        }});
        (async () => {
          const client = new Bun.S3Client({ accessKeyId: "key", secretAccessKey: "secret", bucket: "bucket", region: "us-east-1", endpoint: server.url.href });
          const file = client.file("folder/item.txt");
          console.log("write", await Bun.write(file, "hello"), file instanceof Bun.S3File);
          console.log("exists", await client.exists("folder/item.txt"));
          const stat = await client.stat("folder/item.txt");
          console.log("stat", stat.size, stat.etag, stat.type);
          const listed = await client.list({ prefix: "folder/", maxKeys: 2, fetchOwner: true });
          console.log("list", listed.name, listed.isTruncated, listed.nextContinuationToken, listed.contents[0].key, listed.contents[0].owner.displayName, listed.commonPrefixes[0].prefix);
          console.log("read", await file.text());
          await client.delete("folder/item.txt");
          console.log("deleted", await client.exists("folder/item.txt"), signed);
          server.stop();
        })();
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        ["write 5 true", "exists true", "stat 5 \"etag\" text/plain", "list bucket true next folder/a&b.txt Owner folder/sub/", "read hello", "deleted false true"]
    );
}
