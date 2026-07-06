// node:path — posix and win32 implementations; the default export follows the host platform.

function makePath(sep, isWin) {
  const isAbs = isWin
    ? (p) => /^([a-zA-Z]:[\\/]|[\\/][\\/]|[\\/])/.test(p)
    : (p) => p.startsWith("/");

  function normalizeArray(parts, allowAboveRoot) {
    const res = [];
    for (const p of parts) {
      if (p === "" || p === ".") continue;
      if (p === "..") {
        if (res.length && res[res.length - 1] !== "..") res.pop();
        else if (allowAboveRoot) res.push("..");
      } else {
        res.push(p);
      }
    }
    return res;
  }

  const splitRe = isWin ? /[\\/]+/ : /\/+/;

  const path = {
    sep,
    delimiter: isWin ? ";" : ":",
    isAbsolute: isAbs,
    normalize(p) {
      p = String(p);
      if (p.length === 0) return ".";
      const absolute = isAbs(p);
      const trailing = /[\\/]$/.test(p);
      let parts = normalizeArray(p.split(splitRe), !absolute).join(sep);
      if (!parts && !absolute) parts = ".";
      if (parts && trailing) parts += sep;
      return (absolute ? sep : "") + parts;
    },
    join(...args) {
      const joined = args.filter((a) => a != null && a !== "").join(sep);
      return joined === "" ? "." : path.normalize(joined);
    },
    resolve(...args) {
      let resolved = "";
      let absolute = false;
      for (let i = args.length - 1; i >= 0 && !absolute; i--) {
        const p = i >= 0 ? String(args[i]) : "";
        if (p === "") continue;
        resolved = p + sep + resolved;
        absolute = isAbs(p);
      }
      if (!absolute) resolved = (isWin ? "" : process.cwd()) + sep + resolved;
      const parts = normalizeArray(resolved.split(splitRe), !absolute).join(sep);
      return absolute ? sep + parts : parts || ".";
    },
    dirname(p) {
      p = String(p);
      const parts = p.split(splitRe);
      while (parts.length && parts[parts.length - 1] === "") parts.pop();
      if (parts.length <= 1) return isAbs(p) ? sep : ".";
      parts.pop();
      const d = parts.join(sep);
      return d === "" ? (isAbs(p) ? sep : ".") : d;
    },
    basename(p, ext) {
      p = String(p);
      const parts = p.split(splitRe).filter((x) => x !== "");
      let base = parts.length ? parts[parts.length - 1] : "";
      if (ext && base.endsWith(ext) && base !== ext) base = base.slice(0, -ext.length);
      return base;
    },
    extname(p) {
      const base = path.basename(String(p));
      const i = base.lastIndexOf(".");
      return i <= 0 ? "" : base.slice(i);
    },
    parse(p) {
      const root = isAbs(p) ? sep : "";
      const dir = path.dirname(p);
      const base = path.basename(p);
      const ext = path.extname(base);
      return { root, dir: dir === "." && root ? root : dir, base, ext, name: ext ? base.slice(0, -ext.length) : base };
    },
    format(obj) {
      const dir = obj.dir || obj.root || "";
      const base = obj.base || (obj.name || "") + (obj.ext || "");
      if (!dir) return base;
      return dir === obj.root ? dir + base : dir + sep + base;
    },
    relative(from, to) {
      from = path.resolve(from);
      to = path.resolve(to);
      if (from === to) return "";
      const fromParts = from.split(splitRe).filter(Boolean);
      const toParts = to.split(splitRe).filter(Boolean);
      let i = 0;
      while (i < fromParts.length && i < toParts.length && fromParts[i] === toParts[i]) i++;
      const up = fromParts.slice(i).map(() => "..");
      return [...up, ...toParts.slice(i)].join(sep) || ".";
    },
    toNamespacedPath(p) {
      return p;
    },
  };
  return path;
}

const posix = makePath("/", false);
const win32 = makePath("\\", true);
posix.posix = posix;
posix.win32 = win32;
win32.posix = posix;
win32.win32 = win32;

const isWindows = os_platform_is_win();
__builtins.set("path", isWindows ? win32 : posix);

function os_platform_is_win() {
  // __os.info() is available (os.js runs after this file? no — before). Use the raw op.
  return __os.info().platform === "win32";
}
