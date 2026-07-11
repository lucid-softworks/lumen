// bun:ffi — Bun's foreign-function-interface surface.
//
// Backing reality: lumen has a real dynamic linker (dlopen/dlsym, see dylib.rs) but it is wired
// only for N-API `.node` addons, which are called through a *fixed* C signature
// (`napi_register_module`). A generic FFI — invoking an arbitrary symbol with a caller-described
// signature (int/double/pointer args, native return) — requires a libffi-style trampoline to
// marshal registers per calling convention at runtime. lumen ships no libffi and no such hostcall,
// so there is no honest way to call an arbitrary native function. dlopen/linkSymbols/ptr/read/… all
// throw at call time rather than fake a result. What *is* real and portable is exported for real:
// the FFIType enum table (numeric values copied verbatim from Bun) and the platform library
// `suffix`, both of which tools read for feature-detection and path construction.

{
  const unsupported = (what) => () => {
    throw new Error(
      `bun:ffi ${what} is not supported in lumen (generic native calls need a libffi trampoline)`,
    );
  };

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

  // Real: the platform's shared-library extension, used to build dlopen paths. Read lazily —
  // `process.platform` is populated by the runtime *after* this glue evaluates.
  const suffixFor = () => {
    const platform = (globalThis.process && globalThis.process.platform) || "linux";
    return platform === "darwin" ? "dylib" : platform === "win32" ? "dll" : "so";
  };

  // CString extends String in Bun and decodes a C string starting at a native pointer. Without a
  // real pointer story the only honest thing is to refuse at construction.
  class CString extends String {
    constructor() {
      super("");
      throw new Error(
        "bun:ffi CString is not supported in lumen (reading from a native pointer needs FFI)",
      );
    }
    get arrayBuffer() {
      throw new Error("bun:ffi CString is not supported in lumen");
    }
  }

  class CFunction {
    constructor() {
      throw new Error(
        "bun:ffi CFunction is not supported in lumen (calling a native function needs FFI)",
      );
    }
  }

  class JSCallback {
    constructor() {
      throw new Error(
        "bun:ffi JSCallback is not supported in lumen (exposing JS to native needs FFI)",
      );
    }
  }

  // read.* — DataView-style reads *from a native address*. Every one needs a real pointer.
  const readThrow = (t) => () => {
    throw new Error(`bun:ffi read.${t} is not supported in lumen (needs a native pointer)`);
  };
  const read = {
    u8: readThrow("u8"),
    i8: readThrow("i8"),
    u16: readThrow("u16"),
    i16: readThrow("i16"),
    u32: readThrow("u32"),
    i32: readThrow("i32"),
    u64: readThrow("u64"),
    i64: readThrow("i64"),
    f32: readThrow("f32"),
    f64: readThrow("f64"),
    ptr: readThrow("ptr"),
    intptr: readThrow("intptr"),
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
    dlopen: unsupported("dlopen"),
    linkSymbols: unsupported("linkSymbols"),
    ptr: unsupported("ptr"),
    toArrayBuffer: unsupported("toArrayBuffer"),
    toBuffer: unsupported("toBuffer"),
    cc: unsupported("cc"),
    viewSource: unsupported("viewSource"),
    native: {
      dlopen: unsupported("native.dlopen"),
      callback: unsupported("native.callback"),
    },
  });
}
