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
    this._readableState = { flowing: null, ended: false, endEmitted: false, buffer: [], encoding: null, destroyed: false, reading: false };
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
    this._writableState = { ended: false, finished: false, destroyed: false, corked: 0 };
    this.writable = true;
    if (typeof opts.write === "function") this._write = opts.write;
    if (typeof opts.final === "function") this._final = opts.final;
  }
  _write(chunk, encoding, cb) { cb(); }

  write(chunk, encoding, cb) {
    if (typeof encoding === "function") { cb = encoding; encoding = null; }
    if (this._writableState.ended) {
      const err = new Error("write after end");
      if (cb) queueMicrotask(() => cb(err)); else this.emit("error", err);
      return false;
    }
    this._write(chunk, encoding, (err) => {
      if (err) this.emit("error", err);
      else if (cb) cb();
    });
    return true;
  }

  end(chunk, encoding, cb) {
    if (typeof chunk === "function") { cb = chunk; chunk = null; }
    else if (typeof encoding === "function") { cb = encoding; encoding = null; }
    const finish = () => {
      const state = this._writableState;
      if (state.finished) return;
      state.ended = true;
      const done = () => { state.finished = true; if (cb) cb(); this.emit("finish"); };
      if (this._final) this._final((err) => { if (err) this.emit("error", err); else done(); });
      else done();
    };
    if (chunk != null) this.write(chunk, encoding, finish);
    else finish();
    return this;
  }

  cork() { this._writableState.corked++; }
  uncork() { if (this._writableState.corked) this._writableState.corked--; }
  setDefaultEncoding() { return this; }

  destroy(err) {
    const state = this._writableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    this.writable = false;
    queueMicrotask(() => { if (err) this.emit("error", err); this.emit("close"); });
    return this;
  }
}

// Duplex: readable + writable. Compose by inheriting Readable and copying Writable's methods.
class Duplex extends Readable {
  constructor(opts = {}) {
    super(opts);
    this._writableState = { ended: false, finished: false, destroyed: false, corked: 0 };
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

function finished(stream, opts, cb) {
  if (typeof opts === "function") { cb = opts; opts = {}; }
  let called = false;
  const done = (err) => { if (called) return; called = true; cb(err); };
  stream.on("end", () => done());
  stream.on("finish", () => done());
  stream.on("close", () => done());
  stream.on("error", (err) => done(err));
}

function pipeline(...args) {
  const cb = typeof args[args.length - 1] === "function" ? args.pop() : () => {};
  const streams = args.flat();
  for (let i = 0; i < streams.length - 1; i++) streams[i].pipe(streams[i + 1]);
  const last = streams[streams.length - 1];
  finished(last, (err) => cb(err));
  return last;
}

Stream.Readable = Readable;
Stream.Writable = Writable;
Stream.Duplex = Duplex;
Stream.Transform = Transform;
Stream.PassThrough = PassThrough;
Stream.Stream = Stream;
Stream.finished = finished;
Stream.pipeline = pipeline;

__builtins.set("stream", Stream);
