# Numeric regions inside shared-closure inlines

The shared-closure implementation conservatively cleared JIT fast bit 15 for an
entire chunk with dynamic inline owners. That blocked numeric loops and ordinary
instruction templates, even when they never touched the hidden callee. It also
blocked all numeric field expressions. This follow-up narrows those restrictions.

`jit/numeric_loops.rs` owns selection between the existing numeric CFG and linear
loop emitters. Both planners reject calls, InlineGuard and slot resets. Supported
numeric writes prove that overwritten values are drop-free; receiver objects and
unmentioned locals retain their canonical owners. Thus a numeric loop can run
inside an active inline without moving or releasing its callee. Guard failures
and bounded backedges materialize numeric state before resuming ordinary bytecode
positions. Observable helpers then record the live inline frame as before.

Numeric field expressions are also restored. Their emitter borrows object roots
and publishes only the original terminal instruction's operands after all guards
succeed. It never consumes the hidden callee slot. The existing terminal remains
responsible for writes, returns and their observable behavior.

Complex regions that can consume call setup or perform speculative writes remain
restricted. Ordinary instruction fast paths use the user's original fast mask;
only complex region selection receives the restricted mask. The overall closure
feature remains opt-in through `LUMEN_JIT_INLINE_CLOSURES=1`.

Colocated tests exercise fresh inlined linear and branching loops, actual native
execution, 2048-iteration continuations, out-of-bounds side exits to prototype
getters, GC while a dynamic inline frame is active, exceptions and subsequent
calls. The side-exit getter verifies that a native loop was entered before it ran.
A separate numeric-expression test covers fresh closures and getter/throw fallback.

Measurements on 2026-09-08 use implementation `fb5995a` and retained binary
`835d2fd`, Apple M4/macOS arm64, and three rotated fresh-process runs per mode.
Builds, tests and sampling finished before timing; unrelated host activity was
not controlled. `Retained inline` enables closure inlining on the old binary.
`Off`, `Inline` and `Combined` use the new binary, with neither switch, just
`LUMEN_JIT_INLINE_CLOSURES=1`, or that plus
`LUMEN_JIT_REFRESH_CLOSURE_CACHE=1` respectively.

| Median | Retained inline | Off | Inline | Combined |
| --- | ---: | ---: | ---: | ---: |
| Standalone NavierStokes score | 36693 | 38842 | 38951 | 39287 |
| Shared-closure fixture, ms | 54 | 100 | 56 | 55 |
| Djot, ms | 3489 | 3484 | 3500 | 3448 |
| Classic V8-v7 score | 8557 | 8641 | 8642 | 8692 |
| NavierStokes subscore within classic | 36322 | 38732 | 38765 | 38359 |

Scores are higher-is-better. Standalone NavierStokes improves 6.2% against the
retained inline build and returns to approximate off-mode parity. The full
classic run likewise recovers the inline-mode NavierStokes regression. The
classic aggregate rises about 1.0% against the retained inline build, and is
essentially equal to Off. The shared-closure improvement remains substantial
(100 to 56 ms), although the retained inline sample is slightly faster at 54 ms.
Djot remains roughly flat with inlining alone; Combined reduces its time by
about 1.0% against Off. Small changes are limited by the sample count and host
variation. The closure switches remain opt-in.

Node v24.18.0 and Bun 1.3.14 reference medians for the identical Djot input are
222 and 144 ms, measured afterwards in three fresh processes each. Combined
Lumen therefore still takes about 15.5x Node's time and 23.9x Bun's time on this
workload. This fixes a numeric regression; it does not close the broad engine gap.

The [raw report](benchmarks/closure-numeric-regions-2026-09-08.json) includes all
48 Lumen runs, reference runs, per-suite scores, binary/input hashes, fluid-field
validation values and profile analysis. Djot, shared-closure and classic inputs
are unchanged from the preceding report. Standalone NavierStokes concatenates
`v8-v7/base.js`, `v8-v7/navier-stokes.js` and `v8-v7/run.js` with its `load` calls
removed and `print` supplied by `console.log`.

Numerical validation runs separately from timing: after `setupNavierStokes()`,
replace the display callback with one retaining the latest Field, run 150 frames,
and sample density, x velocity and y velocity at coordinates x/y = 0,13,...,117.
All 300 parsed numeric values match Node and Bun exactly for both retained and
candidate binaries with both closure switches enabled. This sampling is an
additional check, not exhaustive numerical equivalence.

The fresh diagnostic uses Combined, `LUMEN_JIT_MAP=1`, the same Djot input with
40,000 iterations, and `/usr/bin/sample` for six seconds starting after two
seconds. Every HTML output is checked (12,640,000 characters total); instrumented
elapsed time is excluded from benchmark evidence. Offline tree analysis
reconciles all 5,104 samples from one main thread, with nonnegative exclusive
residuals. All 1,260 unknown native leaves map to same-process JIT ranges.

Allocator leaves account for 415 samples (8.1%), destruction for 479 (9.4%), and
GC ancestry for 690 (13.5%). The first two are disjoint leaf categories; GC
ancestry overlaps them and must not be added. Name-path ancestry accounts for
201 samples (3.9%). Generated-code leaves account for 24.7%; common mapped spans
include calls and property/name reads. Opcode spans can include outlined tails
and are not exact opcode cost attribution. These are sampled stacks, not
estimates of removable time. The next investigation should measure avoidable
Value ownership transfers and temporary allocations across call/property paths,
with collector cost assessed alongside them.

Validation: 804 engine tests pass with the flag both off and on. Selected
workspace checks pass 1090 tests with two ignored. The three previously reproduced
HTTP/2 targets (`http2_client`, `http2_secure_client`, `http2_server`) remain
excluded; Node was on PATH for integration checks. Selected Test262 expressions
and statements pass 20438/20439 in both modes, with the same existing async import
failure. Differential seeds 1–2000 produce 1996 agreements and four budget skips.
Strict Clippy retains the baseline error-message multiset (86 lib / 88 lib-test
errors), with no added diagnostics. Formatting and focused structure audits pass.
The legacy JIT monolith remains; extracting its linear-loop planner/emitter is a
cohesive next structural step. Full local logs and raw sample/maps are retained
in `/tmp/lumen-loop-work`; the existing Test262 report directory remains ignored.
