// Next.js pages-style Bun.FileSystemRouter. Route discovery is synchronous, matching Bun's
// constructor/reload contract, while matching itself is pure and ordered by route specificity.
{
  const fs = __builtins.get("fs");
  const path = __builtins.get("path");
  const DEFAULT_EXTENSIONS = [".js", ".jsx", ".ts", ".tsx"];

  function routeName(relative, extension) {
    let name = relative.slice(0, -extension.length).replace(/\\/g, "/");
    if (name === "index") return "/";
    if (name.endsWith("/index")) name = name.slice(0, -6);
    return "/" + name;
  }

  function compileRoute(name, filePath, relative) {
    const segments = name === "/" ? [] : name.slice(1).split("/");
    let kind = "exact";
    let score = 0;
    const parts = segments.map(segment => {
      let match = /^\[\[\.\.\.([^\]]+)\]\]$/.exec(segment);
      if (match) {
        kind = "optional-catch-all";
        score += 1;
        return { type: kind, name: match[1] };
      }
      match = /^\[\.\.\.([^\]]+)\]$/.exec(segment);
      if (match) {
        kind = "catch-all";
        score += 2;
        return { type: kind, name: match[1] };
      }
      match = /^\[([^\]]+)\]$/.exec(segment);
      if (match) {
        if (kind === "exact") kind = "dynamic";
        score += 10;
        return { type: "dynamic", name: match[1] };
      }
      score += 100;
      return { type: "exact", value: segment };
    });
    const rank = { exact: 4, dynamic: 3, "catch-all": 2, "optional-catch-all": 1 }[kind];
    return { name, filePath, relative, parts, kind, rank, score: score * 100 + segments.length };
  }

  function matchParts(route, pathname) {
    const segments = pathname === "/" ? [] : pathname.replace(/^\/+|\/+$/g, "").split("/").map(decodeURIComponent);
    const params = {};
    let cursor = 0;
    for (const part of route.parts) {
      if (part.type === "exact") {
        if (segments[cursor] !== part.value) return null;
        cursor++;
      } else if (part.type === "dynamic") {
        if (cursor >= segments.length) return null;
        params[part.name] = segments[cursor++];
      } else {
        const rest = segments.slice(cursor).join("/");
        if (!rest && part.type === "catch-all") return null;
        if (rest) params[part.name] = rest;
        cursor = segments.length;
      }
    }
    return cursor === segments.length ? params : null;
  }

  function queryObject(searchParams, params) {
    const query = {};
    for (const [key, value] of searchParams) {
      if (!(key in query)) query[key] = value;
      else if (Array.isArray(query[key])) query[key].push(value);
      else query[key] = [query[key], value];
    }
    return Object.assign(query, params);
  }

  class FileSystemRouter {
    constructor(options) {
      if (!options || !options.style) throw new TypeError('Expected \'style\' option (ex: "style": "nextjs")');
      if (options.style !== "nextjs") throw new TypeError("Only 'nextjs' style is currently implemented");
      let directoryExists = false;
      try { directoryExists = !!options.dir && fs.statSync(options.dir).isDirectory(); } catch (_) {}
      if (!directoryExists) {
        throw new Error(`Unable to find directory: ${options && options.dir}`);
      }
      this.style = "nextjs";
      this.dir = fs.realpathSync(options.dir);
      this.origin = options.origin ? String(options.origin).replace(/\/$/, "") : "http://localhost";
      this.assetPrefix = options.assetPrefix == null ? "" : String(options.assetPrefix);
      this.fileExtensions = (options.fileExtensions || DEFAULT_EXTENSIONS).map(ext => String(ext).replace(/^\*?\.?/, "."));
      this.reload();
    }

    reload() {
      const found = [];
      const walk = (directory, prefix) => {
        for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
          const absolute = path.join(directory, entry.name);
          const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
          if (entry.isDirectory()) walk(absolute, relative);
          else {
            const extension = this.fileExtensions.find(ext => entry.name.endsWith(ext));
            if (extension) found.push(compileRoute(routeName(relative, extension), absolute, relative));
          }
        }
      };
      walk(this.dir, "");
      found.sort((a, b) => b.rank - a.rank || b.score - a.score || a.name.localeCompare(b.name));
      this._compiled = found;
      const routes = {};
      for (const route of found) routes[route.name] = route.filePath;
      this.routes = routes;
    }

    match(input) {
      let raw = typeof input === "string" ? input : input && input.url;
      if (typeof raw !== "string") throw new TypeError("FileSystemRouter.match expects a path, Request, or Response");
      let url;
      try { url = new URL(raw, this.origin); } catch (_) { return null; }
      for (const route of this._compiled) {
        const params = matchParts(route, url.pathname);
        if (params === null) continue;
        const query = queryObject(url.searchParams, params);
        const prefix = this.assetPrefix ? "/" + this.assetPrefix.replace(/^\/+|\/+$/g, "") : "";
        const src = this.origin + prefix + "/" + route.relative.replace(/\\/g, "/");
        const result = {};
        for (const [key, value] of Object.entries({
          filePath: route.filePath, kind: route.kind, name: route.name,
          pathname: decodeURIComponent(url.pathname + url.search), src, params, query,
        })) Object.defineProperty(result, key, { value, enumerable: false });
        return result;
      }
      return null;
    }
  }
  Object.defineProperty(FileSystemRouter.prototype, Symbol.toStringTag, { value: "FileSystemRouter" });
  Object.defineProperty(globalThis, "__lumenFileSystemRouter", { value: FileSystemRouter, configurable: true });
}
