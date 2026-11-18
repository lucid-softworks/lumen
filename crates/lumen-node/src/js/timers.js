// node:timers and node:timers/promises.
//
// The globals `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/`setImmediate` exist (the
// timer op crate). `setTimeout`/`setInterval` return cancellable ids; `setImmediate` is fire-once
// and returns nothing, so here it is wrapped in a small cancellable handle so `clearImmediate`
// can actually cancel it (never a silent no-op). `timers/promises` layers Node's promise/async-
// iterator forms — with `AbortSignal` support — over those same primitives.

{
  const rawSetTimeout = globalThis.setTimeout;
  const gClearTimeout = globalThis.clearTimeout;
  const rawSetInterval = globalThis.setInterval;
  const gClearInterval = globalThis.clearInterval;
  const gSetImmediate = globalThis.setImmediate;
  const setTimerRef = globalThis.__timers.setRef;
  delete globalThis.__timers;

  // Keep the raw numeric ids in the timer substrate for web embedders. Node exposes handles
  // whose liveness can be changed independently of cancellation, and accepts their numeric
  // primitive in either clear function. Reference state remains observable after cancellation.
  class Timeout {
    constructor(id) { this._id = id; this._referenced = true; }
    ref() { this._referenced = true; setTimerRef(this._id, true); return this; }
    unref() { this._referenced = false; setTimerRef(this._id, false); return this; }
    hasRef() { return this._referenced; }
    [Symbol.toPrimitive]() { return this._id; }
    close() { gClearTimeout(this._id); return this; }
    [Symbol.dispose]() { gClearTimeout(this._id); }
  }
  const gSetTimeout = (callback, delay, ...args) => new Timeout(rawSetTimeout(callback, delay, ...args));
  const gSetInterval = (callback, delay, ...args) => new Timeout(rawSetInterval(callback, delay, ...args));
  globalThis.setTimeout = gSetTimeout;
  globalThis.setInterval = gSetInterval;

  // --- setImmediate / clearImmediate as cancellable handles ------------------------------------
  class Immediate {
    constructor() { this._cleared = false; }
    ref() { return this; }
    unref() { return this; }
    hasRef() { return true; }
  }
  function setImmediate(callback, ...args) {
    if (typeof callback !== "function") throw new TypeError('The "callback" argument must be of type function');
    const handle = new Immediate();
    gSetImmediate(() => { if (!handle._cleared) callback(...args); });
    return handle;
  }
  function clearImmediate(handle) {
    if (handle && typeof handle === "object") handle._cleared = true;
  }

  // --- legacy "unenrolled timer" API (deprecated, still exported) -------------------------------
  // Operates on an object carrying `_onTimeout` and `_idleTimeout` (ms). `active`/`_unrefActive`
  // (re)arm the timer; `enroll`/`unenroll` set/clear its duration.
  function enroll(item, msecs) {
    item._idleTimeout = msecs;
    if (item._idleTimeoutId != null) { gClearTimeout(item._idleTimeoutId); item._idleTimeoutId = null; }
    return item;
  }
  function unenroll(item) {
    if (item && item._idleTimeoutId != null) { gClearTimeout(item._idleTimeoutId); item._idleTimeoutId = null; }
    if (item) item._idleTimeout = -1;
    return item;
  }
  function active(item) {
    if (!item || typeof item._idleTimeout !== "number" || item._idleTimeout < 0) return;
    if (item._idleTimeoutId != null) gClearTimeout(item._idleTimeoutId);
    item._idleTimeoutId = gSetTimeout(() => {
      if (typeof item._onTimeout === "function") item._onTimeout();
    }, item._idleTimeout);
  }
  function _unrefActive(item) {
    active(item);
    if (item && item._idleTimeoutId != null) item._idleTimeoutId.unref();
  }

  // --- timers/promises -------------------------------------------------------------------------
  const abortReason = (signal) => {
    if (signal && signal.reason !== undefined) return signal.reason;
    const e = new Error("The operation was aborted");
    e.name = "AbortError";
    e.code = "ABORT_ERR";
    return e;
  };

  function setTimeoutP(delay = 1, value, options = {}) {
    const signal = options.signal;
    return new Promise((resolve, reject) => {
      if (signal && signal.aborted) { reject(abortReason(signal)); return; }
      const onAbort = () => { gClearTimeout(id); reject(abortReason(signal)); };
      const id = gSetTimeout(() => {
        if (signal) signal.removeEventListener("abort", onAbort);
        resolve(value);
      }, delay);
      if (options.ref === false) id.unref();
      if (signal) signal.addEventListener("abort", onAbort, { once: true });
    });
  }

  function setImmediateP(value, options = {}) {
    const signal = options.signal;
    return new Promise((resolve, reject) => {
      if (signal && signal.aborted) { reject(abortReason(signal)); return; }
      const handle = setImmediate(() => {
        if (signal) signal.removeEventListener("abort", onAbort);
        resolve(value);
      });
      const onAbort = () => { clearImmediate(handle); reject(abortReason(signal)); };
      if (signal) signal.addEventListener("abort", onAbort, { once: true });
    });
  }

  async function* setIntervalP(delay = 1, value, options = {}) {
    const signal = options.signal;
    if (signal && signal.aborted) throw abortReason(signal);
    while (true) {
      await setTimeoutP(delay, undefined, { signal, ref: options.ref });
      yield value;
    }
  }

  const scheduler = {
    wait: (delay, options) => setTimeoutP(delay, undefined, options),
    yield: () => setImmediateP(),
  };

  const timersPromises = {
    setTimeout: setTimeoutP,
    setImmediate: setImmediateP,
    setInterval: setIntervalP,
    scheduler,
  };
  __builtins.set("timers/promises", timersPromises);

  __builtins.set("timers", {
    setTimeout: gSetTimeout,
    clearTimeout: gClearTimeout,
    setInterval: gSetInterval,
    clearInterval: gClearInterval,
    setImmediate,
    clearImmediate,
    active,
    _unrefActive,
    enroll,
    unenroll,
    promises: timersPromises,
  });
}
