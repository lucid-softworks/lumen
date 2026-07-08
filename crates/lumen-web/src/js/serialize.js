// StructuredSerialize/StructuredDeserialize to/from a flat byte buffer (Uint8Array) — the wire
// format that carries a structured clone ACROSS a thread boundary (Worker.postMessage). The
// in-realm `structuredClone` (encoding.js) stays a direct object copy; this pair is only for
// serialize-here / deserialize-there, where the two objects live in different realms and can't
// share references. Cycles and shared subgraphs are preserved via a memory (index) table, matching
// the spec's serialization memory. Unsupported inputs (functions, symbols, promises) throw
// DataCloneError exactly where structuredClone does.
//
// Format: a tag byte then a type-specific payload. Multi-byte integers are little-endian; f64 uses
// the platform DataView. Object/array/Map/Set/ArrayBuffer register in the memory before their
// contents are written, so a self-reference emits a back-ref (tag REF + u32 index).

const T_UNDEFINED = 0;
const T_NULL = 1;
const T_FALSE = 2;
const T_TRUE = 3;
const T_NUMBER = 4;
const T_STRING = 5;
const T_BIGINT = 6;
const T_REF = 7;
const T_ARRAY = 8;
const T_OBJECT = 9;
const T_DATE = 10;
const T_REGEXP = 11;
const T_MAP = 12;
const T_SET = 13;
const T_ARRAYBUFFER = 14;
const T_TYPEDARRAY = 15;
const T_DATAVIEW = 16;
const T_ERROR = 17;
const T_BOOLOBJ = 18;
const T_NUMOBJ = 19;
const T_STROBJ = 20;

// TypedArray kinds by constructor name — index is the wire byte.
const TA_KINDS = [
  "Int8Array", "Uint8Array", "Uint8ClampedArray", "Int16Array", "Uint16Array",
  "Int32Array", "Uint32Array", "Float32Array", "Float64Array",
  "BigInt64Array", "BigUint64Array",
];
const ERROR_NAMES = ["Error", "TypeError", "RangeError", "ReferenceError", "SyntaxError", "EvalError", "URIError"];

function serializeForClone(value) {
  // A growable Uint8Array sink (capacity doubling) — orders of magnitude faster than pushing
  // numbers into a plain Array and `Uint8Array.from`-ing at the end.
  let buf = new Uint8Array(64);
  let len = 0;
  let dv = new DataView(buf.buffer);
  const memory = new Map(); // object -> assigned ref index
  const utf8 = new TextEncoder();

  const ensure = (extra) => {
    if (len + extra <= buf.length) return;
    let cap = buf.length * 2;
    while (cap < len + extra) cap *= 2;
    const next = new Uint8Array(cap);
    next.set(buf);
    buf = next;
    dv = new DataView(buf.buffer);
  };
  const u8 = (n) => { ensure(1); buf[len++] = n & 0xff; };
  const u32 = (n) => { ensure(4); dv.setUint32(len, n >>> 0, true); len += 4; };
  const f64 = (n) => { ensure(8); dv.setFloat64(len, n, true); len += 8; };
  const raw = (arr) => { ensure(arr.length); buf.set(arr, len); len += arr.length; };
  const str = (s) => {
    const enc = utf8.encode(String(s));
    u32(enc.length);
    raw(enc);
  };
  const rawBytes = (arr) => { u32(arr.length); raw(arr); };

  const write = (v) => {
    if (v === undefined) return u8(T_UNDEFINED);
    if (v === null) return u8(T_NULL);
    const t = typeof v;
    if (t === "boolean") return u8(v ? T_TRUE : T_FALSE);
    if (t === "number") { u8(T_NUMBER); return f64(v); }
    if (t === "string") { u8(T_STRING); return str(v); }
    if (t === "bigint") { u8(T_BIGINT); return str(v.toString()); }
    if (t === "function" || t === "symbol") {
      throw new DOMException("value could not be cloned", "DataCloneError");
    }
    // Objects: a repeat emits a back-ref.
    if (memory.has(v)) { u8(T_REF); return u32(memory.get(v)); }
    const ref = memory.size;

    if (v instanceof Date) { u8(T_DATE); return f64(v.getTime()); }
    if (v instanceof RegExp) { u8(T_REGEXP); str(v.source); return str(v.flags); }
    if (v instanceof Promise) {
      throw new DOMException("a Promise cannot be cloned", "DataCloneError");
    }
    if (v instanceof ArrayBuffer) {
      memory.set(v, ref);
      u8(T_ARRAYBUFFER);
      return rawBytes(new Uint8Array(v));
    }
    if (v instanceof DataView) {
      memory.set(v, ref);
      u8(T_DATAVIEW);
      u32(v.byteOffset);
      u32(v.byteLength);
      return write(v.buffer);
    }
    if (ArrayBuffer.isView(v)) {
      const kind = TA_KINDS.indexOf(v.constructor.name);
      if (kind === -1) throw new DOMException("unsupported typed array", "DataCloneError");
      memory.set(v, ref);
      u8(T_TYPEDARRAY);
      u8(kind);
      u32(v.byteOffset);
      u32(v.length);
      return write(v.buffer);
    }
    if (v instanceof Map) {
      memory.set(v, ref);
      u8(T_MAP);
      u32(v.size);
      for (const [k, val] of v) { write(k); write(val); }
      return;
    }
    if (v instanceof Set) {
      memory.set(v, ref);
      u8(T_SET);
      u32(v.size);
      for (const item of v) write(item);
      return;
    }
    if (v instanceof Error) {
      memory.set(v, ref);
      u8(T_ERROR);
      u8(Math.max(0, ERROR_NAMES.indexOf(v.name)));
      str(v.message);
      return str(v.stack ?? "");
    }
    // Boxed primitives.
    const tag = Object.prototype.toString.call(v);
    if (tag === "[object Boolean]") { memory.set(v, ref); u8(T_BOOLOBJ); return u8(v.valueOf() ? 1 : 0); }
    if (tag === "[object Number]") { memory.set(v, ref); u8(T_NUMOBJ); return f64(v.valueOf()); }
    if (tag === "[object String]") { memory.set(v, ref); u8(T_STROBJ); return str(v.valueOf()); }

    if (Array.isArray(v)) {
      memory.set(v, ref);
      u8(T_ARRAY);
      u32(v.length);
      // Own enumerable string keys beyond indices are cloned too (sparse arrays keep holes as
      // undefined, matching structuredClone's behavior here).
      for (let i = 0; i < v.length; i++) write(v[i]);
      return;
    }
    // Plain object (its own enumerable string-keyed properties).
    memory.set(v, ref);
    u8(T_OBJECT);
    const keys = Object.keys(v);
    u32(keys.length);
    for (const k of keys) { str(k); write(v[k]); }
  };

  write(value);
  return buf.subarray(0, len);
}

function deserializeClone(buf) {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const utf8 = new TextDecoder();
  const memory = []; // ref index -> value
  let pos = 0;

  const u8 = () => buf[pos++];
  const u32 = () => {
    const n = view.getUint32(pos, true);
    pos += 4;
    return n;
  };
  const f64 = () => {
    const n = view.getFloat64(pos, true);
    pos += 8;
    return n;
  };
  const str = () => {
    const len = u32();
    const s = utf8.decode(buf.subarray(pos, pos + len));
    pos += len;
    return s;
  };

  const read = () => {
    const tag = u8();
    switch (tag) {
      case T_UNDEFINED: return undefined;
      case T_NULL: return null;
      case T_FALSE: return false;
      case T_TRUE: return true;
      case T_NUMBER: return f64();
      case T_STRING: return str();
      case T_BIGINT: return BigInt(str());
      case T_REF: return memory[u32()];
      case T_DATE: return new Date(f64());
      case T_REGEXP: { const source = str(); return new RegExp(source, str()); }
      case T_ARRAYBUFFER: {
        const len = u32();
        const ab = new ArrayBuffer(len);
        new Uint8Array(ab).set(buf.subarray(pos, pos + len));
        pos += len;
        memory.push(ab);
        return ab;
      }
      case T_DATAVIEW: {
        const byteOffset = u32();
        const byteLength = u32();
        const dv = new DataView(read(), byteOffset, byteLength);
        memory.push(dv);
        return dv;
      }
      case T_TYPEDARRAY: {
        const kind = u8();
        const byteOffset = u32();
        const length = u32();
        const ctor = globalThis[TA_KINDS[kind]];
        const ta = new ctor(read(), byteOffset, length);
        memory.push(ta);
        return ta;
      }
      case T_MAP: {
        const m = new Map();
        memory.push(m);
        const n = u32();
        for (let i = 0; i < n; i++) { const k = read(); m.set(k, read()); }
        return m;
      }
      case T_SET: {
        const s = new Set();
        memory.push(s);
        const n = u32();
        for (let i = 0; i < n; i++) s.add(read());
        return s;
      }
      case T_ERROR: {
        const ctor = globalThis[ERROR_NAMES[u8()]] ?? Error;
        const e = new ctor(str());
        e.stack = str();
        memory.push(e);
        return e;
      }
      case T_BOOLOBJ: { const o = new Boolean(u8() === 1); memory.push(o); return o; }
      case T_NUMOBJ: { const o = new Number(f64()); memory.push(o); return o; }
      case T_STROBJ: { const o = new String(str()); memory.push(o); return o; }
      case T_ARRAY: {
        const a = [];
        memory.push(a);
        const len = u32();
        for (let i = 0; i < len; i++) a[i] = read();
        return a;
      }
      case T_OBJECT: {
        const o = {};
        memory.push(o);
        const n = u32();
        for (let i = 0; i < n; i++) { const k = str(); o[k] = read(); }
        return o;
      }
      default:
        throw new DOMException("malformed clone data", "DataCloneError");
    }
  };
  return read();
}

// Exposed for the Worker bridge (worker.js) — not global API.
globalThis.__serializeForClone = serializeForClone;
globalThis.__deserializeClone = deserializeClone;
