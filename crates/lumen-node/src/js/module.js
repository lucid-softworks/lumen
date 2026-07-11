// CommonJS require() with node_modules resolution and the module wrapper.
//
// Resolution follows Node's algorithm (the practical core of it): core modules, then
// relative/absolute via LOAD_AS_FILE + LOAD_AS_DIRECTORY, then the node_modules walk.
// package.json "main" and a subset of "exports" are honored. The module body runs inside a
// `new Function(exports, require, module, __filename, __dirname)` wrapper, exactly as Node's
// does.

const path = __builtins.get("path");
const CORE = new Set([...__builtins.keys()]);
const cache = new Map(); // resolved filename -> module

const EXTENSIONS = [".js", ".json", ".cjs", ".node"];

function isCoreSpecifier(spec) {
  const bare = spec.startsWith("node:") ? spec.slice(5) : spec;
  return CORE.has(bare) ? bare : null;
}

function readIfFile(p) {
  return __node.isFile(p) ? p : null;
}

// LOAD_AS_FILE: exact, then with each known extension.
function loadAsFile(p) {
  if (readIfFile(p)) return p;
  for (const ext of EXTENSIONS) if (readIfFile(p + ext)) return p + ext;
  return null;
}

// A subset of package.json "exports": string, or the "." entry, resolving the
// require/node/default conditions. Subpath patterns and deep conditional trees are not handled
// yet (they fall through to "main").
function resolveExports(exports) {
  if (typeof exports === "string") return exports;
  if (exports && typeof exports === "object") {
    const dot = "." in exports ? exports["."] : exports;
    if (typeof dot === "string") return dot;
    if (dot && typeof dot === "object") {
      for (const cond of ["require", "node", "default"]) {
        if (typeof dot[cond] === "string") return dot[cond];
      }
    }
  }
  return null;
}

function loadAsDirectory(dir) {
  const pkgPath = path.join(dir, "package.json");
  if (__node.isFile(pkgPath)) {
    let pkg;
    try {
      pkg = JSON.parse(__node.readText(pkgPath));
    } catch (e) {
      throw new Error(`invalid package.json in ${dir}: ${e.message}`);
    }
    const fromExports = pkg.exports ? resolveExports(pkg.exports) : null;
    const entry = fromExports || pkg.main;
    if (entry) {
      const target = path.resolve(dir, entry);
      const found = loadAsFile(target) || loadAsFile(path.join(target, "index"));
      if (found) return found;
    }
  }
  return loadAsFile(path.join(dir, "index"));
}

// LOAD_NODE_MODULES: walk node_modules from `start` up to the filesystem root.
function loadNodeModules(name, start) {
  let dir = start;
  while (true) {
    if (path.basename(dir) !== "node_modules") {
      const candidate = path.join(dir, "node_modules", name);
      const found = loadAsFile(candidate) || loadAsDirectory(candidate);
      if (found) return found;
    }
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

function resolveFilename(specifier, fromDir) {
  const core = isCoreSpecifier(specifier);
  if (core) return "node:" + core;
  if (specifier.startsWith("./") || specifier.startsWith("../") || path.isAbsolute(specifier)) {
    const base = path.resolve(fromDir, specifier);
    const found = loadAsFile(base) || loadAsDirectory(base);
    if (found) return __node.realpath(found);
    const e = new Error(`Cannot find module '${specifier}' from '${fromDir}'`);
    e.code = "MODULE_NOT_FOUND";
    throw e;
  }
  const found = loadNodeModules(specifier, fromDir);
  if (found) return __node.realpath(found);
  const e = new Error(`Cannot find module '${specifier}'`);
  e.code = "MODULE_NOT_FOUND";
  throw e;
}

function loadModule(filename, parent) {
  if (filename.startsWith("node:")) {
    return { exports: __builtins.get(filename.slice(5)), filename, loaded: true };
  }
  const cached = cache.get(filename);
  if (cached) return cached;

  const module = {
    id: filename,
    filename,
    loaded: false,
    exports: {},
    parent,
    children: [],
    paths: [],
  };
  cache.set(filename, module);
  if (parent) parent.children.push(module);

  // A native addon: dlopen the `.node` file and run its N-API registration. The returned
  // exports become module.exports, exactly as Node does for compiled addons.
  if (filename.endsWith(".node")) {
    module.exports = __node.loadNativeAddon(filename);
    module.loaded = true;
    return module;
  }

  const source = __node.readText(filename);
  if (filename.endsWith(".json")) {
    module.exports = JSON.parse(source);
    module.loaded = true;
    return module;
  }

  const dirname = path.dirname(filename);
  const require = makeRequire(dirname, module);
  // The Node module wrapper. A leading #! shebang line is stripped, as Node does.
  const body = source.replace(/^#!.*/, "");
  const wrapper = new Function("exports", "require", "module", "__filename", "__dirname", body);
  wrapper.call(module.exports, module.exports, require, module, filename, dirname);
  module.loaded = true;
  return module;
}

function makeRequire(fromDir, parentModule) {
  const require = function (specifier) {
    const filename = resolveFilename(String(specifier), fromDir);
    return loadModule(filename, parentModule).exports;
  };
  require.resolve = (specifier) => resolveFilename(String(specifier), fromDir);
  require.cache = Object.create(null);
  require.main = mainModule;
  Object.defineProperty(require, "cache", {
    get() {
      const obj = Object.create(null);
      for (const [k, v] of cache) obj[k] = v;
      return obj;
    },
  });
  return require;
}

let mainModule = null;

// Run `filename` as the program entry (require.main === module), returning its exports.
function runMain(filename) {
  const resolved = __node.realpath(filename);
  const module = {
    id: ".",
    filename: resolved,
    loaded: false,
    exports: {},
    parent: null,
    children: [],
    paths: [],
  };
  mainModule = module;
  cache.set(resolved, module);
  const dirname = path.dirname(resolved);
  const require = makeRequire(dirname, module);
  const source = __node.readText(resolved).replace(/^#!.*/, "");
  const wrapper = new Function("exports", "require", "module", "__filename", "__dirname", source);
  wrapper.call(module.exports, module.exports, require, module, resolved, dirname);
  module.loaded = true;
  return module.exports;
}

// A cwd-bound require for -e / the REPL, plus createRequire(fromPath) like node:module's.
const cwdRequire = makeRequire(process.cwd(), null);
globalThis.require = cwdRequire;

function createRequire(fromPath) {
  // Node accepts a path or a file: URL (string or URL object), e.g. createRequire(import.meta.url).
  let p = typeof fromPath === "object" && fromPath ? fromPath.href || String(fromPath) : String(fromPath);
  if (p.startsWith("file://")) p = p.slice(7).replace(/^\/([A-Za-z]:)/, "$1");
  const dir = __node.isDir(p) ? p : path.dirname(p);
  return makeRequire(dir, null);
}

__builtins.set("module", { createRequire, builtinModules: [...CORE] });
__builtins.set("node:module", __builtins.get("module"));

// Exposed to the CLI (via a tiny bootstrap) to run a file as the main module.
globalThis.__runMain = runMain;

// --- ESM interop: synthetic re-export modules for the builtins ---
// The runtime's module loader (Rust) can't enumerate a builtin's keys, so we precompute one
// ESM source per builtin here — where Object.keys works — and the loader ferries the strings.
globalThis.__esmBuiltin = (name) => __builtins.get(name);

// `process` is populated (env/argv/…) by the runtime *after* this glue runs, so enumerating its
// keys here would miss them. Emit a fixed superset of its named exports instead; each reads the
// live `process` object at import time (missing ones are harmless `undefined`).
const PROCESS_EXPORTS = [
  "env", "argv", "argv0", "execArgv", "execPath", "platform", "arch", "pid", "ppid",
  "version", "versions", "cwd", "chdir", "exit", "exitCode", "nextTick", "hrtime",
  "stdout", "stderr", "stdin", "title", "on", "once", "off", "emit", "emitWarning",
  "memoryUsage", "uptime", "features", "release", "config", "kill", "umask",
  "allowedNodeEnvironmentFlags", "setSourceMapsEnabled",
];

function makeBuiltinEsmSource(name) {
  const m = __builtins.get(name);
  let src = `const __m = globalThis.__esmBuiltin(${JSON.stringify(name)});\nexport default __m;\n`;
  const keys = name === "process" ? PROCESS_EXPORTS : m && (typeof m === "object" || typeof m === "function") ? Object.keys(m) : [];
  for (const k of keys) {
    if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k) && k !== "default") {
      src += `export const ${k} = __m[${JSON.stringify(k)}];\n`;
    }
  }
  return src;
}

// The clean builtin base names (skip the "node:module" alias key). Order is cosmetic here.
const __BUILTIN_NAMES = [
  "buffer", "path", "os", "fs", "module",
  "events", "util", "crypto", "querystring", "url", "net", "assert",
  "string_decoder", "tty", "async_hooks", "zlib", "stream", "http", "https", "http2",
  "perf_hooks", "fs/promises", "child_process", "dns", "dns/promises",
  "v8", "inspector", "inspector/promises", "worker_threads", "readline",
  "readline/promises", "test", "tls", "process",
  "diagnostics_channel", "domain", "trace_events",
  "vm", "repl", "cluster", "dgram", "wasi",
];
const __esmBuiltinSources = {};
for (const name of __BUILTIN_NAMES) {
  const source = makeBuiltinEsmSource(name);
  __esmBuiltinSources["node:" + name] = source;
}
globalThis.__esmBuiltinSources = __esmBuiltinSources;
globalThis.__builtinNames = __BUILTIN_NAMES.join(",");
