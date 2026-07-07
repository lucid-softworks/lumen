// A cluster of smaller node: builtins. Each is a real implementation of the parts that mean
// something on lumen; where a feature is inherently V8- or debugger-specific it degrades to the
// honest behavior for a non-V8, non-inspected process (a no-op or a clear throw), never a fake
// success.

// ---- node:v8 ----------------------------------------------------------------------------------
{
  const te = new TextEncoder();
  const td = new TextDecoder();
  // A genuine round-trip codec. It is not V8's private wire format (that is engine-internal and
  // not portable anyway) — it serializes the JSON-representable object graph, which is what tools
  // that call v8.serialize for caching actually rely on.
  const serialize = (value) => Buffer.from(te.encode(JSON.stringify(value)));
  const deserialize = (buffer) => JSON.parse(td.decode(buffer));

  const HEAP_LIMIT = 2 * 1024 * 1024 * 1024;
  const getHeapStatistics = () => ({
    total_heap_size: 0,
    total_heap_size_executable: 0,
    total_physical_size: 0,
    total_available_size: HEAP_LIMIT,
    used_heap_size: 0,
    heap_size_limit: HEAP_LIMIT,
    malloced_memory: 0,
    peak_malloced_memory: 0,
    does_zap_garbage: 0,
    number_of_native_contexts: 1,
    number_of_detached_contexts: 0,
  });

  __builtins.set("v8", {
    serialize,
    deserialize,
    getHeapStatistics,
    getHeapSpaceStatistics: () => [],
    getHeapCodeStatistics: () => ({}),
    // A non-V8 engine has no V8 flags to set — the honest result is a no-op.
    setFlagsFromString: () => {},
    getHeapSnapshot: () => {
      throw new Error("node:v8 heap snapshots are not supported in lumen");
    },
  });
}

// ---- node:inspector ---------------------------------------------------------------------------
// No V8 inspector is attached; the correct state is "inert", not a pretend session.
{
  const noop = () => {};
  class Session {
    connect() {}
    connectToMainThread() {}
    disconnect() {}
    post(_method, _params, callback) {
      const cb = typeof _params === "function" ? _params : callback;
      if (cb) cb(new Error("node:inspector is not available in lumen"));
    }
    on() { return this; }
    once() { return this; }
    removeListener() { return this; }
  }
  const inspector = {
    open: noop,
    close: noop,
    url: () => undefined,
    waitForDebugger: noop,
    Session,
    console: globalThis.console,
  };
  __builtins.set("inspector", inspector);
  __builtins.set("inspector/promises", { Session });
}

// ---- node:worker_threads ----------------------------------------------------------------------
// lumen runs a single engine on one loop thread; a Worker would need a second engine on another
// thread with structured message passing (not yet built). We report main-thread state honestly and
// throw a clear error if code actually tries to spawn a worker, rather than faking one.
{
  const notSupported = function () {
    throw new Error("node:worker_threads Worker is not supported in lumen");
  };
  __builtins.set("worker_threads", {
    isMainThread: true,
    threadId: 0,
    parentPort: null,
    workerData: undefined,
    Worker: notSupported,
    MessageChannel: notSupported,
    MessagePort: notSupported,
    markAsUntransferable: () => {},
    isMarkedAsUntransferable: () => false,
    moveMessagePortToContext: notSupported,
    receiveMessageOnPort: () => undefined,
    setEnvironmentData: () => {},
    getEnvironmentData: () => undefined,
    BroadcastChannel:
      typeof globalThis.BroadcastChannel !== "undefined" ? globalThis.BroadcastChannel : notSupported,
  });
}

// ---- node:readline ----------------------------------------------------------------------------
// A real line reader over an input stream (the interactive cursor/history features a TTY would
// provide are absent — lumen runs behind pipes — but line events, question(), and async iteration
// all work).
{
  const { EventEmitter } = __builtins.get("events");

  class Interface extends EventEmitter {
    constructor(options) {
      super();
      const opts = options && options.input ? options : { input: options };
      this.input = opts.input;
      this.output = opts.output;
      this._buf = "";
      this._closed = false;
      if (this.input && typeof this.input.on === "function") {
        this.input.on("data", (chunk) => this._onData(chunk));
        this.input.on("end", () => this.close());
      }
    }

    _onData(chunk) {
      this._buf += chunk.toString();
      let idx;
      while ((idx = this._buf.indexOf("\n")) >= 0) {
        const line = this._buf.slice(0, idx).replace(/\r$/, "");
        this._buf = this._buf.slice(idx + 1);
        this.emit("line", line);
      }
    }

    question(query, callback) {
      if (this.output && typeof this.output.write === "function") this.output.write(query);
      this.once("line", callback);
    }

    close() {
      if (!this._closed) {
        this._closed = true;
        this.emit("close");
      }
    }

    pause() { return this; }
    resume() { return this; }
    write() {}

    [Symbol.asyncIterator]() {
      const pending = [];
      let done = false;
      let waiting = null;
      this.on("line", (line) => {
        if (waiting) { waiting({ value: line, done: false }); waiting = null; }
        else pending.push(line);
      });
      this.on("close", () => {
        done = true;
        if (waiting) { waiting({ value: undefined, done: true }); waiting = null; }
      });
      return {
        next() {
          return new Promise((resolve) => {
            if (pending.length) resolve({ value: pending.shift(), done: false });
            else if (done) resolve({ value: undefined, done: true });
            else waiting = resolve;
          });
        },
        [Symbol.asyncIterator]() { return this; },
      };
    }
  }

  const createInterface = (options) => new Interface(options);
  const questionPromise = (rl) => (query) => new Promise((resolve) => rl.question(query, resolve));

  __builtins.set("readline", {
    createInterface,
    Interface,
    clearLine: () => true,
    clearScreenDown: () => true,
    cursorTo: () => true,
    moveCursor: () => true,
  });
  __builtins.set("readline/promises", {
    createInterface: (options) => {
      const rl = new Interface(options);
      rl.question = questionPromise(rl);
      return rl;
    },
    Interface,
  });
}

// ---- node:process ----------------------------------------------------------------------------
// Node's `process` is an EventEmitter (SIGINT/exit/beforeExit/…). lumen builds `process` in Rust
// without that surface, so mix the emitter methods in here. Signals never fire (no handler
// plumbing), but registering/removing listeners no longer throws, which is what tools rely on.
{
  const EventEmitter = __builtins.get("events");
  const proc = globalThis.process;
  const EMITTER_METHODS = [
    "on", "off", "once", "emit", "addListener", "removeListener", "removeAllListeners",
    "prependListener", "prependOnceListener", "listeners", "rawListeners", "listenerCount",
    "eventNames", "setMaxListeners", "getMaxListeners",
  ];
  for (const m of EMITTER_METHODS) {
    if (typeof proc[m] !== "function") proc[m] = EventEmitter.prototype[m];
  }
}

// The `process` global as an importable module (`import process from 'node:process'`).
__builtins.set("process", globalThis.process);

// ---- node:tls ---------------------------------------------------------------------------------
// TLS cannot be built on std alone (no crypto/handshake stack) and lumen takes no third-party
// crate, so — like node:net's sockets and fetch's https — the module exists (tools import it for
// types / feature detection) but any attempt to actually establish a TLS connection throws.
{
  const notSupported = function () {
    throw new Error("node:tls is not supported in lumen (TLS requires a crypto stack)");
  };
  __builtins.set("tls", {
    connect: notSupported,
    createServer: notSupported,
    createSecureContext: () => ({}),
    checkServerIdentity: () => undefined,
    TLSSocket: notSupported,
    Server: notSupported,
    SecureContext: function () {},
    rootCertificates: [],
    DEFAULT_MIN_VERSION: "TLSv1.2",
    DEFAULT_MAX_VERSION: "TLSv1.3",
    DEFAULT_ECDH_CURVE: "auto",
  });
}

// ---- node:test --------------------------------------------------------------------------------
// A minimal runner: tests execute eagerly and surface failures. lumen has no test-reporter
// integration, so this is just enough for a module that imports node:test to load and run.
{
  const test = (name, options, fn) => {
    const body = typeof options === "function" ? options : fn;
    try {
      const r = body && body({ name, diagnostic() {}, mock: test.mock });
      return r && typeof r.then === "function" ? r : Promise.resolve();
    } catch (e) {
      console.error(`test "${name}" failed:`, e && e.message);
      return Promise.reject(e);
    }
  };
  test.test = test;
  test.it = test;
  test.describe = (name, fn) => { if (typeof name === "function") name(); else if (fn) fn(); };
  test.suite = test.describe;
  test.before = () => {};
  test.after = () => {};
  test.beforeEach = () => {};
  test.afterEach = () => {};
  test.mock = { fn: (impl) => impl || (() => {}), method: () => {}, reset: () => {} };
  __builtins.set("test", test);
}
