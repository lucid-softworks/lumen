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

## Captured literal defaults

Captured parameters can now use primitive literals or empty object/array defaults. The
initializer writes the captured binding only for an absent/undefined argument. Defaults
with effects or parameter-scope dependencies, and same-name hoisted-function conflicts,
remain on the interpreter. Existing uncaptured-default analysis moved into
`bytecode/parameters.rs` alongside the new initialization path.

The parser entry still encounters constant computed keys in its handler object, so this
step alone measured flat (5451 ms before, 5470 ms after). Those static keys are the next
compilation dependency.

Validation: 608 unit tests, 24 integration tests and the same 20438/20439 language
conformance tests passed. The remaining failure and strict-Clippy baseline are unchanged.
The new module passes strict structure checks.

## Constant computed object keys

Object literals now fold computed string literals, including parenthesized strings,
into the existing shape-template path. Value evaluation order, inferred function names,
duplicate-key insertion order and computed `__proto__` data properties retain their
semantics. Dynamic keys still use the interpreter. The lowering is extracted into
`bytecode/object_literal.rs`.

Three rotated fresh-process rounds of verified Djot produced medians of 5256 ms before
and 4948 ms after (about 6% less time), versus Node 228 ms and Bun 143 ms. This remains
about 22x/35x slower than those engines on this workload.

Validation: 609 unit tests and 26 integration tests passed, as did 1996 differential
fuzzer cases (four budget skips). Language conformance remains 20438/20439, with the
existing dynamic-import failure. Formatting and strict structure checks pass; strict
Clippy remains at its existing 86/88 diagnostics outside the new module.

## Rejected forwarded-call cache experiment

A lazy target cache behind `Function.prototype.call` reused the existing guarded call
entry, including realm/epoch validation and weak identity pins. Target mutation, fresh
closures, overrides, proxies, strict receivers, recursion and throw-ownership tests
passed across all tiers. However, three-round medians stayed flat on Djot (4986 to
4998 ms) and regressed on DeltaBlue (9734 to 9917 ms). The implementation was removed;
the experiment patch and raw measurements remain in the external benchmark directory.

## Rejected automatic-GC cache-retention experiment

Keeping the bounded allocator cache warm after automatic collections, while retaining
pressure relief for explicit host collections, did not improve these workloads. Three
rotated rounds measured Djot at 5009 to 4988 ms and DeltaBlue at 9745 to 9759 ms. Median
maximum resident sizes were approximately 74 MiB and 49 MiB for both revisions. This
change was removed too; allocation-cache flushing is not the material bottleneck here.

An initial inlining-budget probe also regressed: raising the source budget from 320 to
4096 operations and the callee limit from 96 to 512 changed DeltaBlue from 9725 to
10775 ms and Djot from 4979 to 5152 ms. Delaying that wider recompile to 1000 calls
did not help. These are single-round rejection probes, not performance claims; defaults
remain unchanged. The next experiment targets forwarded-call elimination explicitly.

## Rejected forwarded-call inlining experiment

Profiling forwarded targets enabled guarded inlining behind both builtin and target
identity checks. A constructor optimizer trigger and an ownership-preserving stack
shuffle were also tested; dedicated unit checks proved ordinary calls and fast `new`
entries reached the optimizer. Language/Function conformance passed 20947/20948 cases
(the existing dynamic-import failure), and the earlier variant passed the engine suite.
The stack-shuffle variant passed 612 unit tests and 31 integration tests.

Measured intermediate variants remained essentially flat: forwarded inlining measured
Djot 4977 to 4922 ms and DeltaBlue 10018 to 9936 ms; adding constructor tiering did not
produce a material improvement either. More importantly, a subsequent reflection probe
found a correctness regression: 500 forwarded calls correctly identified their caller
before the experiment, but only the initial 110 did after inlining. The inlined body
had lost its observable legacy `function.caller` frame.

All implementation changes from this experiment were removed. Boundary tests remain
for forwarding, guard misses, constructor behavior and warmed caller reflection. A
general optimizer must preserve or reconstruct inlined frames at observable operations;
identity guards alone do not make frame elimination semantics-preserving. The external
benchmark directory retains the experiment patch, source and measurements.
