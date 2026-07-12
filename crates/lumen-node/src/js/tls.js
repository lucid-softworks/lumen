// node:tls client sockets over the native verified lumen-tls session registry. Server-side TLS
// remains on the fallback module until certificate/key contexts are wired into the backend.
{
  const base = __builtins.get("tls");
  const { Duplex } = __builtins.get("stream");

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
    }

    _connect(options, callback) {
      const port = Number(options.port === undefined ? 443 : options.port);
      if (!Number.isInteger(port) || port < 0 || port > 65535) throw new RangeError(`Invalid TLS port ${options.port}`);
      const host = String(options.host || "localhost");
      const servername = String(options.servername || host);
      this.servername = servername;
      if (callback) this.once("secureConnect", callback);
      this.connecting = true;
      new Promise((resolve, reject) => __tls.connect(host, port, servername, (...descriptor) => resolve(descriptor), reject)).then(
        descriptor => {
          if (this.destroyed) { __tls.close(descriptor[0]); return; }
          this._id = descriptor[0];
          this.localAddress = descriptor[1];
          this.localPort = descriptor[2];
          this.remoteAddress = descriptor[3];
          this.remotePort = descriptor[4];
          this._protocol = descriptor[5];
          this._cipher = descriptor[6];
          this.connecting = false;
          this.pending = false;
          this.authorized = true;
          this.emit("connect");
          this.emit("secureConnect");
          this.emit("ready");
          this._pump();
        },
        error => this.destroy(error),
      );
      return this;
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

    _final(callback) { callback(); }

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

  __builtins.set("tls", { ...base, connect, TLSSocket });
}
