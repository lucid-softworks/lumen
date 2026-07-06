// The WebAssembly JS API over the native __wasm ops (decoder + interpreter in Rust). Modules and
// instances live Rust-side by integer id; these classes are thin handles.
//
// Limitation: `Memory.buffer` for *exported* memory is a fresh snapshot each access (lumen can't
// share an ArrayBuffer's storage with the Rust interpreter). Reading results after a call works;
// writing into wasm memory from JS via `memory.buffer` does not — modules that need host-written
// input should export a function that takes the data, or allocate inside wasm. Imported memory/
// table are not yet supported (define+export them instead).

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

class Memory {
  constructor(descriptor) {
    if (descriptor && descriptor.__instanceId !== undefined) {
      this._instId = descriptor.__instanceId; // exported memory (snapshot-backed)
    } else {
      const initial = (descriptor && descriptor.initial) || 0;
      this._maximum = descriptor && descriptor.maximum;
      this._standalone = new ArrayBuffer(initial * 65536);
    }
  }
  get buffer() {
    if (this._standalone) return this._standalone;
    return __wasm.memBytes(this._instId).buffer;
  }
  grow(delta) {
    if (this._standalone) {
      const oldPages = this._standalone.byteLength / 65536;
      const next = new ArrayBuffer((oldPages + delta) * 65536);
      new Uint8Array(next).set(new Uint8Array(this._standalone));
      this._standalone = next;
      return oldPages;
    }
    throw new RuntimeError("grow() on exported memory is not supported from JS");
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

function buildExports(instId, exportsMeta) {
  const exports = {};
  for (const e of exportsMeta) {
    if (e.kind === "function") {
      const index = e.index;
      exports[e.name] = (...args) => {
        let r;
        try {
          r = __wasm.call(instId, index, args);
        } catch (err) {
          throw wrapWasmError(err);
        }
        return r.length === 0 ? undefined : r.length === 1 ? r[0] : r;
      };
    } else if (e.kind === "memory") {
      exports[e.name] = new Memory({ __instanceId: instId });
    } else if (e.kind === "global") {
      const g = new Global({ mutable: false }, __wasm.globalGet(instId, e.index));
      exports[e.name] = g;
    } else if (e.kind === "table") {
      exports[e.name] = new Table({ initial: 0 });
    }
  }
  return exports;
}

class Instance {
  constructor(module, importObject) {
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Instance expects a Module");
    let res;
    try {
      res = __wasm.instantiate(module._id, importObject || {});
    } catch (e) {
      throw wrapWasmError(e);
    }
    this._id = res.id;
    this.exports = buildExports(res.id, res.exports);
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
