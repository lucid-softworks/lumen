// node:diagnostics_channel — a pure-JS named pub/sub registry. This is a full, real implementation:
// a Channel is a named broadcast point, subscribers are invoked synchronously on publish(), and
// tracingChannel() layers the start/end/asyncStart/asyncEnd/error sub-channel protocol on top.

// Every named channel is interned so that channel("x") always returns the same object and
// subscribe()/publish() on the same name reach each other.
const channels = new Map();

class Channel {
  constructor(name) {
    this.name = name;
    this._subscribers = [];
    this._stores = new Map(); // store -> transform, for the bindStore/runStores context API
  }

  get hasSubscribers() {
    return this._subscribers.length > 0;
  }

  subscribe(onMessage) {
    if (typeof onMessage !== "function") {
      throw new TypeError('The "onMessage" argument must be of type function');
    }
    this._subscribers.push(onMessage);
  }

  unsubscribe(onMessage) {
    const idx = this._subscribers.indexOf(onMessage);
    if (idx === -1) return false;
    this._subscribers.splice(idx, 1);
    return true;
  }

  publish(message) {
    // Copy so a handler that (un)subscribes mid-publish doesn't disturb this dispatch.
    for (const onMessage of this._subscribers.slice()) {
      try {
        onMessage(message, this.name);
      } catch (err) {
        // A throwing subscriber must not break the publisher; surface it out-of-band.
        queueMicrotask(() => {
          throw err;
        });
      }
    }
  }

  bindStore(store, transform) {
    this._stores.set(store, transform);
  }

  unbindStore(store) {
    return this._stores.delete(store);
  }

  runStores(context, fn, thisArg, ...args) {
    // Apply each bound store's context for the duration of fn. lumen's AsyncLocalStorage is a
    // simple enterWith, which is all this needs.
    let run = () => Reflect.apply(fn, thisArg, args);
    for (const [store, transform] of this._stores) {
      const value = transform ? transform(context) : context;
      const inner = run;
      run = () => store.run(value, inner);
    }
    return run();
  }
}

function channel(name) {
  let ch = channels.get(name);
  if (ch === undefined) {
    ch = new Channel(name);
    channels.set(name, ch);
  }
  return ch;
}

function hasSubscribers(name) {
  const ch = channels.get(name);
  return ch !== undefined && ch.hasSubscribers;
}

function subscribe(name, onMessage) {
  channel(name).subscribe(onMessage);
}

function unsubscribe(name, onMessage) {
  const ch = channels.get(name);
  if (ch === undefined) return false;
  return ch.unsubscribe(onMessage);
}

// tracingChannel(name) groups the five lifecycle sub-channels used to trace an operation. The
// trace* helpers publish to them in order and route the result/error to the right sub-channel.
const traceEvents = ["start", "end", "asyncStart", "asyncEnd", "error"];

class TracingChannel {
  constructor(nameOrChannels) {
    if (typeof nameOrChannels === "string") {
      for (const ev of traceEvents) this[ev] = channel(`tracing:${nameOrChannels}:${ev}`);
    } else {
      const c = nameOrChannels || {};
      for (const ev of traceEvents) {
        this[ev] = typeof c[ev] === "string" ? channel(c[ev]) : c[ev];
      }
    }
  }

  get hasSubscribers() {
    return traceEvents.some((ev) => this[ev] && this[ev].hasSubscribers);
  }

  subscribe(handlers) {
    for (const ev of traceEvents) {
      if (typeof handlers[ev] === "function") this[ev].subscribe(handlers[ev]);
    }
  }

  unsubscribe(handlers) {
    let ok = true;
    for (const ev of traceEvents) {
      if (typeof handlers[ev] === "function") ok = this[ev].unsubscribe(handlers[ev]) && ok;
    }
    return ok;
  }

  traceSync(fn, context = {}, thisArg, ...args) {
    this.start.publish(context);
    try {
      const result = Reflect.apply(fn, thisArg, args);
      context.result = result;
      return result;
    } catch (err) {
      context.error = err;
      this.error.publish(context);
      throw err;
    } finally {
      this.end.publish(context);
    }
  }

  tracePromise(fn, context = {}, thisArg, ...args) {
    this.start.publish(context);
    let promise;
    try {
      promise = Reflect.apply(fn, thisArg, args);
    } catch (err) {
      context.error = err;
      this.error.publish(context);
      this.end.publish(context);
      throw err;
    }
    this.end.publish(context);
    this.asyncStart.publish(context);
    return Promise.resolve(promise).then(
      (result) => {
        context.result = result;
        this.asyncEnd.publish(context);
        return result;
      },
      (err) => {
        context.error = err;
        this.error.publish(context);
        this.asyncEnd.publish(context);
        throw err;
      },
    );
  }

  traceCallback(fn, position, context = {}, thisArg, ...args) {
    this.start.publish(context);
    const self = this;
    const callback = args[position];
    if (typeof callback === "function") {
      args[position] = function wrapped(err, ...rest) {
        if (err) {
          context.error = err;
          self.error.publish(context);
        } else {
          context.result = rest[0];
        }
        self.asyncStart.publish(context);
        try {
          return Reflect.apply(callback, this, arguments);
        } finally {
          self.asyncEnd.publish(context);
        }
      };
    }
    try {
      return Reflect.apply(fn, thisArg, args);
    } catch (err) {
      context.error = err;
      this.error.publish(context);
      throw err;
    } finally {
      this.end.publish(context);
    }
  }
}

function tracingChannel(nameOrChannels) {
  return new TracingChannel(nameOrChannels);
}

__builtins.set("diagnostics_channel", {
  channel,
  hasSubscribers,
  subscribe,
  unsubscribe,
  tracingChannel,
  Channel,
});
