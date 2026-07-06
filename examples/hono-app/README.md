# Hono on lumen

A small [Hono](https://hono.dev) app run on the lumen runtime.

Hono's core is just a router that turns a `Request` into a `Response` via
`app.fetch(request)`, so it runs without a listening socket — a good fit for
lumen today, which ships the web platform (`Request`/`Response`/`Headers`/`URL`)
but not yet an HTTP *server*. `serve.js` drives the app by dispatching a handful
of requests through `app.fetch` and printing the responses.

## Run

```sh
npm install
cargo build --release -p lumen-cli        # from the repo root

# Node (baseline):
node serve.js

# lumen:
../../target/release/lumen-cli serve.js
```

- `app.js` — the Hono app (routes, middleware, JSON, path params, 404).
- `serve.js` — dispatches requests through `app.fetch` and prints results.
