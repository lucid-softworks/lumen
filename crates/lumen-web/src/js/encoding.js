// TextEncoder/TextDecoder over the native utf-8 ops, base64 globals, structuredClone.

class TextEncoder {
  get encoding() {
    return "utf-8";
  }
  encode(input = "") {
    return __encoding.encode(String(input));
  }
}

class TextDecoder {
  constructor(label = "utf-8", options = {}) {
    const l = String(label).toLowerCase();
    if (l !== "utf-8" && l !== "utf8" && l !== "unicode-1-1-utf-8") {
      throw new RangeError(`TextDecoder: unsupported encoding '${label}' (utf-8 only for now)`);
    }
    options = options && typeof options === "object" ? options : {};
    this.encoding = "utf-8";
    this.fatal = !!options.fatal;
    this.ignoreBOM = !!options.ignoreBOM;
  }
  decode(input) {
    if (input === undefined) return "";
    if (input instanceof ArrayBuffer) input = new Uint8Array(input);
    let s = __encoding.decode(input, this.fatal);
    if (!this.ignoreBOM && s.charCodeAt(0) === 0xfeff) s = s.slice(1);
    return s;
  }
}

const B64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function btoa(data) {
  const s = String(data);
  let out = "";
  for (let i = 0; i < s.length; i += 3) {
    const cs = [s.charCodeAt(i), s.charCodeAt(i + 1), s.charCodeAt(i + 2)];
    if (cs[0] > 255 || cs[1] > 255 || cs[2] > 255) {
      throw new DOMException("btoa: character beyond latin1 range", "InvalidCharacterError");
    }
    const n = (cs[0] << 16) | ((cs[1] || 0) << 8) | (cs[2] || 0);
    out += B64_ALPHABET[(n >> 18) & 63];
    out += B64_ALPHABET[(n >> 12) & 63];
    out += i + 1 < s.length ? B64_ALPHABET[(n >> 6) & 63] : "=";
    out += i + 2 < s.length ? B64_ALPHABET[n & 63] : "=";
  }
  return out;
}

function atob(data) {
  let s = String(data).replace(/[\t\n\f\r ]/g, "");
  if (s.length % 4 === 0) s = s.replace(/==?$/, "");
  if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) {
    throw new DOMException("atob: invalid base64", "InvalidCharacterError");
  }
  let out = "";
  for (let i = 0; i < s.length; i += 4) {
    const bits = [0, 1, 2, 3].map((j) =>
      j + i < s.length ? B64_ALPHABET.indexOf(s[i + j]) : 0
    );
    const n = (bits[0] << 18) | (bits[1] << 12) | (bits[2] << 6) | bits[3];
    out += String.fromCharCode((n >> 16) & 255);
    if (i + 2 < s.length) out += String.fromCharCode((n >> 8) & 255);
    if (i + 3 < s.length) out += String.fromCharCode(n & 255);
  }
  return out;
}

function structuredClone(value) {
  // No transfer list yet; throws DataCloneError exactly where the spec does.
  const seen = new Map();
  const clone = (v) => {
    if (typeof v === "function" || typeof v === "symbol") {
      throw new DOMException("value could not be cloned", "DataCloneError");
    }
    if (v === null || typeof v !== "object") return v;
    if (seen.has(v)) return seen.get(v);
    if (v instanceof Date) return new Date(v.getTime());
    if (v instanceof RegExp) return new RegExp(v.source, v.flags);
    if (v instanceof Promise) {
      throw new DOMException("a Promise cannot be cloned", "DataCloneError");
    }
    if (v instanceof ArrayBuffer) {
      const c = v.slice(0);
      seen.set(v, c);
      return c;
    }
    if (ArrayBuffer.isView(v) && !(v instanceof DataView)) {
      const c = new v.constructor(v);
      seen.set(v, c);
      return c;
    }
    if (v instanceof Map) {
      const m = new Map();
      seen.set(v, m);
      for (const [k, val] of v) m.set(clone(k), clone(val));
      return m;
    }
    if (v instanceof Set) {
      const s = new Set();
      seen.set(v, s);
      for (const item of v) s.add(clone(item));
      return s;
    }
    if (v instanceof Error) {
      const ctor = typeof v.constructor === "function" ? v.constructor : Error;
      const e = new ctor(v.message);
      e.name = v.name;
      seen.set(v, e);
      return e;
    }
    if (Array.isArray(v)) {
      const a = [];
      seen.set(v, a);
      for (let i = 0; i < v.length; i++) if (i in v) a[i] = clone(v[i]);
      return a;
    }
    const o = {};
    seen.set(v, o);
    for (const k of Object.keys(v)) o[k] = clone(v[k]);
    return o;
  };
  return clone(value);
}

globalThis.TextEncoder = TextEncoder;
globalThis.TextDecoder = TextDecoder;
globalThis.btoa = btoa;
globalThis.atob = atob;
globalThis.structuredClone = structuredClone;
