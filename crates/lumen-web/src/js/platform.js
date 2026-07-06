// Platform-identity globals: `self` (the WindowOrWorkerGlobalScope alias) and the `Performance`
// interface over the native monotonic clock ops.

globalThis.self = globalThis;

class Performance {
  now() {
    return __perf.now();
  }
  get timeOrigin() {
    return __perf.timeOrigin();
  }
  toJSON() {
    return { timeOrigin: this.timeOrigin };
  }
}
globalThis.Performance = Performance;
globalThis.performance = new Performance();
