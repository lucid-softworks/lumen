// Capture the raw op namespaces and remove them from the global scope; everything below
// closes over these consts. (This whole file set runs inside one IIFE — see lib.rs.)
"use strict";
const __encoding = globalThis.__encoding;
const __url = globalThis.__url;
const __http = globalThis.__http;
const __crypto = globalThis.__crypto;
const __perf = globalThis.__perf;
const __compress = globalThis.__compress;
const __wasm = globalThis.__wasm;
const __ws = globalThis.__ws;
delete globalThis.__encoding;
delete globalThis.__url;
delete globalThis.__http;
delete globalThis.__crypto;
delete globalThis.__perf;
delete globalThis.__compress;
delete globalThis.__wasm;
delete globalThis.__ws;
