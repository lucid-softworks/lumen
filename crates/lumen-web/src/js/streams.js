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

// A byte-stream controller (`type: 'bytes'`). Chunks are stored as Uint8Arrays; a default reader
// yields them whole, a BYOB reader copies into caller-provided views. `autoAllocateChunkSize`
// enables the byobRequest zero-copy pull path.
class ReadableByteStreamController {
  constructor(stream) {
    this[kStream] = stream;
    this._byobRequest = null;
  }
  enqueue(chunk) {
    const s = this[kStream];
    if (s._state !== "readable") throw new TypeError("stream is not in a readable state");
    if (!ArrayBuffer.isView(chunk)) throw new TypeError("byte stream enqueue expects an ArrayBufferView");
    s._chunks.push(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength).slice());
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
  get byobRequest() {
    return this._byobRequest;
  }
  get desiredSize() {
    const s = this[kStream];
    if (s._state === "errored") return null;
    if (s._state === "closed") return 0;
    return 1;
  }
}

// The view handed to a byte source during an auto-allocated BYOB pull; the source writes into it
// and calls respond(n) (or respondWithNewView).
class ReadableStreamBYOBRequest {
  constructor(controller, view) {
    this[kStream] = controller[kStream];
    this._controller = controller;
    this.view = view;
  }
  respond(bytesWritten) {
    const s = this[kStream];
    const filled = new Uint8Array(this.view.buffer, this.view.byteOffset, bytesWritten);
    s._chunks.push(filled.slice());
    this._controller._byobRequest = null;
    this.view = null;
    s._fulfillReads();
  }
  respondWithNewView(view) {
    const s = this[kStream];
    s._chunks.push(new Uint8Array(view.buffer, view.byteOffset, view.byteLength).slice());
    this._controller._byobRequest = null;
    this.view = null;
    s._fulfillReads();
  }
}

// Copy queued bytes into `view`, splitting a partial chunk back onto the queue. Returns a view over
// the same buffer covering the filled region.
function fillFromQueue(stream, view) {
  const target = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
  let filled = 0;
  while (filled < target.length && stream._chunks.length) {
    const head = stream._chunks[0];
    const take = Math.min(head.length, target.length - filled);
    target.set(head.subarray(0, take), filled);
    filled += take;
    if (take === head.length) stream._chunks.shift();
    else stream._chunks[0] = head.subarray(take);
  }
  return new view.constructor(view.buffer, view.byteOffset, filled / view.BYTES_PER_ELEMENT);
}

class ReadableStreamBYOBReader {
  constructor(stream) {
    if (!(stream instanceof ReadableStream) || !stream._isBytes) {
      throw new TypeError("BYOB reader requires a byte ReadableStream");
    }
    if (stream.locked) throw new TypeError("stream is already locked to a reader");
    this[kStream] = stream;
    stream._reader = this;
    this._byob = true;
    this._readRequests = [];
    this._closedResolve = null;
    this._closedReject = null;
    this._closedPromise = null;
  }
  read(view) {
    const s = this[kStream];
    if (!s) return Promise.reject(new TypeError("reader has been released"));
    if (!ArrayBuffer.isView(view)) return Promise.reject(new TypeError("BYOB read expects an ArrayBufferView"));
    if (s._chunks.length) return Promise.resolve({ value: fillFromQueue(s, view), done: false });
    if (s._state === "errored") return Promise.reject(s._storedError);
    if (s._state === "closed") {
      return Promise.resolve({ value: new view.constructor(view.buffer, view.byteOffset, 0), done: true });
    }
    // Auto-allocating source: hand it a byobRequest over the caller's view, then pull.
    if (typeof s._source.pull === "function") {
      s._controller._byobRequest = new ReadableStreamBYOBRequest(s._controller, view);
      s._maybePull();
      s._controller._byobRequest = null;
      if (s._chunks.length) return Promise.resolve({ value: fillFromQueue(s, view), done: false });
      if (s._state === "closed") {
        return Promise.resolve({ value: new view.constructor(view.buffer, view.byteOffset, 0), done: true });
      }
    }
    return new Promise((resolve, reject) => this._readRequests.push({ view, resolve, reject }));
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
    this._chunks = [];
    this._state = "readable"; // 'readable' | 'closed' | 'errored'
    this._storedError = undefined;
    this._reader = null;
    this._source = underlyingSource;
    this._pulling = false;
    this._isBytes = underlyingSource.type === "bytes";
    this._controller = this._isBytes
      ? new ReadableByteStreamController(this)
      : new ReadableStreamDefaultController(this);
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
      const req = reader._readRequests[0];
      if (this._chunks.length) {
        reader._readRequests.shift();
        // A BYOB read request carries the target view to fill; a default read takes the chunk.
        req.resolve(req.view ? { value: fillFromQueue(this, req.view), done: false } : { value: this._chunks.shift(), done: false });
      } else if (this._state === "closed") {
        reader._readRequests.shift();
        req.resolve(req.view ? { value: new req.view.constructor(req.view.buffer, req.view.byteOffset, 0), done: true } : { value: undefined, done: true });
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
    if (options && options.mode === "byob") {
      if (!this._isBytes) throw new TypeError("BYOB readers require a byte stream (type: 'bytes')");
      return new ReadableStreamBYOBReader(this);
    }
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
  async pipeTo(dest, options = {}) {
    const reader = this.getReader();
    const writer = dest.getWriter();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        await writer.write(value);
      }
      if (!options.preventClose) await writer.close();
    } catch (e) {
      if (!options.preventAbort) await writer.abort(e).catch(() => {});
      throw e;
    } finally {
      reader.releaseLock();
      writer.releaseLock();
    }
  }
  pipeThrough(transform, options = {}) {
    if (!transform || !transform.writable || !transform.readable) {
      throw new TypeError("pipeThrough expects { readable, writable }");
    }
    // Pump into the writable side; hand back the readable side immediately.
    this.pipeTo(transform.writable, options).catch(() => {});
    return transform.readable;
  }
  // Split into two independent streams. Both share one underlying reader; a chunk read for one
  // branch is buffered for the other (default streams share chunk references, per spec).
  tee() {
    const reader = this.getReader();
    const pending = [[], []];
    let ended = false;
    const readOne = async (branch) => {
      if (pending[branch].length) return { value: pending[branch].shift(), done: false };
      if (ended) return { value: undefined, done: true };
      const r = await reader.read();
      if (r.done) {
        ended = true;
        return { value: undefined, done: true };
      }
      pending[1 - branch].push(r.value);
      return { value: r.value, done: false };
    };
    const branch = (i) =>
      new ReadableStream({
        async pull(controller) {
          try {
            const r = await readOne(i);
            if (r.done) controller.close();
            else controller.enqueue(r.value);
          } catch (e) {
            controller.error(e);
          }
        },
      });
    return [branch(0), branch(1)];
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
globalThis.ReadableStreamDefaultReader = ReadableStreamDefaultReader;
globalThis.ReadableStreamDefaultController = ReadableStreamDefaultController;
globalThis.ReadableStreamBYOBReader = ReadableStreamBYOBReader;
globalThis.ReadableByteStreamController = ReadableByteStreamController;
globalThis.ReadableStreamBYOBRequest = ReadableStreamBYOBRequest;
