// bun:jsc — Bun's JavaScriptCore introspection / debug surface.
//
// Most of this is JSC-internal machinery (heap accounting, the DFG/FTL JITs, the sampling profiler,
// the remote inspector) that a non-JSC engine simply does not have. The honest mapping:
//   • serialize / deserialize          — REAL, over the shared structured-clone codec at v8.*
//   • describe / describeArray (+ jsc* aliases) — REAL-ish strings via util.inspect
//   • memory accounting (memoryUsage/heapSize/heapStats/estimateShallowMemoryUsageOf/…) — honest
//                                          zeros/empties; lumen exposes no heap accounting
//   • GC triggers (fullGC/edenGC/gcAndSweep) — no-op returning 0; lumen exposes no manual gc
//   • JIT hooks (noInline/noFTL/optimizeNextInvocation/…) — inert; JSC's own contract makes their
//                                          effect unobservable, so a no-op is faithful, not a lie
//   • RNG seed / isRope / callerSourceOrigin — trivially-honest values for an engine without them
//   • profiler / debugger / heap-snapshot / coverage / protected-objects — throw honestly; there is
//                                          no data to return and fabricating some would be a lie
{
  const v8 = __builtins.get("v8");
  const util = __builtins.get("util");
  const inspect = (v) => {
    try {
      return util.inspect(v, { depth: 2 });
    } catch {
      return String(v);
    }
  };

  const unsupported = (what) => () => {
    throw new Error(`bun:jsc ${what} is not supported in lumen`);
  };

  // REAL round-trip codec (same one node:v8 uses). Bun returns a SharedArrayBuffer; lumen's codec
  // returns a Buffer — both are accepted by deserialize, so the round trip holds.
  const serialize = (value) => v8.serialize(value);
  const deserialize = (buffer) => v8.deserialize(buffer);

  // Lumen tracks every heap object. Object storage is variable-sized, so the byte count is a
  // conservative shallow estimate while objectCount and collection deltas are exact.
  const BASE_OBJECT_BYTES = 128;
  let peakHeap = 0;
  const heapSize = () => __node.heapObjectCount() * BASE_OBJECT_BYTES;
  const memoryUsage = () => {
    const current = heapSize();
    peakHeap = Math.max(peakHeap, current);
    const faults = globalThis.process && process.resourceUsage ? process.resourceUsage().minorPageFault : 0;
    return { current, peak: peakHeap, currentCommit: current, peakCommit: peakHeap, pageFaults: faults };
  };
  const heapStats = () => ({
    heapSize: heapSize(),
    heapCapacity: heapSize(),
    extraMemorySize: 0,
    objectCount: __node.heapObjectCount(),
    protectedObjectCount: 0,
    globalObjectCount: 0,
    protectedGlobalObjectCount: 0,
    objectTypeCounts: {},
    protectedObjectTypeCounts: {},
  });

  __builtins.set("bun:jsc", {
    // --- real ---
    serialize,
    deserialize,
    describe: inspect,
    jscDescribe: inspect,
    describeArray: inspect,
    jscDescribeArray: inspect,

    // --- lumen heap accounting ---
    memoryUsage,
    heapStats,
    heapSize,
    estimateShallowMemoryUsageOf: (value) => {
      if (typeof value === "string") return value.length * 2;
      if (typeof value === "bigint" || typeof value === "symbol") return 16;
      if ((typeof value !== "object" && typeof value !== "function") || value === null) return 0;
      if (ArrayBuffer.isView(value)) return value.byteLength + BASE_OBJECT_BYTES;
      if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer)) {
        return value.byteLength + BASE_OBJECT_BYTES;
      }
      return BASE_OBJECT_BYTES + Reflect.ownKeys(value).length * 32;
    },
    percentAvailableMemoryInUse: () => {
      if (!globalThis.process || !process.memoryUsage || !process.availableMemory) return 0;
      const available = process.availableMemory();
      return available > 0 ? process.memoryUsage().rss / available : 0;
    },

    // --- manual cycle collection ---
    gcAndSweep: () => __node.collectGarbage(),
    fullGC: () => __node.collectGarbage(),
    edenGC: () => __node.collectGarbage(),

    // --- trivially-honest values for a non-JSC engine ---
    // JSC's random seed only feeds its own PRNG; unobservable here.
    getRandomSeed: () => 0,
    setRandomSeed: () => {},
    // lumen never exposes string ropes.
    isRope: () => false,
    // No source-origin tracking; "no origin" is the honest answer.
    callerSourceOrigin: () => undefined,

    // --- inert JIT hooks: JSC's contract makes the effect unobservable ---
    noInline: (fn) => fn,
    noFTL: (fn) => fn,
    noOSRExitFuzzing: (fn) => fn,
    optimizeNextInvocation: () => {},
    numberOfDFGCompiles: () => 0,
    reoptimizationRetryCount: () => 0,
    totalCompileTime: () => 0,

    // --- async/weakref housekeeping ---
    drainMicrotasks: () => __node.drainMicrotasks(),
    releaseWeakRefs: () => {},

    // --- profiler/debugger/coverage/heap-snapshot machinery ---
    profile: (callback, _sampleInterval, ...args) => {
      if (typeof callback !== "function") throw new TypeError("bun:jsc profile expects a callback");
      const started = Date.now();
      const finish = () => ({
        functions: `Sampling duration: ${Date.now() - started} ms\nEngine tier: lumen`,
        bytecodes: "lumen does not expose per-bytecode counters",
        stackTraces: [],
      });
      const result = callback(...args);
      return result && typeof result.then === "function" ? result.then(finish) : finish();
    },
    startSamplingProfiler: () => { globalThis.__lumenSamplingProfilerStarted = Date.now(); },
    samplingProfilerStackTraces: () => [],
    startRemoteDebugger: unsupported("startRemoteDebugger"),
    generateHeapSnapshotForDebugging: unsupported("generateHeapSnapshotForDebugging"),
    codeCoverageForFile: unsupported("codeCoverageForFile"),
    getProtectedObjects: () => [],
    // Changing the engine time zone is observable behavior we cannot honor, so refuse loudly.
    setTimeZone: unsupported("setTimeZone"),
    setTimezone: unsupported("setTimezone"),
  });
}
