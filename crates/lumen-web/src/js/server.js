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
  try {
    const init = { method, headers: headerPairs };
    // The Request constructor rejects a body on GET/HEAD; only attach one otherwise.
    if (method !== "GET" && method !== "HEAD" && bodyBytes !== undefined) init.body = bodyBytes;
    const request = new Request(url, init);
    const info = { remoteAddr: { transport: "tcp", hostname: remoteHost, port: remotePort } };
    response = await entry.handler(request, info);
    if (!(response instanceof Response)) {
      throw new TypeError("serve handler did not return a Response");
    }
  } catch (err) {
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
Object.defineProperty(globalThis.Lumen, "version", {
  value: __http_server.version(),
  writable: false,
  configurable: true,
  enumerable: true,
});
