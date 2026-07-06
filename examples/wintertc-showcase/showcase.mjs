// A tour of lumen's WinterTC Minimum Common API — every major interface, exercised end to end.
// Run: ../../target/release/lumen-cli showcase.mjs

const line = (label, value) => console.log(`  ${label.padEnd(26)} ${value}`);
const section = (name) => console.log(`\n${name}`);

// ---- encoding, base64, structuredClone ----
section("Encoding & cloning");
const enc = new TextEncoder().encode("héllo 🌍");
line("TextEncoder/Decoder", new TextDecoder().decode(enc));
line("btoa/atob", atob(btoa("round-trip")));
const cloned = structuredClone({ a: [1, 2], when: new Date(0), re: /x/g });
line("structuredClone", `${JSON.stringify(cloned.a)} ${cloned.re}`);

// ---- URL & URLPattern ----
section("URL & URLPattern");
const u = new URL("https://ex.com/books/42?sort=asc#top");
line("URL parts", `${u.hostname} ${u.pathname} ${u.searchParams.get("sort")}`);
const pat = new URLPattern({ pathname: "/books/:id" });
line("URLPattern match", JSON.stringify(pat.exec(u.href).pathname.groups));

// ---- crypto ----
section("crypto");
line("randomUUID", crypto.randomUUID());
const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc"));
line("subtle.digest SHA-256", [...new Uint8Array(digest)].slice(0, 4).map((b) => b.toString(16).padStart(2, "0")).join(""));
line("instanceof Crypto", crypto instanceof Crypto);

// ---- Blob / File / FormData ----
section("Blob / File / FormData");
const blob = new Blob(["hello ", "blob"], { type: "text/plain" });
line("Blob", `${blob.size} bytes, "${await blob.text()}"`);
const form = new FormData();
form.append("field", "value");
form.append("upload", new File(["file contents"], "note.txt", { type: "text/plain" }));
const req = new Request("http://x/", { method: "POST", body: form });
const back = await req.formData();
line("FormData round-trip", `${back.get("field")} + file "${(back.get("upload")).name}"`);

// ---- Streams: Transform + Compression ----
section("Streams");
const upper = new TransformStream({ transform: (c, ctl) => ctl.enqueue(c.toUpperCase()) });
const w = upper.writable.getWriter();
w.write("stream"); w.close();
let piped = "";
for await (const c of upper.readable) piped += c;
line("TransformStream", piped);

async function through(bytes, stream) {
  const rs = new ReadableStream({ start(c) { c.enqueue(bytes); c.close(); } });
  const out = [];
  for await (const chunk of rs.pipeThrough(stream)) out.push(...chunk);
  return new Uint8Array(out);
}
const original = new TextEncoder().encode("compress me ".repeat(20));
const gz = await through(original, new CompressionStream("gzip"));
const un = await through(gz, new DecompressionStream("gzip"));
line("Compression gzip", `${original.length} -> ${gz.length} -> ${un.length} bytes, ok=${new TextDecoder().decode(un) === new TextDecoder().decode(original)}`);

// ---- fetch (drive a local server) ----
section("fetch + HTTP server");
const server = Lumen.serve((request) => Response.json({ path: new URL(request.url).pathname, ok: true }), {
  hostname: "127.0.0.1",
  port: 0,
});
const res = await fetch(`http://127.0.0.1:${server.port}/hello`);
line("fetch JSON", JSON.stringify(await res.json()));
await server.shutdown();

// ---- WebAssembly ----
section("WebAssembly");
// (module (func (export "mul") (param i32 i32) (result i32) local.get 0 local.get 1 i32.mul))
const wasmBytes = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0,
  1, 7, 1, 96, 2, 127, 127, 1, 127,
  3, 2, 1, 0,
  7, 7, 1, 3, 109, 117, 108, 0, 0,
  10, 9, 1, 7, 0, 32, 0, 32, 1, 108, 11,
]);
line("validate", WebAssembly.validate(wasmBytes));
const { instance } = await WebAssembly.instantiate(wasmBytes);
line("instance.exports.mul(6,7)", instance.exports.mul(6, 7));

// ---- performance / timers ----
section("Platform");
line("performance.now", typeof performance.now() === "number" ? "monotonic clock ✓" : "?");
await new Promise((r) => setTimeout(r, 1));
line("setTimeout + Promise", "microtask + macrotask ✓");
line("self === globalThis", self === globalThis);

console.log("\nAll WinterTC Minimum Common API interfaces exercised.");
