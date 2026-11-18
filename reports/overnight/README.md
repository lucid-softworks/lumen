# Lumen overnight performance investigation

The glibc fix is open separately as [PR #53](https://github.com/lucid-softworks/lumen/pull/53). The performance changes are now being submitted for review at the user’s request despite the original speed gates remaining unmet: the final Vite builds are slower than Node and Bun, and the full V8 suite is still outside the requested 2× limit. The benchmark results and original gate outcomes below are unchanged.

Open `lumen-performance.html` for the visual comparison. `summary.json` and the individual result files contain exact numbers, commands, versions, source/binary hashes and raw output.

## Final Vite result

Same locked `examples/vite-app`, Vite 6.4.6, three transformed modules; unchanged application and dependencies. Every successful run has identical output paths and SHA-256 hashes across Lumen, Node and Bun. Timings include process startup, the build and process shutdown. Each run removes `dist` first.

| Compile cache | Lumen | Node 24.22.2 | Bun 1.3.10 | Lumen / Node | Lumen / Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Warm, 21 measured builds each | 236.8 ms | 177.8 ms | 210.8 ms | 1.332× | 1.124× |
| New empty directory each process, 7 builds each | 363.6 ms | 217.9 ms | 210.2 ms | 1.668× | 1.730× |

One excluded warmup per runtime; execution order rotates by round. The empty-cache run supplies a unique `NODE_COMPILE_CACHE` directory to every process, including the warmup, and deletes it afterwards. Bun receives the same environment; its cache behavior remains its own. Filesystem caches are warm in both modes.

The initial seven-run warm comparison is also retained (`vite-warm.json`: 248.4 / 171.9 / 207.8 ms). The wider sample was collected because the shared host showed substantial variation, not because parity had been reached. The extended warm Lumen range was 224.9–360.5 ms; Node 150.8–216.0 ms; Bun 198.4–240.3 ms. These are point estimates on a busy machine, not an isolated performance guarantee.

The original unmodified CLI did not produce a build: Rollup incorrectly selected its musl addon. Adding the actual runtime glibc version exposed a separate 10-second timer delay. After fixing timer references, the early five-run Lumen median was 684.1 ms. The final warm median is about 65% lower, measured in a different time window. The original failed build is not treated as a valid speed result.

## Why it was slow and what changed

- `process.report.getReport().header` omitted `glibcVersionRuntime`, causing Rollup's native addon selection to choose musl on GNU/Linux. PR #53 reports the actual runtime libc version and leaves the field absent elsewhere.
- Vite's unreferenced fallback timer kept Lumen alive for ten seconds. Timeout/interval handles now track reference state and the event loop can stop when only unreferenced timers remain.
- Parsing repeatedly allocated token strings and scanned punctuation and keywords. Token payloads now move, punctuation uses prefix dispatch, source extraction uses byte ranges, and keyword lookup uses the sorted table.
- Vite's `module.enableCompileCache()` previously did nothing. It now enables a private, build-specific, full-source-validated AST cache; missing, corrupt or inaccessible entries fall back to parsing. This is a parse cache, not persisted machine code. Warm and cold results are reported separately.
- Generic property lookup, eager named-index construction, byte-at-a-time TypedArray copying and JavaScript Buffer base64 loops accounted for significant work. Dense slots, lazy named indexes, direct typed-array copies and native base64 decoding remove those costs. POSIX path normalization/join and builtin ESM bridge generation now also run natively.
- The x64 JIT still delegated many basic operations to runtime helpers. It now emits guarded arithmetic, numeric updates, branches, integer shifts/conversions, property reads and dense numeric array reads/writes. Unhandled types, holes, changed descriptors and exotic objects retain checked fallback paths.
- Unused crypto startup arithmetic was deferred or replaced with exact constants. The CLI uses the engine's bounded allocation cache, with safe fallback during thread-local teardown.

Instruction profiles dropped from roughly 8.18 billion instructions after the timer fix to 1.25 billion at the native-Buffer stage. Remaining costs include object allocation/property work, AST decoding, interpreted module initialization and coroutine scheduling. The experimental fiber backend did not ship in this branch; neither did the temporary phase/function profiling hooks.

## V8 benchmark

All 48 runs passed. The final geometric mean gap is **49.10×**, compared with **86.02×** in the original baseline. None of the eight workloads meets the 2× gate.

| Workload | Lumen median score | V8 median score | V8 / Lumen |
| --- | ---: | ---: | ---: |
| Richards | 633 | 56356 | 89.03× |
| DeltaBlue | 643 | 130021 | 202.21× |
| Crypto | 2144 | 55682 | 25.97× |
| RayTrace | 2312 | 91463 | 39.56× |
| EarleyBoyer | 791 | 102410 | 129.47× |
| RegExp | 282 | 12625 | 44.77× |
| Splay | 2361 | 38553 | 16.33× |
| NavierStokes | 2964 | 57225 | 19.31× |

Source: [Mozilla arewefastyet V8 v7 suite](https://github.com/mozilla/arewefastyet/tree/master/benchmarks/v8-v7). The original upstream warmup, timed measurement, minimum 32 iterations and correctness checks remain unchanged. Each of the eight workloads runs in three fresh Lumen processes and three fresh Node/V8 processes, alternating engine order. Scores retain fractional precision; higher is better. The reported gap is median V8 score divided by median Lumen score. The aggregate is the geometric mean of the eight ratios. A PR gate requires **every** workload to be at or below 2×; averaging does not waive a failing workload.

This suite covers a different workload from a Vite build. Vite timing does not establish general JavaScript parity. The machine was not CPU-isolated; baseline and final runs happened in separate measurement windows.

## Validation and limits

- Full engine suite: 772 tests passed (719 unit tests plus 53 integration tests). The subsequent snapshot lookup change passed all four targeted snapshot tests, including one new operator-table test. New JIT semantics are checked in interpreter, bytecode and JIT tiers.
- Runtime unit suite: 105 passed, one pre-existing ignored test. Node-derived fixtures cover 367 base64 strings and POSIX path normalization/join; tests include malformed inputs, lone surrogates, TypedArray copy independence, NaN payloads, detached/resizable/shared buffers and overridden iterators.
- Broader runtime run: 167 passed, one failed, one ignored, plus one integration target stopped after hanging. The cleartext HTTP/2 client failure and server protocol problems reproduce with the original unmodified CLI; baseline reproduction output is retained under `validation/`. This is **not** a fully green runtime integration suite. The editor test's hardcoded `/usr/bin/true` was changed to `true` resolved through `PATH` for this NixOS host.
- Release binaries built successfully. No benchmark fixtures or dependencies were changed. GNU/Linux x64 was exercised locally; Windows, ARM64 and non-GNU libc branches were not executed here. Existing compiler warnings remain.

## Reproduce

From the repository root, with Node, Bun and Rust available:

```sh
npm ci --prefix examples/vite-app
cargo build --release -p lumen-cli -p lumen --bins
node scripts/bench-vite.mjs --bun /path/to/bun --runs 21 --out reports/overnight/vite-warm-extended.json --require-parity
node scripts/bench-vite.mjs --bun /path/to/bun --cold-cache --out reports/overnight/vite-cold.json --require-parity
node reports/overnight/v8/run.mjs
node reports/overnight/summarize.mjs
cargo test -p lumen
cargo test -p lumen-runtime --no-fail-fast
```

The Vite commands intentionally return a failing exit code when parity is not met. Run each command separately to collect both comparisons. Runtime SQLite tests require a loadable system SQLite library. The V8 runner downloads any missing upstream inputs and verifies their recorded hashes before execution. Archived binaries and large intermediate profiling files remain local and are excluded from Git.
