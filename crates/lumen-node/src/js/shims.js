// Small node: builtins the Express stack pulls in. Each is the practical subset its consumers
// use, not a full implementation; gaps throw clearly rather than silently misbehaving.

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

// ---- node:net ---------------------------------------------------------------------------------
// Only the address helpers server code inspects (trust-proxy, x-forwarded-for). A real Socket/
// Server would need the raw sockets lumen doesn't expose to JS.
{
  const v4 = /^(\d{1,3}\.){3}\d{1,3}$/;
  const v6 = /^([0-9a-f]{0,4}:){2,7}[0-9a-f]{0,4}$/i;
  const isIPv4 = (s) => v4.test(s) && s.split(".").every((n) => Number(n) <= 255);
  const isIPv6 = (s) => v6.test(s) && s.includes("::") ? s.split("::").length <= 2 : v6.test(s);
  const isIP = (s) => (isIPv4(s) ? 4 : isIPv6(s) ? 6 : 0);
  const notImpl = () => { throw new Error("node:net sockets are not supported in lumen"); };
  __builtins.set("net", { isIP, isIPv4, isIPv6, Socket: notImpl, Server: notImpl, createConnection: notImpl, connect: notImpl, createServer: notImpl });
}

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
  const brotliUnsupported = () => {
    throw new Error("node:zlib Brotli is not supported in lumen");
  };
  zlib.createBrotliCompress = brotliUnsupported;
  zlib.createBrotliDecompress = brotliUnsupported;
  zlib.brotliCompressSync = brotliUnsupported;
  zlib.brotliDecompressSync = brotliUnsupported;
  __builtins.set("zlib", zlib);
}
