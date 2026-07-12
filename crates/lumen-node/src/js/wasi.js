// node:wasi preview1 imports over lumen's WebAssembly memory and process streams.
{
  const encoder = new TextEncoder();
  const ERRNO_SUCCESS = 0, ERRNO_BADF = 8, ERRNO_FAULT = 21, ERRNO_NOSYS = 52;
  const SUPPORTED_VERSIONS = new Set(["unstable", "preview1"]);

  class WasiExit extends Error {
    constructor(code) { super(`WASI exited with code ${code}`); this.code = code >>> 0; }
  }

  class WASI {
    constructor(options = {}) {
      if (options === null || typeof options !== "object") throw argumentError("options", "object", options);
      const { version, args, env, preopens, returnOnExit } = options;
      if (typeof version !== "string") throw argumentError("options.version", "string", version);
      if (!SUPPORTED_VERSIONS.has(version)) {
        const error = new TypeError(`The property 'options.version' unsupported WASI version. Received '${version}'`);
        error.code = "ERR_INVALID_ARG_VALUE";
        throw error;
      }
      if (args !== undefined && !Array.isArray(args)) throw argumentError("options.args", "Array", args);
      if (env !== undefined && (env === null || typeof env !== "object")) throw argumentError("options.env", "object", env);
      if (preopens !== undefined && (preopens === null || typeof preopens !== "object")) throw argumentError("options.preopens", "object", preopens);
      if (returnOnExit !== undefined && typeof returnOnExit !== "boolean") throw argumentError("options.returnOnExit", "boolean", returnOnExit);

      this.version = version;
      this.args = (args || []).map(String);
      this.env = Object.entries(env || {}).map(([key, value]) => `${key}=${value}`);
      this.preopens = { ...(preopens || {}) };
      this.returnOnExit = returnOnExit !== false;
      this.memory = null;
      const imports = this._imports();
      this.wasiImport = new Proxy(imports, {
        get(target, name) { return name in target ? target[name] : () => ERRNO_NOSYS; }
      });
    }

    _imports() {
      return {
        args_sizes_get: (count, size) => this._sizes(this.args, count, size),
        args_get: (pointers, data) => this._strings(this.args, pointers, data),
        environ_sizes_get: (count, size) => this._sizes(this.env, count, size),
        environ_get: (pointers, data) => this._strings(this.env, pointers, data),
        clock_res_get: (_id, output) => this._writeU64(output, 1000000),
        clock_time_get: (id, _precision, output) => {
          const nanos = id === 1 ? Math.floor(performance.now() * 1000000) : Date.now() * 1000000;
          return this._writeU64(output, nanos);
        },
        random_get: (pointer, length) => this._withMemory(() => {
          const target = new Uint8Array(this.memory.buffer, pointer >>> 0, length >>> 0);
          globalThis.crypto.getRandomValues(target);
          return ERRNO_SUCCESS;
        }),
        fd_write: (fd, iovecs, count, written) => this._fdWrite(fd, iovecs, count, written),
        fd_close: fd => fd <= 2 ? ERRNO_SUCCESS : ERRNO_BADF,
        fd_fdstat_get: (fd, output) => this._fdStat(fd, output),
        sched_yield: () => ERRNO_SUCCESS,
        proc_exit: code => { throw new WasiExit(code); },
      };
    }

    _view() { return new DataView(this.memory.buffer); }
    _withMemory(callback) {
      if (!this.memory) return ERRNO_FAULT;
      try {
        const result = callback();
        if (this.memory._syncToStore) this.memory._syncToStore();
        return result;
      } catch (_) { return ERRNO_FAULT; }
    }
    _writeU32(pointer, value) { this._view().setUint32(pointer >>> 0, value >>> 0, true); }
    _writeU64(pointer, value) { return this._withMemory(() => {
      const low = value >>> 0, high = Math.floor(value / 0x100000000) >>> 0;
      this._view().setUint32(pointer >>> 0, low, true);
      this._view().setUint32((pointer >>> 0) + 4, high, true);
      return ERRNO_SUCCESS;
    }); }
    _sizes(values, countPointer, sizePointer) { return this._withMemory(() => {
      this._writeU32(countPointer, values.length);
      this._writeU32(sizePointer, values.reduce((total, value) => total + encoder.encode(value).length + 1, 0));
      return ERRNO_SUCCESS;
    }); }
    _strings(values, pointers, data) { return this._withMemory(() => {
      const memory = new Uint8Array(this.memory.buffer);
      let offset = data >>> 0;
      for (let index = 0; index < values.length; index++) {
        const bytes = encoder.encode(values[index]);
        this._writeU32((pointers >>> 0) + index * 4, offset);
        memory.set(bytes, offset);
        offset += bytes.length;
        memory[offset++] = 0;
      }
      return ERRNO_SUCCESS;
    }); }
    _fdWrite(fd, iovecs, count, written) { return this._withMemory(() => {
      if (fd !== 1 && fd !== 2) return ERRNO_BADF;
      const memory = new Uint8Array(this.memory.buffer), chunks = [];
      let length = 0;
      for (let index = 0; index < (count >>> 0); index++) {
        const offset = (iovecs >>> 0) + index * 8;
        const pointer = this._view().getUint32(offset, true);
        const size = this._view().getUint32(offset + 4, true);
        chunks.push(Buffer.from(memory.slice(pointer, pointer + size)));
        length += size;
      }
      (fd === 1 ? process.stdout : process.stderr).write(Buffer.concat(chunks));
      this._writeU32(written, length);
      return ERRNO_SUCCESS;
    }); }
    _fdStat(fd, output) { return this._withMemory(() => {
      if (fd > 2) return ERRNO_BADF;
      new Uint8Array(this.memory.buffer, output >>> 0, 24).fill(0);
      new DataView(this.memory.buffer).setUint8(output >>> 0, 2);
      return ERRNO_SUCCESS;
    }); }

    _bind(instance) {
      if (!instance || !instance.exports || !instance.exports.memory) throw argumentError("instance.exports.memory", "WebAssembly.Memory", undefined);
      this.memory = instance.exports.memory;
    }
    start(instance) {
      this._bind(instance);
      if (typeof instance.exports._start !== "function") throw new TypeError("WASI.start requires an exported _start function");
      try { instance.exports._start(); }
      catch (error) { if (error instanceof WasiExit && this.returnOnExit) return error.code; throw error; }
      return 0;
    }
    initialize(instance) {
      this._bind(instance);
      if (typeof instance.exports._start === "function") throw new TypeError("WASI.initialize cannot be used with a _start export");
      if (typeof instance.exports._initialize === "function") instance.exports._initialize();
    }
    getImportObject() {
      return { [this.version === "unstable" ? "wasi_unstable" : "wasi_snapshot_preview1"]: this.wasiImport };
    }
  }

  function argumentError(name, expected, value) {
    const error = new TypeError(`The "${name}" argument must be of type ${expected}. Received ${value === null ? "null" : typeof value}`);
    error.code = "ERR_INVALID_ARG_TYPE";
    return error;
  }

  __builtins.set("wasi", { WASI });
}
