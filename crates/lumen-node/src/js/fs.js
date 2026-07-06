// node:fs — Node's shapes (sync, callback, and fs.promises) over the runtime's `fs` global
// (lumen-fs) plus the __node probes. Buffers: readFile returns a string with an encoding
// argument, else a Buffer.

const nodeFs = {
  readFileSync(path, options) {
    const enc = typeof options === "string" ? options : options && options.encoding;
    // Read raw bytes; decoding to text (the lumen-fs string path) would corrupt binary files.
    const bytes = Buffer.from(__node.readBytes(String(path)));
    return enc ? bytes.toString(enc) : bytes;
  },
  writeFileSync(path, data, options) {
    const enc = typeof options === "string" ? options : options && options.encoding;
    const str = data instanceof Uint8Array ? Buffer.from(data).toString(enc || "utf8") : String(data);
    fs.writeFileSync(String(path), str);
  },
  appendFileSync(path, data) {
    const str = data instanceof Uint8Array ? Buffer.from(data).toString("utf8") : String(data);
    fs.appendFileSync(String(path), str);
  },
  existsSync(path) {
    return fs.existsSync(String(path));
  },
  mkdirSync(path) {
    fs.mkdirSync(String(path));
  },
  readdirSync(path) {
    return fs.readdirSync(String(path));
  },
  unlinkSync(path) {
    fs.unlinkSync(String(path));
  },
  statSync(path) {
    const p = String(path);
    const isFile = __node.isFile(p);
    const isDir = __node.isDir(p);
    if (!isFile && !isDir && !fs.existsSync(p)) {
      const e = new Error(`ENOENT: no such file or directory, stat '${p}'`);
      e.code = "ENOENT";
      throw e;
    }
    return {
      isFile: () => isFile,
      isDirectory: () => isDir,
      isSymbolicLink: () => false,
      isBlockDevice: () => false,
      isCharacterDevice: () => false,
      isFIFO: () => false,
      isSocket: () => false,
      size: 0,
      mtimeMs: 0,
      ctimeMs: 0,
    };
  },
  realpathSync(path) {
    return __node.realpath(String(path));
  },
};

// Callback style: run the sync op, deliver (err, result) on the next tick (Node never calls
// the callback synchronously).
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
nodeFs.writeFile = callbackify((p, d, o) => nodeFs.writeFileSync(p, d, o));
nodeFs.mkdir = callbackify((p) => nodeFs.mkdirSync(p));
nodeFs.readdir = callbackify((p) => nodeFs.readdirSync(p));
nodeFs.unlink = callbackify((p) => nodeFs.unlinkSync(p));
nodeFs.stat = callbackify((p) => nodeFs.statSync(p));
nodeFs.exists = function (path, cb) {
  process.nextTick(() => cb(nodeFs.existsSync(path)));
};

nodeFs.promises = {
  readFile: (path, options) => fs.promises.readFile(String(path)).then((text) => {
    const enc = typeof options === "string" ? options : options && options.encoding;
    return enc ? text : Buffer.from(text, "utf8");
  }),
  writeFile: (path, data) => {
    const str = data instanceof Uint8Array ? Buffer.from(data).toString("utf8") : String(data);
    return fs.promises.writeFile(String(path), str);
  },
  mkdir: async (path) => nodeFs.mkdirSync(path),
  readdir: async (path) => nodeFs.readdirSync(path),
  unlink: async (path) => nodeFs.unlinkSync(path),
  stat: async (path) => nodeFs.statSync(path),
};

__builtins.set("fs", nodeFs);
