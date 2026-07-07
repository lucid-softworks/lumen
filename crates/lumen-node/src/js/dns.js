// node:dns — real name resolution over the `__dns` ops (system resolver for lookup/A/AAAA,
// DNS-over-UDP for the record-type queries). No stubs: an unresolvable name yields the ENOTFOUND
// error Node code branches on.

{
  function normalizeLookupOptions(options) {
    if (typeof options === "number") return { family: options };
    return options || {};
  }

  function lookup(hostname, options, callback) {
    if (typeof options === "function") {
      callback = options;
      options = {};
    }
    options = normalizeLookupOptions(options);
    const family = options.family === 6 ? 6 : options.family === 4 ? 4 : 0;
    __dns.lookup(
      String(hostname),
      family,
      (list) => {
        if (options.all) return callback(null, list);
        const first = list[0];
        callback(null, first.address, first.family);
      },
      (err) => callback(err),
    );
  }

  const makeResolver = (rrtype) => (hostname, callback) =>
    __dns.resolve(String(hostname), rrtype, (records) => callback(null, records), (err) => callback(err));

  const resolve4 = makeResolver("A");
  const resolve6 = makeResolver("AAAA");
  const resolveCname = makeResolver("CNAME");
  const resolveNs = makeResolver("NS");
  const resolveMx = makeResolver("MX");
  const resolveTxt = makeResolver("TXT");

  function resolve(hostname, rrtype, callback) {
    if (typeof rrtype === "function") {
      callback = rrtype;
      rrtype = "A";
    }
    makeResolver(rrtype)(hostname, callback);
  }

  function reverse(ip, callback) {
    let name;
    if (ip.includes(":")) {
      // IPv6 → nibble-reversed ip6.arpa.
      const full = expandIPv6(ip);
      name = full.split("").reverse().join(".") + ".ip6.arpa";
    } else {
      name = ip.split(".").reverse().join(".") + ".in-addr.arpa";
    }
    __dns.resolve(name, "PTR", (records) => callback(null, records), (err) => callback(err));
  }

  // Expand an IPv6 address to its 32 hex nibbles (no colons), for the reverse-PTR name.
  function expandIPv6(ip) {
    const [head, tail = ""] = ip.split("::");
    const h = head ? head.split(":") : [];
    const t = tail ? tail.split(":") : [];
    const missing = 8 - h.length - t.length;
    const groups = [...h, ...Array(missing).fill("0"), ...t];
    return groups.map((g) => g.padStart(4, "0")).join("");
  }

  let resultOrder = "verbatim";
  const setDefaultResultOrder = (order) => {
    resultOrder = order;
  };
  const getDefaultResultOrder = () => resultOrder;

  const promisify = (fn) => (...args) =>
    new Promise((res, rej) =>
      fn(...args, (err, ...vals) => (err ? rej(err) : res(vals.length > 1 ? vals : vals[0]))));

  const lookupPromise = (hostname, options) => {
    const opts = normalizeLookupOptions(options);
    return new Promise((res, rej) =>
      lookup(hostname, opts, (err, address, family) =>
        err ? rej(err) : res(opts.all ? address : { address, family })));
  };

  const dns = {
    lookup,
    resolve,
    resolve4,
    resolve6,
    resolveCname,
    resolveNs,
    resolveMx,
    resolveTxt,
    reverse,
    setDefaultResultOrder,
    getDefaultResultOrder,
    ADDRCONFIG: 1024,
    V4MAPPED: 8,
  };
  dns.promises = {
    lookup: lookupPromise,
    resolve: promisify(resolve),
    resolve4: promisify(resolve4),
    resolve6: promisify(resolve6),
    resolveCname: promisify(resolveCname),
    resolveNs: promisify(resolveNs),
    resolveMx: promisify(resolveMx),
    resolveTxt: promisify(resolveTxt),
    reverse: promisify(reverse),
    setDefaultResultOrder,
    getDefaultResultOrder,
  };

  __builtins.set("dns", dns);
  __builtins.set("dns/promises", dns.promises);
}
