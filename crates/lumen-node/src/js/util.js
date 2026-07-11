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

// ---- util/types -------------------------------------------------------------------------------
// The engine gives every builtin an accurate `Object.prototype.toString` tag (Map, Promise,
// GeneratorFunction, Map Iterator, boxed Number/String/…, ArrayBuffer vs SharedArrayBuffer, …),
// so most of these predicates are exact. The few the engine cannot observe are noted inline and
// return false honestly rather than guessing.

const objToString = Object.prototype.toString;
const tagOf = (v) => {
  const s = objToString.call(v);
  return s.slice(8, s.length - 1); // "[object X]" -> "X"
};
const isObjectValue = (v) => v !== null && typeof v === "object";
const BOXED_TAGS = new Set(["Number", "String", "Boolean", "Symbol", "BigInt"]);

const types = {
  // C++ external pointers have no JS representation in lumen, so nothing is ever an External.
  isExternal: () => false,
  // Proxies are transparent to Object.prototype.toString and there is no engine hook to unwrap
  // them, so membership is undetectable; report false rather than a fake positive.
  isProxy: () => false,
  // node:crypto exposes no KeyObject class in lumen, so a KeyObject can never exist here.
  isKeyObject: () => false,

  isDate: (v) => tagOf(v) === "Date",
  isRegExp: (v) => tagOf(v) === "RegExp",
  isArgumentsObject: (v) => tagOf(v) === "Arguments",
  isNativeError: (v) => v instanceof Error,
  isMap: (v) => tagOf(v) === "Map",
  isSet: (v) => tagOf(v) === "Set",
  isMapIterator: (v) => tagOf(v) === "Map Iterator",
  isSetIterator: (v) => tagOf(v) === "Set Iterator",
  isWeakMap: (v) => tagOf(v) === "WeakMap",
  isWeakSet: (v) => tagOf(v) === "WeakSet",
  isPromise: (v) => tagOf(v) === "Promise",
  isGeneratorFunction: (v) => tagOf(v) === "GeneratorFunction",
  isAsyncFunction: (v) => tagOf(v) === "AsyncFunction",
  isGeneratorObject: (v) => tagOf(v) === "Generator",
  isModuleNamespaceObject: (v) => tagOf(v) === "Module",

  isNumberObject: (v) => isObjectValue(v) && tagOf(v) === "Number",
  isStringObject: (v) => isObjectValue(v) && tagOf(v) === "String",
  isBooleanObject: (v) => isObjectValue(v) && tagOf(v) === "Boolean",
  isSymbolObject: (v) => isObjectValue(v) && tagOf(v) === "Symbol",
  isBigIntObject: (v) => isObjectValue(v) && tagOf(v) === "BigInt",
  isBoxedPrimitive: (v) => isObjectValue(v) && BOXED_TAGS.has(tagOf(v)),

  isArrayBuffer: (v) => tagOf(v) === "ArrayBuffer",
  isSharedArrayBuffer: (v) => tagOf(v) === "SharedArrayBuffer",
  isAnyArrayBuffer: (v) => {
    const t = tagOf(v);
    return t === "ArrayBuffer" || t === "SharedArrayBuffer";
  },
  isDataView: (v) => tagOf(v) === "DataView",
  isArrayBufferView: (v) => ArrayBuffer.isView(v),
  isTypedArray: (v) => ArrayBuffer.isView(v) && tagOf(v) !== "DataView",

  isUint8Array: (v) => v instanceof Uint8Array,
  isUint8ClampedArray: (v) => v instanceof Uint8ClampedArray,
  isUint16Array: (v) => v instanceof Uint16Array,
  isUint32Array: (v) => v instanceof Uint32Array,
  isInt8Array: (v) => v instanceof Int8Array,
  isInt16Array: (v) => v instanceof Int16Array,
  isInt32Array: (v) => v instanceof Int32Array,
  isFloat16Array: (v) => typeof Float16Array !== "undefined" && v instanceof Float16Array,
  isFloat32Array: (v) => v instanceof Float32Array,
  isFloat64Array: (v) => v instanceof Float64Array,
  isBigInt64Array: (v) => v instanceof BigInt64Array,
  isBigUint64Array: (v) => v instanceof BigUint64Array,

  // CryptoKey is a WebCrypto global in lumen, so this one is observable.
  isCryptoKey: (v) => typeof CryptoKey !== "undefined" && v instanceof CryptoKey,
};

__builtins.set("util/types", types);

// ---- coded errors -----------------------------------------------------------------------------
// Node attaches a stable `.code` to these errors; downstream code (and our own tests) branch on it.

function codedError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

// ---- system error names -----------------------------------------------------------------------
// libuv's negative errno table (values are platform-specific; this is the darwin/macOS set that
// lumen ships on, matching Node's own numbers on this platform).

const SYS_ERRORS = {
  "-7": ["E2BIG", "argument list too long"], "-13": ["EACCES", "permission denied"],
  "-48": ["EADDRINUSE", "address already in use"], "-49": ["EADDRNOTAVAIL", "address not available"],
  "-47": ["EAFNOSUPPORT", "address family not supported"], "-35": ["EAGAIN", "resource temporarily unavailable"],
  "-3000": ["EAI_ADDRFAMILY", "address family not supported"], "-3001": ["EAI_AGAIN", "temporary failure"],
  "-3002": ["EAI_BADFLAGS", "bad ai_flags value"], "-3013": ["EAI_BADHINTS", "invalid value for hints"],
  "-3003": ["EAI_CANCELED", "request canceled"], "-3004": ["EAI_FAIL", "permanent failure"],
  "-3005": ["EAI_FAMILY", "ai_family not supported"], "-3006": ["EAI_MEMORY", "out of memory"],
  "-3007": ["EAI_NODATA", "no address"], "-3008": ["EAI_NONAME", "unknown node or service"],
  "-3009": ["EAI_OVERFLOW", "argument buffer overflow"], "-3014": ["EAI_PROTOCOL", "resolved protocol is unknown"],
  "-3010": ["EAI_SERVICE", "service not available for socket type"], "-3011": ["EAI_SOCKTYPE", "socket type not supported"],
  "-37": ["EALREADY", "connection already in progress"], "-9": ["EBADF", "bad file descriptor"],
  "-16": ["EBUSY", "resource busy or locked"], "-89": ["ECANCELED", "operation canceled"],
  "-4080": ["ECHARSET", "invalid Unicode character"], "-53": ["ECONNABORTED", "software caused connection abort"],
  "-61": ["ECONNREFUSED", "connection refused"], "-54": ["ECONNRESET", "connection reset by peer"],
  "-39": ["EDESTADDRREQ", "destination address required"], "-17": ["EEXIST", "file already exists"],
  "-14": ["EFAULT", "bad address in system call argument"], "-27": ["EFBIG", "file too large"],
  "-65": ["EHOSTUNREACH", "host is unreachable"], "-4": ["EINTR", "interrupted system call"],
  "-22": ["EINVAL", "invalid argument"], "-5": ["EIO", "i/o error"],
  "-56": ["EISCONN", "socket is already connected"], "-21": ["EISDIR", "illegal operation on a directory"],
  "-62": ["ELOOP", "too many symbolic links encountered"], "-24": ["EMFILE", "too many open files"],
  "-40": ["EMSGSIZE", "message too long"], "-63": ["ENAMETOOLONG", "name too long"],
  "-50": ["ENETDOWN", "network is down"], "-51": ["ENETUNREACH", "network is unreachable"],
  "-23": ["ENFILE", "file table overflow"], "-55": ["ENOBUFS", "no buffer space available"],
  "-19": ["ENODEV", "no such device"], "-2": ["ENOENT", "no such file or directory"],
  "-12": ["ENOMEM", "not enough memory"], "-4056": ["ENONET", "machine is not on the network"],
  "-42": ["ENOPROTOOPT", "protocol not available"], "-28": ["ENOSPC", "no space left on device"],
  "-78": ["ENOSYS", "function not implemented"], "-57": ["ENOTCONN", "socket is not connected"],
  "-20": ["ENOTDIR", "not a directory"], "-66": ["ENOTEMPTY", "directory not empty"],
  "-38": ["ENOTSOCK", "socket operation on non-socket"], "-45": ["ENOTSUP", "operation not supported on socket"],
  "-84": ["EOVERFLOW", "value too large for defined data type"], "-1": ["EPERM", "operation not permitted"],
  "-32": ["EPIPE", "broken pipe"], "-100": ["EPROTO", "protocol error"],
  "-43": ["EPROTONOSUPPORT", "protocol not supported"], "-41": ["EPROTOTYPE", "protocol wrong type for socket"],
  "-34": ["ERANGE", "result too large"], "-30": ["EROFS", "read-only file system"],
  "-58": ["ESHUTDOWN", "cannot send after transport endpoint shutdown"], "-29": ["ESPIPE", "invalid seek"],
  "-3": ["ESRCH", "no such process"], "-60": ["ETIMEDOUT", "connection timed out"],
  "-26": ["ETXTBSY", "text file is busy"], "-18": ["EXDEV", "cross-device link not permitted"],
  "-4094": ["UNKNOWN", "unknown error"], "-4095": ["EOF", "end of file"],
  "-6": ["ENXIO", "no such device or address"], "-31": ["EMLINK", "too many links"],
  "-64": ["EHOSTDOWN", "host is down"], "-4030": ["EREMOTEIO", "remote I/O error"],
  "-25": ["ENOTTY", "inappropriate ioctl for device"], "-79": ["EFTYPE", "inappropriate file type or format"],
  "-92": ["EILSEQ", "illegal byte sequence"], "-44": ["ESOCKTNOSUPPORT", "socket type not supported"],
  "-96": ["ENODATA", "no data available"], "-4023": ["EUNATCH", "protocol driver not attached"],
};

const sysErrorMap = new Map();
for (const key of Object.keys(SYS_ERRORS)) sysErrorMap.set(Number(key), SYS_ERRORS[key]);

function validateErrno(err) {
  if (typeof err !== "number") throw new TypeError('The "err" argument must be of type number.');
  if (err >= 0 || !Number.isInteger(err)) {
    throw new RangeError(`The value of "err" is out of range. It must be a negative integer. Received ${err}`);
  }
}
function getSystemErrorName(err) {
  validateErrno(err);
  const entry = sysErrorMap.get(err);
  return entry ? entry[0] : `Unknown system error ${err}`;
}
function getSystemErrorMessage(err) {
  validateErrno(err);
  const entry = sysErrorMap.get(err);
  return entry ? entry[1] : `Unknown system error ${err}`;
}
function getSystemErrorMap() {
  return new Map(sysErrorMap);
}

function _errnoException(err, syscall, original) {
  const name = getSystemErrorName(err);
  let message = `${syscall} ${name}`;
  if (original) message += ` ${original}`;
  const e = new Error(message);
  e.errno = err;
  e.code = name;
  e.syscall = syscall;
  return e;
}
function _exceptionWithHostPort(err, syscall, address, port, additional) {
  const name = getSystemErrorName(err);
  let details = "";
  if (port && port > 0) details = ` ${address}:${port}`;
  else if (address) details = ` ${address}`;
  if (additional) details += ` - Local (${additional})`;
  const e = new Error(`${syscall} ${name}${details}`);
  e.errno = err;
  e.code = name;
  e.syscall = syscall;
  if (address) e.address = address;
  if (port) e.port = port;
  return e;
}

// ---- log / debuglog ---------------------------------------------------------------------------

const LOG_MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const pad2 = (n) => String(n).padStart(2, "0");
function log(...args) {
  const d = new Date();
  const stamp = `${pad2(d.getDate())} ${LOG_MONTHS[d.getMonth()]} ${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
  if (typeof console !== "undefined" && console.log) console.log("%s - %s", stamp, format(...args));
}

function debuglog(section, cb) {
  section = String(section).toUpperCase();
  let enabledState = null;
  const isEnabled = () => {
    if (enabledState === null) {
      const env = (typeof process !== "undefined" && process.env && process.env.NODE_DEBUG) || "";
      enabledState = env
        .split(/[\s,]+/)
        .filter(Boolean)
        .some((token) => new RegExp(`^${token.toUpperCase().replace(/[*]/g, ".*")}$`).test(section));
    }
    return enabledState;
  };
  let notified = false;
  const logger = function (...args) {
    if (!isEnabled()) return;
    if (!notified && typeof cb === "function") {
      notified = true;
      cb(logger);
    }
    const pid = (typeof process !== "undefined" && process.pid) || 0;
    if (typeof console !== "undefined" && console.error) {
      console.error("%s %d: %s", section, pid, formatWithOptions({}, ...args));
    }
  };
  Object.defineProperty(logger, "enabled", { get: isEnabled, enumerable: true, configurable: true });
  return logger;
}

// ---- ANSI text helpers ------------------------------------------------------------------------

const ANSI_CODES = {
  reset: [0, 0], bold: [1, 22], dim: [2, 22], italic: [3, 23], underline: [4, 24],
  blink: [5, 25], inverse: [7, 27], hidden: [8, 28], strikethrough: [9, 29],
  doubleunderline: [21, 24], black: [30, 39], red: [31, 39], green: [32, 39],
  yellow: [33, 39], blue: [34, 39], magenta: [35, 39], cyan: [36, 39], white: [37, 39],
  bgBlack: [40, 49], bgRed: [41, 49], bgGreen: [42, 49], bgYellow: [43, 49], bgBlue: [44, 49],
  bgMagenta: [45, 49], bgCyan: [46, 49], bgWhite: [47, 49], framed: [51, 54], overlined: [53, 55],
  gray: [90, 39], redBright: [91, 39], greenBright: [92, 39], yellowBright: [93, 39],
  blueBright: [94, 39], magentaBright: [95, 39], cyanBright: [96, 39], whiteBright: [97, 39],
  bgGray: [100, 49], bgRedBright: [101, 49], bgGreenBright: [102, 49], bgYellowBright: [103, 49],
  bgBlueBright: [104, 49], bgMagentaBright: [105, 49], bgCyanBright: [106, 49], bgWhiteBright: [107, 49],
};

// Matches ANSI/VT escape sequences (CSI colour/style codes and OSC strings).
const ANSI_PATTERN = /[][[\]()#;?]*(?:(?:(?:\d{1,4}(?:;\d{0,4})*)?[0-9A-ORZcf-nqry=><~])|(?:[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?)/g;

function stripVTControlCharacters(str) {
  if (typeof str !== "string") throw new TypeError('The "str" argument must be of type string.');
  return str.replace(ANSI_PATTERN, "");
}

function styleText(fmt, text, options) {
  if (typeof text !== "string") throw new TypeError('The "text" argument must be of type string.');
  const opts = options || {};
  // Honour an explicitly non-TTY stream by returning the text unstyled, like Node. With no stream
  // hint we apply the codes (lumen has no reliable isTTY on its stdout wrapper).
  if (opts.stream && opts.stream.isTTY === false) return text;
  const formats = Array.isArray(fmt) ? fmt : [fmt];
  let open = "";
  let close = "";
  for (const name of formats) {
    const code = ANSI_CODES[name];
    if (code === undefined) {
      throw codedError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'format' must be a valid color/style. Received ${JSON.stringify(name)}`);
    }
    open += `[${code[0]}m`;
    close = `[${code[1]}m${close}`;
  }
  return `${open}${text}${close}`;
}

// ---- toUSVString ------------------------------------------------------------------------------
// Replace lone/unpaired surrogate code units with U+FFFD, yielding a well-formed USV string.

function toUSVString(str) {
  str = `${str}`;
  return str.replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, "�");
}

// ---- isDeepStrictEqual ------------------------------------------------------------------------

function ownEnumerableKeys(obj) {
  const keys = [];
  for (const key of Reflect.ownKeys(obj)) {
    const desc = Object.getOwnPropertyDescriptor(obj, key);
    if (desc && desc.enumerable) keys.push(key);
  }
  return keys;
}

function bytesEqual(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function deepStrictEqual(a, b, seen) {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;

  const tag = tagOf(a);
  if (tag !== tagOf(b)) return false;

  const prior = seen.get(a);
  if (prior !== undefined) return prior === b;
  seen.set(a, b);

  if (tag === "Date") {
    const ta = a.getTime();
    const tb = b.getTime();
    return ta === tb || (Number.isNaN(ta) && Number.isNaN(tb));
  }
  if (tag === "RegExp") {
    if (a.source !== b.source || a.flags !== b.flags) return false;
  }
  if (BOXED_TAGS.has(tag)) {
    if (!Object.is(a.valueOf(), b.valueOf())) return false;
  }
  if (tag === "ArrayBuffer" || tag === "SharedArrayBuffer") {
    if (!bytesEqual(new Uint8Array(a), new Uint8Array(b))) return false;
  }
  if (ArrayBuffer.isView(a) && tag !== "DataView") {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      if (!deepStrictEqual(a[i], b[i], seen)) return false;
    }
  } else if (tag === "DataView") {
    if (!bytesEqual(new Uint8Array(a.buffer, a.byteOffset, a.byteLength), new Uint8Array(b.buffer, b.byteOffset, b.byteLength))) {
      return false;
    }
  }
  if (tag === "Map") {
    if (a.size !== b.size) return false;
    const bRemaining = new Set(b.keys());
    for (const [k, v] of a) {
      if (b.has(k)) {
        if (!deepStrictEqual(v, b.get(k), seen)) return false;
        bRemaining.delete(k);
      } else {
        // Object key: find a structurally-equal, not-yet-matched key in b.
        let matched = false;
        for (const bk of bRemaining) {
          if (deepStrictEqual(k, bk, seen) && deepStrictEqual(v, b.get(bk), seen)) {
            bRemaining.delete(bk);
            matched = true;
            break;
          }
        }
        if (!matched) return false;
      }
    }
  }
  if (tag === "Set") {
    if (a.size !== b.size) return false;
    const bRemaining = new Set(b);
    for (const v of a) {
      if (bRemaining.has(v)) {
        bRemaining.delete(v);
      } else {
        let matched = false;
        for (const bv of bRemaining) {
          if (deepStrictEqual(v, bv, seen)) {
            bRemaining.delete(bv);
            matched = true;
            break;
          }
        }
        if (!matched) return false;
      }
    }
  }

  const keysA = ownEnumerableKeys(a);
  const keysB = ownEnumerableKeys(b);
  if (keysA.length !== keysB.length) return false;
  for (const key of keysA) {
    if (!Object.prototype.propertyIsEnumerable.call(b, key)) return false;
    if (!deepStrictEqual(a[key], b[key], seen)) return false;
  }
  return true;
}

function isDeepStrictEqual(a, b) {
  return deepStrictEqual(a, b, new Map());
}

// ---- AbortSignal helpers ----------------------------------------------------------------------

function aborted(signal, resource) {
  if (signal == null || typeof signal.addEventListener !== "function") {
    throw new TypeError('The "signal" argument must be an instance of AbortSignal.');
  }
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    signal.addEventListener("abort", () => resolve(), { once: true });
  });
}

// lumen has no MessagePort transfer, so the "transferable" marker is a no-op; these return real,
// fully-functional controllers/signals so the common non-transfer usage works.
function transferableAbortController() {
  return new AbortController();
}
function transferableAbortSignal(signal) {
  return signal;
}

// ---- getCallSites -----------------------------------------------------------------------------
// lumen carries no per-frame source information (see preamble.js), so there are no observable call
// sites to report; return an empty list rather than fabricated frames.

function getCallSites() {
  return [];
}
const getCallSite = getCallSites;

// ---- parseEnv ---------------------------------------------------------------------------------

function parseEnv(content) {
  content = `${content}`;
  const result = Object.create(null);
  const n = content.length;
  let i = 0;
  const isSpace = (c) => c === " " || c === "\t" || c === "\r" || c === "\n";
  while (i < n) {
    while (i < n && isSpace(content[i])) i++;
    if (i >= n) break;
    if (content[i] === "#") {
      while (i < n && content[i] !== "\n") i++;
      continue;
    }
    if (content.startsWith("export", i) && isSpace(content[i + 6] || " ")) {
      i += 6;
      while (i < n && (content[i] === " " || content[i] === "\t")) i++;
    }
    let key = "";
    while (i < n && content[i] !== "=" && content[i] !== "\n") key += content[i++];
    key = key.trim();
    if (content[i] !== "=") {
      while (i < n && content[i] !== "\n") i++;
      continue;
    }
    i++; // skip '='
    while (i < n && (content[i] === " " || content[i] === "\t")) i++;
    let value = "";
    const quote = content[i];
    if (quote === '"' || quote === "'" || quote === "`") {
      i++;
      while (i < n && content[i] !== quote) {
        if (quote === '"' && content[i] === "\\" && i + 1 < n) {
          const next = content[i + 1];
          value += next === "n" ? "\n" : next === "t" ? "\t" : next === "r" ? "\r" : next;
          i += 2;
          continue;
        }
        value += content[i++];
      }
      i++; // closing quote
    } else {
      while (i < n && content[i] !== "\n") value += content[i++];
      const comment = value.indexOf(" #");
      if (comment !== -1) value = value.slice(0, comment);
      value = value.trim();
    }
    if (key && key !== "__proto__") result[key] = value;
  }
  return result;
}

// ---- parseArgs --------------------------------------------------------------------------------
// Faithful port of Node's tokenizer + option store, covering strict/non-strict, short groups,
// inline values, `multiple`, defaults, positionals, the `--` terminator, and `tokens`.

function findLongFromShort(short, options) {
  for (const name of Object.keys(options)) {
    if (options[name] && options[name].short === short) return name;
  }
  return undefined;
}
function optionType(long, options) {
  return options[long] ? options[long].type : undefined;
}
function isOptionLikeValue(value) {
  return value != null && value.length > 1 && value[0] === "-";
}

function tokenizeArgs(args, options) {
  const tokens = [];
  const remaining = args.slice();
  let groupCount = 0;
  while (remaining.length > 0) {
    const arg = remaining.shift();
    const nextArg = remaining[0];
    let index = args.length - remaining.length - 1 - groupCount;

    if (arg === "--") {
      tokens.push({ kind: "option-terminator", index });
      for (const rest of remaining) tokens.push({ kind: "positional", index: ++index, value: rest });
      break;
    }

    const isShort = arg.length >= 2 && arg[0] === "-" && arg[1] !== "-";
    const isLong = arg.length > 2 && arg[0] === "-" && arg[1] === "-";

    if (isShort && arg.length === 2) {
      // lone short option: -f
      const short = arg[1];
      const long = findLongFromShort(short, options) ?? short;
      let value;
      let inlineValue;
      if (optionType(long, options) === "string" && nextArg !== undefined) {
        value = remaining.shift();
        inlineValue = false;
      }
      tokens.push({ kind: "option", name: long, rawName: arg, index, value, inlineValue });
      continue;
    }

    if (isShort && arg.length > 2) {
      const firstShort = arg[1];
      const firstLong = findLongFromShort(firstShort, options);
      if (optionType(firstLong, options) === "string") {
        // -xVALUE : first short is a string option, remainder is its inline value
        tokens.push({ kind: "option", name: firstLong, rawName: `-${firstShort}`, index, value: arg.slice(2), inlineValue: true });
        continue;
      }
      // short option group: expand and reprocess
      const expanded = [];
      for (let c = 1; c < arg.length; c++) {
        const short = arg[c];
        const long = findLongFromShort(short, options);
        if (optionType(long, options) !== "string" || c === arg.length - 1) {
          expanded.push(`-${short}`);
        } else {
          expanded.push(`-${arg.slice(c)}`);
          break;
        }
      }
      remaining.unshift(...expanded);
      groupCount += expanded.length - 1;
      continue;
    }

    if (isLong) {
      const eq = arg.indexOf("=");
      if (eq === -1) {
        const long = arg.slice(2);
        let value;
        let inlineValue;
        if (optionType(long, options) === "string" && nextArg !== undefined) {
          value = remaining.shift();
          inlineValue = false;
        }
        tokens.push({ kind: "option", name: long, rawName: arg, index, value, inlineValue });
      } else {
        const long = arg.slice(2, eq);
        tokens.push({ kind: "option", name: long, rawName: `--${long}`, index, value: arg.slice(eq + 1), inlineValue: true });
      }
      continue;
    }

    tokens.push({ kind: "positional", index, value: arg });
  }
  return tokens;
}

function storeOption(long, value, options, values) {
  if (long === "__proto__") return;
  const newValue = value === undefined ? true : value;
  if (options[long] && options[long].multiple) {
    if (Object.prototype.hasOwnProperty.call(values, long)) values[long].push(newValue);
    else values[long] = [newValue];
  } else {
    values[long] = newValue;
  }
}

function parseArgs(config) {
  config = config || {};
  const args = config.args ?? (typeof process !== "undefined" && process.argv ? process.argv.slice(2) : []);
  const strict = config.strict ?? true;
  const allowPositionals = config.allowPositionals ?? !strict;
  const returnTokens = config.tokens ?? false;
  const options = config.options ?? {};

  if (typeof options !== "object" || options === null) {
    throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "options" argument must be of type object.');
  }
  for (const name of Object.keys(options)) {
    const opt = options[name];
    if (typeof opt !== "object" || opt === null || (opt.type !== "string" && opt.type !== "boolean")) {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `options.${name}.type must be "string" or "boolean".`);
    }
  }

  const tokens = tokenizeArgs(args, options);
  const result = { values: Object.create(null), positionals: [] };
  if (returnTokens) result.tokens = tokens;

  for (const token of tokens) {
    if (token.kind === "option") {
      if (strict) {
        if (!Object.prototype.hasOwnProperty.call(options, token.name)) {
          throw codedError(TypeError, "ERR_PARSE_ARGS_UNKNOWN_OPTION", `Unknown option '${token.rawName}'`);
        }
        if (!token.inlineValue && isOptionLikeValue(token.value)) {
          throw codedError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", `Option '${token.rawName}' argument is ambiguous. Received '${token.value}'`);
        }
        const type = optionType(token.name, options);
        if (type === "string" && typeof token.value !== "string") {
          throw codedError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", `Option '${token.rawName} <value>' argument missing`);
        }
        if (type === "boolean" && token.value != null) {
          throw codedError(TypeError, "ERR_PARSE_ARGS_INVALID_OPTION_VALUE", `Option '${token.rawName}' does not take an argument`);
        }
      }
      storeOption(token.name, token.value, options, result.values);
    } else if (token.kind === "positional") {
      if (!allowPositionals) {
        throw codedError(TypeError, "ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL", `Unexpected argument '${token.value}'. This command does not take positional arguments`);
      }
      result.positionals.push(token.value);
    }
  }

  for (const name of Object.keys(options)) {
    if (options[name].default !== undefined && !Object.prototype.hasOwnProperty.call(result.values, name)) {
      result.values[name] = options[name].default;
    }
  }
  return result;
}

// ---- diff (LCS) -------------------------------------------------------------------------------
// Returns an array of [operation, value] triples: 0 = unchanged, 1 = only in `actual`,
// -1 = only in `expected`. Strings are compared code point by code point.

function diff(actual, expected) {
  const isStr = typeof actual === "string";
  if (isStr) {
    if (typeof expected !== "string") {
      throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "expected" argument must be of type string.');
    }
    if (actual === expected) return [];
  } else if (!Array.isArray(actual) || !Array.isArray(expected)) {
    throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", 'The "actual" and "expected" arguments must both be strings or both be arrays.');
  }

  const a = isStr ? [...actual] : actual;
  const b = isStr ? [...expected] : expected;
  for (let i = 0; i < a.length; i++) {
    if (typeof a[i] !== "string") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "actual[${i}]" argument must be of type string.`);
  }
  for (let i = 0; i < b.length; i++) {
    if (typeof b[i] !== "string") throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "expected[${i}]" argument must be of type string.`);
  }

  const m = a.length;
  const k = b.length;
  const lcs = Array.from({ length: m + 1 }, () => new Array(k + 1).fill(0));
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= k; j++) {
      lcs[i][j] = a[i - 1] === b[j - 1] ? lcs[i - 1][j - 1] + 1 : Math.max(lcs[i - 1][j], lcs[i][j - 1]);
    }
  }
  const out = [];
  let i = m;
  let j = k;
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && a[i - 1] === b[j - 1]) {
      out.push([0, a[i - 1]]);
      i--;
      j--;
    } else if (j > 0 && (i === 0 || lcs[i][j - 1] >= lcs[i - 1][j])) {
      out.push([-1, b[j - 1]]);
      j--;
    } else {
      out.push([1, a[i - 1]]);
      i--;
    }
  }
  out.reverse();
  return out;
}

// ---- MIMEType / MIMEParams --------------------------------------------------------------------

const HTTP_TOKEN = /^[!#$%&'*+\-.^_`|~A-Za-z0-9]+$/;
const NEEDS_QUOTE = /[^!#$%&'*+\-.^_`|~A-Za-z0-9]/;

function serializeParamValue(value) {
  if (value.length === 0 || NEEDS_QUOTE.test(value)) {
    return `"${value.replace(/["\\]/g, "\\$&")}"`;
  }
  return value;
}

class MIMEParams {
  #data = new Map();

  get(name) {
    name = `${name}`;
    return this.#data.has(name) ? this.#data.get(name) : null;
  }
  has(name) {
    return this.#data.has(`${name}`);
  }
  set(name, value) {
    name = `${name}`;
    value = `${value}`;
    if (!HTTP_TOKEN.test(name)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME parameter name "${name}" is invalid`);
    }
    this.#data.set(name, value);
  }
  delete(name) {
    this.#data.delete(`${name}`);
  }
  entries() {
    return this.#data.entries();
  }
  keys() {
    return this.#data.keys();
  }
  values() {
    return this.#data.values();
  }
  [Symbol.iterator]() {
    return this.#data.entries();
  }
  // Internal helpers used by the parser/serializer.
  _setRaw(name, value) {
    this.#data.set(name, value);
  }
  _serialize() {
    let out = "";
    for (const [name, value] of this.#data) out += `;${name}=${serializeParamValue(value)}`;
    return out;
  }
}

class MIMEType {
  #type;
  #subtype;
  #params = new MIMEParams();

  constructor(input) {
    input = `${input}`.trim();
    const slash = input.indexOf("/");
    if (slash === -1) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME syntax for "${input}" is invalid: missing "/"`);
    }
    const type = input.slice(0, slash).toLowerCase();
    let rest = input.slice(slash + 1);
    let subtype = rest;
    const semi = rest.indexOf(";");
    if (semi !== -1) {
      subtype = rest.slice(0, semi);
      rest = rest.slice(semi + 1);
    } else {
      rest = "";
    }
    subtype = subtype.trim().toLowerCase();
    if (!HTTP_TOKEN.test(type) || !HTTP_TOKEN.test(subtype)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME syntax for "${input}" is invalid`);
    }
    this.#type = type;
    this.#subtype = subtype;
    this.#parseParams(rest);
  }

  #parseParams(str) {
    let i = 0;
    const n = str.length;
    while (i < n) {
      while (i < n && (str[i] === ";" || str[i] === " " || str[i] === "\t")) i++;
      if (i >= n) break;
      let name = "";
      while (i < n && str[i] !== "=" && str[i] !== ";") name += str[i++];
      name = name.trim().toLowerCase();
      if (str[i] !== "=") {
        while (i < n && str[i] !== ";") i++;
        continue;
      }
      i++; // skip '='
      let value = "";
      if (str[i] === '"') {
        i++;
        while (i < n && str[i] !== '"') {
          if (str[i] === "\\" && i + 1 < n) {
            value += str[i + 1];
            i += 2;
            continue;
          }
          value += str[i++];
        }
        i++; // closing quote
        while (i < n && str[i] !== ";") i++;
      } else {
        while (i < n && str[i] !== ";") value += str[i++];
        value = value.trim();
      }
      if (name && HTTP_TOKEN.test(name) && !this.#params.has(name)) this.#params._setRaw(name, value);
    }
  }

  get type() {
    return this.#type;
  }
  set type(value) {
    value = `${value}`.toLowerCase();
    if (!HTTP_TOKEN.test(value)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME type "${value}" is invalid`);
    }
    this.#type = value;
  }
  get subtype() {
    return this.#subtype;
  }
  set subtype(value) {
    value = `${value}`.toLowerCase();
    if (!HTTP_TOKEN.test(value)) {
      throw codedError(TypeError, "ERR_INVALID_MIME_SYNTAX", `The MIME subtype "${value}" is invalid`);
    }
    this.#subtype = value;
  }
  get essence() {
    return `${this.#type}/${this.#subtype}`;
  }
  get params() {
    return this.#params;
  }
  toString() {
    return `${this.#type}/${this.#subtype}${this.#params._serialize()}`;
  }
  toJSON() {
    return this.toString();
  }
}

// ---- assembled export -------------------------------------------------------------------------

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
  isSymbol: (v) => typeof v === "symbol",
  _extend: (target, source) => Object.assign(target, source),
  isDeepStrictEqual,
  log,
  debuglog,
  debug: debuglog,
  parseArgs,
  parseEnv,
  styleText,
  stripVTControlCharacters,
  toUSVString,
  diff,
  getSystemErrorName,
  getSystemErrorMessage,
  getSystemErrorMap,
  aborted,
  transferableAbortController,
  transferableAbortSignal,
  getCallSite,
  getCallSites,
  _errnoException,
  _exceptionWithHostPort,
  MIMEType,
  MIMEParams,
  TextEncoder: globalThis.TextEncoder,
  TextDecoder: globalThis.TextDecoder,
};

__builtins.set("util", util);
