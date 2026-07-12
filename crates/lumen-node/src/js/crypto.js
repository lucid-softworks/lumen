// node:crypto — the subset that has a real, verifiable backing in lumen. Randomness bridges to the
// native web-crypto source (/dev/urandom); hashing (md5/sha1/sha256/sha384/sha512), HMAC, PBKDF2
// and HKDF are pure-JS but bit-exact with Node (cross-checked against Node). Asymmetric crypto is
// pure-JS over BigInt: Ed25519/X25519 sign/verify + key generation, and ASN.1 DER/PEM/JWK key
// plumbing (createPublicKey/createPrivateKey, KeyObject.export as pkcs1/sec1/pkcs8/spki ×
// pem/der/jwk) for RSA, Ed25519, X25519 and EC P-256 — all cross-verified against Node v22 in
// both directions. Not constant-time (correctness-first, not an HSM). Native code adds the
// SHA-512 family, scrypt ROMix, and symmetric AES ciphers
// aes-{128,192,256}-{ecb,cbc,ctr,gcm} via createCipheriv/createDecipheriv (PKCS#7 padding,
// streaming update/final, GCM AAD + auth tags). DH/ECDH, ECDSA, X.509, and non-AES ciphers
// remain honest throwing stubs.

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
// size (64 bytes for md5/sha1/sha256; 128 for the SHA-512 family, which is native — see
// lumen-node/src/crypto.rs).

const HASH_REG = {
  md5: { fn: md5, outLen: 16, blockSize: 64 },
  sha1: { fn: sha1, outLen: 20, blockSize: 64 },
  sha256: { fn: sha256, outLen: 32, blockSize: 64 },
  sha512: { fn: (b) => __crypto.sha512(0, b), outLen: 64, blockSize: 128 },
  sha384: { fn: (b) => __crypto.sha512(1, b), outLen: 48, blockSize: 128 },
  "sha512-224": { fn: (b) => __crypto.sha512(2, b), outLen: 28, blockSize: 128 },
  "sha512-256": { fn: (b) => __crypto.sha512(3, b), outLen: 32, blockSize: 128 },
};
HASH_REG["sha-1"] = HASH_REG.sha1;
HASH_REG["sha-256"] = HASH_REG.sha256;
HASH_REG["sha-384"] = HASH_REG.sha384;
HASH_REG["sha-512"] = HASH_REG.sha512;
HASH_REG["ssl3-md5"] = HASH_REG.md5;
HASH_REG["ssl3-sha1"] = HASH_REG.sha1;

function resolveHash(algorithm) {
  const reg = HASH_REG[String(algorithm).toLowerCase()];
  if (!reg) {
    throw new Error(
      `Digest method not supported: ${algorithm} (lumen supports md5, sha1, sha256, sha384, sha512, sha512-224, sha512-256)`,
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
    this._asym = null; // key struct (see the asymmetric section below) for public/private keys
  }
  get type() { return this._type; }
  get symmetricKeySize() { return this._type === "secret" ? this._material.length : undefined; }
  get asymmetricKeyType() { return this._asym ? this._asym.kind : undefined; }
  get asymmetricKeyDetails() {
    if (!this._asym) return undefined;
    const k = this._asym;
    if (k.kind === "rsa") return { modulusLength: bitLength(k.n), publicExponent: k.e };
    if (k.kind === "ec") return { namedCurve: k.curve };
    return {};
  }
  export(options) {
    if (this._type !== "secret") return exportAsymmetricKey(this, options);
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
    if (this._type !== "secret") {
      const t = this._type === "private" ? "pkcs8" : "spki";
      const a = exportAsymmetricKey(this, { format: "der", type: t });
      const b = exportAsymmetricKey(other, { format: "der", type: t });
      return a.length === b.length && timingSafeEqual(a, b);
    }
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

// ================================================================================================
// ASYMMETRIC CRYPTO (pure JS, BigInt-backed). Implemented tiers are cross-checked bit-for-bit
// against Node v22 / OpenSSL. Not constant-time — a correctness-first implementation for a JS
// runtime, not an HSM. Anything unsupported throws an honest, specific error.
// ================================================================================================

// ---- bignum / byte helpers ----------------------------------------------------------------------

function bytesToBigIntBE(bytes) {
  let n = 0n;
  for (let i = 0; i < bytes.length; i++) n = (n << 8n) | BigInt(bytes[i]);
  return n;
}
function bigIntToBytesBE(n, len) {
  if (n < 0n) throw new RangeError("bigIntToBytesBE: negative");
  const out = [];
  let x = n;
  while (x > 0n) { out.unshift(Number(x & 0xffn)); x >>= 8n; }
  if (out.length === 0) out.push(0);
  let u = Uint8Array.from(out);
  if (len !== undefined) {
    if (u.length > len) throw new RangeError("bigIntToBytesBE: value larger than requested length");
    if (u.length < len) { const o = new Uint8Array(len); o.set(u, len - u.length); u = o; }
  }
  return u;
}
function amod(a, m) { let r = a % m; if (r < 0n) r += m; return r; }
function modPow(base, exp, m) {
  let b = amod(base, m);
  let r = 1n;
  let e = exp;
  while (e > 0n) { if (e & 1n) r = (r * b) % m; b = (b * b) % m; e >>= 1n; }
  return r;
}
function modInv(a, m) {
  let [old_r, r] = [amod(a, m), m];
  let [old_s, s] = [1n, 0n];
  while (r !== 0n) {
    const q = old_r / r;
    [old_r, r] = [r, old_r - q * r];
    [old_s, s] = [s, old_s - q * s];
  }
  if (old_r !== 1n) throw new Error("modInv: value not invertible");
  return amod(old_s, m);
}
function bitLength(n) { let b = 0; let x = n; while (x > 0n) { x >>= 1n; b++; } return b; }
function concatAll(arrs) {
  let total = 0;
  for (const a of arrs) total += a.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const a of arrs) { out.set(a, off); off += a.length; }
  return out;
}

// ---- SHA-512 / SHA-384 (pure JS, BigInt u64; bit-exact with Node — verified) --------------------

const MASK64 = (1n << 64n) - 1n;
const K512 = [
  0x428a2f98d728ae22n, 0x7137449123ef65cdn, 0xb5c0fbcfec4d3b2fn, 0xe9b5dba58189dbbcn,
  0x3956c25bf348b538n, 0x59f111f1b605d019n, 0x923f82a4af194f9bn, 0xab1c5ed5da6d8118n,
  0xd807aa98a3030242n, 0x12835b0145706fben, 0x243185be4ee4b28cn, 0x550c7dc3d5ffb4e2n,
  0x72be5d74f27b896fn, 0x80deb1fe3b1696b1n, 0x9bdc06a725c71235n, 0xc19bf174cf692694n,
  0xe49b69c19ef14ad2n, 0xefbe4786384f25e3n, 0x0fc19dc68b8cd5b5n, 0x240ca1cc77ac9c65n,
  0x2de92c6f592b0275n, 0x4a7484aa6ea6e483n, 0x5cb0a9dcbd41fbd4n, 0x76f988da831153b5n,
  0x983e5152ee66dfabn, 0xa831c66d2db43210n, 0xb00327c898fb213fn, 0xbf597fc7beef0ee4n,
  0xc6e00bf33da88fc2n, 0xd5a79147930aa725n, 0x06ca6351e003826fn, 0x142929670a0e6e70n,
  0x27b70a8546d22ffcn, 0x2e1b21385c26c926n, 0x4d2c6dfc5ac42aedn, 0x53380d139d95b3dfn,
  0x650a73548baf63den, 0x766a0abb3c77b2a8n, 0x81c2c92e47edaee6n, 0x92722c851482353bn,
  0xa2bfe8a14cf10364n, 0xa81a664bbc423001n, 0xc24b8b70d0f89791n, 0xc76c51a30654be30n,
  0xd192e819d6ef5218n, 0xd69906245565a910n, 0xf40e35855771202an, 0x106aa07032bbd1b8n,
  0x19a4c116b8d2d0c8n, 0x1e376c085141ab53n, 0x2748774cdf8eeb99n, 0x34b0bcb5e19b48a8n,
  0x391c0cb3c5c95a63n, 0x4ed8aa4ae3418acbn, 0x5b9cca4f7763e373n, 0x682e6ff3d6b2b8a3n,
  0x748f82ee5defb2fcn, 0x78a5636f43172f60n, 0x84c87814a1f0ab72n, 0x8cc702081a6439ecn,
  0x90befffa23631e28n, 0xa4506cebde82bde9n, 0xbef9a3f7b2c67915n, 0xc67178f2e372532bn,
  0xca273eceea26619cn, 0xd186b8c721c0c207n, 0xeada7dd6cde0eb1en, 0xf57d4f7fee6ed178n,
  0x06f067aa72176fban, 0x0a637dc5a2c898a6n, 0x113f9804bef90daen, 0x1b710b35131c471bn,
  0x28db77f523047d84n, 0x32caab7b40c72493n, 0x3c9ebe0a15c9bebcn, 0x431d67c49c100d4cn,
  0x4cc5d4becb3e42b6n, 0x597f299cfc657e2an, 0x5fcb6fab3ad6faecn, 0x6c44198c4a475817n,
];
const rotr64 = (x, n) => ((x >> n) | (x << (64n - n))) & MASK64;
function sha512core(bytes, iv, outLen) {
  const ml = BigInt(bytes.length) * 8n;
  const withOne = bytes.length + 1;
  const total = withOne + (((112 - (withOne % 128)) + 128) % 128) + 16;
  const msg = new Uint8Array(total);
  msg.set(bytes);
  msg[bytes.length] = 0x80;
  for (let i = 0; i < 8; i++) msg[total - 1 - i] = Number((ml >> BigInt(8 * i)) & 0xffn);
  let h = iv.slice();
  const w = new Array(80);
  for (let i = 0; i < total; i += 128) {
    for (let j = 0; j < 16; j++) {
      let v = 0n;
      for (let k = 0; k < 8; k++) v = (v << 8n) | BigInt(msg[i + j * 8 + k]);
      w[j] = v;
    }
    for (let j = 16; j < 80; j++) {
      const s0 = rotr64(w[j - 15], 1n) ^ rotr64(w[j - 15], 8n) ^ (w[j - 15] >> 7n);
      const s1 = rotr64(w[j - 2], 19n) ^ rotr64(w[j - 2], 61n) ^ (w[j - 2] >> 6n);
      w[j] = (w[j - 16] + s0 + w[j - 7] + s1) & MASK64;
    }
    let [a, b, c, d, e, f, g, hh] = h;
    for (let j = 0; j < 80; j++) {
      const S1 = rotr64(e, 14n) ^ rotr64(e, 18n) ^ rotr64(e, 41n);
      const ch = (e & f) ^ ((~e & MASK64) & g);
      const t1 = (hh + S1 + ch + K512[j] + w[j]) & MASK64;
      const S0 = rotr64(a, 28n) ^ rotr64(a, 34n) ^ rotr64(a, 39n);
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (S0 + maj) & MASK64;
      hh = g; g = f; f = e; e = (d + t1) & MASK64; d = c; c = b; b = a; a = (t1 + t2) & MASK64;
    }
    h = [(h[0] + a) & MASK64, (h[1] + b) & MASK64, (h[2] + c) & MASK64, (h[3] + d) & MASK64,
      (h[4] + e) & MASK64, (h[5] + f) & MASK64, (h[6] + g) & MASK64, (h[7] + hh) & MASK64];
  }
  const out = new Uint8Array(64);
  for (let i = 0; i < 8; i++) for (let k = 0; k < 8; k++) out[i * 8 + k] = Number((h[i] >> BigInt(56 - 8 * k)) & 0xffn);
  return outLen === 64 ? out : Uint8Array.from(out.subarray(0, outLen));
}
const SHA512_IV = [
  0x6a09e667f3bcc908n, 0xbb67ae8584caa73bn, 0x3c6ef372fe94f82bn, 0xa54ff53a5f1d36f1n,
  0x510e527fade682d1n, 0x9b05688c2b3e6c1fn, 0x1f83d9abfb41bd6bn, 0x5be0cd19137e2179n];
const SHA384_IV = [
  0xcbbb9d5dc1059ed8n, 0x629a292a367cd507n, 0x9159015a3070dd17n, 0x152fecd8f70e5939n,
  0x67332667ffc00b31n, 0x8eb44a8768581511n, 0xdb0c2e0d64f98fa7n, 0x47b5481dbefa4fa4n];
function sha512(bytes) { return sha512core(bytes, SHA512_IV, 64); }
function sha384(bytes) { return sha512core(bytes, SHA384_IV, 48); }

// Register the SHA-512 family so Hash/Hmac/pbkdf2/hkdf and the public-key code share one
// implementation. HMAC block size for SHA-384/512 is 128 bytes.
HASH_REG.sha512 = { fn: sha512, outLen: 64, blockSize: 128 };
HASH_REG.sha384 = { fn: sha384, outLen: 48, blockSize: 128 };
HASH_REG["sha-512"] = HASH_REG.sha512;
HASH_REG["sha-384"] = HASH_REG.sha384;

// Digest-name normalisation for public-key algorithms ("RSA-SHA256", "sha256WithRSAEncryption",
// "ecdsa-with-SHA256" …).
function normalizeDigest(name) {
  let s = String(name).toLowerCase().replace(/^rsa-/, "").replace(/withrsaencryption$/, "");
  s = s.replace(/^ecdsa-with-/, "").replace(/^id-/, "");
  return s;
}

// ---- ASN.1 DER (parse + serialize) + PEM armor --------------------------------------------------

function derRead(buf, off) {
  const tag = buf[off];
  let i = off + 1;
  let len = buf[i++];
  if (len & 0x80) {
    const n = len & 0x7f;
    len = 0;
    for (let j = 0; j < n; j++) len = len * 256 + buf[i++];
  }
  if (i + len > buf.length) throw new Error("ASN.1: truncated DER");
  return { tag, hstart: off, start: i, end: i + len, content: buf.subarray(i, i + len) };
}
function derChildren(buf) {
  const out = [];
  let off = 0;
  while (off < buf.length) {
    const t = derRead(buf, off);
    out.push(t);
    off = t.end;
  }
  return out;
}
function derInt2Big(node) { return bytesToBigIntBE(node.content); }

function derLen(n) {
  if (n < 0x80) return Uint8Array.of(n);
  const b = [];
  let x = n;
  while (x > 0) { b.unshift(x & 0xff); x = Math.floor(x / 256); }
  return Uint8Array.of(0x80 | b.length, ...b);
}
function derTLV(tag, content) { return concatAll([Uint8Array.of(tag), derLen(content.length), content]); }
function derIntFromBig(n) {
  let bytes = bigIntToBytesBE(n);
  if (bytes[0] & 0x80) bytes = concatAll([Uint8Array.of(0), bytes]);
  return derTLV(0x02, bytes);
}
function derSeq(children) { return derTLV(0x30, concatAll(children)); }
function derBitString(bytes) { return derTLV(0x03, concatAll([Uint8Array.of(0), bytes])); }
function derOctet(bytes) { return derTLV(0x04, bytes); }
function derNull() { return Uint8Array.of(0x05, 0x00); }
function encodeOIDBody(str) {
  const parts = str.split(".").map(Number);
  const bytes = [40 * parts[0] + parts[1]];
  for (let i = 2; i < parts.length; i++) {
    let v = parts[i];
    const stack = [v & 0x7f];
    v = Math.floor(v / 128);
    while (v > 0) { stack.unshift((v & 0x7f) | 0x80); v = Math.floor(v / 128); }
    for (const s of stack) bytes.push(s);
  }
  return Uint8Array.from(bytes);
}
function derOID(str) { return derTLV(0x06, encodeOIDBody(str)); }
function decodeOID(bytes) {
  const first = bytes[0];
  const x = first < 80 ? Math.floor(first / 40) : 2;
  const out = [x, first - x * 40];
  let v = 0;
  for (let i = 1; i < bytes.length; i++) {
    v = v * 128 + (bytes[i] & 0x7f);
    if (!(bytes[i] & 0x80)) { out.push(v); v = 0; }
  }
  return out.join(".");
}

function pemEncode(label, der) {
  const b64 = Buffer.from(der).toString("base64");
  let body = "";
  for (let i = 0; i < b64.length; i += 64) body += b64.slice(i, i + 64) + "\n";
  return `-----BEGIN ${label}-----\n${body}-----END ${label}-----\n`;
}
function pemDecode(pem) {
  const s = String(pem);
  const m = s.match(/-----BEGIN ([^-]+)-----\r?\n([\s\S]*?)-----END \1-----/);
  if (!m) throw new Error("crypto: no PEM data found");
  const der = Buffer.from(m[2].replace(/[^A-Za-z0-9+/=]/g, ""), "base64");
  return { label: m[1].trim(), der: Uint8Array.from(der) };
}

const OID = {
  rsa: "1.2.840.113549.1.1.1",
  rsaPss: "1.2.840.113549.1.1.10",
  ed25519: "1.3.101.112",
  x25519: "1.3.101.110",
  ecPublicKey: "1.2.840.10045.2.1",
  p256: "1.2.840.10045.3.1.7",
  sha1WithRSA: "1.2.840.113549.1.1.5",
  sha256WithRSA: "1.2.840.113549.1.1.11",
  sha384WithRSA: "1.2.840.113549.1.1.12",
  sha512WithRSA: "1.2.840.113549.1.1.13",
  ecdsaWithSHA256: "1.2.840.10045.4.3.2",
  ecdsaWithSHA384: "1.2.840.10045.4.3.3",
  ecdsaWithSHA512: "1.2.840.10045.4.3.4",
};
const CURVE_BY_OID = {
  "1.2.840.10045.3.1.7": "prime256v1",
  "1.3.132.0.34": "secp384r1",
  "1.3.132.0.35": "secp521r1",
  "1.3.132.0.10": "secp256k1",
};

// ---- EC P-256 (short Weierstrass, Jacobian coordinates) -----------------------------------------
// Only prime256v1 is implemented; any other named curve throws by name.

const P256 = {
  name: "prime256v1",
  p: 0xffffffff00000001000000000000000000000000ffffffffffffffffffffffffn,
  a: 0xffffffff00000001000000000000000000000000fffffffffffffffffffffffcn,
  b: 0x5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604bn,
  n: 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551n,
  gx: 0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296n,
  gy: 0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5n,
  size: 32,
};
function curveByName(name) {
  const n = String(name).toLowerCase();
  if (n === "prime256v1" || n === "p-256" || n === "secp256r1") return P256;
  throw new Error(`Named curve '${name}' is not supported in lumen (only prime256v1/P-256)`);
}
function ecInfinity() { return [1n, 1n, 0n]; }
function ecIsInfinity(P) { return P[2] === 0n; }
function ecDouble(C, P) {
  const p = C.p;
  const [X1, Y1, Z1] = P;
  if (Z1 === 0n || Y1 === 0n) return ecInfinity();
  const S = amod(4n * X1 * Y1 * Y1, p);
  const Z2 = amod(Z1 * Z1, p);
  const M = amod(3n * X1 * X1 + C.a * Z2 * Z2, p);
  const X3 = amod(M * M - 2n * S, p);
  const Y3 = amod(M * (S - X3) - 8n * Y1 * Y1 * Y1 * Y1, p);
  const Z3 = amod(2n * Y1 * Z1, p);
  return [X3, Y3, Z3];
}
function ecAdd(C, P, Q) {
  const p = C.p;
  if (ecIsInfinity(P)) return Q;
  if (ecIsInfinity(Q)) return P;
  const [X1, Y1, Z1] = P;
  const [X2, Y2, Z2] = Q;
  const Z1Z1 = amod(Z1 * Z1, p);
  const Z2Z2 = amod(Z2 * Z2, p);
  const U1 = amod(X1 * Z2Z2, p);
  const U2 = amod(X2 * Z1Z1, p);
  const S1 = amod(Y1 * Z2 * Z2Z2, p);
  const S2 = amod(Y2 * Z1 * Z1Z1, p);
  if (U1 === U2) {
    if (S1 !== S2) return ecInfinity();
    return ecDouble(C, P);
  }
  const H = amod(U2 - U1, p);
  const R = amod(S2 - S1, p);
  const HH = amod(H * H, p);
  const HHH = amod(H * HH, p);
  const U1HH = amod(U1 * HH, p);
  const X3 = amod(R * R - HHH - 2n * U1HH, p);
  const Y3 = amod(R * (U1HH - X3) - S1 * HHH, p);
  const Z3 = amod(H * Z1 * Z2, p);
  return [X3, Y3, Z3];
}
function ecMul(C, k, P) {
  let R = ecInfinity();
  let Q = P;
  let s = k;
  while (s > 0n) {
    if (s & 1n) R = ecAdd(C, R, Q);
    Q = ecDouble(C, Q);
    s >>= 1n;
  }
  return R;
}
function ecToAffine(C, P) {
  if (ecIsInfinity(P)) return null;
  const zi = modInv(P[2], C.p);
  const zi2 = amod(zi * zi, C.p);
  return [amod(P[0] * zi2, C.p), amod(P[1] * zi2 * zi, C.p)];
}
function ecG(C) { return [C.gx, C.gy, 1n]; }
function ecPointEncode(C, aff) {
  return concatAll([Uint8Array.of(4), bigIntToBytesBE(aff[0], C.size), bigIntToBytesBE(aff[1], C.size)]);
}
function ecPointDecode(C, bytes) {
  if ((bytes[0] === 0x04 || bytes[0] === 0x06 || bytes[0] === 0x07) && bytes.length === 1 + 2 * C.size) {
    const x = bytesToBigIntBE(bytes.subarray(1, 1 + C.size));
    const y = bytesToBigIntBE(bytes.subarray(1 + C.size));
    if (x >= C.p || y >= C.p || amod(y * y - x * x * x - C.a * x - C.b, C.p) !== 0n) {
      throw new Error("crypto: EC point is not on the curve");
    }
    if (bytes[0] >= 0x06 && (y & 1n) !== BigInt(bytes[0] & 1)) {
      throw new Error("crypto: invalid hybrid EC point encoding");
    }
    return [x, y, 1n];
  }
  if ((bytes[0] === 0x02 || bytes[0] === 0x03) && bytes.length === 1 + C.size) {
    const x = bytesToBigIntBE(bytes.subarray(1));
    const y2 = amod(x * x * x + C.a * x + C.b, C.p);
    let y = modPow(y2, (C.p + 1n) / 4n, C.p); // p ≡ 3 mod 4 for P-256
    if (amod(y * y, C.p) !== y2) throw new Error("crypto: EC point is not on the curve");
    if ((y & 1n) !== BigInt(bytes[0] & 1)) y = amod(-y, C.p);
    return [x, y, 1n];
  }
  throw new Error("crypto: unsupported EC point encoding");
}
function ecPubFromPriv(C, d) { return ecPointEncode(C, ecToAffine(C, ecMul(C, d, ecG(C)))); }
function ecPointEncodeFormat(C, point, format) {
  const aff = ecToAffine(C, point);
  if (!aff) throw new Error("crypto: EC point at infinity");
  const x = bigIntToBytesBE(aff[0], C.size);
  const y = bigIntToBytesBE(aff[1], C.size);
  const f = format === undefined ? "uncompressed" : String(format).toLowerCase();
  if (f === "compressed") return concatAll([Uint8Array.of(Number(2n | (aff[1] & 1n))), x]);
  if (f === "hybrid") return concatAll([Uint8Array.of(Number(6n | (aff[1] & 1n))), x, y]);
  if (f === "uncompressed") return concatAll([Uint8Array.of(4), x, y]);
  throw new TypeError(`Invalid EC point conversion form '${format}'`);
}

class ECDH {
  constructor(curve) {
    this._curve = curveByName(curve);
    this._private = null;
    this._public = null;
  }
  generateKeys(encoding, format) {
    let bytes;
    do {
      bytes = randomBytes(this._curve.size);
      this._private = bytesToBigIntBE(bytes) % this._curve.n;
    } while (this._private === 0n);
    this._public = ecMul(this._curve, this._private, ecG(this._curve));
    return this.getPublicKey(encoding, format);
  }
  computeSecret(otherPublicKey, inputEncoding, outputEncoding) {
    if (this._private === null) throw new Error("Private key is not set");
    const bytes = toBytes(otherPublicKey, inputEncoding);
    const shared = ecToAffine(this._curve, ecMul(this._curve, this._private, ecPointDecode(this._curve, bytes)));
    if (!shared) throw new Error("Failed to compute ECDH key");
    const secret = Buffer.from(bigIntToBytesBE(shared[0], this._curve.size));
    return outputEncoding ? secret.toString(outputEncoding) : secret;
  }
  getPrivateKey(encoding) {
    if (this._private === null) throw new Error("Private key is not set");
    const key = Buffer.from(bigIntToBytesBE(this._private, this._curve.size));
    return encoding ? key.toString(encoding) : key;
  }
  getPublicKey(encoding, format) {
    if (!this._public) throw new Error("Public key is not set");
    const key = Buffer.from(ecPointEncodeFormat(this._curve, this._public, format));
    return encoding ? key.toString(encoding) : key;
  }
  setPrivateKey(privateKey, encoding) {
    const d = bytesToBigIntBE(toBytes(privateKey, encoding));
    if (d <= 0n || d >= this._curve.n) throw new RangeError("Private key is not valid for specified curve");
    this._private = d;
    this._public = ecMul(this._curve, d, ecG(this._curve));
  }
  setPublicKey(publicKey, encoding) {
    this._public = ecPointDecode(this._curve, toBytes(publicKey, encoding));
  }
  static convertKey(key, curve, inputEncoding, outputEncoding, format) {
    const C = curveByName(curve);
    const converted = Buffer.from(ecPointEncodeFormat(C, ecPointDecode(C, toBytes(key, inputEncoding)), format));
    return outputEncoding ? converted.toString(outputEncoding) : converted;
  }
}
function createECDH(curve) { return new ECDH(curve); }

function diffieHellman(options) {
  if (!options || !(options.privateKey instanceof KeyObject) || !(options.publicKey instanceof KeyObject)) {
    throw new TypeError("diffieHellman requires privateKey and publicKey KeyObjects");
  }
  const priv = options.privateKey._asym;
  const pub = options.publicKey._asym;
  if (priv.kind === "ec" && pub.kind === "ec") {
    if (priv.d === undefined) throw new Error("diffieHellman privateKey does not contain a private key");
    const C = curveByName(priv.curve);
    const shared = ecToAffine(C, ecMul(C, priv.d, ecPointDecode(C, pub.point)));
    if (!shared) throw new Error("Failed to compute ECDH key");
    return Buffer.from(bigIntToBytesBE(shared[0], C.size));
  }
  if (priv.kind === "x25519" && pub.kind === "x25519" && priv.priv) {
    return Buffer.from(x25519Scalar(priv.priv, pub.pub));
  }
  throw new Error("diffieHellman keys must use the same supported curve");
}

// ---- Ed25519 (RFC 8032) / X25519 (RFC 7748) -----------------------------------------------------

const ED_P = (1n << 255n) - 19n;
const ED_L = (1n << 252n) + 27742317777372353535851937790883648493n;
const ED_D = amod(-121665n * modInv(121666n, ED_P), ED_P);
const ED_SQRT_M1 = modPow(2n, (ED_P - 1n) / 4n, ED_P);
function edRecoverX(y, sign) {
  const y2 = amod(y * y, ED_P);
  const uv = amod(amod(y2 - 1n, ED_P) * modInv(ED_D * y2 + 1n, ED_P), ED_P);
  let x = modPow(uv, (ED_P + 3n) / 8n, ED_P);
  if (amod(x * x - uv, ED_P) !== 0n) x = amod(x * ED_SQRT_M1, ED_P);
  if (amod(x * x - uv, ED_P) !== 0n) return null;
  if ((x & 1n) !== sign) x = amod(-x, ED_P);
  return x;
}
const ED_BY = amod(4n * modInv(5n, ED_P), ED_P);
const ED_BX = edRecoverX(ED_BY, 0n);
const ED_B = [ED_BX, ED_BY, 1n, amod(ED_BX * ED_BY, ED_P)];
function edAdd(P, Q) {
  const [X1, Y1, Z1, T1] = P;
  const [X2, Y2, Z2, T2] = Q;
  const A = amod((Y1 - X1) * (Y2 - X2), ED_P);
  const B = amod((Y1 + X1) * (Y2 + X2), ED_P);
  const Cc = amod(T1 * 2n * ED_D * T2, ED_P);
  const Dd = amod(Z1 * 2n * Z2, ED_P);
  const E = B - A, F = Dd - Cc, G = Dd + Cc, H = B + A;
  return [amod(E * F, ED_P), amod(G * H, ED_P), amod(F * G, ED_P), amod(E * H, ED_P)];
}
function edMul(s, P) {
  let Q = [0n, 1n, 1n, 0n];
  let base = P;
  let k = s;
  while (k > 0n) { if (k & 1n) Q = edAdd(Q, base); base = edAdd(base, base); k >>= 1n; }
  return Q;
}
function edLe2int(bytes) { let n = 0n; for (let i = bytes.length - 1; i >= 0; i--) n = (n << 8n) | BigInt(bytes[i]); return n; }
function edInt2le(n, len) { const o = new Uint8Array(len); let x = n; for (let i = 0; i < len; i++) { o[i] = Number(x & 0xffn); x >>= 8n; } return o; }
function edEncodePoint(P) {
  const zi = modInv(P[2], ED_P);
  const x = amod(P[0] * zi, ED_P);
  const y = amod(P[1] * zi, ED_P);
  const out = edInt2le(y, 32);
  out[31] |= Number(x & 1n) << 7;
  return out;
}
function edDecodePoint(bytes) {
  if (bytes.length !== 32) return null;
  const y = edLe2int(bytes) & ((1n << 255n) - 1n);
  const sign = BigInt(bytes[31] >> 7);
  if (y >= ED_P) return null;
  const x = edRecoverX(y, sign);
  if (x === null) return null;
  return [x, y, 1n, amod(x * y, ED_P)];
}
function edClamp(h) {
  const a = Uint8Array.from(h.subarray(0, 32));
  a[0] &= 248; a[31] &= 127; a[31] |= 64;
  return edLe2int(a);
}
function ed25519PubFromSeed(seed) {
  return edEncodePoint(edMul(edClamp(sha512(seed)), ED_B));
}
function ed25519Sign(seed, msg) {
  const h = sha512(seed);
  const a = edClamp(h);
  const prefix = h.subarray(32, 64);
  const A = edEncodePoint(edMul(a, ED_B));
  const r = amod(edLe2int(sha512(concatBytes(prefix, msg))), ED_L);
  const R = edEncodePoint(edMul(r, ED_B));
  const k = amod(edLe2int(sha512(concatAll([R, A, msg]))), ED_L);
  const S = amod(r + k * a, ED_L);
  return concatBytes(R, edInt2le(S, 32));
}
function ed25519Verify(pub, msg, sig) {
  if (sig.length !== 64) return false;
  const R = sig.subarray(0, 32);
  const S = edLe2int(sig.subarray(32, 64));
  if (S >= ED_L) return false;
  const A = edDecodePoint(pub);
  if (!A) return false;
  const Rp = edDecodePoint(R);
  if (!Rp) return false;
  const k = amod(edLe2int(sha512(concatAll([R, pub, msg]))), ED_L);
  const left = edEncodePoint(edMul(S, ED_B));
  const right = edEncodePoint(edAdd(Rp, edMul(k, A)));
  for (let i = 0; i < 32; i++) if (left[i] !== right[i]) return false;
  return true;
}
// X25519 Montgomery ladder (RFC 7748).
function x25519Scalar(scalarBytes, uBytes) {
  const kBytes = Uint8Array.from(scalarBytes);
  kBytes[0] &= 248; kBytes[31] &= 127; kBytes[31] |= 64;
  const kk = edLe2int(kBytes);
  const u = edLe2int(uBytes) & ((1n << 255n) - 1n);
  let x1 = u, x2 = 1n, z2 = 0n, x3 = u, z3 = 1n, swap = 0n;
  for (let t = 254; t >= 0; t--) {
    const kt = (kk >> BigInt(t)) & 1n;
    swap ^= kt;
    if (swap) { [x2, x3] = [x3, x2]; [z2, z3] = [z3, z2]; }
    swap = kt;
    const A = amod(x2 + z2, ED_P), AA = amod(A * A, ED_P);
    const B = amod(x2 - z2, ED_P), BB = amod(B * B, ED_P);
    const E = amod(AA - BB, ED_P);
    const Cc = amod(x3 + z3, ED_P), Dd = amod(x3 - z3, ED_P);
    const DA = amod(Dd * A, ED_P), CB = amod(Cc * B, ED_P);
    x3 = amod((DA + CB) * (DA + CB), ED_P);
    z3 = amod(x1 * (DA - CB) * (DA - CB), ED_P);
    x2 = amod(AA * BB, ED_P);
    z2 = amod(E * (AA + amod(121665n * E, ED_P)), ED_P);
  }
  if (swap) { x2 = x3; z2 = z3; }
  return edInt2le(amod(x2 * modInv(z2, ED_P), ED_P), 32);
}
function x25519PubFromPriv(priv) { return x25519Scalar(priv, edInt2le(9n, 32)); }

// ---- key structs: parse (SPKI/PKCS#8/PKCS#1/SEC1/JWK) and serialize -----------------------------
// A key struct is { kind: "rsa"|"ed25519"|"x25519"|"ec", ... } — the internal representation all
// asymmetric operations work on.

function parseSpki(der) {
  const seq = derChildren(derRead(der, 0).content);
  const algSeq = derChildren(seq[0].content);
  const oid = decodeOID(algSeq[0].content);
  const pub = seq[1].content.subarray(1); // BIT STRING, drop unused-bits byte
  if (oid === OID.rsa || oid === OID.rsaPss) {
    const rsaSeq = derChildren(derRead(pub, 0).content);
    return { kind: "rsa", n: derInt2Big(rsaSeq[0]), e: derInt2Big(rsaSeq[1]) };
  }
  if (oid === OID.ed25519) return { kind: "ed25519", pub: Uint8Array.from(pub) };
  if (oid === OID.x25519) return { kind: "x25519", pub: Uint8Array.from(pub) };
  if (oid === OID.ecPublicKey) {
    const curveOid = decodeOID(algSeq[1].content);
    if (curveOid !== OID.p256) throw new Error(`EC curve ${CURVE_BY_OID[curveOid] || curveOid} is not supported in lumen (only prime256v1)`);
    return { kind: "ec", curve: "prime256v1", point: Uint8Array.from(pub) };
  }
  throw new Error(`Public key algorithm ${oid} is not supported in lumen`);
}
function parsePkcs1Public(der) {
  const s = derChildren(derRead(der, 0).content);
  return { kind: "rsa", n: derInt2Big(s[0]), e: derInt2Big(s[1]) };
}
function parsePkcs1Private(der) {
  const s = derChildren(derRead(der, 0).content);
  return {
    kind: "rsa", n: derInt2Big(s[1]), e: derInt2Big(s[2]), d: derInt2Big(s[3]),
    p: derInt2Big(s[4]), q: derInt2Big(s[5]), dp: derInt2Big(s[6]), dq: derInt2Big(s[7]), qi: derInt2Big(s[8]),
  };
}
function parseSec1(der, curveHint) {
  const s = derChildren(derRead(der, 0).content);
  const d = bytesToBigIntBE(s[1].content);
  let curve = curveHint;
  let point = null;
  for (let i = 2; i < s.length; i++) {
    if (s[i].tag === 0xa0) curve = CURVE_BY_OID[decodeOID(derChildren(s[i].content)[0].content)] || curve;
    else if (s[i].tag === 0xa1) point = Uint8Array.from(derChildren(s[i].content)[0].content.subarray(1));
  }
  if (curve && curve !== "prime256v1") throw new Error(`EC curve ${curve} is not supported in lumen (only prime256v1)`);
  if (!point) point = ecPubFromPriv(P256, d);
  return { kind: "ec", curve: "prime256v1", d, point };
}
function parsePkcs8(der) {
  const seq = derChildren(derRead(der, 0).content);
  const algSeq = derChildren(seq[1].content);
  const oid = decodeOID(algSeq[0].content);
  const pk = seq[2].content; // OCTET STRING content
  if (oid === OID.rsa || oid === OID.rsaPss) return parsePkcs1Private(pk);
  if (oid === OID.ed25519) {
    const seed = Uint8Array.from(derRead(pk, 0).content);
    return { kind: "ed25519", seed, pub: ed25519PubFromSeed(seed) };
  }
  if (oid === OID.x25519) {
    const priv = Uint8Array.from(derRead(pk, 0).content);
    return { kind: "x25519", priv, pub: x25519PubFromPriv(priv) };
  }
  if (oid === OID.ecPublicKey) {
    const curveOid = algSeq[1] && algSeq[1].tag === 0x06 ? decodeOID(algSeq[1].content) : OID.p256;
    if (curveOid !== OID.p256) throw new Error(`EC curve ${CURVE_BY_OID[curveOid] || curveOid} is not supported in lumen (only prime256v1)`);
    return parseSec1(pk, "prime256v1");
  }
  throw new Error(`Private key algorithm ${oid} is not supported in lumen`);
}

function encodeRsaPublicPkcs1(k) { return derSeq([derIntFromBig(k.n), derIntFromBig(k.e)]); }
function encodeRsaPrivatePkcs1(k) {
  return derSeq([derIntFromBig(0n), derIntFromBig(k.n), derIntFromBig(k.e), derIntFromBig(k.d),
    derIntFromBig(k.p), derIntFromBig(k.q), derIntFromBig(k.dp), derIntFromBig(k.dq), derIntFromBig(k.qi)]);
}
function encodeSec1(key, withParams) {
  const parts = [derIntFromBig(1n), derOctet(bigIntToBytesBE(key.d, 32))];
  if (withParams) parts.push(derTLV(0xa0, derOID(OID.p256)));
  parts.push(derTLV(0xa1, derBitString(key.point)));
  return derSeq(parts);
}
function encodeSpki(key) {
  if (key.kind === "rsa") return derSeq([derSeq([derOID(OID.rsa), derNull()]), derBitString(encodeRsaPublicPkcs1(key))]);
  if (key.kind === "ed25519") return derSeq([derSeq([derOID(OID.ed25519)]), derBitString(key.pub)]);
  if (key.kind === "x25519") return derSeq([derSeq([derOID(OID.x25519)]), derBitString(key.pub)]);
  if (key.kind === "ec") return derSeq([derSeq([derOID(OID.ecPublicKey), derOID(OID.p256)]), derBitString(key.point)]);
  throw new Error("crypto: cannot encode SPKI for this key");
}
function encodePkcs8(key) {
  if (key.kind === "rsa") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.rsa), derNull()]), derOctet(encodeRsaPrivatePkcs1(key))]);
  if (key.kind === "ed25519") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.ed25519)]), derOctet(derOctet(key.seed))]);
  if (key.kind === "x25519") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.x25519)]), derOctet(derOctet(key.priv))]);
  if (key.kind === "ec") return derSeq([derIntFromBig(0n), derSeq([derOID(OID.ecPublicKey), derOID(OID.p256)]), derOctet(encodeSec1(key, false))]);
  throw new Error("crypto: cannot encode PKCS#8 for this key");
}

function b64url(bytes) {
  return Buffer.from(bytes).toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
function b64urlToBytes(s) {
  return Uint8Array.from(Buffer.from(String(s).replace(/-/g, "+").replace(/_/g, "/"), "base64"));
}
function jwkFromKey(key, isPrivate) {
  if (key.kind === "rsa") {
    const j = { kty: "RSA", n: b64url(bigIntToBytesBE(key.n)), e: b64url(bigIntToBytesBE(key.e)) };
    if (isPrivate) {
      j.d = b64url(bigIntToBytesBE(key.d)); j.p = b64url(bigIntToBytesBE(key.p)); j.q = b64url(bigIntToBytesBE(key.q));
      j.dp = b64url(bigIntToBytesBE(key.dp)); j.dq = b64url(bigIntToBytesBE(key.dq)); j.qi = b64url(bigIntToBytesBE(key.qi));
    }
    return j;
  }
  if (key.kind === "ed25519" || key.kind === "x25519") {
    const j = { kty: "OKP", crv: key.kind === "ed25519" ? "Ed25519" : "X25519", x: b64url(key.pub) };
    if (isPrivate) j.d = b64url(key.kind === "ed25519" ? key.seed : key.priv);
    return j;
  }
  if (key.kind === "ec") {
    const j = {
      kty: "EC", crv: "P-256",
      x: b64url(key.point.subarray(1, 1 + P256.size)), y: b64url(key.point.subarray(1 + P256.size)),
    };
    if (isPrivate) j.d = b64url(bigIntToBytesBE(key.d, 32));
    return j;
  }
  throw new Error("crypto: JWK export is not supported for this key");
}
function keyFromJwk(jwk, needPrivate) {
  if (!jwk || typeof jwk !== "object") throw new TypeError("crypto: JWK must be an object");
  if (needPrivate && jwk.d === undefined) throw new Error("crypto: JWK does not contain a private key");
  if (jwk.kty === "RSA") {
    const n = bytesToBigIntBE(b64urlToBytes(jwk.n)), e = bytesToBigIntBE(b64urlToBytes(jwk.e));
    if (jwk.d !== undefined) {
      return {
        kind: "rsa", n, e, d: bytesToBigIntBE(b64urlToBytes(jwk.d)),
        p: bytesToBigIntBE(b64urlToBytes(jwk.p)), q: bytesToBigIntBE(b64urlToBytes(jwk.q)),
        dp: bytesToBigIntBE(b64urlToBytes(jwk.dp)), dq: bytesToBigIntBE(b64urlToBytes(jwk.dq)),
        qi: bytesToBigIntBE(b64urlToBytes(jwk.qi)),
      };
    }
    return { kind: "rsa", n, e };
  }
  if (jwk.kty === "OKP") {
    const kind = jwk.crv === "Ed25519" ? "ed25519" : jwk.crv === "X25519" ? "x25519" : null;
    if (!kind) throw new Error(`OKP curve '${jwk.crv}' is not supported in lumen`);
    if (jwk.d !== undefined) {
      const secret = b64urlToBytes(jwk.d);
      return kind === "ed25519"
        ? { kind, seed: secret, pub: ed25519PubFromSeed(secret) }
        : { kind, priv: secret, pub: x25519PubFromPriv(secret) };
    }
    return { kind, pub: b64urlToBytes(jwk.x) };
  }
  if (jwk.kty === "EC") {
    if (jwk.crv !== "P-256") throw new Error(`EC curve '${jwk.crv}' is not supported in lumen (only P-256)`);
    const point = concatAll([Uint8Array.of(4), b64urlToBytes(jwk.x), b64urlToBytes(jwk.y)]);
    if (jwk.d !== undefined) return { kind: "ec", curve: "prime256v1", d: bytesToBigIntBE(b64urlToBytes(jwk.d)), point };
    return { kind: "ec", curve: "prime256v1", point };
  }
  throw new Error(`JWK kty '${jwk.kty}' is not supported in lumen`);
}
function structIsPrivate(k) { return k.d !== undefined || k.seed !== undefined || k.priv !== undefined; }
function publicFromPrivateStruct(k) {
  if (k.kind === "rsa") return { kind: "rsa", n: k.n, e: k.e };
  if (k.kind === "ed25519") return { kind: "ed25519", pub: k.pub };
  if (k.kind === "x25519") return { kind: "x25519", pub: k.pub };
  if (k.kind === "ec") return { kind: "ec", curve: k.curve, point: k.point };
  throw new Error("crypto: unknown key kind");
}
function makeKeyObject(type, struct) {
  const ko = new KeyObject(type, null);
  ko._asym = struct;
  return ko;
}

// KeyObject.export for asymmetric keys (the secret-key path stays in the class).
function exportAsymmetricKey(ko, options) {
  if (!options || !options.format) throw new TypeError("KeyObject.export: options.format is required for asymmetric keys");
  const k = ko._asym;
  const isPriv = ko._type === "private";
  if (options.format === "jwk") return jwkFromKey(k, isPriv);
  if (options.cipher || options.passphrase) {
    throw new Error("KeyObject.export: encrypted private-key export is not supported in lumen");
  }
  const type = options.type;
  let der, label;
  if (isPriv) {
    if (type === "pkcs8") { der = encodePkcs8(k); label = "PRIVATE KEY"; }
    else if (type === "pkcs1") {
      if (k.kind !== "rsa") throw new Error("KeyObject.export: 'pkcs1' requires an RSA key");
      der = encodeRsaPrivatePkcs1(k); label = "RSA PRIVATE KEY";
    } else if (type === "sec1") {
      if (k.kind !== "ec") throw new Error("KeyObject.export: 'sec1' requires an EC key");
      der = encodeSec1(k, true); label = "EC PRIVATE KEY";
    } else throw new Error(`KeyObject.export: unsupported private key type '${type}'`);
  } else {
    if (type === "spki") { der = encodeSpki(k); label = "PUBLIC KEY"; }
    else if (type === "pkcs1") {
      if (k.kind !== "rsa") throw new Error("KeyObject.export: 'pkcs1' requires an RSA key");
      der = encodeRsaPublicPkcs1(k); label = "RSA PUBLIC KEY";
    } else throw new Error(`KeyObject.export: unsupported public key type '${type}'`);
  }
  if (options.format === "der") return Buffer.from(der);
  if (options.format === "pem") return pemEncode(label, der);
  throw new Error(`KeyObject.export: unsupported format '${options.format}'`);
}

// Normalize the createPublicKey/createPrivateKey input shapes into { keyObject } or
// { data, format, type }.
function normalizeKeyInput(input) {
  if (input instanceof KeyObject) return { keyObject: input };
  if (typeof input === "string" || input instanceof Uint8Array || input instanceof ArrayBuffer || ArrayBuffer.isView(input)) {
    return { data: input, format: "pem" };
  }
  if (input && typeof input === "object") {
    if (input.key instanceof KeyObject) return { keyObject: input.key };
    if (input.key !== undefined) return { data: input.key, format: input.format || "pem", type: input.type, passphrase: input.passphrase };
  }
  throw new TypeError("crypto: invalid key input");
}
function keyDataToDer(norm) {
  if (norm.passphrase !== undefined) throw new Error("crypto: encrypted private keys are not supported in lumen");
  if (norm.format === "der") return { der: toBytes(norm.data), label: null };
  const text = typeof norm.data === "string" ? norm.data : Buffer.from(toBytes(norm.data)).toString("utf8");
  const p = pemDecode(text);
  return { der: p.der, label: p.label };
}
function createPublicKey(input) {
  const norm = normalizeKeyInput(input);
  if (norm.keyObject) {
    const src = norm.keyObject;
    if (src._type === "public") return src;
    if (src._type !== "private" || !src._asym) throw new TypeError("crypto: cannot derive a public key from this KeyObject");
    return makeKeyObject("public", publicFromPrivateStruct(src._asym));
  }
  let struct;
  if (norm.format === "jwk") {
    struct = keyFromJwk(norm.data, false);
  } else {
    const { der, label } = keyDataToDer(norm);
    const type = norm.type || null;
    if (label === "CERTIFICATE") struct = parseSpki(x509SpkiDer(der));
    else if (label === "RSA PUBLIC KEY" || type === "pkcs1") struct = parsePkcs1Public(der);
    else if (label === "RSA PRIVATE KEY") struct = parsePkcs1Private(der);
    else if (label === "EC PRIVATE KEY") struct = parseSec1(der, "prime256v1");
    else if (label === "PRIVATE KEY") struct = parsePkcs8(der);
    else struct = parseSpki(der); // "PUBLIC KEY" PEM, or DER spki
  }
  if (structIsPrivate(struct)) struct = publicFromPrivateStruct(struct);
  return makeKeyObject("public", struct);
}
function createPrivateKey(input) {
  const norm = normalizeKeyInput(input);
  if (norm.keyObject) {
    if (norm.keyObject._type === "private") return norm.keyObject;
    throw new TypeError("crypto: cannot create a private key from a public key");
  }
  let struct;
  if (norm.format === "jwk") {
    struct = keyFromJwk(norm.data, true);
  } else {
    const { der, label } = keyDataToDer(norm);
    const type = norm.type || null;
    if (label === "RSA PRIVATE KEY" || type === "pkcs1") struct = parsePkcs1Private(der);
    else if (label === "EC PRIVATE KEY" || type === "sec1") struct = parseSec1(der, "prime256v1");
    else struct = parsePkcs8(der); // "PRIVATE KEY" PEM, or DER pkcs8
  }
  if (!structIsPrivate(struct)) throw new Error("crypto: key data does not contain a private key");
  return makeKeyObject("private", struct);
}
// Extract the SubjectPublicKeyInfo DER from a certificate (full parsing lands with
// X509Certificate; this walks straight to the SPKI field).
function x509SpkiDer(certDer) {
  const cert = derRead(certDer, 0);
  const tbs = derRead(cert.content, 0);
  const t = derChildren(tbs.content);
  let idx = 0;
  if (t[0].tag === 0xa0) idx = 1; // explicit version
  idx += 4; // serial, sig alg, issuer, validity
  idx += 1; // subject
  const spki = t[idx];
  return certDer.subarray(cert.start + tbs.start + spki.hstart, cert.start + tbs.start + spki.end);
}

// ---- MGF1 + RSA (PKCS#1 v1.5, PSS, OAEP) --------------------------------------------------------

function mgf1(seed, len, reg) {
  const out = new Uint8Array(len);
  let off = 0;
  let counter = 0;
  while (off < len) {
    const block = reg.fn(concatBytes(seed, Uint8Array.of(
      (counter >>> 24) & 0xff, (counter >>> 16) & 0xff, (counter >>> 8) & 0xff, counter & 0xff)));
    const n = Math.min(block.length, len - off);
    out.set(block.subarray(0, n), off);
    off += n;
    counter++;
  }
  return out;
}
function xorBytes(a, b) {
  const out = new Uint8Array(a.length);
  for (let i = 0; i < a.length; i++) out[i] = a[i] ^ b[i];
  return out;
}

// DigestInfo prefixes for EMSA-PKCS1-v1_5 (DER of AlgorithmIdentifier + OCTET STRING header).
const DIGEST_INFO_PREFIX = {
  md5: Uint8Array.from([0x30, 0x20, 0x30, 0x0c, 0x06, 0x08, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05, 0x05, 0x00, 0x04, 0x10]),
  sha1: Uint8Array.from([0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14]),
  sha256: Uint8Array.from([0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20]),
  sha384: Uint8Array.from([0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30]),
  sha512: Uint8Array.from([0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40]),
};
function rsaModLen(key) { return Math.ceil(bitLength(key.n) / 8); }
// Public op (encrypt / verify): m^e mod n.
function rsaEP(key, m) {
  if (m >= key.n) throw new Error("RSA: data too large for the modulus");
  return modPow(m, key.e, key.n);
}
// Private op (decrypt / sign) using the CRT parameters when the key carries them.
function rsaDP(key, c) {
  if (c >= key.n) throw new Error("RSA: data too large for the modulus");
  if (key.p && key.q && key.dp && key.dq && key.qi) {
    const m1 = modPow(c, key.dp, key.p);
    const m2 = modPow(c, key.dq, key.q);
    const h = amod(key.qi * (m1 - m2), key.p);
    return amod(m2 + h * key.q, key.n);
  }
  return modPow(c, key.d, key.n);
}
function rsaPkcs1v15Pad(digestName, hashBytes, emLen) {
  const prefix = DIGEST_INFO_PREFIX[digestName];
  if (!prefix) throw new Error(`RSA PKCS#1 v1.5 digest '${digestName}' is not supported in lumen`);
  const T = concatBytes(prefix, hashBytes);
  if (emLen < T.length + 11) throw new Error("RSA: modulus too short for this digest");
  const ps = new Uint8Array(emLen - T.length - 3).fill(0xff);
  return concatAll([Uint8Array.of(0x00, 0x01), ps, Uint8Array.of(0x00), T]);
}
function emsaPssEncode(mHash, emBits, reg, sLen) {
  const hLen = reg.outLen;
  const emLen = Math.ceil(emBits / 8);
  if (emLen < hLen + sLen + 2) throw new Error("RSA-PSS: salt too long for the modulus");
  const salt = sLen > 0 ? Uint8Array.from(randomBytes(sLen)) : new Uint8Array(0);
  const H = reg.fn(concatAll([new Uint8Array(8), mHash, salt]));
  const DB = concatAll([new Uint8Array(emLen - sLen - hLen - 2), Uint8Array.of(0x01), salt]);
  const maskedDB = xorBytes(DB, mgf1(H, emLen - hLen - 1, reg));
  const bits = 8 * emLen - emBits;
  if (bits > 0) maskedDB[0] &= 0xff >> bits;
  return concatAll([maskedDB, H, Uint8Array.of(0xbc)]);
}
// sLen === -1 means auto-detect (Node's RSA_PSS_SALTLEN_AUTO verify default).
function emsaPssVerify(mHash, em, emBits, reg, sLen) {
  const hLen = reg.outLen;
  const emLen = em.length;
  if (emLen < hLen + 2) return false;
  if (em[emLen - 1] !== 0xbc) return false;
  const maskedDB = em.subarray(0, emLen - hLen - 1);
  const H = em.subarray(emLen - hLen - 1, emLen - 1);
  const bits = 8 * emLen - emBits;
  if (bits > 0 && (maskedDB[0] & (0xff << (8 - bits)) & 0xff) !== 0) return false;
  const DB = xorBytes(maskedDB, mgf1(H, emLen - hLen - 1, reg));
  if (bits > 0) DB[0] &= 0xff >> bits;
  let saltStart;
  if (sLen === -1) {
    let i = 0;
    while (i < DB.length && DB[i] === 0) i++;
    if (i >= DB.length || DB[i] !== 0x01) return false;
    saltStart = i + 1;
  } else {
    saltStart = DB.length - sLen;
    for (let i = 0; i < saltStart - 1; i++) if (DB[i] !== 0) return false;
    if (DB[saltStart - 1] !== 0x01) return false;
  }
  const salt = DB.subarray(saltStart);
  const H2 = reg.fn(concatAll([new Uint8Array(8), mHash, salt]));
  for (let i = 0; i < hLen; i++) if (H[i] !== H2[i]) return false;
  return true;
}
function pssSaltLenForSign(opts, reg, key) {
  const sl = opts.saltLength;
  if (sl === undefined || sl === constants.RSA_PSS_SALTLEN_DIGEST) return reg.outLen;
  if (sl === constants.RSA_PSS_SALTLEN_MAX_SIGN) return Math.ceil((bitLength(key.n) - 1) / 8) - reg.outLen - 2;
  return sl;
}
function rsaOaepEncrypt(key, msg, reg, label) {
  const k = rsaModLen(key);
  const hLen = reg.outLen;
  if (msg.length > k - 2 * hLen - 2) throw new Error("RSA-OAEP: message too long");
  const lHash = reg.fn(label || new Uint8Array(0));
  const DB = concatAll([lHash, new Uint8Array(k - msg.length - 2 * hLen - 2), Uint8Array.of(0x01), msg]);
  const seed = Uint8Array.from(randomBytes(hLen));
  const maskedDB = xorBytes(DB, mgf1(seed, k - hLen - 1, reg));
  const maskedSeed = xorBytes(seed, mgf1(maskedDB, hLen, reg));
  const em = concatAll([Uint8Array.of(0x00), maskedSeed, maskedDB]);
  return bigIntToBytesBE(rsaEP(key, bytesToBigIntBE(em)), k);
}
function rsaOaepDecrypt(key, ct, reg, label) {
  const k = rsaModLen(key);
  const hLen = reg.outLen;
  const em = bigIntToBytesBE(rsaDP(key, bytesToBigIntBE(ct)), k);
  const lHash = reg.fn(label || new Uint8Array(0));
  if (em[0] !== 0x00) throw new Error("RSA-OAEP: decryption error");
  const maskedSeed = em.subarray(1, 1 + hLen);
  const maskedDB = em.subarray(1 + hLen);
  const seed = xorBytes(maskedSeed, mgf1(maskedDB, hLen, reg));
  const DB = xorBytes(maskedDB, mgf1(seed, k - hLen - 1, reg));
  for (let i = 0; i < hLen; i++) if (DB[i] !== lHash[i]) throw new Error("RSA-OAEP: decryption error");
  let i = hLen;
  while (i < DB.length && DB[i] === 0x00) i++;
  if (i >= DB.length || DB[i] !== 0x01) throw new Error("RSA-OAEP: decryption error");
  return Uint8Array.from(DB.subarray(i + 1));
}
function rsaPkcs1Encrypt(key, msg) {
  const k = rsaModLen(key);
  if (msg.length > k - 11) throw new Error("RSA-PKCS1: message too long");
  const ps = new Uint8Array(k - msg.length - 3);
  for (let i = 0; i < ps.length; i++) { let b = 0; while (b === 0) b = randomBytes(1)[0]; ps[i] = b; }
  const em = concatAll([Uint8Array.of(0x00, 0x02), ps, Uint8Array.of(0x00), msg]);
  return bigIntToBytesBE(rsaEP(key, bytesToBigIntBE(em)), k);
}
// (No rsaPkcs1Decrypt: Node v22 removed PKCS#1 v1.5 private decryption — Marvin attack — and
// privateDecrypt matches that refusal exactly.)

// ---- probable primes (Miller-Rabin over BigInt) -------------------------------------------------

const SMALL_PRIMES = (() => {
  const out = [];
  const sieve = new Uint8Array(4096);
  for (let i = 2; i < 4096; i++) {
    if (!sieve[i]) { out.push(BigInt(i)); for (let j = i * i; j < 4096; j += i) sieve[j] = 1; }
  }
  return out;
})();
function randomBigIntBits(bits) {
  const bytes = Math.ceil(bits / 8);
  const b = Uint8Array.from(randomBytes(bytes));
  b[0] &= 0xff >> (bytes * 8 - bits);
  return bytesToBigIntBE(b);
}
function millerRabinRound(n, d, r, a) {
  let x = modPow(a, d, n);
  if (x === 1n || x === n - 1n) return true;
  for (let j = 1n; j < r; j++) {
    x = (x * x) % n;
    if (x === n - 1n) return true;
  }
  return false;
}
function isProbablePrime(n, rounds) {
  if (n < 2n) return false;
  for (const sp of SMALL_PRIMES) {
    if (n === sp) return true;
    if (n % sp === 0n) return false;
  }
  let d = n - 1n;
  let r = 0n;
  while ((d & 1n) === 0n) { d >>= 1n; r++; }
  // A fixed base-2 round first: it rejects almost all composites before the random rounds.
  if (!millerRabinRound(n, d, r, 2n)) return false;
  const nBits = bitLength(n);
  for (let i = 0; i < rounds; i++) {
    const a = 2n + randomBigIntBits(nBits) % (n - 3n);
    if (!millerRabinRound(n, d, r, a)) return false;
  }
  return true;
}
function randomPrime(bits, safe) {
  if (!Number.isInteger(bits) || bits < 2) throw new RangeError("prime size must be an integer >= 2 bits");
  for (;;) {
    let cand = randomBigIntBits(bits);
    cand |= 1n;
    cand |= 1n << BigInt(bits - 1);
    if (bits >= 3) cand |= 1n << BigInt(bits - 2); // full-size products for RSA
    if (safe) {
      if ((cand & 3n) !== 3n) continue; // safe primes are ≡ 3 (mod 4)
      if (isProbablePrime((cand - 1n) >> 1n, 8) && isProbablePrime(cand, 20)) return cand;
    } else if (isProbablePrime(cand, 20)) {
      return cand;
    }
  }
}

function checkPrimeSync(candidate, options) {
  let n;
  if (typeof candidate === "bigint") n = candidate;
  else n = bytesToBigIntBE(toBytes(candidate));
  const checks = options && options.checks ? options.checks : 40;
  return isProbablePrime(n, checks);
}
function checkPrime(candidate, options, cb) {
  if (typeof options === "function") { cb = options; options = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let res, err;
  try { res = checkPrimeSync(candidate, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, res)));
}
function generatePrimeSync(size, options) {
  options = options || {};
  if (options.add !== undefined || options.rem !== undefined) {
    throw new Error("generatePrime options 'add'/'rem' are not supported in lumen");
  }
  const p = randomPrime(size, !!options.safe);
  if (options.bigint) return p;
  return bigIntToBytesBE(p, Math.ceil(size / 8)).buffer; // Node returns an ArrayBuffer
}
function generatePrime(size, options, cb) {
  if (typeof options === "function") { cb = options; options = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let res, err;
  try { res = generatePrimeSync(size, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, res)));
}

// ---- RSA key generation -------------------------------------------------------------------------
// Miller-Rabin probable primes over BigInt. Correct but slow for large moduli (pure JS bignum, no
// Montgomery ladder in the engine yet): 2048-bit generation can take tens of seconds — documented,
// accepted; sign/verify on imported 2048-bit keys is fast enough for real use.

function generateRsa(bits, publicExponent) {
  if (!Number.isInteger(bits) || bits < 512 || bits > 8192) {
    throw new RangeError("RSA modulusLength must be an integer in [512, 8192]");
  }
  const e = BigInt(publicExponent === undefined ? 65537 : publicExponent);
  const pbits = bits >> 1;
  const qbits = bits - pbits;
  for (;;) {
    const p = randomPrime(pbits, false);
    const q = randomPrime(qbits, false);
    if (p === q) continue;
    const n = p * q;
    if (bitLength(n) !== bits) continue;
    const phi = (p - 1n) * (q - 1n);
    let d;
    try { d = modInv(e, phi); } catch (_e) { continue; }
    const [hi, lo] = p > q ? [p, q] : [q, p];
    return {
      kind: "rsa", n, e, d,
      p: hi, q: lo,
      dp: amod(d, hi - 1n), dq: amod(d, lo - 1n), qi: modInv(lo, hi),
    };
  }
}

// ---- public-key encryption ------------------------------------------------------------------------

function oaepReg(opts) { return resolveHash(normalizeDigest(opts.oaepHash || "sha1")); }
function oaepLabel(opts) { return opts.oaepLabel !== undefined ? toBytes(opts.oaepLabel) : undefined; }
function publicEncrypt(keyLike, buffer) {
  const opts = keyOptionsFrom(keyLike);
  const struct = createPublicKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("publicEncrypt: only RSA keys are supported in lumen");
  const data = toBytes(buffer);
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_OAEP_PADDING : opts.padding;
  if (padding === constants.RSA_PKCS1_OAEP_PADDING) return Buffer.from(rsaOaepEncrypt(struct, data, oaepReg(opts), oaepLabel(opts)));
  if (padding === constants.RSA_PKCS1_PADDING) return Buffer.from(rsaPkcs1Encrypt(struct, data));
  throw new Error(`publicEncrypt: padding ${padding} is not supported in lumen`);
}
function privateDecrypt(keyLike, buffer) {
  const opts = keyOptionsFrom(keyLike);
  const struct = createPrivateKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("privateDecrypt: only RSA keys are supported in lumen");
  const data = toBytes(buffer);
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_OAEP_PADDING : opts.padding;
  if (padding === constants.RSA_PKCS1_OAEP_PADDING) return Buffer.from(rsaOaepDecrypt(struct, data, oaepReg(opts), oaepLabel(opts)));
  if (padding === constants.RSA_PKCS1_PADDING) {
    // Match Node v22 exactly: PKCS#1 v1.5 private decryption was removed (Marvin attack).
    throw new TypeError("RSA_PKCS1_PADDING is no longer supported for private decryption");
  }
  throw new Error(`privateDecrypt: padding ${padding} is not supported in lumen`);
}
function privateEncrypt(keyLike, buffer) {
  const struct = createPrivateKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("privateEncrypt: only RSA keys are supported in lumen");
  const k = rsaModLen(struct);
  const msg = toBytes(buffer);
  if (msg.length > k - 11) throw new Error("privateEncrypt: message too long");
  const ps = new Uint8Array(k - msg.length - 3).fill(0xff);
  const em = concatAll([Uint8Array.of(0x00, 0x01), ps, Uint8Array.of(0x00), msg]);
  return Buffer.from(bigIntToBytesBE(rsaDP(struct, bytesToBigIntBE(em)), k));
}
function publicDecrypt(keyLike, buffer) {
  const struct = createPublicKey(keyLike)._asym;
  if (struct.kind !== "rsa") throw new Error("publicDecrypt: only RSA keys are supported in lumen");
  const k = rsaModLen(struct);
  const em = bigIntToBytesBE(rsaEP(struct, bytesToBigIntBE(toBytes(buffer))), k);
  if (em[0] !== 0x00 || em[1] !== 0x01) throw new Error("publicDecrypt: decryption error");
  let i = 2;
  while (i < em.length && em[i] === 0xff) i++;
  if (i >= em.length || em[i] !== 0x00) throw new Error("publicDecrypt: decryption error");
  return Buffer.from(em.subarray(i + 1));
}

// ---- streaming Sign / Verify classes --------------------------------------------------------------

class Sign {
  constructor(algorithm) {
    this._algo = normalizeDigest(algorithm);
    resolveHash(this._algo); // validate eagerly, like Node
    this._chunks = [];
  }
  update(data, encoding) { this._chunks.push(toBytes(data, encoding)); return this; }
  sign(keyLike, encoding) {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const out = signDigest(this._algo, all, keyLike);
    return encoding && encoding !== "buffer" ? out.toString(encoding) : out;
  }
}
class Verify {
  constructor(algorithm) {
    this._algo = normalizeDigest(algorithm);
    resolveHash(this._algo);
    this._chunks = [];
  }
  update(data, encoding) { this._chunks.push(toBytes(data, encoding)); return this; }
  verify(keyLike, signature, encoding) {
    let total = 0;
    for (const c of this._chunks) total += c.length;
    const all = new Uint8Array(total);
    let off = 0;
    for (const c of this._chunks) { all.set(c, off); off += c.length; }
    const sig = typeof signature === "string" ? Buffer.from(signature, encoding || "hex") : toBytes(signature);
    return verifyDigest(this._algo, all, keyLike, sig);
  }
}

// ---- sign / verify ------------------------------------------------------------------------------

// Extra options (padding, saltLength, dsaEncoding …) ride along on the key argument, as in Node.
function keyOptionsFrom(keyLike) {
  if (keyLike instanceof KeyObject) return {};
  if (typeof keyLike === "string" || keyLike instanceof Uint8Array || ArrayBuffer.isView(keyLike)) return {};
  return keyLike && typeof keyLike === "object" ? keyLike : {};
}
function signDigest(algorithm, data, keyLike) {
  const struct = createPrivateKey(keyLike)._asym;
  const msg = toBytes(data);
  if (struct.kind === "ed25519") {
    if (algorithm != null) throw new Error("crypto.sign: algorithm must be null/undefined for Ed25519 keys");
    return Buffer.from(ed25519Sign(struct.seed, msg));
  }
  if (struct.kind === "x25519") throw new Error("crypto.sign: X25519 keys cannot sign");
  if (algorithm == null) throw new Error(`crypto.sign: a digest algorithm is required for ${struct.kind} keys`);
  const digestName = normalizeDigest(algorithm);
  const reg = resolveHash(digestName);
  const mHash = reg.fn(msg);
  const opts = keyOptionsFrom(keyLike);
  if (struct.kind === "rsa") return Buffer.from(rsaSignHash(struct, digestName, reg, mHash, opts));
  if (struct.kind === "ec") return Buffer.from(ecdsaSignHash(struct, mHash, opts));
  throw new Error(`crypto.sign: unsupported key type '${struct.kind}'`);
}
function verifyDigest(algorithm, data, keyLike, signature) {
  const struct = createPublicKey(keyLike)._asym;
  const msg = toBytes(data);
  const sig = toBytes(signature);
  if (struct.kind === "ed25519") {
    if (algorithm != null) throw new Error("crypto.verify: algorithm must be null/undefined for Ed25519 keys");
    return ed25519Verify(struct.pub, msg, sig);
  }
  if (struct.kind === "x25519") throw new Error("crypto.verify: X25519 keys cannot verify");
  if (algorithm == null) throw new Error(`crypto.verify: a digest algorithm is required for ${struct.kind} keys`);
  const digestName = normalizeDigest(algorithm);
  const reg = resolveHash(digestName);
  const mHash = reg.fn(msg);
  const opts = keyOptionsFrom(keyLike);
  if (struct.kind === "rsa") return rsaVerifyHash(struct, digestName, reg, mHash, sig, opts);
  if (struct.kind === "ec") return ecdsaVerifyHash(struct, mHash, sig, opts);
  throw new Error(`crypto.verify: unsupported key type '${struct.kind}'`);
}
function rsaSignHash(struct, digestName, reg, mHash, opts) {
  if (struct.d === undefined) throw new Error("crypto.sign: an RSA private key is required");
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_PADDING : opts.padding;
  if (padding === constants.RSA_PKCS1_PSS_PADDING) {
    const emBits = bitLength(struct.n) - 1;
    const em = emsaPssEncode(mHash, emBits, reg, pssSaltLenForSign(opts, reg, struct));
    return bigIntToBytesBE(rsaDP(struct, bytesToBigIntBE(em)), rsaModLen(struct));
  }
  if (padding !== constants.RSA_PKCS1_PADDING) throw new Error(`crypto.sign: RSA padding ${padding} is not supported in lumen`);
  const k = rsaModLen(struct);
  const em = rsaPkcs1v15Pad(digestName, mHash, k);
  return bigIntToBytesBE(rsaDP(struct, bytesToBigIntBE(em)), k);
}
function rsaVerifyHash(struct, digestName, reg, mHash, sig, opts) {
  const k = rsaModLen(struct);
  if (sig.length !== k) return false;
  const padding = opts.padding === undefined ? constants.RSA_PKCS1_PADDING : opts.padding;
  let m;
  try { m = rsaEP(struct, bytesToBigIntBE(sig)); } catch (_e) { return false; }
  if (padding === constants.RSA_PKCS1_PSS_PADDING) {
    const emBits = bitLength(struct.n) - 1;
    const em = bigIntToBytesBE(m, Math.ceil(emBits / 8));
    let sLen = opts.saltLength;
    if (sLen === undefined || sLen === constants.RSA_PSS_SALTLEN_AUTO) sLen = -1; // auto-detect
    else if (sLen === constants.RSA_PSS_SALTLEN_DIGEST) sLen = reg.outLen;
    return emsaPssVerify(mHash, em, emBits, reg, sLen);
  }
  if (padding !== constants.RSA_PKCS1_PADDING) throw new Error(`crypto.verify: RSA padding ${padding} is not supported in lumen`);
  const em = bigIntToBytesBE(m, k);
  const expect = rsaPkcs1v15Pad(digestName, mHash, k);
  let diff = 0;
  for (let i = 0; i < k; i++) diff |= em[i] ^ expect[i];
  return diff === 0;
}
// ECDSA lands with the EC tier; keep the throws precise until then.
function ecdsaSignHash() { throw new Error("node:crypto ECDSA sign is not supported in lumen (no EC signing primitive available)"); }
function ecdsaVerifyHash() { throw new Error("node:crypto ECDSA verify is not supported in lumen (no EC signing primitive available)"); }

function cryptoSign(algorithm, data, key, cb) {
  if (typeof cb === "function") {
    let r, e;
    try { r = signDigest(algorithm, data, key); } catch (x) { e = x; }
    queueMicrotask(() => (e ? cb(e) : cb(null, r)));
    return;
  }
  return signDigest(algorithm, data, key);
}
function cryptoVerify(algorithm, data, key, signature, cb) {
  if (typeof cb === "function") {
    let r, e;
    try { r = verifyDigest(algorithm, data, key, signature); } catch (x) { e = x; }
    queueMicrotask(() => (e ? cb(e) : cb(null, r)));
    return;
  }
  return verifyDigest(algorithm, data, key, signature);
}

// ---- asymmetric key generation ------------------------------------------------------------------

function makeKeyPair(privStruct) {
  return {
    publicKey: makeKeyObject("public", publicFromPrivateStruct(privStruct)),
    privateKey: makeKeyObject("private", privStruct),
  };
}
function generateEd25519() {
  const seed = Uint8Array.from(randomBytes(32));
  return { kind: "ed25519", seed, pub: ed25519PubFromSeed(seed) };
}
function generateKeyPairStruct(type, options) {
  const t = String(type).toLowerCase();
  if (t === "ed25519") return generateEd25519();
  if (t === "rsa") {
    if (!options || !Number.isInteger(options.modulusLength)) {
      throw new TypeError("options.modulusLength is required for RSA key generation");
    }
    return generateRsa(options.modulusLength, options.publicExponent);
  }
  throw new Error(`generateKeyPair type '${type}' is not supported in lumen (ed25519, rsa)`);
}
function generateKeyPairSync(type, options) {
  const pair = makeKeyPair(generateKeyPairStruct(type, options));
  const pubEnc = options && options.publicKeyEncoding;
  const privEnc = options && options.privateKeyEncoding;
  return {
    publicKey: pubEnc ? pair.publicKey.export(pubEnc) : pair.publicKey,
    privateKey: privEnc ? pair.privateKey.export(privEnc) : pair.privateKey,
  };
}
function generateKeyPair(type, options, cb) {
  if (typeof options === "function") { cb = options; options = undefined; }
  if (typeof cb !== "function") throw new TypeError("callback must be a function");
  let res, err;
  try { res = generateKeyPairSync(type, options); } catch (e) { err = e; }
  queueMicrotask(() => (err ? cb(err) : cb(null, res.publicKey, res.privateKey)));
}
// ---- AES ciphers (native __crypto ops; see lumen-node/src/crypto.rs) ---------------------------
// Real: aes-{128,192,256}-{ecb,cbc,ctr,gcm}, streaming update/final with Node's buffer-holdback,
// PKCS#7 auto-padding, GCM AAD/auth-tag (variable tag lengths) — all bit-exact with Node v22.
// Everything else (chacha20, des, cfb/ofb/ccm/ocb/xts, …) throws honestly, naming the algorithm.

const CIPHERS = {
  "aes-128-ecb": { mode: "ecb", name: "aes-128-ecb", nid: 418, blockSize: 16, keyLength: 16 },
  "aes-128-cbc": { mode: "cbc", name: "aes-128-cbc", nid: 419, blockSize: 16, ivLength: 16, keyLength: 16 },
  "aes-128-ctr": { mode: "ctr", name: "aes-128-ctr", nid: 904, blockSize: 1, ivLength: 16, keyLength: 16 },
  "aes-128-gcm": { mode: "gcm", name: "id-aes128-gcm", nid: 895, blockSize: 1, ivLength: 12, keyLength: 16 },
  "aes-192-ecb": { mode: "ecb", name: "aes-192-ecb", nid: 422, blockSize: 16, keyLength: 24 },
  "aes-192-cbc": { mode: "cbc", name: "aes-192-cbc", nid: 423, blockSize: 16, ivLength: 16, keyLength: 24 },
  "aes-192-ctr": { mode: "ctr", name: "aes-192-ctr", nid: 905, blockSize: 1, ivLength: 16, keyLength: 24 },
  "aes-192-gcm": { mode: "gcm", name: "id-aes192-gcm", nid: 898, blockSize: 1, ivLength: 12, keyLength: 24 },
  "aes-256-ecb": { mode: "ecb", name: "aes-256-ecb", nid: 426, blockSize: 16, keyLength: 32 },
  "aes-256-cbc": { mode: "cbc", name: "aes-256-cbc", nid: 427, blockSize: 16, ivLength: 16, keyLength: 32 },
  "aes-256-ctr": { mode: "ctr", name: "aes-256-ctr", nid: 906, blockSize: 1, ivLength: 16, keyLength: 32 },
  "aes-256-gcm": { mode: "gcm", name: "id-aes256-gcm", nid: 901, blockSize: 1, ivLength: 12, keyLength: 32 },
};
const CIPHER_ALIASES = { aes128: "aes-128-cbc", aes192: "aes-192-cbc", aes256: "aes-256-cbc" };
const GCM_TAG_LENS = [4, 8, 12, 13, 14, 15, 16];

function codedError(Ctor, message, code) {
  const e = new Ctor(message);
  if (code) e.code = code;
  return e;
}

function resolveCipherName(algorithm) {
  let name = String(algorithm).toLowerCase();
  if (CIPHER_ALIASES[name]) name = CIPHER_ALIASES[name];
  return CIPHERS[name];
}

function initCipher(self, algorithm, key, iv, options, isDecipher) {
  const info = resolveCipherName(algorithm);
  if (!info) {
    throw new Error(
      `node:crypto cipher '${algorithm}' is not supported in lumen (aes-{128,192,256}-{ecb,cbc,ctr,gcm} only)`,
    );
  }
  const keyBytes = toBytes(key);
  if (keyBytes.length !== info.keyLength) {
    throw codedError(RangeError, "Invalid key length", "ERR_CRYPTO_INVALID_KEYLEN");
  }
  let ivBytes = iv == null ? null : toBytes(iv);
  if (info.mode === "ecb") {
    if (ivBytes && ivBytes.length !== 0) {
      throw codedError(TypeError, "Invalid initialization vector", "ERR_CRYPTO_INVALID_IV");
    }
    ivBytes = null;
  } else if (info.mode === "gcm") {
    if (!ivBytes || ivBytes.length === 0) {
      throw codedError(TypeError, "Invalid initialization vector", "ERR_CRYPTO_INVALID_IV");
    }
  } else if (!ivBytes || ivBytes.length !== 16) {
    throw codedError(TypeError, "Invalid initialization vector", "ERR_CRYPTO_INVALID_IV");
  }
  self._info = info;
  self._key = Uint8Array.from(keyBytes);
  self._iv = ivBytes ? Uint8Array.from(ivBytes) : null;
  self._decipher = isDecipher;
  self._autoPadding = true;
  self._state = 0; // 0 = init, 1 = updating, 2 = finalized
  self._buf = new Uint8Array(0);
  if (info.mode === "gcm") {
    self._tagLenExplicit = !!(options && options.authTagLength !== undefined);
    if (self._tagLenExplicit) {
      const n = options.authTagLength;
      if (!GCM_TAG_LENS.includes(n)) {
        throw codedError(TypeError, `Invalid authentication tag length: ${n}`, "ERR_CRYPTO_INVALID_AUTH_TAG");
      }
      self._tagLen = n;
    } else {
      self._tagLen = 16;
    }
    self._aad = new Uint8Array(0);
    self._ct = new Uint8Array(0); // accumulated ciphertext, for the tag at final()
    self._counter = Uint8Array.from(__crypto.gcmInit(self._key, self._iv)); // inc32(J0)
    self._ks = new Uint8Array(0);
    self._authTag = null; // decipher: set via setAuthTag
    self._tag = null; // cipher: computed at final()
  } else if (info.mode === "ctr") {
    self._counter = Uint8Array.from(self._iv);
    self._ks = new Uint8Array(0);
  }
}

// Bump the counter block: CTR increments all 128 bits, GCM only the low 32 (SP 800-38D inc32).
function incCounter(block, low32Only) {
  for (let i = 15; i >= (low32Only ? 12 : 0); i--) {
    block[i] = (block[i] + 1) & 0xff;
    if (block[i] !== 0) break;
  }
}

// XOR `input` against the CTR/GCM keystream: use the leftover partial keystream block first, then
// generate the rest in one batched native ECB call over consecutive counter blocks.
function keystreamXor(self, input) {
  const out = new Uint8Array(input.length);
  const left = Math.min(self._ks.length, input.length);
  for (let i = 0; i < left; i++) out[i] = input[i] ^ self._ks[i];
  self._ks = self._ks.subarray(left);
  const remaining = input.length - left;
  if (remaining > 0) {
    const nblocks = Math.ceil(remaining / 16);
    const counters = new Uint8Array(nblocks * 16);
    const low32 = self._info.mode === "gcm";
    for (let b = 0; b < nblocks; b++) {
      counters.set(self._counter, b * 16);
      incCounter(self._counter, low32);
    }
    const stream = __crypto.aesEcb(true, self._key, counters);
    for (let i = 0; i < remaining; i++) out[left + i] = input[left + i] ^ stream[i];
    self._ks = stream.subarray(remaining);
  }
  return out;
}

// ECB/CBC streaming: emit complete blocks, buffering the remainder. A decipher with auto-padding
// additionally holds the last full block back so final() can strip the PKCS#7 padding.
function blockUpdate(self, bytes) {
  const all = concatBytes(self._buf, bytes);
  let n = all.length - (all.length % 16);
  if (self._decipher && self._autoPadding && n > 0 && n === all.length) n -= 16;
  if (n <= 0) {
    self._buf = all;
    return new Uint8Array(0);
  }
  const chunk = all.subarray(0, n);
  self._buf = Uint8Array.from(all.subarray(n));
  let out;
  if (self._info.mode === "ecb") {
    out = __crypto.aesEcb(!self._decipher, self._key, chunk);
  } else {
    out = __crypto.aesCbc(!self._decipher, self._key, self._iv, chunk);
    // CBC chains through the last ciphertext block (output when encrypting, input when decrypting).
    const src = self._decipher ? chunk : out;
    self._iv = Uint8Array.from(src.subarray(src.length - 16));
  }
  return out;
}

function cipherUpdate(self, data, inputEncoding, outputEncoding) {
  if (self._state === 2) throw new Error("Trying to add data in unsupported state");
  self._state = 1;
  const bytes = toBytes(data, inputEncoding || "utf8");
  let out;
  const mode = self._info.mode;
  if (mode === "ecb" || mode === "cbc") {
    out = blockUpdate(self, bytes);
  } else {
    out = keystreamXor(self, bytes);
    if (mode === "gcm") self._ct = concatBytes(self._ct, self._decipher ? bytes : out);
  }
  const buf = Buffer.from(out);
  return outputEncoding && outputEncoding !== "buffer" ? buf.toString(outputEncoding) : buf;
}

const wrongFinalBlock = () =>
  codedError(Error, "error:1C80006B:Provider routines::wrong final block length", "ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH");
const badDecrypt = () =>
  codedError(Error, "error:1C800064:Provider routines::bad decrypt", "ERR_OSSL_BAD_DECRYPT");

function cipherFinal(self, outputEncoding) {
  if (self._state === 2) throw codedError(Error, "Invalid state", "ERR_CRYPTO_INVALID_STATE");
  self._state = 2;
  const mode = self._info.mode;
  let out = new Uint8Array(0);
  if (mode === "ecb" || mode === "cbc") {
    if (!self._decipher) {
      if (self._autoPadding) {
        const padLen = 16 - self._buf.length;
        const block = new Uint8Array(16);
        block.set(self._buf);
        block.fill(padLen, self._buf.length);
        out = mode === "ecb"
          ? __crypto.aesEcb(true, self._key, block)
          : __crypto.aesCbc(true, self._key, self._iv, block);
      } else if (self._buf.length !== 0) {
        throw wrongFinalBlock();
      }
    } else if (self._autoPadding) {
      // The held-back block must be exactly one block; anything else means misaligned input.
      if (self._buf.length !== 16) throw wrongFinalBlock();
      const block = mode === "ecb"
        ? __crypto.aesEcb(false, self._key, self._buf)
        : __crypto.aesCbc(false, self._key, self._iv, self._buf);
      const padLen = block[15];
      let ok = padLen >= 1 && padLen <= 16;
      for (let i = 16 - padLen; ok && i < 16; i++) if (block[i] !== padLen) ok = false;
      if (!ok) throw badDecrypt();
      out = block.subarray(0, 16 - padLen);
    } else if (self._buf.length !== 0) {
      throw wrongFinalBlock();
    }
  } else if (mode === "gcm") {
    const full = __crypto.gcmTag(self._key, self._iv, self._aad, self._ct);
    if (self._decipher) {
      const tag = self._authTag;
      let ok = tag !== null;
      if (ok) {
        let diff = 0;
        for (let i = 0; i < tag.length; i++) diff |= tag[i] ^ full[i];
        ok = diff === 0;
      }
      if (!ok) throw new Error("Unsupported state or unable to authenticate data");
    } else {
      self._tag = Uint8Array.from(full.subarray(0, self._tagLen));
    }
  }
  const buf = Buffer.from(out);
  return outputEncoding && outputEncoding !== "buffer" ? buf.toString(outputEncoding) : buf;
}

function cipherSetAAD(self, buffer, options) {
  if (self._info.mode !== "gcm" || self._state !== 0) {
    throw codedError(Error, "Invalid state for operation setAAD", "ERR_CRYPTO_INVALID_STATE");
  }
  self._aad = concatBytes(self._aad, toBytes(buffer, options && options.encoding));
  return self;
}

class Cipheriv {
  constructor(algorithm, key, iv, options) {
    initCipher(this, algorithm, key, iv, options, false);
  }
  update(data, inputEncoding, outputEncoding) {
    return cipherUpdate(this, data, inputEncoding, outputEncoding);
  }
  final(outputEncoding) {
    return cipherFinal(this, outputEncoding);
  }
  setAutoPadding(autoPadding) {
    this._autoPadding = autoPadding === undefined ? true : !!autoPadding;
    return this;
  }
  setAAD(buffer, options) {
    return cipherSetAAD(this, buffer, options);
  }
  getAuthTag() {
    if (this._info.mode !== "gcm" || this._state !== 2 || this._tag === null) {
      throw codedError(Error, "Invalid state for operation getAuthTag", "ERR_CRYPTO_INVALID_STATE");
    }
    return Buffer.from(this._tag);
  }
}

class Decipheriv {
  constructor(algorithm, key, iv, options) {
    initCipher(this, algorithm, key, iv, options, true);
  }
  update(data, inputEncoding, outputEncoding) {
    return cipherUpdate(this, data, inputEncoding, outputEncoding);
  }
  final(outputEncoding) {
    return cipherFinal(this, outputEncoding);
  }
  setAutoPadding(autoPadding) {
    this._autoPadding = autoPadding === undefined ? true : !!autoPadding;
    return this;
  }
  setAAD(buffer, options) {
    return cipherSetAAD(this, buffer, options);
  }
  setAuthTag(tag, encoding) {
    if (this._info.mode !== "gcm" || this._state === 2) {
      throw codedError(Error, "Invalid state for operation setAuthTag", "ERR_CRYPTO_INVALID_STATE");
    }
    const t = toBytes(tag, encoding);
    const valid = this._tagLenExplicit ? t.length === this._tagLen : GCM_TAG_LENS.includes(t.length);
    if (!valid) {
      throw codedError(TypeError, `Invalid authentication tag length: ${t.length}`, "ERR_CRYPTO_INVALID_AUTH_TAG");
    }
    this._authTag = Uint8Array.from(t);
    return this;
  }
}

// getCipherInfo mirrors Node: name (with the id-aes*-gcm OpenSSL names) or nid lookup; the
// keyLength/ivLength options act as a "does the cipher support this?" probe.
function getCipherInfo(nameOrNid, options) {
  let info;
  if (typeof nameOrNid === "number") {
    info = Object.values(CIPHERS).find((c) => c.nid === nameOrNid);
  } else {
    info = resolveCipherName(nameOrNid);
  }
  if (!info) return undefined;
  const result = { mode: info.mode, name: info.name, nid: info.nid, blockSize: info.blockSize };
  if (info.ivLength !== undefined) result.ivLength = info.ivLength;
  result.keyLength = info.keyLength;
  if (options && options.keyLength !== undefined && options.keyLength !== info.keyLength) return undefined;
  if (options && options.ivLength !== undefined) {
    if (info.mode === "ecb") return undefined;
    if (info.mode === "gcm") {
      if (options.ivLength < 1) return undefined;
      result.ivLength = options.ivLength; // GCM accepts variable IV sizes; Node echoes the query
    } else if (options.ivLength !== 16) {
      return undefined;
    }
  }
  return result;
}

// ---- scrypt (RFC 7914; ROMix is native, PBKDF2-HMAC-SHA256 wrapping is the JS one above) -------

function validateScryptNum(name, v, max) {
  if (typeof v !== "number") {
    const rendered = typeof v === "string" ? `'${v}'` : String(v);
    throw codedError(
      TypeError,
      `The "${name}" argument must be of type number. Received type ${typeof v} (${rendered})`,
      "ERR_INVALID_ARG_TYPE",
    );
  }
  if (!Number.isInteger(v)) {
    throw codedError(RangeError, `The value of "${name}" is out of range. It must be an integer. Received ${v}`, "ERR_OUT_OF_RANGE");
  }
  if (v < 0 || v > max) {
    throw codedError(RangeError, `The value of "${name}" is out of range. It must be >= 0 && <= ${max}. Received ${v}`, "ERR_OUT_OF_RANGE");
  }
  return v;
}

function scryptSync(password, salt, keylen, options) {
  validateScryptNum("keylen", keylen, 2147483647);
  const opts = options || {};
  const pickOpt = (primary, alias) => {
    if (opts[primary] !== undefined && opts[alias] !== undefined) {
      throw codedError(Error, "Invalid scrypt parameter", "ERR_CRYPTO_SCRYPT_INVALID_PARAMETER");
    }
    const name = opts[primary] !== undefined ? primary : alias;
    if (opts[name] === undefined) return undefined;
    return validateScryptNum(name, opts[name], 4294967295);
  };
  // Falsy (0/undefined) falls back to the default, matching Node's `|| default` behavior.
  const N = pickOpt("N", "cost") || 16384;
  const r = pickOpt("r", "blockSize") || 8;
  const p = pickOpt("p", "parallelization") || 1;
  let maxmem = 32 * 1024 * 1024;
  if (opts.maxmem !== undefined) maxmem = validateScryptNum("maxmem", opts.maxmem, Number.MAX_SAFE_INTEGER) || maxmem;
  if (N < 2 || (N & (N - 1)) !== 0) {
    throw codedError(RangeError, "Invalid scrypt params", "ERR_CRYPTO_INVALID_SCRYPT_PARAMS");
  }
  // OpenSSL's memory accounting: B (128*r*p) plus V (128*r*(N+2)) must fit in maxmem.
  if (128 * r * p + 128 * r * (N + 2) > maxmem) {
    throw codedError(
      RangeError,
      "Invalid scrypt params: error:030000AC:digital envelope routines::memory limit exceeded",
      "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
    );
  }
  const B = pbkdf2Sync(password, salt, 1, 128 * r * p, "sha256");
  const mixed = __crypto.scryptRomix(B, N, r);
  return Buffer.from(pbkdf2Sync(password, mixed, 1, keylen, "sha256"));
}

function scrypt(password, salt, keylen, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  if (typeof callback !== "function") throw new TypeError("callback must be a function");
  let result, err;
  try {
    result = scryptSync(password, salt, keylen, options);
  } catch (e) {
    err = e;
  }
  queueMicrotask(() => (err ? callback(err) : callback(null, result)));
}

// ---- honest stubs (no native primitive backs these) -------------------------------------------

function notImpl(name) {
  return () => {
    throw new Error(`node:crypto ${name} is not supported in lumen (no native primitive available)`);
  };
}

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
  getHashes: () => ["md5", "sha1", "sha256", "sha384", "sha512", "sha512-224", "sha512-256"],
  pbkdf2,
  pbkdf2Sync,
  hkdf,
  hkdfSync,
  scrypt,
  scryptSync,

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

  // -- real: symmetric ciphers (native AES; see lumen-node/src/crypto.rs) --
  createCipheriv: (algorithm, key, iv, options) => new Cipheriv(algorithm, key, iv, options),
  createDecipheriv: (algorithm, key, iv, options) => new Decipheriv(algorithm, key, iv, options),
  Cipheriv,
  Decipheriv,

  // -- real: introspection / config --
  // getCiphers lists exactly the cipher set lumen actually implements; no named curves exist.
  getCiphers: () => Object.keys(CIPHERS).slice().sort(),
  getCurves: () => [],
  getCipherInfo,
  getFips: () => 0,
  setFips: (v) => { if (v) throw new Error("FIPS mode is not supported in lumen"); },
  secureHeapUsed: () => ({ total: 0, min: 0, used: 0, utilization: 0 }),
  constants,

  // -- real: WebCrypto bridge (subtle backs SHA-256 digest + getRandomValues/randomUUID) --
  webcrypto: webCrypto,
  subtle: webCrypto.subtle,

  // -- real: sign/verify (Ed25519 + RSA PKCS#1 v1.5/PSS; ECDSA lands with the EC tier) --
  sign: cryptoSign,
  verify: cryptoVerify,
  createSign: (algorithm) => new Sign(algorithm),
  createVerify: (algorithm) => new Verify(algorithm),
  Sign,
  Verify,
  // -- real: RSA encryption (OAEP + PKCS#1 v1.5) --
  privateEncrypt,
  privateDecrypt,
  publicEncrypt,
  publicDecrypt,

  // -- real: asymmetric key management (ASN.1 DER/PEM/JWK, pure JS) --
  createPublicKey,
  createPrivateKey,
  generateKeyPair,
  generateKeyPairSync,
  // -- real: P-256 ECDH and KeyObject diffieHellman (P-256/X25519) --
  ECDH,
  createECDH,
  diffieHellman,
  // -- stubs: finite-field Diffie-Hellman --
  DiffieHellman: notImpl("DiffieHellman"),
  DiffieHellmanGroup: notImpl("DiffieHellmanGroup"),
  createDiffieHellman: notImpl("createDiffieHellman"),
  createDiffieHellmanGroup: notImpl("createDiffieHellmanGroup"),
  getDiffieHellman: notImpl("getDiffieHellman"),

  // -- real: probable primes (Miller-Rabin over BigInt) --
  checkPrime,
  checkPrimeSync,
  generatePrime,
  generatePrimeSync,
  // -- stubs: X.509 / legacy SPKAC --
  X509Certificate: notImpl("X509Certificate"),
  Certificate: notImpl("Certificate"),

  // -- stubs: engines --
  setEngine: notImpl("setEngine"),
};

__builtins.set("crypto", crypto);
