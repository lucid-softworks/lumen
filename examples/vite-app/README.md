# Vite build on lumen

A real [Vite](https://vitejs.dev) production build (`vite build`) running on lumen — bundling with
Rollup's **native N-API addon**, transforming with **esbuild** (a spawned service child), and
writing a hashed, minified bundle.

```sh
npm install
../../target/release/lumen-cli node_modules/vite/bin/vite.js build
```

Expected output:

```
vite vX building for production...
✓ 3 modules transformed.
dist/index.html                0.14 kB │ gzip: 0.14 kB
dist/assets/index-*.js         0.77 kB │ gzip: 0.53 kB
✓ built in ~190ms
```

The process builds, writes `dist/`, and exits cleanly.

## What this exercises

This is one of the most demanding npm workloads, and it drives a lot of lumen at once:

- **Native addons** — `@rollup/rollup-<platform>.node` is `dlopen`ed and run through lumen's
  from-scratch N-API implementation (`crates/lumen-node/src/napi.rs`).
- **child_process + esbuild** — esbuild runs as a long-lived service subprocess; lumen's
  `child.ref()`/`unref()` let the build wait for transforms yet still exit when done.
- **ESM ↔ CommonJS interop** — Vite and its deps mix ESM and CJS; lumen statically discovers a
  CJS module's named exports so `import { parse } from './native.js'` links.
- **The node: surface** — `fs` (real stat metadata), `path`, `url`, `crypto`, `zlib` (real gzip
  size reporting), `dns`, `v8`, `worker_threads`, `process`, and more.
