// node:http2 core constants, settings codecs, and session API.

const stream = __builtins.get("stream");
const { Duplex, Readable, Writable } = stream;
const EventEmitter = __builtins.get("events");
const codec = globalThis.__lumenHttp2Codec;

// Full HTTP/2 constants table, copied verbatim from Node v22 (pure data — nghttp2 error codes,
// frame flags, settings ids, default settings, canonical header/method/status name maps).
const constants = {
  "NGHTTP2_ERR_FRAME_SIZE_ERROR": -522, "NGHTTP2_SESSION_SERVER": 0, "NGHTTP2_SESSION_CLIENT": 1, "NGHTTP2_STREAM_STATE_IDLE": 1, "NGHTTP2_STREAM_STATE_OPEN": 2, "NGHTTP2_STREAM_STATE_RESERVED_LOCAL": 3, "NGHTTP2_STREAM_STATE_RESERVED_REMOTE": 4, "NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL": 5, "NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE": 6, "NGHTTP2_STREAM_STATE_CLOSED": 7, "NGHTTP2_FLAG_NONE": 0, "NGHTTP2_FLAG_END_STREAM": 1, "NGHTTP2_FLAG_END_HEADERS": 4, "NGHTTP2_FLAG_ACK": 1, "NGHTTP2_FLAG_PADDED": 8, "NGHTTP2_FLAG_PRIORITY": 32, "DEFAULT_SETTINGS_HEADER_TABLE_SIZE": 4096, "DEFAULT_SETTINGS_ENABLE_PUSH": 1, "DEFAULT_SETTINGS_MAX_CONCURRENT_STREAMS": 4294967295, "DEFAULT_SETTINGS_INITIAL_WINDOW_SIZE": 65535, "DEFAULT_SETTINGS_MAX_FRAME_SIZE": 16384, "DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE": 65535, "DEFAULT_SETTINGS_ENABLE_CONNECT_PROTOCOL": 0, "MAX_MAX_FRAME_SIZE": 16777215, "MIN_MAX_FRAME_SIZE": 16384, "MAX_INITIAL_WINDOW_SIZE": 2147483647, "NGHTTP2_SETTINGS_HEADER_TABLE_SIZE": 1, "NGHTTP2_SETTINGS_ENABLE_PUSH": 2, "NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS": 3, "NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE": 4, "NGHTTP2_SETTINGS_MAX_FRAME_SIZE": 5, "NGHTTP2_SETTINGS_MAX_HEADER_LIST_SIZE": 6, "NGHTTP2_SETTINGS_ENABLE_CONNECT_PROTOCOL": 8, "PADDING_STRATEGY_NONE": 0, "PADDING_STRATEGY_ALIGNED": 1, "PADDING_STRATEGY_MAX": 2, "PADDING_STRATEGY_CALLBACK": 1, "NGHTTP2_NO_ERROR": 0, "NGHTTP2_PROTOCOL_ERROR": 1, "NGHTTP2_INTERNAL_ERROR": 2, "NGHTTP2_FLOW_CONTROL_ERROR": 3, "NGHTTP2_SETTINGS_TIMEOUT": 4, "NGHTTP2_STREAM_CLOSED": 5, "NGHTTP2_FRAME_SIZE_ERROR": 6, "NGHTTP2_REFUSED_STREAM": 7, "NGHTTP2_CANCEL": 8, "NGHTTP2_COMPRESSION_ERROR": 9, "NGHTTP2_CONNECT_ERROR": 10, "NGHTTP2_ENHANCE_YOUR_CALM": 11, "NGHTTP2_INADEQUATE_SECURITY": 12, "NGHTTP2_HTTP_1_1_REQUIRED": 13, "NGHTTP2_DEFAULT_WEIGHT": 16,
  "HTTP2_HEADER_STATUS": ":status", "HTTP2_HEADER_METHOD": ":method", "HTTP2_HEADER_AUTHORITY": ":authority", "HTTP2_HEADER_SCHEME": ":scheme", "HTTP2_HEADER_PATH": ":path", "HTTP2_HEADER_PROTOCOL": ":protocol", "HTTP2_HEADER_ACCEPT_ENCODING": "accept-encoding", "HTTP2_HEADER_ACCEPT_LANGUAGE": "accept-language", "HTTP2_HEADER_ACCEPT_RANGES": "accept-ranges", "HTTP2_HEADER_ACCEPT": "accept", "HTTP2_HEADER_ACCESS_CONTROL_ALLOW_CREDENTIALS": "access-control-allow-credentials", "HTTP2_HEADER_ACCESS_CONTROL_ALLOW_HEADERS": "access-control-allow-headers", "HTTP2_HEADER_ACCESS_CONTROL_ALLOW_METHODS": "access-control-allow-methods", "HTTP2_HEADER_ACCESS_CONTROL_ALLOW_ORIGIN": "access-control-allow-origin", "HTTP2_HEADER_ACCESS_CONTROL_EXPOSE_HEADERS": "access-control-expose-headers", "HTTP2_HEADER_ACCESS_CONTROL_REQUEST_HEADERS": "access-control-request-headers", "HTTP2_HEADER_ACCESS_CONTROL_REQUEST_METHOD": "access-control-request-method", "HTTP2_HEADER_AGE": "age", "HTTP2_HEADER_AUTHORIZATION": "authorization", "HTTP2_HEADER_CACHE_CONTROL": "cache-control", "HTTP2_HEADER_CONNECTION": "connection", "HTTP2_HEADER_CONTENT_DISPOSITION": "content-disposition", "HTTP2_HEADER_CONTENT_ENCODING": "content-encoding", "HTTP2_HEADER_CONTENT_LENGTH": "content-length", "HTTP2_HEADER_CONTENT_TYPE": "content-type", "HTTP2_HEADER_COOKIE": "cookie", "HTTP2_HEADER_DATE": "date", "HTTP2_HEADER_ETAG": "etag", "HTTP2_HEADER_FORWARDED": "forwarded", "HTTP2_HEADER_HOST": "host", "HTTP2_HEADER_IF_MODIFIED_SINCE": "if-modified-since", "HTTP2_HEADER_IF_NONE_MATCH": "if-none-match", "HTTP2_HEADER_IF_RANGE": "if-range", "HTTP2_HEADER_LAST_MODIFIED": "last-modified", "HTTP2_HEADER_LINK": "link", "HTTP2_HEADER_LOCATION": "location", "HTTP2_HEADER_RANGE": "range", "HTTP2_HEADER_REFERER": "referer", "HTTP2_HEADER_SERVER": "server", "HTTP2_HEADER_SET_COOKIE": "set-cookie", "HTTP2_HEADER_STRICT_TRANSPORT_SECURITY": "strict-transport-security", "HTTP2_HEADER_TRANSFER_ENCODING": "transfer-encoding", "HTTP2_HEADER_TE": "te", "HTTP2_HEADER_UPGRADE_INSECURE_REQUESTS": "upgrade-insecure-requests", "HTTP2_HEADER_UPGRADE": "upgrade", "HTTP2_HEADER_USER_AGENT": "user-agent", "HTTP2_HEADER_VARY": "vary", "HTTP2_HEADER_X_CONTENT_TYPE_OPTIONS": "x-content-type-options", "HTTP2_HEADER_X_FRAME_OPTIONS": "x-frame-options", "HTTP2_HEADER_KEEP_ALIVE": "keep-alive", "HTTP2_HEADER_PROXY_CONNECTION": "proxy-connection", "HTTP2_HEADER_X_XSS_PROTECTION": "x-xss-protection", "HTTP2_HEADER_ALT_SVC": "alt-svc", "HTTP2_HEADER_CONTENT_SECURITY_POLICY": "content-security-policy", "HTTP2_HEADER_EARLY_DATA": "early-data", "HTTP2_HEADER_EXPECT_CT": "expect-ct", "HTTP2_HEADER_ORIGIN": "origin", "HTTP2_HEADER_PURPOSE": "purpose", "HTTP2_HEADER_TIMING_ALLOW_ORIGIN": "timing-allow-origin", "HTTP2_HEADER_X_FORWARDED_FOR": "x-forwarded-for", "HTTP2_HEADER_PRIORITY": "priority", "HTTP2_HEADER_ACCEPT_CHARSET": "accept-charset", "HTTP2_HEADER_ACCESS_CONTROL_MAX_AGE": "access-control-max-age", "HTTP2_HEADER_ALLOW": "allow", "HTTP2_HEADER_CONTENT_LANGUAGE": "content-language", "HTTP2_HEADER_CONTENT_LOCATION": "content-location", "HTTP2_HEADER_CONTENT_MD5": "content-md5", "HTTP2_HEADER_CONTENT_RANGE": "content-range", "HTTP2_HEADER_DNT": "dnt", "HTTP2_HEADER_EXPECT": "expect", "HTTP2_HEADER_EXPIRES": "expires", "HTTP2_HEADER_FROM": "from", "HTTP2_HEADER_IF_MATCH": "if-match", "HTTP2_HEADER_IF_UNMODIFIED_SINCE": "if-unmodified-since", "HTTP2_HEADER_MAX_FORWARDS": "max-forwards", "HTTP2_HEADER_PREFER": "prefer", "HTTP2_HEADER_PROXY_AUTHENTICATE": "proxy-authenticate", "HTTP2_HEADER_PROXY_AUTHORIZATION": "proxy-authorization", "HTTP2_HEADER_REFRESH": "refresh", "HTTP2_HEADER_RETRY_AFTER": "retry-after", "HTTP2_HEADER_TRAILER": "trailer", "HTTP2_HEADER_TK": "tk", "HTTP2_HEADER_VIA": "via", "HTTP2_HEADER_WARNING": "warning", "HTTP2_HEADER_WWW_AUTHENTICATE": "www-authenticate", "HTTP2_HEADER_HTTP2_SETTINGS": "http2-settings",
  "HTTP2_METHOD_ACL": "ACL", "HTTP2_METHOD_BASELINE_CONTROL": "BASELINE-CONTROL", "HTTP2_METHOD_BIND": "BIND", "HTTP2_METHOD_CHECKIN": "CHECKIN", "HTTP2_METHOD_CHECKOUT": "CHECKOUT", "HTTP2_METHOD_CONNECT": "CONNECT", "HTTP2_METHOD_COPY": "COPY", "HTTP2_METHOD_DELETE": "DELETE", "HTTP2_METHOD_GET": "GET", "HTTP2_METHOD_HEAD": "HEAD", "HTTP2_METHOD_LABEL": "LABEL", "HTTP2_METHOD_LINK": "LINK", "HTTP2_METHOD_LOCK": "LOCK", "HTTP2_METHOD_MERGE": "MERGE", "HTTP2_METHOD_MKACTIVITY": "MKACTIVITY", "HTTP2_METHOD_MKCALENDAR": "MKCALENDAR", "HTTP2_METHOD_MKCOL": "MKCOL", "HTTP2_METHOD_MKREDIRECTREF": "MKREDIRECTREF", "HTTP2_METHOD_MKWORKSPACE": "MKWORKSPACE", "HTTP2_METHOD_MOVE": "MOVE", "HTTP2_METHOD_OPTIONS": "OPTIONS", "HTTP2_METHOD_ORDERPATCH": "ORDERPATCH", "HTTP2_METHOD_PATCH": "PATCH", "HTTP2_METHOD_POST": "POST", "HTTP2_METHOD_PRI": "PRI", "HTTP2_METHOD_PROPFIND": "PROPFIND", "HTTP2_METHOD_PROPPATCH": "PROPPATCH", "HTTP2_METHOD_PUT": "PUT", "HTTP2_METHOD_REBIND": "REBIND", "HTTP2_METHOD_REPORT": "REPORT", "HTTP2_METHOD_SEARCH": "SEARCH", "HTTP2_METHOD_TRACE": "TRACE", "HTTP2_METHOD_UNBIND": "UNBIND", "HTTP2_METHOD_UNCHECKOUT": "UNCHECKOUT", "HTTP2_METHOD_UNLINK": "UNLINK", "HTTP2_METHOD_UNLOCK": "UNLOCK", "HTTP2_METHOD_UPDATE": "UPDATE", "HTTP2_METHOD_UPDATEREDIRECTREF": "UPDATEREDIRECTREF", "HTTP2_METHOD_VERSION_CONTROL": "VERSION-CONTROL",
  "HTTP_STATUS_CONTINUE": 100, "HTTP_STATUS_SWITCHING_PROTOCOLS": 101, "HTTP_STATUS_PROCESSING": 102, "HTTP_STATUS_EARLY_HINTS": 103, "HTTP_STATUS_OK": 200, "HTTP_STATUS_CREATED": 201, "HTTP_STATUS_ACCEPTED": 202, "HTTP_STATUS_NON_AUTHORITATIVE_INFORMATION": 203, "HTTP_STATUS_NO_CONTENT": 204, "HTTP_STATUS_RESET_CONTENT": 205, "HTTP_STATUS_PARTIAL_CONTENT": 206, "HTTP_STATUS_MULTI_STATUS": 207, "HTTP_STATUS_ALREADY_REPORTED": 208, "HTTP_STATUS_IM_USED": 226, "HTTP_STATUS_MULTIPLE_CHOICES": 300, "HTTP_STATUS_MOVED_PERMANENTLY": 301, "HTTP_STATUS_FOUND": 302, "HTTP_STATUS_SEE_OTHER": 303, "HTTP_STATUS_NOT_MODIFIED": 304, "HTTP_STATUS_USE_PROXY": 305, "HTTP_STATUS_TEMPORARY_REDIRECT": 307, "HTTP_STATUS_PERMANENT_REDIRECT": 308, "HTTP_STATUS_BAD_REQUEST": 400, "HTTP_STATUS_UNAUTHORIZED": 401, "HTTP_STATUS_PAYMENT_REQUIRED": 402, "HTTP_STATUS_FORBIDDEN": 403, "HTTP_STATUS_NOT_FOUND": 404, "HTTP_STATUS_METHOD_NOT_ALLOWED": 405, "HTTP_STATUS_NOT_ACCEPTABLE": 406, "HTTP_STATUS_PROXY_AUTHENTICATION_REQUIRED": 407, "HTTP_STATUS_REQUEST_TIMEOUT": 408, "HTTP_STATUS_CONFLICT": 409, "HTTP_STATUS_GONE": 410, "HTTP_STATUS_LENGTH_REQUIRED": 411, "HTTP_STATUS_PRECONDITION_FAILED": 412, "HTTP_STATUS_PAYLOAD_TOO_LARGE": 413, "HTTP_STATUS_URI_TOO_LONG": 414, "HTTP_STATUS_UNSUPPORTED_MEDIA_TYPE": 415, "HTTP_STATUS_RANGE_NOT_SATISFIABLE": 416, "HTTP_STATUS_EXPECTATION_FAILED": 417, "HTTP_STATUS_TEAPOT": 418, "HTTP_STATUS_MISDIRECTED_REQUEST": 421, "HTTP_STATUS_UNPROCESSABLE_ENTITY": 422, "HTTP_STATUS_LOCKED": 423, "HTTP_STATUS_FAILED_DEPENDENCY": 424, "HTTP_STATUS_TOO_EARLY": 425, "HTTP_STATUS_UPGRADE_REQUIRED": 426, "HTTP_STATUS_PRECONDITION_REQUIRED": 428, "HTTP_STATUS_TOO_MANY_REQUESTS": 429, "HTTP_STATUS_REQUEST_HEADER_FIELDS_TOO_LARGE": 431, "HTTP_STATUS_UNAVAILABLE_FOR_LEGAL_REASONS": 451, "HTTP_STATUS_INTERNAL_SERVER_ERROR": 500, "HTTP_STATUS_NOT_IMPLEMENTED": 501, "HTTP_STATUS_BAD_GATEWAY": 502, "HTTP_STATUS_SERVICE_UNAVAILABLE": 503, "HTTP_STATUS_GATEWAY_TIMEOUT": 504, "HTTP_STATUS_HTTP_VERSION_NOT_SUPPORTED": 505, "HTTP_STATUS_VARIANT_ALSO_NEGOTIATES": 506, "HTTP_STATUS_INSUFFICIENT_STORAGE": 507, "HTTP_STATUS_LOOP_DETECTED": 508, "HTTP_STATUS_BANDWIDTH_LIMIT_EXCEEDED": 509, "HTTP_STATUS_NOT_EXTENDED": 510, "HTTP_STATUS_NETWORK_AUTHENTICATION_REQUIRED": 511,
};

// The sensitiveHeaders symbol (used to flag never-index HPACK headers). Pure marker — real.
const sensitiveHeaders = Symbol("sensitiveHeaders");

function getDefaultSettings() {
  return {
    headerTableSize: 4096,
    enablePush: true,
    initialWindowSize: 65535,
    maxFrameSize: 16384,
    maxConcurrentStreams: 4294967295,
    maxHeaderSize: 65535,
    maxHeaderListSize: 65535,
    enableConnectProtocol: false,
  };
}

// RFC 7540 §6.5.1 SETTINGS payload: a sequence of (id:u16, value:u32) big-endian pairs. Only the
// fields present on `settings` are emitted, matching Node's ordering so the bytes are identical.
function getPackedSettings(settings = {}) {
  const entries = [];
  if (settings.headerTableSize !== undefined) entries.push([1, settings.headerTableSize]);
  if (settings.maxConcurrentStreams !== undefined) entries.push([3, settings.maxConcurrentStreams]);
  if (settings.initialWindowSize !== undefined) entries.push([4, settings.initialWindowSize]);
  if (settings.maxFrameSize !== undefined) entries.push([5, settings.maxFrameSize]);
  if (settings.maxHeaderListSize !== undefined) entries.push([6, settings.maxHeaderListSize]);
  else if (settings.maxHeaderSize !== undefined) entries.push([6, settings.maxHeaderSize]);
  if (settings.enablePush !== undefined) entries.push([2, settings.enablePush ? 1 : 0]);
  if (settings.enableConnectProtocol !== undefined) entries.push([8, settings.enableConnectProtocol ? 1 : 0]);

  const buf = Buffer.alloc(entries.length * 6);
  let off = 0;
  for (const [id, value] of entries) {
    const v = value >>> 0;
    buf[off] = (id >>> 8) & 0xff;
    buf[off + 1] = id & 0xff;
    buf[off + 2] = (v >>> 24) & 0xff;
    buf[off + 3] = (v >>> 16) & 0xff;
    buf[off + 4] = (v >>> 8) & 0xff;
    buf[off + 5] = v & 0xff;
    off += 6;
  }
  return buf;
}

function getUnpackedSettings(buf) {
  const b = Buffer.from(buf);
  if (b.length % 6 !== 0) {
    const err = new RangeError("Packed settings length must be a multiple of six");
    err.code = "ERR_HTTP2_INVALID_PACKED_SETTINGS_LENGTH";
    throw err;
  }
  const out = {};
  for (let i = 0; i < b.length; i += 6) {
    const id = b.readUInt16BE(i);
    const value = b.readUInt32BE(i + 2);
    switch (id) {
      case 1: out.headerTableSize = value; break;
      case 2: out.enablePush = value !== 0; break;
      case 3: out.maxConcurrentStreams = value; break;
      case 4: out.initialWindowSize = value; break;
      case 5: out.maxFrameSize = value; break;
      case 6: out.maxHeaderListSize = value; out.maxHeaderSize = value; break;
      case 8: out.enableConnectProtocol = value !== 0; break;
      default: break; // unknown settings are ignored
    }
  }
  return out;
}

function notSupported() {
  throw new Error("HTTP/2 servers are not supported in lumen yet");
}

class ClientHttp2Stream extends Duplex {
  constructor(session, id, headers, options = {}) {
    super({});
    this.session = session;
    this.id = id;
    this.sentHeaders = headers;
    this.rstCode = 0;
    this.closed = false;
    this.destroyed = false;
    this._headersEnded = !!options.endStream;
  }

  _write(chunk, encoding, callback) {
    if (this.closed || this._headersEnded) { callback(new Error("HTTP/2 stream is not writable")); return; }
    const bytes = chunk instanceof Uint8Array ? Buffer.from(chunk) : Buffer.from(String(chunk), encoding || "utf8");
    this.session._sendData(this.id, bytes, false, callback);
  }

  _final(callback) {
    if (!this._headersEnded && !this.closed) this.session._sendData(this.id, Buffer.alloc(0), true, callback);
    else callback();
  }

  close(code = constants.NGHTTP2_NO_ERROR, callback) {
    if (callback) this.once("close", callback);
    if (!this.closed) {
      this.rstCode = code >>> 0;
      const payload = Buffer.alloc(4);
      payload.writeUInt32BE(this.rstCode, 0);
      this.session._writeFrame(3, 0, this.id, payload);
      this._finish();
    }
    return this;
  }

  _finish() {
    if (this.closed) return;
    this.closed = true;
    this.push(null);
    this.session._streams.delete(this.id);
    queueMicrotask(() => this.emit("close"));
  }
}

class ClientHttp2Session extends EventEmitter {
  constructor(authority, options = {}, listener) {
    super();
    if (listener) this.once("connect", listener);
    this.type = constants.NGHTTP2_SESSION_CLIENT;
    this.connecting = true;
    this.closed = false;
    this.destroyed = false;
    this.encrypted = false;
    this.localSettings = { ...getDefaultSettings(), ...(options.settings || {}) };
    this.remoteSettings = getDefaultSettings();
    this._decoder = new codec.FrameDecoder();
    this._hpack = new codec.Hpack();
    this._streams = new Map();
    this._pending = [];
    this._nextStreamId = 1;
    this._continuation = null;

    const target = new URL(String(authority));
    if (target.protocol !== "http:" && target.protocol !== "https:") throw new TypeError(`Unsupported HTTP/2 protocol ${target.protocol}`);
    this.encrypted = target.protocol === "https:";
    const port = target.port ? Number(target.port) : this.encrypted ? 443 : 80;
    this.origin = target.origin;
    const transport = this.encrypted ? __builtins.get("tls") : __builtins.get("net");
    const socketOptions = this.encrypted
      ? { ...options, host: target.hostname, port, servername: options.servername || target.hostname, ALPNProtocols: ["h2"] }
      : { host: target.hostname, port };
    this.socket = options.createConnection ? options.createConnection(target, options) : transport.connect(socketOptions);
    this.socket.on("data", chunk => this._receive(chunk));
    this.socket.on("error", error => this._fail(error));
    this.socket.on("close", () => this._socketClosed());
    this.socket.once(this.encrypted ? "secureConnect" : "connect", () => {
      if (this.encrypted && this.socket.alpnProtocol !== "h2") {
        const error = new Error(`HTTP/2 ALPN negotiation failed (received ${this.socket.alpnProtocol || "none"})`);
        error.code = "ERR_HTTP2_ALPN_NEGOTIATION_FAILED";
        this._fail(error);
      } else this._connected();
    });
  }

  _connected() {
    if (this.destroyed) return;
    this.socket.write(Buffer.from("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"));
    this._writeFrame(4, 0, 0, getPackedSettings(this.localSettings));
    this.connecting = false;
    const pending = this._pending;
    this._pending = [];
    for (const send of pending) send();
    this.emit("connect", this, this.socket);
  }

  request(headers = {}, options = {}) {
    if (this.closed || this.destroyed) throw new Error("HTTP/2 session is closed");
    const id = this._nextStreamId;
    this._nextStreamId += 2;
    const normalized = { ...headers };
    if (normalized[":method"] === undefined) normalized[":method"] = "GET";
    if (normalized[":path"] === undefined) normalized[":path"] = "/";
    if (normalized[":scheme"] === undefined) normalized[":scheme"] = this.encrypted ? "https" : "http";
    if (normalized[":authority"] === undefined) normalized[":authority"] = new URL(this.origin).host;
    const clientStream = new ClientHttp2Stream(this, id, normalized, options);
    this._streams.set(id, clientStream);
    const send = () => this._sendHeaders(id, normalized, !!options.endStream);
    if (this.connecting) this._pending.push(send); else send();
    return clientStream;
  }

  _sendHeaders(streamId, headers, endStream) {
    const block = this._hpack.encode(headers);
    const max = this.remoteSettings.maxFrameSize || 16384;
    let offset = 0, first = true;
    do {
      const end = Math.min(offset + max, block.length);
      const final = end === block.length;
      const flags = (final ? 4 : 0) | (first && endStream ? 1 : 0);
      this._writeFrame(first ? 1 : 9, flags, streamId, block.subarray(offset, end));
      first = false;
      offset = end;
    } while (offset < block.length);
  }

  _sendData(streamId, bytes, endStream, callback) {
    const max = this.remoteSettings.maxFrameSize || 16384;
    let offset = 0;
    do {
      const end = Math.min(offset + max, bytes.length);
      const final = end === bytes.length;
      this._writeFrame(0, final && endStream ? 1 : 0, streamId, bytes.subarray(offset, end));
      offset = end;
    } while (offset < bytes.length);
    queueMicrotask(callback);
  }

  _writeFrame(type, flags, streamId, payload) {
    if (!this.destroyed) this.socket.write(codec.encodeFrame(type, flags, streamId, payload));
  }

  _receive(chunk) {
    let frames;
    try { frames = this._decoder.push(chunk); }
    catch (error) { this._fail(error); return; }
    for (const frame of frames) {
      try { this._handleFrame(frame); }
      catch (error) { this._fail(error); return; }
    }
  }

  _handleFrame(frame) {
    if (frame.type === 4) {
      if (!(frame.flags & 1)) {
        this.remoteSettings = { ...this.remoteSettings, ...getUnpackedSettings(frame.payload) };
        this._decoder.maxFrameSize = this.localSettings.maxFrameSize;
        this._writeFrame(4, 1, 0, Buffer.alloc(0));
        this.emit("remoteSettings", this.remoteSettings);
      } else this.emit("localSettings", this.localSettings);
      return;
    }
    if (frame.type === 6) {
      if (frame.payload.length !== 8) throw new Error("invalid HTTP/2 PING frame");
      if (!(frame.flags & 1)) this._writeFrame(6, 1, 0, frame.payload);
      else this.emit("ping", frame.payload);
      return;
    }
    if (frame.type === 7) {
      this.closed = true;
      const lastStreamID = frame.payload.length >= 4 ? frame.payload.readUInt32BE(0) & 0x7fffffff : 0;
      const errorCode = frame.payload.length >= 8 ? frame.payload.readUInt32BE(4) : constants.NGHTTP2_PROTOCOL_ERROR;
      this.emit("goaway", errorCode, lastStreamID, frame.payload.subarray(8));
      return;
    }
    const clientStream = this._streams.get(frame.streamId);
    if (!clientStream) return;
    if (frame.type === 1 || frame.type === 9) {
      if (frame.type === 1) this._continuation = { streamId: frame.streamId, chunks: [] };
      if (!this._continuation || this._continuation.streamId !== frame.streamId) throw new Error("invalid HTTP/2 CONTINUATION sequence");
      this._continuation.chunks.push(frame.payload);
      if (frame.flags & 4) {
        const headers = this._hpack.decode(Buffer.concat(this._continuation.chunks));
        this._continuation = null;
        clientStream.emit(clientStream._responded ? "trailers" : "response", headers, frame.flags);
        clientStream._responded = true;
      }
      if (frame.flags & 1) clientStream._finish();
    } else if (frame.type === 0) {
      clientStream.push(frame.payload);
      if (frame.flags & 1) clientStream._finish();
    } else if (frame.type === 3) {
      clientStream.rstCode = frame.payload.readUInt32BE(0);
      clientStream.emit("aborted");
      clientStream._finish();
    }
  }

  ping(payload, callback) {
    if (typeof payload === "function") { callback = payload; payload = Buffer.alloc(8); }
    payload = Buffer.from(payload || Buffer.alloc(8));
    if (payload.length !== 8) throw new RangeError("HTTP/2 ping payload must be 8 bytes");
    if (callback) this.once("ping", response => callback(null, 0, response));
    this._writeFrame(6, 0, 0, payload);
    return true;
  }

  close(callback) {
    if (callback) this.once("close", callback);
    if (!this.closed) {
      this.closed = true;
      this._writeFrame(7, 0, 0, Buffer.alloc(8));
      this.socket.end();
    }
    return this;
  }

  destroy(error) { this._fail(error); return this; }
  ref() { this.socket.ref(); return this; }
  unref() { this.socket.unref(); return this; }

  _fail(error) {
    if (this.destroyed) return;
    this.destroyed = true;
    for (const clientStream of this._streams.values()) clientStream._finish();
    this.socket.destroy();
    if (error) queueMicrotask(() => this.emit("error", error));
  }

  _socketClosed() {
    if (!this.destroyed) this.destroyed = true;
    for (const clientStream of this._streams.values()) clientStream._finish();
    this.emit("close");
  }
}

function connect(authority, options, listener) {
  if (typeof options === "function") { listener = options; options = {}; }
  return new ClientHttp2Session(authority, options || {}, listener);
}
// Server request/response objects only exist attached to a live session. Exported for the class
// surface / instanceof, but constructing one directly has no transport to bind to.
class Http2ServerRequest extends Readable {
  constructor() { super(); notSupported(); }
}
class Http2ServerResponse extends Writable {
  constructor() { super(); notSupported(); }
}

__builtins.set("http2", {
  // real, transport-free
  constants,
  sensitiveHeaders,
  getDefaultSettings,
  getPackedSettings,
  getUnpackedSettings,
  // server sessions are the remaining transport-dependent surface
  createServer: notSupported,
  createSecureServer: notSupported,
  connect,
  performServerHandshake: notSupported,
  Http2ServerRequest,
  Http2ServerResponse,
  ClientHttp2Session,
  ClientHttp2Stream,
});
