// Headers/Request/Response + fetch over the native __http.request op. Bodies are buffered; a body
// can be consumed once, either through `text()`/`json()`/… or by reading its `.body` ReadableStream
// (see streams.js). The two share one "consumed" flag.

function normalizeHeaderName(name) {
  name = String(name);
  if (name === "" || /[^\x21-\x7e]/.test(name) || /[()<>@,;:\\"/[\]?={} \t]/.test(name)) {
    throw new TypeError(`invalid header name '${name}'`);
  }
  return name.toLowerCase();
}

class Headers {
  constructor(init) {
    this._map = new Map(); // lower-name -> { name, value }
    if (init instanceof Headers) {
      for (const [k, v] of init) this.append(k, v);
    } else if (Array.isArray(init)) {
      for (const pair of init) {
        if (!pair || pair.length !== 2) throw new TypeError("Headers: init pair needs two items");
        this.append(pair[0], pair[1]);
      }
    } else if (init && typeof init === "object") {
      for (const k of Object.keys(init)) this.append(k, init[k]);
    }
  }
  append(name, value) {
    const key = normalizeHeaderName(name);
    value = String(value).trim();
    const existing = this._map.get(key);
    this._map.set(key, { name: key, value: existing ? `${existing.value}, ${value}` : value });
  }
  delete(name) {
    this._map.delete(normalizeHeaderName(name));
  }
  get(name) {
    const hit = this._map.get(normalizeHeaderName(name));
    return hit ? hit.value : null;
  }
  has(name) {
    return this._map.has(normalizeHeaderName(name));
  }
  set(name, value) {
    const key = normalizeHeaderName(name);
    this._map.set(key, { name: key, value: String(value).trim() });
  }
  forEach(fn, thisArg) {
    for (const [k, v] of this) fn.call(thisArg, v, k, this);
  }
  *entries() {
    // Sorted by name, per spec.
    const keys = [...this._map.keys()].sort();
    for (const k of keys) yield [k, this._map.get(k).value];
  }
  *keys() {
    for (const [k] of this) yield k;
  }
  *values() {
    for (const [, v] of this) yield v;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  _pairs() {
    return [...this].map(([k, v]) => [k, v]);
  }
}

const kConsumed = Symbol("bodyConsumed");
const kBodyStream = Symbol("bodyStream");
// A user-supplied ReadableStream body, kept un-drained until the body is actually consumed or
// sent. Draining at construction would break feature-detection code (e.g. ky) that builds — but
// never sends — a `new Request(url, { body: new ReadableStream() })` just to probe support.
const kSourceStream = Symbol("bodySourceStream");

// Set `owner`'s body from a BodyInit. A ReadableStream is stored, not drained (see kSourceStream);
// everything else is encoded to bytes eagerly.
function initBody(owner, body) {
  if (body instanceof ReadableStream) {
    owner[kSourceStream] = body;
    owner._bodyBytes = undefined;
  } else {
    owner._bodyBytes = toBodyBytes(body);
  }
}

function bodyMixin(proto) {
  proto.text = async function () {
    return new TextDecoder().decode(await this._consume());
  };
  proto.json = async function () {
    return JSON.parse(await this.text());
  };
  proto.arrayBuffer = async function () {
    const bytes = await this._consume();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  };
  proto.bytes = async function () {
    return this._consume();
  };
  proto._consume = async function () {
    if (this[kConsumed]) throw new TypeError("body already consumed");
    this[kConsumed] = true;
    this._materialize();
    return this._bodyBytes || new Uint8Array(0);
  };
  // Drain a deferred ReadableStream body into bytes, on first consume/send.
  proto._materialize = function () {
    if (this[kSourceStream] !== undefined) {
      this._bodyBytes = drainStreamSync(this[kSourceStream]);
      this[kSourceStream] = undefined;
    }
  };
  Object.defineProperty(proto, "bodyUsed", {
    get() {
      return !!this[kConsumed];
    },
  });
  // `.body` is the user's ReadableStream if one was given (un-drained), else a stream over the
  // buffered bytes, or `null` when there is no body. The same stream instance is handed out on
  // repeated access (per spec); reading it consumes the body.
  Object.defineProperty(proto, "body", {
    configurable: true,
    get() {
      if (this[kSourceStream] !== undefined) return this[kSourceStream];
      if (this._bodyBytes === undefined) return null;
      if (this[kBodyStream] === undefined) this[kBodyStream] = makeBodyStream(this);
      return this[kBodyStream];
    },
  });
}

function toBodyBytes(body) {
  if (body === undefined || body === null) return undefined;
  if (typeof body === "string") return new TextEncoder().encode(body);
  if (body instanceof Uint8Array) return body;
  if (body instanceof ArrayBuffer) return new Uint8Array(body);
  if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
  if (body instanceof ReadableStream) return drainStreamSync(body);
  if (body instanceof URLSearchParams) return new TextEncoder().encode(body.toString());
  return new TextEncoder().encode(String(body));
}

// Read a whole ReadableStream into one Uint8Array *synchronously*. Bodies are buffered, so a stream
// used as a request/response body must produce its data without awaiting (our own body streams do;
// so does any source whose `start`/`pull` enqueues synchronously).
function drainStreamSync(stream) {
  if (stream.locked) throw new TypeError("cannot construct a body from a locked ReadableStream");
  const reader = stream.getReader();
  const parts = [];
  let total = 0;
  for (;;) {
    const r = reader._readSync();
    if (r.pending) {
      throw new TypeError("a body ReadableStream must produce its data synchronously in this runtime");
    }
    if (r.done) break;
    let chunk = r.value;
    if (typeof chunk === "string") chunk = new TextEncoder().encode(chunk);
    else if (chunk instanceof ArrayBuffer) chunk = new Uint8Array(chunk);
    else if (ArrayBuffer.isView(chunk)) chunk = new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    else if (!(chunk instanceof Uint8Array)) throw new TypeError("ReadableStream chunk is not binary data");
    parts.push(chunk);
    total += chunk.length;
  }
  reader.releaseLock();
  if (parts.length === 1) return parts[0];
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

// The `.body` ReadableStream for a Request/Response: it lazily hands out the buffered bytes as a
// single chunk (marking the body consumed, shared with `text()`/`json()`), or is empty when there
// is no body.
function makeBodyStream(owner) {
  return new ReadableStream({
    pull(controller) {
      if (owner[kConsumed]) {
        controller.error(new TypeError("body already consumed"));
        return;
      }
      owner[kConsumed] = true;
      const bytes = owner._bodyBytes;
      if (bytes && bytes.length) controller.enqueue(bytes);
      controller.close();
    },
  });
}

class Request {
  constructor(input, init = {}) {
    init = init && typeof init === "object" ? init : {};
    if (input instanceof Request) {
      this.url = input.url;
      this.method = init.method ? String(init.method).toUpperCase() : input.method;
      this.headers = new Headers(init.headers || input.headers);
      if ("body" in init) {
        initBody(this, init.body);
      } else {
        // Inherit the source request's body (its deferred stream, if any).
        this._bodyBytes = input._bodyBytes;
        this[kSourceStream] = input[kSourceStream];
      }
      this.signal = init.signal || input.signal || null;
    } else {
      this.url = new URL(String(input)).href;
      this.method = init.method ? String(init.method).toUpperCase() : "GET";
      this.headers = new Headers(init.headers);
      initBody(this, init.body);
      this.signal = init.signal || null;
    }
    if (
      (this.method === "GET" || this.method === "HEAD") &&
      (this._bodyBytes !== undefined || this[kSourceStream] !== undefined)
    ) {
      throw new TypeError(`${this.method} request cannot have a body`);
    }
    this[kConsumed] = false;
  }
  clone() {
    this._materialize(); // can't tee a deferred stream; buffer it, then both share the bytes
    return new Request(this.url, {
      method: this.method,
      headers: this.headers,
      body: this._bodyBytes,
      signal: this.signal,
    });
  }
}
bodyMixin(Request.prototype);

class Response {
  constructor(body = null, init = {}) {
    init = init && typeof init === "object" ? init : {};
    // A dictionary member set to `undefined` counts as absent (WebIDL), so `{ status: undefined }`
    // takes the default 200 rather than coercing to `Number(undefined)` → NaN.
    this.status = init.status !== undefined ? Number(init.status) : 200;
    if (this.status < 200 || this.status > 599) {
      throw new RangeError(`invalid response status ${this.status}`);
    }
    this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
    this.headers = new Headers(init.headers);
    this.url = "";
    this.redirected = false;
    initBody(this, body);
    this[kConsumed] = false;
  }
  get ok() {
    return this.status >= 200 && this.status < 300;
  }
  clone() {
    if (this[kConsumed]) throw new TypeError("cannot clone a used Response");
    this._materialize();
    const r = new Response(this._bodyBytes, {
      status: this.status,
      statusText: this.statusText,
      headers: this.headers,
    });
    r.url = this.url;
    r.redirected = this.redirected;
    return r;
  }
  static json(data, init) {
    const r = new Response(JSON.stringify(data), init);
    if (!r.headers.has("content-type")) r.headers.set("content-type", "application/json");
    return r;
  }
  static error() {
    const r = new Response(null, { status: 200 });
    r.status = 0;
    r.type = "error";
    return r;
  }
}
bodyMixin(Response.prototype);

function fetch(input, init = {}) {
  return new Promise((resolve, reject) => {
    let request;
    try {
      request = new Request(input, init);
    } catch (e) {
      reject(e);
      return;
    }
    const signal = request.signal;
    if (signal && signal.aborted) {
      reject(signal.reason || new DOMException("The operation was aborted", "AbortError"));
      return;
    }
    const headerPairs = request.headers._pairs();
    let bodyBytes;
    try {
      request._materialize(); // drain a deferred stream body now that we're actually sending
      bodyBytes = request._bodyBytes;
    } catch (e) {
      reject(e);
      return;
    }
    let settled = false;
    const onAbort = () => {
      if (settled) return;
      settled = true;
      reject(signal.reason || new DOMException("The operation was aborted", "AbortError"));
    };
    if (signal) signal.addEventListener("abort", onAbort);

    __http.request(
      request.method,
      request.url,
      headerPairs,
      bodyBytes,
      (raw) => {
        if (settled) return; // aborted first
        settled = true;
        if (signal) signal.removeEventListener("abort", onAbort);
        const response = new Response(raw.body, {
          status: raw.status,
          statusText: raw.statusText,
          headers: raw.headers,
        });
        response.url = raw.url;
        response.redirected = raw.url !== request.url;
        resolve(response);
      },
      (err) => {
        if (settled) return;
        settled = true;
        if (signal) signal.removeEventListener("abort", onAbort);
        reject(err instanceof Error ? err : new TypeError(String(err)));
      }
    );
  });
}

globalThis.Headers = Headers;
globalThis.Request = Request;
globalThis.Response = Response;
globalThis.fetch = fetch;
