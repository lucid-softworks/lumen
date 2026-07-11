// node:stream — a minimal but faithful streams implementation over EventEmitter. Enough for
// node:http request/response bodies and the body-parser/raw-body consumers: flowing-mode Readable
// (push/'data'/'end', pause/resume, pipe, setEncoding, async iteration), Writable (write/end,
// 'finish'/'drain'), and Duplex/Transform/PassThrough composed from them. Bodies here are already
// fully buffered by the http bridge, so there is no real backpressure — write() always accepts.
// Not implemented: object mode highWaterMark accounting, cork batching, byte-exact read(n).

const EventEmitter = __builtins.get("events");

class Stream extends EventEmitter {}

class Readable extends Stream {
  constructor(opts = {}) {
    super();
    this._readableState = { flowing: null, ended: false, endEmitted: false, buffer: [], encoding: null, destroyed: false, reading: false, errored: null };
    this.readable = true;
    if (opts.encoding) this.setEncoding(opts.encoding);
    if (typeof opts.read === "function") this._read = opts.read;
  }
  _read() {}

  push(chunk) {
    const state = this._readableState;
    if (chunk === null) {
      state.ended = true;
      maybeEmitEnd(this);
      return false;
    }
    if (state.destroyed) return false;
    if (state.encoding && chunk instanceof Uint8Array) chunk = Buffer.from(chunk).toString(state.encoding);
    if (state.flowing) this.emit("data", chunk);
    else state.buffer.push(chunk);
    return true;
  }

  read() {
    const state = this._readableState;
    if (state.buffer.length === 0) return null;
    const chunk = state.buffer.shift();
    maybeEmitEnd(this);
    return chunk;
  }

  setEncoding(enc) {
    this._readableState.encoding = String(enc).toLowerCase().replace("-", "");
    return this;
  }

  on(ev, fn) {
    const res = super.on(ev, fn);
    if (ev === "data") {
      if (this._readableState.flowing !== false) this.resume();
    } else if (ev === "readable") {
      // Not fully modeled; resume so data still flows to any 'data'/pipe consumers.
    }
    return res;
  }
  addListener(ev, fn) { return this.on(ev, fn); }

  resume() {
    const state = this._readableState;
    if (state.flowing) return this;
    state.flowing = true;
    queueMicrotask(() => flow(this));
    return this;
  }
  pause() {
    this._readableState.flowing = false;
    return this;
  }
  isPaused() { return this._readableState.flowing === false; }

  pipe(dest, opts = {}) {
    this.on("data", (chunk) => {
      const ok = dest.write(chunk);
      if (ok === false && typeof this.pause === "function") {
        this.pause();
        dest.once("drain", () => this.resume());
      }
    });
    if (opts.end !== false) this.on("end", () => dest.end());
    this.on("error", (err) => { if (dest.destroy) dest.destroy(err); });
    if (dest.emit) dest.emit("pipe", this);
    return dest;
  }
  unpipe() { this.pause(); return this; }

  destroy(err) {
    const state = this._readableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    if (err) { state.errored = err; if (this._writableState) this._writableState.errored = err; }
    this.readable = false;
    queueMicrotask(() => {
      if (err) this.emit("error", err);
      this.emit("close");
    });
    return this;
  }

  [Symbol.asyncIterator]() {
    const self = this;
    const queue = [];
    let done = false;
    let error = null;
    let pending = null;
    const settle = () => {
      if (!pending) return;
      if (error) { const p = pending; pending = null; p.reject(error); }
      else if (queue.length) { const p = pending; pending = null; p.resolve({ value: queue.shift(), done: false }); }
      else if (done) { const p = pending; pending = null; p.resolve({ value: undefined, done: true }); }
    };
    self.on("data", (c) => { queue.push(c); settle(); });
    self.on("end", () => { done = true; settle(); });
    self.on("error", (e) => { error = e; settle(); });
    return {
      next() {
        if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
        if (error) return Promise.reject(error);
        if (done) return Promise.resolve({ value: undefined, done: true });
        return new Promise((resolve, reject) => { pending = { resolve, reject }; });
      },
      return() { self.destroy(); return Promise.resolve({ value: undefined, done: true }); },
      [Symbol.asyncIterator]() { return this; },
    };
  }
}

function flow(stream) {
  const state = stream._readableState;
  while (state.flowing && state.buffer.length) stream.emit("data", state.buffer.shift());
  maybeEmitEnd(stream);
}

function maybeEmitEnd(stream) {
  const state = stream._readableState;
  if (state.ended && !state.endEmitted && state.buffer.length === 0 && state.flowing !== false) {
    state.endEmitted = true;
    state.flowing = false;
    stream.readable = false;
    queueMicrotask(() => stream.emit("end"));
  }
}

class Writable extends Stream {
  constructor(opts = {}) {
    super();
    // `tail` serializes _write calls so writes complete in order and end() can wait for them
    // (an async _write — e.g. a subprocess stdin write — must finish before _final closes it).
    this._writableState = { ended: false, finished: false, destroyed: false, corked: 0, errored: null, tail: Promise.resolve() };
    this.writable = true;
    if (typeof opts.write === "function") this._write = opts.write;
    if (typeof opts.final === "function") this._final = opts.final;
  }
  _write(chunk, encoding, cb) { cb(); }

  write(chunk, encoding, cb) {
    if (typeof encoding === "function") { cb = encoding; encoding = null; }
    const state = this._writableState;
    if (state.ended) {
      const err = new Error("write after end");
      if (cb) queueMicrotask(() => cb(err)); else this.emit("error", err);
      return false;
    }
    state.tail = state.tail.then(
      () =>
        new Promise((resolve) => {
          this._write(chunk, encoding, (err) => {
            if (err) this.emit("error", err);
            if (cb) cb(err);
            resolve();
          });
        }),
    );
    return true;
  }

  end(chunk, encoding, cb) {
    if (typeof chunk === "function") { cb = chunk; chunk = null; }
    else if (typeof encoding === "function") { cb = encoding; encoding = null; }
    const state = this._writableState;
    if (chunk != null) this.write(chunk, encoding);
    state.ended = true;
    // Wait for all queued writes to drain, then run _final (e.g. close the child's stdin).
    state.tail.then(() => {
      if (state.finished) return;
      const done = () => { state.finished = true; if (cb) cb(); this.emit("finish"); };
      if (this._final) this._final((err) => { if (err) this.emit("error", err); else done(); });
      else done();
    });
    return this;
  }

  cork() { this._writableState.corked++; }
  uncork() { if (this._writableState.corked) this._writableState.corked--; }
  setDefaultEncoding() { return this; }

  destroy(err) {
    const state = this._writableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    if (err) { state.errored = err; if (this._readableState) this._readableState.errored = err; }
    this.writable = false;
    queueMicrotask(() => { if (err) this.emit("error", err); this.emit("close"); });
    return this;
  }
}

// Duplex: readable + writable. Compose by inheriting Readable and copying Writable's methods.
class Duplex extends Readable {
  constructor(opts = {}) {
    super(opts);
    // Mirror Writable's state (including `tail`, which write()/end() chain on to serialize writes).
    this._writableState = { ended: false, finished: false, destroyed: false, corked: 0, errored: null, tail: Promise.resolve() };
    this.writable = true;
    if (typeof opts.write === "function") this._write = opts.write;
    if (typeof opts.final === "function") this._final = opts.final;
  }
}
for (const m of ["_write", "write", "end", "cork", "uncork", "setDefaultEncoding"]) {
  Duplex.prototype[m] = Writable.prototype[m];
}

class Transform extends Duplex {
  constructor(opts = {}) {
    super(opts);
    if (typeof opts.transform === "function") this._transform = opts.transform;
    if (typeof opts.flush === "function") this._flush = opts.flush;
  }
  _transform(chunk, encoding, cb) { cb(null, chunk); }
  _write(chunk, encoding, cb) {
    this._transform(chunk, encoding, (err, data) => {
      if (err) return cb(err);
      if (data != null) this.push(data);
      cb();
    });
  }
  _final(cb) {
    if (this._flush) this._flush((err, data) => { if (data != null) this.push(data); this.push(null); cb(err); });
    else { this.push(null); cb(); }
  }
}

class PassThrough extends Transform {
  _transform(chunk, encoding, cb) { cb(null, chunk); }
}

// Readable.from(iterable) — async or sync.
Readable.from = function (iterable, opts) {
  const r = new Readable(opts);
  (async () => {
    try {
      for await (const chunk of iterable) r.push(chunk);
      r.push(null);
    } catch (err) { r.destroy(err); }
  })();
  return r;
};

function prematureClose() {
  const e = new Error("Premature close");
  e.code = "ERR_STREAM_PREMATURE_CLOSE";
  return e;
}

// eos/finished: invoke cb exactly once when the stream is fully done. For a duplex/transform we
// wait for BOTH the readable side ('end') and the writable side ('finish'); a bare 'close' (from
// destroy() without an error) before completion is a premature close, matching Node.
function finished(stream, opts, cb) {
  if (typeof opts === "function") { cb = opts; opts = {}; }
  opts = opts || {};
  const rState = stream._readableState;
  const wState = stream._writableState;
  const wantReadable = opts.readable !== false && !!rState;
  const wantWritable = opts.writable !== false && !!wState;
  let readableDone = !wantReadable || rState.endEmitted;
  let writableDone = !wantWritable || wState.finished;
  let called = false;

  const cleanup = () => {
    if (!stream.removeListener) return;
    stream.removeListener("end", onend);
    stream.removeListener("finish", onfinish);
    stream.removeListener("close", onclose);
    stream.removeListener("error", onerror);
  };
  const settle = (err) => {
    if (called) return;
    called = true;
    cleanup();
    queueMicrotask(() => cb(err || null));
  };
  const check = () => { if (readableDone && writableDone) settle(); };
  const onend = () => { readableDone = true; check(); };
  const onfinish = () => { writableDone = true; check(); };
  const onerror = (err) => settle(err || prematureClose());
  const onclose = () => { if (readableDone && writableDone) settle(); else settle(prematureClose()); };

  stream.on("end", onend);
  stream.on("finish", onfinish);
  stream.on("close", onclose);
  stream.on("error", onerror);
  // Already errored/destroyed before we attached — fire on the next tick.
  if ((rState && rState.errored) || (wState && wState.errored)) {
    settle((rState && rState.errored) || (wState && wState.errored));
  } else {
    check();
  }
  return cleanup;
}

function isNodeStream(x) {
  return !!(x && typeof x === "object" && (x._readableState || x._writableState) && typeof x.on === "function");
}

// pipeline(src, ...transforms, dst[, cb]) with real error propagation and cleanup. Non-stream
// sources/transforms (async iterables, arrays, async generator functions) are adapted through
// Readable.from, so `pipeline(asyncIterable, transform, writable, cb)` works like Node's.
function pipeline(...args) {
  const cb = typeof args[args.length - 1] === "function" ? args.pop() : () => {};
  const parts = args.flat();
  if (parts.length < 2) throw new Error("pipeline requires at least 2 streams");

  const nodes = [];
  let done = false;
  const finish = (err) => {
    if (done) return;
    done = true;
    if (err) for (const n of nodes) if (isNodeStream(n) && !isDestroyed(n)) n.destroy(err);
    queueMicrotask(() => cb(err || null));
  };

  let current = parts[0];
  if (!isNodeStream(current)) current = Readable.from(current);
  nodes.push(current);

  for (let i = 1; i < parts.length; i++) {
    const part = parts[i];
    const isLast = i === parts.length - 1;
    if (typeof part === "function") {
      // async generator transform / async destination consuming the previous stream.
      let result;
      try { result = part(current); } catch (e) { finish(e); return current; }
      if (isLast && !(result && typeof result[Symbol.asyncIterator] === "function")) {
        Promise.resolve(result).then(() => finish(null), (e) => finish(e));
        current = null;
        break;
      }
      current = Readable.from(result);
      nodes.push(current);
    } else {
      current.pipe(part);
      nodes.push(part);
      current = part;
    }
  }

  for (const n of nodes) if (isNodeStream(n)) n.on("error", (e) => finish(e));
  if (current) finished(current, (err) => finish(err));
  return current || nodes[nodes.length - 1];
}

const streamPromises = {
  finished(stream, opts) {
    return new Promise((resolve, reject) => finished(stream, opts || {}, (err) => (err ? reject(err) : resolve())));
  },
  pipeline(...args) {
    return new Promise((resolve, reject) => pipeline(...args, (err) => (err ? reject(err) : resolve())));
  },
};

// --- state helpers, matching Node's public predicates ---
function destroy(stream, err) { if (stream && typeof stream.destroy === "function") stream.destroy(err); return stream; }
function isDestroyed(stream) {
  const r = stream && stream._readableState, w = stream && stream._writableState;
  return !!((r && r.destroyed) || (w && w.destroyed));
}
function isErrored(stream) {
  const r = stream && stream._readableState, w = stream && stream._writableState;
  return !!((r && r.errored) || (w && w.errored) || (stream && stream.errored));
}
function isReadable(stream) {
  const r = stream && stream._readableState;
  if (!r) return !!(stream && stream.readable === true);
  return !r.ended && !r.endEmitted && !r.destroyed && stream.readable !== false;
}
function isWritable(stream) {
  const w = stream && stream._writableState;
  if (!w) return !!(stream && stream.writable === true);
  return !w.ended && !w.finished && !w.destroyed && stream.writable !== false;
}
function isDisturbed(stream) {
  const r = stream && stream._readableState;
  return !!(r && (r.flowing !== null || r.endEmitted || r.ended));
}

let defaultHighWaterMark = 65536;
let defaultHighWaterMarkObjectMode = 16;
function getDefaultHighWaterMark(objectMode) { return objectMode ? defaultHighWaterMarkObjectMode : defaultHighWaterMark; }
function setDefaultHighWaterMark(objectMode, value) {
  if (objectMode) defaultHighWaterMarkObjectMode = value; else defaultHighWaterMark = value;
}

function _isArrayBufferView(x) { return ArrayBuffer.isView(x); }
function _isUint8Array(x) { return x instanceof Uint8Array; }
function _uint8ArrayToBuffer(chunk) { return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength); }

// addAbortSignal(signal, stream): destroy `stream` with an ABORT_ERR when `signal` aborts.
function addAbortSignal(signal, stream) {
  if (!signal || typeof signal.addEventListener !== "function") return stream;
  const abort = () => {
    let err = signal.reason;
    if (!err) { err = new Error("The operation was aborted"); err.code = "ABORT_ERR"; err.name = "AbortError"; }
    if (typeof stream.destroy === "function") stream.destroy(err);
  };
  if (signal.aborted) queueMicrotask(abort);
  else signal.addEventListener("abort", abort, { once: true });
  return stream;
}

// duplexPair(): two crossed Duplexes — writes to one surface as readable data on the other.
function duplexPair() {
  let a, b;
  a = new Duplex({ read() {}, write(chunk, enc, cb) { b.push(chunk); cb(); }, final(cb) { b.push(null); cb(); } });
  b = new Duplex({ read() {}, write(chunk, enc, cb) { a.push(chunk); cb(); }, final(cb) { a.push(null); cb(); } });
  return [a, b];
}

// compose(...streams): fuse a chain into a single Duplex (write → head, read ← tail).
function compose(...streams) {
  streams = streams.flat().filter((s) => s != null);
  if (streams.length === 0) return new PassThrough();
  const head = streams[0];
  const tail = streams[streams.length - 1];
  for (let i = 0; i < streams.length - 1; i++) streams[i].pipe(streams[i + 1]);
  const d = new Duplex({
    read() {},
    write(chunk, enc, cb) { head.write(chunk, enc); cb(); },
    final(cb) { head.end(); cb(); },
  });
  if (tail.on) {
    tail.on("data", (c) => d.push(c));
    tail.on("end", () => d.push(null));
  }
  for (const s of streams) if (s.on) s.on("error", (e) => d.destroy(e));
  return d;
}

Stream.Readable = Readable;
Stream.Writable = Writable;
Stream.Duplex = Duplex;
Stream.Transform = Transform;
Stream.PassThrough = PassThrough;
Stream.Stream = Stream;
Stream.finished = finished;
Stream.pipeline = pipeline;
Stream.promises = streamPromises;
Stream.destroy = destroy;
Stream.addAbortSignal = addAbortSignal;
Stream.compose = compose;
Stream.duplexPair = duplexPair;
Stream.isDestroyed = isDestroyed;
Stream.isErrored = isErrored;
Stream.isReadable = isReadable;
Stream.isWritable = isWritable;
Stream.isDisturbed = isDisturbed;
Stream.getDefaultHighWaterMark = getDefaultHighWaterMark;
Stream.setDefaultHighWaterMark = setDefaultHighWaterMark;
Stream._isArrayBufferView = _isArrayBufferView;
Stream._isUint8Array = _isUint8Array;
Stream._uint8ArrayToBuffer = _uint8ArrayToBuffer;

__builtins.set("stream", Stream);

// node:stream/web — the WHATWG streams. lumen-web already ships these as globals (spec-correct
// pull/backpressure, BYOB, and CompressionStream/DecompressionStream over the shared DEFLATE
// codec), so re-export the exact same constructors by identity.
const webStreams = {};
for (const name of [
  "ReadableStream", "ReadableStreamDefaultReader", "ReadableStreamBYOBReader",
  "ReadableStreamDefaultController", "ReadableByteStreamController", "ReadableStreamBYOBRequest",
  "WritableStream", "WritableStreamDefaultWriter", "WritableStreamDefaultController",
  "TransformStream", "TransformStreamDefaultController",
  "ByteLengthQueuingStrategy", "CountQueuingStrategy",
  "TextEncoderStream", "TextDecoderStream", "CompressionStream", "DecompressionStream",
]) {
  if (typeof globalThis[name] !== "undefined") webStreams[name] = globalThis[name];
}
__builtins.set("stream/web", webStreams);
