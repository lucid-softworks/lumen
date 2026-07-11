// Small node: builtins the Express stack pulls in. Each is the practical subset its consumers
// use, not a full implementation; gaps throw clearly rather than silently misbehaving.

// ---- node:perf_hooks --------------------------------------------------------------------------
// The web `performance` global, extended with a real mark/measure entry buffer that dispatches to
// PerformanceObserver. lumen's global `performance` has now()/timeOrigin but no user-timing API, so
// we add mark/measure/getEntries here and wire them to observers — marks and measures are real.
// The observer machinery for entry types lumen cannot produce (gc, http, resource…) simply never
// fires, which is the honest behavior for a runtime that emits no such entries.
{
  const perf = globalThis.performance;
  const now = () => perf.now();

  const buffer = []; // all recorded PerformanceEntry objects
  const observers = new Set(); // live PerformanceObserver instances

  class PerformanceEntry {
    constructor(name, entryType, startTime, duration) {
      this.name = name;
      this.entryType = entryType;
      this.startTime = startTime;
      this.duration = duration;
    }
    toJSON() {
      return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration };
    }
  }
  class PerformanceMark extends PerformanceEntry {
    constructor(name, options) {
      super(name, "mark", options && options.startTime !== undefined ? options.startTime : now(), 0);
      this.detail = (options && options.detail) ?? null;
    }
  }
  class PerformanceMeasure extends PerformanceEntry {
    constructor(name, startTime, duration, detail) {
      super(name, "measure", startTime, duration);
      this.detail = detail ?? null;
    }
  }
  class PerformanceResourceTiming extends PerformanceEntry {
    constructor(name, startTime, duration) {
      super(name, "resource", startTime ?? 0, duration ?? 0);
    }
  }

  class PerformanceObserverEntryList {
    constructor(entries) { this._entries = entries; }
    getEntries() { return this._entries.slice(); }
    getEntriesByName(name, type) {
      return this._entries.filter((e) => e.name === name && (type === undefined || e.entryType === type));
    }
    getEntriesByType(type) { return this._entries.filter((e) => e.entryType === type); }
  }

  class PerformanceObserver {
    constructor(callback) {
      this._callback = callback;
      this._types = new Set();
      this._pending = [];
    }
    observe(options = {}) {
      const types = options.entryTypes || (options.type ? [options.type] : []);
      for (const t of types) this._types.add(t);
      observers.add(this);
      if (options.buffered) {
        const matching = buffer.filter((e) => this._types.has(e.entryType));
        if (matching.length) {
          this._pending.push(...matching);
          queueMicrotask(() => this._flush());
        }
      }
    }
    disconnect() {
      observers.delete(this);
      this._types.clear();
      this._pending = [];
    }
    takeRecords() {
      const records = this._pending;
      this._pending = [];
      return records;
    }
    _deliver(entry) {
      this._pending.push(entry);
      queueMicrotask(() => this._flush());
    }
    _flush() {
      if (this._pending.length === 0) return;
      const list = new PerformanceObserverEntryList(this._pending);
      this._pending = [];
      this._callback(list, this);
    }
  }
  PerformanceObserver.supportedEntryTypes = ["mark", "measure", "resource"];

  function record(entry) {
    buffer.push(entry);
    for (const obs of observers) {
      if (obs._types.has(entry.entryType)) obs._deliver(entry);
    }
    return entry;
  }

  // Augment the global `performance` with the user-timing API if it isn't already present.
  if (typeof perf.mark !== "function") {
    perf.mark = function mark(name, options) {
      return record(new PerformanceMark(name, options));
    };
    perf.measure = function measure(name, startOrOptions, endMark) {
      let start, end;
      if (startOrOptions && typeof startOrOptions === "object") {
        start = resolveMark(startOrOptions.start);
        end = startOrOptions.end !== undefined ? resolveMark(startOrOptions.end) : now();
        if (startOrOptions.duration !== undefined && startOrOptions.start !== undefined) {
          end = start + startOrOptions.duration;
        }
        return record(new PerformanceMeasure(name, start, end - start, startOrOptions.detail));
      }
      start = startOrOptions !== undefined ? resolveMark(startOrOptions) : 0;
      end = endMark !== undefined ? resolveMark(endMark) : now();
      return record(new PerformanceMeasure(name, start, end - start));
    };
    perf.clearMarks = function clearMarks(name) {
      for (let i = buffer.length - 1; i >= 0; i--) {
        if (buffer[i].entryType === "mark" && (name === undefined || buffer[i].name === name)) buffer.splice(i, 1);
      }
    };
    perf.clearMeasures = function clearMeasures(name) {
      for (let i = buffer.length - 1; i >= 0; i--) {
        if (buffer[i].entryType === "measure" && (name === undefined || buffer[i].name === name)) buffer.splice(i, 1);
      }
    };
    perf.getEntries = () => buffer.slice();
    perf.getEntriesByName = (name, type) =>
      buffer.filter((e) => e.name === name && (type === undefined || e.entryType === type));
    perf.getEntriesByType = (type) => buffer.filter((e) => e.entryType === type);
    perf.clearResourceTimings = () => {
      for (let i = buffer.length - 1; i >= 0; i--) if (buffer[i].entryType === "resource") buffer.splice(i, 1);
    };
  }

  function resolveMark(nameOrTime) {
    if (typeof nameOrTime === "number") return nameOrTime;
    for (let i = buffer.length - 1; i >= 0; i--) {
      if (buffer[i].entryType === "mark" && buffer[i].name === nameOrTime) return buffer[i].startTime;
    }
    throw new Error(`The "${nameOrTime}" performance mark has not been set`);
  }

  // A simple, real recordable histogram (used by monitorEventLoopDelay and createHistogram).
  function makeHistogram() {
    let samples = [];
    return {
      record(value) { samples.push(Number(value)); },
      recordDelta() {},
      enable() { return true; },
      disable() { return true; },
      reset() { samples = []; },
      get count() { return samples.length; },
      get min() { return samples.length ? Math.min(...samples) : 0; },
      get max() { return samples.length ? Math.max(...samples) : 0; },
      get mean() { return samples.length ? samples.reduce((a, b) => a + b, 0) / samples.length : 0; },
      get stddev() {
        if (samples.length < 2) return 0;
        const m = samples.reduce((a, b) => a + b, 0) / samples.length;
        return Math.sqrt(samples.reduce((a, b) => a + (b - m) ** 2, 0) / samples.length);
      },
      get exceeds() { return 0; },
      percentile(p) {
        if (samples.length === 0) return 0;
        const sorted = samples.slice().sort((a, b) => a - b);
        const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
        return sorted[Math.max(0, idx)];
      },
      get percentiles() { return new Map(); },
    };
  }

  // The V8 perf-milestone constants Node exposes. lumen isn't V8, so these are the documented
  // enum values (their numeric identity is what code compares against), with no live milestones.
  const constants = {
    NODE_PERFORMANCE_GC_MAJOR: 4,
    NODE_PERFORMANCE_GC_MINOR: 1,
    NODE_PERFORMANCE_GC_INCREMENTAL: 8,
    NODE_PERFORMANCE_GC_WEAKCB: 16,
    NODE_PERFORMANCE_GC_FLAGS_NO: 0,
    NODE_PERFORMANCE_GC_FLAGS_CONSTRUCT_RETAINED: 2,
    NODE_PERFORMANCE_GC_FLAGS_FORCED: 4,
    NODE_PERFORMANCE_GC_FLAGS_SYNCHRONOUS_PHANTOM_PROCESSING: 8,
    NODE_PERFORMANCE_GC_FLAGS_ALL_AVAILABLE_GARBAGE: 16,
    NODE_PERFORMANCE_GC_FLAGS_ALL_EXTERNAL_MEMORY: 32,
    NODE_PERFORMANCE_GC_FLAGS_SCHEDULE_IDLE: 64,
  };

  __builtins.set("perf_hooks", {
    performance: perf,
    Performance: perf.constructor,
    PerformanceEntry,
    PerformanceMark,
    PerformanceMeasure,
    PerformanceResourceTiming,
    PerformanceObserver,
    PerformanceObserverEntryList,
    constants,
    createHistogram: () => makeHistogram(),
    monitorEventLoopDelay: () => {
      // lumen exposes no loop-lag signal, so this histogram stays empty; enable/disable/reset are
      // real, the recorded delay is honestly zero.
      const h = makeHistogram();
      return h;
    },
  });
}

// ---- node:querystring -------------------------------------------------------------------------
// Classic (non-percent-strict) parse/stringify. `qs`/body-parser can use `querystring` for simple
// bodies; Express's default query parser is `qs` (its own package), so this is the fallback path.
{
  const qsUnescape = (s) => { try { return decodeURIComponent(s.replace(/\+/g, " ")); } catch { return s; } };
  const qsEscape = (s) => encodeURIComponent(s);

  function parse(str, sep = "&", eq = "=") {
    const obj = Object.create(null);
    if (typeof str !== "string" || str.length === 0) return obj;
    for (const part of str.split(sep)) {
      if (part === "") continue;
      const idx = part.indexOf(eq);
      let k, v;
      if (idx < 0) { k = qsUnescape(part); v = ""; }
      else { k = qsUnescape(part.slice(0, idx)); v = qsUnescape(part.slice(idx + eq.length)); }
      if (k in obj) {
        if (Array.isArray(obj[k])) obj[k].push(v);
        else obj[k] = [obj[k], v];
      } else obj[k] = v;
    }
    return obj;
  }

  function stringify(obj, sep = "&", eq = "=") {
    if (obj === null || typeof obj !== "object") return "";
    const pairs = [];
    for (const k of Object.keys(obj)) {
      const ek = qsEscape(k);
      const v = obj[k];
      if (Array.isArray(v)) for (const item of v) pairs.push(`${ek}${eq}${qsEscape(String(item))}`);
      else pairs.push(`${ek}${eq}${qsEscape(v == null ? "" : String(v))}`);
    }
    return pairs.join(sep);
  }

  __builtins.set("querystring", { parse, stringify, decode: parse, encode: stringify, escape: qsEscape, unescape: qsUnescape });
}

// ---- node:url ---------------------------------------------------------------------------------
// The web `URL`/`URLSearchParams` globals, plus the legacy `url.parse()` that server middleware
// (parseurl, serve-static) calls on request targets like "/path?query" (origin-form, no host).
{
  const querystring = __builtins.get("querystring");

  function legacyParse(urlStr, parseQueryString = false, slashesDenoteHost = false) {
    const url = { protocol: null, slashes: null, auth: null, host: null, port: null, hostname: null, hash: null, search: null, query: null, pathname: null, path: null, href: urlStr };
    let rest = String(urlStr);

    const hashIdx = rest.indexOf("#");
    if (hashIdx >= 0) { url.hash = rest.slice(hashIdx); rest = rest.slice(0, hashIdx); }

    const protoMatch = /^([a-z0-9.+-]+:)/i.exec(rest);
    if (protoMatch) { url.protocol = protoMatch[1].toLowerCase(); rest = rest.slice(protoMatch[1].length); }

    if ((url.protocol && rest.startsWith("//")) || (slashesDenoteHost && rest.startsWith("//"))) {
      url.slashes = true;
      rest = rest.slice(2);
      let hostEnd = rest.length;
      for (const ch of ["/", "?", "#"]) { const i = rest.indexOf(ch); if (i >= 0 && i < hostEnd) hostEnd = i; }
      let host = rest.slice(0, hostEnd);
      rest = rest.slice(hostEnd);
      const at = host.lastIndexOf("@");
      if (at >= 0) { url.auth = host.slice(0, at); host = host.slice(at + 1); }
      url.host = host;
      const colon = host.lastIndexOf(":");
      if (colon >= 0) { url.hostname = host.slice(0, colon); url.port = host.slice(colon + 1); }
      else url.hostname = host;
    }

    const qIdx = rest.indexOf("?");
    if (qIdx >= 0) { url.search = rest.slice(qIdx); url.pathname = rest.slice(0, qIdx); }
    else url.pathname = rest;
    if (url.search) url.query = parseQueryString ? querystring.parse(url.search.slice(1)) : url.search.slice(1);
    else url.query = parseQueryString ? Object.create(null) : null;
    if (url.pathname === "" && url.host) url.pathname = "/";
    url.path = url.pathname + (url.search || "");
    return url;
  }

  function format(urlObj) {
    if (typeof urlObj === "string") return urlObj;
    if (urlObj instanceof URL) return urlObj.href;
    let out = "";
    if (urlObj.protocol) out += urlObj.protocol + (urlObj.slashes || urlObj.host ? "//" : "");
    if (urlObj.auth) out += urlObj.auth + "@";
    if (urlObj.host) out += urlObj.host;
    else if (urlObj.hostname) out += urlObj.hostname + (urlObj.port ? ":" + urlObj.port : "");
    out += urlObj.pathname || "";
    out += urlObj.search || (urlObj.query && typeof urlObj.query === "object" ? "?" + querystring.stringify(urlObj.query) : "") || "";
    out += urlObj.hash || "";
    return out;
  }

  __builtins.set("url", {
    parse: legacyParse,
    format,
    resolve: (from, to) => new URL(to, new URL(from, "http://localhost")).href,
    URL,
    URLSearchParams,
    Url: function Url() {},
    domainToASCII: (d) => d,
    domainToUnicode: (d) => d,
    fileURLToPath: (u) => (typeof u === "string" ? u : u.pathname).replace(/^file:\/\//, ""),
    pathToFileURL: (p) => new URL("file://" + p),
  });
}

// node:net now lives in its own glue file (net.js) — its surface grew past the "small shim" bar
// (BlockList, SocketAddress, auto-select-family flags).

// ---- node:assert ------------------------------------------------------------------------------
{
  class AssertionError extends Error {
    constructor(opts = {}) {
      super(opts.message || "Assertion failed");
      this.name = "AssertionError";
      this.code = "ERR_ASSERTION";
      this.actual = opts.actual;
      this.expected = opts.expected;
      this.operator = opts.operator;
    }
  }
  const fail = (message) => { throw new AssertionError({ message: typeof message === "string" ? message : "Failed" }); };
  function assert(value, message) {
    if (!value) throw new AssertionError({ message: message || `The expression evaluated to a falsy value:`, actual: value, expected: true, operator: "==" });
  }
  const strictEqual = (a, b, m) => { if (!Object.is(a, b)) throw new AssertionError({ message: m, actual: a, expected: b, operator: "strictEqual" }); };
  const notStrictEqual = (a, b, m) => { if (Object.is(a, b)) throw new AssertionError({ message: m, actual: a, expected: b, operator: "notStrictEqual" }); };
  const equal = (a, b, m) => { if (a != b) throw new AssertionError({ message: m, actual: a, expected: b, operator: "==" }); };
  const notEqual = (a, b, m) => { if (a == b) throw new AssertionError({ message: m, actual: a, expected: b, operator: "!=" }); };
  Object.assign(assert, {
    ok: assert, fail, strictEqual, notStrictEqual, equal, notEqual,
    deepEqual: equal, deepStrictEqual: strictEqual, notDeepStrictEqual: notStrictEqual,
    ifError: (err) => { if (err) throw err; },
    throws: (fn, m) => { let t = false; try { fn(); } catch { t = true; } if (!t) fail(m || "Missing expected exception"); },
    doesNotThrow: (fn) => { fn(); },
    AssertionError,
  });
  assert.strict = assert;
  __builtins.set("assert", assert);
}

// ---- node:string_decoder ----------------------------------------------------------------------
// Streaming multibyte-safe decode (a chunk can split a UTF-8 sequence). Backed by TextDecoder's
// own streaming mode for utf-8; other encodings fall back to Buffer.toString per-chunk.
{
  // A function constructor, NOT a class: iconv-lite inherits via `StringDecoder.call(this, enc)`
  // + `Child.prototype = StringDecoder.prototype`, which a class constructor rejects ("cannot be
  // invoked without new").
  function StringDecoder(encoding) {
    this.encoding = String(encoding || "utf8").toLowerCase().replace("-", "");
    this._utf8 = this.encoding === "utf8";
    if (this._utf8) this._dec = new TextDecoder("utf-8");
  }
  StringDecoder.prototype.write = function (buffer) {
    if (this._utf8) return this._dec.decode(buffer, { stream: true });
    return Buffer.from(buffer).toString(this.encoding);
  };
  StringDecoder.prototype.end = function (buffer) {
    let out = "";
    if (buffer && buffer.length) out += this.write(buffer);
    if (this._utf8) out += this._dec.decode();
    return out;
  };
  __builtins.set("string_decoder", { StringDecoder });
}

// ---- node:tty ---------------------------------------------------------------------------------
// We run behind pipes, never a terminal — isatty is always false (debug uses it for colors).
__builtins.set("tty", {
  isatty: () => false,
  ReadStream: function () { throw new Error("node:tty streams are not supported"); },
  WriteStream: function () { throw new Error("node:tty streams are not supported"); },
});

// ---- node:async_hooks -------------------------------------------------------------------------
// A no-op AsyncResource / hook surface. lumen has no async-context tracking; on-finished/raw-body
// use `AsyncResource.bind` only to preserve context we don't model, so binding is identity.
{
  let nextId = 1;
  class AsyncResource {
    constructor(type) { this.type = type; this._id = nextId++; }
    runInAsyncScope(fn, thisArg, ...args) { return Reflect.apply(fn, thisArg, args); }
    bind(fn) { return fn; }
    emitDestroy() { return this; }
    asyncId() { return this._id; }
    triggerAsyncId() { return 0; }
    static bind(fn) { return fn; }
  }
  __builtins.set("async_hooks", {
    AsyncResource,
    executionAsyncId: () => 0,
    triggerAsyncId: () => 0,
    executionAsyncResource: () => ({}),
    createHook: () => ({ enable() { return this; }, disable() { return this; } }),
    AsyncLocalStorage: class AsyncLocalStorage {
      run(store, cb, ...args) { const prev = this._store; this._store = store; try { return cb(...args); } finally { this._store = prev; } }
      getStore() { return this._store; }
      enterWith(store) { this._store = store; }
      exit(cb, ...args) { const prev = this._store; this._store = undefined; try { return cb(...args); } finally { this._store = prev; } }
      disable() { this._store = undefined; }
    },
  });
}

// ---- node:zlib --------------------------------------------------------------------------------
// Real gzip/deflate over the shared DEFLATE codec (__zlib native ops). Sync, async-callback, and
// Transform-stream (createGzip/…) forms. Brotli is not implemented (no Brotli codec).
{
  const codecs = {
    gzip: __zlib.gzip, gunzip: __zlib.gunzip,
    deflate: __zlib.deflate, inflate: __zlib.inflate,
    deflateRaw: __zlib.deflateRaw, inflateRaw: __zlib.inflateRaw,
  };

  const sync = (fn) => (input) => Buffer.from(fn(input instanceof Uint8Array ? input : Buffer.from(input)));
  const asyncOf = (syncFn) => (input, opts, cb) => {
    if (typeof opts === "function") cb = opts;
    queueMicrotask(() => {
      try {
        const out = syncFn(input);
        cb(null, out);
      } catch (e) {
        cb(e);
      }
    });
  };
  // A Transform that buffers input and (de)compresses it whole on flush (the codec is one-shot).
  // `stream` (node:stream) is looked up lazily: shims.js loads before stream.js.
  const stream = (syncFn) => () => {
    const Transform = __builtins.get("stream").Transform;
    const chunks = [];
    return new Transform({
      transform(chunk, enc, next) {
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, enc));
        next();
      },
      flush(done) {
        try {
          this.push(syncFn(Buffer.concat(chunks)));
          done();
        } catch (e) {
          done(e);
        }
      },
    });
  };

  const zlib = { constants: {} };
  for (const [name, fn] of Object.entries(codecs)) {
    const cap = name[0].toUpperCase() + name.slice(1);
    const syncFn = sync(fn);
    zlib[`${name}Sync`] = syncFn;
    zlib[name] = asyncOf(syncFn);
    // create<Gzip|Gunzip|Deflate|Inflate|DeflateRaw|InflateRaw>
    zlib[`create${cap}`] = stream(syncFn);
  }
  // Aliases Node exposes.
  zlib.unzipSync = zlib.gunzipSync;
  zlib.unzip = zlib.gunzip;
  zlib.createUnzip = zlib.createGunzip;
  // Brotli is not implemented (a from-scratch Brotli codec, with its 120 KB static dictionary, is
  // a large detour; gzip covers the common case). The functions exist — so `promisify(brotli…)`
  // and feature detection work — but reject/throw when actually invoked, rather than silently
  // producing wrong bytes.
  const brotliError = () => new Error("node:zlib Brotli is not supported in lumen");
  const brotliThrow = () => {
    throw brotliError();
  };
  const brotliAsync = (input, opts, cb) => {
    if (typeof opts === "function") cb = opts;
    queueMicrotask(() => cb(brotliError()));
  };
  zlib.createBrotliCompress = brotliThrow;
  zlib.createBrotliDecompress = brotliThrow;
  zlib.brotliCompressSync = brotliThrow;
  zlib.brotliDecompressSync = brotliThrow;
  zlib.brotliCompress = brotliAsync;
  zlib.brotliDecompress = brotliAsync;
  __builtins.set("zlib", zlib);
}
