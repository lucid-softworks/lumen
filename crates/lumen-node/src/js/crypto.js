// node:crypto — the subset real-world server code hits: createHash (sha1/sha256/md5),
// createHmac, randomBytes/randomUUID/randomFillSync. Hashing is pure JS (the engine's SHA-256 op
// is async via SubtleCrypto and not reachable here); randomness bridges to the web crypto global.
// Not covered: cipher/decipher, sign/verify, KDFs, X.509 — they need real crypto primitives
// (STOP-AND-FLAG territory), and no dep in the Express stack uses them.

const webCrypto = globalThis.crypto;

function toBytes(data, encoding) {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (typeof data === "string") return Buffer.from(data, encoding || "utf8");
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  throw new TypeError("crypto: data must be a string or BufferSource");
}

// ---- SHA-1 (pure JS) --------------------------------------------------------------------------

function sha1(bytes) {
  const ml = bytes.length * 8;
  const withOne = bytes.length + 1;
  const total = withOne + ((56 - (withOne % 64) + 64) % 64) + 8;
  const msg = new Uint8Array(total);
  msg.set(bytes);
  msg[bytes.length] = 0x80;
  const dv = new DataView(msg.buffer);
  dv.setUint32(total - 4, ml >>> 0);
  dv.setUint32(total - 8, Math.floor(ml / 0x100000000));

  let h0 = 0x67452301, h1 = 0xefcdab89, h2 = 0x98badcfe, h3 = 0x10325476, h4 = 0xc3d2e1f0;
  const w = new Uint32Array(80);
  const rotl = (n, b) => (n << b) | (n >>> (32 - b));
  for (let i = 0; i < total; i += 64) {
    for (let j = 0; j < 16; j++) w[j] = dv.getUint32(i + j * 4);
    for (let j = 16; j < 80; j++) w[j] = rotl(w[j - 3] ^ w[j - 8] ^ w[j - 14] ^ w[j - 16], 1);
    let a = h0, b = h1, c = h2, d = h3, e = h4;
    for (let j = 0; j < 80; j++) {
      let f, k;
      if (j < 20) { f = (b & c) | (~b & d); k = 0x5a827999; }
      else if (j < 40) { f = b ^ c ^ d; k = 0x6ed9eba1; }
      else if (j < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8f1bbcdc; }
      else { f = b ^ c ^ d; k = 0xca62c1d6; }
      const t = (rotl(a, 5) + f + e + k + w[j]) >>> 0;
      e = d; d = c; c = rotl(b, 30); b = a; a = t;
    }
    h0 = (h0 + a) >>> 0; h1 = (h1 + b) >>> 0; h2 = (h2 + c) >>> 0; h3 = (h3 + d) >>> 0; h4 = (h4 + e) >>> 0;
  }
  const out = new Uint8Array(20);
  new DataView(out.buffer).setUint32(0, h0);
  new DataView(out.buffer).setUint32(4, h1);
  new DataView(out.buffer).setUint32(8, h2);
  new DataView(out.buffer).setUint32(12, h3);
  new DataView(out.buffer).setUint32(16, h4);
  return out;
}

// ---- SHA-256 (pure JS) ------------------------------------------------------------------------

const K256 = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

function sha256(bytes) {
  const ml = bytes.length * 8;
  const withOne = bytes.length + 1;
  const total = withOne + ((56 - (withOne % 64) + 64) % 64) + 8;
  const msg = new Uint8Array(total);
  msg.set(bytes);
  msg[bytes.length] = 0x80;
  const dv = new DataView(msg.buffer);
  dv.setUint32(total - 4, ml >>> 0);
  dv.setUint32(total - 8, Math.floor(ml / 0x100000000));

  const h = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ]);
  const w = new Uint32Array(64);
  const rotr = (n, b) => (n >>> b) | (n << (32 - b));
  for (let i = 0; i < total; i += 64) {
    for (let j = 0; j < 16; j++) w[j] = dv.getUint32(i + j * 4);
    for (let j = 16; j < 64; j++) {
      const s0 = rotr(w[j - 15], 7) ^ rotr(w[j - 15], 18) ^ (w[j - 15] >>> 3);
      const s1 = rotr(w[j - 2], 17) ^ rotr(w[j - 2], 19) ^ (w[j - 2] >>> 10);
      w[j] = (w[j - 16] + s0 + w[j - 7] + s1) >>> 0;
    }
    let [a, b, c, d, e, f, g, hh] = h;
    for (let j = 0; j < 64; j++) {
      const S1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
      const ch = (e & f) ^ (~e & g);
      const t1 = (hh + S1 + ch + K256[j] + w[j]) >>> 0;
      const S0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (S0 + maj) >>> 0;
      hh = g; g = f; f = e; e = (d + t1) >>> 0; d = c; c = b; b = a; a = (t1 + t2) >>> 0;
    }
    h[0] = (h[0] + a) >>> 0; h[1] = (h[1] + b) >>> 0; h[2] = (h[2] + c) >>> 0; h[3] = (h[3] + d) >>> 0;
    h[4] = (h[4] + e) >>> 0; h[5] = (h[5] + f) >>> 0; h[6] = (h[6] + g) >>> 0; h[7] = (h[7] + hh) >>> 0;
  }
  const out = new Uint8Array(32);
  const odv = new DataView(out.buffer);
  for (let j = 0; j < 8; j++) odv.setUint32(j * 4, h[j]);
  return out;
}

const HASHES = { sha1, "sha-1": sha1, sha256, "sha-256": sha256 };

class Hash {
  constructor(algorithm) {
    const fn = HASHES[String(algorithm).toLowerCase()];
    if (!fn) throw new Error(`Digest method not supported: ${algorithm} (lumen: sha1/sha256 only)`);
    this._fn = fn;
    this._chunks = [];
  }
  update(data, encoding) {
    this._chunks.push(toBytes(data, encoding));
    return this;
  }
  digest(encoding) {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const digest = Buffer.from(this._fn(all));
    return encoding ? digest.toString(encoding) : digest;
  }
}

class Hmac {
  constructor(algorithm, key) {
    const fn = HASHES[String(algorithm).toLowerCase()];
    if (!fn) throw new Error(`Digest method not supported: ${algorithm}`);
    this._fn = fn;
    const blockSize = 64;
    let k = toBytes(key);
    if (k.length > blockSize) k = fn(k);
    const pad = new Uint8Array(blockSize);
    pad.set(k);
    this._ipad = pad.map((b) => b ^ 0x36);
    this._opad = pad.map((b) => b ^ 0x5c);
    this._chunks = [this._ipad];
  }
  update(data, encoding) {
    this._chunks.push(toBytes(data, encoding));
    return this;
  }
  digest(encoding) {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const inner = this._fn(all);
    const outer = new Uint8Array(this._opad.length + inner.length);
    outer.set(this._opad);
    outer.set(inner, this._opad.length);
    const digest = Buffer.from(this._fn(outer));
    return encoding ? digest.toString(encoding) : digest;
  }
}

function randomBytes(size, cb) {
  const buf = Buffer.alloc(size);
  webCrypto.getRandomValues(buf);
  if (cb) { queueMicrotask(() => cb(null, buf)); return; }
  return buf;
}

const crypto = {
  createHash: (algorithm) => new Hash(algorithm),
  createHmac: (algorithm, key) => new Hmac(algorithm, key),
  randomBytes,
  randomFillSync: (buf) => (webCrypto.getRandomValues(buf), buf),
  randomUUID: () => webCrypto.randomUUID(),
  // Node's crypto module mirrors the WebCrypto surface at top level.
  getRandomValues: (arr) => webCrypto.getRandomValues(arr),
  subtle: webCrypto.subtle,
  getHashes: () => ["sha1", "sha256"],
  constants: {},
  webcrypto: webCrypto,
};

__builtins.set("crypto", crypto);
