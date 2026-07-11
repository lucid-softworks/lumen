// Lumen.serve — an HTTP server over the native __http_server ops (see server.rs).
//
// WinterCG note: the Minimum Common API standardizes fetch/Request/Response/URL/AbortSignal but
// NOT a server. This follows the cross-runtime `serve(handler)` convention shared by Deno.serve,
// Bun.serve, and Cloudflare Workers: the handler is `(request, info) => Response | Promise<Response>`,
// taking a standard `Request` and returning a standard `Response`. Compared with those runtimes,
// this v1 does not do keep-alive (every connection is Connection: close), request/response body
// streaming (bodies are buffered), HTTP/2, or TLS/https. See server.rs for the wire + concurrency
// limits.
//
// Forms:
//   Lumen.serve(handler)
//   Lumen.serve(handler, { port, hostname, onListen, onError, signal })
//   Lumen.serve({ port, hostname, onListen, onError, signal }, handler)
//   Lumen.serve({ fetch, port, hostname, onListen, onError, signal })   // Deno object form
//   Lumen.serve(app)   // any object with a .fetch(request) method, e.g. a Hono app
//
// The last form reads only port/hostname/onListen/signal off the object; onError is deliberately
// NOT read from it, so a framework method (e.g. Hono's app.onError) can't be mistaken for a serve
// option. To set onError with an app, pass it explicitly: Lumen.serve(app.fetch, { onError }).

const __servers = new Map(); // serverId -> { handler, onError, resolveFinished }

// Connection bookkeeping for WebSocket upgrades: each Request delivered by __dispatch carries its
// connection id under this symbol so Lumen.upgradeWebSocket(request) can hand the socket over.
const __conn = Symbol("lumen.serve.connection");

// The single native accept callback for every server. `connId < 0` signals the listener stopped.
async function __dispatch(serverId, connId, method, url, headerPairs, bodyBytes, remoteHost, remotePort) {
  const entry = __servers.get(serverId);
  if (connId < 0) {
    if (entry) {
      __servers.delete(serverId);
      entry.resolveFinished();
    }
    return;
  }
  if (!entry) return; // server was closed but a connection slipped through — drop it

  let response;
  let conn;
  try {
    const init = { method, headers: headerPairs };
    // The Request constructor rejects a body on GET/HEAD; only attach one otherwise.
    if (method !== "GET" && method !== "HEAD" && bodyBytes !== undefined) init.body = bodyBytes;
    const request = new Request(url, init);
    conn = { connId, upgraded: false, remoteAddress: remoteHost };
    request[__conn] = conn;
    const info = { remoteAddr: { transport: "tcp", hostname: remoteHost, port: remotePort } };
    response = await entry.handler(request, info);
    // An upgraded connection's socket now belongs to the WebSocket machinery; there is nothing
    // to respond with (the 101 already went out in the upgrade op).
    if (conn.upgraded) return;
    if (!(response instanceof Response)) {
      throw new TypeError("serve handler did not return a Response");
    }
  } catch (err) {
    if (conn && conn.upgraded) return; // handler threw after handing the socket over
    if (entry.onError) {
      try {
        response = await entry.onError(err);
      } catch {
        response = new Response("Internal Server Error", { status: 500 });
      }
    } else {
      console.error(err);
      response = new Response("Internal Server Error", { status: 500 });
    }
  }

  let bodyOut;
  try {
    bodyOut = await response.bytes();
  } catch {
    bodyOut = new Uint8Array(0); // body already consumed, etc.
  }
  const headerPairsOut = response.headers._pairs();
  // Resolve/reject settle when the socket write finishes; a write failure means the client hung
  // up, which is not actionable here.
  await new Promise((resolve, reject) => {
    __http_server.respond(connId, response.status, response.statusText, headerPairsOut, bodyOut, resolve, reject);
  }).catch(() => {});
}

// Lumen.upgradeWebSocket(request[, { protocol, headers }]) — hand a Lumen.serve connection over
// to the WebSocket machinery (RFC 6455 server handshake; see op_ws_upgrade in websocket.rs).
//
// Returns `null` when the request is not a WebSocket upgrade (no `Upgrade: websocket` /
// `Sec-WebSocket-Key`), else a low-level handle:
//   { send(data) -> bool, close(code?, reason?), remoteAddress,
//     onmessage: (data, isBinary) => {},        // assign; data is string | Uint8Array
//     onclose: (code, reason, wasClean) => {} } // fires once, including on socket errors
// After a successful upgrade the serve handler's return value is ignored (the 101 already went
// out); the request cannot be responded to or upgraded twice.
function upgradeWebSocket(request, options = {}) {
  const conn = request ? request[__conn] : undefined;
  if (!conn) {
    throw new TypeError("upgradeWebSocket: the request was not delivered by Lumen.serve");
  }
  if (conn.upgraded) throw new Error("upgradeWebSocket: connection already upgraded");
  const upgrade = request.headers.get("upgrade");
  const key = request.headers.get("sec-websocket-key");
  if (!upgrade || upgrade.toLowerCase() !== "websocket" || !key) return null;

  // Extra response headers: Headers | plain object | [name, value] pairs.
  const pairs = [];
  const extra = options.headers;
  if (extra) {
    if (typeof extra._pairs === "function") pairs.push(...extra._pairs());
    else if (Array.isArray(extra)) for (const [n, v] of extra) pairs.push([String(n), String(v)]);
    else for (const n of Object.keys(extra)) pairs.push([n, String(extra[n])]);
  }

  const handle = {
    remoteAddress: conn.remoteAddress,
    onmessage: null,
    onclose: null,
  };
  let closed = false;
  const fireClose = (code, reason, wasClean) => {
    if (closed) return;
    closed = true;
    if (typeof handle.onclose === "function") handle.onclose(code, reason, wasClean);
  };
  const id = __ws.upgrade(conn.connId, key, String(options.protocol || ""), pairs, (kind, a, b) => {
    if (kind === "text" || kind === "binary") {
      if (typeof handle.onmessage === "function") handle.onmessage(a, kind === "binary");
    } else if (kind === "close") {
      fireClose(a, b, true);
    } else if (kind === "fail") {
      fireClose(a, String(b), false);
    } else if (kind === "io") {
      fireClose(1006, String(a), false);
    }
  });
  conn.upgraded = true;
  handle.send = (data) => (closed ? false : __ws.send(id, data));
  handle.close = (code, reason) => {
    __ws.close(id, code === undefined ? 1000 : code, reason === undefined ? "" : reason);
  };
  return handle;
}

function serve(a, b) {
  let handler;
  let options;
  let handlerFromObject = false;
  if (typeof a === "function") {
    handler = a;
    options = b && typeof b === "object" ? b : {};
  } else if (a && typeof a === "object" && typeof b === "function") {
    handler = b;
    options = a;
  } else if (a && typeof a === "object" && typeof a.fetch === "function") {
    handler = a.fetch.bind(a);
    options = a;
    handlerFromObject = true;
  } else {
    throw new TypeError("Lumen.serve requires a handler function or an object with a fetch() method");
  }

  const hostname = typeof options.hostname === "string" ? options.hostname : "0.0.0.0";
  const port = typeof options.port === "number" ? options.port : 8000;
  const onListen = typeof options.onListen === "function" ? options.onListen : null;
  // See the header comment: skip onError when the handler was pulled off the object itself.
  const onError = !handlerFromObject && typeof options.onError === "function" ? options.onError : null;

  const [serverId, boundPort] = __http_server.listen(hostname, port, __dispatch);

  let resolveFinished;
  const finished = new Promise((resolve) => {
    resolveFinished = resolve;
  });
  __servers.set(serverId, { handler, onError, resolveFinished });

  const shutdown = () => {
    __http_server.close(serverId);
    return finished;
  };

  // WinterCG AbortSignal: aborting the signal shuts the server down.
  const signal = options.signal;
  if (signal && typeof signal.addEventListener === "function") {
    if (signal.aborted) shutdown();
    else signal.addEventListener("abort", shutdown, { once: true });
  }

  if (onListen) onListen({ hostname, port: boundPort });

  return {
    hostname,
    port: boundPort,
    finished,
    shutdown,
    ref() {},
    unref() {},
  };
}

if (typeof globalThis.Lumen === "undefined") globalThis.Lumen = {};
Object.defineProperty(globalThis.Lumen, "serve", {
  value: serve,
  writable: true,
  configurable: true,
  enumerable: true,
});
Object.defineProperty(globalThis.Lumen, "upgradeWebSocket", {
  value: upgradeWebSocket,
  writable: true,
  configurable: true,
  enumerable: true,
});
Object.defineProperty(globalThis.Lumen, "version", {
  value: __http_server.version(),
  writable: false,
  configurable: true,
  enumerable: true,
});
