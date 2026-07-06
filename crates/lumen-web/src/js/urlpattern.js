// URLPattern — matches URLs against a pattern with named/wildcard/regex groups. Each component
// pattern (protocol, hostname, pathname, …) compiles to a RegExp built on lumen's own regex engine.
// Supported syntax: `:name` named groups, `*` wildcard, `(regex)` custom groups, `{…}` groups with
// an optional `?`/`+`/`*` modifier, and literal text. (The full spec's grouping/modifier algebra is
// larger; this covers the routing patterns real code uses.)

const URLPATTERN_COMPONENTS = [
  "protocol",
  "username",
  "password",
  "hostname",
  "port",
  "pathname",
  "search",
  "hash",
];

function escapeRegexLiteral(ch) {
  return /[.*+?^${}()|[\]\\]/.test(ch) ? "\\" + ch : ch;
}

// Compile one component pattern to { regex, names }. `sep` is the segment separator for bare named
// groups (`/` for pathname → `[^/]+?`, none elsewhere → `[^...]` becomes `.+?` style via `.`).
function compileComponent(pattern, segmentSep) {
  if (pattern === undefined || pattern === "*") {
    return { regex: /^.*$/su, names: pattern === "*" ? ["0"] : [] };
  }
  const names = [];
  let out = "^";
  let group = 0;
  const bareGroup = segmentSep ? `[^${segmentSep}]+?` : "[^]+?";
  let i = 0;
  const readParen = (start) => {
    let depth = 1;
    let k = start + 1;
    let body = "";
    while (k < pattern.length && depth > 0) {
      const ch = pattern[k];
      if (ch === "(") depth++;
      else if (ch === ")") {
        depth--;
        if (depth === 0) break;
      }
      body += ch;
      k++;
    }
    return { body, end: k + 1 };
  };
  while (i < pattern.length) {
    const c = pattern[i];
    if (c === ":") {
      let j = i + 1;
      let name = "";
      while (j < pattern.length && /[A-Za-z0-9_]/.test(pattern[j])) name += pattern[j++];
      if (pattern[j] === "(") {
        const { body, end } = readParen(j);
        names.push(name);
        out += `(${body})`;
        i = end;
      } else {
        names.push(name);
        out += `(${bareGroup})`;
        i = j;
      }
    } else if (c === "*") {
      names.push(String(group++));
      out += "(.*)";
      i++;
    } else if (c === "(") {
      const { body, end } = readParen(i);
      names.push(String(group++));
      out += `(${body})`;
      i = end;
    } else if (c === "{") {
      // {…} group: literal/pattern segment, optionally suffixed with ? + or *.
      let depth = 1;
      let k = i + 1;
      let inner = "";
      while (k < pattern.length && depth > 0) {
        if (pattern[k] === "{") depth++;
        else if (pattern[k] === "}") {
          depth--;
          if (depth === 0) break;
        }
        inner += pattern[k++];
      }
      const modifier = "?+*".includes(pattern[k + 1]) ? pattern[k + 1] : "";
      const compiledInner = compileComponent(inner, segmentSep);
      for (const n of compiledInner.names) names.push(n);
      const innerBody = compiledInner.regex.source.replace(/^\^/, "").replace(/\$$/, "");
      out += modifier ? `(?:${innerBody})${modifier}` : `(?:${innerBody})`;
      i = k + 1 + (modifier ? 1 : 0);
    } else {
      out += escapeRegexLiteral(c);
      i++;
    }
  }
  out += "$";
  return { regex: new RegExp(out, "su"), names };
}

// Turn an input (string or object) into per-component pattern strings.
function componentsFromInput(input, baseURL) {
  if (input && typeof input === "object") {
    const out = {};
    for (const key of URLPATTERN_COMPONENTS) if (input[key] !== undefined) out[key] = String(input[key]);
    return out;
  }
  // String form: resolve against baseURL when relative, then split a URL-ish shape. Named groups
  // (`:id`) don't collide with the structural `:` here because we only split on `://`, `/`, `?`, `#`.
  let str = String(input ?? "*");
  const out = {};
  const hashIdx = str.indexOf("#");
  if (hashIdx >= 0) {
    out.hash = str.slice(hashIdx + 1);
    str = str.slice(0, hashIdx);
  }
  const searchIdx = str.indexOf("?");
  if (searchIdx >= 0) {
    out.search = str.slice(searchIdx + 1);
    str = str.slice(0, searchIdx);
  }
  const protoMatch = /^([^:/?#]+):\/\//.exec(str);
  if (protoMatch) {
    out.protocol = protoMatch[1];
    str = str.slice(protoMatch[0].length);
    const slash = str.indexOf("/");
    const authority = slash >= 0 ? str.slice(0, slash) : str;
    str = slash >= 0 ? str.slice(slash) : "";
    const at = authority.lastIndexOf("@");
    let hostport = authority;
    if (at >= 0) {
      const cred = authority.slice(0, at);
      const colon = cred.indexOf(":");
      out.username = colon >= 0 ? cred.slice(0, colon) : cred;
      if (colon >= 0) out.password = cred.slice(colon + 1);
      hostport = authority.slice(at + 1);
    }
    const pColon = hostport.lastIndexOf(":");
    if (pColon >= 0 && !hostport.slice(pColon + 1).includes("(")) {
      out.hostname = hostport.slice(0, pColon);
      out.port = hostport.slice(pColon + 1);
    } else {
      out.hostname = hostport;
    }
  } else if (baseURL) {
    const base = new URL(baseURL);
    out.protocol = base.protocol.replace(/:$/, "");
    out.hostname = base.hostname;
  }
  out.pathname = str || (protoMatch ? "/" : "*");
  return out;
}

class URLPattern {
  constructor(input = {}, baseURL, options) {
    if (typeof baseURL === "object" && options === undefined) {
      options = baseURL;
      baseURL = undefined;
    }
    const comps = componentsFromInput(input, baseURL);
    this._parts = {};
    for (const key of URLPATTERN_COMPONENTS) {
      const sep = key === "pathname" ? "/" : key === "hostname" ? "." : "";
      this._parts[key] = compileComponent(comps[key], sep);
      // The public `.<component>` pattern strings the spec exposes.
      this[key] = comps[key] ?? "*";
    }
  }

  _match(input, baseURL) {
    let url;
    try {
      if (typeof input === "object" && input !== null) {
        // A URLPatternInit-like object: match each provided component directly.
        const groups = {};
        for (const key of URLPATTERN_COMPONENTS) {
          const value = input[key] !== undefined ? String(input[key]) : "";
          const m = this._parts[key].regex.exec(value);
          if (!m) return null;
          collectGroups(this._parts[key].names, m, groups, key);
        }
        return { inputs: [input], ...perComponent(this, input, {}) };
      }
      url = new URL(input, baseURL);
    } catch {
      return null;
    }
    const values = {
      protocol: url.protocol.replace(/:$/, ""),
      username: url.username,
      password: url.password,
      hostname: url.hostname,
      port: url.port,
      pathname: url.pathname,
      search: url.search.replace(/^\?/, ""),
      hash: url.hash.replace(/^#/, ""),
    };
    const result = { inputs: [input] };
    for (const key of URLPATTERN_COMPONENTS) {
      const m = this._parts[key].regex.exec(values[key]);
      if (!m) return null;
      const groups = {};
      collectGroups(this._parts[key].names, m, groups, key);
      result[key] = { input: values[key], groups };
    }
    return result;
  }

  test(input, baseURL) {
    return this._match(input, baseURL) !== null;
  }
  exec(input, baseURL) {
    return this._match(input, baseURL);
  }
}

function collectGroups(names, match, groups, _component) {
  for (let g = 0; g < names.length; g++) {
    groups[names[g]] = match[g + 1] !== undefined ? match[g + 1] : "";
  }
}

function perComponent(pattern, input, _extra) {
  const result = {};
  for (const key of URLPATTERN_COMPONENTS) {
    const value = input[key] !== undefined ? String(input[key]) : "";
    const m = pattern._parts[key].regex.exec(value);
    const groups = {};
    if (m) collectGroups(pattern._parts[key].names, m, groups, key);
    result[key] = { input: value, groups };
  }
  return result;
}

globalThis.URLPattern = URLPattern;
