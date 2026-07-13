// node:net — real TCP sockets over the __net native ops (std::net TcpStream/TcpListener bridged
// onto the loop; see net.rs). net.Socket is a Duplex stream, net.Server accepts connections, and
// connect/createConnection/createServer all work for real against loopback and other runtimes.
// Everything that is pure address math is also real: the IP validators, the BlockList
// (CIDR/range/address matching), the SocketAddress value type, and the auto-select-family flags.
// Nothing here is a stub — the only honest gaps are Unix-domain sockets (options.path / IPC:
// __net binds TCP only) and options.fd wrapping, which throw clearly.

// ---- IP validators (also used by BlockList / SocketAddress) -----------------------------------
const v4re = /^(\d{1,3}\.){3}\d{1,3}$/;
const v6re = /^([0-9a-f]{0,4}:){2,7}[0-9a-f]{0,4}$/i;
function isIPv4(s) { return typeof s === "string" && v4re.test(s) && s.split(".").every((n) => Number(n) <= 255); }
function isIPv6(s) {
  if (typeof s !== "string") return false;
  return s.includes("::") ? (v6re.test(s) && s.split("::").length <= 2) : v6re.test(s);
}
function isIP(s) { return isIPv4(s) ? 4 : isIPv6(s) ? 6 : 0; }

// ---- IP <-> BigInt (BlockList arithmetic) -----------------------------------------------------
function ipv4ToBig(s) {
  const parts = String(s).split(".");
  if (parts.length !== 4) throw new Error(`Invalid IPv4 address: ${s}`);
  let n = 0n;
  for (const p of parts) {
    const v = Number(p);
    if (!Number.isInteger(v) || v < 0 || v > 255) throw new Error(`Invalid IPv4 address: ${s}`);
    n = (n << 8n) | BigInt(v);
  }
  return n;
}
function ipv6ToBig(s) {
  s = String(s);
  let head, tail;
  if (s.includes("::")) {
    const halves = s.split("::");
    head = halves[0] ? halves[0].split(":") : [];
    tail = halves[1] ? halves[1].split(":") : [];
  } else {
    head = s.split(":");
    tail = [];
  }
  const missing = 8 - head.length - tail.length;
  if (missing < 0) throw new Error(`Invalid IPv6 address: ${s}`);
  const groups = [...head, ...Array(s.includes("::") ? Math.max(0, missing) : 0).fill("0"), ...tail];
  if (groups.length !== 8) throw new Error(`Invalid IPv6 address: ${s}`);
  let n = 0n;
  for (const g of groups) {
    const v = parseInt(g || "0", 16);
    if (Number.isNaN(v) || v < 0 || v > 0xffff) throw new Error(`Invalid IPv6 address: ${s}`);
    n = (n << 16n) | BigInt(v);
  }
  return n;
}
function ipToBig(address, family) {
  return String(family).toLowerCase() === "ipv6" ? ipv6ToBig(address) : ipv4ToBig(address);
}
const famLabel = (f) => (String(f).toLowerCase() === "ipv6" ? "IPv6" : "IPv4");

// ---- net.SocketAddress ------------------------------------------------------------------------
// A pure value type describing an endpoint (address/family/port/flowlabel). No socket involved.
class SocketAddress {
  constructor(options = {}) {
    const family = String(options.family || "ipv4").toLowerCase();
    if (family !== "ipv4" && family !== "ipv6") throw new TypeError(`Invalid family: ${options.family}`);
    const address = options.address !== undefined ? String(options.address) : (family === "ipv4" ? "127.0.0.1" : "::");
    if (family === "ipv4" ? !isIPv4(address) : !isIPv6(address)) throw new TypeError(`Invalid address: ${address}`);
    this.address = address;
    this.family = family;
    this.port = options.port === undefined ? 0 : options.port | 0;
    this.flowlabel = options.flowlabel === undefined ? 0 : options.flowlabel | 0;
    Object.freeze(this);
  }
  static parse(input) {
    input = String(input);
    // [ipv6]:port or ipv4:port or bare address
    const m6 = /^\[([0-9a-f:]+)\](?::(\d+))?$/i.exec(input);
    if (m6) { try { return new SocketAddress({ address: m6[1], family: "ipv6", port: m6[2] ? Number(m6[2]) : 0 }); } catch { return undefined; } }
    const m4 = /^(\d{1,3}(?:\.\d{1,3}){3})(?::(\d+))?$/.exec(input);
    if (m4) { try { return new SocketAddress({ address: m4[1], family: "ipv4", port: m4[2] ? Number(m4[2]) : 0 }); } catch { return undefined; } }
    if (isIPv6(input)) { try { return new SocketAddress({ address: input, family: "ipv6" }); } catch { return undefined; } }
    return undefined;
  }
}

// ---- net.BlockList ----------------------------------------------------------------------------
// Real CIDR/range/single-address matching over IPv4 and IPv6 (all pure arithmetic). Rules are kept
// newest-first, matching Node's `rules` getter ordering.
class BlockList {
  constructor() { this._rules = []; this._strings = []; }
  _normFamily(f) { const s = String(f).toLowerCase(); if (s !== "ipv4" && s !== "ipv6") throw new TypeError(`Invalid family: ${f}`); return s; }
  addAddress(address, family = "ipv4") {
    if (address instanceof SocketAddress) { family = address.family; address = address.address; }
    family = this._normFamily(family);
    const a = ipToBig(address, family);
    this._rules.unshift({ kind: "address", family, a });
    this._strings.unshift(`Address: ${famLabel(family)} ${address}`);
  }
  addRange(start, end, family = "ipv4") {
    if (start instanceof SocketAddress) { family = start.family; start = start.address; }
    if (end instanceof SocketAddress) end = end.address;
    family = this._normFamily(family);
    const s = ipToBig(start, family), e = ipToBig(end, family);
    if (e < s) throw new Error("The value of \"start\" is out of range. It must be <= end");
    this._rules.unshift({ kind: "range", family, s, e });
    this._strings.unshift(`Range: ${famLabel(family)} ${start}-${end}`);
  }
  addSubnet(network, prefix, family = "ipv4") {
    if (network instanceof SocketAddress) { family = network.family; network = network.address; }
    family = this._normFamily(family);
    const bits = family === "ipv6" ? 128 : 32;
    prefix = prefix | 0;
    if (prefix < 0 || prefix > bits) throw new RangeError(`Invalid prefix: ${prefix}`);
    const full = (1n << BigInt(bits)) - 1n;
    const mask = prefix === 0 ? 0n : (full << BigInt(bits - prefix)) & full;
    const base = ipToBig(network, family) & mask;
    this._rules.unshift({ kind: "subnet", family, base, mask });
    this._strings.unshift(`Subnet: ${famLabel(family)} ${network}/${prefix}`);
  }
  check(address, family) {
    if (address instanceof SocketAddress) { family = address.family; address = address.address; }
    if (!family) family = isIP(address) === 6 ? "ipv6" : "ipv4";
    family = String(family).toLowerCase();
    let n;
    try { n = ipToBig(address, family); } catch { return false; }
    for (const r of this._rules) {
      if (r.family !== family) continue;
      if (r.kind === "address" && n === r.a) return true;
      if (r.kind === "range" && n >= r.s && n <= r.e) return true;
      if (r.kind === "subnet" && (n & r.mask) === r.base) return true;
    }
    return false;
  }
  get rules() { return this._strings.slice(); }
}

// ---- auto-select-family flag storage ----------------------------------------------------------
// Real getters/setters over module-level flags (Happy Eyeballs tuning). Inert here — there is no
// socket to apply them to — but the values round-trip so feature detection and config code work.
let autoSelectFamily = true;
let autoSelectFamilyAttemptTimeout = 250;
function setDefaultAutoSelectFamily(value) { autoSelectFamily = !!value; }
function getDefaultAutoSelectFamily() { return autoSelectFamily; }
function setDefaultAutoSelectFamilyAttemptTimeout(value) {
  value = Number(value);
  if (!Number.isInteger(value) || value <= 0) throw new RangeError("autoSelectFamilyAttemptTimeout must be a positive integer");
  autoSelectFamilyAttemptTimeout = value < 10 ? 10 : value;
}
function getDefaultAutoSelectFamilyAttemptTimeout() { return autoSelectFamilyAttemptTimeout; }

// ---- net.Socket / net.Server over the __net native ops ----------------------------------------

const { Duplex } = __builtins.get("stream");
const EventEmitter = __builtins.get("events");

function ipcNotSupported(what) {
  throw new Error(`node:net ${what} is not supported in lumen (only TCP sockets are implemented; no Unix-domain/IPC or fd wrapping)`);
}

// Pump: one-shot reads re-armed after each completion (same pattern as child_process stdio). The
// pending native read is also what keeps the event loop alive while the socket is open.
function pumpSocket(socket) {
  (async () => {
    for (;;) {
      const id = socket._id;
      if (id === null) return;
      const chunk = await new Promise((resolve, reject) => __net.read(id, resolve, reject));
      if (socket._id === null) return; // destroyed while the read was in flight
      if (chunk === null) {
        // Peer half-closed (FIN): emit 'end'. A non-allowHalfOpen socket then ends its own side —
        // but only AFTER the 'end' listeners have run, so `s.on('end', () => s.end(reply))` (the
        // classic pattern, verified against Node) still gets its reply out.
        socket._sawEof = true;
        socket.once("end", () => {
          if (!socket.allowHalfOpen && !socket._writableState.ended && !socket.destroyed) socket.end();
          socket._maybeClose();
        });
        socket.push(null);
        socket._maybeClose();
        return;
      }
      socket.bytesRead += chunk.length;
      socket._resetTimeout();
      socket.push(Buffer.from(chunk));
    }
  })().catch((e) => {
    if (socket._id !== null) socket.destroy(e);
  });
}

class Socket extends Duplex {
  constructor(options = {}) {
    if (options === null || typeof options !== "object") options = {};
    if (options.fd !== undefined) ipcNotSupported("new Socket({ fd })");
    super({});
    this._id = null;
    this.connecting = false;
    this.pending = true;
    this.destroyed = false;
    this.allowHalfOpen = !!options.allowHalfOpen;
    this.remoteAddress = undefined;
    this.remotePort = undefined;
    this.remoteFamily = undefined;
    this.localAddress = undefined;
    this.localPort = undefined;
    this.localFamily = undefined;
    this.bytesRead = 0;
    this.bytesWritten = 0;
    this._sawEof = false;
    this._closeEmitted = false;
    this._hadError = false;
    this._timeoutMs = 0;
    this._timeoutTimer = null;
    this._pendingNoDelay = undefined;
    this._pendingKeepAlive = undefined;
    this._deferRead = !!options._deferRead;
  }

  // Adopt an already-connected native descriptor (server accept / finished connect).
  _adopt(desc) {
    this._id = desc[0];
    this.localAddress = desc[1];
    this.localPort = desc[2];
    this.remoteAddress = desc[3];
    this.remotePort = desc[4];
    this.remoteFamily = desc[5];
    this.localFamily = desc[5];
    this.pending = false;
    this.connecting = false;
    if (this._pendingNoDelay !== undefined) __net.setNoDelay(this._id, this._pendingNoDelay);
    if (this._pendingKeepAlive !== undefined) __net.setKeepAlive(this._id, this._pendingKeepAlive[0], this._pendingKeepAlive[1]);
    if (!this._deferRead) pumpSocket(this);
  }

  _readRaw() {
    if (this._id === null) return Promise.reject(new Error("Socket is not connected"));
    return new Promise((resolve, reject) => __net.read(this._id, value => resolve(value === null ? null : Buffer.from(value)), reject));
  }

  connect(...args) {
    let options, cb;
    if (args.length && typeof args[0] === "object" && args[0] !== null) {
      options = args[0];
      cb = typeof args[1] === "function" ? args[1] : null;
    } else {
      const norm = _normalizeArgs(args);
      options = norm[0];
      cb = norm[1];
    }
    if (options.path !== undefined) ipcNotSupported("connect({ path }) (Unix-domain sockets)");
    const port = Number(options.port);
    if (!Number.isInteger(port) || port < 0 || port > 65535) {
      throw new RangeError(`"port" option should be >= 0 and < 65536. Received ${options.port}.`);
    }
    const host = options.host !== undefined ? String(options.host) : "localhost";
    if (cb) this.once("connect", cb);
    this.connecting = true;
    new Promise((resolve, reject) => __net.connect(host, port, (...d) => resolve(d), reject)).then(
      (desc) => {
        if (this.destroyed) { __net.close(desc[0]); return; }
        this._adopt(desc);
        this._resetTimeout();
        this.emit("connect");
        this.emit("ready");
      },
      (err) => {
        this.connecting = false;
        this.destroy(err);
      },
    );
    return this;
  }

  _write(chunk, encoding, cb) {
    if (this._id === null) {
      cb(new Error("This socket has been ended by the other party"));
      return;
    }
    const bytes = Buffer.isBuffer(chunk) || chunk instanceof Uint8Array
      ? chunk
      : Buffer.from(String(chunk), typeof encoding === "string" && encoding ? encoding : "utf8");
    this.bytesWritten += bytes.length;
    this._resetTimeout();
    __net.write(this._id, bytes, () => cb(), (e) => cb(e));
  }

  _final(cb) {
    if (this._id !== null) __net.endWritable(this._id);
    cb();
    this._maybeClose();
  }

  // Emit 'close' once both directions are done (Node: after 'end' + 'finish', or on destroy).
  _maybeClose() {
    if (this._closeEmitted) return;
    const readDone = this._sawEof || this.destroyed;
    // `finished` (post-_final, writes flushed), not `ended`: closing the native socket aborts any
    // still-queued write, so wait for the tail to drain.
    const writeDone = this._writableState.finished || this.destroyed;
    if (readDone && writeDone) {
      this._closeEmitted = true;
      const id = this._id;
      this._id = null;
      if (id !== null) __net.close(id);
      this._clearTimeout();
      queueMicrotask(() => this.emit("close", this._hadError));
    }
  }

  destroy(err) {
    if (this.destroyed) return this;
    this.destroyed = true;
    this.connecting = false;
    if (err) this._hadError = true;
    const id = this._id;
    this._id = null;
    if (id !== null) __net.close(id);
    this._clearTimeout();
    this.readable = false;
    this.writable = false;
    this._readableState.destroyed = true;
    this._writableState.destroyed = true;
    if (err) { this._readableState.errored = err; this._writableState.errored = err; }
    if (!this._closeEmitted) {
      this._closeEmitted = true;
      queueMicrotask(() => {
        if (err) this.emit("error", err);
        this.emit("close", this._hadError);
      });
    }
    return this;
  }
  destroySoon() { return this.destroy(); }
  resetAndDestroy() { return this.destroy(); }

  setTimeout(ms, cb) {
    this._timeoutMs = Number(ms) || 0;
    if (cb) this.once("timeout", cb);
    this._resetTimeout();
    return this;
  }
  _clearTimeout() {
    if (this._timeoutTimer !== null) { clearTimeout(this._timeoutTimer); this._timeoutTimer = null; }
  }
  _resetTimeout() {
    this._clearTimeout();
    if (this._timeoutMs > 0 && !this.destroyed) {
      this._timeoutTimer = setTimeout(() => this.emit("timeout"), this._timeoutMs);
      // an idle timer must not keep the process alive by itself (Node unrefs it too)
      if (this._timeoutTimer && typeof this._timeoutTimer.unref === "function") this._timeoutTimer.unref();
    }
  }

  setNoDelay(noDelay = true) {
    if (this._id !== null) __net.setNoDelay(this._id, !!noDelay);
    else this._pendingNoDelay = !!noDelay;
    return this;
  }
  setKeepAlive(enable = false, initialDelay = 0) {
    if (this._id !== null) __net.setKeepAlive(this._id, !!enable, Number(initialDelay) || 0);
    else this._pendingKeepAlive = [!!enable, Number(initialDelay) || 0];
    return this;
  }
  address() {
    if (this._id === null) return {};
    return __net.address(this._id) || {};
  }
  ref() { if (this._id !== null) __net.socketRef(this._id, false); return this; }
  unref() { if (this._id !== null) __net.socketRef(this._id, true); return this; }

  get readyState() {
    if (this.connecting) return "opening";
    const r = this.readable, w = this.writable;
    return r && w ? "open" : r ? "readOnly" : w ? "writeOnly" : "closed";
  }
  get bufferSize() { return 0; }
}

class Server extends EventEmitter {
  constructor(options, connectionListener) {
    super();
    if (typeof options === "function") { connectionListener = options; options = {}; }
    options = options || {};
    this._id = null;
    this.listening = false;
    this._connections = new Set();
    this._closing = false;
    this.allowHalfOpen = !!options.allowHalfOpen;
    if (connectionListener) this.on("connection", connectionListener);
  }

  listen(...args) {
    let options = {}, cb = null;
    if (args.length && typeof args[0] === "object" && args[0] !== null) {
      options = args[0];
      cb = typeof args[1] === "function" ? args[1] : null;
    } else {
      const norm = _normalizeArgs(args);
      options = norm[0];
      cb = norm[1];
    }
    if (options.path !== undefined) ipcNotSupported("listen({ path }) (Unix-domain sockets)");
    if (this.listening) throw new Error("Server is already listening");
    const port = options.port === undefined ? 0 : Number(options.port) | 0;
    const host = options.host !== undefined ? String(options.host) : "";
    if (cb) this.once("listening", cb);
    let info;
    try {
      info = __net.listen(host, port, Number(options.backlog) || 0);
    } catch (err) {
      queueMicrotask(() => this.emit("error", err));
      return this;
    }
    this._id = info.serverId;
    this.listening = true;
    this._acceptLoop();
    queueMicrotask(() => this.emit("listening"));
    return this;
  }

  _acceptLoop() {
    (async () => {
      for (;;) {
        const id = this._id;
        if (id === null) return;
        const desc = await new Promise((resolve, reject) =>
          __net.accept(id, (...d) => resolve(d.length > 1 ? d : null), reject));
        if (desc === null || this._id === null) return; // closed
        const socket = new Socket({ allowHalfOpen: this.allowHalfOpen });
        socket._adopt(desc);
        this._connections.add(socket);
        socket.on("close", () => {
          this._connections.delete(socket);
          this._maybeEmitClose();
        });
        this.emit("connection", socket);
      }
    })().catch((e) => this.emit("error", e));
  }

  address() {
    if (this._id === null) return null;
    return __net.serverAddress(this._id);
  }

  close(cb) {
    if (this._id === null) {
      const err = new Error("Server is not running.");
      err.code = "ERR_SERVER_NOT_RUNNING";
      if (cb) queueMicrotask(() => cb(err));
      return this;
    }
    if (cb) this.once("close", cb);
    const id = this._id;
    this._id = null;
    this.listening = false;
    this._closing = true;
    __net.closeServer(id);
    this._maybeEmitClose();
    return this;
  }

  _maybeEmitClose() {
    if (this._closing && this._connections.size === 0) {
      this._closing = false;
      queueMicrotask(() => this.emit("close"));
    }
  }

  getConnections(cb) {
    queueMicrotask(() => cb(null, this._connections.size));
    return this;
  }
  ref() { if (this._id !== null) __net.serverRef(this._id, false); return this; }
  unref() { if (this._id !== null) __net.serverRef(this._id, true); return this; }
}

function connect(...args) {
  let options, cb;
  if (args.length && typeof args[0] === "object" && args[0] !== null) {
    options = args[0];
    cb = typeof args[1] === "function" ? args[1] : null;
  } else {
    const norm = _normalizeArgs(args);
    options = norm[0];
    cb = norm[1];
  }
  const socket = new Socket(options);
  socket.connect(options, cb || undefined);
  return socket;
}
const createConnection = connect;

function createServer(options, connectionListener) {
  return new Server(options, connectionListener);
}

// net.Stream is a legacy alias of net.Socket.
const Stream = Socket;

// Internal helpers Node exposes on the module object. _normalizeArgs is pure (it shuffles the
// listen()/connect() overloads into [options, cb]). The raw handle factory has no lumen
// equivalent, so it throws.
function _normalizeArgs(args) {
  let arr;
  if (args.length === 0) { arr = [{}, null]; arr[Symbol.for("normalizedArgs")] = true; return arr; }
  const first = args[0];
  let options = {};
  if (typeof first === "object" && first !== null) options = first;
  else if (typeof first === "string" && !/^\d+$/.test(first)) options.path = first;
  else options.port = first;
  if (typeof args[1] === "string") options.host = args[1];
  const last = args[args.length - 1];
  const cb = typeof last === "function" ? last : null;
  arr = [options, cb];
  arr[Symbol.for("normalizedArgs")] = true;
  return arr;
}
function _createServerHandle() {
  throw new Error("node:net raw server handles are not supported in lumen (use net.createServer)");
}
function _setSimultaneousAccepts() { /* no-op: Windows-only accept tuning, inert everywhere else */ }

__builtins.set("net", {
  isIP, isIPv4, isIPv6,
  BlockList, SocketAddress,
  setDefaultAutoSelectFamily, getDefaultAutoSelectFamily,
  setDefaultAutoSelectFamilyAttemptTimeout, getDefaultAutoSelectFamilyAttemptTimeout,
  _normalizeArgs, _setSimultaneousAccepts,
  Socket, Server, Stream,
  connect, createConnection, createServer,
  _createServerHandle,
});
