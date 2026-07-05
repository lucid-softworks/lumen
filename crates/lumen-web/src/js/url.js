// URL + URLSearchParams over the native parser. Setters recompose + reparse, so every
// mutation goes through the same validation as the constructor.

function formDecode(s) {
  const out = [];
  if (s.startsWith("?")) s = s.slice(1);
  if (s === "") return out;
  for (const part of s.split("&")) {
    if (part === "") continue;
    const eq = part.indexOf("=");
    const rawK = eq >= 0 ? part.slice(0, eq) : part;
    const rawV = eq >= 0 ? part.slice(eq + 1) : "";
    const dec = (x) => {
      x = x.replace(/\+/g, " ");
      try {
        return decodeURIComponent(x);
      } catch {
        return x; // stray %: keep verbatim rather than throw (form parsing never throws)
      }
    };
    out.push([dec(rawK), dec(rawV)]);
  }
  return out;
}

function formEncode(list) {
  const enc = (x) => encodeURIComponent(x).replace(/%20/g, "+");
  return list.map(([k, v]) => `${enc(k)}=${enc(v)}`).join("&");
}

class URLSearchParams {
  constructor(init = "") {
    this._list = [];
    this._onchange = null;
    if (typeof init === "string") {
      this._list = formDecode(init);
    } else if (init instanceof URLSearchParams) {
      this._list = init._list.map((p) => [...p]);
    } else if (Array.isArray(init)) {
      for (const pair of init) {
        if (!pair || pair.length !== 2) {
          throw new TypeError("URLSearchParams: each init pair needs exactly two items");
        }
        this._list.push([String(pair[0]), String(pair[1])]);
      }
    } else if (init && typeof init === "object") {
      for (const k of Object.keys(init)) this._list.push([k, String(init[k])]);
    }
  }
  _changed() {
    if (this._onchange) this._onchange();
  }
  _reset(search) {
    this._list = formDecode(search);
  }
  append(name, value) {
    this._list.push([String(name), String(value)]);
    this._changed();
  }
  delete(name) {
    name = String(name);
    this._list = this._list.filter(([k]) => k !== name);
    this._changed();
  }
  get(name) {
    name = String(name);
    const hit = this._list.find(([k]) => k === name);
    return hit ? hit[1] : null;
  }
  getAll(name) {
    name = String(name);
    return this._list.filter(([k]) => k === name).map(([, v]) => v);
  }
  has(name) {
    name = String(name);
    return this._list.some(([k]) => k === name);
  }
  set(name, value) {
    name = String(name);
    const i = this._list.findIndex(([k]) => k === name);
    if (i >= 0) {
      this._list[i][1] = String(value);
      this._list = this._list.filter(([k], j) => k !== name || j <= i);
    } else {
      this._list.push([name, String(value)]);
    }
    this._changed();
  }
  sort() {
    // Stable by key (Array.prototype.sort is stable in the engine).
    this._list.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
    this._changed();
  }
  forEach(fn, thisArg) {
    for (const [k, v] of [...this._list]) fn.call(thisArg, v, k, this);
  }
  *entries() {
    yield* this._list.map((p) => [...p]);
  }
  *keys() {
    for (const [k] of this._list) yield k;
  }
  *values() {
    for (const [, v] of this._list) yield v;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  get size() {
    return this._list.length;
  }
  toString() {
    return formEncode(this._list);
  }
}

class URL {
  constructor(input, base) {
    this._c = __url.parse(String(input), base === undefined ? undefined : String(base));
    this._searchParams = null;
  }
  _recompose(patch) {
    const c = { ...this._c, ...patch };
    let href = `${c.scheme}:`;
    if (c.host !== "" || c.scheme === "file") {
      href += "//";
      if (c.username !== "" || c.password !== "") {
        href += c.username;
        if (c.password !== "") href += `:${c.password}`;
        href += "@";
      }
      href += c.host;
      if (c.port !== "") href += `:${c.port}`;
    }
    href += c.path + c.query + c.fragment;
    this._c = __url.parse(href, undefined);
    if (this._searchParams) this._searchParams._reset(this._c.query);
  }
  get href() {
    return this._c.href;
  }
  set href(v) {
    this._c = __url.parse(String(v), undefined);
    if (this._searchParams) this._searchParams._reset(this._c.query);
  }
  get origin() {
    return this._c.origin;
  }
  get protocol() {
    return `${this._c.scheme}:`;
  }
  get username() {
    return this._c.username;
  }
  get password() {
    return this._c.password;
  }
  get host() {
    return this._c.port === "" ? this._c.host : `${this._c.host}:${this._c.port}`;
  }
  get hostname() {
    return this._c.host;
  }
  set hostname(v) {
    this._recompose({ host: String(v) });
  }
  get port() {
    return this._c.port;
  }
  set port(v) {
    this._recompose({ port: String(v) });
  }
  get pathname() {
    return this._c.path;
  }
  set pathname(v) {
    this._recompose({ path: String(v) });
  }
  get search() {
    return this._c.query;
  }
  set search(v) {
    v = String(v);
    this._recompose({ query: v === "" || v.startsWith("?") ? v : `?${v}` });
  }
  get hash() {
    return this._c.fragment;
  }
  set hash(v) {
    v = String(v);
    this._recompose({ fragment: v === "" || v.startsWith("#") ? v : `#${v}` });
  }
  get searchParams() {
    if (!this._searchParams) {
      const sp = new URLSearchParams(this._c.query);
      sp._onchange = () => {
        const q = sp.toString();
        // Bypass _recompose's param reset: the list is already current.
        const saved = this._searchParams;
        this._searchParams = null;
        this._recompose({ query: q === "" ? "" : `?${q}` });
        this._searchParams = saved;
      };
      this._searchParams = sp;
    }
    return this._searchParams;
  }
  toString() {
    return this.href;
  }
  toJSON() {
    return this.href;
  }
  static canParse(input, base) {
    try {
      new URL(input, base);
      return true;
    } catch {
      return false;
    }
  }
  static parse(input, base) {
    try {
      return new URL(input, base);
    } catch {
      return null;
    }
  }
}

globalThis.URLSearchParams = URLSearchParams;
globalThis.URL = URL;
