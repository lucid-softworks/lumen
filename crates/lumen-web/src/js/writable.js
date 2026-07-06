// WritableStream / TransformStream + queuing strategies + Text{Encoder,Decoder}Stream. Loaded
// after streams.js (TransformStream feeds a ReadableStream). Backpressure is modeled loosely (a
// desiredSize counter, sequential write chain); enough for pipeThrough/pipeTo and the text streams.

const kWS = Symbol("writableStream");
const kTS = Symbol("transformStream");

class WritableStreamDefaultController {
  constructor(stream) {
    this[kWS] = stream;
    this.signal = stream._abortController.signal;
  }
  error(e) {
    this[kWS]._errorInternal(e);
  }
  get [Symbol.toStringTag]() {
    return "WritableStreamDefaultController";
  }
}

class WritableStreamDefaultWriter {
  constructor(stream) {
    if (!(stream instanceof WritableStream)) throw new TypeError("not a WritableStream");
    if (stream.locked) throw new TypeError("WritableStream is already locked");
    this[kWS] = stream;
    stream._writer = this;
    this._ready = stream._state === "writable" ? Promise.resolve() : Promise.reject(stream._storedError);
    this._ready.catch(() => {});
    this._closed = new Promise((res, rej) => {
      this._closedResolve = res;
      this._closedReject = rej;
    });
    this._closed.catch(() => {});
    if (stream._state === "closed") this._closedResolve();
    else if (stream._state === "errored") this._closedReject(stream._storedError);
  }
  get desiredSize() {
    const s = this[kWS];
    if (!s) throw new TypeError("writer released");
    if (s._state === "errored" || s._state === "erroring") return null;
    if (s._state === "closed") return 0;
    return s._hwm - s._queueSize;
  }
  get ready() {
    return this._ready;
  }
  get closed() {
    return this._closed;
  }
  write(chunk) {
    const s = this[kWS];
    if (!s) return Promise.reject(new TypeError("writer released"));
    if (s._state === "errored") return Promise.reject(s._storedError);
    s._queueSize += 1;
    s._writeChain = s._writeChain.then(
      () => {
        if (s._state === "errored" || s._state === "erroring") throw s._storedError;
        return s._sink.write ? s._sink.write(chunk, s._controller) : undefined;
      },
    ).then(
      () => {
        s._queueSize -= 1;
      },
      (e) => {
        s._queueSize -= 1;
        s._errorInternal(e);
        throw e;
      },
    );
    return s._writeChain;
  }
  close() {
    const s = this[kWS];
    if (!s) return Promise.reject(new TypeError("writer released"));
    return s._closeInternal().then(() => this._closedResolve());
  }
  abort(reason) {
    return this[kWS] ? this[kWS].abort(reason) : Promise.reject(new TypeError("writer released"));
  }
  releaseLock() {
    const s = this[kWS];
    if (!s) return;
    s._writer = null;
    this[kWS] = null;
  }
}

class WritableStream {
  constructor(sink = {}, strategy = {}) {
    this._sink = sink || {};
    this._state = "writable"; // 'writable' | 'erroring' | 'errored' | 'closed'
    this._storedError = undefined;
    this._writer = null;
    this._writeChain = Promise.resolve();
    this._queueSize = 0;
    this._hwm = strategy && strategy.highWaterMark !== undefined ? Number(strategy.highWaterMark) : 1;
    this._abortController = new AbortController();
    this._controller = new WritableStreamDefaultController(this);
    if (typeof this._sink.start === "function") {
      try {
        Promise.resolve(this._sink.start(this._controller)).catch((e) => this._errorInternal(e));
      } catch (e) {
        this._errorInternal(e);
      }
    }
  }
  get locked() {
    return this._writer !== null;
  }
  getWriter() {
    return new WritableStreamDefaultWriter(this);
  }
  close() {
    if (this.locked) return Promise.reject(new TypeError("WritableStream is locked"));
    const w = this.getWriter();
    return w.close().finally(() => w.releaseLock());
  }
  abort(reason) {
    if (this._state === "closed" || this._state === "errored") return Promise.resolve();
    this._abortController.abort(reason);
    return this._writeChain.then(() => {
      const p = this._sink.abort ? Promise.resolve(this._sink.abort(reason)) : Promise.resolve();
      return p.then(() => {
        this._state = "errored";
        this._storedError = reason;
      });
    });
  }
  _closeInternal() {
    return this._writeChain.then(() => {
      if (this._state === "errored") throw this._storedError;
      const p = this._sink.close ? Promise.resolve(this._sink.close()) : Promise.resolve();
      return p.then(() => {
        this._state = "closed";
      });
    });
  }
  _errorInternal(e) {
    if (this._state === "writable" || this._state === "erroring") {
      this._state = "errored";
      this._storedError = e;
      if (this._writer && this._writer._closedReject) this._writer._closedReject(e);
    }
  }
  get [Symbol.toStringTag]() {
    return "WritableStream";
  }
}

class TransformStreamDefaultController {
  constructor(ts) {
    this[kTS] = ts;
  }
  get desiredSize() {
    const c = this[kTS]._readableController;
    return c ? c.desiredSize : null;
  }
  enqueue(chunk) {
    this[kTS]._readableController.enqueue(chunk);
  }
  error(e) {
    this[kTS]._readableController.error(e);
    this[kTS]._writable._errorInternal(e);
  }
  terminate() {
    try {
      this[kTS]._readableController.close();
    } catch {
      /* already closed */
    }
    this[kTS]._writable._errorInternal(new TypeError("TransformStream terminated"));
  }
  get [Symbol.toStringTag]() {
    return "TransformStreamDefaultController";
  }
}

class TransformStream {
  constructor(transformer = {}, writableStrategy = {}, readableStrategy = {}) {
    transformer = transformer || {};
    let readableController;
    this.readable = new ReadableStream({ start(c) {
      readableController = c;
    } }, readableStrategy);
    this._readableController = readableController;
    this._controller = new TransformStreamDefaultController(this);
    const self = this;
    this.writable = new WritableStream(
      {
        write(chunk) {
          if (typeof transformer.transform === "function") {
            return Promise.resolve(transformer.transform(chunk, self._controller));
          }
          self._readableController.enqueue(chunk); // identity transform
        },
        close() {
          const flush =
            typeof transformer.flush === "function"
              ? Promise.resolve(transformer.flush(self._controller))
              : Promise.resolve();
          return flush.then(() => {
            try {
              self._readableController.close();
            } catch {
              /* already closed */
            }
          });
        },
        abort(reason) {
          self._readableController.error(reason);
        },
      },
      writableStrategy,
    );
    this._writable = this.writable;
    if (typeof transformer.start === "function") transformer.start(this._controller);
  }
  get [Symbol.toStringTag]() {
    return "TransformStream";
  }
}

class ByteLengthQueuingStrategy {
  constructor(options) {
    this.highWaterMark = options.highWaterMark;
  }
  get size() {
    return (chunk) => chunk.byteLength;
  }
  get [Symbol.toStringTag]() {
    return "ByteLengthQueuingStrategy";
  }
}

class CountQueuingStrategy {
  constructor(options) {
    this.highWaterMark = options.highWaterMark;
  }
  get size() {
    return () => 1;
  }
  get [Symbol.toStringTag]() {
    return "CountQueuingStrategy";
  }
}

// TextEncoderStream / TextDecoderStream — TransformStreams over the encoders. The decoder keeps
// streaming state across chunks (a multibyte sequence may straddle a boundary).
class TextEncoderStream {
  constructor() {
    const enc = new TextEncoder();
    this._ts = new TransformStream({
      transform(chunk, controller) {
        const bytes = enc.encode(String(chunk));
        if (bytes.length) controller.enqueue(bytes);
      },
    });
  }
  get encoding() {
    return "utf-8";
  }
  get readable() {
    return this._ts.readable;
  }
  get writable() {
    return this._ts.writable;
  }
}

class TextDecoderStream {
  constructor(label = "utf-8", options = {}) {
    const dec = new TextDecoder(label, options);
    this._encoding = dec.encoding;
    this._ts = new TransformStream({
      transform(chunk, controller) {
        const text = dec.decode(chunk, { stream: true });
        if (text) controller.enqueue(text);
      },
      flush(controller) {
        const text = dec.decode();
        if (text) controller.enqueue(text);
      },
    });
  }
  get encoding() {
    return this._encoding;
  }
  get readable() {
    return this._ts.readable;
  }
  get writable() {
    return this._ts.writable;
  }
}

globalThis.WritableStream = WritableStream;
globalThis.WritableStreamDefaultWriter = WritableStreamDefaultWriter;
globalThis.WritableStreamDefaultController = WritableStreamDefaultController;
globalThis.TransformStream = TransformStream;
globalThis.TransformStreamDefaultController = TransformStreamDefaultController;
globalThis.ByteLengthQueuingStrategy = ByteLengthQueuingStrategy;
globalThis.CountQueuingStrategy = CountQueuingStrategy;
globalThis.TextEncoderStream = TextEncoderStream;
globalThis.TextDecoderStream = TextDecoderStream;
