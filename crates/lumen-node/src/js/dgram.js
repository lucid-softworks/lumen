// node:dgram — real UDP over the __udp native ops (std::net::UdpSocket bridged onto the loop;
// see net.rs). createSocket('udp4'|'udp6'), bind (port/host/options object + callback), send
// (buffer or string, offset/length variants), 'message' with rinfo {address, family, port, size},
// 'listening'/'error'/'close', address(), setBroadcast/setTTL, setMulticastTTL (udp4 only —
// udp6 throws, std has no IPv6 multicast-hops setter), setMulticastLoopback, add/dropMembership,
// ref/unref, connect/disconnect/remoteAddress (Node's connected-UDP emulation over sendto).
// Honest throws: setMulticastInterface (std has no IP_MULTICAST_IF), source-specific membership
// (no SSM in std), get/set send/recv buffer sizes (no SO_SNDBUF/SO_RCVBUF in std). The bind
// option `reuseAddr` is accepted but inert (std binds without exposing it).

const EventEmitter = __builtins.get("events");

function normalizeSendArgs(args) {
  // send(msg[, offset, length][, port][, address][, callback]) — on a connected socket the
  // port/address slots are omitted, so (msg, offset, length, callback) is valid (Node-verified).
  const [msg, a1, a2, a3, a4, a5] = args;
  let offset, length, port, address, callback;
  if (typeof a1 === "number" && typeof a2 === "number") {
    offset = a1; length = a2; port = a3; address = a4; callback = a5;
  } else {
    port = a1; address = a2; callback = a3;
  }
  if (typeof address === "function") { callback = address; address = undefined; }
  if (typeof port === "function") { callback = port; port = undefined; }
  return { msg, offset, length, port, address, callback };
}

function toBytes(msg, offset, length) {
  let buf;
  if (typeof msg === "string") buf = Buffer.from(msg);
  else if (Buffer.isBuffer(msg)) buf = msg;
  else if (msg instanceof Uint8Array) buf = Buffer.from(msg.buffer, msg.byteOffset, msg.byteLength);
  else if (Array.isArray(msg)) buf = Buffer.concat(msg.map((m) => (typeof m === "string" ? Buffer.from(m) : Buffer.from(m))));
  else throw new TypeError('The "msg" argument must be of type string or an instance of Buffer, TypedArray, or DataView');
  if (offset !== undefined && length !== undefined) buf = buf.subarray(offset, offset + length);
  return buf;
}

class Socket extends EventEmitter {
  constructor(type, listener) {
    super();
    let options = type;
    if (typeof type === "string") options = { type };
    if (!options || (options.type !== "udp4" && options.type !== "udp6")) {
      const e = new Error("Bad socket type specified. Valid types are: udp4, udp6");
      e.code = "ERR_SOCKET_BAD_TYPE";
      throw e;
    }
    this.type = options.type;
    this._id = null;
    this._bindState = 0; // 0 unbound, 1 binding, 2 bound
    this._closed = false;
    this._remote = null;
    if (listener) this.on("message", listener);
  }

  bind(...args) {
    if (this._bindState !== 0) {
      const e = new Error("Socket is already bound");
      e.code = "ERR_SOCKET_ALREADY_BOUND";
      throw e;
    }
    let port = 0, address = "", cb = null;
    if (typeof args[0] === "object" && args[0] !== null) {
      port = Number(args[0].port) || 0;
      address = args[0].address !== undefined ? String(args[0].address) : "";
      cb = typeof args[1] === "function" ? args[1] : null;
    } else {
      if (typeof args[0] === "number" || typeof args[0] === "string") port = Number(args[0]) || 0;
      if (typeof args[1] === "string") address = args[1];
      const last = args[args.length - 1];
      if (typeof last === "function") cb = last;
    }
    if (cb) this.once("listening", cb);
    this._bindState = 1;
    let info;
    try {
      info = __udp.bind(this.type, address, port, 0);
    } catch (err) {
      this._bindState = 0;
      queueMicrotask(() => this.emit("error", err));
      return this;
    }
    this._id = info.socketId;
    this._bindState = 2;
    this._recvLoop();
    queueMicrotask(() => this.emit("listening"));
    return this;
  }

  _ensureBound() {
    // Node auto-binds an unbound socket to an ephemeral port on first send/connect.
    if (this._bindState === 0) this.bind(0);
  }

  _recvLoop() {
    (async () => {
      for (;;) {
        const id = this._id;
        if (id === null) return;
        const msg = await new Promise((resolve, reject) => __udp.recv(id, resolve, reject));
        if (msg === null || this._id === null) return; // closed
        const { data, address, port, family, size } = msg;
        this.emit("message", Buffer.from(data), { address, family, port, size });
      }
    })().catch((e) => {
      if (this._id !== null) this.emit("error", e);
    });
  }

  send(...args) {
    const { msg, offset, length, port, address, callback } = normalizeSendArgs(args);
    const fail = (e) => {
      if (callback) queueMicrotask(() => callback(e));
      else throw e;
    };
    if (this._closed) {
      // Node throws synchronously here even when a callback is given (healthCheck).
      const e = new Error("Not running");
      e.code = "ERR_SOCKET_DGRAM_NOT_RUNNING";
      throw e;
    }
    let bytes;
    try {
      bytes = toBytes(msg, offset, length);
    } catch (e) {
      return fail(e);
    }
    let dportRaw = port, daddr = address;
    if (this._remote) {
      if (port !== undefined) {
        const e = new Error("Already connected");
        e.code = "ERR_SOCKET_DGRAM_IS_CONNECTED";
        return fail(e);
      }
      dportRaw = this._remote.port;
      daddr = this._remote.address;
    }
    const dport = Number(dportRaw);
    if (!Number.isInteger(dport) || dport <= 0 || dport > 65535) {
      const e = new RangeError(`Port should be > 0 and < 65536. Received ${dportRaw}.`);
      e.code = "ERR_SOCKET_BAD_PORT";
      return fail(e);
    }
    const host = daddr === undefined || daddr === "" ? (this.type === "udp6" ? "::1" : "127.0.0.1") : String(daddr);
    this._ensureBound();
    new Promise((resolve, reject) => __udp.send(this._id, bytes, dport, host, resolve, reject)).then(
      (n) => { if (callback) callback(null, n); },
      (err) => { if (callback) callback(err); else this.emit("error", err); },
    );
  }

  connect(port, address, cb) {
    if (typeof address === "function") { cb = address; address = undefined; }
    if (this._remote) {
      const e = new Error("Already connected");
      e.code = "ERR_SOCKET_DGRAM_IS_CONNECTED";
      throw e;
    }
    this._ensureBound();
    this._remote = {
      address: address !== undefined ? String(address) : (this.type === "udp6" ? "::1" : "127.0.0.1"),
      family: this.type === "udp6" ? "IPv6" : "IPv4",
      port: Number(port),
    };
    if (cb) this.once("connect", cb);
    queueMicrotask(() => this.emit("connect"));
    return this;
  }
  disconnect() {
    if (!this._remote) {
      const e = new Error("Not connected");
      e.code = "ERR_SOCKET_DGRAM_NOT_CONNECTED";
      throw e;
    }
    this._remote = null;
  }
  remoteAddress() {
    if (!this._remote) {
      const e = new Error("Not connected");
      e.code = "ERR_SOCKET_DGRAM_NOT_CONNECTED";
      throw e;
    }
    return { ...this._remote };
  }

  address() {
    if (this._id === null) {
      const e = new Error("getsockname EBADF");
      e.code = "EBADF";
      throw e;
    }
    return __udp.address(this._id);
  }

  close(cb) {
    if (this._closed) {
      const e = new Error("Not running");
      e.code = "ERR_SOCKET_DGRAM_NOT_RUNNING";
      if (cb) { queueMicrotask(() => cb(e)); return this; }
      throw e;
    }
    if (cb) this.once("close", cb);
    this._closed = true;
    const id = this._id;
    this._id = null;
    if (id !== null) __udp.close(id);
    queueMicrotask(() => this.emit("close"));
    return this;
  }

  _op(name, ...opArgs) {
    if (this._id === null) {
      const e = new Error(`${name} EBADF`);
      e.code = "EBADF";
      throw e;
    }
    return __udp[name](this._id, ...opArgs);
  }
  setBroadcast(flag) { this._op("setBroadcast", !!flag); }
  setTTL(ttl) {
    ttl = Number(ttl);
    if (!Number.isInteger(ttl) || ttl < 1 || ttl > 255) {
      const e = new RangeError(`The value of "ttl" is out of range. It must be >= 1 and <= 255. Received ${ttl}`);
      e.code = "ERR_OUT_OF_RANGE";
      throw e;
    }
    this._op("setTTL", ttl);
    return ttl;
  }
  setMulticastTTL(ttl) {
    ttl = Number(ttl);
    if (!Number.isInteger(ttl) || ttl < 0 || ttl > 255) {
      const e = new RangeError(`The value of "ttl" is out of range. It must be >= 0 and <= 255. Received ${ttl}`);
      e.code = "ERR_OUT_OF_RANGE";
      throw e;
    }
    this._op("setMulticastTTL", ttl);
    return ttl;
  }
  setMulticastLoopback(flag) { this._op("setMulticastLoopback", !!flag); return !!flag; }
  setMulticastInterface(multicastInterface) {
    this._ensureBound();
    this._op("setMulticastInterface", String(multicastInterface));
    return this;
  }
  addMembership(multicastAddress, multicastInterface) {
    this._ensureBound();
    this._op("addMembership", String(multicastAddress), multicastInterface === undefined ? undefined : String(multicastInterface));
  }
  dropMembership(multicastAddress, multicastInterface) {
    this._op("dropMembership", String(multicastAddress), multicastInterface === undefined ? undefined : String(multicastInterface));
  }
  addSourceSpecificMembership() {
    throw new Error("dgram.addSourceSpecificMembership is not supported in lumen (std exposes no source-specific multicast)");
  }
  dropSourceSpecificMembership() {
    throw new Error("dgram.dropSourceSpecificMembership is not supported in lumen (std exposes no source-specific multicast)");
  }
  ref() { if (this._id !== null) __udp.udpRef(this._id, false); return this; }
  unref() { if (this._id !== null) __udp.udpRef(this._id, true); return this; }
  getRecvBufferSize() { this._ensureBound(); return this._op("getBufferSize", true); }
  getSendBufferSize() { this._ensureBound(); return this._op("getBufferSize", false); }
  setRecvBufferSize(size) { return this._setBufferSize(true, size); }
  setSendBufferSize(size) { return this._setBufferSize(false, size); }
  _setBufferSize(receive, size) {
    size = Number(size);
    if (!Number.isInteger(size) || size <= 0 || size > 0x7fffffff) {
      const error = new RangeError(`The value of "size" is out of range. It must be a positive integer. Received ${size}`);
      error.code = "ERR_OUT_OF_RANGE";
      throw error;
    }
    this._ensureBound();
    this._op("setBufferSize", receive, size);
  }
}

function createSocket(type, listener) {
  return new Socket(type, listener);
}

function _createSocketHandle() {
  throw new Error("node:dgram raw socket handles are not supported in lumen (use dgram.createSocket)");
}

__builtins.set("dgram", {
  Socket,
  createSocket,
  _createSocketHandle,
});
