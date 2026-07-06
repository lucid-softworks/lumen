// Capture the raw op namespaces; everything below closes over these. Runs in one IIFE.
"use strict";
const __node = globalThis.__node;
const __os = globalThis.__os;
delete globalThis.__node;
delete globalThis.__os;

// Node's `global` is an alias for the global object.
if (typeof globalThis.global === "undefined") {
  globalThis.global = globalThis;
}

// A registry the module system fills in; each builtin registers itself as it is defined.
const __builtins = new Map();
