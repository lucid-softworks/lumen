// Capture the raw op namespaces; everything below closes over these. Runs in one IIFE.
"use strict";
const __node = globalThis.__node;
const __os = globalThis.__os;
const __zlib = globalThis.__zlib;
const __child = globalThis.__child;
delete globalThis.__node;
delete globalThis.__os;
delete globalThis.__zlib;
delete globalThis.__child;

// Node's `global` is an alias for the global object.
if (typeof globalThis.global === "undefined") {
  globalThis.global = globalThis;
}

// V8's `Error.captureStackTrace` / `Error.prepareStackTrace` / CallSite API. The engine does not
// implement these (grep confirms: no such symbol), and this preamble runs once — so we define them
// outright rather than guarding on `typeof`. They are pervasive in the Node ecosystem: http-errors
// and depd build Error subclasses with captureStackTrace, and depd reads *structured* frames
// (a `prepareStackTrace` hook receiving CallSite objects it calls `.getFileName()`/`.getLineNumber()`
// on). lumen has no per-frame source info, so we hand back placeholder CallSites — enough that the
// deprecation machinery runs (with a `<lumen>` location) instead of crashing. `.stack` stays a
// normal string when no custom `prepareStackTrace` hook is installed.
Error.stackTraceLimit = 10;
const makeCallSite = () => ({
  getThis: () => undefined,
  getTypeName: () => null,
  getFunction: () => undefined,
  getFunctionName: () => null,
  getMethodName: () => null,
  getFileName: () => "<lumen>",
  getLineNumber: () => 0,
  getColumnNumber: () => 0,
  getEvalOrigin: () => undefined,
  isToplevel: () => true,
  isEval: () => false,
  isNative: () => false,
  isConstructor: () => false,
  isAsync: () => false,
  toString: () => "<lumen>:0:0",
});
Error.captureStackTrace = function (target, _ctorOpt) {
  const sites = Array.from({ length: Error.stackTraceLimit || 10 }, makeCallSite);
  Object.defineProperty(target, "stack", {
    configurable: true,
    get() {
      const prepare = Error.prepareStackTrace;
      if (typeof prepare === "function") return prepare(target, sites);
      return `${target.name || "Error"}: ${target.message || ""}\n    at <lumen>:0:0`;
    },
    set(value) {
      Object.defineProperty(target, "stack", { value, writable: true, configurable: true });
    },
  });
  return target;
};

// A registry the module system fills in; each builtin registers itself as it is defined.
const __builtins = new Map();
