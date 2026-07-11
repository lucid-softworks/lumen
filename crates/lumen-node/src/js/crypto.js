// node:crypto — the subset that has a real, verifiable backing in lumen. Randomness bridges to the
// native web-crypto source (/dev/urandom); hashing (md5/sha1/sha256), HMAC, PBKDF2 and HKDF are
// pure-JS but bit-exact with Node (the probe cross-checks digests/derived keys against Node). No
// native cipher/RSA/EC/DH primitives exist in the engine, so ciphers, public-key sign/verify,
// key generation for asymmetric keys, Diffie-Hellman, and X.509 are honest throwing stubs — they
// refuse loudly rather than returning fake ciphertext or signatures (STOP-AND-FLAG territory for
// crypto correctness).

const webCrypto = globalThis.crypto;

function toBytes(data, encoding) {
  if (data instanceof KeyObject) return data._material;
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (typeof data === "string") return Buffer.from(data, encoding || "utf8");
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  throw new TypeError("crypto: data must be a string or BufferSource");
}

function concatBytes(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
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

// ---- MD5 (pure JS, RFC 1321) ------------------------------------------------------------------

const MD5_S = [
  7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
  5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
  4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
  6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];
const MD5_K = new Uint32Array(64);
for (let i = 0; i < 64; i++) MD5_K[i] = Math.floor(Math.abs(Math.sin(i + 1)) * 4294967296) >>> 0;

function md5(bytes) {
  const ml = bytes.length * 8;
  const withOne = bytes.length + 1;
  const total = withOne + ((56 - (withOne % 64) + 64) % 64) + 8;
  const msg = new Uint8Array(total);
  msg.set(bytes);
  msg[bytes.length] = 0x80;
  const dv = new DataView(msg.buffer);
  dv.setUint32(total - 8, ml >>> 0, true);
  dv.setUint32(total - 4, Math.floor(ml / 0x100000000), true);

  let a0 = 0x67452301, b0 = 0xefcdab89, c0 = 0x98badcfe, d0 = 0x10325476;
  const M = new Uint32Array(16);
  const rotl = (x, c) => ((x << c) | (x >>> (32 - c))) >>> 0;
  for (let i = 0; i < total; i += 64) {
    for (let j = 0; j < 16; j++) M[j] = dv.getUint32(i + j * 4, true);
    let A = a0, B = b0, C = c0, D = d0;
    for (let j = 0; j < 64; j++) {
      let F, g;
      if (j < 16) { F = (B & C) | (~B & D); g = j; }
      else if (j < 32) { F = (D & B) | (~D & C); g = (5 * j + 1) % 16; }
      else if (j < 48) { F = B ^ C ^ D; g = (3 * j + 5) % 16; }
      else { F = C ^ (B | ~D); g = (7 * j) % 16; }
      F = (F + A + MD5_K[j] + M[g]) >>> 0;
      A = D; D = C; C = B;
      B = (B + rotl(F, MD5_S[j])) >>> 0;
    }
    a0 = (a0 + A) >>> 0; b0 = (b0 + B) >>> 0; c0 = (c0 + C) >>> 0; d0 = (d0 + D) >>> 0;
  }
  const out = new Uint8Array(16);
  const odv = new DataView(out.buffer);
  odv.setUint32(0, a0, true); odv.setUint32(4, b0, true);
  odv.setUint32(8, c0, true); odv.setUint32(12, d0, true);
  return out;
}

// ---- hash registry ----------------------------------------------------------------------------
// Every algorithm here is bit-exact with Node (verified in the probe). blockSize is the HMAC block
// size (64 bytes for all three md5/sha1/sha256).

const HASH_REG = {
  md5: { fn: md5, outLen: 16, blockSize: 64 },
  sha1: { fn: sha1, outLen: 20, blockSize: 64 },
  sha256: { fn: sha256, outLen: 32, blockSize: 64 },
};
HASH_REG["sha-1"] = HASH_REG.sha1;
HASH_REG["sha-256"] = HASH_REG.sha256;
HASH_REG["ssl3-md5"] = HASH_REG.md5;
HASH_REG["ssl3-sha1"] = HASH_REG.sha1;

function resolveHash(algorithm) {
  const reg = HASH_REG[String(algorithm).toLowerCase()];
  if (!reg) {
    throw new Error(
      `Digest method not supported: ${algorithm} (lumen supports md5, sha1, sha256)`,
    );
  }
  return reg;
}

// Raw HMAC over Uint8Array inputs — the shared core for the Hmac class, PBKDF2 and HKDF.
function hmacRaw(reg, keyBytes, dataBytes) {
  const bs = reg.blockSize;
  let k = keyBytes;
  if (k.length > bs) k = reg.fn(k);
  const ipad = new Uint8Array(bs);
  const opad = new Uint8Array(bs);
  for (let i = 0; i < bs; i++) {
    const kb = i < k.length ? k[i] : 0;
    ipad[i] = kb ^ 0x36;
    opad[i] = kb ^ 0x5c;
  }
  const inner = reg.fn(concatBytes(ipad, dataBytes));
  return reg.fn(concatBytes(opad, inner));
}

// ---- Hash / Hmac classes ----------------------------------------------------------------------

class Hash {
  constructor(algorithm) {
    this._reg = resolveHash(algorithm);
    this._chunks = [];
    this._used = false;
  }
  update(data, encoding) {
    this._chunks.push(toBytes(data, encoding));
    return this;
  }
  _bytes() {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    return all;
  }
  digest(encoding) {
    if (this._used) throw new Error("Digest already called");
    this._used = true;
    const digest = Buffer.from(this._reg.fn(this._bytes()));
    return encoding && encoding !== "buffer" ? digest.toString(encoding) : digest;
  }
  copy() {
    const h = new Hash("md5");
    h._reg = this._reg;
    h._chunks = this._chunks.slice();
    return h;
  }
}

class Hmac {
  constructor(algorithm, key) {
    this._reg = resolveHash(algorithm);
    this._key = key instanceof KeyObject ? key._material : toBytes(key);
    this._chunks = [];
    this._used = false;
  }
  update(data, encoding) {
    this._chunks.push(toBytes(data, encoding));
    return this;
  }
  digest(encoding) {
    if (this._used) throw new Error("Digest already called");
    this._used = true;
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const digest = Buffer.from(hmacRaw(this._reg, this._key, all));
    return encoding && encoding !== "buffer" ? digest.toString(encoding) : digest;
  }
}

// ---- one-shot hash (Node 22 crypto.hash) ------------------------------------------------------

function hash(algorithm, data, outputEncoding) {
  const enc = outputEncoding === undefined ? "hex" : outputEncoding;
  const h = new Hash(algorithm);
  h.update(data);
  return enc === "buffer" ? h.digest() : h.digest(enc);
}

// ---- KDFs (pure JS, bit-exact with Node) ------------------------------------------------------

function pbkdf2Sync(password, salt, iterations, keylen, digest) {
  if (digest === undefined) throw new TypeError("The \"digest\" argument is required");
  const reg = resolveHash(digest);
  const pw = toBytes(password);
  const saltBytes = toBytes(salt);
  const hLen = reg.outLen;
  const numBlocks = Math.ceil(keylen / hLen);
  const out = Buffer.alloc(numBlocks * hLen);
  const idx = new Uint8Array(4);
  for (let i = 1; i <= numBlocks; i++) {
    idx[0] = (i >>> 24) & 0xff; idx[1] = (i >>> 16) & 0xff; idx[2] = (i >>> 8) & 0xff; idx[3] = i & 0xff;
    let u = hmacRaw(reg, pw, concatBytes(saltBytes, idx));
    const t = Uint8Array.from(u);
    for (let j = 1; j < iterations; j++) {
      u = hmacRaw(reg, pw, u);
      for (let k = 0; k < t.length; k++) t[k] ^= u[k];
    }
    out.set(t, (i - 1) * hLen);
  }
  return out.subarray(0, keylen);
}

function pbkdf2(password, salt, iterations, keylen, digest, cb) {
  if (typeof digest === "function") { cb = digest; digest = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let result, err;
  try { result = pbkdf2Sync(password, salt, iterations, keylen, digest); }
  catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, result)));
}

function hkdfSync(digest, ikm, salt, info, keylen) {
  const reg = resolveHash(digest);
  const ikmB = toBytes(ikm);
  const saltB = toBytes(salt);
  const infoB = toBytes(info);
  const hLen = reg.outLen;
  // RFC 5869: an empty salt is replaced by HashLen zero bytes.
  const saltFinal = saltB.length ? saltB : new Uint8Array(hLen);
  const prk = hmacRaw(reg, saltFinal, ikmB); // extract
  const n = Math.ceil(keylen / hLen);
  if (n > 255) throw new RangeError("hkdf: requested key length is too large");
  const okm = new Uint8Array(n * hLen);
  let prev = new Uint8Array(0);
  for (let i = 1; i <= n; i++) {
    const input = concatBytes(concatBytes(prev, infoB), new Uint8Array([i]));
    prev = hmacRaw(reg, prk, input);
    okm.set(prev, (i - 1) * hLen);
  }
  return okm.slice(0, keylen).buffer; // Node returns an ArrayBuffer
}

function hkdf(digest, ikm, salt, info, keylen, cb) {
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let result, err;
  try { result = hkdfSync(digest, ikm, salt, info, keylen); }
  catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, result)));
}

// ---- randomness (native /dev/urandom via web crypto) ------------------------------------------

function fillRandom(view) {
  const CHUNK = 65536; // getRandomValues quota
  for (let off = 0; off < view.length; off += CHUNK) {
    const end = Math.min(off + CHUNK, view.length);
    webCrypto.getRandomValues(view.subarray(off, end));
  }
}

function randomBytes(size, cb) {
  const buf = Buffer.alloc(size);
  fillRandom(buf);
  if (cb) { queueMicrotask(() => cb(null, buf)); return; }
  return buf;
}

function randomFillSync(buf, offset, size) {
  const view = ArrayBuffer.isView(buf)
    ? new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength)
    : new Uint8Array(buf);
  const off = offset === undefined ? 0 : offset;
  const sz = size === undefined ? view.length - off : size;
  const rnd = randomBytes(sz);
  view.set(rnd, off);
  return buf;
}

function randomFill(buf, offset, size, cb) {
  if (typeof offset === "function") { cb = offset; offset = undefined; size = undefined; }
  else if (typeof size === "function") { cb = size; size = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let err;
  try { randomFillSync(buf, offset, size); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, buf)));
}

function randomInt(...args) {
  let cb;
  if (typeof args[args.length - 1] === "function") cb = args.pop();
  let min, max;
  if (args.length === 1) { min = 0; max = args[0]; }
  else { min = args[0]; max = args[1]; }
  if (!Number.isSafeInteger(min) || !Number.isSafeInteger(max)) {
    throw new RangeError("randomInt: min and max must be safe integers");
  }
  const range = max - min;
  if (range <= 0) {
    throw new RangeError('The value of "max" is out of range. It must be greater than the value of "min".');
  }
  const draw = () => {
    const bits = Math.ceil(Math.log2(range));
    const bytes = Math.max(1, Math.ceil(bits / 8));
    const mod = Math.pow(2, bytes * 8);
    const limit = mod - (mod % range);
    for (;;) {
      const b = randomBytes(bytes);
      let v = 0;
      for (let i = 0; i < bytes; i++) v = v * 256 + b[i];
      if (v < limit) return min + (v % range);
    }
  };
  if (cb) {
    let result, err;
    try { result = draw(); } catch (e) { err = e; }
    queueMicrotask(() => (err ? cb(err) : cb(null, result)));
    return;
  }
  return draw();
}

function timingSafeEqual(a, b) {
  const ab = toBytes(a);
  const bb = toBytes(b);
  if (ab.length !== bb.length) {
    throw new RangeError("Input buffers must have the same byte length");
  }
  let diff = 0;
  for (let i = 0; i < ab.length; i++) diff |= ab[i] ^ bb[i];
  return diff === 0;
}

// ---- secret KeyObjects ------------------------------------------------------------------------

class KeyObject {
  constructor(type, material) {
    this._type = type;
    this._material = material;
  }
  get type() { return this._type; }
  get symmetricKeySize() { return this._type === "secret" ? this._material.length : undefined; }
  get asymmetricKeyType() { return undefined; }
  get asymmetricKeyDetails() { return undefined; }
  export(options) {
    if (this._type !== "secret") {
      throw new Error("KeyObject.export for asymmetric keys is not supported in lumen");
    }
    if (options && options.format === "jwk") {
      const k = Buffer.from(this._material)
        .toString("base64")
        .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      return { kty: "oct", k };
    }
    return Buffer.from(this._material);
  }
  equals(other) {
    if (!(other instanceof KeyObject) || other._type !== this._type) return false;
    if (this._material.length !== other._material.length) return false;
    return timingSafeEqual(this._material, other._material);
  }
}

function createSecretKey(key, encoding) {
  const bytes = typeof key === "string" ? Buffer.from(key, encoding || "utf8") : toBytes(key);
  return new KeyObject("secret", Uint8Array.from(bytes));
}

function generateKeySync(type, options) {
  const t = String(type).toLowerCase();
  if (t !== "hmac" && t !== "aes") {
    throw new Error(`Unsupported key type '${type}' in lumen (secret 'hmac'/'aes' only)`);
  }
  const length = options && options.length;
  if (!Number.isInteger(length)) throw new TypeError("options.length must be an integer number of bits");
  if (length % 8 !== 0) throw new RangeError("options.length must be a multiple of 8");
  return new KeyObject("secret", Uint8Array.from(randomBytes(length / 8)));
}

function generateKey(type, options, cb) {
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let key, err;
  try { key = generateKeySync(type, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, key)));
}

// ---- honest stubs (no native primitive backs these) -------------------------------------------

function notImpl(name) {
  return () => {
    throw new Error(`node:crypto ${name} is not supported in lumen (no native primitive available)`);
  };
}

// getCipherInfo returns undefined for unknown ciphers in Node; lumen supports none, so every
// query is "unknown" — this matches Node's contract rather than inventing cipher metadata.
const getCipherInfo = () => undefined;

// ---- constants (real OpenSSL values, captured from Node v22) -----------------------------------

const constants = {
  OPENSSL_VERSION_NUMBER: 805306624,
  SSL_OP_ALL: 2147485776, SSL_OP_ALLOW_NO_DHE_KEX: 1024,
  SSL_OP_ALLOW_UNSAFE_LEGACY_RENEGOTIATION: 262144, SSL_OP_CIPHER_SERVER_PREFERENCE: 4194304,
  SSL_OP_CISCO_ANYCONNECT: 32768, SSL_OP_COOKIE_EXCHANGE: 8192,
  SSL_OP_CRYPTOPRO_TLSEXT_BUG: 2147483648, SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS: 2048,
  SSL_OP_LEGACY_SERVER_CONNECT: 4, SSL_OP_NO_COMPRESSION: 131072,
  SSL_OP_NO_ENCRYPT_THEN_MAC: 524288, SSL_OP_NO_QUERY_MTU: 4096,
  SSL_OP_NO_RENEGOTIATION: 1073741824, SSL_OP_NO_SESSION_RESUMPTION_ON_RENEGOTIATION: 65536,
  SSL_OP_NO_SSLv2: 0, SSL_OP_NO_SSLv3: 33554432, SSL_OP_NO_TICKET: 16384,
  SSL_OP_NO_TLSv1: 67108864, SSL_OP_NO_TLSv1_1: 268435456, SSL_OP_NO_TLSv1_2: 134217728,
  SSL_OP_NO_TLSv1_3: 536870912, SSL_OP_PRIORITIZE_CHACHA: 2097152, SSL_OP_TLS_ROLLBACK_BUG: 8388608,
  ENGINE_METHOD_RSA: 1, ENGINE_METHOD_DSA: 2, ENGINE_METHOD_DH: 4, ENGINE_METHOD_RAND: 8,
  ENGINE_METHOD_EC: 2048, ENGINE_METHOD_CIPHERS: 64, ENGINE_METHOD_DIGESTS: 128,
  ENGINE_METHOD_PKEY_METHS: 512, ENGINE_METHOD_PKEY_ASN1_METHS: 1024, ENGINE_METHOD_ALL: 65535,
  ENGINE_METHOD_NONE: 0,
  DH_CHECK_P_NOT_SAFE_PRIME: 2, DH_CHECK_P_NOT_PRIME: 1, DH_UNABLE_TO_CHECK_GENERATOR: 4,
  DH_NOT_SUITABLE_GENERATOR: 8,
  RSA_PKCS1_PADDING: 1, RSA_NO_PADDING: 3, RSA_PKCS1_OAEP_PADDING: 4, RSA_X931_PADDING: 5,
  RSA_PKCS1_PSS_PADDING: 6, RSA_PSS_SALTLEN_DIGEST: -1, RSA_PSS_SALTLEN_MAX_SIGN: -2,
  RSA_PSS_SALTLEN_AUTO: -2,
  defaultCoreCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA",
  TLS1_VERSION: 769, TLS1_1_VERSION: 770, TLS1_2_VERSION: 771, TLS1_3_VERSION: 772,
  POINT_CONVERSION_COMPRESSED: 2, POINT_CONVERSION_UNCOMPRESSED: 4, POINT_CONVERSION_HYBRID: 6,
  defaultCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA",
};

// ---- module surface ---------------------------------------------------------------------------

const crypto = {
  // -- real: hashing / MAC / KDF --
  createHash: (algorithm) => new Hash(algorithm),
  createHmac: (algorithm, key) => new Hmac(algorithm, key),
  Hash,
  Hmac,
  hash,
  getHashes: () => ["md5", "sha1", "sha256"],
  pbkdf2,
  pbkdf2Sync,
  hkdf,
  hkdfSync,

  // -- real: randomness --
  randomBytes,
  randomFill,
  randomFillSync,
  randomInt,
  randomUUID: () => webCrypto.randomUUID(),
  getRandomValues: (arr) => webCrypto.getRandomValues(arr),
  timingSafeEqual,

  // -- real: secret keys --
  KeyObject,
  createSecretKey,
  generateKey,
  generateKeySync,

  // -- real: introspection / config --
  // lumen backs no symmetric ciphers or named curves, so these are honestly empty.
  getCiphers: () => [],
  getCurves: () => [],
  getCipherInfo,
  getFips: () => 0,
  setFips: (v) => { if (v) throw new Error("FIPS mode is not supported in lumen"); },
  secureHeapUsed: () => ({ total: 0, min: 0, used: 0, utilization: 0 }),
  constants,

  // -- real: WebCrypto bridge (subtle backs SHA-256 digest + getRandomValues/randomUUID) --
  webcrypto: webCrypto,
  subtle: webCrypto.subtle,

  // -- stubs: symmetric ciphers (no AES/ChaCha primitive) --
  createCipheriv: notImpl("createCipheriv"),
  createDecipheriv: notImpl("createDecipheriv"),
  Cipher: notImpl("Cipher"),
  Cipheriv: notImpl("Cipheriv"),
  Decipher: notImpl("Decipher"),
  Decipheriv: notImpl("Decipheriv"),

  // -- stubs: public-key sign/verify & encryption (no RSA/EC/DSA primitive) --
  createSign: notImpl("createSign"),
  createVerify: notImpl("createVerify"),
  Sign: notImpl("Sign"),
  Verify: notImpl("Verify"),
  sign: notImpl("sign"),
  verify: notImpl("verify"),
  privateEncrypt: notImpl("privateEncrypt"),
  privateDecrypt: notImpl("privateDecrypt"),
  publicEncrypt: notImpl("publicEncrypt"),
  publicDecrypt: notImpl("publicDecrypt"),

  // -- stubs: asymmetric key management (no ASN.1/PEM key backend) --
  createPublicKey: notImpl("createPublicKey"),
  createPrivateKey: notImpl("createPrivateKey"),
  generateKeyPair: notImpl("generateKeyPair"),
  generateKeyPairSync: notImpl("generateKeyPairSync"),

  // -- stubs: Diffie-Hellman / ECDH (no bignum/EC primitive) --
  DiffieHellman: notImpl("DiffieHellman"),
  DiffieHellmanGroup: notImpl("DiffieHellmanGroup"),
  ECDH: notImpl("ECDH"),
  createDiffieHellman: notImpl("createDiffieHellman"),
  createDiffieHellmanGroup: notImpl("createDiffieHellmanGroup"),
  createECDH: notImpl("createECDH"),
  getDiffieHellman: notImpl("getDiffieHellman"),
  diffieHellman: notImpl("diffieHellman"),

  // -- stubs: scrypt (memory-hard; no native backing, refused rather than shipped unverified) --
  scrypt: notImpl("scrypt"),
  scryptSync: notImpl("scryptSync"),

  // -- stubs: primes (no bignum primitive) --
  checkPrime: notImpl("checkPrime"),
  checkPrimeSync: notImpl("checkPrimeSync"),
  generatePrime: notImpl("generatePrime"),
  generatePrimeSync: notImpl("generatePrimeSync"),

  // -- stubs: X.509 / legacy SPKAC --
  X509Certificate: notImpl("X509Certificate"),
  Certificate: notImpl("Certificate"),

  // -- stubs: engines --
  setEngine: notImpl("setEngine"),
};

__builtins.set("crypto", crypto);
