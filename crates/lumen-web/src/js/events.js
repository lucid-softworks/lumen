// DOMException + the DOM event model, flattened: one target, no tree, no capture phase.

class DOMException extends Error {
  constructor(message = "", name = "Error") {
    super(message);
    this.name = String(name);
  }
}

class Event {
  constructor(type, init = {}) {
    if (arguments.length === 0) {
      throw new TypeError("Event constructor requires a type");
    }
    init = init && typeof init === "object" ? init : {};
    this.type = String(type);
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.composed = !!init.composed;
    this.defaultPrevented = false;
    this.target = null;
    this.currentTarget = null;
    this.eventPhase = Event.AT_TARGET;
    this.isTrusted = false;
    this.timeStamp = performance.now();
    this._propagationStopped = false;
    this._immediateStopped = false;
  }
  preventDefault() {
    if (this.cancelable) this.defaultPrevented = true;
  }
  stopPropagation() {
    this._propagationStopped = true;
  }
  stopImmediatePropagation() {
    this._propagationStopped = true;
    this._immediateStopped = true;
  }
}
Event.NONE = 0;
Event.CAPTURING_PHASE = 1;
Event.AT_TARGET = 2;
Event.BUBBLING_PHASE = 3;

class CustomEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    this.detail = init && "detail" in init ? init.detail : null;
  }
}

class EventTarget {
  constructor() {
    this._listeners = new Map();
  }
  addEventListener(type, callback, options = {}) {
    if (callback === null || callback === undefined) return;
    if (typeof options === "boolean") options = { capture: options };
    options = options && typeof options === "object" ? options : {};
    const key = String(type);
    let list = this._listeners.get(key);
    if (!list) {
      list = [];
      this._listeners.set(key, list);
    }
    const capture = !!options.capture;
    if (list.some((l) => l.callback === callback && l.capture === capture)) return;
    if (options.signal) {
      if (options.signal.aborted) return;
      options.signal.addEventListener("abort", () => {
        this.removeEventListener(key, callback, { capture });
      });
    }
    list.push({ callback, capture, once: !!options.once, removed: false });
  }
  removeEventListener(type, callback, options = {}) {
    if (typeof options === "boolean") options = { capture: options };
    options = options && typeof options === "object" ? options : {};
    const list = this._listeners.get(String(type));
    if (!list) return;
    const capture = !!options.capture;
    const i = list.findIndex((l) => l.callback === callback && l.capture === capture);
    if (i >= 0) {
      list[i].removed = true;
      list.splice(i, 1);
    }
  }
  dispatchEvent(event) {
    if (!(event instanceof Event)) {
      throw new TypeError("dispatchEvent expects an Event");
    }
    event.target = this;
    event.currentTarget = this;
    const list = this._listeners.get(event.type);
    if (list) {
      for (const entry of [...list]) {
        if (event._immediateStopped) break;
        if (entry.removed) continue;
        if (entry.once) {
          this.removeEventListener(event.type, entry.callback, { capture: entry.capture });
        }
        try {
          if (typeof entry.callback === "function") {
            entry.callback.call(this, event);
          } else if (entry.callback && typeof entry.callback.handleEvent === "function") {
            entry.callback.handleEvent(event);
          }
        } catch (e) {
          // A listener throwing must not break dispatch (the spec "reports" the exception).
          console.error("Uncaught (in event listener)", e instanceof Error ? `${e.name}: ${e.message}` : String(e));
        }
      }
    }
    event.currentTarget = null;
    return !event.defaultPrevented;
  }
}

const kSignalCreate = Symbol("AbortSignal-internal-create");

class AbortSignal extends EventTarget {
  constructor(token) {
    if (token !== kSignalCreate) throw new TypeError("Illegal constructor");
    super();
    this.aborted = false;
    this.reason = undefined;
    this.onabort = null;
  }
  throwIfAborted() {
    if (this.aborted) throw this.reason;
  }
  _doAbort(reason) {
    if (this.aborted) return;
    this.aborted = true;
    this.reason =
      reason !== undefined
        ? reason
        : new DOMException("signal is aborted without reason", "AbortError");
    const event = new Event("abort");
    if (typeof this.onabort === "function") {
      try {
        this.onabort.call(this, event);
      } catch (e) {
        console.error("Uncaught (in onabort)", e instanceof Error ? `${e.name}: ${e.message}` : String(e));
      }
    }
    this.dispatchEvent(event);
  }
  static abort(reason) {
    const signal = new AbortSignal(kSignalCreate);
    signal._doAbort(reason);
    return signal;
  }
  static timeout(ms) {
    const controller = new AbortController();
    setTimeout(() => controller.abort(new DOMException("signal timed out", "TimeoutError")), ms);
    return controller.signal;
  }
}

class AbortController {
  constructor() {
    this.signal = new AbortSignal(kSignalCreate);
  }
  abort(reason) {
    this.signal._doAbort(reason);
  }
}

globalThis.DOMException = DOMException;
globalThis.Event = Event;
globalThis.CustomEvent = CustomEvent;
globalThis.EventTarget = EventTarget;
globalThis.AbortSignal = AbortSignal;
globalThis.AbortController = AbortController;
