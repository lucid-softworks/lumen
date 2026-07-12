// node:child_process over the __child native ops (real std::process subprocesses). spawn returns a
// ChildProcess (EventEmitter) whose stdout/stderr are Readable node streams pumped by one-shot
// reads and whose stdin is a Writable; exec*/spawnSync build on that or the synchronous execSync
// op. `kill()` sends SIGKILL (std can't send arbitrary signals). `fork` re-execs the current Lumen
// binary and carries JSON-framed IPC over its piped stdin/stdout.

const EventEmitter = __builtins.get("events");
const IPC_PREFIX = "\x1eLUMEN_IPC ";

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

function makeReadable(childId, which, onIpcMessage) {
  const { Readable } = __builtins.get("stream");
  const stream = new Readable({ read() {} });
  let pending = "";
  (async () => {
    for (;;) {
      const chunk = await new Promise((resolve, reject) => __child.read(childId, which, resolve, reject));
      if (chunk === null) {
        if (pending) stream.push(Buffer.from(pending));
        stream.push(null);
        return;
      }
      if (!onIpcMessage) {
        stream.push(Buffer.from(chunk));
        continue;
      }
      pending += Buffer.from(chunk).toString("utf8");
      for (;;) {
        const newline = pending.indexOf("\n");
        if (newline < 0) break;
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (line.startsWith(IPC_PREFIX)) {
          try {
            onIpcMessage(JSON.parse(line.slice(IPC_PREFIX.length)));
          } catch (error) {
            stream.destroy(error);
            return;
          }
        } else {
          stream.push(Buffer.from(line + "\n"));
        }
      }
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
  constructor(childId, pid, stdio, onIpcMessage) {
    super();
    this._id = childId;
    this.pid = pid;
    this.killed = false;
    this.exitCode = null;
    this.signalCode = null;
    this.stdin = stdio[0] === "pipe" ? makeWritable(childId) : null;
    this.stdout = stdio[1] === "pipe" ? makeReadable(childId, 1, onIpcMessage) : null;
    this.stderr = stdio[2] === "pipe" ? makeReadable(childId, 2) : null;
    this.stdio = [this.stdin, this.stdout, this.stderr];
    this.connected = typeof onIpcMessage === "function";
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
  send(message, sendHandle, options, callback) {
    if (typeof sendHandle === "function") callback = sendHandle;
    else if (typeof options === "function") callback = options;
    if (sendHandle != null && typeof sendHandle !== "function") {
      const error = new Error("child_process.fork handle transfer is not supported in lumen");
      if (callback) queueMicrotask(() => callback(error));
      else throw error;
      return false;
    }
    if (!this.connected || !this.stdin) {
      const error = new Error("IPC channel is closed");
      error.code = "ERR_IPC_CHANNEL_CLOSED";
      if (callback) queueMicrotask(() => callback(error));
      else this.emit("error", error);
      return false;
    }
    let frame;
    try {
      frame = IPC_PREFIX + JSON.stringify(message === undefined ? null : message) + "\n";
    } catch (error) {
      if (callback) queueMicrotask(() => callback(error));
      else throw error;
      return false;
    }
    this.stdin.write(frame, callback);
    return true;
  }
  disconnect() {
    if (!this.connected) return;
    this.connected = false;
    if (this.stdin) this.stdin.end();
    queueMicrotask(() => this.emit("disconnect"));
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
  let child;
  const onIpcMessage = options._ipc
    ? (message) => child.emit("message", message, null)
    : undefined;
  child = new ChildProcess(info.childId, info.pid, stdio, onIpcMessage);
  return child;
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

function fork(modulePath, args, options) {
  if (!Array.isArray(args)) {
    options = args;
    args = [];
  }
  options = options || {};
  const env = { ...process.env, ...(options.env || {}), LUMEN_FORK_IPC: "1" };
  return spawn(process.execPath, [String(modulePath), ...(args || []).map(String)], {
    ...options,
    env,
    stdio: ["pipe", "pipe", options.silent ? "pipe" : "inherit"],
    _ipc: true,
  });
}

// The child-side channel is installed by the process bootstrap in stdlib_extras.js.
function _forkChild() {}

__builtins.set("child_process", {
  spawn,
  exec,
  execFile,
  execFileSync,
  execSync,
  spawnSync,
  fork,
  _forkChild,
  ChildProcess,
});
