// A minimal but functional `ReadableStream` — default readers only (no BYOB/byte streams, no
// `tee()`, no piping, no `WritableStream`/`TransformStream`). Chunks are queued and served from the
// queue, pulling from the underlying source on demand. lumen buffers request/response bodies, so a
// stream backed by a *synchronous* source can also be drained synchronously (see `_readSync`, used
// by the Request/Response body interop in fetch.js).

const kStream = Symbol("stream");

class ReadableStreamDefaultController {
  constructor(stream) {
    this[kStream] = stream;
  }
  enqueue(chunk) {
    const s = this[kStream];
    if (s._state !== "readable") throw new TypeError("stream is not in a readable state");
    s._chunks.push(chunk);
    s._fulfillReads();
  }
  close() {
    const s = this[kStream];
    if (s._state !== "readable") return;
    s._state = "closed";
    s._fulfillReads();
  }
  error(e) {
    const s = this[kStream];
    if (s._state !== "readable") return;
    s._state = "errored";
    s._storedError = e;
    s._chunks = [];
    s._fulfillReads();
  }
  get desiredSize() {
    const s = this[kStream];
    if (s._state === "errored") return null;
    if (s._state === "closed") return 0;
    return 1; // backpressure is not enforced (bodies are already buffered)
  }
}

class ReadableStreamDefaultReader {
  constructor(stream) {
    if (!(stream instanceof ReadableStream)) throw new TypeError("not a ReadableStream");
    if (stream.locked) throw new TypeError("stream is already locked to a reader");
    this[kStream] = stream;
    stream._reader = this;
    this._readRequests = []; // pending { resolve, reject }
    this._closedPromise = null; // created lazily so an un-awaited error isn't an unhandled rejection
    this._closedResolve = null;
    this._closedReject = null;
  }
  read() {
    const s = this[kStream];
    if (!s) return Promise.reject(new TypeError("reader has been released"));
    if (s._chunks.length) return Promise.resolve({ value: s._chunks.shift(), done: false });
    if (s._state === "errored") return Promise.reject(s._storedError);
    if (s._state === "closed") return Promise.resolve({ value: undefined, done: true });
    s._maybePull();
    if (s._chunks.length) return Promise.resolve({ value: s._chunks.shift(), done: false });
    if (s._state === "errored") return Promise.reject(s._storedError);
    if (s._state === "closed") return Promise.resolve({ value: undefined, done: true });
    return new Promise((resolve, reject) => this._readRequests.push({ resolve, reject }));
  }
  // Non-standard internal: get one chunk without awaiting, for synchronous body draining. Returns
  // `{ pending: true }` when the source can't produce synchronously.
  _readSync() {
    const s = this[kStream];
    if (!s) throw new TypeError("reader has been released");
    if (s._chunks.length) return { value: s._chunks.shift(), done: false };
    if (s._state === "errored") throw s._storedError;
    if (s._state === "closed") return { value: undefined, done: true };
    s._maybePull();
    if (s._chunks.length) return { value: s._chunks.shift(), done: false };
    if (s._state === "errored") throw s._storedError;
    if (s._state === "closed") return { value: undefined, done: true };
    return { pending: true };
  }
  releaseLock() {
    const s = this[kStream];
    if (!s) return;
    if (this._readRequests.length) throw new TypeError("cannot release a reader with pending reads");
    s._reader = null;
    this[kStream] = null;
  }
  cancel(reason) {
    const s = this[kStream];
    if (!s) return Promise.reject(new TypeError("reader has been released"));
    // Cancel goes through the stream, but the reader keeps the lock per spec; drop it here so the
    // simple buffered model doesn't wedge.
    s._reader = null;
    this[kStream] = null;
    return s.cancel(reason);
  }
  get closed() {
    if (!this._closedPromise) {
      this._closedPromise = new Promise((res, rej) => {
        this._closedResolve = res;
        this._closedReject = rej;
      });
      const s = this[kStream];
      if (!s || s._state === "closed") this._closedResolve(undefined);
      else if (s._state === "errored") this._closedReject(s._storedError);
    }
    return this._closedPromise;
  }
}

class ReadableStream {
  constructor(underlyingSource = {}, _strategy = {}) {
    underlyingSource = underlyingSource || {};
    if (underlyingSource.type === "bytes") throw new TypeError("byte streams are not supported");
    this._chunks = [];
    this._state = "readable"; // 'readable' | 'closed' | 'errored'
    this._storedError = undefined;
    this._reader = null;
    this._source = underlyingSource;
    this._pulling = false;
    this._controller = new ReadableStreamDefaultController(this);
    const start = underlyingSource.start;
    if (typeof start === "function") start.call(underlyingSource, this._controller); // sync start only
  }
  _maybePull() {
    if (this._state !== "readable" || this._pulling) return;
    const pull = this._source.pull;
    if (typeof pull !== "function") return;
    this._pulling = true;
    try {
      pull.call(this._source, this._controller);
    } finally {
      this._pulling = false;
    }
  }
  _fulfillReads() {
    const reader = this._reader;
    if (!reader) return;
    while (reader._readRequests.length) {
      if (this._chunks.length) {
        reader._readRequests.shift().resolve({ value: this._chunks.shift(), done: false });
      } else if (this._state === "closed") {
        reader._readRequests.shift().resolve({ value: undefined, done: true });
      } else if (this._state === "errored") {
        reader._readRequests.shift().reject(this._storedError);
      } else {
        break;
      }
    }
    if (this._state === "closed" && reader._closedResolve) reader._closedResolve(undefined);
    else if (this._state === "errored" && reader._closedReject) reader._closedReject(this._storedError);
  }
  getReader(options = {}) {
    if (options && options.mode === "byob") throw new TypeError("BYOB readers are not supported");
    return new ReadableStreamDefaultReader(this);
  }
  get locked() {
    return this._reader !== null;
  }
  cancel(reason) {
    if (this.locked) return Promise.reject(new TypeError("cannot cancel a locked stream"));
    this._chunks = [];
    if (this._state === "readable") this._state = "closed";
    const cancel = this._source.cancel;
    try {
      if (typeof cancel === "function") cancel.call(this._source, reason);
    } catch (e) {
      return Promise.reject(e);
    }
    return Promise.resolve(undefined);
  }
  values(options = {}) {
    const reader = this.getReader();
    const preventCancel = !!(options && options.preventCancel);
    return {
      next() {
        return reader.read().then((r) => {
          if (r.done) reader.releaseLock();
          return r;
        });
      },
      return(value) {
        if (!preventCancel) reader.cancel(value);
        else reader.releaseLock();
        return Promise.resolve({ value, done: true });
      },
      [Symbol.asyncIterator]() {
        return this;
      },
    };
  }
}
ReadableStream.prototype[Symbol.asyncIterator] = ReadableStream.prototype.values;

globalThis.ReadableStream = ReadableStream;
