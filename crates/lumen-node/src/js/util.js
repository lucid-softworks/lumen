// node:util — the slice real-world code uses: inherits, format/formatWithOptions, a practical
// inspect, deprecate, promisify/callbackify, types.*, and the legacy is* predicates. inspect is
// not byte-identical to Node's (that formatter is enormous) but covers the shapes debug output
// and error messages need.

function inherits(ctor, superCtor) {
  if (ctor === undefined || ctor === null) throw new TypeError('The "ctor" argument must be a function');
  if (superCtor === undefined || superCtor === null) throw new TypeError('The "superCtor" argument must be a function');
  if (superCtor.prototype === undefined) throw new TypeError('The "superCtor.prototype" property must not be undefined');
  Object.defineProperty(ctor, "super_", { value: superCtor, writable: true, configurable: true });
  Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

// ---- inspect ----------------------------------------------------------------------------------

function inspect(value, opts) {
  const options = normalizeInspectOptions(opts);
  return formatValue(value, options, new Set(), 0);
}
inspect.custom = Symbol.for("nodejs.util.inspect.custom");
inspect.defaultOptions = { depth: 2 };

function normalizeInspectOptions(opts) {
  if (typeof opts === "boolean") return { depth: 2, showHidden: opts, colors: false };
  return { depth: 2, showHidden: false, colors: false, ...(opts || {}) };
}

function formatValue(value, options, seen, depth) {
  switch (typeof value) {
    case "string":
      return depth === 0 ? value : `'${value.replace(/'/g, "\\'")}'`;
    case "number":
    case "boolean":
    case "bigint":
      return String(value) + (typeof value === "bigint" ? "n" : "");
    case "undefined":
      return "undefined";
    case "symbol":
      return value.toString();
    case "function": {
      const name = value.name ? `: ${value.name}` : " (anonymous)";
      return `[Function${name}]`;
    }
  }
  if (value === null) return "null";

  // Custom inspect hook (debug, many libs).
  if (typeof value[inspect.custom] === "function") {
    return String(value[inspect.custom](depth, options));
  }
  if (value instanceof Error) return value.stack || `${value.name}: ${value.message}`;
  if (value instanceof RegExp) return value.toString();
  if (value instanceof Date) return Number.isNaN(value.getTime()) ? "Invalid Date" : value.toISOString();
  if (typeof Buffer !== "undefined" && Buffer.isBuffer && Buffer.isBuffer(value)) {
    const hex = [...value.subarray(0, 50)].map((b) => b.toString(16).padStart(2, "0")).join(" ");
    return `<Buffer ${hex}${value.length > 50 ? " ..." : ""}>`;
  }

  if (seen.has(value)) return "[Circular *1]";
  if (depth > options.depth) return Array.isArray(value) ? "[Array]" : "[Object]";
  seen.add(value);
  try {
    if (Array.isArray(value)) {
      const items = value.map((v) => formatValue(v, options, seen, depth + 1));
      return `[ ${items.join(", ")} ]`.replace("[  ]", "[]");
    }
    if (value instanceof Map) {
      const items = [...value].map(([k, v]) => `${formatValue(k, options, seen, depth + 1)} => ${formatValue(v, options, seen, depth + 1)}`);
      return `Map(${value.size}) { ${items.join(", ")} }`;
    }
    if (value instanceof Set) {
      const items = [...value].map((v) => formatValue(v, options, seen, depth + 1));
      return `Set(${value.size}) { ${items.join(", ")} }`;
    }
    const keys = options.showHidden ? Reflect.ownKeys(value) : Object.keys(value);
    const ctor = value.constructor && value.constructor.name;
    const prefix = ctor && ctor !== "Object" ? `${ctor} ` : "";
    if (keys.length === 0) return prefix ? `${prefix}{}` : "{}";
    const items = keys.map((k) => {
      const label = typeof k === "symbol" ? `[${k.toString()}]` : /^[A-Za-z_$][\w$]*$/.test(k) ? k : `'${k}'`;
      return `${label}: ${formatValue(value[k], options, seen, depth + 1)}`;
    });
    return `${prefix}{ ${items.join(", ")} }`;
  } finally {
    seen.delete(value);
  }
}

// ---- format -----------------------------------------------------------------------------------

const formatRegExp = /%[sdifjoOc%]/g;

function formatWithOptions(inspectOptions, ...args) {
  const first = args[0];
  let str = "";
  let a = 0;
  if (typeof first === "string") {
    if (args.length === 1) return first;
    a = 1;
    str = first.replace(formatRegExp, (match) => {
      if (match === "%%") return "%";
      if (a >= args.length) return match;
      const arg = args[a++];
      switch (match) {
        case "%s":
          return typeof arg === "bigint" ? `${arg}n` : typeof arg === "object" && arg !== null ? inspect(arg, { ...inspectOptions, depth: 0 }) : String(arg);
        case "%d":
          return typeof arg === "bigint" ? `${arg}n` : String(Number(arg));
        case "%i":
          return String(parseInt(arg, 10));
        case "%f":
          return String(parseFloat(arg));
        case "%j":
          try { return JSON.stringify(arg); } catch { return "[Circular]"; }
        case "%o":
        case "%O":
          return inspect(arg, { ...inspectOptions, showHidden: match === "%o" });
        case "%c":
          return ""; // CSS directive — ignored outside a browser
        default:
          return match;
      }
    });
  }
  for (; a < args.length; a++) {
    const arg = args[a];
    str += (str ? " " : "") + (typeof arg === "string" ? arg : inspect(arg, inspectOptions));
  }
  return str;
}

function format(...args) {
  return formatWithOptions({}, ...args);
}

// ---- deprecate / promisify / callbackify ------------------------------------------------------

function deprecate(fn, msg, code) {
  let warned = false;
  function deprecated(...args) {
    if (!warned) {
      warned = true;
      if (typeof console !== "undefined" && console.error) {
        console.error(`DeprecationWarning:${code ? ` [${code}]` : ""} ${msg}`);
      }
    }
    return Reflect.apply(fn, this, args);
  }
  return deprecated;
}

const kCustomPromisify = Symbol.for("nodejs.util.promisify.custom");

function promisify(original) {
  if (typeof original !== "function") throw new TypeError('The "original" argument must be of type function');
  if (original[kCustomPromisify]) return original[kCustomPromisify];
  function fn(...args) {
    return new Promise((resolve, reject) => {
      original.call(this, ...args, (err, ...values) => {
        if (err) return reject(err);
        resolve(values.length > 1 ? values[0] : values[0]);
      });
    });
  }
  Object.setPrototypeOf(fn, Object.getPrototypeOf(original));
  return fn;
}
promisify.custom = kCustomPromisify;

function callbackify(original) {
  if (typeof original !== "function") throw new TypeError('The "original" argument must be of type function');
  return function (...args) {
    const cb = args.pop();
    original.apply(this, args).then(
      (value) => queueMicrotask(() => cb(null, value)),
      (err) => queueMicrotask(() => cb(err || new Error("Promise was rejected with a falsy value"))),
    );
  };
}

// ---- types + legacy predicates ----------------------------------------------------------------

const types = {
  isDate: (v) => v instanceof Date,
  isRegExp: (v) => v instanceof RegExp,
  isNativeError: (v) => v instanceof Error,
  isMap: (v) => v instanceof Map,
  isSet: (v) => v instanceof Set,
  isPromise: (v) => v != null && typeof v.then === "function",
  isAnyArrayBuffer: (v) => v instanceof ArrayBuffer,
  isArrayBuffer: (v) => v instanceof ArrayBuffer,
  isTypedArray: (v) => ArrayBuffer.isView(v) && !(v instanceof DataView),
  isUint8Array: (v) => v instanceof Uint8Array,
  isAsyncFunction: (v) => typeof v === "function" && v.constructor && v.constructor.name === "AsyncFunction",
};

const util = {
  inherits,
  inspect,
  format,
  formatWithOptions,
  deprecate,
  promisify,
  callbackify,
  types,
  isArray: Array.isArray,
  isDate: types.isDate,
  isRegExp: types.isRegExp,
  isError: types.isNativeError,
  isFunction: (v) => typeof v === "function",
  isString: (v) => typeof v === "string",
  isNumber: (v) => typeof v === "number",
  isBoolean: (v) => typeof v === "boolean",
  isNull: (v) => v === null,
  isNullOrUndefined: (v) => v == null,
  isUndefined: (v) => v === undefined,
  isObject: (v) => v !== null && typeof v === "object",
  isPrimitive: (v) => v === null || (typeof v !== "object" && typeof v !== "function"),
  isBuffer: (v) => (typeof Buffer !== "undefined" && Buffer.isBuffer ? Buffer.isBuffer(v) : false),
  _extend: (target, source) => Object.assign(target, source),
  TextEncoder: globalThis.TextEncoder,
  TextDecoder: globalThis.TextDecoder,
};

__builtins.set("util", util);
