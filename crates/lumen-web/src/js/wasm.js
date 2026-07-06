// The WebAssembly JS API over the native __wasm ops (decoder + interpreter in Rust). Modules and
// instances live Rust-side by integer id; these classes are thin handles.
//
// Memory is kept coherent by syncing the JS `Memory.buffer` with the interpreter's Rust-side bytes
// at call boundaries (JS writes pushed in before a call, wasm writes pulled out after) — see the
// Memory class. This stands in for a live shared ArrayBuffer (which lumen's embed API can't
// provide) and is correct as long as wasm memory only changes during calls. Imported memory and
// globals are supported; imported *tables* are not (their funcrefs would reference another
// instance's functions, which this single-instance model doesn't represent) — define and export
// the table instead. No SIMD/threads/GC.

class CompileError extends Error {
  constructor(m) {
    super(m);
    this.name = "CompileError";
  }
}
class LinkError extends Error {
  constructor(m) {
    super(m);
    this.name = "LinkError";
  }
}
class RuntimeError extends Error {
  constructor(m) {
    super(m);
    this.name = "RuntimeError";
  }
}

// Map a prefixed native error to the matching WebAssembly error type.
function wrapWasmError(e) {
  const msg = (e && e.message) || String(e);
  if (msg.startsWith("CompileError:")) return new CompileError(msg.slice(13).trim());
  if (msg.startsWith("LinkError:")) return new LinkError(msg.slice(10).trim());
  if (msg.startsWith("RuntimeError:")) return new RuntimeError(msg.slice(13).trim());
  return e;
}

function toBytes(source) {
  if (source instanceof Uint8Array) return source;
  if (source instanceof ArrayBuffer) return new Uint8Array(source);
  if (ArrayBuffer.isView(source)) return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  throw new TypeError("WebAssembly: expected a BufferSource");
}

class Module {
  constructor(bytes) {
    try {
      this._id = __wasm.compile(toBytes(bytes));
    } catch (e) {
      throw wrapWasmError(e);
    }
  }
  static exports(module) {
    return __wasm.moduleExports(module._id);
  }
  static imports(module) {
    return __wasm.moduleImports(module._id);
  }
  static customSections() {
    return []; // custom sections are skipped by the decoder
  }
}

// A linear memory. `.buffer` is a persistent ArrayBuffer; when the memory is bound to an instance
// it is synced with the interpreter's Rust-side bytes at call boundaries (see buildExports) — JS
// writes are pushed in before a call and wasm's writes pulled back out after. (lumen can't share an
// ArrayBuffer's backing store with Rust, so this boundary sync stands in for a live shared buffer;
// between calls, wasm memory doesn't change, so JS sees a consistent view.)
class Memory {
  constructor(descriptor) {
    this._instId = descriptor && descriptor.__instanceId !== undefined ? descriptor.__instanceId : null;
    this._maximum = descriptor && descriptor.maximum;
    if (this._instId !== null) {
      this._buf = null;
      this._view = null;
      this._syncFromRust();
    } else {
      const initial = (descriptor && descriptor.initial) || 0;
      this._buf = new ArrayBuffer(initial * 65536);
      this._view = new Uint8Array(this._buf);
    }
  }
  get buffer() {
    return this._buf;
  }
  // Bind a standalone memory (created via `new WebAssembly.Memory`) to the instance importing it.
  _bindInstance(instId) {
    this._instId = instId;
    this._syncFromRust();
  }
  _syncToRust() {
    if (this._instId !== null && this._view) __wasm.memWrite(this._instId, 0, this._view);
  }
  _syncFromRust() {
    if (this._instId === null) return;
    const bytes = __wasm.memBytes(this._instId);
    if (!this._buf || this._buf.byteLength !== bytes.byteLength) {
      this._buf = bytes.buffer; // size changed (grow detaches the old buffer, per spec)
      this._view = new Uint8Array(this._buf);
    } else {
      this._view.set(bytes); // same size: copy in place, preserving buffer identity
    }
  }
  grow(delta) {
    if (this._instId !== null) {
      const prev = __wasm.memGrow(this._instId, delta);
      if (prev < 0) throw new RangeError("WebAssembly.Memory.grow() failed");
      this._syncFromRust();
      return prev;
    }
    const oldPages = this._buf.byteLength / 65536;
    const next = new ArrayBuffer((oldPages + delta) * 65536);
    new Uint8Array(next).set(this._view);
    this._buf = next;
    this._view = new Uint8Array(next);
    return oldPages;
  }
}

class Global {
  constructor(descriptor, value) {
    this._type = descriptor && descriptor.value;
    this._mutable = !!(descriptor && descriptor.mutable);
    this._value = value !== undefined ? value : 0;
  }
  get value() {
    return this._value;
  }
  set value(v) {
    if (!this._mutable) throw new TypeError("cannot set the value of an immutable global");
    this._value = v;
  }
  valueOf() {
    return this._value;
  }
}

class Table {
  constructor(descriptor, init = null) {
    this._elements = new Array((descriptor && descriptor.initial) || 0).fill(init);
    this._maximum = descriptor && descriptor.maximum;
  }
  get length() {
    return this._elements.length;
  }
  get(i) {
    return this._elements[i];
  }
  set(i, v) {
    this._elements[i] = v;
  }
  grow(delta, init = null) {
    const old = this._elements.length;
    for (let k = 0; k < delta; k++) this._elements.push(init);
    return old;
  }
}

function buildExports(instId, exportsMeta, importedMemory) {
  const exports = {};
  // The instance's memory object (imported, or created here for an exported memory). Function
  // wrappers sync it around each call so JS and wasm see each other's writes.
  let memory = importedMemory || null;
  for (const e of exportsMeta) {
    if (e.kind === "memory") {
      if (!memory) memory = new Memory({ __instanceId: instId });
      exports[e.name] = memory;
    }
  }
  for (const e of exportsMeta) {
    if (e.kind === "function") {
      const index = e.index;
      exports[e.name] = (...args) => {
        if (memory) memory._syncToRust();
        let r;
        try {
          r = __wasm.call(instId, index, args);
        } catch (err) {
          throw wrapWasmError(err);
        }
        if (memory) memory._syncFromRust();
        return r.length === 0 ? undefined : r.length === 1 ? r[0] : r;
      };
    } else if (e.kind === "global") {
      exports[e.name] = new Global({ mutable: false }, __wasm.globalGet(instId, e.index));
    } else if (e.kind === "table") {
      exports[e.name] = new Table({ initial: 0 });
    }
  }
  return exports;
}

// Resolve the JS import object into the flat, module-order array the native op consumes.
function resolveImports(module, importObject) {
  const io = importObject || {};
  let memory = null;
  const resolved = Module.imports(module).map((imp) => {
    const ns = io[imp.module] || {};
    const v = ns[imp.name];
    if (imp.kind === "function") return { fn: v };
    if (imp.kind === "memory") {
      memory = v;
      return { bytes: v && v._view ? v._view : new Uint8Array(0) };
    }
    if (imp.kind === "global") return { value: v instanceof Global ? v.value : v };
    return {}; // table (rejected by the op) / unknown
  });
  return { resolved, memory };
}

class Instance {
  constructor(module, importObject) {
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Instance expects a Module");
    const { resolved, memory } = resolveImports(module, importObject);
    let res;
    try {
      res = __wasm.instantiate(module._id, resolved);
    } catch (e) {
      throw wrapWasmError(e);
    }
    this._id = res.id;
    if (memory) memory._bindInstance(res.id);
    this.exports = buildExports(res.id, res.exports, memory);
  }
}

function validate(bytes) {
  try {
    return __wasm.validate(toBytes(bytes));
  } catch {
    return false;
  }
}

async function compile(bytes) {
  return new Module(bytes);
}

async function instantiate(source, importObject) {
  if (source instanceof Module) {
    return new Instance(source, importObject);
  }
  const module = new Module(source);
  const instance = new Instance(module, importObject);
  return { module, instance };
}

globalThis.WebAssembly = {
  validate,
  compile,
  instantiate,
  Module,
  Instance,
  Memory,
  Table,
  Global,
  CompileError,
  LinkError,
  RuntimeError,
};
