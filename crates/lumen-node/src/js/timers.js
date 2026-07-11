// node:timers and node:timers/promises.
//
// The globals `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/`setImmediate` exist (the
// timer op crate). `setTimeout`/`setInterval` return cancellable ids; `setImmediate` is fire-once
// and returns nothing, so here it is wrapped in a small cancellable handle so `clearImmediate`
// can actually cancel it (never a silent no-op). `timers/promises` layers Node's promise/async-
// iterator forms — with `AbortSignal` support — over those same primitives.

{
  const gSetTimeout = globalThis.setTimeout;
  const gClearTimeout = globalThis.clearTimeout;
  const gSetInterval = globalThis.setInterval;
  const gClearInterval = globalThis.clearInterval;
  const gSetImmediate = globalThis.setImmediate;

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
  const _unrefActive = active;

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
      await setTimeoutP(delay, undefined, { signal });
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
