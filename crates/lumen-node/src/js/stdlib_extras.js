// A cluster of smaller node: builtins. Each is a real implementation of the parts that mean
// something on lumen; where a feature is inherently V8- or debugger-specific it degrades to the
// honest behavior for a non-V8, non-inspected process (a no-op or a clear throw), never a fake
// success.

// ---- node:v8 ----------------------------------------------------------------------------------
{
  const te = new TextEncoder();
  const td = new TextDecoder();

  // A genuine structured-clone-style codec over Buffer. It is NOT V8's private wire format (that is
  // engine-internal and not portable), but it is self-consistent: serialize -> deserialize round-
  // trips the object graph, including the reference types JSON drops (Map/Set/Date/BigInt/typed
  // arrays/ArrayBuffer) and shared/circular references. Format is a 2-byte header then tagged
  // values; container tags register the object before recursing so cycles resolve via back-refs.
  const TAG = {
    UNDEFINED: 0, NULL: 1, TRUE: 2, FALSE: 3, NUMBER: 4, STRING: 5, BIGINT: 6,
    ARRAY: 7, OBJECT: 8, MAP: 9, SET: 10, DATE: 11, REGEXP: 12,
    ARRAYBUFFER: 13, TYPEDARRAY: 14, BUFFER: 15, REF: 16,
  };
  // Ordered so an index survives round-trips; BUFFER is handled separately (it is Uint8Array too).
  const VIEW_CTORS = [
    Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
    Int32Array, Uint32Array, Float32Array, Float64Array, BigInt64Array, BigUint64Array, DataView,
  ];
  const scratch = new DataView(new ArrayBuffer(8));

  class Serializer {
    constructor() {
      this._bytes = [];
      this._ids = new Map();
      this._nextId = 0;
    }
    _byte(b) { this._bytes.push(b & 0xff); }
    writeHeader() { this._byte(0xff); this._byte(0x0f); }
    writeUint32(v) {
      this._byte(v); this._byte(v >>> 8); this._byte(v >>> 16); this._byte(v >>> 24);
    }
    writeUint64(v) {
      const lo = Number(BigInt(v) & 0xffffffffn);
      const hi = Number((BigInt(v) >> 32n) & 0xffffffffn);
      this.writeUint32(lo); this.writeUint32(hi);
    }
    writeDouble(v) {
      scratch.setFloat64(0, v, true);
      for (let i = 0; i < 8; i++) this._byte(scratch.getUint8(i));
    }
    writeRawBytes(bytes) { for (let i = 0; i < bytes.length; i++) this._byte(bytes[i]); }
    _writeString(str) {
      const enc = te.encode(str);
      this.writeUint32(enc.length);
      this.writeRawBytes(enc);
    }
    transferArrayBuffer() {}
    _setTreatArrayBufferViewsAsHostObjects() {}
    _writeHostObject() {
      const err = new Error("Unserializable host object");
      err.code = "ERR_CANNOT_TRANSFER_OBJECT";
      throw err;
    }
    _ref(obj) {
      if (this._ids.has(obj)) { this._byte(TAG.REF); this.writeUint32(this._ids.get(obj)); return true; }
      this._ids.set(obj, this._nextId++);
      return false;
    }
    writeValue(value) {
      const t = typeof value;
      if (value === undefined) return this._byte(TAG.UNDEFINED);
      if (value === null) return this._byte(TAG.NULL);
      if (t === "boolean") return this._byte(value ? TAG.TRUE : TAG.FALSE);
      if (t === "number") { this._byte(TAG.NUMBER); return this.writeDouble(value); }
      if (t === "string") { this._byte(TAG.STRING); return this._writeString(value); }
      if (t === "bigint") { this._byte(TAG.BIGINT); return this._writeString(value.toString()); }
      if (t !== "object" && t !== "function") {
        const err = new Error(`Unsupported value type: ${t}`);
        err.code = "ERR_CANNOT_TRANSFER_OBJECT";
        throw err;
      }
      if (this._ref(value)) return;
      if (Array.isArray(value)) {
        this._byte(TAG.ARRAY);
        this.writeUint32(value.length);
        for (let i = 0; i < value.length; i++) this.writeValue(value[i]);
        return;
      }
      if (value instanceof Date) { this._byte(TAG.DATE); return this.writeDouble(value.getTime()); }
      if (value instanceof RegExp) {
        this._byte(TAG.REGEXP); this._writeString(value.source); return this._writeString(value.flags);
      }
      if (value instanceof Map) {
        this._byte(TAG.MAP);
        this.writeUint32(value.size);
        for (const [k, v] of value) { this.writeValue(k); this.writeValue(v); }
        return;
      }
      if (value instanceof Set) {
        this._byte(TAG.SET);
        this.writeUint32(value.size);
        for (const v of value) this.writeValue(v);
        return;
      }
      if (value instanceof ArrayBuffer) {
        this._byte(TAG.ARRAYBUFFER);
        const view = new Uint8Array(value);
        this.writeUint32(view.length);
        return this.writeRawBytes(view);
      }
      if (typeof Buffer !== "undefined" && Buffer.isBuffer(value)) {
        this._byte(TAG.BUFFER);
        this.writeUint32(value.length);
        return this.writeRawBytes(value);
      }
      if (ArrayBuffer.isView(value)) {
        this._byte(TAG.TYPEDARRAY);
        this._byte(VIEW_CTORS.indexOf(value.constructor));
        const raw = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        this.writeUint32(raw.length);
        return this.writeRawBytes(raw);
      }
      // Plain object: own enumerable string keys (Symbols are dropped, as in structured clone).
      this._byte(TAG.OBJECT);
      const keys = Object.keys(value);
      this.writeUint32(keys.length);
      for (const k of keys) { this._writeString(k); this.writeValue(value[k]); }
    }
    releaseBuffer() { return Buffer.from(this._bytes); }
  }

  // DefaultSerializer is the class serialize() uses; it differs from Serializer only in how it
  // treats host objects (here: the base behavior is already correct for our surface).
  class DefaultSerializer extends Serializer {}

  class Deserializer {
    constructor(buffer) {
      this._buf = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
      this._view = new DataView(this._buf.buffer, this._buf.byteOffset, this._buf.byteLength);
      this._pos = 0;
      this._objs = [];
    }
    readHeader() { this._pos += 2; return 0x0f; }
    getWireFormatVersion() { return 0x0f; }
    readUint32() {
      const v = this._view.getUint32(this._pos, true);
      this._pos += 4;
      return v;
    }
    readUint64() {
      const lo = BigInt(this.readUint32());
      const hi = BigInt(this.readUint32());
      return Number((hi << 32n) | lo);
    }
    readDouble() {
      const v = this._view.getFloat64(this._pos, true);
      this._pos += 8;
      return v;
    }
    readRawBytes(len) {
      const out = this._buf.subarray(this._pos, this._pos + len);
      this._pos += len;
      return out;
    }
    _readString() {
      const len = this.readUint32();
      return td.decode(this.readRawBytes(len));
    }
    transferArrayBuffer() {}
    _readHostObject() {
      throw new Error("Unserializable host object");
    }
    readValue() {
      const tag = this._buf[this._pos++];
      switch (tag) {
        case TAG.UNDEFINED: return undefined;
        case TAG.NULL: return null;
        case TAG.TRUE: return true;
        case TAG.FALSE: return false;
        case TAG.NUMBER: return this.readDouble();
        case TAG.STRING: return this._readString();
        case TAG.BIGINT: return BigInt(this._readString());
        case TAG.REF: return this._objs[this.readUint32()];
        case TAG.ARRAY: {
          const n = this.readUint32();
          const arr = [];
          this._objs.push(arr);
          for (let i = 0; i < n; i++) arr.push(this.readValue());
          return arr;
        }
        case TAG.OBJECT: {
          const n = this.readUint32();
          const obj = {};
          this._objs.push(obj);
          for (let i = 0; i < n; i++) { const k = this._readString(); obj[k] = this.readValue(); }
          return obj;
        }
        case TAG.MAP: {
          const n = this.readUint32();
          const map = new Map();
          this._objs.push(map);
          for (let i = 0; i < n; i++) { const k = this.readValue(); map.set(k, this.readValue()); }
          return map;
        }
        case TAG.SET: {
          const n = this.readUint32();
          const set = new Set();
          this._objs.push(set);
          for (let i = 0; i < n; i++) set.add(this.readValue());
          return set;
        }
        case TAG.DATE: { const d = new Date(this.readDouble()); this._objs.push(d); return d; }
        case TAG.REGEXP: {
          const re = new RegExp(this._readString(), this._readString());
          this._objs.push(re);
          return re;
        }
        case TAG.ARRAYBUFFER: {
          const n = this.readUint32();
          const ab = this.readRawBytes(n).slice().buffer;
          this._objs.push(ab);
          return ab;
        }
        case TAG.BUFFER: {
          const n = this.readUint32();
          const b = Buffer.from(this.readRawBytes(n));
          this._objs.push(b);
          return b;
        }
        case TAG.TYPEDARRAY: {
          const kind = this._buf[this._pos++];
          const n = this.readUint32();
          const bytes = this.readRawBytes(n).slice();
          const Ctor = VIEW_CTORS[kind];
          const view = Ctor === DataView ? new DataView(bytes.buffer) : new Ctor(bytes.buffer);
          this._objs.push(view);
          return view;
        }
        default:
          throw new Error(`v8.deserialize: unknown tag ${tag}`);
      }
    }
  }

  class DefaultDeserializer extends Deserializer {}

  const serialize = (value) => {
    const s = new DefaultSerializer();
    s.writeHeader();
    s.writeValue(value);
    return s.releaseBuffer();
  };
  const deserialize = (buffer) => {
    const d = new DefaultDeserializer(buffer);
    d.readHeader();
    return d.readValue();
  };

  // lumen exposes no V8 heap accounting, so the numeric fields are honest zeros in Node's shape
  // (rather than invented figures). Field names and count match Node v22 exactly.
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
    total_global_handles_size: 0,
    used_global_handles_size: 0,
    external_memory: 0,
  });
  const HEAP_SPACES = [
    "read_only_space", "new_space", "old_space", "code_space", "shared_space",
    "trusted_space", "new_large_object_space", "large_object_space",
    "code_large_object_space", "shared_large_object_space", "trusted_large_object_space",
  ];
  const getHeapSpaceStatistics = () =>
    HEAP_SPACES.map((space_name) => ({
      space_name,
      space_size: 0,
      space_used_size: 0,
      space_available_size: 0,
      physical_space_size: 0,
    }));
  const getHeapCodeStatistics = () => ({
    code_and_metadata_size: 0,
    bytecode_and_metadata_size: 0,
    external_script_source_size: 0,
    cpu_profiler_metadata_size: 0,
  });
  const getCppHeapStatistics = () => ({
    committed_size_bytes: 0,
    resident_size_bytes: 0,
    used_size_bytes: 0,
    space_statistics: [],
    type_names: [],
    detail_level: "brief",
  });

  // A real byte-level check: whether the string is representable in a single-byte (latin1) form.
  const isStringOneByteRepresentation = (str) => {
    if (typeof str !== "string") {
      const err = new TypeError('The "content" argument must be of type string.');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    for (let i = 0; i < str.length; i++) if (str.charCodeAt(i) > 0xff) return false;
    return true;
  };

  // Not backed by V8's build hash; a stable tag so callers that only compare it for equality across
  // a single process (compile-cache guards) behave consistently.
  const cachedDataVersionTag = () => 0x6c756d65;

  // Inert GC profiler: lumen surfaces no GC event stream, so a session records nothing.
  class GCProfiler {
    start() { this._start = Date.now(); }
    stop() {
      return { version: 1, startTime: this._start || Date.now(), statistics: [], endTime: Date.now() };
    }
  }

  const notSupported = (what) => () => {
    throw new Error(`node:v8 ${what} is not supported in lumen`);
  };

  // promiseHooks: lumen has no promise-lifecycle hook plumbing. Shaped like Node's; each registrar
  // is a no-op that returns the standard "stop" function.
  const noopStop = () => {};
  const promiseHooks = {
    createHook: () => noopStop,
    onInit: () => noopStop,
    onBefore: () => noopStop,
    onAfter: () => noopStop,
    onSettled: () => noopStop,
  };

  // startupSnapshot: we never build a V8 startup snapshot, so callbacks are inert and
  // isBuildingSnapshot is always false — the honest state for a non-snapshotting process.
  const startupSnapshot = {
    addDeserializeCallback: () => {},
    addSerializeCallback: () => {},
    setDeserializeMainFunction: () => {},
    isBuildingSnapshot: () => false,
  };

  __builtins.set("v8", {
    serialize,
    deserialize,
    Serializer,
    Deserializer,
    DefaultSerializer,
    DefaultDeserializer,
    getHeapStatistics,
    getHeapSpaceStatistics,
    getHeapCodeStatistics,
    getCppHeapStatistics,
    isStringOneByteRepresentation,
    cachedDataVersionTag,
    GCProfiler,
    promiseHooks,
    startupSnapshot,
    // A non-V8 engine has no V8 flags to set — the honest result is a no-op.
    setFlagsFromString: () => {},
    // No snapshot-on-near-heap-limit mechanism exists here; registering a limit is a no-op.
    setHeapSnapshotNearHeapLimit: () => {},
    // Coverage collection is not wired up (no NODE_V8_COVERAGE sink); these are the inert no-ops
    // Node itself uses when coverage is disabled.
    takeCoverage: () => {},
    stopCoverage: () => {},
    // Heap introspection (walking live objects / writing .heapsnapshot) is unbackable without V8.
    queryObjects: notSupported("queryObjects"),
    getHeapSnapshot: notSupported("heap snapshots"),
    writeHeapSnapshot: notSupported("heap snapshots"),
  });
}

// ---- node:inspector (and node:inspector/promises) ---------------------------------------------
// No V8 inspector is attached; the correct state is "inert", not a pretend session. Session.post
// reports the unavailability honestly (callback error / rejected promise); the module shape
// otherwise mirrors Node's (open/close/url/waitForDebugger, the Network domain, `console`).
{
  const noop = () => {};
  const unavailable = () => new Error("node:inspector is not available in lumen");
  class Session {
    connect() {}
    connectToMainThread() {}
    disconnect() {}
    post(_method, _params, callback) {
      const cb = typeof _params === "function" ? _params : callback;
      if (cb) cb(unavailable());
    }
    on() { return this; }
    once() { return this; }
    removeListener() { return this; }
    emit() { return false; }
  }
  // The Network inspector domain (Node ≥ 22): reporting hooks that no-op without an inspector.
  const Network = {
    requestWillBeSent: noop,
    responseReceived: noop,
    loadingFinished: noop,
    loadingFailed: noop,
  };
  const base = {
    open: noop,
    close: noop,
    url: () => undefined,
    waitForDebugger: noop,
    console: globalThis.console,
    Network,
  };
  __builtins.set("inspector", { ...base, Session });

  // The promises variant: identical surface, but Session.post returns a Promise.
  class SessionPromises extends Session {
    post(_method, _params) {
      return Promise.reject(unavailable());
    }
  }
  __builtins.set("inspector/promises", { ...base, Session: SessionPromises });
}

// ---- node:sys ---------------------------------------------------------------------------------
// The long-deprecated alias for node:util — the *same* object, exactly as Node's `sys` is.
__builtins.set("sys", __builtins.get("util"));

// ---- node:stream/promises ---------------------------------------------------------------------
// Promise forms of stream.pipeline / stream.finished, over the existing node:stream module.
{
  const stream = __builtins.get("stream");
  __builtins.set("stream/promises", {
    finished: (s, opts) =>
      new Promise((resolve, reject) => stream.finished(s, opts || {}, (err) => (err ? reject(err) : resolve()))),
    pipeline: (...args) =>
      new Promise((resolve, reject) => stream.pipeline(...args, (err) => (err ? reject(err) : resolve()))),
  });
}

// ---- node:stream/consumers --------------------------------------------------------------------
// Fully consume a stream / async-iterable / iterable into a single value.
{
  async function collect(src) {
    const chunks = [];
    for await (const chunk of src) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk));
    }
    return Buffer.concat(chunks);
  }
  const text = async (src) => (await collect(src)).toString("utf8");
  const buffer = async (src) => collect(src);
  const json = async (src) => JSON.parse(await text(src));
  const arrayBuffer = async (src) => {
    const b = await collect(src);
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  };
  const blob = async (src) => new Blob([await collect(src)]);
  __builtins.set("stream/consumers", { arrayBuffer, blob, buffer, json, text });
}

// ---- node:readline ----------------------------------------------------------------------------
// A real line reader over an input stream (the interactive cursor/history features a TTY would
// provide are absent — lumen runs behind pipes — but line events, question(), and async iteration
// all work).
{
  const { EventEmitter } = __builtins.get("events");
  const kKeypressWired = Symbol("readline.keypressWired");

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

  // emitKeypressEvents(stream) — turn a stream's incoming data into 'keypress' events. lumen has no
  // TTY key decoding, so this emits one keypress per character with a best-effort key descriptor,
  // which is what non-TTY consumers of the API can observe.
  const emitKeypressEvents = (stream, iface) => {
    if (!stream || stream[kKeypressWired]) return;
    stream[kKeypressWired] = true;
    stream.on("data", (chunk) => {
      const str = chunk.toString();
      for (const ch of str) {
        const key = { sequence: ch, name: undefined, ctrl: false, meta: false, shift: false };
        if (ch === "\r" || ch === "\n") key.name = "return";
        else if (ch === "\t") key.name = "tab";
        else if (ch === "\x7f") key.name = "backspace";
        else if (ch >= " ") key.name = ch.toLowerCase();
        stream.emit("keypress", ch, key);
      }
    });
  };

  // node:readline/promises — same Interface with a promise-returning question(), plus the Readline
  // class that batches cursor/screen operations and applies them on commit().
  class Readline {
    constructor(stream, options = {}) {
      this._stream = stream;
      this._autoCommit = Boolean(options.autoCommit);
      this._ops = [];
    }
    _push(op) {
      if (this._autoCommit) return this.commit();
      this._ops.push(op);
      return this;
    }
    clearLine(dir) { return this._push(() => {}); }
    clearScreenDown() { return this._push(() => {}); }
    cursorTo(x, y) { return this._push(() => {}); }
    moveCursor(dx, dy) { return this._push(() => {}); }
    commit() {
      for (const op of this._ops) op();
      this._ops = [];
      return Promise.resolve();
    }
    rollback() {
      this._ops = [];
      return Promise.resolve();
    }
  }

  const promisesModule = {
    createInterface: (options) => {
      const rl = new Interface(options);
      rl.question = questionPromise(rl);
      return rl;
    },
    Interface,
    Readline,
  };

  __builtins.set("readline", {
    createInterface,
    Interface,
    clearLine: () => true,
    clearScreenDown: () => true,
    cursorTo: () => true,
    moveCursor: () => true,
    emitKeypressEvents,
    promises: promisesModule,
  });
  __builtins.set("readline/promises", promisesModule);
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

  // ---- fuller node:process surface ----------------------------------------------------------
  // The Rust layer (lumen-runtime/process.rs) already supplies argv/env/platform/pid/arch,
  // cwd/exit/nextTick, stdout/stderr, hrtime/uptime, and the real OS-identity calls (uid/gid/ppid,
  // kill/umask/chdir/abort). Everything below is JS-expressible: derived facts, honest zero-data
  // metrics, and Node-shaped stubs for surfaces lumen can't back with real data. Nothing here
  // fabricates plausible-but-false numbers — unmeasured metrics report 0 / [] and unsupported
  // operations throw.

  // EventEmitter bookkeeping fields Node exposes as own keys (methods above lazily init these too).
  if (proc._events === undefined) {
    proc._events = Object.create(null);
    proc._eventsCount = 0;
    proc._maxListeners = undefined;
  }

  // argv0/execPath/title are stamped by the Rust data-prop pass (which, unlike this glue, runs
  // after argv is populated). execArgv — the runtime flags before the script — is empty for lumen.
  proc.execArgv = [];

  // Plain data slots (all settable, matching Node).
  proc.exitCode = undefined;
  proc.debugPort = 9229;
  proc.domain = null;
  proc.moduleLoadList = [];
  proc.config = { target_defaults: {}, variables: {} };
  proc.sourceMapsEnabled = false;
  proc.allowedNodeEnvironmentFlags = new Set();

  // cwd/exit/nextTick are defined by the Rust op layer as non-enumerable; Node exposes them as
  // own-enumerable keys, so re-stamp the descriptor (they are configurable).
  for (const k of ["cwd", "nextTick"]) {
    const d = Object.getOwnPropertyDescriptor(proc, k);
    if (d && !d.enumerable && d.configurable) {
      Object.defineProperty(proc, k, { value: proc[k], enumerable: true, configurable: true, writable: true });
    }
  }

  // exit() honors process.exitCode when called without an explicit code; reallyExit is the raw op.
  const nativeExit = proc.exit;
  proc.reallyExit = nativeExit;
  Object.defineProperty(proc, "exit", {
    value: function (code) {
      const c = code !== undefined && code !== null ? code
        : (proc.exitCode !== undefined && proc.exitCode !== null ? proc.exitCode : 0);
      return nativeExit(c);
    },
    enumerable: true, configurable: true, writable: true,
  });

  // Real: print the Node-style warning line to stderr and emit a 'warning' event with an Error.
  proc.emitWarning = function (warning, options) {
    let type = "Warning", code, detail;
    if (typeof options === "string") {
      type = options;
    } else if (options && typeof options === "object") {
      if (options.type) type = options.type;
      code = options.code;
      detail = options.detail;
    }
    let err;
    if (warning instanceof Error) {
      err = warning;
    } else {
      err = new Error(String(warning));
      err.name = type;
    }
    if (code !== undefined) err.code = code;
    let line = "(node:" + proc.pid + ") ";
    if (code !== undefined) line += "[" + code + "] ";
    line += (err.name || type) + ": " + err.message;
    proc.stderr.write(line + "\n");
    if (detail) proc.stderr.write(String(detail) + "\n");
    proc.emit("warning", err);
  };

  // Real: bridge to the builtin-module registry (getBuiltinModule('fs') === require('fs')).
  proc.getBuiltinModule = function (id) {
    const name = typeof id === "string" && id.startsWith("node:") ? id.slice(5) : id;
    return __builtins.has(name) ? __builtins.get(name) : undefined;
  };

  // Real: parse a .env file (default ".env") and assign into process.env. Throws if unreadable.
  proc.loadEnvFile = function (path) {
    const fs = __builtins.get("fs");
    const text = fs.readFileSync(path == null ? ".env" : path, "utf8");
    for (const rawLine of text.split(/\r?\n/)) {
      const line = rawLine.trim();
      if (!line || line[0] === "#") continue;
      const eq = line.indexOf("=");
      if (eq === -1) continue;
      const key = line.slice(0, eq).trim();
      if (!key) continue;
      let val = line.slice(eq + 1).trim();
      const q = val[0];
      if ((q === '"' || q === "'") && val[val.length - 1] === q) val = val.slice(1, -1);
      proc.env[key] = val;
    }
  };

  // Real: dlopen a native addon into module.exports via the N-API loader (dylib.rs / napi.rs).
  proc.dlopen = function (module, filename) {
    module.exports = globalThis.__node.loadNativeAddon(filename);
    return module.exports;
  };

  // Real state, no real source-map support yet: toggle the flag setSourceMapsEnabled reads back.
  proc.setSourceMapsEnabled = function (val) { proc.sourceMapsEnabled = !!val; };

  // Honest zero-data metrics: lumen doesn't instrument memory/CPU, so these report 0 rather than
  // fabricating figures. Shapes match Node so callers that destructure them don't crash.
  const memoryUsage = () => ({ rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 });
  memoryUsage.rss = () => 0;
  proc.memoryUsage = memoryUsage;
  proc.availableMemory = () => 0;
  proc.constrainedMemory = () => 0;
  proc.cpuUsage = () => ({ user: 0, system: 0 });
  proc.resourceUsage = () => ({
    userCPUTime: 0, systemCPUTime: 0, maxRSS: 0, sharedMemorySize: 0, unsharedDataSize: 0,
    unsharedStackSize: 0, minorPageFault: 0, majorPageFault: 0, swappedOut: 0, fsRead: 0,
    fsWrite: 0, ipcSent: 0, ipcReceived: 0, signalsCount: 0, voluntaryContextSwitches: 0,
    involuntaryContextSwitches: 0,
  });
  proc.getActiveResourcesInfo = () => [];

  // Honest build-feature booleans (false where lumen genuinely lacks the capability, e.g. tls).
  proc.features = {
    inspector: false, debug: false, uv: false, ipv6: true,
    tls_alpn: false, tls_sni: false, tls_ocsp: false, tls: false, cached_builtins: true,
  };

  proc.release = { name: "node", lts: undefined, sourceUrl: "", headersUrl: "" };

  // Node-shaped diagnostic-report namespace; lumen writes no reports, so the writers are stubs.
  proc.report = {
    compact: false, directory: "", filename: "", signal: "SIGUSR2",
    reportOnFatalError: false, reportOnSignal: false, reportOnUncaughtException: false,
    excludeEnv: false, excludeNetwork: false,
    getReport: () => ({}),
    writeReport: () => "",
  };

  // lumen imposes no permission model, so every capability is genuinely permitted.
  proc.permission = { has: () => true };

  // Honest no-ops: lumen has no exit-time finalization hooks to register against.
  proc.finalization = { register: () => {}, registerBeforeExit: () => {}, unregister: () => {} };

  // Modern Node throws for internal bindings; lumen exposes none.
  proc.binding = function (name) {
    throw new Error("process.binding('" + name + "') is not supported in lumen");
  };
  proc._linkedBinding = proc.binding;

  // Not supported: replacing the process image / changing OS identity can't be done honestly
  // without the underlying syscall, and silently "succeeding" would misrepresent the result.
  proc.execve = function () { throw new Error("process.execve is not supported in lumen"); };
  const unsupportedId = (name) => function () {
    throw new Error("process." + name + " is not supported in lumen");
  };
  for (const s of ["setuid", "setgid", "seteuid", "setegid", "setgroups", "initgroups"]) {
    proc[s] = unsupportedId(s);
  }
  // getgroups: lumen doesn't enumerate supplementary groups; [] is the honest empty answer.
  if (typeof proc.getgroups !== "function") proc.getgroups = () => [];

  // Real state tracking (the callback isn't routed anywhere yet, but registration is honest).
  let uncaughtCb = null;
  proc.setUncaughtExceptionCaptureCallback = function (cb) {
    if (cb !== null && typeof cb !== "function") {
      throw new TypeError('The "fn" argument must be of type function or null');
    }
    if (cb !== null && uncaughtCb !== null) {
      throw new Error("`process.setUncaughtExceptionCaptureCallback()` was called while a capture callback was already active");
    }
    uncaughtCb = cb;
  };
  proc.hasUncaughtExceptionCaptureCallback = () => uncaughtCb !== null;

  // Real: the deprecated process.assert.
  proc.assert = function (value, message) {
    if (!value) throw new Error("assertion failed" + (message ? ": " + message : ""));
  };

  // No-ops: process-level ref/unref have no handle to keep the loop alive here.
  proc.ref = () => {};
  proc.unref = () => {};

  // stdin: an on-demand Readable over the runtime's blocking stdin op. Reads begin only when the
  // stream consumer asks for data, so an unused stdin never holds the event loop open.
  const streamMod = __builtins.get("stream");
  let stdin;
  if (streamMod && streamMod.Readable) {
    stdin = new streamMod.Readable({ read() {} });
    let pumping = false;
    const pump = async () => {
      try {
        for (;;) {
          const chunk = await new Promise((resolve, reject) => proc._readStdin(resolve, reject));
          if (chunk === null) {
            stdin.push(null);
            return;
          }
          stdin.push(Buffer.from(chunk));
        }
      } catch (error) {
        stdin.destroy(error);
      }
    };
    const resume = stdin.resume;
    stdin.resume = function () {
      if (!pumping) {
        pumping = true;
        pump();
      }
      return resume.call(this);
    };
  } else {
    stdin = {
      on: () => stdin, once: () => stdin, removeListener: () => stdin,
      read: () => null, resume: () => stdin, pause: () => stdin, setEncoding: () => stdin,
    };
  }
  stdin.isTTY = false;
  stdin.fd = 0;
  Object.defineProperty(proc, "stdin", { value: stdin, enumerable: true, configurable: true });
  proc.openStdin = function () { if (stdin.resume) stdin.resume(); return stdin; };

  // child_process.fork IPC. Lumen has no extra inherited fd, so fork reserves stdin and frames
  // messages with a control-prefixed JSON line; ordinary stdout lines remain ordinary stdout.
  const IPC_PREFIX = "\x1eLUMEN_IPC ";
  let forkIpcStarted = false;
  let forkConnected = false;
  let forkPending = "";
  const isForkChild = () => proc.env && proc.env.LUMEN_FORK_IPC === "1";
  const startForkIpc = () => {
    if (forkIpcStarted || !isForkChild()) return;
    forkIpcStarted = true;
    forkConnected = true;
    stdin.on("data", (chunk) => {
      forkPending += Buffer.from(chunk).toString("utf8");
      for (;;) {
        const newline = forkPending.indexOf("\n");
        if (newline < 0) break;
        const line = forkPending.slice(0, newline);
        forkPending = forkPending.slice(newline + 1);
        if (!line.startsWith(IPC_PREFIX)) continue;
        try {
          proc.emit("message", JSON.parse(line.slice(IPC_PREFIX.length)), null);
        } catch (error) {
          proc.emit("error", error);
        }
      }
    });
    stdin.on("end", () => {
      if (!forkConnected) return;
      forkConnected = false;
      proc.emit("disconnect");
    });
  };
  const forkSend = (message, sendHandle, options, callback) => {
    if (typeof sendHandle === "function") callback = sendHandle;
    else if (typeof options === "function") callback = options;
    if (sendHandle != null && typeof sendHandle !== "function") {
      const error = new Error("child_process.fork handle transfer is not supported in lumen");
      if (callback) queueMicrotask(() => callback(error)); else throw error;
      return false;
    }
    startForkIpc();
    if (!forkConnected) return false;
    try {
      proc.stdout.write(IPC_PREFIX + JSON.stringify(message === undefined ? null : message) + "\n");
      if (callback) queueMicrotask(() => callback(null));
      return true;
    } catch (error) {
      if (callback) queueMicrotask(() => callback(error)); else throw error;
      return false;
    }
  };
  Object.defineProperty(proc, "send", {
    enumerable: true,
    configurable: true,
    get() {
      if (!isForkChild()) return undefined;
      startForkIpc();
      return forkSend;
    },
  });
  Object.defineProperty(proc, "connected", {
    enumerable: true,
    configurable: true,
    get() { return isForkChild() ? forkConnected : undefined; },
  });
  proc.disconnect = function () {
    if (!isForkChild() || !forkConnected) return;
    forkConnected = false;
    queueMicrotask(() => proc.emit("disconnect"));
  };
  const processOn = proc.on;
  proc.on = proc.addListener = function (event, listener) {
    if (event === "message" || event === "disconnect") startForkIpc();
    return processOn.call(this, event, listener);
  };

  // Semi-internal underscore surface Node exposes as own keys. Honest no-ops / empty collectors;
  // _rawDebug writes straight to stderr (its one real behavior).
  proc._getActiveHandles = () => [];
  proc._getActiveRequests = () => [];
  proc._rawDebug = (...a) => { proc.stderr.write(a.join(" ") + "\n"); };
  proc._tickCallback = () => {};
  proc._fatalException = () => false;
  proc._exiting = false;
  proc._kill = (pid, sig) => proc.kill(pid, sig);
  proc._eval = undefined;
  proc._print_eval = false;
  proc._preload_modules = [];
  proc._debugProcess = () => {};
  proc._debugEnd = () => {};
  proc._startProfilerIdleNotifier = () => {};
  proc._stopProfilerIdleNotifier = () => {};
}

// The `process` global as an importable module (`import process from 'node:process'`).
__builtins.set("process", globalThis.process);

// ---- node:tls ---------------------------------------------------------------------------------
// TLS cannot be built on std alone (no crypto/handshake stack) and lumen takes no third-party
// crate, so — like node:net's sockets and fetch's https — anything that establishes a TLS
// connection (connect/createServer/TLSSocket) throws. The pure pieces are real: the constants,
// checkServerIdentity (RFC 6125 hostname/SAN matching), convertALPNProtocols (the length-prefixed
// wire encoding), and getCiphers (the OpenSSL cipher enumeration).
{
  const notSupported = function () {
    throw new Error("node:tls is not supported in lumen (TLS requires a crypto stack)");
  };

  // Real: leftmost-label wildcard match (RFC 6125). "*.example.com" matches one label only.
  function matchHostname(host, pattern) {
    if (pattern === host) return true;
    if (!pattern.startsWith("*.")) return false;
    const dot = host.indexOf(".");
    if (dot < 0) return false;
    return host.slice(dot + 1) === pattern.slice(2);
  }

  // Real hostname/IP verification against a peer certificate's subjectAltName (falling back to the
  // subject CN), returning undefined on success or an ERR_TLS_CERT_ALTNAME_INVALID Error on failure
  // — the exact shape callers (and Node's own https client) check.
  function checkServerIdentity(hostname, cert) {
    hostname = String(hostname);
    const net = __builtins.get("net");
    const dnsNames = [];
    const ips = [];
    if (cert && cert.subjectaltname) {
      for (const part of cert.subjectaltname.split(", ")) {
        const idx = part.indexOf(":");
        const kind = part.slice(0, idx);
        const val = part.slice(idx + 1);
        if (kind === "DNS") dnsNames.push(val);
        else if (kind === "IP Address") ips.push(val);
      }
    }
    if (dnsNames.length === 0 && ips.length === 0 && cert && cert.subject && cert.subject.CN) {
      const cns = Array.isArray(cert.subject.CN) ? cert.subject.CN : [cert.subject.CN];
      for (const cn of cns) dnsNames.push(cn);
    }
    let valid;
    if (net.isIP(hostname)) valid = ips.includes(hostname);
    else { const host = hostname.toLowerCase(); valid = dnsNames.some((n) => matchHostname(host, n.toLowerCase())); }
    if (valid) return undefined;

    const altStr = cert && cert.subjectaltname
      ? cert.subjectaltname
      : [...dnsNames.map((d) => `DNS:${d}`), ...ips.map((ip) => `IP Address:${ip}`)].join(", ");
    const err = new Error(`Hostname/IP does not match certificate's altnames: Host: ${hostname}. is not in the cert's altnames: ${altStr}`);
    err.name = "Error";
    err.reason = `Host: ${hostname}. is not in the cert's altnames: ${altStr}`;
    err.host = hostname;
    err.cert = cert;
    err.code = "ERR_TLS_CERT_ALTNAME_INVALID";
    return err;
  }

  // Real: encode ALPN protocol names as the length-prefixed wire format; mutate `context.ALPNProtocols`.
  function convertALPNProtocols(protocols, context) {
    let buf;
    if (Array.isArray(protocols)) {
      let total = 0;
      const encoded = protocols.map((p) => Buffer.from(String(p), "utf8"));
      for (const e of encoded) total += 1 + e.length;
      buf = Buffer.alloc(total);
      let off = 0;
      for (const e of encoded) {
        buf[off++] = e.length;
        for (let i = 0; i < e.length; i++) buf[off + i] = e[i];
        off += e.length;
      }
    } else if (protocols instanceof Uint8Array) {
      buf = Buffer.from(protocols);
    } else {
      buf = Buffer.alloc(0);
    }
    if (context) context.ALPNProtocols = buf;
  }

  // The OpenSSL cipher enumeration (pure data). Real list, lowercased, as Node returns it.
  const CIPHERS = ["aes128-gcm-sha256", "aes128-sha", "aes128-sha256", "aes256-gcm-sha384", "aes256-sha", "aes256-sha256", "dhe-psk-aes128-cbc-sha", "dhe-psk-aes128-cbc-sha256", "dhe-psk-aes128-gcm-sha256", "dhe-psk-aes256-cbc-sha", "dhe-psk-aes256-cbc-sha384", "dhe-psk-aes256-gcm-sha384", "dhe-psk-chacha20-poly1305", "dhe-rsa-aes128-gcm-sha256", "dhe-rsa-aes128-sha", "dhe-rsa-aes128-sha256", "dhe-rsa-aes256-gcm-sha384", "dhe-rsa-aes256-sha", "dhe-rsa-aes256-sha256", "dhe-rsa-chacha20-poly1305", "ecdhe-ecdsa-aes128-gcm-sha256", "ecdhe-ecdsa-aes128-sha", "ecdhe-ecdsa-aes128-sha256", "ecdhe-ecdsa-aes256-gcm-sha384", "ecdhe-ecdsa-aes256-sha", "ecdhe-ecdsa-aes256-sha384", "ecdhe-ecdsa-chacha20-poly1305", "ecdhe-psk-aes128-cbc-sha", "ecdhe-psk-aes128-cbc-sha256", "ecdhe-psk-aes256-cbc-sha", "ecdhe-psk-aes256-cbc-sha384", "ecdhe-psk-chacha20-poly1305", "ecdhe-rsa-aes128-gcm-sha256", "ecdhe-rsa-aes128-sha", "ecdhe-rsa-aes128-sha256", "ecdhe-rsa-aes256-gcm-sha384", "ecdhe-rsa-aes256-sha", "ecdhe-rsa-aes256-sha384", "ecdhe-rsa-chacha20-poly1305", "psk-aes128-cbc-sha", "psk-aes128-cbc-sha256", "psk-aes128-gcm-sha256", "psk-aes256-cbc-sha", "psk-aes256-cbc-sha384", "psk-aes256-gcm-sha384", "psk-chacha20-poly1305", "rsa-psk-aes128-cbc-sha", "rsa-psk-aes128-cbc-sha256", "rsa-psk-aes128-gcm-sha256", "rsa-psk-aes256-cbc-sha", "rsa-psk-aes256-cbc-sha384", "rsa-psk-aes256-gcm-sha384", "rsa-psk-chacha20-poly1305", "srp-aes-128-cbc-sha", "srp-aes-256-cbc-sha", "srp-rsa-aes-128-cbc-sha", "srp-rsa-aes-256-cbc-sha", "tls_aes_128_ccm_8_sha256", "tls_aes_128_ccm_sha256", "tls_aes_128_gcm_sha256", "tls_aes_256_gcm_sha384", "tls_chacha20_poly1305_sha256"];

  __builtins.set("tls", {
    // socket-dependent (honest throwing stubs)
    connect: notSupported,
    createServer: notSupported,
    TLSSocket: notSupported,
    Server: notSupported,
    // createSecurePair is deprecated in Node and only made sense atop a real socket pair.
    createSecurePair: function () { throw new Error("tls.createSecurePair is deprecated and not supported in lumen"); },
    // context objects: shells (no OpenSSL context underneath, but constructing is inert)
    createSecureContext: () => ({}),
    SecureContext: function () {},
    // real, transport-free
    checkServerIdentity,
    convertALPNProtocols,
    getCiphers: () => CIPHERS.slice(),
    // No CA trust store is bundled, so getCACertificates yields an empty set (not fake certs).
    getCACertificates: () => [],
    rootCertificates: Object.freeze([]),
    // real constants
    CLIENT_RENEG_LIMIT: 3,
    CLIENT_RENEG_WINDOW: 600,
    DEFAULT_CIPHERS: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA",
    DEFAULT_ECDH_CURVE: "auto",
    DEFAULT_MIN_VERSION: "TLSv1.2",
    DEFAULT_MAX_VERSION: "TLSv1.3",
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
