// Bun.Cookie and Bun.CookieMap are pure header parsing/serialization APIs. Keep them independent
// from bun.js and from the HTTP transport so they can also be used with fetch-style handlers.
{
  const decode = value => { try { return decodeURIComponent(value); } catch (_) { return value; } };
  const sameSiteValue = value => {
    if (value === true) return "strict";
    if (value === false || value == null) return undefined;
    const normalized = String(value).toLowerCase();
    return ["strict", "lax", "none"].includes(normalized) ? normalized : undefined;
  };

  class Cookie {
    constructor(input, value, options) {
      if (arguments.length === 0) throw new TypeError("Not enough arguments");
      let init;
      if (input && typeof input === "object") init = input;
      else if (arguments.length > 1) init = { ...(options || {}), name: input, value };
      else init = Cookie._parse(String(input));
      if (init.name == null || init.value == null) throw new TypeError("Cookie name and value are required");
      this.name = String(init.name);
      this.value = String(init.value);
      this.domain = init.domain == null ? undefined : String(init.domain).toLowerCase();
      this.path = init.path == null ? "/" : String(init.path);
      this.expires = init.expires == null ? undefined : new Date(init.expires);
      this.maxAge = init.maxAge == null ? undefined : Number(init.maxAge);
      this.secure = !!init.secure;
      this.httpOnly = !!init.httpOnly;
      this.sameSite = sameSiteValue(init.sameSite === undefined ? "lax" : init.sameSite);
      this.partitioned = !!init.partitioned;
    }

    static _parse(source) {
      const parts = source.split(";");
      const first = parts.shift().trim();
      const equals = first.indexOf("=");
      if (equals <= 0) throw new TypeError("Invalid cookie string");
      const result = { name: first.slice(0, equals).trim(), value: first.slice(equals + 1).trim() };
      for (const raw of parts) {
        const part = raw.trim();
        const i = part.indexOf("=");
        const key = (i < 0 ? part : part.slice(0, i)).toLowerCase();
        const value = i < 0 ? true : part.slice(i + 1).trim();
        if (key === "domain") result.domain = value;
        else if (key === "path") result.path = value;
        else if (key === "expires") result.expires = value;
        else if (key === "max-age") result.maxAge = Number(value);
        else if (key === "secure") result.secure = true;
        else if (key === "httponly") result.httpOnly = true;
        else if (key === "samesite") result.sameSite = value;
        else if (key === "partitioned") result.partitioned = true;
      }
      return result;
    }

    isExpired() {
      if (this.maxAge !== undefined && this.maxAge <= 0) return true;
      return this.expires !== undefined && this.expires.getTime() <= Date.now();
    }

    serialize() { return this.toString(); }
    toString() {
      let result = `${this.name}=${encodeURIComponent(this.value)}`;
      if (this.domain) result += `; Domain=${this.domain}`;
      if (this.path) result += `; Path=${this.path}`;
      if (this.expires && !Number.isNaN(this.expires.getTime())) result += `; Expires=${this.expires.toUTCString()}`;
      if (this.maxAge !== undefined) result += `; Max-Age=${Math.trunc(this.maxAge)}`;
      if (this.secure) result += "; Secure";
      if (this.httpOnly) result += "; HttpOnly";
      if (this.sameSite) result += `; SameSite=${this.sameSite[0].toUpperCase()}${this.sameSite.slice(1)}`;
      if (this.partitioned) result += "; Partitioned";
      return result;
    }

    toJSON() {
      const result = { name: this.name, value: this.value };
      if (this.domain !== undefined) result.domain = this.domain;
      if (this.path !== undefined) result.path = this.path;
      if (this.expires !== undefined) result.expires = this.expires;
      if (this.maxAge !== undefined) result.maxAge = this.maxAge;
      result.secure = this.secure;
      if (this.sameSite !== undefined) result.sameSite = this.sameSite;
      result.httpOnly = this.httpOnly;
      result.partitioned = this.partitioned;
      return result;
    }
  }
  Object.defineProperty(Cookie.prototype, Symbol.toStringTag, { value: "Cookie" });

  class CookieMap {
    constructor(input) {
      this._values = new Map();
      this._outgoing = new Map();
      if (input == null) return;
      if (typeof input === "string") {
        for (const raw of input.split(";")) {
          const i = raw.indexOf("=");
          if (i < 0) continue;
          this._values.set(raw.slice(0, i).trim(), decode(raw.slice(i + 1).trim()));
        }
      } else if (typeof input[Symbol.iterator] === "function") {
        for (const [name, value] of input) this._values.set(String(name), String(value));
      } else {
        for (const name of Object.keys(input)) this._values.set(name, String(input[name]));
      }
    }
    get size() { return this._values.size; }
    get(name) { return this._values.get(String(name)); }
    has(name) { return this._values.has(String(name)); }
    set(name, value, options) {
      const cookie = name && typeof name === "object" ? new Cookie(name) : new Cookie(name, value, options);
      this._values.set(cookie.name, cookie.value);
      this._outgoing.set(cookie.name, cookie);
    }
    delete(name) {
      name = String(name);
      this._values.delete(name);
      this._outgoing.delete(name);
    }
    entries() { return this._values.entries(); }
    keys() { return this._values.keys(); }
    values() { return this._values.values(); }
    [Symbol.iterator]() { return this.entries(); }
    forEach(callback, thisArg) { this._values.forEach((value, key) => callback.call(thisArg, value, key, this)); }
    toSetCookieHeaders() { return [...this._outgoing.values()].map(cookie => cookie.toString()); }
    toJSON() { return Object.fromEntries(this._values); }
  }
  Object.defineProperty(CookieMap.prototype, Symbol.toStringTag, { value: "CookieMap" });

  Object.defineProperty(globalThis, "__lumenBunCookies", {
    value: { Cookie, CookieMap }, configurable: true,
  });
}
