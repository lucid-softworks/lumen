// bun:ffi — Bun's foreign-function-interface surface.
//
// Backing reality: lumen has a real dynamic linker (dlopen/dlsym, see dylib.rs) and a libffi-free
// trampoline (ffi.rs) that calls arbitrary symbols by monomorphizing an `extern "C"` fn-pointer per
// argument register-class signature. This glue is a thin marshalling layer over the `__ffi` ops:
// it resolves symbols, normalizes the FFIType table, and shapes CString/JSCallback/read.* the way
// Bun v1.2.21 does. Signatures the trampoline can't lower (>8 args, struct-by-value, varargs, and
// float-typed JSCallbacks) throw honestly rather than fake a result.

{
  // Bun's FFIType — numeric values copied exactly (verified against bun v1.2.21). Both the named
  // aliases and the self-referential numeric keys are present, matching Bun's object shape.
  const FFIType = {
    char: 0,
    int8_t: 1,
    i8: 1,
    uint8_t: 2,
    u8: 2,
    int16_t: 3,
    i16: 3,
    uint16_t: 4,
    u16: 4,
    int32_t: 5,
    i32: 5,
    int: 5,
    c_int: 5,
    uint32_t: 6,
    u32: 6,
    c_uint: 6,
    int64_t: 7,
    i64: 7,
    isize: 7,
    uint64_t: 8,
    u64: 8,
    usize: 8,
    double: 9,
    f64: 9,
    float: 10,
    f32: 10,
    bool: 11,
    "void*": 12,
    ptr: 12,
    pointer: 12,
    "char*": 12,
    void: 13,
    cstring: 14,
    i64_fast: 15,
    u64_fast: 16,
    function: 17,
    callback: 17,
    fn: 17,
    napi_env: 18,
    napi_value: 19,
    buffer: 20,
  };
  // Self-referential numeric keys (0..17), exactly as Bun exposes them.
  for (let i = 0; i <= 17; i++) FFIType[i] = i;

  const CSTRING = 14;
  const VOID = 13;

  // Resolve a type spec (a numeric FFIType, an alias number, or a string like "i32") to its code.
  const normalizeType = (t) => {
    if (typeof t === "number") return t;
    if (typeof t === "string" && t in FFIType) return FFIType[t];
    if (t && typeof t === "object" && typeof t.tag === "string" && t.tag in FFIType) {
      return FFIType[t.tag];
    }
    throw new TypeError(`bun:ffi: unknown FFIType ${String(t)}`);
  };

  // The platform's shared-library extension, used to build dlopen paths. Read lazily —
  // `process.platform` is populated by the runtime *after* this glue evaluates.
  const suffixFor = () => {
    const platform = (globalThis.process && globalThis.process.platform) || "linux";
    return platform === "darwin" ? "dylib" : platform === "win32" ? "dll" : "so";
  };

  // CString extends String and decodes a NUL-terminated (or fixed-length) C string at a native
  // pointer. `byteLength` is left undefined when no explicit length is given (matching Bun).
  class CString extends String {
    constructor(ptr, byteOffset, byteLength) {
      const off = byteOffset || 0;
      const len = byteLength === undefined ? -1 : byteLength;
      super(ptr ? __ffi.readCString(ptr, off, len) : "");
      this.ptr = ptr;
      this.byteOffset = off;
      this.byteLength = byteLength;
    }
    get arrayBuffer() {
      if (this.byteLength === undefined) {
        throw new TypeError("bun:ffi CString.arrayBuffer needs an explicit byteLength");
      }
      return __ffi.toArrayBuffer(this.ptr, this.byteOffset, this.byteLength);
    }
  }

  // Wrap a resolved function pointer as a JS callable: marshal args, trampoline, marshal the
  // return. A cstring return is rewrapped in a CString.
  const makeCaller = (fnPtr, retCode, argCodes) => {
    const isCString = retCode === CSTRING;
    return (...args) => {
      const r = __ffi.call(fnPtr, retCode, argCodes, args);
      return isCString ? new CString(r) : r;
    };
  };

  const defToCodes = (def) => ({
    argCodes: (def.args || []).map(normalizeType),
    retCode: normalizeType(def.returns === undefined ? VOID : def.returns),
  });

  // CFunction — build a callable straight from a raw function pointer.
  function CFunction(def) {
    const { argCodes, retCode } = defToCodes(def);
    return makeCaller(def.ptr, retCode, argCodes);
  }

  // JSCallback — expose a JS function to native code via a pooled `extern "C"` thunk. Only
  // integer/pointer register-class signatures are supported (the qsort-comparator family); a
  // float-typed callback throws honestly.
  class JSCallback {
    #id;
    constructor(fn, def) {
      const d = def || {};
      const argCodes = (d.args || []).map(normalizeType);
      const retCode = normalizeType(d.returns === undefined ? VOID : d.returns);
      const [ptr, id] = __ffi.registerCallback(fn, argCodes, retCode);
      this.ptr = ptr;
      this.#id = id;
    }
    close() {
      if (this.#id !== undefined) {
        __ffi.unregisterCallback(this.#id);
        this.#id = undefined;
        this.ptr = null;
      }
    }
  }

  // read.* — typed reads from a native address (little-endian on both supported targets).
  const readKinds = {
    u8: 0,
    i8: 1,
    u16: 2,
    i16: 3,
    u32: 4,
    i32: 5,
    u64: 6,
    i64: 7,
    f32: 8,
    f64: 9,
    ptr: 10,
    intptr: 11,
  };
  const read = {};
  for (const name of Object.keys(readKinds)) {
    const kind = readKinds[name];
    read[name] = (p, offset) => __ffi.read(p, offset || 0, kind);
  }

  const ptr = (value, byteOffset) => __ffi.ptr(value, byteOffset || 0);

  const toArrayBuffer = (p, byteOffset, byteLength) =>
    __ffi.toArrayBuffer(p, byteOffset || 0, byteLength || 0);

  const toBuffer = (p, byteOffset, byteLength) => {
    const u8 = __ffi.toBuffer(p, byteOffset || 0, byteLength || 0);
    return globalThis.Buffer ? globalThis.Buffer.from(u8.buffer) : u8;
  };

  // dlopen — open a library and bind each declared symbol to a caller.
  const dlopen = (path, symbols) => {
    const libId = __ffi.dlopen(String(path));
    const out = {};
    for (const name of Object.keys(symbols || {})) {
      const def = symbols[name] || {};
      const { argCodes, retCode } = defToCodes(def);
      const fnPtr = __ffi.dlsym(libId, def.name || name);
      out[name] = makeCaller(fnPtr, retCode, argCodes);
    }
    return {
      symbols: out,
      close: () => __ffi.dlclose(libId),
    };
  };

  // linkSymbols — like dlopen, but each symbol already carries its resolved `ptr`.
  const linkSymbols = (symbols) => {
    const out = {};
    for (const name of Object.keys(symbols || {})) {
      const def = symbols[name] || {};
      const { argCodes, retCode } = defToCodes(def);
      out[name] = makeCaller(def.ptr, retCode, argCodes);
    }
    return { symbols: out, close: () => {} };
  };

  // Runtime C compilation through the host C compiler. Bun embeds TinyCC; lumen deliberately
  // uses the installed toolchain but preserves the synchronous API and the same symbol binder.
  const cc = (options) => {
    if (!options || typeof options !== "object") throw new TypeError("bun:ffi cc expects an options object");
    let source = options.source;
    if (source instanceof URL) source = source.protocol === "file:" ? source.pathname : String(source);
    else if (source && typeof source === "object" && typeof source.name === "string") source = source.name;
    if (typeof source !== "string") throw new TypeError("bun:ffi cc options.source must be a file path");
    const args = [];
    const flags = options.flags === undefined ? [] : Array.isArray(options.flags) ? options.flags : [options.flags];
    for (const flag of flags) args.push(String(flag));
    for (const name of Object.keys(options.define || {})) {
      const value = options.define[name];
      args.push(value === undefined || value === "" ? `-D${name}` : `-D${name}=${value}`);
    }
    for (const library of options.library || []) args.push(`-l${library}`);
    const path = __ffi.cc(source, args.join("\0"));
    const library = dlopen(path, options.symbols || {});
    const close = library.close;
    library.close = () => {
      close();
      try { __builtins.get("fs").unlinkSync(path); } catch {}
    };
    return library;
  };

  const unsupported = (what, why) => () => {
    throw new Error(`bun:ffi ${what} is not supported in lumen${why ? ` (${why})` : ""}`);
  };

  __builtins.set("bun:ffi", {
    FFIType,
    get suffix() {
      return suffixFor();
    },
    CString,
    CFunction,
    JSCallback,
    read,
    ptr,
    toArrayBuffer,
    toBuffer,
    dlopen,
    linkSymbols,
    cc,
    viewSource: unsupported("viewSource", "no JIT source to disassemble"),
    native: {
      // Bun's internal fast-path hooks; the public dlopen/JSCallback above are the supported entry.
      dlopen: unsupported("native.dlopen"),
      callback: unsupported("native.callback"),
    },
  });
}
