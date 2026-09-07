# Optimizing JIT work

The first implemented pass is backward local liveness over the existing target-neutral
control-flow graph (`jit_ir/liveness.rs`). ARM64 local-load lowering transfers ownership
at proven last uses, avoiding a clone and the original slot's eventual drop. The source
slot becomes `undefined`; the normal TDZ check still runs before the transfer. This
works for every value representation, including BigInt.

The analysis joins successor live sets and iterates to a fixed point across backedges.
Unknown instructions keep all slots live. Captured/aliased frames, exception handlers,
frames above 128 slots and chunks above 4096 instructions are excluded. Failure to
converge within the compile-time budget also declines the optimization. Other native
regions continue using their existing emitters; this pass does not replace their
register allocation or introduce speculative deoptimization.

Set `LUMEN_JIT_NO_LAST_USES=1` to disable the pass for comparison. `LUMEN_JIT_FAST`
continues to control local-load lowering through its existing bit 3.

## Initial evidence

On Apple M4, preliminary three-round comparisons with the same release executable,
alternating the pass on and off, were effectively flat:

| Workload | Disabled median | Enabled median |
| --- | ---: | ---: |
| 5000 DeltaBlue iterations | 9802 ms | 9758 ms |
| 10000 Djot parse/render iterations | 6064 ms | 6047 ms |

These differences are within noise, and some runs overlapped development compilation; the shared machine also had unrelated
compiler activity.
They are not evidence of an application speedup. A quiet rerun is required before
making any performance claim. The pass is an initial reusable optimization, not the
completed milestone of improving both DeltaBlue and a real application substantially.

Five-second native sampling showed reference-counting, allocation and cycle collection
in both workloads. Djot additionally executes significant AST-interpreter work; its
hot-function log includes arrows reading lexical `this`, class constructors and renderer
methods. Sampling does not by itself establish how much any proposed change will save.

The next work needs to address those costs: broaden compilation coverage for real
application functions, then use shared IR facts for register-resident values and guarded
inlining across operations. Heap work should be guided by measured collector/allocation
costs. Keep the interpreter as the semantic oracle throughout.

## Reproducing the parser workload

From the repository root:

```sh
(cd examples/djot-parser && npm ci --ignore-scripts)
bun build examples/djot-parser/bench.mjs --target=browser --format=iife --outfile=/tmp/lumen-djot.js
cargo build --release -p lumen --bin lumen
LUMEN_JIT_NO_LAST_USES=1 target/release/lumen --tier=jit /tmp/lumen-djot.js
target/release/lumen --tier=jit /tmp/lumen-djot.js
node /tmp/lumen-djot.js
bun /tmp/lumen-djot.js
```

The bundle isolates engine execution from module-loader differences. Bun is only a
benchmark build tool. Every iteration verifies the complete rendered HTML against the
reference output. The timer includes tier warmup but excludes initial bundle loading.
Run engines sequentially, alternate ordering and compare several fresh processes.

## Validation of this slice

- 603 engine unit tests passed, including five new liveness tests.
- Three public-boundary ownership tests passed in all three execution tiers.
- Differential fuzzing: 1996 agreeing programs, four skipped for resource budgets.
- Language expression/statement test262 slice: 20438/20439 passed. The remaining
  `dynamic-import/import-fulfilled-member-of-errored-cycle.js` failure reproduces with
  the pass disabled.
- Formatting and strict structure checks on the new modules passed. Strict Clippy
  remains blocked by 86 library / 88 library-test pre-existing errors; no new module
  diagnostics were reported.

## Lexical-this compilation

Arrow functions that read lexical `this` now compile. The new bytecode operation resolves
that binding when it is read, using the interpreter's shared resolver. It does not bind
`this` from the arrow's caller or eagerly read a derived constructor's uninitialized
binding. Nested arrows forward the original binding through their environment chain.
Receiver-direct property operations remain reserved for ordinary function receivers.

Three fresh-process rounds, alternating engine order and without concurrent development
builds, produced these medians (milliseconds, lower is better):

| Workload | Before | After | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Djot, 10000 verified parse/render iterations | 6149 | 5705 | 235 | 145 |
| DeltaBlue, 5000 iterations | 10003 | 9872 | 205 | 315 |

Djot takes about 7% less time; DeltaBlue's approximately 1% difference is not a meaningful
win. This is progress in compilation coverage, not proximity to Node/Bun. The working
performance target is within 2x of both on the engine suite and real application workload.

Validation: 604 unit tests and 13 public-boundary integration tests passed. The same
20438/20439 language conformance tests passed, with only the previously reproduced
dynamic-import failure. Strict Clippy still reports the existing 86/88 errors outside
these modules. The new modules pass strict structure checks.

## Case-block lexical declarations

Switch lowering now creates the case block's shared lexical scope after evaluating the
discriminant, initializes every case's `let`/`const` to TDZ before testing cases, and
preserves fallthrough, default placement and outer-loop control flow. The lowering is
extracted into `bytecode/switch.rs`.

This removes one renderer compilation blocker, but Djot's renderer still contains an
unsupported `for…in` loop. Its three-round median stayed flat (5541 ms before, 5546 ms
after). Compiled enumeration is the next coverage dependency; no renderer speedup is
claimed from switch support alone.

Validation: 605 unit tests, 17 integration tests, and all 111 switch conformance tests
passed. Strict Clippy remains at its existing 86/88 diagnostics, with none in the new
module. The module passes the strict structure audit.

## Compiled enumeration

`for…in` loops with uncaptured `let`/`const` identifier heads now compile. Key snapshots
use the interpreter's shared namespace/prototype enumeration implementation; stepping
rechecks property presence to skip deleted keys. Hidden frame slots retain the base,
private key snapshot and cursor. No user array iterator is invoked. Captured loop heads
retain their per-iteration environments through the interpreter. SSA regions explicitly
decline the new cursor-writing operation until that local effect is modeled.

This completes the two compilation dependencies for Djot's main renderer. Three-round
medians were 5587 ms before and 5182 ms after (about 7% less time), versus Node 227 ms
and Bun 143 ms. The parser entry still falls back because of a captured default parameter.

Validation: 606 unit tests, 21 integration tests and 20438/20439 language conformance
tests passed. The sole conformance failure is the previously reproduced dynamic import
case. Formatting/structure checks pass; strict Clippy remains at the same existing
86/88 errors, with no diagnostics in the new modules.
