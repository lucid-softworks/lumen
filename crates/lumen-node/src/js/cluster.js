// node:cluster over child_process.fork. Workers are real child Lumen processes with JSON IPC.
// Socket/server handle transfer is intentionally unsupported: the IPC transport cannot pass OS
// descriptors, so applications that depend on shared listening handles get an explicit error.
{
  const EventEmitter = __builtins.get("events");
  const childProcess = __builtins.get("child_process");

  const SCHED_NONE = 1;
  const SCHED_RR = 2;
  const isWorkerProcess = () => process.env && process.env.LUMEN_CLUSTER_WORKER === "1";

  class Worker extends EventEmitter {
    constructor(id, child, childSide = false) {
      super();
      this.id = id;
      this.process = child;
      this.exitedAfterDisconnect = undefined;
      if (childSide) return;
      child.on("message", (message, handle) => {
        this.emit("message", message, handle);
        cluster.emit("message", this, message, handle);
      });
      child.on("disconnect", () => this.emit("disconnect"));
      child.on("error", error => this.emit("error", error));
      child.on("exit", (code, signal) => {
        delete cluster.workers[this.id];
        this.emit("exit", code, signal);
        cluster.emit("exit", this, code, signal);
      });
    }
    send(message, sendHandle, options, callback) {
      return this.process.send(message, sendHandle, options, callback);
    }
    kill(signal) {
      this.process.kill(signal);
      return this;
    }
    destroy(signal) { return this.kill(signal); }
    disconnect() {
      this.exitedAfterDisconnect = true;
      this.process.disconnect();
      return this;
    }
    isConnected() { return !!this.process.connected; }
    isDead() { return this.process.exitCode !== null || this.process.killed; }
  }

  const cluster = new EventEmitter();
  let nextWorkerId = 1;
  let currentWorker;

  Object.defineProperties(cluster, {
    isPrimary: { enumerable: true, get: () => !isWorkerProcess() },
    isMaster: { enumerable: true, get: () => !isWorkerProcess() },
    isWorker: { enumerable: true, get: isWorkerProcess },
    worker: {
      enumerable: false,
      get() {
        if (!isWorkerProcess()) return undefined;
        if (!currentWorker) {
          currentWorker = new Worker(Number(process.env.NODE_UNIQUE_ID) || 0, process, true);
          process.on("message", (message, handle) => currentWorker.emit("message", message, handle));
          process.on("disconnect", () => currentWorker.emit("disconnect"));
        }
        return currentWorker;
      },
    },
  });
  cluster.workers = {};
  cluster.settings = {};
  cluster.SCHED_NONE = SCHED_NONE;
  cluster.SCHED_RR = SCHED_RR;
  cluster.schedulingPolicy = SCHED_RR;
  cluster.Worker = Worker;

  cluster.setupPrimary = function setupPrimary(settings = {}) {
    cluster.settings = Object.assign(
      {
        args: process.argv.slice(2),
        exec: process.argv[1],
        execArgv: process.execArgv || [],
        silent: false,
      },
      settings,
    );
    cluster.emit("setup", cluster.settings);
  };
  cluster.setupMaster = cluster.setupPrimary;

  cluster.fork = function fork(env = {}) {
    if (isWorkerProcess()) throw new Error("cluster.fork may only be called from the primary process");
    if (!cluster.settings.exec) cluster.setupPrimary();
    const id = nextWorkerId++;
    const child = childProcess.fork(cluster.settings.exec, cluster.settings.args, {
      execArgv: cluster.settings.execArgv,
      silent: cluster.settings.silent,
      env: {
        ...process.env,
        ...env,
        NODE_UNIQUE_ID: String(id),
        LUMEN_CLUSTER_WORKER: "1",
      },
    });
    const worker = new Worker(id, child);
    cluster.workers[id] = worker;
    queueMicrotask(() => cluster.emit("fork", worker));
    child.on("spawn", () => {
      worker.emit("online");
      cluster.emit("online", worker);
    });
    return worker;
  };

  cluster.disconnect = function disconnect(callback) {
    const workers = Object.values(cluster.workers);
    if (workers.length === 0) {
      if (typeof callback === "function") queueMicrotask(callback);
      return;
    }
    let remaining = workers.length;
    const done = () => {
      if (--remaining === 0 && typeof callback === "function") callback();
    };
    for (const worker of workers) {
      worker.once("disconnect", done);
      worker.disconnect();
    }
  };

  __builtins.set("cluster", cluster);
}
