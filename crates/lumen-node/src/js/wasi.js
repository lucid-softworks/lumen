// ---- node:wasi --------------------------------------------------------------------------------
// `globalThis.WebAssembly` exists, so a module can be compiled and instantiated — but WASI is not
// WebAssembly: it is a POSIX-ish syscall layer (fd_read/path_open/clock_time_get/...) that must be
// bridged to real OS resources. lumen exposes no such bridge to this glue (there is no wasi crate
// hook reachable from JS), so a WASI instance cannot actually service a guest's imports.
//
// The constructor still performs Node's option validation (so misuse fails the same way early),
// and the instance carries the real shape. The pieces that would run a guest against absent
// syscalls throw honestly instead of trapping deep inside a WebAssembly instance.
{
  const SUPPORTED_VERSIONS = new Set(["unstable", "preview1"]);
  const notImpl = () => {
    throw new Error("node:wasi syscalls are not supported in lumen");
  };

  class WASI {
    constructor(options = {}) {
      if (options === null || typeof options !== "object") {
        const e = new TypeError("The \"options\" argument must be of type object. Received " + (options === null ? "null" : typeof options));
        e.code = "ERR_INVALID_ARG_TYPE";
        throw e;
      }
      const { version, args, env, preopens, returnOnExit } = options;
      if (typeof version !== "string") {
        const e = new TypeError("The \"options.version\" property must be of type string. Received " + (version === undefined ? "undefined" : typeof version));
        e.code = "ERR_INVALID_ARG_TYPE";
        throw e;
      }
      if (!SUPPORTED_VERSIONS.has(version)) {
        const e = new TypeError("The property 'options.version' unsupported WASI version. Received '" + version + "'");
        e.code = "ERR_INVALID_ARG_VALUE";
        throw e;
      }
      if (args !== undefined && !Array.isArray(args)) {
        const e = new TypeError("The \"options.args\" property must be an instance of Array.");
        e.code = "ERR_INVALID_ARG_TYPE";
        throw e;
      }
      if (env !== undefined && (env === null || typeof env !== "object")) {
        const e = new TypeError("The \"options.env\" property must be of type object.");
        e.code = "ERR_INVALID_ARG_TYPE";
        throw e;
      }
      if (preopens !== undefined && (preopens === null || typeof preopens !== "object")) {
        const e = new TypeError("The \"options.preopens\" property must be of type object.");
        e.code = "ERR_INVALID_ARG_TYPE";
        throw e;
      }
      if (returnOnExit !== undefined && typeof returnOnExit !== "boolean") {
        const e = new TypeError("The \"options.returnOnExit\" property must be of type boolean.");
        e.code = "ERR_INVALID_ARG_TYPE";
        throw e;
      }

      this.version = version;
      // The import namespace a guest would receive. Every syscall throws — lumen has no bridge to
      // back them — so a guest cannot silently run against dead imports.
      this.wasiImport = new Proxy({}, { get: () => notImpl });
    }

    start() {
      notImpl();
    }
    initialize() {
      notImpl();
    }
    getImportObject() {
      return { wasi_snapshot_preview1: this.wasiImport };
    }
  }

  __builtins.set("wasi", { WASI });
}
