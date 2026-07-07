// The WebAssembly JS API over the native __wasm ops (decoder + interpreter in Rust). Every wasm
// entity lives in one shared Rust-side Store; these JS handles carry integer *store addresses*, so
// a Memory/Table/Global can be created standalone and imported by any module (cross-module
// linking). Functions are called by their store address.
//
// Memory is kept coherent by syncing Memory.buffer with the store's bytes at call boundaries (JS
// writes pushed in before a call, wasm writes pulled out after) — see the Memory class. lumen's
// embed API can't share an ArrayBuffer's backing store with Rust, so this stands in for a live
// shared buffer; it's correct as long as wasm memory only changes during calls. No SIMD/threads/GC.

class CompileError extends Error {
  constructor(m) { super(m); this.name = "CompileError"; }
}
class LinkError extends Error {
  constructor(m) { super(m); this.name = "LinkError"; }
}
class RuntimeError extends Error {
  constructor(m) { super(m); this.name = "RuntimeError"; }
}

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

// Wrap a store function address as a callable.
function funcFromAddr(faddr) {
  const fn = (...args) => {
    let r;
    try { r = __wasm.call(faddr, args); } catch (err) { throw wrapWasmError(err); }
    return r.length === 0 ? undefined : r.length === 1 ? r[0] : r;
  };
  fn._funcAddr = faddr;
  return fn;
}

class Module {
  constructor(bytes) {
    try { this._id = __wasm.compile(toBytes(bytes)); }
    catch (e) { throw wrapWasmError(e); }
  }
  static exports(module) { return __wasm.moduleExports(module._id); }
  static imports(module) { return __wasm.moduleImports(module._id); }
  static customSections() { return []; }
}

class Memory {
  constructor(descriptor) {
    if (descriptor && descriptor.__addr !== undefined) {
      this._addr = descriptor.__addr; // bound to an existing store memory (an export)
    } else {
      const initial = (descriptor && descriptor.initial) || 0;
      this._addr = __wasm.allocMemory(initial, descriptor && descriptor.maximum);
    }
    this._buf = null;
    this._view = null;
    this._syncFromStore();
  }
  get buffer() { return this._buf; }
  _syncToStore() {
    if (this._view) __wasm.memWrite(this._addr, 0, this._view);
  }
  _syncFromStore() {
    const bytes = __wasm.memBytes(this._addr);
    if (!this._buf || this._buf.byteLength !== bytes.byteLength) {
      this._buf = bytes.buffer; // grow detaches the old buffer, per spec
      this._view = new Uint8Array(this._buf);
    } else {
      this._view.set(bytes); // same size: copy in place, preserving buffer identity
    }
  }
  grow(delta) {
    const prev = __wasm.memGrow(this._addr, delta);
    if (prev < 0) throw new RangeError("WebAssembly.Memory.grow() failed");
    this._syncFromStore();
    return prev;
  }
}

class Table {
  constructor(descriptor) {
    this._addr =
      descriptor && descriptor.__addr !== undefined
        ? descriptor.__addr
        : __wasm.allocTable((descriptor && descriptor.initial) || 0, descriptor && descriptor.maximum);
  }
  get length() { return __wasm.tableSize(this._addr); }
  get(i) {
    const faddr = __wasm.tableGet(this._addr, i);
    return faddr < 0 ? null : funcFromAddr(faddr);
  }
  set(i, value) {
    if (value == null) return __wasm.tableSet(this._addr, i, -1);
    if (typeof value !== "function" || value._funcAddr === undefined) {
      throw new TypeError("Table.set expects an exported wasm function or null");
    }
    __wasm.tableSet(this._addr, i, value._funcAddr);
  }
}

class Global {
  constructor(descriptor, value) {
    this._mutable = !!(descriptor && descriptor.mutable);
    if (descriptor && descriptor.__addr !== undefined) {
      this._addr = descriptor.__addr;
    } else {
      const type = (descriptor && descriptor.value) || "i32";
      this._addr = __wasm.allocGlobal(value !== undefined ? value : 0, this._mutable, type);
    }
  }
  get value() { return __wasm.globalGet(this._addr); }
  set value(v) {
    if (!this._mutable) throw new TypeError("cannot set the value of an immutable global");
    __wasm.globalSet(this._addr, v);
  }
  valueOf() { return this.value; }
}

function buildExports(exportsMeta, importedMemory) {
  const exports = {};
  let memory = importedMemory || null;
  for (const e of exportsMeta) {
    if (e.kind === "memory") {
      if (!memory) memory = new Memory({ __addr: e.addr });
      exports[e.name] = memory;
    }
  }
  for (const e of exportsMeta) {
    if (e.kind === "function") {
      const faddr = e.addr;
      const fn = (...args) => {
        if (memory) memory._syncToStore();
        let r;
        try { r = __wasm.call(faddr, args); } catch (err) { throw wrapWasmError(err); }
        if (memory) memory._syncFromStore();
        return r.length === 0 ? undefined : r.length === 1 ? r[0] : r;
      };
      fn._funcAddr = faddr;
      exports[e.name] = fn;
    } else if (e.kind === "global") {
      exports[e.name] = new Global({ __addr: e.addr, mutable: false });
    } else if (e.kind === "table") {
      exports[e.name] = new Table({ __addr: e.addr });
    }
  }
  return exports;
}

// Resolve the JS import object into the flat, module-order array the native op consumes: {fn} for
// functions, and store addresses for memory/table/global (so imported entities are shared).
function resolveImports(module, importObject) {
  const io = importObject || {};
  let memory = null;
  const resolved = Module.imports(module).map((imp) => {
    const v = (io[imp.module] || {})[imp.name];
    if (imp.kind === "function") return { fn: v };
    if (imp.kind === "memory") {
      memory = v;
      return { memAddr: v ? v._addr : -1 };
    }
    if (imp.kind === "table") return { tableAddr: v && v._addr !== undefined ? v._addr : -1 };
    if (imp.kind === "global") {
      if (v instanceof Global) return { globalAddr: v._addr };
      const g = new Global({ value: Number.isInteger(v) ? "i32" : "f64", mutable: false }, v);
      return { globalAddr: g._addr };
    }
    return {};
  });
  return { resolved, memory };
}

class Instance {
  constructor(module, importObject) {
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Instance expects a Module");
    const { resolved, memory } = resolveImports(module, importObject);
    let res;
    try { res = __wasm.instantiate(module._id, resolved); }
    catch (e) { throw wrapWasmError(e); }
    this._inst = res.inst;
    this.exports = buildExports(res.exports, memory);
  }
}

function validate(bytes) {
  try { return __wasm.validate(toBytes(bytes)); } catch { return false; }
}

async function compile(bytes) {
  return new Module(bytes);
}

async function instantiate(source, importObject) {
  if (source instanceof Module) return new Instance(source, importObject);
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
