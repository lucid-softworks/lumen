// The `bun` module and the `Bun` global (same object identity: require("bun") === globalThis.Bun).
// Parity target is Bun v1.2.21's require("bun") surface. Everything backable by lumen's existing
// node: glue (zlib/crypto/child_process/fs/url/util/dns) or the WHATWG globals is implemented for
// real; anything that needs a native client lumen doesn't have (postgres, s3) is an honest
// throwing stub rather than a wrong answer.
//
// This block is wrapped (build.rs wrap:true) so its top-level names stay private. It loads right
// before module.js so require("bun") resolves through CORE, and can reach the sibling builtins via
// __builtins.get(...) lazily (module.js's Module is only registered after this file runs).

const proc = globalThis.process;
const nodeZlib = __builtins.get("zlib");
const nodeCrypto = __builtins.get("crypto");
const nodeChild = __builtins.get("child_process");
const nodeFs = __builtins.get("fs");
const nodeUrl = __builtins.get("url");
const nodeUtil = __builtins.get("util");
const nodePath = __builtins.get("path");
const nodeDns = __builtins.get("dns");
const nodeNet = __builtins.get("net");
const nodeDgram = __builtins.get("dgram");

// ---- byte helpers -----------------------------------------------------------------------------
function toU8(x) {
  if (x == null) return new Uint8Array(0);
  if (typeof x === "string") return Buffer.from(x, "utf8");
  if (x instanceof ArrayBuffer) return new Uint8Array(x);
  if (ArrayBuffer.isView(x)) return new Uint8Array(x.buffer, x.byteOffset, x.byteLength);
  return Buffer.from(String(x), "utf8");
}
const notImpl = (what) => () => {
  throw new Error(`${what} is not supported in lumen`);
};

// ---- identity / environment -------------------------------------------------------------------
const version = "1.2.21";
// The upstream git revision of the Bun we emulate. Fixed string (lumen is not built from Bun).
const revision = "7c45ed97def1264717a1b55ab0f789f2ea58f986";

// ---- timing -----------------------------------------------------------------------------------
const START_NS = proc.hrtime.bigint();
function nanoseconds() {
  return Number(proc.hrtime.bigint() - START_NS);
}
function sleep(ms) {
  if (typeof ms === "object" && ms instanceof Date) ms = ms.getTime() - Date.now();
  return new Promise((r) => setTimeout(r, Math.max(0, Number(ms) || 0)));
}
function sleepSync(ms) {
  const end = Date.now() + Math.max(0, Number(ms) || 0);
  while (Date.now() < end) { /* busy-wait: Bun.sleepSync blocks the thread by contract */ }
}

// ---- escapeHTML / stripANSI / stringWidth -----------------------------------------------------
function escapeHTML(input) {
  const s = typeof input === "string" ? input : String(input);
  let out = "";
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    out += c === "&" ? "&amp;"
      : c === "<" ? "&lt;"
      : c === ">" ? "&gt;"
      : c === '"' ? "&quot;"
      : c === "'" ? "&#x27;"
      : c;
  }
  return out;
}

// The canonical ansi-regex (matches CSI/OSC control sequences).
const ANSI_RE = /[\u001B\u009B][[\]()#;?]*(?:(?:(?:[a-zA-Z\d]*(?:;[-a-zA-Z\d\/#&.:=?%@~_]*)*)?\u0007)|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-ntqry=><~]))/g;
function stripANSI(s) {
  return String(s).replace(ANSI_RE, "");
}

const WIDE_RANGES = [
  [0x1100, 0x115f], [0x2329, 0x232a], [0x2e80, 0x303e], [0x3041, 0x33ff], [0x3400, 0x4dbf],
  [0x4e00, 0x9fff], [0xa000, 0xa4cf], [0xac00, 0xd7a3], [0xf900, 0xfaff], [0xfe10, 0xfe19],
  [0xfe30, 0xfe6f], [0xff00, 0xff60], [0xffe0, 0xffe6], [0x1f300, 0x1faff], [0x20000, 0x3fffd],
];
function isWide(cp) {
  for (const [a, b] of WIDE_RANGES) if (cp >= a && cp <= b) return true;
  return false;
}
function stringWidth(str, _opts) {
  const s = stripANSI(typeof str === "string" ? str : String(str));
  let w = 0;
  for (const ch of s) {
    const cp = ch.codePointAt(0);
    if (cp === 0 || (cp >= 0x200b && cp <= 0x200f) || (cp >= 0x0300 && cp <= 0x036f)) continue;
    w += isWide(cp) ? 2 : 1;
  }
  return w;
}

// ---- deepEquals / deepMatch -------------------------------------------------------------------
function deepEquals(a, b, strict) {
  return eq(a, b, !!strict, new Set());
}
function eq(a, b, strict, seen) {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) {
    return !strict && a == b && typeof a === typeof b ? a === b : a === b;
  }
  if (seen.has(a)) return true;
  seen.add(a);
  if (a instanceof Date || b instanceof Date) return a instanceof Date && b instanceof Date && a.getTime() === b.getTime();
  if (a instanceof RegExp || b instanceof RegExp) return String(a) === String(b);
  const aArr = Array.isArray(a), bArr = Array.isArray(b);
  if (aArr !== bArr) return false;
  if (aArr) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!eq(a[i], b[i], strict, seen)) return false;
    return true;
  }
  if (ArrayBuffer.isView(a) || ArrayBuffer.isView(b)) {
    const ua = toU8(a), ub = toU8(b);
    if (ua.length !== ub.length) return false;
    for (let i = 0; i < ua.length; i++) if (ua[i] !== ub[i]) return false;
    return true;
  }
  if (a instanceof Map || b instanceof Map) {
    if (!(a instanceof Map && b instanceof Map) || a.size !== b.size) return false;
    for (const [k, v] of a) { if (!b.has(k) || !eq(v, b.get(k), strict, seen)) return false; }
    return true;
  }
  if (a instanceof Set || b instanceof Set) {
    if (!(a instanceof Set && b instanceof Set) || a.size !== b.size) return false;
    for (const v of a) if (!b.has(v)) return false;
    return true;
  }
  if (strict && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
  const ka = Object.keys(a), kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (const k of ka) {
    if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
    if (!eq(a[k], b[k], strict, seen)) return false;
  }
  return true;
}
function deepMatch(subset, obj) {
  if (subset === obj) return true;
  if (typeof subset !== "object" || subset === null) return Object.is(subset, obj);
  if (typeof obj !== "object" || obj === null) return false;
  if (Array.isArray(subset)) {
    if (!Array.isArray(obj) || obj.length !== subset.length) return false;
    return subset.every((v, i) => deepMatch(v, obj[i]));
  }
  for (const k of Object.keys(subset)) {
    if (!Object.prototype.hasOwnProperty.call(obj, k)) return false;
    if (!deepMatch(subset[k], obj[k])) return false;
  }
  return true;
}

// ---- hashing ----------------------------------------------------------------------------------
// All of Bun.hash is real and verified bit-for-bit against Bun 1.2.21 (oracle fixture:
// crates/lumen-node/tests/fixtures/bun_hash_oracle.txt). crc32/adler32 stay in JS (cheap);
// wyhash/cityHash/xxHash/murmur/rapidhash are native ops (__bunhash, crates/lumen-node/src/
// bunhash.rs) ported from the exact Zig stdlib sources Bun compiles in.
//
// Input coercion mirrors Bun's hashWrap: no argument → ""; ArrayBuffer/TypedArray/DataView →
// their bytes; anything else (including explicit undefined/null) → String(x) as UTF-8. The seed
// mirrors JSC toUInt64NoTruncate: bigint → mod 2^64; int32 numbers sign-extend (so -1 →
// 2^64-1); other integer doubles are used only when 0 <= d < 2^51 (JSC int52 — negative doubles
// clamp to 0); everything else (non-integer, NaN, ±Inf, non-number) → 0.
const HASH_EMPTY = new Uint8Array(0);
function hashBytes(args) {
  if (args.length === 0) return HASH_EMPTY;
  const x = args[0];
  if (typeof x === "string") return Buffer.from(x, "utf8");
  if (x instanceof ArrayBuffer) return new Uint8Array(x);
  if (ArrayBuffer.isView(x)) return new Uint8Array(x.buffer, x.byteOffset, x.byteLength);
  return Buffer.from(String(x), "utf8");
}
function hashSeed(args) {
  const s = args[1];
  if (typeof s === "bigint") return BigInt.asUintN(64, s);
  if (typeof s === "number") {
    if ((s | 0) === s) return BigInt.asUintN(64, BigInt(s | 0));
    if (Number.isInteger(s) && s > 0 && s < 2 ** 51) return BigInt(s);
  }
  return 0n;
}
const hashU64 = (v) => BigInt.asUintN(64, v);
const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
function crc32Core(b) {
  let c = 0xffffffff;
  for (let i = 0; i < b.length; i++) c = CRC_TABLE[(c ^ b[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function adler32Core(b) {
  let a = 1, s = 0;
  for (let i = 0; i < b.length; i++) { a = (a + b[i]) % 65521; s = (s + a) % 65521; }
  return ((s << 16) | a) >>> 0;
}
// Bun.hash(input, seed) IS wyhash (the same callback object in Bun).
const hash = function hash(...args) {
  return hashU64(__bunhash.wyhash(hashBytes(args), hashSeed(args)));
};
hash.wyhash = function wyhash(...args) { return hashU64(__bunhash.wyhash(hashBytes(args), hashSeed(args))); };
hash.cityHash64 = function cityHash64(...args) { return hashU64(__bunhash.cityHash64(hashBytes(args), hashSeed(args))); };
hash.xxHash64 = function xxHash64(...args) { return hashU64(__bunhash.xxHash64(hashBytes(args), hashSeed(args))); };
hash.xxHash3 = function xxHash3(...args) { return hashU64(__bunhash.xxHash3(hashBytes(args), hashSeed(args))); };
hash.murmur64v2 = function murmur64v2(...args) { return hashU64(__bunhash.murmur64v2(hashBytes(args), hashSeed(args))); };
hash.rapidhash = function rapidhash(...args) { return hashU64(__bunhash.rapidhash(hashBytes(args), hashSeed(args))); };
hash.cityHash32 = function cityHash32(...args) { return __bunhash.cityHash32(hashBytes(args)); }; // seed ignored, like Bun
hash.xxHash32 = function xxHash32(...args) { return __bunhash.xxHash32(hashBytes(args), hashSeed(args)); };
hash.murmur32v3 = function murmur32v3(...args) { return __bunhash.murmur32v3(hashBytes(args), hashSeed(args)); };
hash.murmur32v2 = function murmur32v2(...args) { return __bunhash.murmur32v2(hashBytes(args), hashSeed(args)); };
hash.crc32 = function crc32(...args) { return crc32Core(hashBytes(args)); }; // seed ignored, like Bun
hash.adler32 = function adler32(...args) { return adler32Core(hashBytes(args)); };

// ---- CryptoHasher / MD5 / SHA* ----------------------------------------------------------------
// Backed by node crypto (md5/sha1/sha256/sha384/sha512/sha512-224/sha512-256 are real; every other
// algorithm throws at the crypto layer, which is the honest outcome — lumen has no impl for them).
class CryptoHasher {
  constructor(algorithm, hmacKey) {
    this.algorithm = algorithm;
    this._inner = hmacKey !== undefined
      ? nodeCrypto.createHmac(algorithm, hmacKey)
      : nodeCrypto.createHash(algorithm);
  }
  update(data, encoding) {
    this._inner.update(ArrayBuffer.isView(data) || data instanceof ArrayBuffer ? Buffer.from(toU8(data)) : data, encoding);
    return this;
  }
  digest(encoding) {
    const out = this._inner.digest();
    return encoding ? out.toString(encoding) : new Uint8Array(out);
  }
  static hash(algorithm, input, encoding) {
    const h = nodeCrypto.createHash(algorithm);
    h.update(Buffer.from(toU8(input)));
    const out = h.digest();
    return encoding ? out.toString(encoding) : new Uint8Array(out);
  }
}
CryptoHasher.algorithms = ["md5", "sha1", "sha256", "sha384", "sha512", "sha512-224", "sha512-256"];

function makeHashClass(algo) {
  const C = class {
    constructor() { this._h = nodeCrypto.createHash(algo); }
    update(data, enc) { this._h.update(Buffer.from(toU8(data)), enc); return this; }
    digest(enc) { const o = this._h.digest(); return enc ? o.toString(enc) : new Uint8Array(o); }
    static hash(data, enc) {
      const h = nodeCrypto.createHash(algo);
      h.update(Buffer.from(toU8(data)));
      const o = h.digest();
      return enc ? o.toString(enc) : new Uint8Array(o);
    }
  };
  return C;
}
// md5/sha1/sha256/sha384/sha512/sha512-256 are real; md4/sha224 throw at construct/hash time.
const MD5 = makeHashClass("md5");
const MD4 = makeHashClass("md4");
const SHA1 = makeHashClass("sha1");
const SHA224 = makeHashClass("sha224");
const SHA256 = makeHashClass("sha256");
const SHA384 = makeHashClass("sha384");
const SHA512 = makeHashClass("sha512");
const SHA512_256 = makeHashClass("sha512-256");

// Bun.sha is the SHA-512/256 one-shot (real: native SHA-512 core in lumen-node/src/crypto.rs).
function sha(input, encoding) {
  return SHA512_256.hash(input, encoding);
}

// ---- UUID v5 / v7 -----------------------------------------------------------------------------
const UUID_NS = {
  dns: "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
  url: "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
  oid: "6ba7b812-9dad-11d1-80b4-00c04fd430c8",
  x500: "6ba7b814-9dad-11d1-80b4-00c04fd430c8",
};
function uuidBytesToStr(b) {
  const h = [];
  for (let i = 0; i < 16; i++) h.push(b[i].toString(16).padStart(2, "0"));
  return `${h.slice(0, 4).join("")}-${h.slice(4, 6).join("")}-${h.slice(6, 8).join("")}-${h.slice(8, 10).join("")}-${h.slice(10, 16).join("")}`;
}
function uuidStrToBytes(s) {
  const hex = s.replace(/-/g, "");
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}
function randomUUIDv5(name, namespace, encoding) {
  const nsStr = UUID_NS[namespace] || namespace;
  if (typeof nsStr !== "string" || !/^[0-9a-f-]{36}$/i.test(nsStr)) throw new TypeError("Invalid namespace");
  const nsBytes = uuidStrToBytes(nsStr);
  const nameBytes = toU8(name);
  const buf = new Uint8Array(nsBytes.length + nameBytes.length);
  buf.set(nsBytes, 0);
  buf.set(nameBytes, nsBytes.length);
  const digest = new Uint8Array(nodeCrypto.createHash("sha1").update(Buffer.from(buf)).digest());
  const out = digest.slice(0, 16);
  out[6] = (out[6] & 0x0f) | 0x50; // version 5
  out[8] = (out[8] & 0x3f) | 0x80; // RFC 4122 variant
  if (encoding === "buffer") return Buffer.from(out);
  return uuidBytesToStr(out);
}
function randomUUIDv7(encoding, timestamp) {
  const ts = timestamp === undefined ? Date.now() : (timestamp instanceof Date ? timestamp.getTime() : Number(timestamp));
  const out = new Uint8Array(16);
  const rnd = new Uint8Array(16);
  globalThis.crypto.getRandomValues(rnd);
  let t = BigInt(ts);
  for (let i = 5; i >= 0; i--) { out[i] = Number(t & 0xffn); t >>= 8n; }
  out.set(rnd.subarray(6, 16), 6);
  out[6] = (out[6] & 0x0f) | 0x70; // version 7
  out[8] = (out[8] & 0x3f) | 0x80; // variant
  if (encoding === "buffer") return Buffer.from(out);
  return uuidBytesToStr(out);
}

// ---- peek -------------------------------------------------------------------------------------
// lumen exposes no synchronous promise-state introspection, so peek is best-effort per Bun's
// contract: a non-thenable resolves to itself; a pending/unknown promise returns the promise.
function isThenable(x) {
  return x != null && (typeof x === "object" || typeof x === "function") && typeof x.then === "function";
}
function peek(value) {
  return isThenable(value) ? value : value;
}
peek.status = (value) => (isThenable(value) ? "pending" : "fulfilled");

// ---- which ------------------------------------------------------------------------------------
function which(cmd, options) {
  cmd = String(cmd);
  const opts = options || {};
  if (cmd.includes("/")) {
    try { const st = nodeFs.statSync(cmd); if (st.isFile()) return nodePath.resolve(cmd); } catch { /* fall through */ }
    return null;
  }
  const pathVar = opts.PATH !== undefined ? opts.PATH : (proc.env.PATH || "");
  const cwd = opts.cwd || proc.cwd();
  for (const dir of pathVar.split(":")) {
    if (!dir) continue;
    const base = nodePath.isAbsolute(dir) ? dir : nodePath.resolve(cwd, dir);
    const full = nodePath.join(base, cmd);
    try {
      const st = nodeFs.statSync(full);
      if (st.isFile()) return full;
    } catch { /* not here */ }
  }
  return null;
}

// ---- zlib one-shots (Bun returns Uint8Array) --------------------------------------------------
const gzipSync = (data) => new Uint8Array(nodeZlib.gzipSync(Buffer.from(toU8(data))));
const gunzipSync = (data) => new Uint8Array(nodeZlib.gunzipSync(Buffer.from(toU8(data))));
const deflateSync = (data) => new Uint8Array(nodeZlib.deflateSync(Buffer.from(toU8(data))));
const inflateSync = (data) => new Uint8Array(nodeZlib.inflateSync(Buffer.from(toU8(data))));

// ---- misc buffer utilities --------------------------------------------------------------------
function concatArrayBuffers(buffers, maxLength, asUint8Array) {
  if (typeof maxLength === "boolean") { asUint8Array = maxLength; maxLength = undefined; }
  let total = 0;
  const parts = [];
  for (const b of buffers) { const u = toU8(b); parts.push(u); total += u.length; }
  if (typeof maxLength === "number") total = Math.min(total, maxLength);
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    if (off >= total) break;
    const take = Math.min(p.length, total - off);
    out.set(p.subarray(0, take), off);
    off += take;
  }
  return asUint8Array ? out : out.buffer;
}
function allocUnsafe(size) {
  return Buffer.allocUnsafe(size);
}
function indexOfLine(buffer, offset = 0) {
  const b = toU8(buffer);
  for (let i = offset | 0; i < b.length; i++) if (b[i] === 0x0a) return i;
  return -1;
}

// ---- readableStreamTo* ------------------------------------------------------------------------
async function drainStream(stream) {
  const reader = stream.getReader();
  const chunks = [];
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
  }
  return chunks;
}
async function collectStreamBytes(stream) {
  const chunks = await drainStream(stream);
  return Buffer.concat(chunks.map((c) => Buffer.from(toU8(c))));
}
const readableStreamToArray = (s) => drainStream(s);
async function readableStreamToArrayBuffer(s) {
  const b = await collectStreamBytes(s);
  return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
}
const readableStreamToBytes = async (s) => new Uint8Array(await collectStreamBytes(s));
const readableStreamToBlob = async (s) => new Blob([await collectStreamBytes(s)]);
const readableStreamToText = async (s) => (await collectStreamBytes(s)).toString("utf8");
const readableStreamToJSON = async (s) => JSON.parse(await readableStreamToText(s));
async function readableStreamToFormData(stream, boundary) {
  const bytes = await collectStreamBytes(stream);
  const headers = boundary ? { "content-type": `multipart/form-data; boundary=${boundary}` } : undefined;
  return new Response(bytes, headers ? { headers } : undefined).formData();
}

// ---- ArrayBufferSink --------------------------------------------------------------------------
class ArrayBufferSink {
  constructor() { this._chunks = []; this._stream = false; this._asUint8 = false; }
  start(options) {
    this._chunks = [];
    if (options) { this._stream = !!options.stream; this._asUint8 = !!options.asUint8Array; }
    return this;
  }
  write(chunk) { const u = toU8(chunk); this._chunks.push(u); return u.length; }
  _drain() {
    const b = Buffer.concat(this._chunks.map((c) => Buffer.from(c)));
    this._chunks = [];
    return b;
  }
  flush() {
    if (!this._stream) return 0;
    const b = this._drain();
    const ab = b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
    return this._asUint8 ? new Uint8Array(ab) : ab;
  }
  end() {
    const b = this._drain();
    const ab = b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
    return this._asUint8 ? new Uint8Array(ab) : ab;
  }
}

// ---- BunFile / write / stdio ------------------------------------------------------------------
const MIME = {
  js: "text/javascript;charset=utf-8", mjs: "text/javascript;charset=utf-8", cjs: "text/javascript;charset=utf-8",
  json: "application/json;charset=utf-8", txt: "text/plain;charset=utf-8", html: "text/html;charset=utf-8",
  htm: "text/html;charset=utf-8", css: "text/css;charset=utf-8", csv: "text/csv;charset=utf-8",
  xml: "text/xml;charset=utf-8", md: "text/markdown;charset=utf-8", png: "image/png", jpg: "image/jpeg",
  jpeg: "image/jpeg", gif: "image/gif", svg: "image/svg+xml", webp: "image/webp", wasm: "application/wasm",
  pdf: "application/pdf", zip: "application/zip", gz: "application/gzip", mp3: "audio/mpeg", mp4: "video/mp4",
  woff: "font/woff", woff2: "font/woff2", ttf: "font/ttf", ico: "image/vnd.microsoft.icon",
};
function mimeOf(p) {
  const ext = (String(p).split(".").pop() || "").toLowerCase();
  return MIME[ext] || "application/octet-stream";
}
function resolvePathArg(p) {
  if (p instanceof URL) return nodeUrl.fileURLToPath(p);
  if (typeof p === "string" && p.startsWith("file://")) return nodeUrl.fileURLToPath(p);
  return String(p);
}

class FileSink {
  constructor(path) { this._path = path; this._chunks = []; this._started = false; }
  write(chunk) { const u = toU8(chunk); this._chunks.push(u); return u.length; }
  _writeAll(flag) {
    const buf = Buffer.concat(this._chunks.map((c) => Buffer.from(c)));
    if (flag === "append" && this._started) nodeFs.appendFileSync(this._path, buf);
    else nodeFs.writeFileSync(this._path, buf);
    this._started = true;
    this._chunks = [];
    return buf.length;
  }
  flush() { return this._writeAll(this._started ? "append" : "write"); }
  end() { return this._writeAll(this._started ? "append" : "write"); }
}

class BunFile {
  constructor(path, options) {
    this._path = resolvePathArg(path);
    this._type = options && options.type;
  }
  get name() { return this._path; }
  get size() { try { return nodeFs.statSync(this._path).size; } catch { return 0; } }
  get type() { return this._type || mimeOf(this._path); }
  get lastModified() { try { return nodeFs.statSync(this._path).mtimeMs; } catch { return 0; } }
  async exists() { return nodeFs.existsSync(this._path); }
  async stat() { return nodeFs.statSync(this._path); }
  async text() { return nodeFs.readFileSync(this._path, "utf8"); }
  async json() { return JSON.parse(await this.text()); }
  async arrayBuffer() {
    const b = Buffer.from(nodeFs.readFileSync(this._path));
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
  async bytes() { return new Uint8Array(nodeFs.readFileSync(this._path)); }
  async formData() {
    return new Response(new Uint8Array(nodeFs.readFileSync(this._path)), { headers: { "content-type": this.type } }).formData();
  }
  stream() {
    const bytes = new Uint8Array(nodeFs.readFileSync(this._path));
    return new ReadableStream({ start(c) { c.enqueue(bytes); c.close(); } });
  }
  slice(start, end, type) {
    const bytes = new Uint8Array(nodeFs.readFileSync(this._path));
    return new Blob([bytes.slice(start, end)], { type: type || this._type });
  }
  writer() { return new FileSink(this._path); }
  async write(data) { return write(this._path, data); }
  async delete() { nodeFs.unlinkSync(this._path); }
  async unlink() { nodeFs.unlinkSync(this._path); }
}

function file(path, options) {
  return new BunFile(path, options);
}

async function write(dest, data) {
  const path = dest instanceof BunFile ? dest._path : resolvePathArg(dest);
  let bytes;
  if (typeof data === "string") bytes = Buffer.from(data, "utf8");
  else if (data instanceof BunFile) bytes = Buffer.from(nodeFs.readFileSync(data._path));
  else if (typeof Blob !== "undefined" && data instanceof Blob) bytes = Buffer.from(new Uint8Array(await data.arrayBuffer()));
  else if (typeof Response !== "undefined" && data instanceof Response) bytes = Buffer.from(new Uint8Array(await data.arrayBuffer()));
  else if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) bytes = Buffer.from(toU8(data));
  else if (Array.isArray(data)) bytes = Buffer.concat(data.map((d) => Buffer.from(toU8(d))));
  else bytes = Buffer.from(String(data), "utf8");
  nodeFs.writeFileSync(path, bytes);
  return bytes.length;
}

function makeStdioWriter(nodeStream, fd) {
  const f = new BunFile(`/dev/fd/${fd}`, {});
  f.write = (data) => {
    const u = toU8(data);
    if (nodeStream && typeof nodeStream.write === "function") nodeStream.write(Buffer.from(u));
    return u.length;
  };
  f.writer = () => ({
    write: f.write,
    flush() { return 0; },
    end() { return 0; },
  });
  return f;
}
const stdout = makeStdioWriter(proc.stdout, 1);
const stderr = makeStdioWriter(proc.stderr, 2);
const stdin = (() => {
  const f = new BunFile("/dev/fd/0", {});
  f.text = async () => new Promise((resolve) => {
    if (!proc.stdin || typeof proc.stdin.on !== "function") return resolve("");
    const chunks = [];
    proc.stdin.on("data", (c) => chunks.push(Buffer.from(c)));
    proc.stdin.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    if (typeof proc.stdin.resume === "function") proc.stdin.resume();
  });
  f.stream = () => new ReadableStream({
    start(c) {
      if (!proc.stdin || typeof proc.stdin.on !== "function") return c.close();
      proc.stdin.on("data", (d) => c.enqueue(new Uint8Array(Buffer.from(d))));
      proc.stdin.on("end", () => c.close());
      if (typeof proc.stdin.resume === "function") proc.stdin.resume();
    },
  });
  return f;
})();

// ---- Glob -------------------------------------------------------------------------------------
// Anchored regex compilation for a glob (** crosses separators, * does not), and a recursive
// directory walk over node:fs (lumen's fs has no globSync, so this is a from-scratch scanner).
function globToRegExp(glob) {
  let re = "^";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") { i++; if (glob[i + 1] === "/") { i++; re += "(?:.*/)?"; } else re += ".*"; }
      else re += "[^/]*";
    } else if (c === "?") re += "[^/]";
    else if (c === ".") re += "\\.";
    else if ("+^${}()|[]\\".includes(c)) re += "\\" + c;
    else if (c === "{") re += "(?:";
    else if (c === "}") re += ")";
    else if (c === ",") re += "|";
    else re += c;
  }
  return new RegExp(re + "$");
}
class Glob {
  constructor(pattern) { this.pattern = String(pattern); this._re = globToRegExp(this.pattern); }
  _scanArray(options) {
    const cwd = typeof options === "string" ? options : (options && options.cwd) || proc.cwd();
    const onlyFiles = !options || options.onlyFiles !== false;
    const dot = !!(options && options.dot);
    const results = [];
    const walk = (dir, rel) => {
      let entries;
      try { entries = nodeFs.readdirSync(dir); } catch { return; }
      for (const name of entries) {
        if (!dot && name.startsWith(".")) continue;
        const full = nodePath.join(dir, name);
        const relPath = rel ? rel + "/" + name : name;
        let st;
        try { st = nodeFs.statSync(full); } catch { continue; }
        if (st.isDirectory()) {
          if (!onlyFiles && this._re.test(relPath)) results.push(relPath);
          walk(full, relPath);
        } else if (this._re.test(relPath)) {
          results.push(relPath);
        }
      }
    };
    walk(cwd, "");
    return results;
  }
  scanSync(options) { return this._scanArray(options); }
  async *scan(options) { for (const p of this._scanArray(options)) yield p; }
  match(str) { return this._re.test(String(str)); }
}

// ---- $ shell ----------------------------------------------------------------------------------
// Real, over child_process (sh -c). Async; supports .text()/.json()/.quiet()/.cwd()/.env()/.nothrow().
// Caveat: interpolated values are shell-quoted with single quotes; no streaming; runs via /bin/sh.
function shellQuote(v) {
  if (Array.isArray(v)) return v.map(shellQuote).join(" ");
  if (v instanceof BunFile) return "'" + v._path.replace(/'/g, "'\\''") + "'";
  const s = String(v);
  return "'" + s.replace(/'/g, "'\\''") + "'";
}
class ShellPromise {
  constructor(cmd) {
    this._cmd = cmd; this._quiet = false; this._nothrow = false;
    this._cwd = undefined; this._env = undefined; this._promise = null;
  }
  quiet() { this._quiet = true; return this; }
  nothrow() { this._nothrow = true; return this; }
  cwd(dir) { this._cwd = dir; return this; }
  env(e) { this._env = e; return this; }
  _run() {
    if (this._promise) return this._promise;
    this._promise = new Promise((resolve, reject) => {
      nodeChild.exec(this._cmd, { cwd: this._cwd, env: this._env, encoding: "buffer" }, (err, so, se) => {
        const stdout = Buffer.from(so || []);
        const stderr = Buffer.from(se || []);
        const exitCode = err ? (typeof err.code === "number" ? err.code : 1) : 0;
        const result = {
          stdout, stderr, exitCode,
          text: (enc) => stdout.toString(enc || "utf8"),
          json: () => JSON.parse(stdout.toString("utf8")),
          arrayBuffer: () => stdout.buffer.slice(stdout.byteOffset, stdout.byteOffset + stdout.byteLength),
          blob: () => new Blob([stdout]),
          bytes: () => new Uint8Array(stdout),
        };
        if (exitCode !== 0 && !this._nothrow) {
          const e = new Error(`Command "${this._cmd}" failed with exit code ${exitCode}\n${stderr.toString("utf8")}`);
          e.exitCode = exitCode; e.stdout = stdout; e.stderr = stderr;
          return reject(e);
        }
        resolve(result);
      });
    });
    return this._promise;
  }
  then(a, b) { return this._run().then(a, b); }
  catch(b) { return this._run().catch(b); }
  finally(f) { return this._run().finally(f); }
  async text(enc) { return (await this._run()).stdout.toString(enc || "utf8"); }
  async json() { return JSON.parse(await this.text()); }
  async arrayBuffer() { const r = await this._run(); return r.stdout.buffer.slice(r.stdout.byteOffset, r.stdout.byteOffset + r.stdout.byteLength); }
  async blob() { return new Blob([(await this._run()).stdout]); }
  async bytes() { return new Uint8Array((await this._run()).stdout); }
  async lines() { return (await this.text()).split("\n"); }
}
function $(strings, ...values) {
  if (!Array.isArray(strings)) throw new TypeError("Bun.$ must be used as a template tag");
  let cmd = "";
  for (let i = 0; i < strings.length; i++) {
    cmd += strings[i];
    if (i < values.length) cmd += shellQuote(values[i]);
  }
  return new ShellPromise(cmd);
}
$.escape = shellQuote;
$.braces = (pattern) => [String(pattern)];

// ---- spawn / spawnSync ------------------------------------------------------------------------
function nodeReadableToWebStream(readable) {
  if (!readable) return null;
  return new ReadableStream({
    start(controller) {
      readable.on("data", (chunk) => controller.enqueue(new Uint8Array(Buffer.from(chunk))));
      readable.on("end", () => { try { controller.close(); } catch { /* already closed */ } });
      readable.on("error", (e) => controller.error(e));
    },
  });
}
function normalizeSpawnArgs(cmd, options) {
  let argv, opts;
  if (Array.isArray(cmd)) { argv = cmd.map(String); opts = options || {}; }
  else if (cmd && typeof cmd === "object") { opts = cmd; argv = (cmd.cmd || []).map(String); }
  else { argv = [String(cmd)]; opts = options || {}; }
  if (!argv.length) throw new TypeError("Bun.spawn requires a non-empty command");
  return { file: argv[0], args: argv.slice(1), opts };
}
function spawn(cmd, options) {
  const { file, args, opts } = normalizeSpawnArgs(cmd, options);
  const child = nodeChild.spawn(file, args, {
    cwd: opts.cwd,
    env: opts.env,
    stdio: [
      opts.stdin === "inherit" ? "inherit" : opts.stdin === "ignore" ? "ignore" : "pipe",
      opts.stdout === "inherit" ? "inherit" : opts.stdout === "ignore" ? "ignore" : "pipe",
      opts.stderr === "inherit" ? "inherit" : opts.stderr === "ignore" ? "ignore" : "pipe",
    ],
  });
  let exitResolve;
  const exited = new Promise((r) => { exitResolve = r; });
  const proc2 = {
    pid: child.pid,
    exitCode: null,
    signalCode: null,
    killed: false,
    stdin: child.stdin || null,
    stdout: nodeReadableToWebStream(child.stdout),
    stderr: nodeReadableToWebStream(child.stderr),
    exited,
    kill(signal) { this.killed = true; child.kill(signal); },
    ref() {},
    unref() {},
  };
  child.on("exit", (code, sig) => { proc2.exitCode = code; proc2.signalCode = sig; });
  child.on("close", (code, sig) => { proc2.exitCode = code; proc2.signalCode = sig; exitResolve(code); });
  child.on("error", () => { exitResolve(1); });
  if (opts.onExit) exited.then((code) => opts.onExit(proc2, code, proc2.signalCode, undefined));
  return proc2;
}
function spawnSync(cmd, options) {
  const { file, args, opts } = normalizeSpawnArgs(cmd, options);
  const input = opts.stdin != null && (typeof opts.stdin === "string" || ArrayBuffer.isView(opts.stdin))
    ? Buffer.from(toU8(opts.stdin)) : undefined;
  const r = nodeChild.spawnSync(file, args, { cwd: opts.cwd, env: opts.env, input });
  const stdout = Buffer.from(r.stdout || []);
  const stderr = Buffer.from(r.stderr || []);
  const exitCode = r.status == null ? 0 : r.status;
  return {
    exitCode,
    stdout,
    stderr,
    success: exitCode === 0,
    pid: r.pid || 0,
    signalCode: r.signal || null,
    resourceUsage: {},
  };
}

// ---- serve (Bun HTTP server over Lumen.serve) -------------------------------------------------
// Bun.serve({ fetch, port, hostname, websocket }) → a server object. The fetch handler gets
// (request, server) and returns a Response, matching Lumen.serve's WinterCG dispatch. WebSocket
// upgrade is real: server.upgrade(req, { data, headers }) hands the accepted socket to the native
// RFC 6455 machinery (Lumen.upgradeWebSocket), and the `websocket` handlers {open, message, close}
// drive Bun-shaped ServerWebSocket objects with send/close/terminate + in-process pub/sub
// (subscribe/publish topic map, shared with server.publish). Caveats vs Bun: no backpressure
// (send never returns -1 and `drain` never fires), no permessage-deflate, ping/pong handlers are
// not called (pings are answered at the protocol layer), and per-route `routes` config is not
// supported.
function serve(options) {
  options = options || {};
  let fetchHandler = options.fetch;
  let wsHandlers = options.websocket;
  if (typeof fetchHandler !== "function") throw new TypeError("Bun.serve requires a `fetch` handler function");
  let server;

  // ---- WebSocket support (over Lumen.upgradeWebSocket) ----
  const topics = new Map(); // topic -> Set<ServerWebSocket>
  const liveSockets = new Set();
  const binaryTypeOf = () => (wsHandlers && wsHandlers.binaryType) || "nodebuffer";
  const toBinary = (u8) => {
    const t = binaryTypeOf();
    if (t === "arraybuffer") return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
    if (t === "uint8array") return u8;
    return Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength); // "nodebuffer" (Bun's default)
  };
  const call = (name, ...args) => {
    if (wsHandlers && typeof wsHandlers[name] === "function") {
      try { wsHandlers[name](...args); } catch (err) { console.error(err); }
    }
  };
  const publishTo = (topic, data, except) => {
    const subs = topics.get(String(topic));
    if (!subs || subs.size === 0) return 0;
    const bytes = typeof data === "string" ? Buffer.byteLength(data) : toU8(data).byteLength;
    let sent = 0;
    for (const peer of subs) {
      if (peer === except) continue;
      if (peer.readyState === 1) { peer._handle.send(typeof data === "string" ? data : toU8(data)); sent++; }
    }
    return sent ? bytes : 0;
  };
  const makeServerWebSocket = (handle, data) => {
    const ws = {
      data,
      remoteAddress: handle.remoteAddress,
      readyState: 1, // OPEN (the 101 is written before upgrade() returns)
      get binaryType() { return binaryTypeOf(); },
      send(payload, _compress) {
        if (ws.readyState !== 1) return 0;
        const out = typeof payload === "string" ? payload : toU8(payload);
        const ok = handle.send(out);
        return ok ? (typeof out === "string" ? Buffer.byteLength(out) : out.byteLength) : 0;
      },
      sendText(s, compress) { return ws.send(String(s), compress); },
      sendBinary(b, compress) { return ws.send(toU8(b), compress); },
      close(code, reason) {
        if (ws.readyState >= 2) return;
        ws.readyState = 2; // CLOSING; the peer's echo (or socket teardown) fires the close event
        handle.close(code, reason);
        // Bun reports CLOSED immediately after a local close().
        ws.readyState = 3;
      },
      terminate() { ws.close(1006, ""); },
      subscribe(topic) {
        topic = String(topic);
        let set = topics.get(topic);
        if (!set) topics.set(topic, (set = new Set()));
        set.add(ws);
      },
      unsubscribe(topic) {
        const set = topics.get(String(topic));
        if (set) { set.delete(ws); if (set.size === 0) topics.delete(String(topic)); }
      },
      isSubscribed(topic) {
        const set = topics.get(String(topic));
        return !!set && set.has(ws);
      },
      // ws.publish excludes the publisher itself (server.publish includes everyone).
      publish(topic, payload, _compress) { return publishTo(topic, payload, ws); },
      publishText(topic, s) { return ws.publish(topic, String(s)); },
      publishBinary(topic, b) { return ws.publish(topic, toU8(b)); },
      ping() { return 0; }, // protocol-layer pings aren't exposed; accepted as a no-op
      pong() { return 0; },
      cork(cb) { return typeof cb === "function" ? cb(ws) : undefined; },
      _handle: handle,
    };
    return ws;
  };

  const lu = globalThis.Lumen.serve((request, info) => fetchHandler(request, server), {
    hostname: typeof options.hostname === "string" ? options.hostname : "0.0.0.0",
    port: typeof options.port === "number" ? options.port : 3000,
    onError: typeof options.error === "function" ? options.error : undefined,
  });
  server = {
    port: lu.port,
    hostname: lu.hostname,
    development: !!options.development,
    pendingRequests: 0,
    get pendingWebSockets() { return liveSockets.size; },
    get url() { return new URL(`http://${lu.hostname}:${lu.port}/`); },
    stop(_closeActive) { return lu.shutdown(); },
    reload(newOptions) {
      if (newOptions && typeof newOptions.fetch === "function") fetchHandler = newOptions.fetch;
      if (newOptions && newOptions.websocket) wsHandlers = newOptions.websocket;
      return server;
    },
    fetch(req) { return fetchHandler(typeof req === "string" ? new Request(req) : req, server); },
    ref() {}, unref() {},
    requestIP() { return null },
    // server.publish(topic, data): every subscriber, including the would-be publisher.
    publish(topic, data, _compress) { return publishTo(topic, data, null); },
    subscriberCount(topic) { const s = topics.get(String(topic)); return s ? s.size : 0; },
    upgrade(request, opts = {}) {
      if (!wsHandlers || typeof wsHandlers !== "object") {
        const err = new TypeError('To enable websocket support, set the "websocket" object in Bun.serve({})');
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
      }
      const handle = globalThis.Lumen.upgradeWebSocket(request, { headers: opts.headers });
      if (!handle) return false; // not a WebSocket handshake
      const ws = makeServerWebSocket(handle, opts.data);
      liveSockets.add(ws);
      handle.onmessage = (payload, isBinary) => call("message", ws, isBinary ? toBinary(payload) : payload);
      handle.onclose = (code, reason) => {
        ws.readyState = 3;
        liveSockets.delete(ws);
        for (const set of topics.values()) set.delete(ws);
        call("close", ws, code, reason);
      };
      // Bun fires open() asynchronously, after the fetch handler returns.
      queueMicrotask(() => call("open", ws));
      return true;
    },
  };
  return server;
}

// ---- dns (subset Bun exposes, over node dns.promises) -----------------------------------------
const dns = {
  async lookup(hostname, options) {
    const opts = typeof options === "number" ? { family: options } : (options || {});
    const res = await nodeDns.promises.lookup(String(hostname), { all: true, family: opts.family || 0 });
    const arr = Array.isArray(res) ? res : [res];
    return arr.map((r) => ({ address: r.address, family: r.family }));
  },
  prefetch() {},
  getCacheStats() { return { cacheHitsCompleted: 0, cacheHitsInflight: 0, cacheMisses: 0, size: 0, errors: 0, totalCount: 0 }; },
};

// ---- resolve / resolveSync (over the module resolver) -----------------------------------------
function makeResolver(specifier, parent) {
  const Module = __builtins.get("module");
  if (!Module || typeof Module.createRequire !== "function") {
    throw new Error("module resolver unavailable");
  }
  let from = parent ? resolvePathArg(parent) : proc.cwd() + "/";
  if (!from.endsWith("/") && nodeFs.existsSync(from)) {
    try { if (nodeFs.statSync(from).isDirectory()) from += "/"; } catch { /* treat as file */ }
  }
  const anchor = from.endsWith("/") ? from + "__resolver__.js" : from;
  const req = Module.createRequire(anchor);
  return req.resolve(String(specifier));
}
function resolveSync(specifier, parent) { return makeResolver(specifier, parent); }
function resolve(specifier, parent) {
  return new Promise((res, rej) => {
    try { res(makeResolver(specifier, parent)); } catch (e) { rej(e); }
  });
}

// ---- inspect ----------------------------------------------------------------------------------
const inspect = (value, options) => nodeUtil.inspect(value, options);
inspect.custom = nodeUtil.inspect.custom;
inspect.table = (data, properties, options) => nodeUtil.inspect(data, options);

// ---- color ------------------------------------------------------------------------------------
// Real conversions for the deterministic output formats (hex/HEX/number/{rgb}/{rgba}/[rgb]/[rgba]
// /ansi-16m). css is best-effort (returns hex; Bun may minify to a named color). Palette-mapped
// ansi-256 / ansi-16 use Bun's own nearest-color search, so they honestly throw rather than emit a
// different index. Returns null for unparseable input, like Bun.
const NAMED = {
  black: [0, 0, 0], white: [255, 255, 255], red: [255, 0, 0], green: [0, 128, 0], blue: [0, 0, 255],
  yellow: [255, 255, 0], cyan: [0, 255, 255], magenta: [255, 0, 255], gray: [128, 128, 128],
  grey: [128, 128, 128], silver: [192, 192, 192], maroon: [128, 0, 0], olive: [128, 128, 0],
  lime: [0, 255, 0], aqua: [0, 255, 255], teal: [0, 128, 128], navy: [0, 0, 128], fuchsia: [255, 0, 255],
  purple: [128, 0, 128], orange: [255, 165, 0], pink: [255, 192, 203], brown: [165, 42, 42],
  gold: [255, 215, 0], indigo: [75, 0, 130], violet: [238, 130, 238], rebeccapurple: [102, 51, 153],
  transparent: [0, 0, 0, 0], coral: [255, 127, 80], salmon: [250, 128, 114], khaki: [240, 230, 140],
  crimson: [220, 20, 60], tomato: [255, 99, 71], turquoise: [64, 224, 208], tan: [210, 180, 140],
  skyblue: [135, 206, 235], royalblue: [65, 105, 225], steelblue: [70, 130, 180], slategray: [112, 128, 144],
  darkred: [139, 0, 0], darkgreen: [0, 100, 0], darkblue: [0, 0, 139], lightgray: [211, 211, 211],
  lightgrey: [211, 211, 211], lightblue: [173, 216, 230], lightgreen: [144, 238, 144],
  hotpink: [255, 105, 180], chocolate: [210, 105, 30], beige: [245, 245, 220], ivory: [255, 255, 240],
};
function parseColor(input) {
  if (input && typeof input === "object" && !Array.isArray(input)) {
    const r = input.r ?? 0, g = input.g ?? 0, b = input.b ?? 0, a = input.a ?? 1;
    return [r & 255, g & 255, b & 255, a];
  }
  if (Array.isArray(input)) {
    return [input[0] & 255, input[1] & 255, input[2] & 255, input[3] == null ? 1 : (input[3] > 1 ? input[3] / 255 : input[3])];
  }
  if (typeof input === "number") {
    return [(input >> 16) & 255, (input >> 8) & 255, input & 255, 1];
  }
  let s = String(input).trim().toLowerCase();
  if (NAMED[s]) { const n = NAMED[s]; return [n[0], n[1], n[2], n[3] == null ? 1 : n[3]]; }
  if (s[0] === "#") {
    s = s.slice(1);
    if (s.length === 3) return [parseInt(s[0] + s[0], 16), parseInt(s[1] + s[1], 16), parseInt(s[2] + s[2], 16), 1];
    if (s.length === 4) return [parseInt(s[0] + s[0], 16), parseInt(s[1] + s[1], 16), parseInt(s[2] + s[2], 16), parseInt(s[3] + s[3], 16) / 255];
    if (s.length === 6) return [parseInt(s.slice(0, 2), 16), parseInt(s.slice(2, 4), 16), parseInt(s.slice(4, 6), 16), 1];
    if (s.length === 8) return [parseInt(s.slice(0, 2), 16), parseInt(s.slice(2, 4), 16), parseInt(s.slice(4, 6), 16), parseInt(s.slice(6, 8), 16) / 255];
    return null;
  }
  const m = /^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)\s*(?:[,/]\s*([\d.]+%?)\s*)?\)$/.exec(s);
  if (m) {
    let a = 1;
    if (m[4] != null) a = m[4].endsWith("%") ? parseFloat(m[4]) / 100 : parseFloat(m[4]);
    return [Math.round(+m[1]) & 255, Math.round(+m[2]) & 255, Math.round(+m[3]) & 255, a];
  }
  return null;
}
const hex2 = (n) => (n & 255).toString(16).padStart(2, "0");
function color(input, format) {
  const c = parseColor(input);
  if (!c) return null;
  const [r, g, b, a] = c;
  switch (format || "css") {
    case "css": return `#${hex2(r)}${hex2(g)}${hex2(b)}`;
    case "hex": return `#${hex2(r)}${hex2(g)}${hex2(b)}`;
    case "HEX": return `#${hex2(r)}${hex2(g)}${hex2(b)}`.toUpperCase();
    case "number": return (r << 16) | (g << 8) | b;
    case "{rgb}": return { r, g, b };
    case "{rgba}": return { r, g, b, a };
    case "[rgb]": return [r, g, b];
    case "[rgba]": return [r, g, b, Math.round(a * 255)];
    case "ansi-16m":
    case "ansi_16m": return `\u001b[38;2;${r};${g};${b}m`;
    case "ansi":
    case "ansi-16":
    case "ansi-256":
      // Bun disables ANSI when the stream is not a TTY; with palette mapping unavailable we cannot
      // reproduce Bun's exact index, so mirror the disabled case.
      return "";
    default:
      throw new Error(`Bun.color: unsupported output format "${format}"`);
  }
}

// ---- TOML -------------------------------------------------------------------------------------
// A pragmatic TOML parser: tables/[[array-of-tables]], dotted keys, inline tables/arrays (may span
// lines), basic+literal strings, ints/floats (with underscores), bools; datetimes fall through as
// strings. Not a full TOML 1.0 validator, but covers the common config shapes.
function stripTomlComment(line) {
  let inS = null;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (inS) { if (c === inS) inS = null; }
    else if (c === '"' || c === "'") inS = c;
    else if (c === "#") return line.slice(0, i);
  }
  return line;
}
function tomlBalanced(s) {
  let depth = 0, inS = null;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (inS) { if (c === inS) inS = null; continue; }
    if (c === '"' || c === "'") inS = c;
    else if (c === "[" || c === "{") depth++;
    else if (c === "]" || c === "}") depth--;
  }
  return depth <= 0 && !inS;
}
function splitDottedKey(s) {
  const parts = [];
  let cur = "", inS = null;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (inS) { if (c === inS) inS = null; else cur += c; }
    else if (c === '"' || c === "'") inS = c;
    else if (c === ".") { parts.push(cur.trim()); cur = ""; }
    else cur += c;
  }
  parts.push(cur.trim());
  return parts.map((p) => p.replace(/^["']|["']$/g, ""));
}
function tomlScalar(tok) {
  tok = tok.trim();
  if (tok === "true") return true;
  if (tok === "false") return false;
  const noUnd = tok.replace(/_/g, "");
  if (/^[+-]?\d+$/.test(noUnd)) return parseInt(noUnd, 10);
  if (/^0x[0-9a-fA-F]+$/.test(noUnd)) return parseInt(noUnd, 16);
  if (/^0o[0-7]+$/.test(noUnd)) return parseInt(noUnd.slice(2), 8);
  if (/^0b[01]+$/.test(noUnd)) return parseInt(noUnd.slice(2), 2);
  if (/^[+-]?(\d+\.\d*|\.\d+|\d+)([eE][+-]?\d+)?$/.test(noUnd) && /[.eE]/.test(noUnd)) return parseFloat(noUnd);
  if (tok === "inf" || tok === "+inf") return Infinity;
  if (tok === "-inf") return -Infinity;
  if (tok === "nan") return NaN;
  return tok;
}
function tomlParseValue(s, i) {
  while (i < s.length && /\s/.test(s[i])) i++;
  const ch = s[i];
  if (ch === '"' || ch === "'") {
    // triple-quoted?
    if (s.slice(i, i + 3) === ch + ch + ch) {
      const end = s.indexOf(ch + ch + ch, i + 3);
      const raw = s.slice(i + 3, end === -1 ? s.length : end);
      return [raw.replace(/^\r?\n/, ""), (end === -1 ? s.length : end + 3)];
    }
    let j = i + 1, out = "";
    while (j < s.length && s[j] !== ch) {
      if (ch === '"' && s[j] === "\\") {
        const n = s[j + 1];
        out += n === "n" ? "\n" : n === "t" ? "\t" : n === "r" ? "\r" : n === '"' ? '"' : n === "\\" ? "\\" : n;
        j += 2;
      } else { out += s[j++]; }
    }
    return [out, j + 1];
  }
  if (ch === "[") {
    const arr = [];
    let j = i + 1;
    while (j < s.length) {
      while (j < s.length && (/\s/.test(s[j]) || s[j] === ",")) j++;
      if (s[j] === "]") { j++; break; }
      const [v, nj] = tomlParseValue(s, j);
      arr.push(v);
      j = nj;
      while (j < s.length && /\s/.test(s[j])) j++;
      if (s[j] === ",") j++;
      else if (s[j] === "]") { j++; break; }
    }
    return [arr, j];
  }
  if (ch === "{") {
    const obj = {};
    let j = i + 1;
    while (j < s.length) {
      while (j < s.length && (/\s/.test(s[j]) || s[j] === ",")) j++;
      if (s[j] === "}") { j++; break; }
      let k = "";
      while (j < s.length && s[j] !== "=" && s[j] !== "}") k += s[j++];
      if (s[j] === "=") j++;
      const [v, nj] = tomlParseValue(s, j);
      obj[k.trim().replace(/^["']|["']$/g, "")] = v;
      j = nj;
      while (j < s.length && /\s/.test(s[j])) j++;
      if (s[j] === ",") j++;
      else if (s[j] === "}") { j++; break; }
    }
    return [obj, j];
  }
  let j = i;
  while (j < s.length && !",]}\n".includes(s[j])) j++;
  return [tomlScalar(s.slice(i, j)), j];
}
function tomlNav(root, keys, isArray) {
  let cur = root;
  for (let i = 0; i < keys.length; i++) {
    const k = keys[i];
    const last = i === keys.length - 1;
    if (last && isArray) {
      if (!Array.isArray(cur[k])) cur[k] = [];
      const entry = {};
      cur[k].push(entry);
      return entry;
    }
    if (cur[k] == null) cur[k] = {};
    else if (Array.isArray(cur[k])) cur = cur[k][cur[k].length - 1];
    if (!Array.isArray(cur[k])) cur = cur[k];
  }
  return cur;
}
function parseTOML(src) {
  const root = {};
  let cur = root;
  const lines = String(src).split(/\r?\n/);
  for (let li = 0; li < lines.length; li++) {
    let line = stripTomlComment(lines[li]).trim();
    if (!line) continue;
    if (line[0] === "[") {
      const isArray = line[1] === "[";
      const close = isArray ? line.indexOf("]]") : line.indexOf("]");
      const inner = line.slice(isArray ? 2 : 1, close).trim();
      cur = tomlNav(root, splitDottedKey(inner), isArray);
      continue;
    }
    let eq = -1, inS = null;
    for (let i = 0; i < line.length; i++) {
      const c = line[i];
      if (inS) { if (c === inS) inS = null; }
      else if (c === '"' || c === "'") inS = c;
      else if (c === "=") { eq = i; break; }
    }
    if (eq === -1) continue;
    let keyPart = line.slice(0, eq).trim();
    let valPart = line.slice(eq + 1).trim();
    while (!tomlBalanced(valPart) && li + 1 < lines.length) { li++; valPart += "\n" + stripTomlComment(lines[li]); }
    const keys = splitDottedKey(keyPart);
    const target = keys.length > 1 ? tomlNav(cur, keys.slice(0, -1), false) : cur;
    target[keys[keys.length - 1]] = tomlParseValue(valPart, 0)[0];
  }
  return root;
}
const TOML = { parse: parseTOML };

// ---- YAML -------------------------------------------------------------------------------------
// Block-style YAML subset: nested maps, block sequences, flow [..]/{..}, and scalar typing
// (int/float/bool/null/string). No anchors, tags, multi-doc, or block scalar folding. Covers the
// common config/data cases; documented as a subset.
function yamlUnquote(s) {
  s = s.trim();
  if ((s[0] === '"' && s[s.length - 1] === '"') || (s[0] === "'" && s[s.length - 1] === "'")) {
    const body = s.slice(1, -1);
    return s[0] === '"' ? body.replace(/\\n/g, "\n").replace(/\\t/g, "\t").replace(/\\"/g, '"').replace(/\\\\/g, "\\") : body.replace(/''/g, "'");
  }
  return s;
}
function yamlScalarPlain(s) {
  s = s.trim();
  if (s === "" || s === "~" || s === "null" || s === "Null" || s === "NULL") return null;
  if (s === "true" || s === "True" || s === "TRUE") return true;
  if (s === "false" || s === "False" || s === "FALSE") return false;
  if (/^[+-]?\d+$/.test(s)) return parseInt(s, 10);
  if (/^[+-]?(\d+\.\d*|\.\d+|\d+)([eE][+-]?\d+)?$/.test(s) && /[.eE]/.test(s)) return parseFloat(s);
  return yamlUnquote(s);
}
function yamlParseFlow(s) {
  let i = 0;
  const ws = () => { while (i < s.length && /\s/.test(s[i])) i++; };
  function val() {
    ws();
    if (s[i] === "[") {
      i++; const a = []; ws();
      if (s[i] === "]") { i++; return a; }
      while (i < s.length) { a.push(val()); ws(); if (s[i] === ",") { i++; continue; } if (s[i] === "]") { i++; break; } break; }
      return a;
    }
    if (s[i] === "{") {
      i++; const o = {}; ws();
      if (s[i] === "}") { i++; return o; }
      while (i < s.length) {
        ws(); let k = ""; while (i < s.length && s[i] !== ":" && s[i] !== "," && s[i] !== "}") k += s[i++];
        if (s[i] === ":") i++;
        o[yamlUnquote(k)] = val(); ws();
        if (s[i] === ",") { i++; continue; } if (s[i] === "}") { i++; break; } break;
      }
      return o;
    }
    if (s[i] === '"' || s[i] === "'") { const q = s[i++]; let r = ""; while (i < s.length && s[i] !== q) r += s[i++]; i++; return r; }
    let t = ""; while (i < s.length && !",]}".includes(s[i])) t += s[i++];
    return yamlScalarPlain(t);
  }
  return val();
}
function yamlScalar(s) {
  s = s.trim();
  if (s[0] === "[" || s[0] === "{") return yamlParseFlow(s);
  return yamlScalarPlain(s);
}
function yamlFindColon(l) {
  let depth = 0, inS = null;
  for (let i = 0; i < l.length; i++) {
    const c = l[i];
    if (inS) { if (c === inS) inS = null; continue; }
    if (c === '"' || c === "'") inS = c;
    else if (c === "[" || c === "{") depth++;
    else if (c === "]" || c === "}") depth--;
    else if (c === ":" && depth === 0 && (i + 1 >= l.length || l[i + 1] === " ")) return i;
  }
  return -1;
}
function parseYAML(text) {
  const lines = [];
  for (const raw of String(text).split(/\r?\n/)) {
    const t = raw.replace(/\t/g, "  ");
    const trimmed = t.trim();
    if (trimmed === "" || trimmed[0] === "#" || trimmed === "---" || trimmed === "...") continue;
    lines.push(t);
  }
  let pos = 0;
  const indent = (l) => l.match(/^ */)[0].length;
  function parseNode(curIndent) {
    if (pos >= lines.length) return null;
    const ind = indent(lines[pos]);
    if (ind < curIndent) return null;
    if (lines[pos].trim()[0] === "-") {
      const arr = [];
      while (pos < lines.length && indent(lines[pos]) === ind && lines[pos].trim()[0] === "-") {
        let rest = lines[pos].trim().slice(1).trim();
        if (rest === "") { pos++; arr.push(parseNode(ind + 1)); }
        else if (yamlFindColon(rest) >= 0) { lines[pos] = " ".repeat(ind + 2) + rest; arr.push(parseNode(ind + 1)); }
        else { pos++; arr.push(yamlScalar(rest)); }
      }
      return arr;
    }
    const map = {};
    while (pos < lines.length && indent(lines[pos]) === ind && lines[pos].trim()[0] !== "-") {
      const l = lines[pos].trim();
      const ci = yamlFindColon(l);
      if (ci < 0) { pos++; continue; }
      const key = yamlUnquote(l.slice(0, ci));
      const valStr = l.slice(ci + 1).trim();
      if (valStr === "") { pos++; const child = parseNode(ind + 1); map[key] = child === undefined ? null : child; }
      else { pos++; map[key] = yamlScalar(valStr); }
    }
    return map;
  }
  return parseNode(0);
}
function stringifyYAML(value, indentLevel) {
  const pad = "  ".repeat(indentLevel || 0);
  if (value === null || value === undefined) return "null";
  if (typeof value !== "object") {
    if (typeof value === "string" && (value === "" || /[:#\-{}\[\],&*!|>'"%@`]/.test(value) || /^\s|\s$/.test(value))) return JSON.stringify(value);
    return String(value);
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return "[]";
    return value.map((v) => `${pad}- ${typeof v === "object" && v !== null ? "\n" + stringifyYAML(v, (indentLevel || 0) + 1) : stringifyYAML(v, 0)}`).join("\n");
  }
  const keys = Object.keys(value);
  if (keys.length === 0) return "{}";
  return keys.map((k) => {
    const v = value[k];
    if (typeof v === "object" && v !== null) return `${pad}${k}:\n${stringifyYAML(v, (indentLevel || 0) + 1)}`;
    return `${pad}${k}: ${stringifyYAML(v, 0)}`;
  }).join("\n");
}
const YAML = { parse: parseYAML, stringify: (v) => stringifyYAML(v, 0) + "\n" };

// ---- gc / shrink / unsafe ---------------------------------------------------------------------
function gc(_force) { if (typeof globalThis.gc === "function") { try { globalThis.gc(); } catch { /* ignore */ } } return 0; }
function shrink() { /* memory-shrink hint; lumen manages its own heap — honest no-op */ }
const unsafe = {
  arrayBufferToString(buf, offset, length) {
    const u = toU8(buf);
    return new TextDecoder().decode(offset != null ? u.subarray(offset, length != null ? offset + length : undefined) : u);
  },
  gcAggressionLevel() { return 0; },
  segfault: notImpl("Bun.unsafe.segfault"),
  mimallocDump: notImpl("Bun.unsafe.mimallocDump"),
};

// ---- password hashing -------------------------------------------------------------------------
function passwordOptions(options) {
  if (options === undefined) options = {};
  if (typeof options === "string") options = { algorithm: options };
  if (options === null || typeof options !== "object") {
    throw new TypeError("Bun.password options must be an object or algorithm string");
  }
  const algorithm = String(options.algorithm || "argon2id").toLowerCase();
  if (!["argon2id", "argon2i", "argon2d", "bcrypt"].includes(algorithm)) {
    throw new TypeError(`Unsupported password hashing algorithm '${algorithm}'`);
  }
  const integer = (name, fallback, min, max) => {
    const value = options[name] === undefined ? fallback : options[name];
    if (!Number.isInteger(value) || value < min || value > max) {
      throw new RangeError(`${name} must be an integer between ${min} and ${max}`);
    }
    return value;
  };
  if (algorithm === "bcrypt") {
    return { algorithm, memoryCost: 0, timeCost: 0, cost: integer("cost", 10, 4, 31) };
  }
  return {
    algorithm,
    memoryCost: integer("memoryCost", 65536, 1, 0xffffffff),
    timeCost: integer("timeCost", 2, 1, 0xffffffff),
    cost: 0,
  };
}
function passwordHashSync(input, options) {
  const p = passwordOptions(options);
  return __password.hashSync(toU8(input), p.algorithm, p.memoryCost, p.timeCost, p.cost);
}
function passwordHash(input, options) {
  let p;
  let bytes;
  try {
    p = passwordOptions(options);
    bytes = toU8(input);
  } catch (error) {
    return Promise.reject(error);
  }
  return new Promise((resolve, reject) => {
    __password.hash(bytes, p.algorithm, p.memoryCost, p.timeCost, p.cost, resolve, reject);
  });
}
function passwordVerifySync(input, hash) {
  return __password.verifySync(toU8(input), String(hash));
}
function passwordVerify(input, hash) {
  let bytes;
  try {
    bytes = toU8(input);
    hash = String(hash);
  } catch (error) {
    return Promise.reject(error);
  }
  return new Promise((resolve, reject) => __password.verify(bytes, hash, resolve, reject));
}
const password = {
  hash: passwordHash,
  verify: passwordVerify,
  hashSync: passwordHashSync,
  verifySync: passwordVerifySync,
};

// ---- honest throwing stubs (no backing engine capability) -------------------------------------
const secrets = { get: notImpl("Bun.secrets.get"), set: notImpl("Bun.secrets.set"), delete: notImpl("Bun.secrets.delete") };
const CSRF = { generate: notImpl("Bun.CSRF.generate"), verify: notImpl("Bun.CSRF.verify") };
const throwClass = (name) => class { constructor() { throw new Error(`${name} is not supported in lumen`); } };
// bun:ffi is registered before this module; Bun.FFI is the same live surface.
const FFI = __builtins.get("bun:ffi");

// ---- Zstandard --------------------------------------------------------------------------------
function zstdCompressSync(input) {
  return Uint8Array.from(__zlib.zstdCompress(toU8(input)));
}
function zstdDecompressSync(input) {
  return Uint8Array.from(__zlib.zstdDecompress(toU8(input)));
}
function zstdCompress(input) {
  return Promise.resolve().then(() => zstdCompressSync(input));
}
function zstdDecompress(input) {
  return Promise.resolve().then(() => zstdDecompressSync(input));
}

// ---- TCP / UDP sockets ------------------------------------------------------------------------
function attachBunSocket(socket, handlers, data) {
  socket.data = data;
  let lastError;
  if (handlers.data) socket.on("data", chunk => handlers.data(socket, chunk));
  if (handlers.drain) socket.on("drain", () => handlers.drain(socket));
  if (handlers.timeout) socket.on("timeout", () => handlers.timeout(socket));
  if (handlers.end) socket.on("end", () => handlers.end(socket));
  socket.on("error", error => {
    lastError = error;
    if (handlers.error) handlers.error(socket, error);
  });
  if (handlers.close) socket.on("close", () => handlers.close(socket, lastError));
  return socket;
}
function bunConnect(options) {
  if (!options || typeof options !== "object") return Promise.reject(new TypeError("Bun.connect options are required"));
  const handlers = options.socket || {};
  return new Promise((resolve, reject) => {
    const socket = attachBunSocket(new nodeNet.Socket(), handlers, options.data);
    let settled = false;
    socket.once("connect", () => {
      settled = true;
      if (handlers.open) handlers.open(socket);
      resolve(socket);
    });
    socket.once("error", error => {
      if (!settled) {
        if (handlers.connectError) handlers.connectError(socket, error);
        reject(error);
      }
    });
    socket.connect({ host: options.hostname || options.host || "localhost", port: options.port });
  });
}
function bunListen(options) {
  if (!options || typeof options !== "object") throw new TypeError("Bun.listen options are required");
  const handlers = options.socket || {};
  let socketData = options.data;
  const server = nodeNet.createServer(socket => {
    attachBunSocket(socket, handlers, socketData);
    if (handlers.open) handlers.open(socket);
  });
  server.data = options.data;
  server.stop = function () { this.close(); };
  server.reload = function (next) {
    if (next && next.socket) Object.assign(handlers, next.socket);
    if (next && Object.prototype.hasOwnProperty.call(next, "data")) {
      socketData = next.data;
      server.data = next.data;
    }
    return server;
  };
  Object.defineProperty(server, "port", { enumerable: true, get() { const a = this.address(); return a && a.port; } });
  Object.defineProperty(server, "hostname", { enumerable: true, get() { const a = this.address(); return a && a.address; } });
  server.listen({ host: options.hostname || "0.0.0.0", port: options.port || 0, exclusive: !!options.exclusive });
  return server;
}
function bunUdpSocket(options = {}) {
  const handlers = options.socket || {};
  const family = options.hostname && String(options.hostname).includes(":") ? "udp6" : "udp4";
  return new Promise((resolve, reject) => {
    const socket = nodeDgram.createSocket(family);
    socket.data = options.data;
    socket.on("message", (message, rinfo) => {
      if (handlers.data) handlers.data(socket, message, rinfo.port, rinfo.address);
    });
    socket.on("error", error => {
      if (handlers.error) handlers.error(socket, error);
      reject(error);
    });
    socket.once("listening", () => {
      if (handlers.open) handlers.open(socket);
      resolve(socket);
    });
    const rawSend = socket.send.bind(socket);
    socket.send = function (data, port, address) {
      const bytes = toU8(data);
      rawSend(bytes, port, address);
      return bytes.length;
    };
    Object.defineProperty(socket, "port", { enumerable: true, get() { return this.address().port; } });
    Object.defineProperty(socket, "hostname", { enumerable: true, get() { return this.address().address; } });
    socket.bind(options.port || 0, options.hostname || "0.0.0.0");
  });
}

// ---- semver -----------------------------------------------------------------------------------
// node-semver-compatible satisfies() and order(). Supports ^ ~ x-ranges hyphen-ranges and the
// comparison operators, with the standard prerelease-tag ordering and gating.
function semverParse(v) {
  const m = /^[v=\s]*(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?\s*$/.exec(String(v));
  if (!m) return null;
  return { major: +m[1], minor: +(m[2] || 0), patch: +(m[3] || 0), pre: m[4] ? m[4].split(".") : [] };
}
function semverCmp(a, b) {
  if (a.major !== b.major) return a.major < b.major ? -1 : 1;
  if (a.minor !== b.minor) return a.minor < b.minor ? -1 : 1;
  if (a.patch !== b.patch) return a.patch < b.patch ? -1 : 1;
  if (a.pre.length && !b.pre.length) return -1;
  if (!a.pre.length && b.pre.length) return 1;
  const n = Math.max(a.pre.length, b.pre.length);
  for (let i = 0; i < n; i++) {
    const x = a.pre[i], y = b.pre[i];
    if (x === undefined) return -1;
    if (y === undefined) return 1;
    const xn = /^\d+$/.test(x), yn = /^\d+$/.test(y);
    if (xn && yn) { if (+x !== +y) return +x < +y ? -1 : 1; }
    else if (xn) return -1;
    else if (yn) return 1;
    else if (x !== y) return x < y ? -1 : 1;
  }
  return 0;
}
function semverCoerce(str) {
  const m = /^[v=\s]*(\d+|x|X|\*)?(?:\.(\d+|x|X|\*))?(?:\.(\d+|x|X|\*))?(?:-([0-9A-Za-z.-]+))?/.exec(String(str).trim());
  const wild = (s) => s === undefined || s === "x" || s === "X" || s === "*";
  return {
    major: wild(m[1]) ? null : +m[1],
    minor: wild(m[2]) ? null : +m[2],
    patch: wild(m[3]) ? null : +m[3],
    pre: m[4] ? m[4].split(".") : [],
  };
}
const semverFill = (p) => ({ major: p.major ?? 0, minor: p.minor ?? 0, patch: p.patch ?? 0, pre: p.pre });
function semverOneComparator(tok) {
  let op = "", rest = tok;
  const opm = /^(>=|<=|>|<|=|\^|~>|~)/.exec(tok);
  if (opm) { op = opm[1]; rest = tok.slice(op.length); }
  const p = semverCoerce(rest);
  if (op === "^") {
    if (p.major === null) return [{ op: ">=", v: { major: 0, minor: 0, patch: 0, pre: [] } }];
    let hi;
    if (p.major !== 0) hi = { major: p.major + 1, minor: 0, patch: 0, pre: [] };
    else if (p.minor === null) hi = { major: 1, minor: 0, patch: 0, pre: [] };
    else if (p.minor !== 0) hi = { major: 0, minor: p.minor + 1, patch: 0, pre: [] };
    else hi = { major: 0, minor: 0, patch: (p.patch ?? 0) + 1, pre: [] };
    return [{ op: ">=", v: semverFill(p) }, { op: "<", v: hi }];
  }
  if (op === "~" || op === "~>") {
    const hi = p.minor === null ? { major: p.major + 1, minor: 0, patch: 0, pre: [] } : { major: p.major, minor: p.minor + 1, patch: 0, pre: [] };
    return [{ op: ">=", v: semverFill(p) }, { op: "<", v: hi }];
  }
  if (op === "" || op === "=") {
    if (p.major === null) return [{ op: ">=", v: { major: 0, minor: 0, patch: 0, pre: [] } }];
    if (p.minor === null) return [{ op: ">=", v: { major: p.major, minor: 0, patch: 0, pre: [] } }, { op: "<", v: { major: p.major + 1, minor: 0, patch: 0, pre: [] } }];
    if (p.patch === null) return [{ op: ">=", v: { major: p.major, minor: p.minor, patch: 0, pre: [] } }, { op: "<", v: { major: p.major, minor: p.minor + 1, patch: 0, pre: [] } }];
    return [{ op: "=", v: semverFill(p) }];
  }
  return [{ op, v: semverFill(p) }];
}
function semverComparators(setStr) {
  setStr = String(setStr).trim();
  if (setStr === "" || setStr === "*" || setStr === "x" || setStr === "X") return [{ op: ">=", v: { major: 0, minor: 0, patch: 0, pre: [] } }];
  const hy = /^(\S+)\s+-\s+(\S+)$/.exec(setStr);
  if (hy) {
    const lo = semverCoerce(hy[1]), hi = semverCoerce(hy[2]);
    const cmps = [{ op: ">=", v: semverFill(lo) }];
    if (hi.minor === null) cmps.push({ op: "<", v: { major: hi.major + 1, minor: 0, patch: 0, pre: [] } });
    else if (hi.patch === null) cmps.push({ op: "<", v: { major: hi.major, minor: hi.minor + 1, patch: 0, pre: [] } });
    else cmps.push({ op: "<=", v: semverFill(hi) });
    return cmps;
  }
  const out = [];
  for (const tok of setStr.split(/\s+/).filter(Boolean)) out.push(...semverOneComparator(tok));
  return out.length ? out : [{ op: ">=", v: { major: 0, minor: 0, patch: 0, pre: [] } }];
}
function semverTest(ver, cmp) {
  const c = semverCmp(ver, cmp.v);
  switch (cmp.op) {
    case "=": return c === 0;
    case ">": return c > 0;
    case ">=": return c >= 0;
    case "<": return c < 0;
    case "<=": return c <= 0;
  }
  return false;
}
const semver = {
  satisfies(version, range) {
    const ver = semverParse(version);
    if (!ver) return false;
    for (const group of String(range).split("||")) {
      const cmps = semverComparators(group.trim());
      let ok = cmps.every((cmp) => semverTest(ver, cmp));
      if (ok && ver.pre.length) {
        ok = cmps.some((cmp) => cmp.v.pre.length && cmp.v.major === ver.major && cmp.v.minor === ver.minor && cmp.v.patch === ver.patch);
      }
      if (ok) return true;
    }
    return false;
  },
  order(a, b) {
    const pa = semverParse(a), pb = semverParse(b);
    if (!pa || !pb) return 0;
    return semverCmp(pa, pb);
  },
};

// ---- assemble ---------------------------------------------------------------------------------
const Bun = {
  semver,
  // identity / env
  version, revision,
  get env() { return globalThis.process.env; },
  get argv() { return globalThis.process.argv; },
  get main() { return globalThis.process.argv[1] ? nodePath.resolve(globalThis.process.argv[1]) : ""; },
  isMainThread: true,
  embeddedFiles: [],
  enableANSIColors: false,

  // timing
  nanoseconds, sleep, sleepSync,

  // strings / structural
  escapeHTML, stripANSI, stringWidth, deepEquals, deepMatch, inspect,

  // hashing
  hash, sha, CryptoHasher, MD4, MD5, SHA1, SHA224, SHA256, SHA384, SHA512, SHA512_256,

  // uuid
  randomUUIDv5, randomUUIDv7,

  // buffers / streams
  allocUnsafe, concatArrayBuffers, indexOfLine, ArrayBufferSink,
  readableStreamToArray, readableStreamToArrayBuffer, readableStreamToBytes, readableStreamToBlob,
  readableStreamToText, readableStreamToJSON, readableStreamToFormData,

  // fs / io
  file, write, stdin, stdout, stderr, Glob,

  // compression
  gzipSync, gunzipSync, deflateSync, inflateSync,

  // process / shell / net
  $, spawn, spawnSync, serve, dns,
  which, peek, fileURLToPath: (u) => nodeUrl.fileURLToPath(u), pathToFileURL: (p) => nodeUrl.pathToFileURL(p),
  resolve, resolveSync,

  // color / config formats
  color, TOML, YAML,

  // networking / global helpers
  fetch: (...a) => globalThis.fetch(...a),

  // memory / misc
  gc, shrink, unsafe,

  // honest stubs
  password, secrets, CSRF, FFI,
  openInEditor: notImpl("Bun.openInEditor"),
  mmap: notImpl("Bun.mmap"),
  generateHeapSnapshot: notImpl("Bun.generateHeapSnapshot"),
  plugin: notImpl("Bun.plugin"),
  build: notImpl("Bun.build"),
  Transpiler: throwClass("Bun.Transpiler"),
  FileSystemRouter: throwClass("Bun.FileSystemRouter"),
  connect: bunConnect,
  listen: bunListen,
  udpSocket: bunUdpSocket,
  Cookie: throwClass("Bun.Cookie"),
  CookieMap: throwClass("Bun.CookieMap"),
  RedisClient: globalThis.__lumenRedisClient,
  redis: undefined,
  S3Client: throwClass("Bun.S3Client"),
  s3: undefined,
  SQL: throwClass("Bun.SQL"),
  sql: undefined,
  postgres: notImpl("Bun.postgres"),
  zstdCompressSync,
  zstdDecompressSync,
  zstdCompress,
  zstdDecompress,
};

let defaultRedisClient;
Object.defineProperty(Bun, "redis", {
  enumerable: true, configurable: true,
  get() {
    if (!defaultRedisClient) defaultRedisClient = new Bun.RedisClient();
    return defaultRedisClient;
  },
});

// `s3`/`sql` are lazy throwing getters in Bun; the keys are present, but touching them fails
// honestly rather than reading `undefined`.
for (const k of ["s3", "sql"]) {
  Object.defineProperty(Bun, k, {
    enumerable: true, configurable: true,
    get() { throw new Error(`Bun.${k} is not supported in lumen`); },
  });
}

__builtins.set("bun", Bun);

// The Bun global: same object identity, non-enumerable like other host globals.
Object.defineProperty(globalThis, "Bun", { value: Bun, writable: true, enumerable: false, configurable: true });
