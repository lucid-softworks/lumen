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

const EXTENSIONS = [".js", ".json", ".cjs", ".ts", ".cts", ".node"];

function isCoreSpecifier(spec) {
  if (spec === "bun" || spec.startsWith("bun:")) return CORE.has(spec) ? spec : null;
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

function defaultResolveFilename(specifier, fromDir) {
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

// --- registerHooks (Node's sync module customization hooks) -------------------------------------
// module.registerHooks({ resolve, load }) chains user hooks onto require()'s resolve and load
// steps, LIFO like Node's (the most recently registered hook runs first; nextResolve/nextLoad
// walks toward the default). Hooks speak URLs — file:// for files, node: for builtins — so the
// default steps translate to/from the path-based machinery above. Scope, honestly: this covers
// the CommonJS require() path (resolve + load, including require.resolve and Module._load).
// ESM `import` of files is resolved/loaded by the runtime's native loader and does NOT consult
// these hooks (wiring that up needs loader changes in Rust, not glue).
const __registeredHooks = []; // registration order; chains are built newest-outermost

// Minimal path <-> file URL translation (symmetric; percent-encodes only what breaks a URL).
function pathToFileUrl(p) {
  let s = String(p).replace(/\\/g, "/");
  if (!s.startsWith("/")) s = "/" + s; // win32 drive form C:/x -> /C:/x
  return "file://" + s.replace(/%/g, "%25").replace(/#/g, "%23").replace(/\?/g, "%3F").replace(/ /g, "%20");
}
function fileUrlToPath(u) {
  let p = String(u).slice("file://".length);
  const q = p.indexOf("?");
  if (q >= 0) p = p.slice(0, q); // a search suffix is legal in a loader URL; the file is the path
  p = p.replace(/^\/([A-Za-z]:)/, "$1");
  return decodeURIComponent(p);
}
function hookConditions() {
  return ["require", "node", "default"];
}

// Compose the registered hooks of one kind around `defaultStep`, newest hook outermost. Each
// hook must either call next() or return { shortCircuit: true }, exactly as Node enforces.
function chainHooks(kind, defaultStep) {
  let next = defaultStep;
  for (const reg of __registeredHooks) { // oldest first, so the newest ends up outermost
    const fn = reg[kind];
    if (!fn) continue;
    const inner = next;
    next = (arg0, context) => {
      let calledNext = false;
      const nextHook = (a, c) => {
        calledNext = true;
        return inner(a === undefined ? arg0 : a, c === undefined ? context : c);
      };
      const result = fn(arg0, context, nextHook);
      if (!result || typeof result !== "object") {
        const e = new TypeError(`The "${kind}" hook must return an object`);
        e.code = "ERR_INVALID_RETURN_VALUE";
        throw e;
      }
      if (!calledNext && !result.shortCircuit) {
        const e = new TypeError(
          `Expected true to be returned for the "shortCircuit" from the "${kind}" hook but got ${result.shortCircuit}.`,
        );
        e.code = "ERR_INVALID_RETURN_PROPERTY_VALUE";
        throw e;
      }
      return result;
    };
  }
  return next;
}

function hasHook(kind) {
  for (const reg of __registeredHooks) if (reg[kind]) return true;
  return false;
}

// resolveFilename: the single resolve funnel (require, require.resolve, Module._load /
// _resolveFilename all land here). With hooks registered, run the chain over URLs and map the
// winning url back into the internal filename form ("node:x" or an absolute path).
function resolveFilename(specifier, fromDir, parentFilename) {
  if (!hasHook("resolve")) return defaultResolveFilename(specifier, fromDir);
  const defaultStep = (spec, _context) => {
    const filename = defaultResolveFilename(String(spec), fromDir);
    return {
      url: filename.startsWith("node:") ? filename : pathToFileUrl(filename),
      shortCircuit: true,
    };
  };
  const run = chainHooks("resolve", defaultStep);
  const result = run(String(specifier), {
    conditions: hookConditions(),
    importAttributes: {},
    parentURL: parentFilename ? pathToFileUrl(parentFilename) : pathToFileUrl(fromDir + "/"),
  });
  const url = String(result.url);
  if (url.startsWith("node:") || isBuiltin(url)) return url.startsWith("node:") ? url : "node:" + url;
  if (url.startsWith("file://")) return __node.realpath(fileUrlToPath(url));
  throw new Error(
    `registerHooks: resolve returned unsupported URL scheme '${url}' (file:// and node: are supported in lumen)`,
  );
}

// Compile CommonJS source into `module` via the wrapper — the shared back half of the `.js`
// extension handler, split out so a registerHooks load hook can feed transformed source in.
function compileCommonJS(module, filename, source) {
  const dirname = path.dirname(filename);
  // A leading #! shebang line is stripped, as Node does, before wrapping.
  source = String(source).replace(/^#!.*/, "");
  const require = makeRequire(dirname, module);
  const compiled = new Function("exports", "require", "module", "__filename", "__dirname", source);
  compiled.call(module.exports, module.exports, require, module, filename, dirname);
}

// A load-hook `source` may be a string or a TypedArray/ArrayBuffer (Node accepts both).
function hookSourceToString(source) {
  if (typeof source === "string") return source;
  if (source instanceof ArrayBuffer) return Buffer.from(new Uint8Array(source)).toString("utf8");
  if (ArrayBuffer.isView(source)) {
    return Buffer.from(new Uint8Array(source.buffer, source.byteOffset, source.byteLength)).toString("utf8");
  }
  return String(source);
}

// With load hooks registered, run the chain for `filename` and materialize the result. Returns
// true when the hooks produced the module; false to fall through to the extension dispatch
// (builtin/addon results, i.e. source === null on a non-transformable format).
function loadViaHooks(module, filename) {
  const defaultStep = (u, _context) => {
    const url = String(u);
    if (url.startsWith("node:")) return { format: "builtin", source: null, shortCircuit: true };
    const p = url.startsWith("file://") ? fileUrlToPath(url) : url;
    const ext = path.extname(p);
    if (ext === ".json") return { format: "json", source: __node.readText(p), shortCircuit: true };
    if (ext === ".node") return { format: "addon", source: null, shortCircuit: true };
    // Node v22's default nextLoad reports `format: undefined` for a required .js file (the CJS
    // loader decides by extension after the chain); mirror that so format-switching hooks match.
    return { format: undefined, source: __node.readText(p), shortCircuit: true };
  };
  const run = chainHooks("load", defaultStep);
  const result = run(pathToFileUrl(filename), {
    format: undefined,
    conditions: hookConditions(),
    importAttributes: {},
  });
  if (result.source == null) return false; // addon or builtin: extension dispatch handles it
  const source = hookSourceToString(result.source);
  if (result.format === "json") {
    module.exports = JSON.parse(source);
    return true;
  }
  if (result.format === undefined || result.format === "commonjs") {
    compileCommonJS(module, filename, source);
    return true;
  }
  throw new Error(
    `registerHooks: load format '${result.format}' is not supported in lumen (commonjs and json are)`,
  );
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

  // registerHooks load chain first (it can rewrite the source); otherwise — and for results the
  // hooks leave alone (addons) — dispatch on file extension through Module._extensions, exactly
  // as Node does: `.js`/`.cjs` run the module wrapper, `.json` is parsed, `.node` is dlopen'd for
  // its N-API registration. An unknown extension falls back to the `.js` loader, like Node.
  if (!(hasHook("load") && loadViaHooks(module, filename))) {
    const ext = path.extname(filename);
    const handler = Module._extensions[ext] || Module._extensions[".js"];
    handler(module, filename);
  }
  module.loaded = true;
  return module;
}

function makeRequire(fromDir, parentModule) {
  const parentFilename = parentModule && typeof parentModule.filename === "string" ? parentModule.filename : undefined;
  const require = function (specifier) {
    const filename = resolveFilename(String(specifier), fromDir, parentFilename);
    return loadModule(filename, parentModule).exports;
  };
  // Node v22.16 quirk, mirrored deliberately: require.resolve() does NOT consult registerHooks
  // (only actual loads do) — verified against `node -e`.
  require.resolve = (specifier) => defaultResolveFilename(String(specifier), fromDir);
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
  const ext = path.extname(resolved);
  const handler = Module._extensions[ext] || Module._extensions[".js"];
  handler(module, resolved);
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

// --- node:module surface -----------------------------------------------------------------
// `module`/`node:module` were not in `__builtins` when CORE was snapshotted at the top of this
// file (module.js is last and registers them right here), so add the bare name to the core set
// now — otherwise require('module') and require('node:module') would throw MODULE_NOT_FOUND, and
// isBuiltin('module') would be wrong. Only the bare name is a core specifier; "node:module" stays
// a __builtins alias key, not a member of CORE.
CORE.add("module");

// The frozen list of core module names, mirroring node:module's `builtinModules` (bare names,
// no "node:" prefix), which now includes "module" itself.
const builtinModules = Object.freeze([...CORE]);

// isBuiltin(spec): true when `spec` — with an optional "node:" prefix — names a core module.
function isBuiltin(spec) {
  if (typeof spec !== "string") return false;
  return CORE.has(spec.startsWith("node:") ? spec.slice(5) : spec);
}

// Normalize a path or a file: URL (string or URL object) to a filesystem path — the same shape
// createRequire accepts above.
function toFsPath(input) {
  let p = typeof input === "object" && input ? input.href || String(input) : String(input);
  if (p.startsWith("file://")) p = p.slice(7).replace(/^\/([A-Za-z]:)/, "$1");
  return p;
}

// The pieces of Node's constants surface we can represent. Only the compile-cache status enum is
// public today; lumen never enables the cache, so DISABLED is the only value it reports back.
const constants = {
  compileCacheStatus: { FAILED: 0, ENABLED: 1, ALREADY_ENABLED: 2, DISABLED: 3 },
};

// The module wrapper strings Node exposes so tools can reconstruct the `(function (exports, ...))`
// preamble; kept identical to Node's so byte-offset math in coverage/source tooling lines up.
const wrapper = ["(function (exports, require, module, __filename, __dirname) { ", "\n});"];
function wrap(source) {
  return wrapper[0] + source + wrapper[1];
}

// Node's per-extension loaders. loadModule() above dispatches through these, so replacing or
// wrapping an entry (as ts-node / pirates do) actually takes effect — real behavior, not a stub.
const _extensions = {
  ".js": function (module, filename) {
    compileCommonJS(module, filename, __node.readText(filename));
  },
  ".ts": function (module, filename) {
    compileCommonJS(module, filename, stripTypeScriptTypes(__node.readText(filename), { sourceUrl: filename }));
  },
  ".cts": function (module, filename) {
    compileCommonJS(module, filename, stripTypeScriptTypes(__node.readText(filename), { sourceUrl: filename }));
  },
  ".json": function (module, filename) {
    module.exports = JSON.parse(__node.readText(filename));
  },
  ".node": function (module, filename) {
    module.exports = __node.loadNativeAddon(filename);
  },
};

// Module.prototype instances, shaped like Node's. The internal require() machinery above uses
// plain objects, but the class is here for code that constructs modules or subclasses Module.
function Module(id = "", parent) {
  this.id = id;
  this.path = id ? path.dirname(id) : ".";
  this.exports = {};
  this.filename = null;
  this.loaded = false;
  this.children = [];
  this.parent = parent;
  this.paths = [];
}
// A require() bound to this module instance's directory, like Node's Module.prototype.require.
Module.prototype.require = function (id) {
  return makeRequire(this.path || process.cwd(), this)(String(id));
};

// node_modules lookup paths for `from`, walking to the filesystem root (Node's _nodeModulePaths).
function nodeModulePaths(from) {
  const paths = [];
  let dir = path.resolve(String(from));
  while (true) {
    if (path.basename(dir) !== "node_modules") paths.push(path.join(dir, "node_modules"));
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return paths;
}

// The dir a module-ish `parent` resolves specifiers from (its `path`, else its `filename`'s dir).
function parentDir(parent) {
  if (parent && typeof parent.path === "string") return parent.path;
  if (parent && typeof parent.filename === "string") return path.dirname(parent.filename);
  return process.cwd();
}

// Module._resolveFilename(request, parent): like require.resolve, but core modules come back in
// the specifier's own form ("fs" / "node:fs"), matching Node rather than our internal "node:" tag.
function _resolveFilename(request, parent) {
  const spec = String(request);
  if (isBuiltin(spec)) return spec;
  // Like require.resolve, Node v22.16's _resolveFilename does not consult registerHooks.
  return defaultResolveFilename(spec, parentDir(parent));
}

// Module._load(request, parent, isMain): resolve then load, returning the module's exports.
function _load(request, parent, isMain) {
  const filename = resolveFilename(String(request), parentDir(parent));
  const mod = loadModule(filename, parent);
  if (isMain) mainModule = mod;
  return mod.exports;
}

// Module._findPath(request, paths): first existing file/dir for `request` under any of `paths`,
// else false — the primitive resolveFilename builds on, exposed for parity.
function _findPath(request, paths) {
  for (const p of paths || []) {
    const base = path.resolve(String(p), String(request));
    const found = loadAsFile(base) || loadAsDirectory(base);
    if (found) return __node.realpath(found);
  }
  return false;
}

// Module._resolveLookupPaths(request, parent): the search paths for a bare specifier (null for
// relative/absolute/core requests, as Node returns).
function _resolveLookupPaths(request, parent) {
  const spec = String(request);
  if (isBuiltin(spec)) return null;
  if (spec.startsWith("./") || spec.startsWith("../") || spec.startsWith("/")) return null;
  return nodeModulePaths(parentDir(parent));
}

// Module.runMain(): run the process entry point (process.argv[1]) as the main module.
function moduleRunMain(main) {
  const entry = main != null ? String(main) : process.argv && process.argv[1];
  if (!entry) throw new Error("Module.runMain: no entry point (process.argv[1] is empty)");
  return runMain(entry);
}

// findPackageJSON(specifier, base): the nearest package.json for a resolved specifier, walking up
// from its directory. Experimental in Node; a real filesystem walk here, undefined if none found.
function findPackageJSON(specifier, base) {
  let dir;
  try {
    const fromInput = base != null ? toFsPath(base) : process.cwd();
    const fromDir = __node.isDir(fromInput) ? fromInput : path.dirname(fromInput);
    const resolved = resolveFilename(String(specifier), fromDir);
    if (resolved.startsWith("node:")) return undefined;
    dir = path.dirname(resolved);
  } catch (e) {
    return undefined;
  }
  while (true) {
    const pkg = path.join(dir, "package.json");
    if (__node.isFile(pkg)) return pkg;
    const parent = path.dirname(dir);
    if (parent === dir) return undefined;
    dir = parent;
  }
}

// syncBuiltinESMExports(): a no-op. Node uses it to push CJS monkeypatches of a builtin onto that
// builtin's ESM named exports. lumen's ESM builtins re-read the live module object at import time
// (see __esmBuiltin below), so there is nothing to re-sync; returns undefined like Node.
function syncBuiltinESMExports() {}

// Source-maps: lumen carries no per-frame source information (see the placeholder CallSites in
// preamble.js), so there are no maps to hand back. The support flags are honest state — settable
// and observable — but toggling them cannot make maps materialize.
let __sourceMapsSupport = { enabled: false, nodeModules: false, generatedCode: false };
function getSourceMapsSupport() {
  return { ...__sourceMapsSupport };
}
function setSourceMapsSupport(enabled, options) {
  __sourceMapsSupport = {
    enabled: !!enabled,
    nodeModules: !!(options && options.nodeModules),
    generatedCode: !!(options && options.generatedCode),
  };
}
// A minimal SourceMap: it stores the payload it is given (Node exposes `payload`/`lineLengths`),
// but lumen decodes no mappings, so findEntry/findOrigin resolve to empty results.
function SourceMap(payload, opts) {
  const lineLengths = (opts && opts.lineLengths) || [];
  Object.defineProperty(this, "payload", { enumerable: true, get: () => payload });
  Object.defineProperty(this, "lineLengths", { enumerable: true, get: () => lineLengths });
}
SourceMap.prototype.findEntry = function () {
  return {};
};
SourceMap.prototype.findOrigin = function () {
  return {};
};
// findSourceMap(path): undefined — lumen registers no maps, which is a valid Node result.
function findSourceMap() {
  return undefined;
}

// The compile cache is a V8 code-cache-on-disk optimization lumen does not implement. Report it as
// permanently DISABLED (honest, non-throwing) rather than pretending to enable it.
function enableCompileCache() {
  return { status: constants.compileCacheStatus.DISABLED, message: "compile cache is not supported in lumen" };
}
function getCompileCacheDir() {
  return undefined;
}
function flushCompileCache() {}

// registerHooks({ resolve, load }) — Node's SYNC module customization hooks, wired into the
// resolve/load funnels above (see the "__registeredHooks" section). Returns { deregister }.
// Covers CommonJS require(); ESM file imports go through the native loader and are not hooked.
function registerHooks(hooks) {
  if (hooks == null || typeof hooks !== "object") {
    throw new TypeError(`Cannot destructure property 'resolve' of 'hooks' as it is ${hooks === null ? "null" : typeof hooks}.`);
  }
  const entry = { resolve: hooks.resolve, load: hooks.load };
  for (const name of ["resolve", "load"]) {
    if (entry[name] !== undefined && typeof entry[name] !== "function") {
      const e = new TypeError(
        `The "hooks.${name}" property must be of type function. Received type ${typeof entry[name]}`,
      );
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
  }
  __registeredHooks.push(entry);
  return {
    deregister() {
      const i = __registeredHooks.indexOf(entry);
      if (i >= 0) __registeredHooks.splice(i, 1);
    },
  };
}

// The async loader-thread hook machinery remains unavailable. For sync hooks, use registerHooks.
function register() {
  throw new Error("node:module register() (async ESM loader hooks) is not supported in lumen; module.registerHooks (the sync API) is");
}
const stripTypeScriptTypes = globalThis.__lumenStripTypeScriptTypes;

// Node's `require('module')` is the Module constructor itself, with every named export hung off it
// as a static (so require('module') === require('module').Module). Mirror that exactly.
Module.Module = Module;
Module.SourceMap = SourceMap;
Module.builtinModules = builtinModules;
Module.constants = constants;
Module.createRequire = createRequire;
Module.isBuiltin = isBuiltin;
Module.syncBuiltinESMExports = syncBuiltinESMExports;
Module.findSourceMap = findSourceMap;
Module.getSourceMapsSupport = getSourceMapsSupport;
Module.setSourceMapsSupport = setSourceMapsSupport;
Module.findPackageJSON = findPackageJSON;
Module.register = register;
Module.registerHooks = registerHooks;
Module.stripTypeScriptTypes = stripTypeScriptTypes;
Module.enableCompileCache = enableCompileCache;
Module.getCompileCacheDir = getCompileCacheDir;
Module.flushCompileCache = flushCompileCache;
Module.runMain = moduleRunMain;
// wrap/wrapper are non-enumerable in Node (they stay off Object.keys(module)), so define them so.
Object.defineProperty(Module, "wrap", { value: wrap, writable: true, configurable: true });
Object.defineProperty(Module, "wrapper", { value: wrapper, writable: true, configurable: true });
Module._extensions = _extensions;
Module._pathCache = Object.create(null);
Module._debug = function () {};
Module._findPath = _findPath;
Module._nodeModulePaths = nodeModulePaths;
Module._resolveFilename = _resolveFilename;
Module._resolveLookupPaths = _resolveLookupPaths;
Module._load = _load;
Module._initPaths = function () {};
Module._preloadModules = function () {};
// _cache mirrors the live require cache; globalPaths is computed on access because process.env is
// populated by the runtime *after* this glue runs (so reading HOME eagerly here would miss it).
Object.defineProperty(Module, "_cache", {
  enumerable: true,
  get() {
    const obj = Object.create(null);
    for (const [k, v] of cache) obj[k] = v;
    return obj;
  },
});
Object.defineProperty(Module, "globalPaths", {
  enumerable: true,
  get() {
    const home = process.env.HOME || process.env.USERPROFILE || "";
    const list = [];
    if (process.env.NODE_PATH) list.push(...process.env.NODE_PATH.split(path.delimiter).filter(Boolean));
    if (home) list.push(path.join(home, ".node_modules"), path.join(home, ".node_libraries"));
    return list;
  },
});

__builtins.set("module", Module);
__builtins.set("node:module", Module);

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
  "events", "util", "sys", "console", "timers", "timers/promises",
  "crypto", "querystring", "url", "net", "assert", "assert/strict",
  "string_decoder", "tty", "async_hooks", "zlib", "stream", "stream/web",
  "stream/promises", "stream/consumers", "http", "https", "http2",
  "perf_hooks", "fs/promises", "path/posix", "path/win32", "child_process",
  "dns", "dns/promises",
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
