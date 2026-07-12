// node:tls client sockets over the native verified lumen-tls session registry. Server-side TLS
// remains on the fallback module until certificate/key contexts are wired into the backend.
{
  const base = __builtins.get("tls");
  const { Duplex } = __builtins.get("stream");
  const EventEmitter = __builtins.get("events");

  class TLSSocket extends Duplex {
    constructor(socket, options = {}) {
      if (socket && typeof socket === "object" && typeof socket.write === "function") {
        throw new Error("TLSSocket wrapping an existing net.Socket is not supported in lumen; use tls.connect(options)");
      }
      if (socket && typeof socket === "object" && !options) options = socket;
      super({});
      this._id = null;
      this.connecting = false;
      this.pending = true;
      this.destroyed = false;
      this.encrypted = true;
      this.authorized = false;
      this.authorizationError = null;
      this.alpnProtocol = false;
      this.servername = undefined;
      this.remoteAddress = undefined;
      this.remotePort = undefined;
      this.localAddress = undefined;
      this.localPort = undefined;
      this.bytesRead = 0;
      this.bytesWritten = 0;
      this._protocol = null;
      this._cipher = null;
      this._closeEmitted = false;
      this._sawEof = false;
      this._serverSide = false;
    }

    _connect(options, callback) {
      const port = Number(options.port === undefined ? 443 : options.port);
      if (!Number.isInteger(port) || port < 0 || port > 65535) throw new RangeError(`Invalid TLS port ${options.port}`);
      const host = String(options.host || "localhost");
      const servername = String(options.servername || host);
      this._rejectUnauthorized = options.rejectUnauthorized !== false;
      this.servername = servername;
      if (callback) this.once("secureConnect", callback);
      this.connecting = true;
      const alpn = (options.ALPNProtocols || []).map(value => Buffer.isBuffer(value) ? value.toString() : String(value)).join(",");
      new Promise((resolve, reject) => __tls.connect(host, port, servername, alpn, options.rejectUnauthorized !== false, (...descriptor) => resolve(descriptor), reject)).then(
        descriptor => {
          if (this.destroyed) { __tls.close(descriptor[0]); return; }
          this._adopt(descriptor);
          this.emit("connect");
          this.emit("secureConnect");
          this.emit("ready");
        },
        error => this.destroy(error),
      );
      return this;
    }

    _adopt(descriptor, serverSide = false) {
      this._id = descriptor[0];
      this.localAddress = descriptor[1];
      this.localPort = descriptor[2];
      this.remoteAddress = descriptor[3];
      this.remotePort = descriptor[4];
      this._protocol = descriptor[5];
      this._cipher = descriptor[6];
      this.alpnProtocol = descriptor[7] || false;
      this.connecting = false;
      this.pending = false;
      this.authorized = this._rejectUnauthorized !== false;
      this.authorizationError = this.authorized ? null : "UNABLE_TO_VERIFY_LEAF_SIGNATURE";
      this._serverSide = serverSide;
      this._pump();
    }

    async _pump() {
      try {
        while (this._id !== null) {
          const chunk = await new Promise((resolve, reject) => __tls.read(this._id, resolve, reject));
          if (chunk === null || this._id === null) {
            this._sawEof = true;
            this.push(null);
            this._finishClose();
            return;
          }
          const bytes = Buffer.from(chunk);
          this.bytesRead += bytes.length;
          this.push(bytes);
        }
      } catch (error) { this.destroy(error); }
    }

    _write(chunk, encoding, callback) {
      if (this._id === null) { callback(new Error("TLS socket is not connected")); return; }
      const bytes = chunk instanceof Uint8Array ? chunk : Buffer.from(String(chunk), encoding || "utf8");
      __tls.write(this._id, bytes, length => { this.bytesWritten += length; callback(); }, callback);
    }

    _final(callback) {
      callback();
      if (this._serverSide) this._finishClose();
    }

    destroy(error) {
      if (this.destroyed) return this;
      this.destroyed = true;
      this.connecting = false;
      const id = this._id;
      this._id = null;
      if (id !== null) __tls.close(id);
      this.readable = false;
      this.writable = false;
      if (!this._closeEmitted) {
        this._closeEmitted = true;
        queueMicrotask(() => { if (error) this.emit("error", error); this.emit("close", !!error); });
      }
      return this;
    }

    _finishClose() {
      if (this._closeEmitted) return;
      const id = this._id;
      this._id = null;
      if (id !== null) __tls.close(id);
      this._closeEmitted = true;
      queueMicrotask(() => this.emit("close", false));
    }

    address() {
      return this._id === null ? {} : { address: this.localAddress, port: this.localPort, family: this.localAddress && this.localAddress.includes(":") ? "IPv6" : "IPv4" };
    }
    getProtocol() { return this._protocol; }
    getCipher() { return this._cipher ? { name: this._cipher, standardName: this._cipher, version: this._protocol } : null; }
    getPeerCertificate() { return {}; }
    getCertificate() { return {}; }
    getFinished() { return undefined; }
    getPeerFinished() { return undefined; }
    getSession() { return undefined; }
    isSessionReused() { return false; }
    renegotiate(_options, callback) { const error = new Error("TLS renegotiation is not supported"); if (callback) queueMicrotask(() => callback(error)); return false; }
    setServername(name) { this.servername = String(name); }
    setMaxSendFragment() { return false; }
    disableRenegotiation() {}
    ref() { return this; }
    unref() { return this; }
  }

  function normalizeConnectArgs(args) {
    let options = {}, callback;
    if (args[0] && typeof args[0] === "object") {
      options = { ...args[0] };
      callback = typeof args[1] === "function" ? args[1] : undefined;
    } else {
      options.port = args[0];
      if (typeof args[1] === "string") options.host = args[1];
      else if (args[1] && typeof args[1] === "object") Object.assign(options, args[1]);
      callback = args.find(value => typeof value === "function");
    }
    return [options, callback];
  }

  function connect(...args) {
    const [options, callback] = normalizeConnectArgs(args);
    return new TLSSocket()._connect(options, callback);
  }

  class SecureContext {
    constructor(options = {}) { this.context = { ...options }; }
  }
  function createSecureContext(options) { return new SecureContext(options); }

  class Server extends EventEmitter {
    constructor(options, listener) {
      super();
      if (typeof options === "function") { listener = options; options = {}; }
      this.options = options || {};
      this._id = null;
      this._address = null;
      this.listening = false;
      this._connections = new Set();
      if (listener) this.on("secureConnection", listener);
    }
    listen(...args) {
      let port = 0, host = "0.0.0.0", callback;
      if (args[0] && typeof args[0] === "object") {
        port = Number(args[0].port || 0);
        host = String(args[0].host || host);
        callback = typeof args[1] === "function" ? args[1] : undefined;
      } else {
        port = Number(args[0] || 0);
        if (typeof args[1] === "string") host = args[1];
        callback = args.find(value => typeof value === "function");
      }
      if (callback) this.once("listening", callback);
      const context = this.options.secureContext && this.options.secureContext.context || this.options;
      const cert = Array.isArray(context.cert) ? context.cert[0] : context.cert;
      const key = Array.isArray(context.key) ? context.key[0] : context.key;
      if (cert == null || key == null) throw new TypeError("tls.createServer requires cert and key options");
      const alpn = (context.ALPNProtocols || []).map(value => Buffer.isBuffer(value) ? value.toString() : String(value)).join(",");
      const info = __tls.listen(host, port, Buffer.from(cert), Buffer.from(key), alpn);
      this._id = info.serverId;
      this._address = { address: info.address, port: info.port, family: info.address.includes(":") ? "IPv6" : "IPv4" };
      this.listening = true;
      queueMicrotask(() => this.emit("listening"));
      this._acceptLoop();
      return this;
    }
    async _acceptLoop() {
      while (this._id !== null) {
        try {
          const descriptor = await new Promise((resolve, reject) => __tls.accept(this._id, (...values) => resolve(values.length ? values : null), reject));
          if (!descriptor || this._id === null) return;
          const socket = new TLSSocket();
          socket._adopt(descriptor, true);
          this._connections.add(socket);
          socket.once("close", () => this._connections.delete(socket));
          this.emit("secureConnection", socket);
        } catch (error) {
          if (this._id !== null) this.emit("tlsClientError", error);
        }
      }
    }
    address() { return this._address; }
    close(callback) {
      if (callback) this.once("close", callback);
      const id = this._id;
      this._id = null;
      this.listening = false;
      if (id !== null) __tls.closeServer(id);
      queueMicrotask(() => this.emit("close"));
      return this;
    }
    getConnections(callback) { queueMicrotask(() => callback(null, this._connections.size)); return this; }
    ref() { return this; }
    unref() { return this; }
  }
  function createServer(options, listener) { return new Server(options, listener); }

  __builtins.set("tls", { ...base, connect, TLSSocket, Server, createServer, SecureContext, createSecureContext });
}
