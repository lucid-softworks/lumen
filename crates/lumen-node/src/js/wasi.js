// node:wasi preview1 imports over lumen's WebAssembly memory and process streams.
{
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  const fs = __builtins.get("fs"), path = __builtins.get("path");
  const ERRNO_SUCCESS = 0, ERRNO_ACCES = 2, ERRNO_BADF = 8, ERRNO_EXIST = 20;
  const ERRNO_FAULT = 21, ERRNO_INVAL = 28, ERRNO_IO = 29, ERRNO_ISDIR = 31;
  const ERRNO_NOENT = 44, ERRNO_NOSYS = 52, ERRNO_NOTDIR = 54, ERRNO_NOTCAPABLE = 76;
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
      this.fds = new Map();
      let preopenFd = 3;
      for (const [guest, host] of Object.entries(this.preopens)) {
        const root = fs.realpathSync(path.resolve(String(host)));
        this.fds.set(preopenFd++, { preopen: true, guest: String(guest), root });
      }
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
        fd_read: (fd, iovecs, count, read) => this._fdRead(fd, iovecs, count, read),
        fd_seek: (fd, offset, whence, output) => this._fdSeek(fd, offset, whence, output),
        fd_tell: (fd, output) => this._fdSeek(fd, 0, 1, output),
        fd_close: fd => this._fdClose(fd),
        fd_fdstat_get: (fd, output) => this._fdStat(fd, output),
        fd_filestat_get: (fd, output) => this._fileStat(fd, output),
        fd_prestat_get: (fd, output) => this._prestat(fd, output),
        fd_prestat_dir_name: (fd, output, length) => this._prestatName(fd, output, length),
        path_open: (fd, _dirflags, name, nameLength, oflags, _rights, _inherited, fdflags, output) =>
          this._pathOpen(fd, name, nameLength, oflags, fdflags, output),
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
      const memory = new Uint8Array(this.memory.buffer), chunks = [];
      let length = 0;
      for (let index = 0; index < (count >>> 0); index++) {
        const offset = (iovecs >>> 0) + index * 8;
        const pointer = this._view().getUint32(offset, true);
        const size = this._view().getUint32(offset + 4, true);
        chunks.push(Buffer.from(memory.slice(pointer, pointer + size)));
        length += size;
      }
      const data = Buffer.concat(chunks);
      if (fd === 1 || fd === 2) (fd === 1 ? process.stdout : process.stderr).write(data);
      else {
        const entry = this.fds.get(fd);
        if (!entry || entry.preopen) return ERRNO_BADF;
        length = fs.writeSync(entry.fd, data, 0, data.length, entry.append ? null : entry.position);
        if (!entry.append) entry.position += length;
      }
      this._writeU32(written, length);
      return ERRNO_SUCCESS;
    }); }
    _fdStat(fd, output) { return this._withMemory(() => {
      const entry = this.fds.get(fd);
      if (fd > 2 && !entry) return ERRNO_BADF;
      new Uint8Array(this.memory.buffer, output >>> 0, 24).fill(0);
      new DataView(this.memory.buffer).setUint8(output >>> 0, entry && entry.preopen ? 3 : 2);
      return ERRNO_SUCCESS;
    }); }

    _fdRead(fd, iovecs, count, output) { return this._withMemory(() => {
      const entry = this.fds.get(fd);
      if (!entry || entry.preopen) return ERRNO_BADF;
      const memory = new Uint8Array(this.memory.buffer);
      let total = 0;
      for (let index = 0; index < (count >>> 0); index++) {
        const offset = (iovecs >>> 0) + index * 8;
        const pointer = this._view().getUint32(offset, true), length = this._view().getUint32(offset + 4, true);
        const target = Buffer.alloc(length);
        const read = fs.readSync(entry.fd, target, 0, length, entry.position);
        memory.set(target.subarray(0, read), pointer);
        entry.position += read; total += read;
        if (read < length) break;
      }
      this._writeU32(output, total);
      return ERRNO_SUCCESS;
    }); }

    _fdSeek(fd, rawOffset, whence, output) { return this._withMemory(() => {
      const entry = this.fds.get(fd);
      if (!entry || entry.preopen) return ERRNO_BADF;
      const offset = typeof rawOffset === "bigint" ? Number(rawOffset) : Number(rawOffset);
      if (!Number.isSafeInteger(offset) || whence < 0 || whence > 2) return ERRNO_INVAL;
      const base = whence === 0 ? 0 : whence === 1 ? entry.position : fs.fstatSync(entry.fd).size;
      if (base + offset < 0) return ERRNO_INVAL;
      entry.position = base + offset;
      return this._writeU64(output, entry.position);
    }); }

    _fdClose(fd) {
      const entry = this.fds.get(fd);
      if (!entry || entry.preopen) return fd <= 2 ? ERRNO_SUCCESS : ERRNO_BADF;
      try { fs.closeSync(entry.fd); this.fds.delete(fd); return ERRNO_SUCCESS; }
      catch (error) { return wasiError(error); }
    }

    _prestat(fd, output) { return this._withMemory(() => {
      const entry = this.fds.get(fd);
      if (!entry || !entry.preopen) return ERRNO_BADF;
      new Uint8Array(this.memory.buffer, output >>> 0, 8).fill(0);
      this._writeU32((output >>> 0) + 4, encoder.encode(entry.guest).length);
      return ERRNO_SUCCESS;
    }); }

    _prestatName(fd, output, length) { return this._withMemory(() => {
      const entry = this.fds.get(fd);
      if (!entry || !entry.preopen) return ERRNO_BADF;
      const name = encoder.encode(entry.guest);
      if (length < name.length) return ERRNO_FAULT;
      new Uint8Array(this.memory.buffer).set(name, output >>> 0);
      return ERRNO_SUCCESS;
    }); }

    _pathOpen(fd, namePointer, nameLength, oflags, fdflags, output) { return this._withMemory(() => {
      const directory = this.fds.get(fd);
      if (!directory || !directory.preopen) return ERRNO_BADF;
      const name = decoder.decode(new Uint8Array(this.memory.buffer, namePointer >>> 0, nameLength >>> 0));
      const resolved = path.resolve(directory.root, name);
      if (resolved !== directory.root && !resolved.startsWith(directory.root + path.sep)) return ERRNO_NOTCAPABLE;
      let canonical;
      try { canonical = fs.realpathSync(resolved); }
      catch (error) {
        if (!(oflags & 1)) return wasiError(error);
        try { canonical = path.join(fs.realpathSync(path.dirname(resolved)), path.basename(resolved)); }
        catch (parentError) { return wasiError(parentError); }
      }
      if (canonical !== directory.root && !canonical.startsWith(directory.root + path.sep)) return ERRNO_NOTCAPABLE;
      if (oflags & 2) {
        try { if (!fs.statSync(canonical).isDirectory()) return ERRNO_NOTDIR; }
        catch (error) { return wasiError(error); }
      }
      let mode = "r+";
      if (fdflags & 1) mode = "a+";
      else if (oflags & 9) mode = "w+";
      let native;
      try { native = fs.openSync(canonical, mode); }
      catch (error) {
        if (mode === "r+") try { native = fs.openSync(canonical, "r"); } catch (nested) { return wasiError(nested); }
        else return wasiError(error);
      }
      let guestFd = 3;
      while (this.fds.has(guestFd)) guestFd++;
      this.fds.set(guestFd, { fd: native, path: canonical, position: 0, append: !!(fdflags & 1) });
      this._writeU32(output, guestFd);
      return ERRNO_SUCCESS;
    }); }

    _fileStat(fd, output) { return this._withMemory(() => {
      const entry = this.fds.get(fd);
      if (!entry) return ERRNO_BADF;
      let stat;
      try { stat = entry.preopen ? fs.statSync(entry.root) : fs.fstatSync(entry.fd); }
      catch (error) { return wasiError(error); }
      new Uint8Array(this.memory.buffer, output >>> 0, 64).fill(0);
      this._view().setUint8((output >>> 0) + 16, stat.isDirectory() ? 3 : 4);
      this._writeU64((output >>> 0) + 32, stat.size || 0);
      this._writeU64((output >>> 0) + 40, Math.floor(stat.atimeMs * 1000000));
      this._writeU64((output >>> 0) + 48, Math.floor(stat.mtimeMs * 1000000));
      this._writeU64((output >>> 0) + 56, Math.floor(stat.ctimeMs * 1000000));
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

  function wasiError(error) {
    if (!error || !error.code) return ERRNO_IO;
    return ({ EACCES: ERRNO_ACCES, EPERM: ERRNO_ACCES, EBADF: ERRNO_BADF, EEXIST: ERRNO_EXIST,
      EISDIR: ERRNO_ISDIR, ENOENT: ERRNO_NOENT, ENOTDIR: ERRNO_NOTDIR })[error.code] || ERRNO_IO;
  }

  __builtins.set("wasi", { WASI });
}
