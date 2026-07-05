// Headers/Request/Response + fetch over the native __http.request op. Bodies are buffered
// (no ReadableStream yet); a Response body can be consumed once.

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
    return this._bodyBytes || new Uint8Array(0);
  };
  Object.defineProperty(proto, "bodyUsed", {
    get() {
      return !!this[kConsumed];
    },
  });
}

function toBodyBytes(body) {
  if (body === undefined || body === null) return undefined;
  if (typeof body === "string") return new TextEncoder().encode(body);
  if (body instanceof Uint8Array) return body;
  if (body instanceof ArrayBuffer) return new Uint8Array(body);
  if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
  if (body instanceof URLSearchParams) return new TextEncoder().encode(body.toString());
  return new TextEncoder().encode(String(body));
}

class Request {
  constructor(input, init = {}) {
    init = init && typeof init === "object" ? init : {};
    if (input instanceof Request) {
      this.url = input.url;
      this.method = init.method ? String(init.method).toUpperCase() : input.method;
      this.headers = new Headers(init.headers || input.headers);
      this._bodyBytes = "body" in init ? toBodyBytes(init.body) : input._bodyBytes;
      this.signal = init.signal || input.signal || null;
    } else {
      this.url = new URL(String(input)).href;
      this.method = init.method ? String(init.method).toUpperCase() : "GET";
      this.headers = new Headers(init.headers);
      this._bodyBytes = toBodyBytes(init.body);
      this.signal = init.signal || null;
    }
    if ((this.method === "GET" || this.method === "HEAD") && this._bodyBytes !== undefined) {
      throw new TypeError(`${this.method} request cannot have a body`);
    }
    this[kConsumed] = false;
  }
  clone() {
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
    this.status = "status" in init ? Number(init.status) : 200;
    if (this.status < 200 || this.status > 599) {
      throw new RangeError(`invalid response status ${this.status}`);
    }
    this.statusText = "statusText" in init ? String(init.statusText) : "";
    this.headers = new Headers(init.headers);
    this.url = "";
    this.redirected = false;
    this._bodyBytes = toBodyBytes(body);
    this[kConsumed] = false;
  }
  get ok() {
    return this.status >= 200 && this.status < 300;
  }
  clone() {
    if (this[kConsumed]) throw new TypeError("cannot clone a used Response");
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
      request._bodyBytes,
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
