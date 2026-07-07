# lumen

A from-scratch JavaScript **engine** in Rust — std only, zero dependencies — and a
**runtime** being built on top of it, the way Node/Deno/Bun wrap a JS engine with an event
loop and host APIs. Every crate in the workspace is std-only: no `tokio`, `mio`, `libc`,
`rustyline`, `serde`, or any other third-party dependency, anywhere.

## The engine (`crates/lumen`)

A lexer, parser, and tree-walking interpreter with generators and `async`/`await` running on
stackful coroutines, full `RegExp` (including `\p{…}` and inline modifiers), typed arrays,
`Proxy`/`Reflect`, ES modules (top-level await, `import defer`, source phase), `Intl`, and
`Temporal`. An opt-in bytecode execution tier is under active development alongside the
reference tree-walker.

**Passes 100% of [tc39/test262](https://github.com/tc39/test262): 53,400/53,400** (including
annexB, intl402, and staging).

Extracted from — and used by — the [lucid-softworks/browser](https://github.com/lucid-softworks/browser)
engine as its JS backend (`backend-lumen`), with full git history.

## The runtime

A curated `embed` API on the engine exposes just enough — native-function registration, a
typed host-state slot, and event-loop hooks — for a runtime layer to be assembled from
independent op crates, without leaking the interpreter's internals into the published API.
On top of that:

- **Event loop** (`lumen-runtime`) — a single loop thread owns the (`!Send`) engine; blocking
  work runs on a std thread pool and completes back over `mpsc`. No epoll/kqueue reactor
  (that would need raw syscalls); the thread-pool-plus-completion model is libuv's own fs
  strategy. Each turn drains microtasks, queued callbacks, due timers, and I/O completions,
  then blocks until the next event.
- **Timers** (`lumen-timers`) — `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/
  `setImmediate`, plus `queueMicrotask`.
- **`console` and `process`** — streaming `console.*`; `process.argv`/`env`/`platform`/
  `cwd()`/`exit()`/`nextTick()`.
- **Filesystem** (`lumen-fs`) — synchronous ops (`readFileSync`, `writeFileSync`,
  `existsSync`, `mkdirSync`, `readdirSync`, …), file handles via a resource table
  (`openSync`/`readSync`/`writeSync`/`closeSync`), and async `fs.promises.readFile`/
  `writeFile` on the thread pool.
- **Web platform** (`lumen-web`) — a growing slice of the WinterTC Minimum Common API:
  `Event`/`EventTarget`/`CustomEvent`/`AbortController`/`AbortSignal`/`DOMException`,
  `TextEncoder`/`TextDecoder`, `atob`/`btoa`, `structuredClone`, `URL`/`URLSearchParams`,
  `performance.now()`, `crypto.getRandomValues`/`randomUUID`/`subtle.digest` (SHA-256), and
  `fetch`/`Headers`/`Request`/`Response`. See the checklist at the top of
  `crates/lumen-web/src/lib.rs` for what's implemented vs. deferred (streams, `Blob`/
  `FormData`, `URLPattern`, …).

  `fetch` speaks HTTP/1.1 over `std::net`. **`https:` is not supported**: TLS cannot be
  implemented on std alone and no third-party crate is permitted, so `https` URLs reject with
  a clear error; plain `http` works.

  `Lumen.serve((request) => Response)` is the matching HTTP/1.1 **server** — not a WinterTC API,
  but the cross-runtime `serve(handler)` convention (Deno/Bun/Workers), so a Hono app runs with
  `Lumen.serve(app.fetch)`. v1 is single-accept, `Connection: close`, buffered bodies, http only
  (see `crates/lumen-web/src/server.rs`). Cold-start and usage: `examples/hono-app`.

- **Modules — both CommonJS and ESM.** `lumen-cli` picks the module kind the way Node does:
  `.mjs` is ESM, `.cjs` is CommonJS, `.js` follows the nearest `package.json` `"type"`. ES
  modules run through the engine's real module graph (linking, top-level `await`); `import`
  specifiers resolve against disk and `node_modules`, `node:` builtins are importable
  (named imports included), and CommonJS packages interop by default export. CommonJS files
  run as the program entry with `require.main === module`.

- **`node:` compatibility** (`lumen-node`) — a CommonJS `require` with `node_modules`
  resolution and the module wrapper, `package.json` `main`/`exports`, the `node:path`/
  `node:os`/`node:fs` builtins, and `Buffer`, so packages written against the `node:` surface
  run. See the checklist at the top of `crates/lumen-node/src/lib.rs` for the deferred pieces
  (subpath-pattern exports, the full N-API surface).

  **Native addons** load too: `require('./addon.node')` dlopens the compiled library and runs its
  N-API registration, resolving the addon's `napi_*` symbols against the lumen executable — the
  same mechanism the `node` binary uses. The N-API surface is implemented from scratch (values,
  properties, functions, callbacks, errors, references, object wrap, classes, promises, buffers,
  typed arrays, async work); the loader reaches `dlopen`/`dlsym` through raw `extern "C"`
  declarations, so no third-party crate is added. See `examples/native-addon`.

  **`vite build` runs on lumen** (`examples/vite-app`): a full Vite production build, bundling
  through Rollup's native N-API addon, transforming with esbuild's service subprocess, over
  ESM↔CommonJS interop and the `node:` surface — building `dist/` and exiting cleanly.

- **REPL + CLI** (`lumen-repl`, `lumen-cli`) — an interactive shell with a persistent realm,
  parser-driven incomplete-input detection (multi-line continuation), top-level `await`, and
  loop-to-quiescence so timers and awaited promises settle before the next prompt. Line
  editing is line-buffered (raw-mode/history would need `termios`); use `rlwrap` for arrows
  and history.

### Workspace crates

```
lumen          engine (std-only, zero-dep; `embed` feature gates the runtime API)
lumen-host     substrate: OpState, ResourceTable, Extension, the thread-pool/callback primitives
lumen-timers   setTimeout/setInterval/queueMicrotask/setImmediate
lumen-fs       filesystem (sync + async)
lumen-web      WinterTC Minimum Common API (Event, URL, crypto, fetch, …)
lumen-node     node: compatibility (require, node:path/os/fs, Buffer)
lumen-runtime  the event loop; assembles the op crates; console + process
lumen-repl     interactive shell
lumen-cli      node/deno-style entrypoint
```

The dependency graph is a strict DAG — `lumen ← lumen-host ← {op crates} ← lumen-runtime ←
lumen-repl ← lumen-cli` — so each op crate can be worked on in isolation.

## Install

Grab a prebuilt runtime for your platform (macOS arm64, Linux x86_64/arm64):

```sh
curl -fsSL https://raw.githubusercontent.com/lucid-softworks/lumen/main/scripts/install.sh | bash
```

It installs the `lumen` CLI to `~/.lumen/bin` from the rolling `nightly` release
(`LUMEN_INSTALL` and `LUMEN_RELEASE` override the location and tag). Other platforms build from
source — see below.

## Usage

Run scripts / open a REPL through the runtime:

```sh
cargo build --release -p lumen-cli
./target/release/lumen-cli                 # REPL (or: lumen-cli repl)
./target/release/lumen-cli file.js [args]  # run a script to loop quiescence
./target/release/lumen-cli -e 'code'       # evaluate a string
```

The engine also ships a minimal standalone shell (the test262 host, no runtime/host APIs):

```sh
cargo build --release -p lumen --bin lumen
./target/release/lumen file.js [more.js ...]
```

## Conformance

```sh
scripts/test262-clone.sh    # one-time: clone the suite into ./test262
scripts/run-test262.sh      # run it (see crates/test262-runner for env knobs)
```

## Benchmarks

```sh
scripts/run-v8bench.sh      # classic V8 suite (v8-v7); downloads on first run

git clone https://github.com/chromium/octane.git ../octane   # one-time: Octane checkout
scripts/run-octane.sh                    # full Octane suite
scripts/run-octane.sh richards crypto    # selected benchmarks
```

Octane is expected at `../octane` by default; set `OCTANE=/path/to/octane` to override.
