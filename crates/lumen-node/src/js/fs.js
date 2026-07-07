// node:fs — Node's shapes (sync, callback, and promises) over the runtime's `fs` global
// (lumen-fs) plus the `__node` filesystem ops (real stat metadata, rm, rename, copy, …).
// readFile returns a string with an encoding argument, else a Buffer.

// Build a Node-style Stats object from the raw fields `__node.stat`/`lstat` return.
function makeStats(raw) {
  const kind = raw.kind;
  const mkDate = (ms) => new Date(ms);
  return {
    dev: raw.dev, ino: raw.ino, mode: raw.mode, nlink: raw.nlink,
    uid: raw.uid, gid: raw.gid, rdev: raw.rdev, size: raw.size,
    blksize: raw.blksize, blocks: raw.blocks,
    atimeMs: raw.atimeMs, mtimeMs: raw.mtimeMs, ctimeMs: raw.ctimeMs, birthtimeMs: raw.birthtimeMs,
    atime: mkDate(raw.atimeMs), mtime: mkDate(raw.mtimeMs),
    ctime: mkDate(raw.ctimeMs), birthtime: mkDate(raw.birthtimeMs),
    isFile: () => kind === "file",
    isDirectory: () => kind === "dir",
    isSymbolicLink: () => kind === "symlink",
    isBlockDevice: () => false,
    isCharacterDevice: () => false,
    isFIFO: () => false,
    isSocket: () => false,
  };
}

// A Dirent (readdir withFileTypes).
function makeDirent(name, kind, parentPath) {
  return {
    name,
    parentPath,
    path: parentPath,
    isFile: () => kind === "file",
    isDirectory: () => kind === "dir",
    isSymbolicLink: () => kind === "symlink",
    isBlockDevice: () => false,
    isCharacterDevice: () => false,
    isFIFO: () => false,
    isSocket: () => false,
  };
}

const encOf = (options) => (typeof options === "string" ? options : options && options.encoding);

// Node's fs accepts a path string, a Buffer, or a file: URL (string or URL object). Reduce any
// of those to a filesystem path.
function toPath(p) {
  if (p instanceof URL) return __builtins.get("url").fileURLToPath(p);
  const s = p instanceof Uint8Array ? Buffer.from(p).toString("utf8") : String(p);
  return s.startsWith("file://") ? __builtins.get("url").fileURLToPath(s) : s;
}

const nodeFs = {
  readFileSync(path, options) {
    const bytes = Buffer.from(__node.readBytes(toPath(path)));
    const enc = encOf(options);
    return enc ? bytes.toString(enc) : bytes;
  },
  writeFileSync(path, data, options) {
    const enc = encOf(options);
    const str = data instanceof Uint8Array ? Buffer.from(data).toString(enc || "utf8") : String(data);
    fs.writeFileSync(toPath(path), str);
  },
  appendFileSync(path, data) {
    const str = data instanceof Uint8Array ? Buffer.from(data).toString("utf8") : String(data);
    fs.appendFileSync(toPath(path), str);
  },
  existsSync(path) {
    return fs.existsSync(toPath(path));
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
    return fs.readdirSync(p);
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
  accessSync(path) {
    __node.access(toPath(path));
  },
  chmodSync(path, mode) {
    __node.chmod(toPath(path), Number(mode) || 0);
  },
  mkdtempSync(prefix) {
    return __node.mkdtemp(toPath(prefix));
  },
  realpathSync(path) {
    return __node.realpath(toPath(path));
  },
  openSync(path, flags) {
    // lumen-fs takes a single-char mode; take the leading r/w/a of a Node flags string.
    const mode = typeof flags === "string" ? flags[0] : "r";
    return fs.openSync(toPath(path), mode === "r" || mode === "w" || mode === "a" ? mode : "r");
  },
  closeSync(fd) {
    fs.closeSync(fd);
  },
  readSync(fd) {
    return fs.readSync(fd);
  },
  writeSync(fd, data) {
    const str = data instanceof Uint8Array ? Buffer.from(data).toString("utf8") : String(data);
    fs.writeSync(fd, str);
  },
};
nodeFs.realpathSync.native = nodeFs.realpathSync;

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
nodeFs.stat = callbackify(nodeFs.statSync);
nodeFs.lstat = callbackify(nodeFs.lstatSync);
nodeFs.readlink = callbackify(nodeFs.readlinkSync);
nodeFs.symlink = callbackify(nodeFs.symlinkSync);
nodeFs.access = callbackify(nodeFs.accessSync);
nodeFs.chmod = callbackify(nodeFs.chmodSync);
nodeFs.mkdtemp = callbackify(nodeFs.mkdtempSync);
nodeFs.realpath = callbackify(nodeFs.realpathSync);
nodeFs.close = callbackify(nodeFs.closeSync);
// open takes (path, flags?, mode?, cb) — flags/mode are optional before the callback.
nodeFs.open = function (path, flags, mode, cb) {
  if (typeof flags === "function") { cb = flags; flags = "r"; }
  else if (typeof mode === "function") { cb = mode; }
  process.nextTick(() => {
    try { cb(null, nodeFs.openSync(path, flags)); } catch (e) { cb(e); }
  });
};
nodeFs.exists = function (path, cb) {
  process.nextTick(() => cb(nodeFs.existsSync(path)));
};

// Constants a few tools read (fs.constants / accessSync flags).
nodeFs.constants = {
  F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1,
  O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2, O_CREAT: 512, O_EXCL: 2048,
  O_TRUNC: 1024, O_APPEND: 8,
  COPYFILE_EXCL: 1,
};

// Promises API: reuse the sync ops off the microtask queue. readFile/writeFile go through the
// runtime's real async fs ops; the rest are quick metadata calls run synchronously then resolved.
const promised = (syncFn) => (...args) =>
  new Promise((resolve, reject) => {
    try {
      resolve(syncFn(...args));
    } catch (err) {
      reject(err);
    }
  });

nodeFs.promises = {
  readFile: (path, options) =>
    fs.promises.readFile(toPath(path)).then((text) => {
      const enc = encOf(options);
      return enc ? text : Buffer.from(text, "utf8");
    }),
  writeFile: (path, data) => {
    const str = data instanceof Uint8Array ? Buffer.from(data).toString("utf8") : String(data);
    return fs.promises.writeFile(toPath(path), str);
  },
  appendFile: promised(nodeFs.appendFileSync),
  mkdir: promised(nodeFs.mkdirSync),
  rmdir: promised(nodeFs.rmdirSync),
  rm: promised(nodeFs.rmSync),
  readdir: promised(nodeFs.readdirSync),
  unlink: promised(nodeFs.unlinkSync),
  rename: promised(nodeFs.renameSync),
  copyFile: promised(nodeFs.copyFileSync),
  stat: promised(nodeFs.statSync),
  lstat: promised(nodeFs.lstatSync),
  readlink: promised(nodeFs.readlinkSync),
  symlink: promised(nodeFs.symlinkSync),
  access: promised(nodeFs.accessSync),
  chmod: promised(nodeFs.chmodSync),
  mkdtemp: promised(nodeFs.mkdtempSync),
  realpath: promised(nodeFs.realpathSync),
};

__builtins.set("fs", nodeFs);
// `node:fs/promises` is the promises API as its own module.
__builtins.set("fs/promises", nodeFs.promises);
