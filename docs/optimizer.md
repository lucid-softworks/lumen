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

## Shared compiled activation layouts

Compiled closures now share an immutable name-to-slot layout and allocate a contiguous
array of binding values for each activation. Parameters, lexical bindings, lexical
`this` and hoisted functions initialize slots directly. Structural map changes promote
the activation to the dynamic representation and invalidate cached entry addresses.
Zero generation is reserved for pristine layouts and is never reused after wrap.

ARM64 captured loads/stores/initialization can use the activation's binding-array base
and a compile-time offset. The generated path checks the scope generation and borrow
state, preserves TDZ and reference ownership, and falls back after structural mutation
or for BigInt operations. Ordinary call frames remain present.

The work is split across `interpreter/bindings.rs`, `interpreter/bindings/layout.rs`,
`bytecode/activation.rs` and `jit/captured.rs`. A callback test promotes a live compiled
activation's map and verifies subsequent captured reads/writes use the new storage.

The immutable-layout-only stage measured flat on Djot (5016 to 5019 ms) and approximately
flat on DeltaBlue (9965 to 9832 ms). With native captured access, three rotated rounds
measured Djot at 4985 to 4983 ms and DeltaBlue at 9799 to 9788 ms. No application-level
speedup is claimed. Node/Bun medians were 232/147 ms for Djot and 197/317 ms for DeltaBlue.

A separate verified closure diagnostic (100000 activations, ten reads per activation)
used 3.30 billion retired instructions versus 3.68 billion before, with maximum resident
size approximately 76 versus 110 MiB. This was one diagnostic run during unrelated build
activity, so its wall-clock timing is not used as a performance claim. Shared layouts
also enable caching deeper lexical lookup paths across fresh activations; the current
name cache only handles direct and one-parent binding resolutions.

Validation: 613 unit tests, 34 integration tests and 1996 differential cases passed
(four fuzzer budget skips). Language conformance remains 20438/20439 with the existing
dynamic-import failure. Strict Clippy remains at the existing 86/88 diagnostics, with
none in the new modules. Formatting and strict module-structure checks pass.


### Guarded deeper lexical paths

Free-name reads can now cache paths of up to eight scopes. Pristine compiled scopes
are validated by their shared binding-layout identity; dynamic scopes require their
exact weak-pinned identity and structural generation. The reader walks live parent
links, validates every intervening scope against shadowing, and reads the live binding
or ordinary global data-property slot. TDZ, import redirects, with scopes, changed
shapes and accessors keep the checked resolution path. No JavaScript executes during
cache validation, and cached paths do not retain scope values or closures strongly.

Two quiet rotated Djot comparisons measured 4974 to 4714 ms and 4965 to 4740 ms
(approximately five percent lower). Node measured 230/228 ms and Bun 150/143 ms.
DeltaBlue remained approximately flat. An unrelated compiler build restarted during
the third round, which is retained in the raw results but excluded from this observation.
This remains preliminary performance evidence, far from the two-times target. Raw
results are in `name-path-results.json` in the external optimizer artifact directory.

Validation: 616 unit tests, 34 integration tests and 1996 differential cases passed
(four budget skips). Language conformance is unchanged at 20438/20439; strict Clippy
has the same existing 86/88 diagnostics and none in the new module. Formatting and
strict module-structure checks pass. Dedicated tests exercise fresh-layout reuse,
changed ancestors, shadowing, TDZ, live writes, global getters/deletion and realm guards.


### Direct helper for guarded name-path hits

ARM64 free-name cache misses now enter a dedicated guarded-path probe before the
full bytecode operation dispatcher. A successful probe transfers the cloned value
directly into the operand stack; call references also receive an undefined receiver.
A failed probe leaves the operand stack and local representation untouched and runs
the existing checked helper. BigInts keep Rust's normal clone behavior. Operation
statistics still include these hits. This is a shorter Rust helper route, not generated
machine-code validation of the complete scope path.

Three rotated rounds measured Djot medians of 4669 to 4599 ms (about 1.5 percent lower),
with Node/Bun at 230/151 ms. The first two paired results were 4634 to 4558 and 4669 to
4599 ms; all engines slowed in the third parser round. DeltaBlue was approximately
flat at 9729 to 9653 ms (Node/Bun 200/307 ms). Raw results are in
`direct-name-path-results.json` in the external optimizer artifact directory.

All 617 unit and 34 integration tests passed. A test-only hit counter proves the native
route was exercised while testing owned values, receivers and getter/deletion changes.
Language conformance remains 20438/20439, differential testing remains 1996 agreements
with four budget skips, and strict Clippy retains only the existing 86/88 diagnostics.
Formatting and the new module's strict structure audit pass.

A separate warmed ordinary-inline caller-reflection probe reproduced an existing
correctness gap: `target.caller === invoke` succeeded only 110 of 500 calls, already
on the archived pre-helper binary. Correct frame bookkeeping is required before
expanding inlining. The probe is archived as `ordinary-inline-caller-probe.js`.


### Virtual frames for inlined calls

The optimizer now records an immutable inline call chain for each bytecode location,
separately from the op stream. Checked helpers record their location before an operation
can enter JavaScript, throw or collect garbage. Native direct calls record the caller's
location before pushing the physical callee. Reflection and error-stack capture expand
that chain on demand, preserving nested `fn.caller` identities and strict-caller censoring.
The physical frame grows from 24 to 32 bytes; its native push/pop ABI and layout assertions
were updated together. Inlined bodies retain the existing restrictions on arguments,
closures and handlers; async/generator callees are explicitly excluded.

Inline targets use the existing GC bookkeeping pins when optimized code is installed.
These pins count as internal references, so inactive targets remain collectable. During
collection, active inline locations temporarily root their callees. This also keeps a
callee alive if its last ordinary reference is removed inside the inlined body, without
adding a reference-count operation to every inlined call. The locations themselves own
only weak function handles and immutable parent metadata.

An earlier explicit enter/leave-op implementation fixed reflection but disrupted five
specialized-region planning checks. It was replaced, not retained. Keeping location
metadata outside the bytecode restores all five checks. A dedicated Richards test changes
a field into a getter after warmup and verifies that the optimized region's side exit
reconstructs the inlined predicate as the getter's caller. Other tests cover nested
exceptions, strict callers, native code generation, active-callee collection safety and
collection of the same callee after return. The original warmed caller probe now passes
500/500 instead of 110/500.

Three rotated comparisons measured Djot at 4561 to 4611 ms (about one percent overhead)
and DeltaBlue at 9718 to 9693 ms (flat). Node/Bun were 230/146 ms and 195/312 ms respectively.
This is a correctness prerequisite for expanding the optimizer, not a speedup claim.
Raw results are in `virtual-frame-results.json` in the external optimizer directory.

Validation: all 622 unit and 34 integration tests pass, including every specialized-region
check. Language plus Function conformance remains 20947/20948 with the existing dynamic
import failure; differential testing has 1996 agreements and four budget skips. Strict
Clippy retains the existing 86/88 diagnostics and none in the new modules. Formatting
and strict module-structure audits pass.
