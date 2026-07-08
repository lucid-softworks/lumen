// Channel messaging (HTML §9.4) + the web event classes beyond the DOM core: MessageEvent,
// CloseEvent, ErrorEvent, PromiseRejectionEvent, MessageChannel/MessagePort (in-process
// entangled pair), BroadcastChannel (same-realm), and AbortSignal.any. Message data is
// serialized SYNCHRONOUSLY at postMessage time via structuredClone and delivered as a task;
// each BroadcastChannel receiver gets its own clone (mutation isolation, like the spec's
// per-destination deserialize).

class MessageEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    init = init && typeof init === "object" ? init : {};
    this.data = "data" in init ? init.data : null;
    this.origin = "origin" in init ? String(init.origin) : "";
    this.lastEventId = "lastEventId" in init ? String(init.lastEventId) : "";
    this.source = "source" in init ? init.source : null;
    this.ports = Object.freeze("ports" in init && init.ports ? [...init.ports] : []);
  }
}

class CloseEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    init = init && typeof init === "object" ? init : {};
    const code = Number(init.code);
    this.wasClean = !!init.wasClean;
    this.code = Number.isFinite(code) ? code & 0xffff : 0;
    this.reason = "reason" in init ? String(init.reason) : "";
  }
}

class ErrorEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    init = init && typeof init === "object" ? init : {};
    const num = (v) => (Number.isFinite(Number(v)) ? Number(v) >>> 0 : 0);
    this.message = "message" in init ? String(init.message) : "";
    this.filename = "filename" in init ? String(init.filename) : "";
    this.lineno = "lineno" in init ? num(init.lineno) : 0;
    this.colno = "colno" in init ? num(init.colno) : 0;
    this.error = "error" in init ? init.error : undefined;
  }
}

class PromiseRejectionEvent extends Event {
  constructor(type, init) {
    if (!init || typeof init !== "object" || !("promise" in init)) {
      throw new TypeError("PromiseRejectionEvent requires an init with a promise");
    }
    super(type, init);
    this.promise = init.promise;
    this.reason = "reason" in init ? init.reason : undefined;
  }
}

// Event-handler IDL attribute (`onmessage` and friends): the assigned function participates in
// dispatch as a real listener, so handler + addEventListener fire in registration order.
function defineEventHandler(proto, name, afterSet) {
  const listeners = new WeakMap();
  const handlers = new WeakMap();
  Object.defineProperty(proto, `on${name}`, {
    configurable: true,
    get() {
      return handlers.get(this) ?? null;
    },
    set(fn) {
      const old = listeners.get(this);
      if (old) this.removeEventListener(name, old);
      if (typeof fn === "function") {
        handlers.set(this, fn);
        const wrapped = (e) => fn.call(this, e);
        listeners.set(this, wrapped);
        this.addEventListener(name, wrapped);
      } else {
        handlers.delete(this);
        listeners.delete(this);
      }
      if (afterSet) afterSet(this);
    },
  });
}

const kPortCreate = Symbol("MessagePort-create");

class MessagePort extends EventTarget {
  constructor(token) {
    if (token !== kPortCreate) throw new TypeError("Illegal constructor");
    super();
    this._other = null;
    this._queue = [];
    this._started = false;
    this._closed = false;
  }
  postMessage(message, options) {
    if (this._closed) return;
    const transfer = Array.isArray(options)
      ? options
      : options && typeof options === "object" && options.transfer
        ? options.transfer
        : [];
    // Serialize NOW (spec order — later mutations of `message` are invisible to the receiver);
    // a DataCloneError propagates to the caller.
    const data = transfer.length ? structuredClone(message, { transfer }) : structuredClone(message);
    const target = this._other;
    if (!target || target._closed) return;
    setTimeout(() => target._deliver(data), 0);
  }
  start() {
    if (this._started || this._closed) return;
    this._started = true;
    const queued = this._queue.splice(0);
    for (const data of queued) {
      setTimeout(() => this._dispatch(data), 0);
    }
  }
  close() {
    this._closed = true;
    if (this._other) this._other._other = null;
    this._other = null;
  }
  _deliver(data) {
    if (this._closed) return;
    if (!this._started) {
      this._queue.push(data);
      return;
    }
    this._dispatch(data);
  }
  _dispatch(data) {
    if (this._closed) return;
    this.dispatchEvent(new MessageEvent("message", { data }));
  }
}
defineEventHandler(MessagePort.prototype, "message", (port) => port.start());
defineEventHandler(MessagePort.prototype, "messageerror");

class MessageChannel {
  constructor() {
    const port1 = new MessagePort(kPortCreate);
    const port2 = new MessagePort(kPortCreate);
    port1._other = port2;
    port2._other = port1;
    this.port1 = port1;
    this.port2 = port2;
  }
}

// Same-realm broadcast registry: name -> Set of live channels.
const broadcastChannels = new Map();

class BroadcastChannel extends EventTarget {
  constructor(name) {
    if (arguments.length === 0) {
      throw new TypeError("BroadcastChannel requires a name");
    }
    super();
    this.name = String(name);
    this._closed = false;
    let set = broadcastChannels.get(this.name);
    if (!set) {
      set = new Set();
      broadcastChannels.set(this.name, set);
    }
    set.add(this);
  }
  postMessage(message) {
    if (this._closed) {
      throw new DOMException("BroadcastChannel is closed", "InvalidStateError");
    }
    const data = structuredClone(message); // serialize now, once
    const peers = [...(broadcastChannels.get(this.name) ?? [])].filter(
      (c) => c !== this && !c._closed,
    );
    setTimeout(() => {
      for (const peer of peers) {
        if (peer._closed) continue;
        peer.dispatchEvent(new MessageEvent("message", { data: structuredClone(data) }));
      }
    }, 0);
  }
  close() {
    this._closed = true;
    const set = broadcastChannels.get(this.name);
    if (set) {
      set.delete(this);
      if (set.size === 0) broadcastChannels.delete(this.name);
    }
  }
}
defineEventHandler(BroadcastChannel.prototype, "message");
defineEventHandler(BroadcastChannel.prototype, "messageerror");

AbortSignal.any = function any(signals) {
  const list = [...signals];
  const controller = new AbortController();
  for (const s of list) {
    if (s.aborted) {
      controller.abort(s.reason);
      return controller.signal;
    }
  }
  for (const s of list) {
    s.addEventListener("abort", () => controller.abort(s.reason), { once: true });
  }
  return controller.signal;
};

globalThis.MessageEvent = MessageEvent;
globalThis.CloseEvent = CloseEvent;
globalThis.ErrorEvent = ErrorEvent;
globalThis.PromiseRejectionEvent = PromiseRejectionEvent;
globalThis.MessagePort = MessagePort;
globalThis.MessageChannel = MessageChannel;
globalThis.BroadcastChannel = BroadcastChannel;
