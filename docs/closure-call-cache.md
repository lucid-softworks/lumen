# Bounded fresh-closure call-cache refresh

The implementation in `2faa9d7` is opt-in:

```sh
cargo build --release -p lumen --bin lumen
LUMEN_JIT_REFRESH_CLOSURE_CACHE=1 target/release/lumen \
  crates/lumen/benches/fixtures/closure_calls.js
```

Run the same command without the environment variable for the disabled control.
The switch enables the validated no-activation closure retry as well as publication
of its result; the older `LUMEN_JIT_FRESH_CLOSURE_RETRY` switch is not required.
Both switches are read once per process and use presence, so unset them to disable.

## What changes

Previously, a fresh closure could reuse cached code through a temporary retry, but
subsequent calls to that same closure still missed the native identity cache. The
new path publishes the validated entry so those calls can use ordinary native
dispatch. It preserves the actual callee, receiver and captured environment, the
normal inline-recompile opportunity, and ordinary physical call frames.

Publication uses one replaceable weak pin per affected call site. Pins are stored
lazily in the owning chunk, preserving the public `CallSite` layout and avoiding
the chunk's 4,096-entry lifetime pin budget. Every entry referencing the previous
replaceable identity is cleared before its pin is released. Cache seeding and
optimizer discovery do not use these replaceable pins: they must not authorize
machine code that embeds a pointer beyond the pin's lifetime.

The retry revalidates live Function, environment, realm, compiled code and frame
metadata. Native entry addresses and direct-call flags are reconstructed from the
live code before publication; recycled allocation addresses cannot prove these
derived fields. Activation-bearing retries and inline identity guards retain
their existing behavior.

## Measurement

Measurements use an Apple M4, release builds, verified outputs, sequential fresh
processes, and three rounds rotating the retained, disabled and enabled binaries.
The retained binary is from branch base `6976d91`. No builds, tests or profiling
jobs from this task overlap timings. Other machine activity is not controlled.

The closure fixture warms once, then verifies five batches of 10,000 closures,
each called 32 times. This measures 1.6 million calls after warmup. Its millisecond
timer is suitable for comparing Lumen modes; Node/Bun complete this fixture too
quickly for a useful ratio at this timer resolution.

Djot uses the unchanged bundled `examples/djot-parser/bench.mjs`, checking the
complete HTML on each of 10,000 iterations. DeltaBlue runs 5,000 iterations with
the upstream checks. The classic V8-v7 suite retains its built-in verification.
Application times include tier warmup but exclude initial source loading.

Final medians (milliseconds are lower-is-better; classic score is higher-is-better):

| Workload | Retained | Disabled | Refresh enabled |
| --- | ---: | ---: | ---: |
| Closure calls, 1.6 million | 111 ms | 111 ms | 78 ms |
| Djot, 10,000 verified parses | 3,434 ms | 3,457 ms | 3,402 ms |
| DeltaBlue, 5,000 iterations | 8,328 ms | 8,269 ms | 8,288 ms |
| Classic V8-v7 score | 8,694 | 8,662 | 8,735 |

Refresh reduces the closure fixture's time by 29.7% against both medians; enabled
runs are 78/78/78 ms versus disabled 114/110/111 ms. Djot is 1.6% lower against
disabled, with all three paired runs improving, and 0.9% lower against retained.
DeltaBlue is 0.2% slower against disabled: one pair ties and two are slower.
Classic score is 0.8% higher against disabled and 0.5% higher against retained.
Host variation is visible in the final classic controls (retained scores range
8,416–8,716); an unrelated process used about one CPU core during part of this
batch. All runs, including that lower control, are retained.

These results establish a useful closure-call improvement and a small Djot gain
in this batch, not broad engine parity. Refresh remains disabled by default. The
within-2x Node/Bun objective remains unmet. The next substantial boundary is
inlining across closure instances with correct dynamic callee and lexical-frame
ownership; these refreshed physical calls still encounter the existing exact
inline guards.

[Raw runs, medians, configuration and hashes](benchmarks/closure-call-cache-2026-09-08.json)
are committed with this report. The initial experiment also compared Node 24.18.0
and Bun 1.3.14, but used an earlier candidate layout; its timings are not mixed
into this final table.

## Validation

- All 797 engine unit tests pass with refresh enabled. The ordinary workspace run
  has 1,083 passing tests and two ignored tests across 74 successful targets.
- The workspace is not fully green: `http2_client` and `http2_secure_client` fail,
  and `http2_server` hangs. Both client failures reproduce on an isolated export
  of base `6976d91` with Node 24.18.0. Both baseline server tests also remain
  running at 65 seconds, when their process group is terminated. The candidate's
  hung server target was terminated so the remaining workspace tests could run.
- Strict Clippy reports the same error-message multiset as the isolated baseline:
  86 library errors and 88 library-test errors. No new lint messages were added.
- Enabled and disabled JIT conformance runs each pass 20,438 of 20,439 tests in
  `language/expressions` and `language/statements`. Both report the same async
  module failure, `TestError: The import of C`. This is a selected conformance
  run, not the complete Test262 suite.
- Differential seeds 1 through 2,000 produce 1,996 agreements and four budget
  skips with refresh enabled; no divergence is reported.
- Formatting, whitespace checks and the structural audits of the affected
  focused Rust modules pass.

Tests cover repeated calls actually avoiding the retry, current closure identity
and captures, receiver binding, collection between closure instances, exception
unwinding, argument evaluation count, native entry reconstruction from live code,
and bounded pin replacement after 5,000 refreshes.

Final engine binary SHA-256:
`0e6b452bed4e027ccbe035704d0dc667457b826440c51bf794f3fc7b6c4161e8`.
Retained baseline binary SHA-256:
`44ab385b25778668dab77218f262f2bcf88c3450a24c46c7ae50287de5d95597`.

Detailed build/test logs, the initial experiment and retained binaries are in the
local `/tmp/lumen-closure-work` archive and `/tmp/lumen-closure-baseline`.
