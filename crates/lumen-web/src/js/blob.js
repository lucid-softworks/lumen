// Blob / File / FormData. Byte containers over the buffered-body model: a Blob normalizes its
// parts to one Uint8Array at construction. FormData serializes to multipart/form-data (see
// encodeFormData), consumed by the fetch body path. Loaded before fetch.js so its bodyInit
// handling can reference these.

const kBlobBytes = Symbol("blobBytes");
const kBlobType = Symbol("blobType");

// Clamp a Blob.slice() index (negative = from the end), per spec.
function clampSlice(value, size) {
  if (value === undefined) return undefined;
  value = Math.trunc(Number(value)) || 0;
  return value < 0 ? Math.max(size + value, 0) : Math.min(value, size);
}

function partToBytes(part) {
  if (part instanceof Blob) return part[kBlobBytes];
  if (typeof part === "string") return new TextEncoder().encode(part);
  if (part instanceof ArrayBuffer) return new Uint8Array(part.slice(0));
  if (ArrayBuffer.isView(part)) return new Uint8Array(part.buffer.slice(part.byteOffset, part.byteOffset + part.byteLength));
  return new TextEncoder().encode(String(part));
}

class Blob {
  constructor(parts = [], options = {}) {
    if (parts != null && (typeof parts === "string" || typeof parts[Symbol.iterator] !== "function")) {
      throw new TypeError("Blob parts must be an iterable (e.g. an array)");
    }
    const chunks = [];
    let size = 0;
    for (const part of parts ?? []) {
      const bytes = partToBytes(part);
      chunks.push(bytes);
      size += bytes.length;
    }
    const all = new Uint8Array(size);
    let off = 0;
    for (const c of chunks) {
      all.set(c, off);
      off += c.length;
    }
    this[kBlobBytes] = all;
    const type = options && options.type ? String(options.type) : "";
    // A Blob's type is lowercased; a value with out-of-range chars is dropped to "".
    this[kBlobType] = /[^ -~]/.test(type) ? "" : type.toLowerCase();
  }
  get size() {
    return this[kBlobBytes].length;
  }
  get type() {
    return this[kBlobType];
  }
  slice(start, end, contentType) {
    const bytes = this[kBlobBytes];
    const s = clampSlice(start, bytes.length) ?? 0;
    const e = clampSlice(end, bytes.length) ?? bytes.length;
    const b = new Blob([], { type: contentType });
    b[kBlobBytes] = bytes.slice(s, Math.max(s, e));
    return b;
  }
  async text() {
    return new TextDecoder().decode(this[kBlobBytes]);
  }
  async arrayBuffer() {
    const b = this[kBlobBytes];
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
  async bytes() {
    return this[kBlobBytes].slice();
  }
  stream() {
    const bytes = this[kBlobBytes];
    return new ReadableStream({
      start(controller) {
        if (bytes.length) controller.enqueue(bytes.slice());
        controller.close();
      },
    });
  }
  get [Symbol.toStringTag]() {
    return "Blob";
  }
}

const kFileName = Symbol("fileName");
const kFileLastMod = Symbol("fileLastModified");

class File extends Blob {
  constructor(parts, name, options = {}) {
    if (arguments.length < 2) throw new TypeError("File requires fileBits and fileName");
    super(parts, options);
    this[kFileName] = String(name);
    this[kFileLastMod] =
      options && options.lastModified !== undefined ? Number(options.lastModified) : Date.now();
  }
  get name() {
    return this[kFileName];
  }
  get lastModified() {
    return this[kFileLastMod];
  }
  get [Symbol.toStringTag]() {
    return "File";
  }
}

const kEntries = Symbol("formEntries");

// A FormData entry value is either a string or a File (a Blob value is wrapped in a File).
function toEntryValue(value, filename) {
  if (value instanceof Blob) {
    if (filename !== undefined) return new File([value], String(filename), { type: value.type });
    if (value instanceof File) return value;
    return new File([value], "blob", { type: value.type });
  }
  if (filename !== undefined) {
    throw new TypeError("FormData: a filename is only valid with a Blob/File value");
  }
  return String(value);
}

class FormData {
  constructor() {
    this[kEntries] = [];
  }
  append(name, value, filename) {
    this[kEntries].push([String(name), toEntryValue(value, filename)]);
  }
  set(name, value, filename) {
    name = String(name);
    const v = toEntryValue(value, filename);
    const out = [];
    let done = false;
    for (const [n, val] of this[kEntries]) {
      if (n === name) {
        if (!done) {
          out.push([name, v]);
          done = true;
        }
      } else {
        out.push([n, val]);
      }
    }
    if (!done) out.push([name, v]);
    this[kEntries] = out;
  }
  get(name) {
    name = String(name);
    const e = this[kEntries].find(([n]) => n === name);
    return e ? e[1] : null;
  }
  getAll(name) {
    name = String(name);
    return this[kEntries].filter(([n]) => n === name).map(([, v]) => v);
  }
  has(name) {
    name = String(name);
    return this[kEntries].some(([n]) => n === name);
  }
  delete(name) {
    name = String(name);
    this[kEntries] = this[kEntries].filter(([n]) => n !== name);
  }
  *entries() {
    for (const [n, v] of this[kEntries]) yield [n, v];
  }
  *keys() {
    for (const [n] of this[kEntries]) yield n;
  }
  *values() {
    for (const [, v] of this[kEntries]) yield v;
  }
  forEach(callback, thisArg) {
    for (const [n, v] of this[kEntries]) callback.call(thisArg, v, n, this);
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  get [Symbol.toStringTag]() {
    return "FormData";
  }
}

// Serialize FormData to multipart/form-data bytes + the matching Content-Type (with boundary).
function encodeFormData(form) {
  const boundary = "----lumenFormBoundary" + crypto.randomUUID().replace(/-/g, "");
  const enc = new TextEncoder();
  const chunks = [];
  const push = (s) => chunks.push(typeof s === "string" ? enc.encode(s) : s);
  for (const [name, value] of form[kEntries]) {
    push(`--${boundary}\r\n`);
    const safeName = name.replace(/"/g, "%22").replace(/\r?\n/g, "%0A");
    if (value instanceof Blob) {
      const filename = (value instanceof File ? value.name : "blob").replace(/"/g, "%22");
      push(`Content-Disposition: form-data; name="${safeName}"; filename="${filename}"\r\n`);
      push(`Content-Type: ${value.type || "application/octet-stream"}\r\n\r\n`);
      push(value[kBlobBytes]);
      push("\r\n");
    } else {
      push(`Content-Disposition: form-data; name="${safeName}"\r\n\r\n`);
      push(`${value}\r\n`);
    }
  }
  push(`--${boundary}--\r\n`);
  let size = 0;
  for (const c of chunks) size += c.length;
  const out = new Uint8Array(size);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return { bytes: out, contentType: `multipart/form-data; boundary=${boundary}` };
}

// Byte-level substring search (multipart bodies are binary, so we can't decode-then-split).
function indexOfBytes(haystack, needle, from) {
  outer: for (let i = from; i <= haystack.length - needle.length; i++) {
    for (let j = 0; j < needle.length; j++) {
      if (haystack[i + j] !== needle[j]) continue outer;
    }
    return i;
  }
  return -1;
}

// Parse multipart/form-data bytes back into a FormData (the inverse of encodeFormData; also parses
// bodies produced by other clients). Used by Request/Response.formData().
function decodeMultipart(bytes, boundary) {
  const fd = new FormData();
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const marker = enc.encode(`--${boundary}`);
  const headerSep = enc.encode("\r\n\r\n");
  let pos = indexOfBytes(bytes, marker, 0);
  while (pos !== -1) {
    let start = pos + marker.length;
    if (bytes[start] === 0x2d && bytes[start + 1] === 0x2d) break; // closing "--boundary--"
    if (bytes[start] === 0x0d && bytes[start + 1] === 0x0a) start += 2; // skip CRLF after boundary
    const headerEnd = indexOfBytes(bytes, headerSep, start);
    if (headerEnd === -1) break;
    const headerText = dec.decode(bytes.subarray(start, headerEnd));
    const bodyStart = headerEnd + 4;
    const next = indexOfBytes(bytes, marker, bodyStart);
    if (next === -1) break;
    const body = bytes.subarray(bodyStart, next - 2); // drop the CRLF before the next boundary

    let name = null;
    let filename = null;
    let ctype = "";
    for (const line of headerText.split("\r\n")) {
      if (/^content-disposition:/i.test(line)) {
        const nm = /name="([^"]*)"/i.exec(line);
        if (nm) name = nm[1].replace(/%22/g, '"').replace(/%0A/g, "\n");
        const fn = /filename="([^"]*)"/i.exec(line);
        if (fn) filename = fn[1].replace(/%22/g, '"');
      } else if (/^content-type:/i.test(line)) {
        ctype = line.slice(line.indexOf(":") + 1).trim();
      }
    }
    if (name !== null) {
      if (filename !== null) fd.append(name, new File([body.slice()], filename, { type: ctype }));
      else fd.append(name, dec.decode(body));
    }
    pos = next;
  }
  return fd;
}

globalThis.Blob = Blob;
globalThis.File = File;
globalThis.FormData = FormData;
