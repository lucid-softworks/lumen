// Buffer — a Uint8Array subclass with Node's codec + accessor surface (the common slice of
// it). Encodings: utf8/utf-8, hex, base64, base64url, latin1/binary, ascii.

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function normEncoding(enc) {
  enc = (enc || "utf8").toLowerCase();
  if (enc === "utf-8") return "utf8";
  if (enc === "ucs2" || enc === "ucs-2") return "utf16le";
  if (enc === "binary") return "latin1";
  return enc;
}

function bytesFromString(str, enc) {
  enc = normEncoding(enc);
  switch (enc) {
    case "utf8":
      return new TextEncoder().encode(str);
    case "ascii":
    case "latin1": {
      const out = new Uint8Array(str.length);
      for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0xff;
      return out;
    }
    case "hex": {
      const clean = str.replace(/[^0-9a-fA-F]/g, "");
      const n = clean.length >> 1;
      const out = new Uint8Array(n);
      for (let i = 0; i < n; i++) out[i] = parseInt(clean.substr(i * 2, 2), 16);
      return out;
    }
    case "base64":
    case "base64url": {
      let s = str.replace(/[-_]/g, (c) => (c === "-" ? "+" : "/")).replace(/[^A-Za-z0-9+/]/g, "");
      const pad = s.length % 4;
      const bytes = [];
      for (let i = 0; i < s.length; i += 4) {
        const q = [0, 1, 2, 3].map((j) => (i + j < s.length ? B64.indexOf(s[i + j]) : 0));
        const n = (q[0] << 18) | (q[1] << 12) | (q[2] << 6) | q[3];
        bytes.push((n >> 16) & 0xff);
        if (i + 2 < s.length) bytes.push((n >> 8) & 0xff);
        if (i + 3 < s.length) bytes.push(n & 0xff);
      }
      void pad;
      return new Uint8Array(bytes);
    }
    case "utf16le": {
      const out = new Uint8Array(str.length * 2);
      for (let i = 0; i < str.length; i++) {
        const c = str.charCodeAt(i);
        out[i * 2] = c & 0xff;
        out[i * 2 + 1] = c >> 8;
      }
      return out;
    }
    default:
      throw new TypeError(`Unknown encoding: ${enc}`);
  }
}

function stringFromBytes(bytes, enc, start, end) {
  start = start || 0;
  end = end === undefined ? bytes.length : end;
  const view = bytes.subarray(start, end);
  enc = normEncoding(enc);
  switch (enc) {
    case "utf8":
      return new TextDecoder().decode(view);
    case "ascii": {
      let s = "";
      for (const b of view) s += String.fromCharCode(b & 0x7f);
      return s;
    }
    case "latin1": {
      let s = "";
      for (const b of view) s += String.fromCharCode(b);
      return s;
    }
    case "hex": {
      let s = "";
      for (const b of view) s += b.toString(16).padStart(2, "0");
      return s;
    }
    case "base64":
    case "base64url": {
      let s = "";
      for (let i = 0; i < view.length; i += 3) {
        const n = (view[i] << 16) | ((view[i + 1] || 0) << 8) | (view[i + 2] || 0);
        s += B64[(n >> 18) & 63] + B64[(n >> 12) & 63];
        s += i + 1 < view.length ? B64[(n >> 6) & 63] : "=";
        s += i + 2 < view.length ? B64[n & 63] : "=";
      }
      if (enc === "base64url") s = s.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      return s;
    }
    case "utf16le": {
      let s = "";
      for (let i = 0; i + 1 < view.length; i += 2) s += String.fromCharCode(view[i] | (view[i + 1] << 8));
      return s;
    }
    default:
      throw new TypeError(`Unknown encoding: ${enc}`);
  }
}

class Buffer extends Uint8Array {
  static from(value, encOrOffset, length) {
    if (typeof value === "string") return new Buffer(bytesFromString(value, encOrOffset));
    if (value instanceof ArrayBuffer) {
      const u = length === undefined ? new Uint8Array(value, encOrOffset || 0) : new Uint8Array(value, encOrOffset || 0, length);
      return new Buffer(u);
    }
    if (ArrayBuffer.isView(value)) return new Buffer(new Uint8Array(value));
    if (Array.isArray(value) || (value && typeof value.length === "number")) {
      const b = new Buffer(value.length);
      for (let i = 0; i < value.length; i++) b[i] = value[i] & 0xff;
      return b;
    }
    throw new TypeError("Buffer.from: unsupported value");
  }
  static alloc(size, fill, enc) {
    const b = new Buffer(size);
    if (fill !== undefined && fill !== 0) {
      if (typeof fill === "string") {
        const src = bytesFromString(fill, enc);
        for (let i = 0; i < size; i++) b[i] = src.length ? src[i % src.length] : 0;
      } else {
        b.fill(fill & 0xff);
      }
    }
    return b;
  }
  static allocUnsafe(size) {
    return new Buffer(size);
  }
  // Node distinguishes allocUnsafeSlow (un-pooled) from allocUnsafe; we don't pool, so they are
  // the same. Its presence also matters for feature-detection: safe-buffer only passes the real
  // Buffer through when all four of from/alloc/allocUnsafe/allocUnsafeSlow exist.
  static allocUnsafeSlow(size) {
    return new Buffer(size);
  }
  static isBuffer(x) {
    return x instanceof Buffer;
  }
  static isEncoding(enc) {
    return typeof enc === "string" && ["utf8", "utf-8", "hex", "base64", "base64url", "latin1", "binary", "ascii", "ucs2", "ucs-2", "utf16le", "utf-16le"].includes(enc.toLowerCase());
  }
  static compare(a, b) {
    if (!Buffer.isBuffer(a) || !Buffer.isBuffer(b)) throw new TypeError("Arguments must be Buffers");
    const len = Math.min(a.length, b.length);
    for (let i = 0; i < len; i++) {
      if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
    }
    return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
  }
  static byteLength(str, enc) {
    if (typeof str !== "string") return str.byteLength ?? str.length ?? 0;
    return bytesFromString(str, enc).length;
  }
  static concat(list, totalLength) {
    if (totalLength === undefined) {
      totalLength = 0;
      for (const b of list) totalLength += b.length;
    }
    const out = new Buffer(totalLength);
    let offset = 0;
    for (const b of list) {
      if (offset >= totalLength) break;
      out.set(b.subarray(0, Math.min(b.length, totalLength - offset)), offset);
      offset += b.length;
    }
    return out;
  }
  toString(enc, start, end) {
    return stringFromBytes(this, enc, start, end);
  }
  write(string, offset, length, enc) {
    if (typeof offset === "string") {
      enc = offset;
      offset = 0;
      length = this.length;
    } else if (typeof length === "string") {
      enc = length;
      length = this.length - offset;
    }
    offset = offset || 0;
    const src = bytesFromString(string, enc);
    const n = Math.min(src.length, length === undefined ? this.length - offset : length, this.length - offset);
    this.set(src.subarray(0, n), offset);
    return n;
  }
  slice(start, end) {
    return new Buffer(this.subarray(start, end));
  }
  equals(other) {
    if (!(other instanceof Uint8Array) || other.length !== this.length) return false;
    for (let i = 0; i < this.length; i++) if (this[i] !== other[i]) return false;
    return true;
  }
  compare(other) {
    const n = Math.min(this.length, other.length);
    for (let i = 0; i < n; i++) {
      if (this[i] !== other[i]) return this[i] < other[i] ? -1 : 1;
    }
    return this.length === other.length ? 0 : this.length < other.length ? -1 : 1;
  }
  toJSON() {
    return { type: "Buffer", data: Array.from(this) };
  }
  readUInt8(o = 0) {
    return this[o];
  }
  writeUInt8(v, o = 0) {
    this[o] = v & 0xff;
    return o + 1;
  }
  readUInt16LE(o = 0) {
    return this[o] | (this[o + 1] << 8);
  }
  readUInt16BE(o = 0) {
    return (this[o] << 8) | this[o + 1];
  }
  readUInt32LE(o = 0) {
    return (this[o] | (this[o + 1] << 8) | (this[o + 2] << 16) | (this[o + 3] << 24)) >>> 0;
  }
  readUInt32BE(o = 0) {
    return ((this[o] << 24) | (this[o + 1] << 16) | (this[o + 2] << 8) | this[o + 3]) >>> 0;
  }
  writeUInt32LE(v, o = 0) {
    this[o] = v & 0xff;
    this[o + 1] = (v >>> 8) & 0xff;
    this[o + 2] = (v >>> 16) & 0xff;
    this[o + 3] = (v >>> 24) & 0xff;
    return o + 4;
  }
}

// Node deprecated SlowBuffer (it used to return an un-pooled Buffer); we never pool, so it is just
// an un-pooled allocation.
function SlowBuffer(length) {
  return Buffer.allocUnsafeSlow(length);
}

// Coerce the ArrayBuffer/TypedArray/DataView inputs that isAscii/isUtf8 accept into a byte view,
// throwing Node's ERR_INVALID_ARG_TYPE for anything else (notably strings, which Node rejects).
function bytesOf(input, name) {
  if (input instanceof ArrayBuffer) return new Uint8Array(input);
  if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  const err = new TypeError(
    `The "${name}" argument must be an instance of ArrayBuffer, Buffer, TypedArray, or DataView. Received ${typeof input}`,
  );
  err.code = "ERR_INVALID_ARG_TYPE";
  throw err;
}

function isAscii(input) {
  const b = bytesOf(input, "input");
  for (let i = 0; i < b.length; i++) if (b[i] > 0x7f) return false;
  return true;
}

// A real UTF-8 validator: walks the byte stream, rejecting overlong forms, surrogate code points,
// out-of-range code points, and truncated/misaligned continuation bytes.
function isUtf8(input) {
  const b = bytesOf(input, "input");
  const n = b.length;
  let i = 0;
  while (i < n) {
    const c = b[i];
    if (c < 0x80) { i++; continue; }
    let extra, min, cp;
    if ((c & 0xe0) === 0xc0) { extra = 1; min = 0x80; cp = c & 0x1f; }
    else if ((c & 0xf0) === 0xe0) { extra = 2; min = 0x800; cp = c & 0x0f; }
    else if ((c & 0xf8) === 0xf0) { extra = 3; min = 0x10000; cp = c & 0x07; }
    else return false;
    if (i + extra >= n) return false;
    for (let j = 1; j <= extra; j++) {
      const cc = b[i + j];
      if ((cc & 0xc0) !== 0x80) return false;
      cp = (cp << 6) | (cc & 0x3f);
    }
    if (cp < min || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) return false;
    i += extra + 1;
  }
  return true;
}

// Re-encode bytes from one encoding to another by round-tripping through a JS string. Covers every
// pair the Buffer codec already supports; unknown encodings throw a Node-shaped ERR_UNKNOWN_ENCODING
// (Node itself surfaces an ICU error here, but an explicit unknown-encoding throw is the honest
// signal on an engine without ICU's transcoder).
function transcode(source, fromEnc, toEnc) {
  if (!ArrayBuffer.isView(source) && !(source instanceof ArrayBuffer)) {
    const err = new TypeError('The "source" argument must be an instance of Buffer or Uint8Array.');
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  for (const enc of [fromEnc, toEnc]) {
    if (!Buffer.isEncoding(enc)) {
      const err = new Error(`Unknown encoding: ${enc}`);
      err.code = "ERR_UNKNOWN_ENCODING";
      throw err;
    }
  }
  const bytes =
    source instanceof ArrayBuffer
      ? new Uint8Array(source)
      : new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  return new Buffer(bytesFromString(stringFromBytes(bytes, fromEnc), toEnc));
}

// lumen has no URL.createObjectURL registry, so every blob: id is unknown — which is exactly the
// value Node returns for an unregistered/expired id.
function resolveObjectURL(_id) {
  return undefined;
}

const kMaxLength = 9007199254740991;
const kStringMaxLength = 536870888;

globalThis.Buffer = Buffer;
__builtins.set("buffer", {
  Buffer,
  SlowBuffer,
  // Node exposes MAX_LENGTH/MAX_STRING_LENGTH here too (mirrors the k* aliases below).
  constants: { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: kStringMaxLength },
  kMaxLength,
  kStringMaxLength,
  INSPECT_MAX_BYTES: 50,
  // Reuse the web globals by identity rather than redefining them (lumen-web installs these).
  atob: globalThis.atob,
  btoa: globalThis.btoa,
  Blob: globalThis.Blob,
  File: globalThis.File,
  isAscii,
  isUtf8,
  transcode,
  resolveObjectURL,
});
