# Hono on lumen

A small [Hono](https://hono.dev) app run on the lumen runtime — over a real HTTP server
(`Lumen.serve`) or in-process through `app.fetch`.

Hono's core turns a `Request` into a `Response` via `app.fetch(request)`, the same
`(Request) -> Response` contract that `Lumen.serve` (and Deno/Bun/Workers) drive. lumen ships the
web platform (`Request`/`Response`/`Headers`/`URL`) and now a WinterCG-style HTTP server on top.

## Run

```sh
npm install
cargo build --release -p lumen-cli        # from the repo root
LUMEN=../../target/release/lumen-cli

$LUMEN serve-http.js        # real HTTP server on http://localhost:3000
$LUMEN ant-style.js         # the hono/logger example, faithfully
$LUMEN serve.js             # no socket: dispatch a few requests through app.fetch
```

Then, in another terminal:

```sh
curl localhost:3000/
curl localhost:3000/api/hello?name=lumen
curl -X POST localhost:3000/echo -H 'content-type: application/json' -d '{"hi":"there"}'
```

## Files

- `app.js` — the Hono app (routes, middleware, JSON, path params, custom 404).
- `serve-http.js` — serves `app.js` over HTTP with `Lumen.serve(app.fetch, { port })`.
- `ant-style.js` — the `import { logger } from 'hono/logger'` + `export default app` example,
  adapted to `Lumen.serve` and `Lumen.version` (see [cold start](#cold-start)).
- `serve.js` — drives `app.fetch` in-process (no listening socket); handy for testing routing.
- `bench.mjs` — in-process throughput bench (`app.fetch` in a loop; runtime-agnostic).
- `cold-start-server.js` + `cold-start.py` — cold-start benchmark (below).

## `Lumen.serve`

```js
// (request) => Response | Promise<Response>
const server = Lumen.serve(handler, { hostname: '127.0.0.1', port: 3000 });

// Also accepted: Lumen.serve(options, handler), Lumen.serve({ fetch, port }),
// and Lumen.serve(app) for any object with a .fetch() method (e.g. a Hono app).
server.shutdown();     // stop accepting; resolves `server.finished`
server.port;           // the bound port (useful with port: 0)
```

This isn't a WinterTC API — the Minimum Common API standardizes fetch/Request/Response/URL but
not a server — so it follows the cross-runtime `serve(handler)` convention. **v1 limitations**
(see `crates/lumen-web/src/server.rs`): one connection accepted at a time, `Connection: close`
(no keep-alive), buffered request/response bodies (no streaming), http only (no TLS), no HTTP/2.

## Cold start

`cold-start.py` measures wall time from spawning the runtime to its server answering the first
request — the metric [ant](https://github.com/theMackabu/ant#cold-start) reports. It runs the
same `cold-start-server.js` on any runtime (it picks `Lumen.serve`, `Bun.serve`, or a small
`node:http` adapter automatically):

```sh
./cold-start.py --runs 20 --warmup -- ../../target/release/lumen-cli cold-start-server.js
./cold-start.py --runs 20 --warmup -- bun  cold-start-server.js
./cold-start.py --runs 20 --warmup -- node cold-start-server.js
```

Representative medians on an Apple-silicon laptop (lower is better):

| runtime | cold start (median) |
|---|---|
| lumen | ~15.8 ms |
| bun   | ~16.3 ms |
| node  | ~53.9 ms |

Most of lumen's number is a fixed startup floor (process spawn + engine realm + extension JS
glue); Hono parsing and the first dispatch add well under 1 ms.
