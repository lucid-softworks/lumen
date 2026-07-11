// node:events — EventEmitter. Pure JS; the base class for streams, http servers, and req/res.
// Covers the surface real-world code (Express and its deps) actually uses: on/once/emit,
// prepend variants, removeListener/removeAllListeners, listeners/rawListeners/listenerCount,
// the `newListener`/`removeListener` meta-events, `error` throw-if-unhandled, and captureRejections
// for async listeners.

const kDefaultMaxListeners = 10;

class EventEmitter {
  constructor(opts) {
    EventEmitter.init.call(this, opts);
  }

  setMaxListeners(n) {
    if (typeof n !== "number" || n < 0 || Number.isNaN(n)) {
      throw new RangeError(`The value of "n" is out of range. Received ${n}`);
    }
    this._maxListeners = n;
    return this;
  }
  getMaxListeners() {
    return this._maxListeners === undefined ? EventEmitter.defaultMaxListeners : this._maxListeners;
  }

  emit(type, ...args) {
    const events = this._events;
    const handler = events === undefined ? undefined : events[type];

    if (handler === undefined) {
      // An unhandled 'error' throws (Node's defining behavior).
      if (type === "error") {
        const err = args[0];
        throw err instanceof Error ? err : Object.assign(new Error(`Unhandled 'error' event`), { context: err });
      }
      return false;
    }

    if (typeof handler === "function") {
      invoke(this, handler, args);
      return true;
    }
    // Copy so removeListener during dispatch doesn't disturb this emit.
    for (const fn of handler.slice()) invoke(this, fn, args);
    return true;
  }

  addListener(type, listener) {
    return addListener(this, type, listener, false);
  }
  on(type, listener) {
    return addListener(this, type, listener, false);
  }
  prependListener(type, listener) {
    return addListener(this, type, listener, true);
  }
  once(type, listener) {
    checkListener(listener);
    return addListener(this, type, onceWrap(this, type, listener), false);
  }
  prependOnceListener(type, listener) {
    checkListener(listener);
    return addListener(this, type, onceWrap(this, type, listener), true);
  }

  removeListener(type, listener) {
    checkListener(listener);
    const events = this._events;
    if (events === undefined) return this;
    const list = events[type];
    if (list === undefined) return this;

    if (list === listener || list.listener === listener) {
      if (--this._eventsCount === 0) this._events = Object.create(null);
      else {
        delete events[type];
        if (events.removeListener) this.emit("removeListener", type, list.listener || list);
      }
      return this;
    }
    if (typeof list === "function") return this;

    let position = -1;
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i] === listener || list[i].listener === listener) {
        position = i;
        break;
      }
    }
    if (position < 0) return this;
    const removed = list[position];
    list.splice(position, 1);
    if (list.length === 1) events[type] = list[0];
    if (events.removeListener) this.emit("removeListener", type, removed.listener || removed);
    return this;
  }
  off(type, listener) {
    return this.removeListener(type, listener);
  }

  removeAllListeners(type) {
    const events = this._events;
    if (events === undefined) return this;
    if (events.removeListener === undefined) {
      if (arguments.length === 0) {
        this._events = Object.create(null);
        this._eventsCount = 0;
      } else if (events[type] !== undefined) {
        if (--this._eventsCount === 0) this._events = Object.create(null);
        else delete events[type];
      }
      return this;
    }
    if (arguments.length === 0) {
      for (const key of Object.keys(events)) {
        if (key === "removeListener") continue;
        this.removeAllListeners(key);
      }
      this.removeAllListeners("removeListener");
      this._events = Object.create(null);
      this._eventsCount = 0;
      return this;
    }
    const listeners = events[type];
    if (typeof listeners === "function") this.removeListener(type, listeners);
    else if (listeners !== undefined) {
      for (let i = listeners.length - 1; i >= 0; i--) this.removeListener(type, listeners[i]);
    }
    return this;
  }

  listeners(type) {
    return this._events === undefined ? [] : unwrapListeners(this._events[type], true);
  }
  rawListeners(type) {
    return this._events === undefined ? [] : unwrapListeners(this._events[type], false);
  }
  listenerCount(type) {
    if (this._events === undefined) return 0;
    const ev = this._events[type];
    if (typeof ev === "function") return 1;
    if (ev !== undefined) return ev.length;
    return 0;
  }
  eventNames() {
    return this._eventsCount > 0 ? Reflect.ownKeys(this._events) : [];
  }
}

EventEmitter.defaultMaxListeners = kDefaultMaxListeners;
EventEmitter.EventEmitter = EventEmitter;
const kCapture = Symbol("kCapture");
EventEmitter.captureRejectionSymbol = Symbol.for("nodejs.rejection");
EventEmitter.errorMonitor = Symbol("events.errorMonitor");
// Legacy domain support was removed from Node; the export survives as a `false` flag.
EventEmitter.usingDomains = false;

// Module-level default for captureRejections (an instance uses it when its options omit the flag).
let captureRejectionsDefault = false;
Object.defineProperty(EventEmitter, "captureRejections", {
  enumerable: true,
  get() { return captureRejectionsDefault; },
  set(v) { captureRejectionsDefault = Boolean(v); },
});

// The constructor body, exposed as `EventEmitter.init` — Node lets subclasses that skip `super()`
// initialize their emitter state via `EventEmitter.init.call(this)`.
EventEmitter.init = function init(opts) {
  if (this._events === undefined || this._events === Object.getPrototypeOf(this)._events) {
    this._events = Object.create(null);
    this._eventsCount = 0;
  }
  this._maxListeners = this._maxListeners ?? undefined;
  this[kCapture] =
    opts && opts.captureRejections !== undefined
      ? Boolean(opts.captureRejections)
      : captureRejectionsDefault;
};

function checkListener(listener) {
  if (typeof listener !== "function") {
    throw new TypeError(`The "listener" argument must be of type function. Received ${typeof listener}`);
  }
}

function addListener(target, type, listener, prepend) {
  checkListener(listener);
  // Lazily initialize `_events`: Express (and other libs) mix EventEmitter.prototype onto a plain
  // object without running the constructor, so on()/emit() must tolerate an uninitialized emitter.
  let events = target._events;
  if (events === undefined) {
    events = target._events = Object.create(null);
    target._eventsCount = 0;
  }

  // 'newListener' fires before the add, with the raw (possibly onceWrapped) listener.
  if (events.newListener !== undefined) {
    target.emit("newListener", type, listener.listener || listener);
  }

  let existing = events[type];
  if (existing === undefined) {
    events[type] = listener;
    ++target._eventsCount;
  } else if (typeof existing === "function") {
    events[type] = prepend ? [listener, existing] : [existing, listener];
  } else if (prepend) {
    existing.unshift(listener);
  } else {
    existing.push(listener);
  }

  // Leak warning (informational — matches Node's soft cap).
  const m = target.getMaxListeners();
  const list = events[type];
  if (m > 0 && Array.isArray(list) && list.length > m && !list.warned) {
    list.warned = true;
    const w = `Possible EventEmitter memory leak detected. ${list.length} ${String(type)} listeners added. Use emitter.setMaxListeners() to increase limit`;
    if (typeof console !== "undefined" && console.error) console.error("MaxListenersExceededWarning:", w);
  }
  return target;
}

function onceWrap(target, type, listener) {
  const state = { fired: false, wrapFn: undefined };
  function wrapped(...args) {
    if (state.fired) return;
    state.fired = true;
    target.removeListener(type, state.wrapFn);
    return Reflect.apply(listener, target, args);
  }
  wrapped.listener = listener;
  state.wrapFn = wrapped;
  return wrapped;
}

function invoke(target, handler, args) {
  const result = Reflect.apply(handler, target, args);
  // captureRejections: route an async listener's rejection to 'error'.
  if (result != null && typeof result.then === "function" && target[kCapture]) {
    result.then(undefined, (err) => target.emit("error", err));
  }
}

function unwrapListeners(list, unwrapOnce) {
  if (list === undefined) return [];
  if (typeof list === "function") return unwrapOnce ? [list.listener || list] : [list];
  return list.map((l) => (unwrapOnce ? l.listener || l : l));
}

// once(emitter, name) -> Promise, resolving with the event args (rejecting on 'error').
function once(emitter, name, options) {
  return new Promise((resolve, reject) => {
    const signal = options && options.signal;
    if (signal && signal.aborted) return reject(abortErr(signal));
    const onEvent = (...args) => {
      emitter.removeListener("error", onError);
      if (signal) signal.removeEventListener("abort", onAbort);
      resolve(args);
    };
    const onError = (err) => {
      emitter.removeListener(name, onEvent);
      if (signal) signal.removeEventListener("abort", onAbort);
      reject(err);
    };
    const onAbort = () => {
      emitter.removeListener(name, onEvent);
      emitter.removeListener("error", onError);
      reject(abortErr(signal));
    };
    emitter.once(name, onEvent);
    if (name !== "error") emitter.once("error", onError);
    if (signal) signal.addEventListener("abort", onAbort, { once: true });
  });
}

function abortErr(signal) {
  return signal.reason || Object.assign(new Error("The operation was aborted"), { name: "AbortError" });
}

EventEmitter.once = once;
EventEmitter.listenerCount = (emitter, type) => emitter.listenerCount(type);
EventEmitter.getEventListeners = (emitter, name) => emitter.listeners(name);

// getMaxListeners/setMaxListeners as static helpers — Node accepts either an EventEmitter or an
// EventTarget. For an EventTarget (no getMaxListeners method) we track the limit on a symbol.
const kMaxListeners = Symbol("events.maxListeners");
EventEmitter.getMaxListeners = (emitterOrTarget) => {
  if (emitterOrTarget && typeof emitterOrTarget.getMaxListeners === "function") {
    return emitterOrTarget.getMaxListeners();
  }
  if (emitterOrTarget && emitterOrTarget[kMaxListeners] !== undefined) {
    return emitterOrTarget[kMaxListeners];
  }
  return EventEmitter.defaultMaxListeners;
};
EventEmitter.setMaxListeners = (n = EventEmitter.defaultMaxListeners, ...eventTargets) => {
  if (typeof n !== "number" || n < 0 || Number.isNaN(n)) {
    throw new RangeError(`The value of "n" is out of range. Received ${n}`);
  }
  if (eventTargets.length === 0) {
    EventEmitter.defaultMaxListeners = n;
    return;
  }
  for (const target of eventTargets) {
    if (target && typeof target.setMaxListeners === "function") target.setMaxListeners(n);
    else if (target) target[kMaxListeners] = n;
  }
};

// addAbortListener(signal, listener) — registers a one-shot 'abort' listener and returns a
// Disposable that removes it. Mirrors Node's helper used to wire AbortSignal into resources.
EventEmitter.addAbortListener = function addAbortListener(signal, listener) {
  if (signal == null || typeof signal.addEventListener !== "function") {
    throw new TypeError('The "signal" argument must be an AbortSignal');
  }
  checkListener(listener);
  let removed = false;
  const handler = (e) => listener.call(signal, e);
  if (signal.aborted) {
    queueMicrotask(() => handler({ type: "abort", target: signal }));
  } else {
    signal.addEventListener("abort", handler, { once: true });
  }
  const dispose = () => {
    if (removed) return;
    removed = true;
    signal.removeEventListener("abort", handler);
  };
  const disposable = { [Symbol.dispose ?? Symbol.for("nodejs.dispose")]: dispose };
  return disposable;
};

// on(emitter, name, options) -> async iterator yielding [...args] for each emitted `name` event.
// Rejects on 'error', honors an AbortSignal, and ends when any `options.close` event fires.
function on(emitter, event, options = {}) {
  const signal = options.signal;
  if (signal && signal.aborted) throw abortErr(signal);
  const closeEvents = options.close ?? [];

  const unconsumed = []; // queued events waiting for a next() call
  const pending = []; // pending next() resolvers waiting for an event
  let finished = false;
  let error = null;

  const iterator = {
    next() {
      if (unconsumed.length > 0) {
        return Promise.resolve(unconsumed.shift());
      }
      if (error) {
        const p = Promise.reject(error);
        error = null;
        return p;
      }
      if (finished) return Promise.resolve({ value: undefined, done: true });
      return new Promise((resolve, reject) => pending.push({ resolve, reject }));
    },
    return() {
      cleanup();
      finished = true;
      for (const p of pending.splice(0)) p.resolve({ value: undefined, done: true });
      return Promise.resolve({ value: undefined, done: true });
    },
    throw(err) {
      cleanup();
      finished = true;
      for (const p of pending.splice(0)) p.reject(err);
      return Promise.reject(err);
    },
    [Symbol.asyncIterator]() { return this; },
  };

  const onEvent = (...args) => {
    const item = { value: args, done: false };
    const p = pending.shift();
    if (p) p.resolve(item);
    else unconsumed.push(item);
  };
  const onError = (err) => {
    const p = pending.shift();
    if (p) p.reject(err);
    else error = err;
    cleanup();
    finished = true;
  };
  const onClose = () => iterator.return();
  const onAbort = () => onError(abortErr(signal));

  function cleanup() {
    emitter.removeListener(event, onEvent);
    emitter.removeListener("error", onError);
    for (const ev of closeEvents) emitter.removeListener(ev, onClose);
    if (signal) signal.removeEventListener("abort", onAbort);
  }

  emitter.on(event, onEvent);
  if (event !== "error") emitter.on("error", onError);
  for (const ev of closeEvents) emitter.on(ev, onClose);
  if (signal) signal.addEventListener("abort", onAbort, { once: true });

  return iterator;
}
EventEmitter.on = on;

// EventEmitterAsyncResource — an EventEmitter that carries an AsyncResource so listeners run in the
// emitter's async context. lumen has no async-context tracking, so the resource is inert, but the
// documented surface (asyncResource/asyncId/triggerAsyncId/emitDestroy) is present and callbacks run.
let earNextId = 1;
class EventEmitterAsyncResource extends EventEmitter {
  constructor(options = {}) {
    super(options);
    const type = (typeof options === "string" ? options : options.name) || "EventEmitterAsyncResource";
    const id = earNextId++;
    const self = this;
    this[kAsyncResource] = {
      type,
      runInAsyncScope(fn, thisArg, ...args) { return Reflect.apply(fn, thisArg, args); },
      asyncId() { return id; },
      triggerAsyncId() { return options.triggerAsyncId ?? 0; },
      emitDestroy() { return this; },
      bind(fn) { return fn; },
    };
    this[kAsyncResource].eventEmitter = self;
  }
  emit(...args) {
    return this[kAsyncResource].runInAsyncScope(super.emit, this, ...args);
  }
  emitDestroy() { this[kAsyncResource].emitDestroy(); }
  get asyncId() { return this[kAsyncResource].asyncId(); }
  get triggerAsyncId() { return this[kAsyncResource].triggerAsyncId(); }
  get asyncResource() { return this[kAsyncResource]; }
}
const kAsyncResource = Symbol("kAsyncResource");
EventEmitter.EventEmitterAsyncResource = EventEmitterAsyncResource;

__builtins.set("events", EventEmitter);
