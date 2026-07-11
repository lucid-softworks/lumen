// node:worker_threads — REAL workers: one OS thread + one fresh engine realm per Worker, over the
// runtime's __worker/__wself ops (lumen-runtime/src/worker.rs). Messages cross the thread boundary
// as structured-clone wire bytes (__serializeForClone, lumen-web/src/js/serialize.js).
//
// Real: new Worker(filename | URL | source-with-{eval:true}) with workerData/argv/env options,
// postMessage both ways, 'online'/'message'/'messageerror'/'error'/'exit', terminate() (resolves
// with the exit code), worker.ref()/unref(), threadId, parentPort (postMessage/'message'/ref/
// unref/close with Node's keep-alive-while-listening semantics), isMainThread, workerData,
// get/setEnvironmentData (snapshot inherited by new workers), receiveMessageOnPort, and the
// same-realm MessageChannel/MessagePort pair. process.exit(code) in a worker stops only that
// worker; an uncaught exception emits 'error' on the parent and exits the worker with code 1.
//
// Honest throws (semantics lumen cannot honor): SHARE_ENV (realms snapshot the env; there is no
// shared store), a non-empty transferList (messages are always copied — no SharedArrayBuffer or
// port transfer), the stdin/stdout/stderr capture options (workers share the process stdio),
// moveMessagePortToContext, postMessageToThread. BroadcastChannel is the same-realm web one.
{
  const EventEmitter = __builtins.get("events");
  const pathMod = __builtins.get("path");

  const SHARE_ENV = Symbol.for("nodejs.worker_threads.SHARE_ENV");
  const environmentData = new Map();

  const ser = (v) => globalThis.__serializeForClone(v);
  const deser = (b) => globalThis.__deserializeClone(b);

  // The runtime's worker extension installs after this glue and stashes its ops in a hidden
  // global (see WORKER_JS in lumen-runtime/src/worker.rs); grab them lazily on first use.
  let workerOps = null;
  function getWorkerOps() {
    if (workerOps === null) workerOps = globalThis.__lumenWorkerOps ?? null;
    if (workerOps === null) {
      throw new Error("worker_threads Worker requires the lumen runtime (worker ops not installed)");
    }
    return workerOps;
  }

  function checkTransferList(transfer) {
    const list = Array.isArray(transfer)
      ? transfer
      : transfer && typeof transfer === "object" && Array.isArray(transfer.transfer)
        ? transfer.transfer
        : [];
    if (list.length > 0) {
      throw new Error("worker_threads transferList is not supported in lumen (messages are always copied)");
    }
  }

  // Worker-side uncaught errors travel as "Name: message" text; rebuild a matching Error here.
  function reviveError(text) {
    const m = /^([A-Za-z][A-Za-z0-9_$]*(?:Error|Exception)): ([\s\S]*)$/.exec(String(text));
    const Ctor = m && typeof globalThis[m[1]] === "function" ? globalThis[m[1]] : Error;
    return m ? new Ctor(m[2]) : new Error(String(text));
  }

  class Worker extends EventEmitter {
    #id;
    #exited = false;
    #exitCode = null;
    #exitResolvers = [];
    constructor(filename, options = {}) {
      super();
      options = options && typeof options === "object" ? options : {};
      const ops = getWorkerOps();
      for (const opt of ["stdin", "stdout", "stderr"]) {
        if (options[opt]) {
          throw new Error(`worker_threads option '${opt}' is not supported in lumen (workers share the process stdio)`);
        }
      }
      if (options.env === SHARE_ENV) {
        throw new Error("worker_threads SHARE_ENV is not supported in lumen (realms snapshot the environment)");
      }
      let entry;
      let isModule = false;
      if (options.eval) {
        entry = String(filename);
      } else {
        let p = filename;
        if (typeof URL === "function" && p instanceof URL) p = p.href;
        p = String(p);
        if (p.startsWith("file://")) p = p.slice(7);
        if (!(p.startsWith("/") || p.startsWith("./") || p.startsWith("../"))) {
          throw new TypeError(
            "The worker script or module filename must be an absolute path or a relative path " +
              `starting with './' or '../'. Received ${JSON.stringify(p)}`,
          );
        }
        p = pathMod.resolve(p);
        isModule = p.endsWith(".mjs");
        entry = p;
      }
      // Snapshot the environment (Node copies the parent's process.env unless overridden).
      const envSrc = options.env == null ? process.env : options.env;
      if (typeof envSrc !== "object") throw new TypeError("options.env must be an object");
      const env = {};
      for (const k of Object.keys(envSrc)) {
        const v = envSrc[k];
        if (v !== undefined) env[k] = String(v);
      }
      const argv = Array.isArray(options.argv) ? options.argv.map(String) : [];
      // One structured-clone payload carries everything the worker realm needs at boot; a
      // DataCloneError from workerData surfaces to the caller, like Node.
      const init = ser({
        workerData: options.workerData,
        argv,
        env,
        envData: environmentData,
        entry: options.eval ? "[worker eval]" : entry,
      });
      const res = ops.spawn(entry, isModule, (kind, a) => this.#onEvent(kind, a), {
        node: true,
        eval: !!options.eval,
        init,
      });
      this.#id = res.id;
      this.threadId = res.threadId;
      this.resourceLimits = {};
      this.performance = { eventLoopUtilization: () => ({ idle: 0, active: 0, utilization: 0 }) };
      // stdio capture is unsupported (the options throw above); worker console output goes
      // straight to the shared process streams.
      this.stdin = null;
      this.stdout = null;
      this.stderr = null;
    }
    #onEvent(kind, a) {
      if (kind === "online") {
        this.emit("online");
      } else if (kind === "message") {
        let data;
        try {
          data = deser(a);
        } catch (e) {
          this.emit("messageerror", e);
          return;
        }
        this.emit("message", data);
      } else if (kind === "error") {
        this.emit("error", reviveError(a));
      } else if (kind === "exit") {
        this.#exited = true;
        this.#exitCode = a;
        for (const resolve of this.#exitResolvers.splice(0)) resolve(a);
        this.emit("exit", a);
      }
    }
    postMessage(value, transferList) {
      checkTransferList(transferList);
      if (this.#exited) return;
      getWorkerOps().post(this.#id, ser(value));
    }
    terminate(callback) {
      // Legacy callback form, still honored by Node alongside the promise.
      if (typeof callback === "function") this.once("exit", (code) => callback(null, code));
      if (this.#exited) return Promise.resolve(this.#exitCode);
      getWorkerOps().terminate(this.#id);
      return new Promise((resolve) => this.#exitResolvers.push(resolve));
    }
    ref() {
      getWorkerOps().setRef(this.#id, true);
      return this;
    }
    unref() {
      getWorkerOps().setRef(this.#id, false);
      return this;
    }
    getHeapSnapshot() {
      return Promise.reject(new Error("worker.getHeapSnapshot is not supported in lumen"));
    }
    getHeapStatistics() {
      return Promise.reject(new Error("worker.getHeapStatistics is not supported in lumen"));
    }
  }

  // Real for the same-realm MessageChannel ports (messages queue on the receiving port until it
  // starts): a queued message pops synchronously, exactly Node's contract.
  function receiveMessageOnPort(port) {
    if (!port || typeof port !== "object" || !Array.isArray(port._queue)) {
      throw new TypeError("receiveMessageOnPort expects a MessagePort");
    }
    return port._queue.length > 0 ? { message: port._queue.shift() } : undefined;
  }

  const notSupported = (name) =>
    function () {
      throw new Error(`worker_threads ${name} is not supported in lumen`);
    };

  const wt = {
    isMainThread: true,
    isInternalThread: false,
    threadId: 0,
    parentPort: null,
    workerData: undefined,
    SHARE_ENV,
    resourceLimits: {},
    Worker,
    // The same-realm entangled pair from the web glue; transferring a port across threads is what
    // the non-empty-transferList throw above refuses.
    MessageChannel: globalThis.MessageChannel,
    MessagePort: globalThis.MessagePort,
    BroadcastChannel: globalThis.BroadcastChannel,
    markAsUntransferable: () => {},
    isMarkedAsUntransferable: () => false,
    markAsUncloneable: () => {},
    moveMessagePortToContext: notSupported("moveMessagePortToContext"),
    postMessageToThread: notSupported("postMessageToThread"),
    receiveMessageOnPort,
    setEnvironmentData: (key, value) => {
      if (value === undefined) environmentData.delete(key);
      else environmentData.set(key, value);
    },
    getEnvironmentData: (key) => environmentData.get(key),
  };

  // The worker-realm bootstrap. NODE_WORKER_SCOPE_JS (lumen-runtime/src/worker.rs) calls this in
  // each node worker realm BEFORE the worker entry runs: it flips this module to worker-side
  // state (parentPort/workerData/threadId), patches process (argv/env/exit), wires uncaught-error
  // fatality, and returns the message-inbox dispatcher.
  Object.defineProperty(globalThis, "__lumenInitWorkerThread", {
    configurable: true,
    enumerable: false,
    writable: false,
    value: function __lumenInitWorkerThread(wself, threadId, initBytes) {
      let init = {};
      try {
        if (initBytes) init = deser(initBytes) || {};
      } catch {
        init = {};
      }

      wt.isMainThread = false;
      wt.threadId = typeof threadId === "number" ? threadId : -1;
      wt.workerData = init.workerData;
      if (init.envData instanceof Map) {
        for (const [k, v] of init.envData) environmentData.set(k, v);
      }
      if (init.env && typeof init.env === "object") process.env = init.env;
      process.argv = [
        process.execPath,
        init.entry ?? "[worker]",
        ...(Array.isArray(init.argv) ? init.argv : []),
      ];
      // Tell the cluster glue this realm is a worker *thread*, never a cluster worker's main
      // realm — it must not adopt the process's cluster IPC channel (see cluster.js).
      Object.defineProperty(globalThis, "__lumenWorkerThreadRealm", {
        configurable: true, enumerable: false, writable: false, value: true,
      });
      // process.exit in a worker stops this thread's loop, never the whole process. Cooperative:
      // the current synchronous JS runs to its end first (documented in worker.rs).
      const exitWorker = (code) => {
        const n = code == null ? (process.exitCode ?? 0) : Number(code);
        wself.exit(Number.isFinite(n) ? Math.trunc(n) : 0);
      };
      Object.defineProperty(process, "exit", {
        value: exitWorker, enumerable: true, configurable: true, writable: true,
      });
      process.reallyExit = exitWorker;

      class ParentPort extends EventEmitter {
        #closed = false;
        constructor() {
          super();
          // Node's port-ref semantics: the port keeps the worker alive only while a 'message'
          // listener is attached.
          this.on("newListener", (ev) => {
            if (ev === "message" && !this.#closed) wself.setRef(true);
          });
          this.on("removeListener", (ev) => {
            if (ev === "message" && this.listenerCount("message") === 0) wself.setRef(false);
          });
        }
        postMessage(value, transferList) {
          checkTransferList(transferList);
          if (this.#closed) return;
          wself.post(ser(value));
        }
        // Node's MessagePort is also an EventTarget; alias the listener API.
        addEventListener(type, fn) {
          this.on(type, fn);
        }
        removeEventListener(type, fn) {
          this.off(type, fn);
        }
        start() {}
        ref() {
          wself.setRef(true);
          return this;
        }
        unref() {
          wself.setRef(false);
          return this;
        }
        close() {
          this.#closed = true;
          wself.setRef(false);
        }
      }
      const parentPort = new ParentPort();
      wt.parentPort = parentPort;

      // Node kills a worker on an uncaught exception or unhandled rejection: the parent Worker
      // gets 'error', then 'exit' with code 1.
      const reportFatal = (err) => {
        let text;
        try {
          text =
            err instanceof Error
              ? `${err.name}: ${err.message}`
              : String(err).replace(/^Uncaught /, "");
        } catch {
          text = "Error: uncaught";
        }
        try {
          wself.report(text);
        } catch {}
        wself.exit(1);
      };
      globalThis.onerror = (message, _file, _line, _col, error) => {
        reportFatal(error !== undefined ? error : message);
        return true;
      };
      globalThis.onunhandledrejection = (event) => {
        event.preventDefault();
        reportFatal(event.reason);
      };

      return (bytes) => {
        if (bytes === false) return; // channel-closed sentinel (terminate)
        let data;
        try {
          data = deser(bytes);
        } catch (e) {
          parentPort.emit("messageerror", e);
          return;
        }
        parentPort.emit("message", data);
      };
    },
  });

  __builtins.set("worker_threads", wt);
}
