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

  // No JSC heap to measure — report honest zeros rather than invent numbers.
  const memoryUsage = () => ({
    current: 0,
    peak: 0,
    currentCommit: 0,
    peakCommit: 0,
    pageFaults: 0,
  });
  const heapStats = () => ({
    heapSize: 0,
    heapCapacity: 0,
    extraMemorySize: 0,
    objectCount: 0,
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

    // --- honest zeros: no heap accounting in lumen ---
    memoryUsage,
    heapStats,
    heapSize: () => 0,
    estimateShallowMemoryUsageOf: () => 0,
    percentAvailableMemoryInUse: () => 0,

    // --- honest no-op returning 0: no manual GC exposed ---
    gcAndSweep: () => 0,
    fullGC: () => 0,
    edenGC: () => 0,

    // --- trivially-honest values for a non-JSC engine ---
    // JSC's random seed only feeds its own PRNG; unobservable here.
    getRandomSeed: () => 0,
    setRandomSeed: () => {},
    // lumen never exposes string ropes.
    isRope: () => false,
    // No source-origin tracking; "no origin" is the honest answer.
    callerSourceOrigin: () => undefined,

    // --- inert JIT hooks: JSC's contract makes the effect unobservable ---
    noInline: () => {},
    noFTL: () => {},
    noOSRExitFuzzing: () => {},
    optimizeNextInvocation: () => {},
    numberOfDFGCompiles: () => 0,
    reoptimizationRetryCount: () => 0,
    totalCompileTime: () => 0,

    // --- inert async/weakref housekeeping ---
    // lumen has no synchronous microtask-drain hook; scheduled work runs on the loop as usual.
    drainMicrotasks: () => {},
    releaseWeakRefs: () => {},

    // --- honest throws: no profiler/debugger/coverage/heap-snapshot machinery ---
    profile: unsupported("profile"),
    startSamplingProfiler: unsupported("startSamplingProfiler"),
    samplingProfilerStackTraces: unsupported("samplingProfilerStackTraces"),
    startRemoteDebugger: unsupported("startRemoteDebugger"),
    generateHeapSnapshotForDebugging: unsupported("generateHeapSnapshotForDebugging"),
    codeCoverageForFile: unsupported("codeCoverageForFile"),
    getProtectedObjects: unsupported("getProtectedObjects"),
    // Changing the engine time zone is observable behavior we cannot honor, so refuse loudly.
    setTimeZone: unsupported("setTimeZone"),
    setTimezone: unsupported("setTimezone"),
  });
}
