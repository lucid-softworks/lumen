// node:domain — the legacy error-routing shim. Domains are deprecated in Node but the module still
// ships; this is a minimal real implementation: run/bind/intercept execute callbacks with the
// domain active and funnel synchronous throws to the domain's 'error' handler. lumen has no
// async-context propagation, so a domain only spans the synchronous extent of run()/bind() calls.

const EventEmitter = __builtins.get("events");

// The domain stack: the top of stack is the currently-active domain.
const stack = [];
const domainModule = {};

class Domain extends EventEmitter {
  constructor() {
    super();
    this.members = [];
  }

  enter() {
    stack.push(this);
    domainModule.active = this;
    process.domain = this;
  }

  exit() {
    const idx = stack.lastIndexOf(this);
    if (idx !== -1) stack.splice(idx, 1);
    const top = stack[stack.length - 1];
    domainModule.active = top;
    process.domain = top ?? null;
  }

  run(fn, ...args) {
    this.enter();
    try {
      return Reflect.apply(fn, this, args);
    } catch (err) {
      this._handle(err);
    } finally {
      this.exit();
    }
  }

  // Wrap a callback so its synchronous throw is caught by the domain.
  bind(fn) {
    const self = this;
    return function bound(...args) {
      self.enter();
      try {
        return Reflect.apply(fn, this, args);
      } catch (err) {
        self._handle(err);
      } finally {
        self.exit();
      }
    };
  }

  // Wrap a Node-style callback: an error first-arg is routed to the domain, otherwise the callback
  // runs bound to the domain.
  intercept(fn) {
    const self = this;
    return function intercepted(err, ...args) {
      if (err) return self._handle(err);
      self.enter();
      try {
        return Reflect.apply(fn, this, args);
      } catch (e) {
        self._handle(e);
      } finally {
        self.exit();
      }
    };
  }

  add(emitter) {
    if (emitter.domain === this) return;
    if (emitter.domain) emitter.domain.remove(emitter);
    emitter.domain = this;
    this.members.push(emitter);
  }

  remove(emitter) {
    emitter.domain = null;
    const idx = this.members.indexOf(emitter);
    if (idx !== -1) this.members.splice(idx, 1);
  }

  _handle(err) {
    try {
      err.domain = this;
      err.domainThrown = true;
    } catch {
      /* err may be a primitive; ignore */
    }
    if (this.listenerCount("error") === 0) {
      // No handler: re-throw so the error is not silently swallowed.
      throw err;
    }
    this.emit("error", err);
  }
}

function create() {
  return new Domain();
}

domainModule.Domain = Domain;
domainModule.create = create;
domainModule.createDomain = create;
domainModule.active = null;
domainModule._stack = stack;

__builtins.set("domain", domainModule);
