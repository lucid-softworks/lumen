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
  static isBuffer(x) {
    return x instanceof Buffer;
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

globalThis.Buffer = Buffer;
__builtins.set("buffer", { Buffer, constants: {} });
