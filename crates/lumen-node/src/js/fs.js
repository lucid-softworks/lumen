// node:fs — Node's shapes (sync, callback, and promises) over the runtime's `fs` global
// (lumen-fs) plus the `__node` filesystem ops (real stat metadata, rm, rename, copy, …).
// readFile returns a string with an encoding argument, else a Buffer.

// Node's `Stats`. Built from the raw fields `__node.stat`/`lstat`/`fs.fstatSync` return.
class Stats {
  constructor(raw) {
    this.dev = raw.dev; this.ino = raw.ino; this.mode = raw.mode; this.nlink = raw.nlink;
    this.uid = raw.uid; this.gid = raw.gid; this.rdev = raw.rdev; this.size = raw.size;
    this.blksize = raw.blksize; this.blocks = raw.blocks;
    this.atimeMs = raw.atimeMs; this.mtimeMs = raw.mtimeMs;
    this.ctimeMs = raw.ctimeMs; this.birthtimeMs = raw.birthtimeMs;
    this.atime = new Date(raw.atimeMs); this.mtime = new Date(raw.mtimeMs);
    this.ctime = new Date(raw.ctimeMs); this.birthtime = new Date(raw.birthtimeMs);
    this._kind = raw.kind;
  }
  isFile() { return this._kind === "file"; }
  isDirectory() { return this._kind === "dir"; }
  isSymbolicLink() { return this._kind === "symlink"; }
  isBlockDevice() { return false; }
  isCharacterDevice() { return false; }
  isFIFO() { return false; }
  isSocket() { return false; }
}

function makeStats(raw) {
  return new Stats(raw);
}

// A Dirent (readdir withFileTypes).
class Dirent {
  constructor(name, kind, parentPath) {
    this.name = name;
    this.parentPath = parentPath;
    this.path = parentPath;
    this._kind = kind;
  }
  isFile() { return this._kind === "file"; }
  isDirectory() { return this._kind === "dir"; }
  isSymbolicLink() { return this._kind === "symlink"; }
  isBlockDevice() { return false; }
  isCharacterDevice() { return false; }
  isFIFO() { return false; }
  isSocket() { return false; }
}

function makeDirent(name, kind, parentPath) {
  return new Dirent(name, kind, parentPath);
}

const encOf = (options) => (typeof options === "string" ? options : options && options.encoding);

// Node's fs accepts a path string, a Buffer, or a file: URL (string or URL object). Reduce any
// of those to a filesystem path.
function toPath(p) {
  if (p instanceof URL) return __builtins.get("url").fileURLToPath(p);
  const s = p instanceof Uint8Array ? Buffer.from(p).toString("utf8") : String(p);
  return s.startsWith("file://") ? __builtins.get("url").fileURLToPath(s) : s;
}

// Coerce write data (string | Buffer | TypedArray | DataView | ArrayBuffer) to a Buffer for the
// byte ops. Buffer.from copies views/ArrayBuffers, so the returned Buffer owns its bytes.
function toBuffer(data, encoding) {
  if (typeof data === "string") return Buffer.from(data, encoding || "utf8");
  return Buffer.from(data);
}

// A synthesized errno-tagged Error, matching the shape `__node`'s ops produce.
function fsError(code, syscall, path, extra) {
  const msg = `${code}: ${extra || "operation failed"}, ${syscall}${path != null ? ` '${path}'` : ""}`;
  const err = new Error(msg);
  err.code = code;
  err.syscall = syscall;
  if (path != null) err.path = path;
  return err;
}

// Map a Node open-flags string (or numeric flag) to lumen-fs's r/w/a/r+/w+/a+ mode. The
// exclusive ('x') and sync ('s') qualifiers are accepted but not separately enforced (lumen-fs
// has no O_EXCL); the read/write/append intent is honoured.
function flagToMode(flags) {
  if (flags == null) return "r";
  if (typeof flags === "number") {
    const c = nodeFs.constants;
    const rw = flags & 3; // O_RDONLY=0, O_WRONLY=1, O_RDWR=2
    if (flags & c.O_APPEND) return rw === 0 ? "a" : "a+";
    if (flags & c.O_TRUNC || flags & c.O_CREAT) return rw === 2 ? "w+" : "w";
    if (rw === 2) return "r+";
    return "r";
  }
  switch (String(flags)) {
    case "r": case "rs": return "r";
    case "r+": case "rs+": return "r+";
    case "w": case "wx": return "w";
    case "w+": case "wx+": return "w+";
    case "a": case "ax": return "a";
    case "a+": case "ax+": return "a+";
    default: return "r";
  }
}

const nodeFs = {
  readFileSync(path, options) {
    // A numeric fd reads via the handle; a path reads the file.
    if (typeof path === "number") {
      const bytes = nodeFs.readvBytes(path);
      const enc = encOf(options);
      return enc ? bytes.toString(enc) : bytes;
    }
    const bytes = Buffer.from(__node.readBytes(toPath(path)));
    const enc = encOf(options);
    return enc ? bytes.toString(enc) : bytes;
  },
  writeFileSync(path, data, options) {
    const enc = encOf(options);
    const buf = toBuffer(data, enc);
    if (typeof path === "number") { nodeFs.writeSync(path, buf, 0, undefined, null); return; }
    // Write raw bytes via a handle so binary content is preserved (the string op re-encodes UTF-8).
    const flag = (options && options.flag) || "w";
    const fd = fs.openSync(toPath(path), flagToMode(flag));
    try { fs.pwriteSync(fd, buf, -1); } finally { fs.closeSync(fd); }
  },
  appendFileSync(path, data, options) {
    const enc = encOf(options);
    const buf = toBuffer(data, enc);
    if (typeof path === "number") { nodeFs.writeSync(path, buf); return; }
    const fd = fs.openSync(toPath(path), "a");
    try { fs.pwriteSync(fd, buf, -1); } finally { fs.closeSync(fd); }
  },
  existsSync(path) {
    try { return fs.existsSync(toPath(path)); } catch { return false; }
  },
  mkdirSync(path, options) {
    return __node.mkdir(toPath(path), !!(options && options.recursive));
  },
  rmdirSync(path, options) {
    __node.rm(toPath(path), !!(options && options.recursive), false);
  },
  rmSync(path, options) {
    __node.rm(toPath(path), !!(options && options.recursive), !!(options && options.force));
  },
  readdirSync(path, options) {
    const p = toPath(path);
    if (options && options.withFileTypes) {
      return __node.readdirTypes(p).map(([name, kind]) => makeDirent(name, kind, p));
    }
    const names = fs.readdirSync(p);
    return encOf(options) === "buffer" ? names.map((n) => Buffer.from(n, "utf8")) : names;
  },
  unlinkSync(path) {
    fs.unlinkSync(toPath(path));
  },
  renameSync(from, to) {
    __node.rename(toPath(from), toPath(to));
  },
  copyFileSync(from, to) {
    __node.copyFile(toPath(from), toPath(to));
  },
  linkSync(existing, path) {
    __node.link(toPath(existing), toPath(path));
  },
  statSync(path) {
    return makeStats(__node.stat(toPath(path)));
  },
  lstatSync(path) {
    return makeStats(__node.lstat(toPath(path)));
  },
  readlinkSync(path) {
    return __node.readlink(toPath(path));
  },
  symlinkSync(target, path) {
    __node.symlink(toPath(target), toPath(path));
  },
  accessSync(path, mode) {
    __node.access(toPath(path));
  },
  chmodSync(path, mode) {
    __node.chmod(toPath(path), Number(mode) || 0);
  },
  utimesSync(path, atime, mtime) {
    __node.utimes(toPath(path), toUnixSeconds(atime), toUnixSeconds(mtime));
  },
  lutimesSync(path, atime, mtime) {
    __node.lutimes(toPath(path), toUnixSeconds(atime), toUnixSeconds(mtime));
  },
  mkdtempSync(prefix, options) {
    const dir = __node.mkdtemp(toPath(prefix));
    return encOf(options) === "buffer" ? Buffer.from(dir, "utf8") : dir;
  },
  realpathSync(path) {
    return __node.realpath(toPath(path));
  },
  statfsSync(path) {
    return __node.statfs(toPath(path));
  },

  // ---- file-descriptor ops ----
  openSync(path, flags, mode) {
    return fs.openSync(toPath(path), flagToMode(flags));
  },
  closeSync(fd) {
    fs.closeSync(fd);
  },
  fstatSync(fd) {
    return makeStats(fs.fstatSync(fd));
  },
  ftruncateSync(fd, len) {
    fs.ftruncateSync(fd, Number(len) || 0);
  },
  truncateSync(path, len) {
    if (typeof path === "number") return nodeFs.ftruncateSync(path, len);
    const fd = nodeFs.openSync(path, "r+");
    try { nodeFs.ftruncateSync(fd, len); } finally { nodeFs.closeSync(fd); }
  },
  fsyncSync(fd) { fs.fsyncSync(fd); },
  fdatasyncSync(fd) { fs.fdatasyncSync(fd); },
  fchmodSync(fd, mode) { fs.fchmodSync(fd, Number(mode) || 0); },
  futimesSync(fd, atime, mtime) {
    fs.futimesSync(fd, toUnixSeconds(atime), toUnixSeconds(mtime));
  },
  // Read a whole file descriptor to a Buffer, from position 0 (helper for readFileSync(fd)).
  readvBytes(fd) {
    const chunks = [];
    let total = 0;
    let pos = 0;
    while (true) {
      const bytes = fs.preadSync(fd, 65536, pos);
      if (bytes.length === 0) break;
      chunks.push(Buffer.from(bytes));
      total += bytes.length;
      pos += bytes.length;
    }
    return Buffer.concat(chunks, total);
  },
  readSync(fd, buffer, offset, length, position) {
    // Options-object overload: readSync(fd, buffer, { offset, length, position }).
    if (offset != null && typeof offset === "object") {
      const o = offset;
      offset = o.offset || 0;
      length = o.length == null ? buffer.length - offset : o.length;
      position = o.position == null ? null : o.position;
    }
    offset = offset || 0;
    if (length == null) length = buffer.length - offset;
    const pos = position == null ? -1 : position;
    const bytes = fs.preadSync(fd, length, pos);
    buffer.set(bytes, offset);
    return bytes.length;
  },
  writeSync(fd, data, a, b, c) {
    if (typeof data === "string") {
      const position = a == null ? -1 : a;
      const bytes = Buffer.from(data, b || "utf8");
      return fs.pwriteSync(fd, bytes, position);
    }
    const buf = toBuffer(data);
    let offset = a || 0;
    let length = b == null ? buf.length - offset : b;
    const position = c == null ? -1 : c;
    const slice = buf.subarray(offset, offset + length);
    return fs.pwriteSync(fd, slice, position);
  },
  readvSync(fd, buffers, position) {
    let total = 0;
    let pos = position == null ? -1 : position;
    for (const buf of buffers) {
      const bytes = fs.preadSync(fd, buf.length, pos);
      if (bytes.length === 0) break;
      buf.set(bytes, 0);
      total += bytes.length;
      if (pos >= 0) pos += bytes.length;
    }
    return total;
  },
  writevSync(fd, buffers, position) {
    let total = 0;
    let pos = position == null ? -1 : position;
    for (const buf of buffers) {
      const n = fs.pwriteSync(fd, buf, pos);
      total += n;
      if (pos >= 0) pos += n;
    }
    return total;
  },

  // ---- unsupported-without-syscall stubs (honest throws, never silent) ----
  chownSync(path, uid, gid) { throw fsError("ENOSYS", "chown", toPath(path), "chown is not supported"); },
  lchownSync(path, uid, gid) { throw fsError("ENOSYS", "lchown", toPath(path), "lchown is not supported"); },
  fchownSync(fd, uid, gid) { throw fsError("ENOSYS", "fchown", null, "fchown is not supported"); },
  lchmodSync(path, mode) { throw fsError("ENOSYS", "lchmod", toPath(path), "lchmod is not supported"); },
};
nodeFs.realpathSync.native = nodeFs.realpathSync;

// _toUnixTimestamp: seconds since epoch, from a number|string|Date (Node's internal helper).
function toUnixSeconds(time) {
  if (time == null) return Date.now() / 1000;
  if (time instanceof Date) return time.getTime() / 1000;
  if (typeof time === "number") return time > 1e11 ? time / 1000 : time;
  const n = Number(time);
  if (!Number.isNaN(n)) return n;
  return new Date(time).getTime() / 1000;
}
nodeFs._toUnixTimestamp = toUnixSeconds;

// Callback style: run the sync op, deliver (err, result) on the next tick (Node never calls the
// callback synchronously). A trailing options argument before the callback is tolerated.
function callbackify(syncFn) {
  return function (...args) {
    const cb = args.pop();
    if (typeof cb !== "function") throw new TypeError("callback is not a function");
    process.nextTick(() => {
      let result;
      try {
        result = syncFn(...args);
      } catch (err) {
        cb(err);
        return;
      }
      cb(null, result);
    });
  };
}

nodeFs.readFile = callbackify(nodeFs.readFileSync);
nodeFs.writeFile = callbackify(nodeFs.writeFileSync);
nodeFs.appendFile = callbackify(nodeFs.appendFileSync);
nodeFs.mkdir = callbackify(nodeFs.mkdirSync);
nodeFs.rmdir = callbackify(nodeFs.rmdirSync);
nodeFs.rm = callbackify(nodeFs.rmSync);
nodeFs.readdir = callbackify(nodeFs.readdirSync);
nodeFs.unlink = callbackify(nodeFs.unlinkSync);
nodeFs.rename = callbackify(nodeFs.renameSync);
nodeFs.copyFile = callbackify(nodeFs.copyFileSync);
nodeFs.link = callbackify(nodeFs.linkSync);
nodeFs.stat = callbackify(nodeFs.statSync);
nodeFs.lstat = callbackify(nodeFs.lstatSync);
nodeFs.fstat = callbackify(nodeFs.fstatSync);
nodeFs.statfs = callbackify(nodeFs.statfsSync);
nodeFs.readlink = callbackify(nodeFs.readlinkSync);
nodeFs.symlink = callbackify(nodeFs.symlinkSync);
nodeFs.access = callbackify(nodeFs.accessSync);
nodeFs.chmod = callbackify(nodeFs.chmodSync);
nodeFs.fchmod = callbackify(nodeFs.fchmodSync);
nodeFs.lchmod = callbackify(nodeFs.lchmodSync);
nodeFs.chown = callbackify(nodeFs.chownSync);
nodeFs.lchown = callbackify(nodeFs.lchownSync);
nodeFs.fchown = callbackify(nodeFs.fchownSync);
nodeFs.utimes = callbackify(nodeFs.utimesSync);
nodeFs.lutimes = callbackify(nodeFs.lutimesSync);
nodeFs.futimes = callbackify(nodeFs.futimesSync);
nodeFs.mkdtemp = callbackify(nodeFs.mkdtempSync);
nodeFs.realpath = callbackify(nodeFs.realpathSync);
nodeFs.realpath.native = nodeFs.realpath;
nodeFs.close = callbackify(nodeFs.closeSync);
nodeFs.truncate = callbackify(nodeFs.truncateSync);
nodeFs.ftruncate = callbackify(nodeFs.ftruncateSync);
nodeFs.fsync = callbackify(nodeFs.fsyncSync);
nodeFs.fdatasync = callbackify(nodeFs.fdatasyncSync);

// open takes (path, flags?, mode?, cb) — flags/mode are optional before the callback.
nodeFs.open = function (path, flags, mode, cb) {
  if (typeof flags === "function") { cb = flags; flags = "r"; mode = undefined; }
  else if (typeof mode === "function") { cb = mode; mode = undefined; }
  process.nextTick(() => {
    try { cb(null, nodeFs.openSync(path, flags, mode)); } catch (e) { cb(e); }
  });
};
nodeFs.exists = function (path, cb) {
  process.nextTick(() => cb(nodeFs.existsSync(path)));
};

// read(fd, buffer, offset, length, position, cb) | read(fd, options, cb) | read(fd, cb).
// Callback receives (err, bytesRead, buffer) — Node's non-standard 3-arg convention.
nodeFs.read = function (fd, a, b, c, d, e) {
  let buffer, offset, length, position, cb;
  if (typeof a === "function") {
    cb = a; buffer = Buffer.alloc(16384); offset = 0; length = buffer.length; position = null;
  } else if (a != null && typeof a === "object" && !ArrayBuffer.isView(a)) {
    const o = a; buffer = o.buffer || Buffer.alloc(16384);
    offset = o.offset || 0; length = o.length == null ? buffer.length - offset : o.length;
    position = o.position == null ? null : o.position; cb = b;
  } else {
    buffer = a; offset = b || 0; length = c == null ? buffer.length - offset : c;
    position = d == null ? null : d; cb = e;
  }
  process.nextTick(() => {
    try { const n = nodeFs.readSync(fd, buffer, offset, length, position); cb(null, n, buffer); }
    catch (err) { cb(err); }
  });
};

// write(fd, buffer, offset, length, position, cb) | write(fd, string, position, encoding, cb).
// Callback receives (err, bytesWritten, buffer|string).
nodeFs.write = function (fd, data, ...rest) {
  const cb = typeof rest[rest.length - 1] === "function" ? rest.pop() : undefined;
  process.nextTick(() => {
    try { const n = nodeFs.writeSync(fd, data, rest[0], rest[1], rest[2]); if (cb) cb(null, n, data); }
    catch (err) { if (cb) cb(err); }
  });
};

nodeFs.readv = function (fd, buffers, position, cb) {
  if (typeof position === "function") { cb = position; position = null; }
  process.nextTick(() => {
    try { const n = nodeFs.readvSync(fd, buffers, position); cb(null, n, buffers); } catch (e) { cb(e); }
  });
};
nodeFs.writev = function (fd, buffers, position, cb) {
  if (typeof position === "function") { cb = position; position = null; }
  process.nextTick(() => {
    try { const n = nodeFs.writevSync(fd, buffers, position); cb(null, n, buffers); } catch (e) { cb(e); }
  });
};

// ---- cp / cpSync (recursive copy) ----
const pathMod = () => __builtins.get("path");

function cpSyncImpl(src, dest, opts) {
  const o = opts || {};
  const p = pathMod();
  const srcStat = o.dereference ? nodeFs.statSync(src) : nodeFs.lstatSync(src);
  if (srcStat.isDirectory()) {
    if (!o.recursive) throw fsError("EISDIR", "cp", src, "recursive option not set for a directory");
    nodeFs.mkdirSync(dest, { recursive: true });
    for (const entry of fs.readdirSync(toPath(src))) {
      cpSyncImpl(p.join(toPath(src), entry), p.join(toPath(dest), entry), o);
    }
    return;
  }
  if (srcStat.isSymbolicLink() && !o.dereference) {
    const target = nodeFs.readlinkSync(src);
    try { nodeFs.unlinkSync(dest); } catch {}
    nodeFs.symlinkSync(target, dest);
    return;
  }
  if (o.filter && !o.filter(toPath(src), toPath(dest))) return;
  const parent = p.dirname(toPath(dest));
  if (parent && parent !== ".") { try { nodeFs.mkdirSync(parent, { recursive: true }); } catch {} }
  if (o.errorOnExist && !o.force && nodeFs.existsSync(dest)) {
    throw fsError("ERR_FS_CP_EEXIST", "cp", toPath(dest), "destination already exists");
  }
  if (!o.force && o.errorOnExist !== true && nodeFs.existsSync(dest) && o.force === false) return;
  nodeFs.copyFileSync(src, dest);
}
nodeFs.cpSync = function (src, dest, options) { cpSyncImpl(src, dest, options); };
nodeFs.cp = function (src, dest, options, cb) {
  if (typeof options === "function") { cb = options; options = {}; }
  process.nextTick(() => {
    try { cpSyncImpl(src, dest, options); cb(null); } catch (e) { cb(e); }
  });
};

// ---- glob / globSync ----
// Translate a glob pattern to an anchored RegExp. '**' crosses path separators, '*' does not,
// '?' is a single non-separator char, and '[...]' is a character class.
function globToRegExp(glob) {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        i++;
        if (glob[i + 1] === "/") { i++; re += "(?:[^/]*/)*"; }
        else re += ".*";
      } else re += "[^/]*";
    } else if (c === "?") {
      re += "[^/]";
    } else if (c === "[") {
      let j = i + 1;
      let cls = "";
      if (glob[j] === "!" || glob[j] === "^") { cls += "^"; j++; }
      while (j < glob.length && glob[j] !== "]") { cls += glob[j] === "\\" ? "\\\\" : glob[j]; j++; }
      if (j >= glob.length) { re += "\\["; } else { re += "[" + cls + "]"; i = j; }
    } else if (".+^${}()|\\/".includes(c)) {
      re += "\\" + c;
    } else {
      re += c;
    }
  }
  return new RegExp("^" + re + "$");
}

function globSyncImpl(pattern, options) {
  const o = options || {};
  const p = pathMod();
  const cwd = o.cwd ? toPath(o.cwd) : process.cwd();
  const patterns = Array.isArray(pattern) ? pattern : [pattern];
  const regexes = patterns.map(globToRegExp);
  const excludes = typeof o.exclude === "function" ? o.exclude : null;
  const withTypes = !!o.withFileTypes;
  const results = [];
  const walk = (absDir, rel) => {
    let entries;
    try { entries = __node.readdirTypes(absDir); } catch { return; }
    for (const [name, kind] of entries) {
      const relPath = rel ? rel + "/" + name : name;
      const matched = regexes.some((r) => r.test(relPath));
      if (matched && !(excludes && excludes(withTypes ? makeDirent(name, kind, absDir) : relPath))) {
        results.push(withTypes ? makeDirent(name, kind, absDir) : relPath);
      }
      if (kind === "dir") walk(p.join(absDir, name), relPath);
    }
  };
  walk(cwd, "");
  return results;
}
nodeFs.globSync = function (pattern, options) { return globSyncImpl(pattern, options); };
nodeFs.glob = function (pattern, options, cb) {
  if (typeof options === "function") { cb = options; options = {}; }
  process.nextTick(() => {
    try { cb(null, globSyncImpl(pattern, options)); } catch (e) { cb(e); }
  });
};

// ---- opendir / Dir ----
class Dir {
  constructor(path, entries) {
    this.path = path;
    this._entries = entries;
    this._i = 0;
    this._closed = false;
  }
  readSync() {
    if (this._i >= this._entries.length) return null;
    return this._entries[this._i++];
  }
  read(cb) {
    const next = this._i < this._entries.length ? this._entries[this._i++] : null;
    if (cb) { process.nextTick(() => cb(null, next)); return; }
    return Promise.resolve(next);
  }
  closeSync() { this._closed = true; }
  close(cb) {
    this._closed = true;
    if (cb) { process.nextTick(() => cb(null)); return; }
    return Promise.resolve();
  }
  async *[Symbol.asyncIterator]() {
    let e;
    while ((e = this.readSync()) !== null) yield e;
  }
}
function opendirSyncImpl(path) {
  const p = toPath(path);
  const entries = __node.readdirTypes(p).map(([name, kind]) => makeDirent(name, kind, p));
  return new Dir(p, entries);
}
nodeFs.opendirSync = function (path, options) { return opendirSyncImpl(path); };
nodeFs.opendir = function (path, options, cb) {
  if (typeof options === "function") { cb = options; options = {}; }
  process.nextTick(() => {
    try { cb(null, opendirSyncImpl(path)); } catch (e) { cb(e); }
  });
};

// ---- openAsBlob ----
nodeFs.openAsBlob = function (path, options) {
  try {
    const bytes = __node.readBytes(toPath(path));
    return Promise.resolve(new Blob([bytes], { type: (options && options.type) || "" }));
  } catch (e) {
    return Promise.reject(e);
  }
};

// ---- watchFile / unwatchFile (real, polling; Node's watchFile is polling too) ----
const fileWatchers = new Map();
function zeroStats(kind) {
  return makeStats({
    dev: 0, ino: 0, mode: 0, nlink: 0, uid: 0, gid: 0, rdev: 0, size: 0,
    blksize: 0, blocks: 0, atimeMs: 0, mtimeMs: 0, ctimeMs: 0, birthtimeMs: 0, kind,
  });
}
function pollStat(p) {
  try { return nodeFs.statSync(p); } catch { return zeroStats("other"); }
}
class StatWatcher extends __builtins.get("events") {
  constructor() { super(); this._handle = null; }
  start() {}
  stop() {}
  ref() { return this; }
  unref() { return this; }
}
nodeFs.watchFile = function (filename, options, listener) {
  if (typeof options === "function") { listener = options; options = {}; }
  const interval = (options && options.interval) || 5007;
  const p = toPath(filename);
  let entry = fileWatchers.get(p);
  if (!entry) {
    const watcher = new StatWatcher();
    entry = { watcher, listeners: new Set(), prev: pollStat(p) };
    entry.timer = setInterval(() => {
      const cur = pollStat(p);
      if (cur.mtimeMs !== entry.prev.mtimeMs || cur.size !== entry.prev.size) {
        const prev = entry.prev;
        entry.prev = cur;
        for (const l of entry.listeners) l(cur, prev);
        entry.watcher.emit("change", cur, prev);
      }
    }, interval);
    if (entry.timer && entry.timer.unref) entry.timer.unref();
    fileWatchers.set(p, entry);
  }
  if (listener) { entry.listeners.add(listener); entry.watcher.on("change", listener); }
  return entry.watcher;
};
nodeFs.unwatchFile = function (filename, listener) {
  const p = toPath(filename);
  const entry = fileWatchers.get(p);
  if (!entry) return;
  if (listener) entry.listeners.delete(listener);
  else entry.listeners.clear();
  if (entry.listeners.size === 0) {
    if (entry.timer) clearInterval(entry.timer);
    fileWatchers.delete(p);
  }
};
// Event-based watch (inotify/FSEvents) has no host backend — throw honestly, never no-op.
nodeFs.watch = function (filename, options, listener) {
  throw fsError("ENOSYS", "watch", toPath(filename), "fs.watch requires a filesystem-event backend that is not available");
};

// ---- ReadStream / WriteStream (real, over the stream module + fd ops) ----
const StreamMod = __builtins.get("stream");
function normalizeStreamOpts(options) {
  if (typeof options === "string") return { encoding: options };
  return options || {};
}

class ReadStream extends StreamMod.Readable {
  constructor(path, options) {
    const o = normalizeStreamOpts(options);
    super({ encoding: o.encoding, highWaterMark: o.highWaterMark });
    this.path = path;
    this.flags = o.flags || "r";
    this.bytesRead = 0;
    this.pos = o.start || 0;
    this._end = o.end == null ? Infinity : o.end;
    this._chunk = o.highWaterMark || 65536;
    this._autoClose = o.autoClose !== false;
    this._ownFd = o.fd == null;
    try {
      this.fd = o.fd != null ? o.fd : nodeFs.openSync(path, this.flags, o.mode);
    } catch (e) {
      this.fd = null;
      process.nextTick(() => this.emit("error", e));
      return;
    }
    process.nextTick(() => {
      if (this.fd == null) return;
      this.emit("open", this.fd);
      this.emit("ready");
      this._pump();
    });
  }
  _pump() {
    try {
      while (this.pos <= this._end) {
        let want = this._chunk;
        if (this._end !== Infinity) want = Math.min(want, this._end - this.pos + 1);
        const bytes = fs.preadSync(this.fd, want, this.pos);
        if (bytes.length === 0) break;
        this.pos += bytes.length;
        this.bytesRead += bytes.length;
        this.push(Buffer.from(bytes));
      }
      this.push(null);
      this._closeStream(null);
    } catch (e) {
      this._closeStream(e);
    }
  }
  _closeStream(err) {
    if (this._ownFd && this._autoClose && this.fd != null) {
      try { nodeFs.closeSync(this.fd); } catch {}
    }
    this.fd = null;
    if (err) process.nextTick(() => this.emit("error", err));
    process.nextTick(() => this.emit("close"));
  }
  close(cb) {
    if (cb) this.once("close", cb);
    if (this.fd != null && this._ownFd) { try { nodeFs.closeSync(this.fd); } catch {} this.fd = null; }
  }
}

class WriteStream extends StreamMod.Writable {
  constructor(path, options) {
    const o = normalizeStreamOpts(options);
    super();
    this.path = path;
    this.flags = o.flags || "w";
    this.bytesWritten = 0;
    this.pos = o.start;
    this._encoding = o.encoding || "utf8";
    this._autoClose = o.autoClose !== false;
    this._ownFd = o.fd == null;
    try {
      this.fd = o.fd != null ? o.fd : nodeFs.openSync(path, this.flags, o.mode);
    } catch (e) {
      this.fd = null;
      process.nextTick(() => this.emit("error", e));
      return;
    }
    this.on("finish", () => {
      if (this._autoClose) process.nextTick(() => this.emit("close"));
    });
    process.nextTick(() => {
      if (this.fd == null) return;
      this.emit("open", this.fd);
      this.emit("ready");
    });
  }
  _write(chunk, encoding, cb) {
    try {
      const buf = toBuffer(chunk, encoding || this._encoding);
      const pos = this.pos == null ? -1 : this.pos;
      const n = fs.pwriteSync(this.fd, buf, pos);
      this.bytesWritten += n;
      if (this.pos != null) this.pos += n;
      cb();
    } catch (e) { cb(e); }
  }
  _final(cb) {
    if (this._ownFd && this._autoClose && this.fd != null) {
      try { nodeFs.closeSync(this.fd); this.fd = null; cb(); } catch (e) { cb(e); }
    } else cb();
  }
  close(cb) {
    if (cb) this.once("close", cb);
    this.end();
  }
}

nodeFs.createReadStream = function (path, options) { return new ReadStream(path, options); };
nodeFs.createWriteStream = function (path, options) { return new WriteStream(path, options); };

// Constants a few tools read (fs.constants / accessSync flags).
nodeFs.constants = {
  F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1,
  O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2,
  O_CREAT: 0x200, O_EXCL: 0x800, O_NOCTTY: 0x20000, O_TRUNC: 0x400, O_APPEND: 0x8,
  O_DIRECTORY: 0x100000, O_NOFOLLOW: 0x100, O_SYNC: 0x80, O_DSYNC: 0x400000,
  O_SYMLINK: 0x200000, O_NONBLOCK: 0x4,
  S_IFMT: 0xf000, S_IFREG: 0x8000, S_IFDIR: 0x4000, S_IFCHR: 0x2000, S_IFBLK: 0x6000,
  S_IFIFO: 0x1000, S_IFLNK: 0xa000, S_IFSOCK: 0xc000,
  S_IRWXU: 0x1c0, S_IRUSR: 0x100, S_IWUSR: 0x80, S_IXUSR: 0x40,
  S_IRWXG: 0x38, S_IRGRP: 0x20, S_IWGRP: 0x10, S_IXGRP: 0x8,
  S_IRWXO: 0x7, S_IROTH: 0x4, S_IWOTH: 0x2, S_IXOTH: 0x1,
  COPYFILE_EXCL: 1, COPYFILE_FICLONE: 2, COPYFILE_FICLONE_FORCE: 4,
  UV_FS_O_FILEMAP: 0,
};

// Top-level access-mode constants (Node re-exports these on the module object).
nodeFs.F_OK = nodeFs.constants.F_OK;
nodeFs.R_OK = nodeFs.constants.R_OK;
nodeFs.W_OK = nodeFs.constants.W_OK;
nodeFs.X_OK = nodeFs.constants.X_OK;

// Class exports.
nodeFs.Stats = Stats;
nodeFs.Dirent = Dirent;
nodeFs.Dir = Dir;
nodeFs.StatWatcher = StatWatcher;
nodeFs.ReadStream = ReadStream;
nodeFs.WriteStream = WriteStream;
nodeFs.FileReadStream = ReadStream;
nodeFs.FileWriteStream = WriteStream;

// ---- Promises API ----
// A FileHandle over an open fd, mirroring node:fs/promises' FileHandle.
class FileHandle {
  constructor(fd) { this.fd = fd; }
  read(buffer, offset, length, position) {
    return new Promise((resolve, reject) => {
      try {
        if (buffer != null && typeof buffer === "object" && !ArrayBuffer.isView(buffer)) {
          const o = buffer; buffer = o.buffer || Buffer.alloc(16384);
          offset = o.offset || 0; length = o.length == null ? buffer.length - offset : o.length;
          position = o.position == null ? null : o.position;
        }
        const bytesRead = nodeFs.readSync(this.fd, buffer, offset, length, position);
        resolve({ bytesRead, buffer });
      } catch (e) { reject(e); }
    });
  }
  write(buffer, offset, length, position) {
    return new Promise((resolve, reject) => {
      try {
        const bytesWritten = nodeFs.writeSync(this.fd, buffer, offset, length, position);
        resolve({ bytesWritten, buffer });
      } catch (e) { reject(e); }
    });
  }
  readv(buffers, position) {
    return new Promise((resolve, reject) => {
      try { resolve({ bytesRead: nodeFs.readvSync(this.fd, buffers, position), buffers }); }
      catch (e) { reject(e); }
    });
  }
  writev(buffers, position) {
    return new Promise((resolve, reject) => {
      try { resolve({ bytesWritten: nodeFs.writevSync(this.fd, buffers, position), buffers }); }
      catch (e) { reject(e); }
    });
  }
  readFile(options) {
    return new Promise((resolve, reject) => {
      try {
        const bytes = nodeFs.readvBytes(this.fd);
        const enc = encOf(options);
        resolve(enc ? bytes.toString(enc) : bytes);
      } catch (e) { reject(e); }
    });
  }
  writeFile(data, options) {
    return new Promise((resolve, reject) => {
      try { nodeFs.writeSync(this.fd, toBuffer(data, encOf(options)), 0, undefined, null); resolve(); }
      catch (e) { reject(e); }
    });
  }
  appendFile(data, options) {
    return new Promise((resolve, reject) => {
      try { nodeFs.writeSync(this.fd, toBuffer(data, encOf(options))); resolve(); }
      catch (e) { reject(e); }
    });
  }
  chmod(mode) { return promisedCall(() => nodeFs.fchmodSync(this.fd, mode)); }
  chown(uid, gid) { return promisedCall(() => nodeFs.fchownSync(this.fd, uid, gid)); }
  stat(options) { return promisedCall(() => nodeFs.fstatSync(this.fd)); }
  truncate(len) { return promisedCall(() => nodeFs.ftruncateSync(this.fd, len || 0)); }
  utimes(atime, mtime) { return promisedCall(() => nodeFs.futimesSync(this.fd, atime, mtime)); }
  sync() { return promisedCall(() => nodeFs.fsyncSync(this.fd)); }
  datasync() { return promisedCall(() => nodeFs.fdatasyncSync(this.fd)); }
  close() { return promisedCall(() => { if (this.fd != null) { nodeFs.closeSync(this.fd); this.fd = null; } }); }
  createReadStream(options) { return new ReadStream(null, { ...normalizeStreamOpts(options), fd: this.fd, autoClose: false }); }
  createWriteStream(options) { return new WriteStream(null, { ...normalizeStreamOpts(options), fd: this.fd, autoClose: false }); }
  [Symbol.asyncDispose]() { return this.close(); }
}

function promisedCall(fn) {
  return new Promise((resolve, reject) => {
    try { resolve(fn()); } catch (e) { reject(e); }
  });
}
const promised = (syncFn) => (...args) => promisedCall(() => syncFn(...args));

const fsPromises = {
  readFile: (path, options) =>
    promisedCall(() => nodeFs.readFileSync(path instanceof FileHandle ? path.fd : path, options)),
  writeFile: (path, data, options) =>
    path instanceof FileHandle
      ? path.writeFile(data, options)
      : promisedCall(() => nodeFs.writeFileSync(path, data, options)),
  appendFile: (path, data, options) =>
    promisedCall(() => nodeFs.appendFileSync(path instanceof FileHandle ? path.fd : path, data, options)),
  open: (path, flags, mode) => promisedCall(() => new FileHandle(nodeFs.openSync(path, flags, mode))),
  mkdir: promised(nodeFs.mkdirSync),
  rmdir: promised(nodeFs.rmdirSync),
  rm: promised(nodeFs.rmSync),
  readdir: promised(nodeFs.readdirSync),
  unlink: promised(nodeFs.unlinkSync),
  rename: promised(nodeFs.renameSync),
  copyFile: promised(nodeFs.copyFileSync),
  link: promised(nodeFs.linkSync),
  cp: promised(nodeFs.cpSync),
  stat: promised(nodeFs.statSync),
  lstat: promised(nodeFs.lstatSync),
  statfs: promised(nodeFs.statfsSync),
  readlink: promised(nodeFs.readlinkSync),
  symlink: promised(nodeFs.symlinkSync),
  access: promised(nodeFs.accessSync),
  chmod: promised(nodeFs.chmodSync),
  lchmod: promised(nodeFs.lchmodSync),
  chown: promised(nodeFs.chownSync),
  lchown: promised(nodeFs.lchownSync),
  utimes: promised(nodeFs.utimesSync),
  lutimes: promised(nodeFs.lutimesSync),
  truncate: promised(nodeFs.truncateSync),
  mkdtemp: promised(nodeFs.mkdtempSync),
  realpath: promised(nodeFs.realpathSync),
  opendir: promised(opendirSyncImpl),
  glob: (pattern, options) => {
    const matches = globSyncImpl(pattern, options);
    return (async function* () { for (const m of matches) yield m; })();
  },
  watch: (filename, options) => {
    throw fsError("ENOSYS", "watch", toPath(filename), "fs.watch requires a filesystem-event backend that is not available");
  },
  FileHandle,
};

nodeFs.promises = fsPromises;
fsPromises.constants = nodeFs.constants;

__builtins.set("fs", nodeFs);
// `node:fs/promises` is the promises API as its own module.
__builtins.set("fs/promises", fsPromises);
