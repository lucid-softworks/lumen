// node:child_process over the __child native ops (real std::process subprocesses). spawn returns a
// ChildProcess (EventEmitter) whose stdout/stderr are Readable node streams pumped by one-shot
// reads and whose stdin is a Writable; exec*/spawnSync build on that or the synchronous execSync
// op. `kill()` sends SIGKILL (std can't send arbitrary signals). `fork` is not supported (there is
// no node binary to re-exec).

const EventEmitter = __builtins.get("events");

// Normalize the stdio option to a 3-tuple of "pipe" | "inherit" | "ignore".
function normalizeStdio(stdio) {
  if (stdio === "inherit") return ["inherit", "inherit", "inherit"];
  if (stdio === "ignore") return ["ignore", "ignore", "ignore"];
  if (Array.isArray(stdio)) {
    return [0, 1, 2].map((i) => {
      const v = stdio[i];
      return v === "inherit" || v === "ignore" ? v : "pipe";
    });
  }
  return ["pipe", "pipe", "pipe"];
}

// env object -> array of [key, value] pairs (or undefined to inherit).
function envPairs(env) {
  if (!env || typeof env !== "object") return undefined;
  return Object.keys(env).map((k) => [k, String(env[k])]);
}

function makeReadable(childId, which) {
  const { Readable } = __builtins.get("stream");
  const stream = new Readable({ read() {} });
  (async () => {
    for (;;) {
      const chunk = await new Promise((resolve, reject) => __child.read(childId, which, resolve, reject));
      if (chunk === null) {
        stream.push(null);
        return;
      }
      stream.push(Buffer.from(chunk));
    }
  })().catch((e) => stream.destroy(e));
  return stream;
}

function makeWritable(childId) {
  const { Writable } = __builtins.get("stream");
  return new Writable({
    write(chunk, enc, cb) {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, typeof enc === "string" ? enc : "utf8");
      __child.write(childId, bytes, () => cb(), (e) => cb(e));
    },
    final(cb) {
      __child.closeStdin(childId);
      cb();
    },
  });
}

class ChildProcess extends EventEmitter {
  constructor(childId, pid, stdio) {
    super();
    this._id = childId;
    this.pid = pid;
    this.killed = false;
    this.exitCode = null;
    this.signalCode = null;
    this.stdin = stdio[0] === "pipe" ? makeWritable(childId) : null;
    this.stdout = stdio[1] === "pipe" ? makeReadable(childId, 1) : null;
    this.stderr = stdio[2] === "pipe" ? makeReadable(childId, 2) : null;
    this.stdio = [this.stdin, this.stdout, this.stderr];
    queueMicrotask(() => this.emit("spawn"));
    new Promise((resolve, reject) => __child.wait(childId, resolve, reject)).then(
      (code) => {
        this.exitCode = code;
        this.emit("exit", code, null);
        // 'close' should follow once stdio has flushed; a microtask is close enough here.
        queueMicrotask(() => this.emit("close", code, null));
      },
      (err) => this.emit("error", err),
    );
  }
  kill(signal) {
    this.killed = true;
    return __child.kill(this._id, typeof signal === "number" ? signal : 0);
  }
  ref() {
    __child.ref(this._id);
  }
  unref() {
    // Detach from the event loop's keep-alive count (Node semantics), so a long-lived service
    // child (esbuild) doesn't block process exit once the main work is done. esbuild toggles
    // ref/unref per request, so both must be real.
    __child.unref(this._id);
  }
}

function spawn(command, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  let cmd = String(command);
  let argv = (args || []).map(String);
  if (options.shell) {
    const shell = typeof options.shell === "string" ? options.shell : "/bin/sh";
    argv = ["-c", [cmd, ...argv].join(" ")];
    cmd = shell;
  }
  const stdio = normalizeStdio(options.stdio);
  const info = __child.spawn(cmd, argv, options.cwd, envPairs(options.env), stdio);
  return new ChildProcess(info.childId, info.pid, stdio);
}

// Collect a ChildProcess's stdout/stderr and invoke a Node-style callback.
function collect(child, encoding, callback) {
  const out = [];
  const err = [];
  if (child.stdout) child.stdout.on("data", (c) => out.push(c));
  if (child.stderr) child.stderr.on("data", (c) => err.push(c));
  let done = false;
  const finish = (error, code) => {
    if (done) return;
    done = true;
    const stdout = Buffer.concat(out);
    const stderr = Buffer.concat(err);
    const asText = (b) => (encoding && encoding !== "buffer" ? b.toString(encoding) : b);
    if (error) return callback(error, asText(stdout), asText(stderr));
    if (code !== 0) {
      const e = new Error(`Command failed with exit code ${code}`);
      e.code = code;
      return callback(e, asText(stdout), asText(stderr));
    }
    callback(null, asText(stdout), asText(stderr));
  };
  child.on("error", (e) => finish(e));
  child.on("close", (code) => finish(null, code));
}

function exec(command, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  options = options || {};
  const child = spawn(command, { ...options, shell: options.shell || "/bin/sh" });
  if (callback) collect(child, options.encoding ?? "utf8", callback);
  return child;
}

function execFile(file, args, options, callback) {
  if (typeof args === "function") {
    callback = args;
    args = [];
    options = {};
  } else if (typeof options === "function") {
    callback = options;
    options = {};
  }
  const child = spawn(file, args || [], options || {});
  if (callback) collect(child, (options && options.encoding) ?? "utf8", callback);
  return child;
}

// ---- synchronous variants ---------------------------------------------------------------------

function makeSyncResult(res, encoding) {
  const enc = encoding && encoding !== "buffer" ? encoding : null;
  const stdout = Buffer.from(res.stdout);
  const stderr = Buffer.from(res.stderr);
  return {
    pid: 0,
    status: res.status,
    signal: null,
    stdout: enc ? stdout.toString(enc) : stdout,
    stderr: enc ? stderr.toString(enc) : stderr,
    output: [null, enc ? stdout.toString(enc) : stdout, enc ? stderr.toString(enc) : stderr],
  };
}

function execFileSync(file, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  const input = options.input ? (Buffer.isBuffer(options.input) ? options.input : Buffer.from(options.input)) : null;
  const res = __child.execSync(String(file), (args || []).map(String), input, options.cwd);
  if (res.status !== 0 && res.status !== null) {
    const e = new Error(`Command failed: ${file}`);
    e.status = res.status;
    e.stdout = Buffer.from(res.stdout);
    e.stderr = Buffer.from(res.stderr);
    throw e;
  }
  const enc = (options.encoding && options.encoding !== "buffer") ? options.encoding : null;
  const out = Buffer.from(res.stdout);
  return enc ? out.toString(enc) : out;
}

function execSync(command, options) {
  options = options || {};
  const input = options.input ? (Buffer.isBuffer(options.input) ? options.input : Buffer.from(options.input)) : null;
  const res = __child.execSync("/bin/sh", ["-c", String(command)], input, options.cwd);
  if (res.status !== 0 && res.status !== null) {
    const e = new Error(`Command failed: ${command}`);
    e.status = res.status;
    e.stderr = Buffer.from(res.stderr);
    throw e;
  }
  const enc = (options.encoding && options.encoding !== "buffer") ? options.encoding : null;
  const out = Buffer.from(res.stdout);
  return enc ? out.toString(enc) : out;
}

function spawnSync(command, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  let cmd = String(command);
  let argv = (args || []).map(String);
  if (options.shell) {
    const shell = typeof options.shell === "string" ? options.shell : "/bin/sh";
    argv = ["-c", [cmd, ...argv].join(" ")];
    cmd = shell;
  }
  const input = options.input ? (Buffer.isBuffer(options.input) ? options.input : Buffer.from(options.input)) : null;
  const res = __child.execSync(cmd, argv, input, options.cwd);
  return makeSyncResult(res, options.encoding);
}

function fork() {
  throw new Error("child_process.fork is not supported (lumen has no node binary to re-exec)");
}

__builtins.set("child_process", {
  spawn,
  exec,
  execFile,
  execFileSync,
  execSync,
  spawnSync,
  fork,
  ChildProcess,
});
