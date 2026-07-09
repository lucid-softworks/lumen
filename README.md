# lumen

A from-scratch JavaScript **engine** in Rust — std only, zero dependencies — and a
**runtime** being built on top of it, the way Node/Deno/Bun wrap a JS engine with an event
loop and host APIs. Every crate in the workspace is std-only: no `tokio`, `mio`, `libc`,
`rustyline`, `serde`, or any other third-party dependency, anywhere.

## The engine (`crates/lumen`)

A lexer, parser, and **three execution tiers**:

- a **tree-walking interpreter** — the reference oracle: the spec semantics live here, and
  every other tier must match it observably (a differential fuzzer, `lumen-difftest`, holds
  them to that);
- an opt-in **bytecode VM** — functions compile whole (or not at all — no deoptimization) to
  a stack machine with slot-homed locals, per-site inline caches for property and free-name
  access backed by object shapes (hidden classes), and dense-array element fast paths;
- an **ARM64 template JIT** (macOS/Apple Silicon) — bytecode lowers to real machine code:
  per-op templates with the interpreter as the shared slow path, inline-cache reads baked
  into the instruction stream, fused compare-and-branch, exact-`ToInt32` bitops, and numeric
  *register chains* that keep runs of arithmetic entirely in FP registers. On other
  platforms the JIT tier degrades to the bytecode VM.

Tier selection: `--tier=interp|bytecode|jit` (interp is the default). Functions tier up
after a call-count threshold — immediately if the body contains a loop.

The language surface: generators and `async`/`await` running on stackful coroutines (async
bodies suspend on the bytecode VM itself), full `RegExp` (including `\p{…}` and inline
modifiers), typed arrays, `Proxy`/`Reflect`, ES modules (top-level await, `import defer`,
source phase), `Intl`, and `Temporal`.

`Intl` (ECMA-402) and its CLDR data tables are behind the default-on `intl` cargo feature —
the largest single contributor to binary size (~3 MB of the release binary). Build with
`--no-default-features` for a small engine: the `Intl` global is absent and the `toLocale*`
methods degrade to their locale-independent forms, the way engines built without i18n do.

On dependencies and `unsafe`: the workspace stays std-only — the JIT maps executable memory
through raw `extern "C"` declarations of `mmap`/`pthread_jit_write_protect_np` rather than
libc. The interpreter and bytecode VM are safe Rust; `unsafe` is concentrated where machine
code meets the object graph (the JIT's executable pages and its templates' raw reads — every
baked offset is *measured at runtime* against the live types and fails closed to the checked
helper if anything doesn't hold) and in the N-API addon loader's `dlopen` bridge.

**Passes 100% of [tc39/test262](https://github.com/tc39/test262): 53,400/53,400** (including
annexB, intl402, and staging) — on the default tier and under `LUMEN_TIER=jit`.

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

test262-runner   conformance harness (parallel workers over ./test262)
lumen-difftest   differential fuzzer across the three execution tiers
lumen-wasm       wasm build of the engine
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
LUMEN_TIER=jit scripts/run-test262.sh    # same suite against the compiled tiers
```

The execution tiers are also held together by a differential fuzzer: every generated program
runs in all three tiers, which must agree on the completion value, thrown errors, the
observable side-effect trace, and final global state. Divergences are delta-minimized into a
regression corpus that replays on every run.

```sh
cargo run --release -p lumen-difftest -- --count 2000
```

## Benchmarks

```sh
scripts/run-v8bench.sh      # classic V8 suite (v8-v7) on lumen; downloads on first run
scripts/bench-compare.sh    # same suite on node + bun + lumen, as a markdown table

git clone https://github.com/chromium/octane.git ../octane   # one-time: Octane checkout
scripts/run-octane.sh                    # full Octane suite
scripts/run-octane.sh richards crypto    # selected benchmarks
```

Octane is expected at `../octane` by default; set `OCTANE=/path/to/octane` to override.

### ARES-6

```sh
# one-time: provide an ARES-6 checkout outside this repo
# the default lookup is the sibling ../ARES-6; ARES6=... overrides it
scripts/run-ares6.sh                    # full ARES-6 suite
scripts/run-ares6.sh air basic          # selected workloads: air, basic, babylon, ml
ARES6=/path/to/ARES-6 scripts/run-ares6.sh babylon ml
```

ARES-6 sources are not vendored here. The runner expects a checkout at `../ARES-6` by default; set `ARES6=/path/to/ARES-6` to point at another checkout.

The reported `summary:` is the ARES-6 geomean in milliseconds, so lower is better. A selected run reports a partial geomean over only the selected workloads, which is useful for local iteration but is not the official full-suite ARES-6 score. If a workload fails, or if the expected metric and completion lines are missing, `scripts/run-ares6.sh` exits nonzero instead of hiding the failure.

Expect the full suite to take a long time on the current tree-walking engine; use selected workloads for quicker local checks.
