// node:net — the transport-free surface. Lumen does not expose raw TCP sockets to JS, so Socket/
// Server/connect stay honest throwing stubs (same STOP-AND-FLAG convention as tls/https). What we
// CAN do for real is everything that is pure address math: the IP validators, the BlockList
// (CIDR/range/address matching), the SocketAddress value type, and the auto-select-family flag
// storage. Those are implemented for real; only the parts that need a live socket throw.

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

// ---- socket-dependent surface (honest stubs) --------------------------------------------------
function notImpl() {
  throw new Error("node:net sockets are not supported in lumen (raw TCP sockets are not exposed to JS)");
}
// net.Stream is a legacy alias of net.Socket.
const Stream = notImpl;

// Internal helpers Node exposes on the module object. _normalizeArgs is pure (it shuffles the
// listen()/connect() overloads into [options, cb]); implement it for real. The handle factory
// needs a live socket, so it throws.
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
  throw new Error("node:net server handles are not supported in lumen (raw TCP sockets are not exposed to JS)");
}
function _setSimultaneousAccepts() { /* no-op: Windows-only accept tuning, inert everywhere else */ }

__builtins.set("net", {
  // real
  isIP, isIPv4, isIPv6,
  BlockList, SocketAddress,
  setDefaultAutoSelectFamily, getDefaultAutoSelectFamily,
  setDefaultAutoSelectFamilyAttemptTimeout, getDefaultAutoSelectFamilyAttemptTimeout,
  _normalizeArgs, _setSimultaneousAccepts,
  // socket-dependent (honest throwing stubs)
  Socket: notImpl, Server: notImpl, Stream,
  connect: notImpl, createConnection: notImpl, createServer: notImpl,
  _createServerHandle,
});
