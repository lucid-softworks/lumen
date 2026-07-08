// WebSocket (RFC 6455 / the HTML WebSocket interface) over the native __ws ops (see
// websocket.rs). The Rust side owns the socket and drives this object's lifecycle through one
// dispatch callback; this class is the EventTarget-shaped front the spec defines.

const CONNECTING = 0;
const OPEN = 1;
const CLOSING = 2;
const CLOSED = 3;

// A subprotocol token must be a non-empty HTTP token (RFC 7230): visible ASCII minus separators.
const PROTO_SEPARATORS = new Set([
  ..."()<>@,;:\\\"/[]?={} \t",
]);
function isValidProtocol(s) {
  if (s.length === 0) return false;
  for (const ch of s) {
    const c = ch.charCodeAt(0);
    if (c < 0x21 || c > 0x7e || PROTO_SEPARATORS.has(ch)) return false;
  }
  return true;
}

function normalizeProtocols(protocols) {
  if (protocols === undefined) return [];
  const list = Array.isArray(protocols) ? protocols : [protocols];
  const seen = new Set();
  for (const p of list) {
    const s = String(p);
    if (!isValidProtocol(s) || seen.has(s)) {
      throw new (globalThis.DOMException ?? SyntaxError)(
        "invalid or duplicate WebSocket subprotocol",
        "SyntaxError",
      );
    }
    seen.add(s);
  }
  return list.map(String);
}

class WebSocket extends EventTarget {
  #url;
  #id;
  #readyState;
  #binaryType;
  #extensions;
  #protocol;
  #bufferedAmount;

  constructor(url, protocols) {
    super();
    if (arguments.length === 0) {
      throw new TypeError("WebSocket constructor requires a url");
    }
    let parsed;
    try {
      parsed = new URL(String(url));
    } catch {
      throw new (globalThis.DOMException ?? SyntaxError)(
        `invalid WebSocket URL: ${url}`,
        "SyntaxError",
      );
    }
    if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
      throw new (globalThis.DOMException ?? SyntaxError)(
        `WebSocket URL scheme must be ws or wss, got '${parsed.protocol}'`,
        "SyntaxError",
      );
    }
    if (parsed.hash !== "") {
      throw new (globalThis.DOMException ?? SyntaxError)(
        "WebSocket URL must not contain a fragment",
        "SyntaxError",
      );
    }
    const list = normalizeProtocols(protocols);

    this.#url = parsed.href;
    this.#readyState = CONNECTING;
    this.#binaryType = "blob";
    this.#extensions = "";
    this.#protocol = "";
    this.#bufferedAmount = 0;

    this.#id = __ws.connect(parsed.href, list.join(", "), (kind, ...args) =>
      this.#onDispatch(kind, args),
    );
  }

  static get CONNECTING() { return CONNECTING; }
  static get OPEN() { return OPEN; }
  static get CLOSING() { return CLOSING; }
  static get CLOSED() { return CLOSED; }
  get CONNECTING() { return CONNECTING; }
  get OPEN() { return OPEN; }
  get CLOSING() { return CLOSING; }
  get CLOSED() { return CLOSED; }

  get url() { return this.#url; }
  get readyState() { return this.#readyState; }
  get bufferedAmount() { return this.#bufferedAmount; }
  get extensions() { return this.#extensions; }
  get protocol() { return this.#protocol; }
  get binaryType() { return this.#binaryType; }
  set binaryType(v) {
    if (v === "blob" || v === "arraybuffer") this.#binaryType = v;
    else throw new SyntaxError("binaryType must be 'blob' or 'arraybuffer'");
  }

  send(data) {
    if (this.#readyState === CONNECTING) {
      throw new (globalThis.DOMException ?? Error)(
        "WebSocket is still CONNECTING",
        "InvalidStateError",
      );
    }
    if (this.#readyState !== OPEN) return; // CLOSING/CLOSED: silently dropped (spec)
    let payload;
    if (typeof data === "string") {
      payload = data;
    } else if (data instanceof ArrayBuffer) {
      payload = new Uint8Array(data.slice(0));
    } else if (ArrayBuffer.isView(data)) {
      payload = new Uint8Array(
        data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength),
      );
    } else if (data && typeof data.arrayBuffer === "function") {
      // Blob: send its bytes once read (ordering preserved per-socket by the task queue).
      data.arrayBuffer().then((buf) => {
        if (this.#readyState === OPEN) __ws.send(this.#id, new Uint8Array(buf));
      });
      return;
    } else {
      payload = String(data);
    }
    __ws.send(this.#id, payload);
  }

  close(code, reason) {
    if (code !== undefined && code !== 1000 && !(code >= 3000 && code <= 4999)) {
      throw new (globalThis.DOMException ?? Error)(
        "close code must be 1000 or in 3000-4999",
        "InvalidAccessError",
      );
    }
    const r = reason === undefined ? "" : String(reason);
    if (new TextEncoder().encode(r).length > 123) {
      throw new (globalThis.DOMException ?? Error)(
        "close reason too long (>123 bytes)",
        "SyntaxError",
      );
    }
    if (this.#readyState === CLOSING || this.#readyState === CLOSED) return;
    this.#readyState = CLOSING;
    __ws.close(this.#id, code ?? 1000, r);
  }

  #fireEvent(type, event) {
    const handler = this[`on${type}`];
    if (typeof handler === "function") {
      try {
        handler.call(this, event);
      } catch (e) {
        reportError(e);
      }
    }
    this.dispatchEvent(event);
  }

  #onDispatch(kind, args) {
    switch (kind) {
      case "open": {
        this.#readyState = OPEN;
        this.#protocol = args[0] ?? "";
        this.#fireEvent("open", new Event("open"));
        break;
      }
      case "text": {
        if (this.#readyState !== OPEN) break;
        this.#fireEvent(
          "message",
          new MessageEvent("message", { data: args[0], origin: this.#url }),
        );
        break;
      }
      case "binary": {
        if (this.#readyState !== OPEN) break;
        const bytes = args[0];
        const data =
          this.#binaryType === "arraybuffer"
            ? bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
            : new Blob([bytes]);
        this.#fireEvent(
          "message",
          new MessageEvent("message", { data, origin: this.#url }),
        );
        break;
      }
      case "close": {
        this.#readyState = CLOSED;
        this.#fireEvent(
          "close",
          new CloseEvent("close", {
            code: args[0] ?? 1005,
            reason: args[1] ?? "",
            wasClean: args[2] ?? false,
          }),
        );
        break;
      }
      case "fail": {
        // A protocol error: an error event, then a non-clean close with the negotiated code.
        this.#readyState = CLOSED;
        this.#fireEvent("error", new Event("error"));
        this.#fireEvent(
          "close",
          new CloseEvent("close", { code: args[0] ?? 1006, reason: args[1] ?? "", wasClean: false }),
        );
        break;
      }
      case "error":
      case "io": {
        // Handshake failure or transport death: error event + 1006 close.
        this.#readyState = CLOSED;
        this.#fireEvent("error", new Event("error"));
        this.#fireEvent(
          "close",
          new CloseEvent("close", { code: 1006, reason: "", wasClean: false }),
        );
        break;
      }
    }
  }
}

// Event-handler IDL attributes (onopen/onmessage/onerror/onclose) — plain writable data
// properties read by #fireEvent, matching the WebSocket interface.
for (const name of ["open", "message", "error", "close"]) {
  Object.defineProperty(WebSocket.prototype, `on${name}`, {
    configurable: true,
    enumerable: true,
    writable: true,
    value: null,
  });
}

globalThis.WebSocket = WebSocket;
