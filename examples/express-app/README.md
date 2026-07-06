# Express on lumen

A real-world [Express 4](https://expressjs.com) app — CommonJS, with third-party middleware —
running **unmodified** on the lumen runtime.

Unlike the Hono example (which is web-standard `Request`/`Response`), Express is built on Node's
`http.createServer` and the `req`/`res` stream objects, and pulls in a deep dependency tree. lumen
runs it through its `node:http` compatibility layer, which bridges to the `Lumen.serve` HTTP
server — so `app.listen(3000)` binds a real socket and `curl localhost:3000` works like any Node
server.

## What it exercises

- **Express core** — routing, route params (`/users/:id`), query parsing, `res.json`/`res.send`,
  status codes, `ETag` generation, the 404 fall-through.
- **`morgan`** — request logging middleware, writing the `tiny` format (with real response times)
  to `process.stdout`.
- **`cors`** — sets `Access-Control-*` headers, including the `OPTIONS` preflight.
- **`express.json()`** — parses JSON request bodies (via `body-parser` → `raw-body` → `iconv-lite`).

Behind the scenes this drove a lot of `node:` compatibility: `http`, `events`, `stream`, `util`,
`crypto` (the `etag` hash), `querystring`, `url`, `net`, `assert`, `string_decoder`, plus
`Buffer`, `process.stdout`/`hrtime`, and `Error.captureStackTrace`. See
`crates/lumen-node/src/js/` for the implementations.

## Run

```sh
npm install
cargo build --release -p lumen-cli        # from the repo root

../../target/release/lumen-cli server.js  # http://localhost:3000
```

Then, in another terminal:

```sh
curl localhost:3000/
curl localhost:3000/users/42
curl "localhost:3000/search?q=lumen&page=2"
curl -X POST localhost:3000/echo -H 'content-type: application/json' -d '{"hi":"there"}'
curl -i localhost:3000/teapot          # 418, custom header
curl -i -X OPTIONS localhost:3000/ -H 'Origin: http://example.com' \
     -H 'Access-Control-Request-Method: POST'   # CORS preflight
```

- `app.js` — the Express app (routes, middleware).
- `server.js` — `app.listen(port)`.

## Known limits

The `node:` layer targets the common server path, not 100% of Node. Notably: no `zlib`, so a
gzip/deflate-encoded request or response body is unsupported (identity encoding works); the
`node:http` **client** (`http.request`) isn't implemented (use the global `fetch`); and there's no
TLS, so `https` can't terminate. See the checklists at the top of each `crates/lumen-node/src/js/`
module for specifics.
