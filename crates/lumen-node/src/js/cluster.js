// ---- node:cluster -----------------------------------------------------------------------------
// lumen is single-process: there is no primary/worker split and no way to fork a worker from JS.
// So the module reports the honest primary-with-no-workers state (`isPrimary`/`isMaster` true,
// empty `workers`) and, like Node's primary-side cluster, is itself an EventEmitter. `setupPrimary`
// really records settings and emits `setup`; `disconnect(cb)` really invokes the callback (there is
// nothing to disconnect); `fork` throws honestly because there is no worker to spawn.
{
  const EventEmitter = __builtins.get("events");

  const SCHED_NONE = 1;
  const SCHED_RR = 2;

  // Exported so callers can `instanceof cluster.Worker`; never actually instantiated here because
  // `fork` is unsupported.
  class Worker extends EventEmitter {
    constructor() {
      super();
      this.id = 0;
      this.process = undefined;
      this.exitedAfterDisconnect = undefined;
    }
    send() {
      throw new Error("cluster workers are not supported in lumen");
    }
    kill() {}
    destroy() {}
    disconnect() {
      return this;
    }
    isConnected() {
      return false;
    }
    isDead() {
      return true;
    }
  }

  const cluster = new EventEmitter();

  cluster.isMaster = true;
  cluster.isPrimary = true;
  cluster.isWorker = false;
  // Node keeps `worker` (the current worker, undefined in the primary) non-enumerable.
  Object.defineProperty(cluster, "worker", { value: undefined, enumerable: false, writable: true, configurable: true });
  cluster.workers = {};
  cluster.settings = {};
  cluster.SCHED_NONE = SCHED_NONE;
  cluster.SCHED_RR = SCHED_RR;
  cluster.schedulingPolicy = SCHED_RR;
  cluster.Worker = Worker;

  cluster.setupPrimary = function setupPrimary(settings = {}) {
    cluster.settings = Object.assign({ args: process.argv.slice(2), exec: process.argv[1], execArgv: process.execArgv || [], silent: false }, settings);
    cluster.emit("setup", cluster.settings);
  };
  // Deprecated alias, identical behavior.
  cluster.setupMaster = cluster.setupPrimary;

  cluster.fork = function fork() {
    throw new Error("cluster.fork is not supported in lumen");
  };

  cluster.disconnect = function disconnect(cb) {
    // No workers to disconnect; honor the callback contract immediately.
    if (typeof cb === "function") queueMicrotask(cb);
  };

  __builtins.set("cluster", cluster);
}
