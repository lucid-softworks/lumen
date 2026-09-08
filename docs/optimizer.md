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


### Full engine comparison after virtual frames

At `41ed9b3`, three fresh-process rotated runs on the Apple M4 produced the following
V8-v7 median scores (higher is better). Every run passed the benchmark checks.

| Workload | Lumen | Node | Bun |
|---|---:|---:|---:|
| Richards | 23,495 | 66,103 | 72,440 |
| DeltaBlue | 3,375 | 154,432 | 109,961 |
| Crypto | 23,914 | 92,293 | 119,171 |
| RayTrace | 6,320 | 135,196 | 303,025 |
| EarleyBoyer | 3,622 | 148,087 | 158,702 |
| RegExp | 1,628 | 22,729 | 30,518 |
| Splay | 10,455 | 80,766 | 96,045 |
| NavierStokes | 38,173 | 70,716 | 71,457 |
| Score | 8,560 | 83,795 | 99,344 |

The composite is still 9.8 times below Node and 11.6 below Bun. Only NavierStokes is
within twice both engines on this suite. The separately verified parser remains about
20/32 times slower, so the performance goal is unfulfilled.

Warm 10000-entry collection calls measured Map build/lookup at 570/380 microseconds
versus Node 204/32 and Bun 117.8/32. Set build/lookup measured 560/333.3 versus Node
144/26.4 and Bun 108.9/28. Lower is better. Collection lookup is therefore another
clear remaining gap; the indexed storage fixed scaling without closing constant costs.

The complete report, commands, source hashes, versions and every raw sample are in
`/Volumes/XEX-VM/codex-builds/lumen-engine-comparison-virtual-frames/`. The runner is
`run.py`; `results.json`, `summary.json` and `report.md` contain the measurements.

### Guarded collection reads

The collection lookup profile put receiver checks, value cleanup and native call dispatch
ahead of hash-table lookup. Map.get, Map.has and Set.has now share named implementations
that borrow their keys and resolve the backing table once. Exact builtin call-IC hits with
one argument use a dedicated consuming helper, preserving the active-realm guard, depth
limit, GC polling, virtual-frame location and exception cleanup. The helper clones a read
result before releasing its operands, since the result can alias the key or receiver.
It still checks the live writable `__ck` marker; method replacement, proxies, incompatible
receivers and foreign-realm errors retain their checked behavior.

An initial implementation routed these reads through the shared intrinsic helper. That
version regressed Map/Set lookup medians from 360/340 to 385/365 microseconds and was
replaced. The smaller dedicated helper avoids unrelated intrinsic dispatch, dynamic
operand cleanup and constructor-state transitions. Collection reads never enter JS or
coerce their keys, so those transitions are unobservable here.

Three rotated comparisons against `eaf0e46` measured these medians (microseconds per
10000-entry invocation, lower is better):

| Workload | Before | After | Node | Bun |
|---|---:|---:|---:|---:|
| Map build | 570 | 570 | 192 | 118.8 |
| Map lookup | 366.7 | 248 | 33.2 | 31.6 |
| Set build | 560 | 560 | 148.6 | 108 |
| Set lookup | 333.3 | 228 | 26.4 | 26 |

Lookup time falls about 32%; throughput rises about 46–48%. Djot remains flat at
4624 to 4637 ms, versus Node/Bun 229/144 ms. DeltaBlue remains flat at 9778 to 9807 ms,
versus 207/310 ms. All benchmark output checks pass. The remaining lookup gap is about
7.5–8.8 times; this change does not close the engine-wide or parser gap.

The external optimizer directory contains `compare-collection-read-helper.py`,
`collection-read-helper-results.json` and `collection-read-helper-summary.json`.
The rejected shared-helper measurements are in `collection-intrinsics-results.json`.

Validation: 626 unit and 34 integration tests pass, including warmed native-path counters,
all key categories, live updates/deletions, aliasing, method replacement, receiver brands
and foreign-realm errors. Map/Set/WeakMap/WeakSet conformance passes 813/813. Differential
testing has 1996 agreements and four budget skips. Formatting and strict structure audits
pass; strict Clippy retains the existing 86/88 diagnostics, with none in the new modules.

### Consuming collection insertion

Map.set and Set.add now have named native implementations and exact-identity JIT helpers
at their ordinary arities. After the existing depth check and GC poll, the helpers move
key/value operands into storage and return the original receiver owner. Errors consume
the same operands without double drops. Brand checks still inspect the live marker and
backing slot; foreign-realm calls retain their ordinary checked path. Set canonicalizes
negative zero on both sides of its stored pair, while Map preserves the value's sign.

The storage insertion path also uses a single hash-table entry probe. Existing hash
collisions still traverse a SameValueZero chain, updates keep their insertion position,
and new entries append after the old collision head. This avoids a separate find followed
by another index insertion probe.

Three rotated comparisons against `6bb1663` produced these medians (microseconds per
10000-entry invocation):

| Workload | Before | After | Node | Bun |
|---|---:|---:|---:|---:|
| Map build | 560 | 386.7 | 192 | 117.5 |
| Map lookup | 250 | 244 | 32.8 | 30.8 |
| Set build | 560 | 406.7 | 142.9 | 106.7 |
| Set lookup | 232 | 224 | 26 | 28.8 |

Map/Set construction time falls about 31/27 percent; lookup times are essentially flat.
The verified Djot parser remains flat at 4587 to 4612 ms (Node/Bun 225/144), as does
DeltaBlue at 9783 to 9751 ms (198/320). All benchmark output checks pass. Construction
is still about 2.0–2.8 times slower than Node and 3.3–3.8 than Bun; the broader goal is
still unfulfilled. The external optimizer directory contains `compare-collection-inserts.py`,
`collection-inserts-results.json` and `collection-inserts-summary.json`.

Validation: 629 unit and 34 integration tests pass, including both native helper counters,
ownership/aliasing, negative zero, iterator order, missing arguments, method replacement,
brand failures and foreign-realm errors. Collection conformance passes 813/813 and
differential testing has 1996 agreements with four budget skips. Formatting and strict
module audits pass. Strict Clippy retains the existing 86/88 diagnostics, none in the
new modules or modified collection storage.

### Internal collection brands

CollectionData now records its immutable Map/Set/WeakMap/WeakSet kind. Constructors,
subclass slot transfers, Map.groupBy and Set algebra results carry this kind; normal
properties never participate in brand checks. The earlier `__ck` compatibility behavior
allowed a Map to masquerade as a Set, so it was deliberately corrected. New collections
expose no marker property, and a user-created `__ck` property has ordinary JS semantics.
WeakMap and WeakSet has/delete methods now require their distinct internal slots too.
This follows the [ECMAScript keyed collection requirements](https://tc39.es/ecma262/2023/multipage/keyed-collections.html).
Weak entries still use the existing strong storage; this change does not implement GC
weakness.

The hot read/insert helpers compare the stored kind directly, avoiding receiver-property
lookup and temporary string ownership. Shared strong brand checks were extracted into
`builtins/collections/brand.rs` with tests for spoofing, accessor properties, subclasses,
alternate new-target prototypes, factory results, weak compaction and foreign realms.
The standalone spoofing probe now matches Node and Bun: Map.get remains valid after an
ordinary marker write, while Set.add still rejects that Map.

Three rotated full comparisons against `33fd903` measured Map build/lookup at
386.7/240 to 346.7/198 microseconds and Set build/lookup at 413.3/224 to 373.3/173.3.
The first baseline process was an outlier (513.3/420 and 526.7/412), so five further
paired collection runs checked repeatability. Their medians were:

| Workload | Before | After |
|---|---:|---:|
| Map build | 393.3 | 350 |
| Map lookup | 244 | 204 |
| Set build | 413.3 | 380 |
| Set lookup | 224 | 183.3 |

The outlier did not recur. These repeats support about 16–18% lower lookup time and
8–11% lower construction time. Node/Bun in the full comparison measured Map build/lookup
at 190/32.4 and 117.5/31.2, and Set at 146.7/26.8 and 107.8/28. All output checks pass.
Djot measured 4598 to 4658 ms (about 1.3% slower in this run; Node/Bun 228/143), while
DeltaBlue was flat at 9741 to 9703 ms (203/319). There is no application speedup claim,
and the overall goal remains unfulfilled.

The external optimizer directory holds `compare-collection-brands.py`,
`collection-brands-results.json`, `collection-brands-summary.json` and
`collection-brands-repeat.json`. Validation passes 633 unit and 34 integration tests,
813/813 collection conformance cases, and 1996 differential agreements with four budget
skips. Formatting and strict structure audits pass; strict Clippy retains the existing
86/88 diagnostics, none in the modified collection modules.

### Property-storage migration preparation

A fresh Djot sample taken after one second of process CPU time shows cycle collection,
value cleanup, property access and allocation ahead of arithmetic or native-call dispatch.
An earlier sample caught dyld startup and is not evidence about engine execution. The
valid profile is `djot-after-collections-running.sample` in the external optimizer directory;
its run still verified all 3160000 output characters.

Property storage has been extracted from value.rs into modules for map construction,
access/reflection, mutation, dense elements, numeric mirrors, buffer ownership and shapes.
All 51 moved method bodies are unchanged. The public Props facade and probed JIT layout
remain intact. All 633 unit and 34 integration tests pass, formatting and strict module
audits pass, and strict Clippy's diagnostic titles/counts match the existing 86/88 baseline
(some diagnostics now point to the extracted files).

This structural commit does not claim a speedup. The next coordinated change replaces
per-object `(Rc<str>, Property)` entries with shared property-name layouts and contiguous
Property slots, including the native key-probe and creation-cache paths. Reserved constructor
names must stay invisible until their values are initialized; deletion and divergent
constructor paths must detach shared names without affecting another object.

### Rejected shared-name storage experiment

A coordinated experiment replaced each 32-byte `(Rc<str>, Property)` entry with a
16-byte Property slot and a shared name vector. Literal templates, constructor hints,
array length names, RegExp result suffixes and iterator-result templates could share
names. Reserved constructor names were invisible until their value slots were initialized;
deletion and divergent insertion detached the name vector. Native key checks followed
the separate name vector, and native creation wrote only a value slot after checking the
reserved name. An execution counter verified that native creation remained active.

Correctness checks passed 640 unit and 34 integration tests, including new coverage for
reserved-field visibility, descriptor/value isolation, deletion and larger constructor
layouts. Strict Clippy matched the existing 86/88 diagnostic baseline, and module/format
checks passed. Conformance and differential runs were deferred pending the performance
result; these experiments are not retained in production.

The storage rewrite regressed the verified Djot parser in every paired run of the first two
comparisons. A final five-round rotated control compared the original engine, the isolated
iterator-result template change, and the complete rewrite. Median milliseconds:

| Workload | Original | Iterator template only | Complete rewrite |
| --- | ---: | ---: | ---: |
| Djot, 10000 verified parse/render iterations | 4731 | 4734 | 4993 |
| DeltaBlue, 5000 verified iterations | 9801 | 9876 | 10221 |

The complete rewrite was 5.5% slower on Djot and 4.3% slower on DeltaBlue; iterator templates
alone were flat on Djot. Smaller slots did not establish an application benefit. Both
experiments were removed. The external optimizer directory retains the complete source
snapshot (`shared-layouts-experiment.zip`, including untracked modules), patch, three
experimental binaries, the iterator-only control patch, valid profile and raw results.
`layout-control-metadata.json` records binary and workload hashes; `layout-control-results.json`
and `layout-control-summary.json` contain all five final rounds.

The next profile-driven target is the blanket exclusion of class constructors from compiled
execution. Base-class field initialization already occurs before the body in
`run_constructor_on`; derived constructors still require their special this/return handling.
This needs explicit eligibility and correctness checks before any performance claim.

### Compiled base-class constructor bodies

Eligible base-class constructor bodies now enter the bytecode/JIT tiers. The previous blanket
class exclusion kept Djot's large InlineParser and EventParser constructors on the tree-walker,
even after their default parameters and captured arrows became supported. `run_constructor_on`
still initializes instance fields, private members and decorator initializers before the body.
Derived constructors retain their TDZ this binding, super rebinding and return validation on
the existing path. Ordinary class calls and constructor shortcuts keep their separate guards;
a compiled class body does not permit calling that class without new.

Five new all-tier tests cover field/default ordering, captured this, independent instances,
private members, return overrides through super, Reflect.construct prototypes, initializer
errors after warmup, and rejection of ordinary calls to hot compiled classes. The tests inspect
compiled chunks and native code to verify that the intended path executes. All 638 unit and
34 integration tests pass. Forced first-call JIT conformance passes 20956/20958 cases across
expressions, statements, Function and Reflect.construct. The saved pre-change binary has the
same two failures under these settings: lexical-arguments.js and the known errored-cycle
import case. Differential testing agrees on 1996 seeds with four budget skips; formatting,
strict module audit and the existing strict Clippy diagnostic baseline also match.

Three rotated release comparisons on the same M4 measured these median milliseconds:

| Workload | Before | Compiled base constructors | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Djot, 10000 verified parse/render iterations | 4669 | 4008 | 227 | 145 |
| DeltaBlue, 5000 verified iterations | 9797 | 9786 | 201 | 334 |

Djot time falls 14.2%; every run verifies all 3160000 HTML characters. DeltaBlue is flat.
A separate three-round classic V8-v7 comparison is also flat overall (higher scores are better):

| Test | Before | After | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23670 | 23462 | 64338 | 71003 |
| DeltaBlue | 3358 | 3349 | 146504 | 107845 |
| Crypto | 23858 | 23819 | 90088 | 119628 |
| RayTrace | 6240 | 6234 | 133642 | 304061 |
| EarleyBoyer | 3607 | 3624 | 144094 | 157838 |
| RegExp | 1639 | 1632 | 22729 | 30427 |
| Splay | 10243 | 10227 | 80212 | 95418 |
| NavierStokes | 37656 | 37285 | 70193 | 70787 |
| Composite | 8466 | 8456 | 82288 | 98398 |

The collection comparison exposed a tradeoff. Five extra paired repeats measured Map lookup
at 190 to 204 microseconds per 10000 reads (7.4% slower), Map construction unchanged at 346.7,
Set construction 373.3 to 380, and Set lookup 165.7 to 170. Each binary also had one roughly
2x slower process, retained in the raw results. The cause is not established. This change is
retained for its repeatable application improvement; collection lookup remains follow-up work.

The goal is still unmet: the composite is 9.7x behind Node and 11.6x behind Bun; Djot remains
17.7x and 27.6x slower respectively. Only NavierStokes is within 2x of both in this suite.
The external optimizer directory retains `base-constructors-{results,summary,metadata}.json`,
`base-constructors-collections-repeat.json` and the release binary. The sibling
`lumen-engine-comparison-base-constructors` directory contains the full-suite driver, workload,
raw runs, binary/source hashes and medians.

### Bounded integer collection index

Map and Set storage now has an optional direct index for nearby nonnegative integer keys
below 65536. It stores insertion-list slot numbers, not JavaScript values, and grows only
near its current frontier. Sparse, fractional, negative, large and non-numeric keys retain
the hash index. The absent index adds one pointer; allocated slot storage is capped at
256 KiB. Non-numeric reads go directly to the existing hash lookup.

Both indexes refer to the same ordered entries, preserving iteration, deletion/reinsertion,
clear and value ownership. A direct-index miss still checks the hash index: an integer first
inserted sparsely can remain hashed after the direct frontier grows past it. Updates cannot
create duplicate keys. Compaction rebuilds both indexes, and SameValueZero still merges
signed zero and NaN without merging numeric keys with strings, booleans or BigInts.

Validation passes all 642 unit and 34 integration tests, all 813 Map/Set/WeakMap/WeakSet
conformance cases with first-call JIT enabled, and 1996 differential programs across all
three tiers (four resource-budget skips). Formatting and strict module checks pass; strict
Clippy matches the existing 86 library / 88 library-test diagnostics with no additions.

An initial dispatch checked the optional index for every key. The final dispatch checks
the key type first. Three rotated before/after repeats for string, object and fractional
keys put median construction changes between -1.6% and +3.2%, and lookup changes between
-3.3% and +4.2%. Large outliers occurred in both builds and remain in the raw samples.
These results do not establish a non-integer speedup.

Final three-round rotated release medians, microseconds per 10000-operation invocation
(lower is better):

| Integer-key workload | Before | Direct index | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Map construction | 346.7 | 194.3 | 183.3 | 124.0 |
| Map lookup | 208.0 | 160.0 | 32.8 | 32.0 |
| Set construction | 380.0 | 200.0 | 154.0 | 112.5 |
| Set lookup | 177.1 | 135.6 | 26.0 | 28.0 |

Construction time falls 44.0% for Map and 47.4% for Set; lookup time falls 23.1% and
23.5%, respectively. These two construction workloads are within 2x of both other engines.
Lookup remains approximately 5x behind, so this is not parity across collection operations.

The same comparison measured Djot at 4077 to 4122 ms and DeltaBlue at 9775 to 9852 ms,
approximately 1% changes rather than application gains. Node/Bun measured 235/146 ms for
Djot and 196/320 ms for DeltaBlue. Every application run verifies its result. An earlier
comparison had large parser outliers in both builds; its raw samples are also retained.
The classic suite has not been rerun for this collection-only change; its latest composite
gap remains the previously measured 9.7x/11.6x. The overall performance goal remains unmet.

The external optimizer directory contains `dense-collections-{results,summary,metadata}.json`,
the corresponding `dense-collections-other-keys` results and summary, drivers and workload
hashes, and `lumen-dense-collections`. Files prefixed `dense-collections-initial` retain the
earlier dispatch experiment and noisy comparison; they are not the final measurement.

### ASCII code-point reads

String.prototype.codePointAt now shares charCodeAt's guarded native byte-load path for
ASCII receivers and exact, in-bounds integer indices. The call cache still checks the exact
builtin and realm. All other cases call the original named implementation, preserving
Unicode surrogate pairs, coercion order and codePointAt's undefined out-of-bounds result.
No new machine-code emitter or helper ABI is needed for the shared ASCII case.

Three new all-tier tests cover warm ASCII reads, Unicode and index edge cases, coercion,
prototype overrides and foreign error realms. A native-call counter proves that 1000 hot
ASCII reads bypass native dispatch rather than merely producing the correct result through
fallback. All 645 unit and 34 integration tests pass, as do 41 codePointAt/charCodeAt
conformance cases with first-call JIT enabled. Formatting, strict module checks and the
existing 86/88 Clippy diagnostic baseline also match.

Three rotated release runs on the same M4 produced these medians:

| Workload | Before | ASCII code-point path | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| 10000 ASCII code-point reads, microseconds | 370.0 | 208.0 | 12.53 | 9.87 |
| 10000 verified Djot iterations, milliseconds | 4144 | 3957 | 234 | 148 |
| 5000 verified DeltaBlue iterations, milliseconds | 10042 | 10098 | 202 | 318 |

ASCII read time falls 43.8%; Djot improves in every pair, with a 4.5% lower median.
DeltaBlue is approximately flat. This still leaves Djot 16.9x behind Node and 26.7x behind
Bun, and the ASCII microbenchmark itself is far from the 2x target. The external optimizer
directory retains `code-point-{results,summary,metadata}.json`, the comparison driver,
workloads and `lumen-code-point`. The classic suite has not been rerun for this change.

### Strict writes after JIT fast calls

Testing a broader inlining opportunity exposed an existing execution bug: direct JIT calls
can leave the interpreter's strictness flag describing their caller. Slow write helpers then
used the wrong mode when a strict function called a non-strict writer, or vice versa. A
non-strict write to a frozen object could throw merely because its caller was strict.
The saved `lumen-code-point` binary reproduces this failure.

Write helpers now temporarily select strictness from the active compiled frame and inline
source location, restoring the previous state on success and error. Ordinary reads and JIT
call entry do not need additional state transitions. The regression tests cover named and
computed writes, updates, this receivers, read-only array elements and length, unresolved
names, nested calls and restoration after exceptions.

Those tests also exposed a shared interpreter bug: assignment discarded a false [[Set]]
result from a proxy trap. The assignment wrapper now converts that failure into TypeError
in strict code while non-strict assignment remains a no-op. Reflect.set keeps its boolean
result through its separate [[Set]] path. This second failure also reproduces in the saved
baseline, including the interpreter tier. The inlining eligibility experiment is archived
separately in `inline-strictness-initial.zip`; it is not part of this correctness change.

Validation passes 648 unit and 34 integration tests. Forced first-call JIT conformance
passes 21001/21003 cases across expressions, statements, Function, Reflect.construct,
Proxy.set and Reflect.set; only the previously reproduced lexical-arguments and errored-cycle
import failures remain. Differential testing agrees on 1996 programs with four budget skips.
Formatting, strict module audit and the existing 86/88 Clippy diagnostic baseline match.
Some process launches were delayed in dyld before entering engine code; the conformance
worker timeout was increased to 300 seconds for this run, and all final counts above come
from completed runs. No engine benchmark ran concurrently with these validation jobs.

Three rotated release comparisons measured Djot at 4051 to 4096 ms (1.1% higher median)
and DeltaBlue at 11129 to 10858 ms. DeltaBlue varied substantially between pairs; one
post-fix Djot run was also slower at 4545 ms. These measurements do not establish a speedup.
The fix is retained for correct write behavior, with the small observed parser cost recorded
for follow-up. The external optimizer directory retains `write-strictness-{results,summary,
metadata}.json`, the reproduction scripts and `lumen-write-strictness`.

### Rejected strictness-independent leaf inlining experiment

A closed opcode whitelist allowed local-value leaf functions to inline across a strictness
difference. It admitted strict equality, truthiness and control flow while excluding calls,
coercion, property access, free names and receiver reads. Existing activation/arguments guards
remained in force. Tests verified actual native inlining in both directions, callee identity
changes and lexical TDZ error frames. All 651 unit and 34 integration tests passed, as did
the existing conformance and differential baselines; Clippy added no diagnostics.

The same executable supported an environment switch to disable the new eligibility rule.
Three rotated enabled/disabled comparisons reduced a 10000-iteration predicate loop from
433.3 to 333.3 microseconds (23.1%), but Djot medians were 4210/4260 ms and DeltaBlue medians
10232/10323 ms. Five additional Djot pairs measured 4667/4906 ms, with mixed pair directions
and substantial timing variation. Neither comparison established an application benefit.
The experiment was removed from production; its microbenchmark result does not establish
progress toward application parity.

`inline-neutral-experiment.zip` includes both changed source files, including the new module.
The external optimizer directory also retains the binary, patch, source/workload hashes,
three-round results, five-pair Djot repeat and diagnostics showing getEol's new inline.
The prior validated production executable was restored. Further work should address general
loop execution: the existing register-resident loop planner explicitly admits linear loops,
while its separate branch-region paths recognize specific instruction patterns.

### Full-suite recheck after strict-write fixes

Nine fresh-process V8-v7 runs completed and verified their outputs, rotating Lumen, Node
24.18.0 and Bun 1.3.14. Scores varied sharply while unrelated compiler processes and other
applications were active on the machine. Composite scores (higher is better):

| Engine | Median | Observed range |
| --- | ---: | ---: |
| Lumen | 7176 | 5404–7346 |
| Node | 37419 | 37096–75374 |
| Bun | 63474 | 55574–67085 |

These data are too unstable to establish a change in the gap from the earlier comparison;
in particular, Node's lower median is not evidence of a Lumen improvement. Even the fastest
observed Lumen score remains far below half of either other engine's slowest observed score.
The performance goal remains unmet. The sibling `lumen-engine-comparison-write-strictness`
directory retains all nine runs, per-test scores/ranges, source and binary hashes and the
driver. No Lumen builds or validation jobs ran alongside these measurements.

### Numeric loops with general control flow

A new ARM64 backend discovers closed numeric loops from the shared CFG and validates their
SSA graph. It supports forward branches, multiple backedges and nested loops without a fixed
instruction-pattern match. Up to eight private numeric locals keep fixed floating-point
register homes across block edges. Entry guards precede every mutation; every external exit
and bounded continuation writes modified locals back before resuming baseline code. Calls,
handlers, nonnumeric operands and unsupported instructions retain the existing path.

Four colocated all-tier tests verify actual optimized execution as well as branch/continue/
break behavior, continuation after 1024 backedges, NaN/infinity/signed zero, coercion callbacks,
BigInt updates and effectful/exceptional fallback. All 652 unit and 34 integration tests pass.
Forced first-call JIT conformance remains 21001/21003 with the same two known failures.
Differential testing agrees on 1996 programs, with four execution-budget skips.
Formatting, the strict module audit and the existing 86/88 Clippy diagnostic baseline match.

Three rotated same-executable enabled/disabled comparisons produced these medians:

| Workload | Disabled | Enabled | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| 10000 branched numeric iterations, microseconds | 33.6 | 17.2 | 7.10 | 5.30 |
| 10000 nested numeric iterations, microseconds | 33.6 | 16.8 | 4.16 | 5.00 |
| 10000 verified Djot iterations, milliseconds | 3987 | 3943 | 232 | 146 |
| 5000 verified DeltaBlue iterations, milliseconds | 9940 | 9980 | 197 | 315 |

The disabled numeric runs varied significantly (16.6–34.0 and 20.0–34.0 microseconds), so
these medians should not be read as a universal 2x speedup. Enabled runs were stable at
16.8–17.4 and 16.8–17.0 microseconds. Application results are essentially flat; separate
coverage logging shows no Djot loops emitted by this numeric-only backend. This is a
foundation for broader loop lowering, not application parity: Djot remains about 17x behind
Node and 27x behind Bun. No builds or validation jobs ran alongside these measurements.

The external optimizer directory retains `numeric-cfg-{results,summary,metadata}.json`,
`compare-numeric-cfg.py`, `numeric-cfg.js` and `lumen-numeric-cfg`. The environment switch
`LUMEN_JIT_NO_NUMERIC_CFG=1` provides the same-binary control. Guarded array reads and exact
mid-loop state reconstruction are the next extension.

### Read-only numeric arrays in branch graphs

The general numeric CFG backend now lowers local-receiver element reads. Entry guards
require a plain object/array with a coherent, hole-free numeric mirror, then pin the buffer
and length in caller-saved registers. The closed region cannot call helpers, change receiver
slots or write objects, so the borrowed buffers stay valid. Each read checks an exact uint32
index and its bounds; negative zero correctly addresses index zero. Other values and exotic,
holey or accessor-backed receivers keep checked execution.

A failed index guard now reconstructs every live numeric operand and modified local before
resuming the exact baseline opcode. Three additional all-tier tests cover multiple buffers,
branches and continuations, fallback after local updates with live operands, fractional and
nonfinite keys, getters/proxies/typed arrays/holes and receiver reassignment. Test counters
verify actual optimized entry and mid-loop fallback. All 655 unit and 34 integration tests
pass; forced-JIT conformance remains 21001/21003, differential testing agrees on 1996 programs
with four budget skips, and the formatting/module/Clippy baselines match.

An instrumented classic-suite run verified its output and showed the general backend now
emitting numeric array-comparison loops, including a big-integer comparison kernel. Its
scores are not timing evidence: the run enabled bytecode/region logging alongside validation.
`LUMEN_JIT_NO_NUMERIC_ARRAYS=1` independently disables this extension for same-binary controls.

Three rotated same-binary comparisons measured 10000 branched array reads at 35.2 to
17.8 microseconds and nested array reads at 36.8 to 17.6 microseconds (disabled/enabled
medians). Node measured 9.07/7.47 microseconds and Bun 7.60/7.87 microseconds, respectively:
the new path roughly halves Lumen's time but remains outside 2x Bun. Djot was flat at
4267/4271 ms, as was DeltaBlue at 10224/10169 ms.

The classic-suite medians were 8034/7993 versus Node 78441 and Bun 79865, but broad timing
variation prevents a reliable small-change comparison. One enabled run fell to 7366; three
additional paired runs then ranged from 5302–7898 disabled and 7488–7857 enabled. CPU
snapshots during the repeats recorded heavy unrelated Rust compiler and test-runner activity.
No full-suite gain or regression is established, and the roughly 10x composite gap remains.
No Lumen builds or tests ran alongside these measurements.

The external optimizer directory retains `numeric-arrays-{results,summary,metadata}.json`,
`numeric-arrays-v8-repeat-{results,summary}.json` (including CPU snapshots), both comparison
drivers, the workload, the instrumented coverage log and `lumen-numeric-arrays`.

### Numeric CFG branch layout

Numeric CFG edges now have a dedicated lowering module. Forward conditions branch directly
to their non-adjacent successor and use fallthrough when possible; backward edges retain
bounded continuation and local reconstruction. An additional all-tier regression covers
nested continues and bottom-tested loops. All 656 unit and 34 integration tests pass, as do
the existing 21001/21003 conformance, 1996-agreement/four-skip differential, formatting,
module-audit and 86/88 Clippy baselines.

Three same-binary enabled/disabled comparisons show no measured speedup: branched numeric
loops were 18.0/18.0 microseconds, nested numeric loops 18.2/18.4, and both array-loop cases
18.0/18.0 (disabled/enabled). Djot measured 4335/4275 ms and DeltaBlue 10486/10565 ms, with
substantial application timing variation. The change is retained as a branch-lowering
refactor that removes redundant jumps, not as evidence of progress toward engine parity.
The next register-allocation work should remove local/operand-stack copies on the numeric
recurrences rather than assuming branch count is the limiting cost.

`LUMEN_JIT_NO_CFG_FALLTHROUGH=1` preserves the previous edge layout for comparison. The
external optimizer directory retains `cfg-fallthrough-{results,summary,metadata}.json`, its
comparison driver and `lumen-cfg-fallthrough`. No builds or tests overlapped these timings.

### Numeric operand register allocation

Numeric CFG operands now borrow local register homes instead of copying each read onto a
fixed register stack. Temporary registers are reused when their values die. Before a local
is overwritten, any live reads of its old value are preserved together; duplicate values
can share a register until a write requires separation. Numeric definitions followed
immediately by a local store are coalesced into that destination. Array guards still precede
result writes, and side exits reconstruct the actual live register mapping.

Two more all-tier tests cover nested assignments, shared expression values, pre/post updates,
coalesced reads and guard failure with borrowed operands. All 658 unit and 34 integration
tests pass, with the same 21001/21003 conformance, 1996-agreement/four-skip differential and
86/88 Clippy baselines. Formatting and the strict module audit pass. The full debug checks
used `CARGO_INCREMENTAL=0` after sampling showed long incremental-cache hard-link waits on
the external build disk; release compiler settings were unchanged.

Three rotated same-binary comparisons produced these medians (microseconds per 10000
iterations for the first four rows):

| Workload | Fixed registers | Allocated registers | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Branched numeric | 17.8 | 10.2 | 7.30 | 5.60 |
| Nested numeric | 17.8 | 9.33 | 4.32 | 5.30 |
| Branched array reads | 17.8 | 9.07 | 8.93 | 7.60 |
| Nested array reads | 17.8 | 9.33 | 7.33 | 7.73 |
| Djot, milliseconds | 4313 | 4280 | 243 | 243 |
| DeltaBlue, milliseconds | 10375 | 10188 | 205 | 378 |

All four kernels improve in every pair, reducing time by 43–49%. Three are within 2x both
other engines; nested numeric remains 2.16x Node. Applications are essentially flat with
mixed pair directions, so the broader engine goal is still unmet. The classic suite was not
rerun for this allocator change. No Lumen builds or tests overlapped these measurements.

`LUMEN_JIT_NO_CFG_REGALLOC=1` selects the previous fixed-register emitter in the same binary.
The external optimizer directory retains `cfg-regalloc-{results,summary,metadata}.json`,
the comparison driver and `lumen-cfg-regalloc`.

### Numeric region inputs

Closed numeric CFG regions now guard and pin up to six numeric inputs from enclosing
bindings or own data properties, including `this` fields and array length. Live property
cache ways are checked at entry; array named slots additionally check their keys because
numeric element changes can move those slots. Accessors, proxies, inherited properties,
coercion and failed guards retain baseline execution. Regions contain no calls or object
writes, and side exits reload inputs before reentry.

All 662 unit and 34 integration tests pass, including changed closure bindings, shifted
array slots and a mid-loop getter that changes both a field and the loop bound. Conformance
remains 21001/21003, differential testing reports 1996 agreements and four budget skips,
and Clippy retains the same 86/88 baseline. Formatting and the strict module audit pass.

Three rotated same-binary comparisons produced these medians. Numeric rows report
microseconds per 10000 iterations; application rows report milliseconds.

| Workload | Inputs disabled | Inputs enabled | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Numeric fields | 69.41 | 7.60 | 8.67 | 6.50 |
| Enclosing numbers | 36.40 | 7.60 | 7.47 | 7.33 |
| Djot | 3949 | 3914 | 228 | 146 |
| DeltaBlue | 9758 | 9841 | 199 | 308 |

Both numeric kernels improve in every pair and are within 1.17x both other engines.
Applications remain essentially flat: Djot is still 17–27x slower, and DeltaBlue 32–49x
slower. These kernels demonstrate useful coverage, not overall engine parity. No Lumen
builds or tests overlapped the timings; the classic suite was not rerun for this change.

`LUMEN_JIT_NO_CFG_INPUTS=1` disables this extension. The external optimizer directory
retains `cfg-inputs-{results,summary,metadata}.json`, `compare-cfg-inputs.py`, the verified
workloads and `lumen-cfg-inputs`.

### Object-edge scanning

A fresh Djot sample still showed collection and property ownership work among the larger
native costs. Heap edge enumeration now lives in `value/gc_edges.rs`, where packed object
tags are checked directly. Scalar properties are no longer widened and cloned merely to
reject them as graph edges, and packed-array hole filtering uses the empty tag directly.
Every physical object reference remains counted, including duplicate value/getter/setter
owners; the source borrow is released before following self references.

All 665 unit and 34 integration tests pass. New tests cover duplicate edge ownership,
self references, non-object payloads, and accessor storage without an accessor flag.
Clippy matches the existing 86/88 baseline, and formatting and the strict new-module audit
pass. This change does not alter JavaScript execution or collector root selection.

Three alternating application comparisons are effectively flat: Djot medians are
3980 ms before and 4008 ms after; DeltaBlue 9835 ms before and 9875 ms after. Both workloads
have mixed pair directions, with visible host timing variation. This is retained as a small
collector refactor, not an established speed gain. No builds or tests overlapped timings.
The external optimizer directory retains `gc-edges-{results,summary,metadata}.json`, the
comparison driver, `lumen-gc-edges` and the fresh `djot-cfg-inputs.sample`.

### Rejected forwarded-call cache experiment

A mapped DeltaBlue profile attributed 72.3% of exclusive samples to generated code, with
32.3% in the chunk consistent with `Plan.execute`. Property/method-read templates account
for roughly 31% overall. All anonymous addresses matched unique emitted ranges. The external
`delta-mapped.{maps,sample,out,summary.json}` and `profile-mapped-delta.py` preserve the
same-process mapping; fused operations and shared tails remain coarse attributions.

A separate experiment added a lazy identity cache for `Function.prototype.call` targets,
reusing existing call-cache lifetime, epoch, realm and ownership checks. All 668 unit and
34 integration tests passed, with unchanged conformance, differential and Clippy baselines.
However, three rotated comparisons consistently regressed focused calls: 460 to 630 us and
470 to 650 us per 10000 forwarded calls. DeltaBlue was flat (9875 to 9870 ms), and Djot's
4033 to 3971 ms medians had mixed pair directions and host variation. Node/Bun can optimize
these simple forwarded-call kernels much further (roughly 2.4–3 us).

The cache-hit assembly adds a 304-byte frame, a 104-byte cache-entry copy, inline accounting
and a separate committed-call layer while preserving the original activation setup. Those
costs plausibly outweigh the skipped guards for simple targets. The experiment is removed;
there is no production forwarded-cache module or flag. Its source archive, executable,
comparison driver and `forwarded-cache-{results,summary,metadata}.json` remain in the external
optimizer directory. No builds or tests overlapped its timings.

### Eliding temporary owners for inlined methods

The mapped profile exposed repeated `GetMethod → InlineGuard → Pop` sequences. For eligible
zero-argument calls, the JIT now checks the live packed method identity directly after the
existing property guards, preserving environment and receiver checks. Success bypasses the
temporary method clone, stack write/reload and drop. Original templates and entry labels
remain available on misses, and the new continuation is explicitly targeted.

Three colocated all-tier tests confirm actual execution and cover replacement, accessors,
prototype changes, proxies, closure environments, primitive receivers and collection after
removing the active method's property. All 668 unit and 34 integration tests pass, with the
same 21001/21003 conformance, 1996-agreement/four-skip differential and 86/88 Clippy baselines.
Formatting and the strict new-module audit pass.

Three rotated same-binary comparisons produced these medians. Method rows are microseconds
per 10000 invocations; applications are milliseconds.

| Workload | Disabled | Enabled | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Inherited method | 100 | 92 | 2.75 | 2.36 |
| Own method after inherited warmup | 216.67 | 220 | 2.75 | 2.35 |
| Djot | 3931 | 3932 | 233 | 146 |
| DeltaBlue | 9786 | 9281 | 199 | 306 |

DeltaBlue improves in every pair (9745→9281, 9791→9269, 9786→9305), reducing median time by
5.2%. Inherited calls improve consistently by 8%. The own-method case has no established
gain and one enabled outlier at 313 us; Djot is flat. DeltaBlue remains roughly 47x Node and
30x Bun, so this is measured incremental progress, not engine parity.

`LUMEN_JIT_NO_INLINE_METHOD=1` disables the bypass. The external optimizer directory retains
`inline-method-{results,summary,metadata}.json`, the comparison driver/workload and
`lumen-inline-method`. No builds or tests overlapped these timings.

### Current full-suite comparison after retained changes

Three rotated classic V8 version 7 runs on September 7, 2026 use retained commit `24cfb1b`,
Node 24.18.0 and Bun 1.3.14. Scores are higher-is-better; each cell is the median of three
runs. This snapshot includes the numeric CFG/register/input work and method-owner bypass,
and excludes the rejected forwarded-call cache.

| Benchmark | Lumen | Node | Bun |
| --- | ---: | ---: | ---: |
| Richards | 23488 | 65591 | 72563 |
| DeltaBlue | 3551 | 154200 | 108400 |
| Crypto | 23812 | 91784 | 119825 |
| RayTrace | 6290 | 135788 | 302582 |
| EarleyBoyer | 3623 | 146866 | 155680 |
| RegExp | 1641 | 22319 | 30548 |
| Splay | 10203 | 77653 | 95467 |
| NavierStokes | 37766 | 68789 | 60190 |
| Composite | 8558 | 82895 | 97052 |

The composite remains 9.69x below Node and 11.34x below Bun. Lumen's three scores were
8558, 8559 and 8544; Node ranged 79457–83305 and Bun 95813–99039. NavierStokes is within
2x both engines, but the overall goal remains unmet. This is a current comparison, not an
attribution of aggregate improvement against older binaries measured under different host
conditions. The same-binary DeltaBlue comparison above establishes the latest 5.2% gain.

All nine runs exited successfully with all eight benchmark scores. No Lumen builds, tests
or profiling runs overlapped the timings. The external optimizer directory retains
`current-v8-{results,summary,metadata}.json` (including host CPU snapshots),
`compare-current-v8.py` and the exact `lumen-current-v8` executable.

### Forwarding adjacent last-use stores and loads

Mapped `Plan.execute` already moves its last-use receiver out of the local slot, including
after inline expansion. The remaining adjacent `StoreLocal; LoadLocal` pair now retains the
new owner on the operand stack when existing liveness proves the load is a last use. The
old slot owner is still destroyed correctly, internal Empty retains the original TDZ path,
and independently targeted loads are excluded. The continuation is explicitly targeted.
Store lowering now lives in `jit/local_store.rs`.

All 671 unit and 34 integration tests pass, including old object/BigInt destruction,
reference payloads, captures, handlers and TDZ cases. Conformance remains 21001/21003,
differential testing 1996 agreements/four budget skips, and Clippy the same 86/88 baseline.
Formatting and the strict new-module audit pass.

Three rotated same-binary comparisons were flat on numeric/object transfer kernels
(52.22 and 52.00 us per 10000 transfers with either mode) and Djot (3915→3931 ms).
DeltaBlue improved in each pair: 9838→9408, 9343→9247, 9295→9130 ms. Its median reduction
is a modest 1.0%; the first pair also shows larger host variation. This is incremental
application progress, not a general throughput breakthrough. Current Node/Bun medians for
DeltaBlue were 200/316 ms, so the overall goal remains far away.

`LUMEN_JIT_NO_STORE_LOAD_FORWARD=1` disables the forwarding. The external optimizer directory
retains `store-forward-{results,summary,metadata}.json`, its driver/workload and
`lumen-store-forward`. No builds or tests overlapped these timings.

### Rejected standalone borrowed property chains

A two-read prefix borrowed an intermediate own-property object and decoded/cloned only the
final value. Both live cache ways and warmed own-shape hints were guarded; accessors,
proxies, inherited properties and unsupported values resumed the untouched bytecodes.
The revised experiment passed 675 unit and 34 integration tests, including actual warmed
hint execution, with unchanged conformance, differential and Clippy baselines.

Three rotated same-binary comparisons nevertheless showed a mixed tradeoff:

| Workload | Disabled | Enabled | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Own field chain (us/10000) | 81.67 | 87 | 2.62 | 2.27 |
| Array length chain (us/10000) | 97 | 91 | 2.65 | 2.25 |
| Djot (ms) | 3861 | 3863 | 225 | 142 |
| DeltaBlue (ms) | 9193 | 9062 | 195 | 302 |

Ordinary field reads regressed in every pair; their median time increased 6.5%. The array
case improved 6.2%, DeltaBlue improved 1.4%, and Djot was flat. The standalone prefix was
removed rather than retaining this general field-read regression. The external optimizer
directory preserves `property-chain-v2-source.zip`, `lumen-property-chain.v2` and the
`property-chain-{results,summary,metadata}.json.v2` artifacts. No builds or tests overlapped
the timings. The shared own-entry probe remains a reusable prerequisite for larger numeric
expressions; extracting that probe is not itself an established application speedup.

### Borrowed nested numeric expressions

`jit/numeric_expr` plans short, acyclic expressions containing object-valued property
intermediates and arithmetic. It borrows rooted objects in general-purpose registers,
keeps numbers in floating-point registers, and publishes only the operands needed by an
existing return, inline-return jump, local store or property store. Every guard failure
replays the untouched original expression before any VM-state or ownership change.
Getters, proxies, inherited reads and coercions therefore retain their original behavior;
existing terminal instructions retain writes, strictness, error handling and cleanup.

The planner requires a property result used as another property's receiver. A destination
alone does not qualify, and direct numeric fields stay with the existing numeric-chain
backend. It admits no calls, branches or writes inside the expression. Own-entry probes
check live receiver kind/plainness, cache shape/slot, descriptor kind and value type; array
slots additionally validate names. Shapes encode key order, not descriptor attributes, so
matching a shape does not remove the descriptor guard.

Ten colocated tests cover actual execution, aliases, numeric edge values, live descriptor
and prototype changes, setters/proxies, strict failures, handlers/captures, and warmed hints.
The warmed tests caught inline returns lowering to jumps; stopping before the original jump
now preserves that useful expression boundary without replacing jump semantics.

`LUMEN_JIT_NO_NUMERIC_EXPR=1` disables the expression path. The shared own-entry probe also
serves existing numeric-CFG inputs; the standalone two-read chain experiment remains removed.

Validation passes 681 unit and 34 integration tests, with 21001/21003 conformance and
1996 differential agreements/four execution-budget skips. Clippy has exactly the existing
86 library/88 library-test error baseline; formatting and the strict five-file module audit
pass. Host filesystem delays stalled executable loading and dependency-directory scanning.
The byte-identical conformance runner completed from local disk, and the zero-test rustdoc
stage completed separately with its unnecessary dependency-directory scan omitted. Engine
source and validation inputs were unchanged. Timing uses a hash-verified local copy of the
archived release executable for the same reason.

Three rotated same-binary comparisons produced these medians. Kernels are microseconds per
10000 invocations; application rows are milliseconds.

| Workload | Disabled | Enabled | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: |
| Nested numeric return | 132.50 | 132 | 4.75 | 2.27 |
| Nested numeric write | 213.33 | 210 | 2.80 | 9.73 |
| Numeric fields in branch graphs | 7.60 | 7.60 | 8.67 | 6.30 |
| Enclosing numbers in branch graphs | 7.60 | 7.60 | 7.47 | 7.20 |
| Djot | 3890 | 3901 | 231 | 146 |
| DeltaBlue | 9236 | 9067 | 195 | 309 |

DeltaBlue improves in every pair (9210→9067, 9236→9042, 9264→9165 ms), reducing median
time by 1.8%. The nested write kernel has a modest 1.6% median reduction; nested returns,
existing numeric kernels and Djot have no established gain. Djot pairs are mixed and the
third round shows broader host variation. DeltaBlue remains 46.5x Node and 29.3x Bun; this
is incremental progress, not a breakthrough. The current zero-depth entry restriction can
exclude inlined arithmetic with an existing operand-stack prefix; relaxing it requires
separate ownership/fallback coverage and measurement.

All 48 runs exit successfully and verify their outputs. No builds, tests or profiling runs
overlap timings. The external optimizer directory retains `numeric-expr-{results,summary,
metadata}.json`, drivers/workloads and `lumen-numeric-expr`; the metadata also identifies its
byte-identical local execution copy.

### Full-suite comparison including nested numeric expressions

Three rotated runs on September 7, 2026 use retained commit `2bf3b7d`, Node 24.18.0 and
Bun 1.3.14. This snapshot includes adjacent store/load forwarding and nested numeric
expressions; the rejected standalone property-chain prefix is absent. Classic V8 version 7
scores are higher-is-better; cells are medians of three runs.

| Benchmark | Lumen | Node | Bun |
| --- | ---: | ---: | ---: |
| Richards | 23571 | 65898 | 72627 |
| DeltaBlue | 3597 | 151833 | 109928 |
| Crypto | 23853 | 91997 | 119717 |
| RayTrace | 6252 | 137268 | 305393 |
| EarleyBoyer | 3638 | 148206 | 158468 |
| RegExp | 1630 | 22843 | 30821 |
| Splay | 10252 | 79845 | 94481 |
| NavierStokes | 37804 | 70935 | 71161 |
| Composite | 8587 | 83560 | 99325 |

The composite gap is 9.73x to Node and 11.57x to Bun. Lumen's scores were 8587, 8587 and
8532. NavierStokes remains within 2x both engines (1.88x each); overall proximity remains
unmet. The separate verified Djot workload above remains 16.9x Node and 26.7x Bun, and the
5000-iteration DeltaBlue workload remains 46.5x/29.3x. Those standalone elapsed-time ratios
are distinct from the classic suite's scores and calibration.

The earlier composite of 8558 and this 8587 snapshot were taken under different host
conditions, so their small difference is not an attributed aggregate gain. The paired
9236→9067 ms DeltaBlue experiment establishes the latest 1.8% improvement. All nine full
runs exited successfully with eight benchmark scores and a composite, with no builds,
tests or profiling runs overlapping. The external optimizer directory retains
`numeric-expr-v8-{results,summary,metadata}.json` (including host CPU snapshots), its driver,
and the hash-verified release executable identified in the metadata.

### Compact own-property hints and pending operand prefixes

Numeric expressions now accept any known CFG entry depth. Their checked relative-stack
operations cannot consume existing operands, and successful emission appends exactly the
original terminal operands above the untouched prefix. Tests cover pending numeric and
owned/coercible operands, pending setter destinations, and getter-triggered GC on fallback.

The prefix-only experiment passed 683 unit/34 integration tests and unchanged conformance,
differential and Clippy baselines. Its three-round application medians (Djot 3972→3955 ms,
DeltaBlue 9227→9142 ms) and kernel results did not establish a useful general speedup.
Mapped code proved that the actual inlined return loop had selected the new region: its
first property-read span grew from 504 to 5820 bytes, adding 5316 bytes to the chunk. Write
paths already entered at depth zero and were unchanged. A benchmark-shaped execution test
then confirmed 42000 successful prefix commits and 252000 warmed property-hint hits, ruling
out repeated guard failure as the explanation for the flat return timing.

Warmed own-property probes now check their known exotic kind directly and address the
entry by a constant offset. They still validate live plainness, shape, entry bounds, data
descriptor and value type; array hints still check the live entry key. A miss restores the
borrowed stored-Rc receiver and probes all live cache ways. This removes dynamic mode
selection and slot multiplication from successful warmed hints without changing ownership.

Four added tests bring validation to 685 unit and 34 integration tests, with 21001/21003
conformance, 1996 differential agreements/four budget skips and the same 86/88 Clippy error
baseline. All 37 focused JIT tests pass, including dedicated actual array-hint execution
and warm descriptor/shape/key mutations. Formatting and strict module audits pass.

Three rotated same-binary comparisons distinguish the retained behavior, the prefix-only
extension, and the combined compact-hint/prefix path. Kernels are microseconds per 10000
invocations; applications are milliseconds.

| Workload | Retained behavior | Prefix only | Compact + prefix | Node 24.18.0 | Bun 1.3.14 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Nested numeric return | 132.50 | 130 | 102 | 4.75 | 2.27 |
| Nested numeric write | 207.50 | 207.50 | 182.50 | 2.83 | 10 |
| Numeric fields in branch graphs | 7.47 | 7.60 | 7.47 | 8.67 | 6.40 |
| Enclosing numbers in branch graphs | 7.60 | 7.60 | 7.60 | 7.47 | 7.33 |
| Djot | 3900 | 3934 | 3900 | 230 | 145 |
| DeltaBlue | 9079 | 9123 | 9111 | 200 | 314 |

The combined path reduces nested-return time 23.0% and nested-write time 12.0%, consistently
across all three pairs. Existing numeric kernels and Djot are flat. Standalone DeltaBlue
is 0.35% slower by median, with all three combined-path runs slightly slower than their
retained-behavior counterparts; these results do not establish an application speedup.

`LUMEN_JIT_NO_NUMERIC_EXPR_PREFIX=1` restores the zero-depth selection restriction.
`LUMEN_JIT_NO_COMPACT_PROPERTY_HINT=1` retains the previous warmed-hint instruction sequence.
Both flags together reproduce the retained behavior for the comparisons above. All 60 runs
exit successfully and verify their results, without overlapping builds, tests or profiling.
The external optimizer directory preserves `numeric-prefix-*`, the mapped-code diagnosis,
`compact-hint-{results,summary,metadata}.json`, comparison drivers and exact saved executables.

A further three rotated classic V8 version 7 pairs compare the retained behavior (both
optimizations disabled) against the combined path. Scores are higher-is-better:

| Benchmark | Retained behavior | Compact + prefix |
| --- | ---: | ---: |
| Richards | 23438 | 23436 |
| DeltaBlue | 3573 | 3573 |
| Crypto | 23251 | 24031 |
| RayTrace | 6259 | 6339 |
| EarleyBoyer | 3564 | 3608 |
| RegExp | 1643 | 1645 |
| Splay | 10374 | 10309 |
| NavierStokes | 38139 | 38211 |
| Composite | 8565 | 8600 |

Composite medians increase 0.4%, from 8565 to 8600. Individual composite pairs are
8510→8600, 8565→8616 and 8565→8572; the last difference is very small. The suite therefore
shows a modest measured change alongside the much clearer kernel gains, not a broad
throughput breakthrough. Splay's median is slightly lower with mixed pairs; the other
median scores are flat or higher. All six runs verify all scores and exit successfully,
without overlapping builds, tests or profiling. `compact-hint-v8-{results,summary,metadata}.json`
and its driver preserve the comparison, including host CPU snapshots. These are same-engine
on/off measurements, not a new full-suite Node/Bun comparison.


### Rejected feedback-only hot-function recompilation

An experiment allowed the existing one-shot second compilation to proceed with an empty
inline plan when at least two distinct property reads held monomorphic own-property
feedback. It reused cache seeding, guarded emission, old-code ownership and call-cache epoch
invalidation. `LUMEN_JIT_NO_FEEDBACK_RECOMPILE=1` selected the retained policy in the same
saved executable; `LUMEN_JIT_NO_CACHE_SEED` also suppressed the new eligibility branch.

The experiment passed 689 unit and 34 integration tests, including live property mutations,
ordinary nonempty inline plans, the one-read exclusion, and distinct closures sharing an AST
across GC. The closure test explicitly asserted feedback-version publication; a named
function expression initially failed that coverage assertion because it never entered the
JIT. An anonymous returned closure exercised the intended path and passed. Conformance stayed
at 21001/21003 with the same two failures, differential testing produced 1996 agreements and
four budget skips, and Clippy retained exactly the same 86/88 error multiset. Formatting and
the strict module audit passed.

Three rotated rounds compared the same executable with the new policy disabled/enabled,
Node 24.18.0 and Bun 1.3.14. All 48 application/kernel runs verified their results. Kernels
are microseconds per 10000 invocations; applications are milliseconds.

| Workload | Retained policy | Feedback only | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Nested numeric return | 102 | 102 | 4.85 | 2.36 |
| Nested numeric write | 182.50 | 182.50 | 2.83 | 9.90 |
| Numeric fields in branch graphs | 7.60 | 7.60 | 8.67 | 6.50 |
| Enclosing numbers in branch graphs | 7.60 | 7.60 | 7.47 | 7.33 |
| Djot | 3960 | 3956 | 228 | 144 |
| DeltaBlue | 9148 | 9257 | 210 | 327 |

Djot's 0.1% median difference is effectively flat and has mixed pairs. DeltaBlue is 1.2%
slower by median, with every pair slower: 9067→9087, 9276→9773 and 9148→9257 ms. A separate
untimed tier-log run confirmed nine feedback-only publications in DeltaBlue, so the policy
was exercised. These measurements do not establish an application benefit.

Twelve further sequential runs measured the classic suite with the same three-round
rotation. Scores are higher-is-better:

| Benchmark | Retained policy | Feedback only | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23564 | 23409 | 65573 | 71949 |
| DeltaBlue | 3603 | 3577 | 153652 | 106701 |
| Crypto | 23954 | 23966 | 92093 | 119277 |
| RayTrace | 6313 | 6339 | 135788 | 304283 |
| EarleyBoyer | 3615 | 3578 | 147380 | 157702 |
| RegExp | 1643 | 1633 | 22866 | 30154 |
| Splay | 10260 | 10252 | 79918 | 94432 |
| NavierStokes | 38025 | 38063 | 70193 | 69678 |
| Composite | 8583 | 8592 | 83657 | 98128 |

Composite medians differ by only +0.1%, with mixed pairs: 8589→8610, 8583→8592 and
8533→8372. Richards, DeltaBlue and RegExp are lower in every paired run. Host load varied,
so these small score differences do not establish a general improvement. The policy and its
feature-specific tests were removed; production code returned exactly to the previously
validated compact-hint implementation, with 685 unit and 34 integration tests.

The retained-policy control includes the compact-hint/prefix changes and refreshes the full
Node/Bun comparison: composite gaps are 9.75× Node and 11.43× Bun. NavierStokes remains within
2× both (1.85×/1.83×), while Djot is 17.37×/27.50× and standalone DeltaBlue 43.56×/27.98×.
These remain far from the overall within-2× objective; the differing ratios across dated
snapshots should not be interpreted as a cumulative speedup.

All timings ran without overlapping Lumen builds, tests or profiling. The external optimizer
directory preserves `feedback-recompile-{results,summary,metadata,validation}.json`,
`feedback-recompile-v8-{results,summary,metadata}.json`, drivers, the exact executable and
`feedback-recompile-source.zip`. Classic metadata includes host CPU snapshots. Executable,
workload and source hashes were checked before removing the experiment.

A static follow-up identified a possible limitation: compact property-load hints send a
shape miss directly to the checked helper, even after the live cache learns another shape.
Falling back through that live cache is a separate, unmeasured candidate; the current data
does not prove stale hints caused the regression. An audit of existing application profiles
and tier logs also found little direct interpreter execution, providing no evidence that
broader syntax compilation alone would close the application gap.


### Rejected live-cache fallback for compact property hints

A separate experiment kept compact property-load hits but routed their pre-load guard misses
through the site's live four-way cache before the checked helper. It reused the existing
prototype, absence and array-key checks, with no ownership or operand-stack changes during
probing. Descriptor, bounds and value-decoding failures still went directly to the helper.
The general probe was extracted into a small module. This experiment used the retained
recompilation policy, not the rejected feedback-only policy above.

The first implementation embedded the fallback at each compact site. Three rotated rounds
(60 verified runs) cut alternating own-read time 25.20→11.20 microseconds per 1000 reads and
alternating inherited-method time 47.20→14.80. However, Djot increased 3929→3974 ms and
DeltaBlue 9134→9238 ms, both about 1.1% slower by median, with mixed pairs. This motivated an
outlining experiment rather than immediate retention.

The outlined version queued these fallback bodies after the main code and teardown stub.
Compact guard failures reached a nearby unconditional-branch trampoline, keeping new long
conditional-branch relaxation out of the opcode stream. The outlined probes returned to the
same native load/commit or checked-helper labels. Generic sites without a compact hint kept
their original inline probes. A test forced far cold branches to relax while checking that
hot instruction positions remained unchanged.

Both versions passed full validation: 687 unit/34 integration tests for inline fallback,
then 688/34 with outlining. Tests asserted successful native cache decoding after an actual
compact miss and covered new receiver/prototype shapes, descriptors, absence, getter GC,
string/array method receivers and method replacement. Conformance stayed at 21001/21003,
differential testing at 1996 agreements/four budget skips, and Clippy at the identical 86/88
error multiset. Formatting and strict audits passed for the extracted modules.

Another 75 verified runs compared three modes of one saved executable, Node 24.18.0 and
Bun 1.3.14. The first four rows are microseconds per 1000 reads; the next four are
microseconds per 10000 invocations; applications are milliseconds.

| Workload | Retained behavior | Inline fallback | Outlined fallback | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Own read, stable | 9.61 | 9.76 | 9.47 | 0.62 | 0.88 |
| Own read, alternating | 16.20 | 11.20 | 11.20 | 0.89 | 1.32 |
| Inherited method, stable | 11.60 | 12.00 | 11.60 | 0.63 | 0.90 |
| Inherited method, alternating | 25.20 | 14.80 | 15.20 | 1.20 | 1.48 |
| Nested numeric return | 104.00 | 102.00 | 102.00 | 4.31 | 2.31 |
| Nested numeric write | 183.33 | 185.00 | 182.50 | 2.86 | 10.00 |
| Numeric fields | 7.73 | 7.60 | 7.60 | 8.80 | 6.50 |
| Enclosing numbers | 7.60 | 7.60 | 7.73 | 7.47 | 7.33 |
| Djot | 3926.00 | 3943.00 | 3951.00 | 229.00 | 144.00 |
| DeltaBlue | 9222.00 | 9232.00 | 9097.00 | 198.00 | 316.00 |

Outlined alternating-own reads improve 30.9% and alternating inherited methods 39.7% by
median. Every corresponding pair improves, by 30–55% and 39–67% respectively. The retained
behavior varies substantially between runs on these kernels, so the larger
percentages from the initial comparison are not interchangeable with this comparison.
Stable inherited-method reads and the established numeric kernels are essentially unchanged.

The application result remains inconclusive: outlined Djot is 0.64% slower by median, with
pairs 3926→3916, 3954→3959 and 3920→3951 ms. DeltaBlue is 1.36% faster by median, with mixed
pairs 9309→9097, 9222→9261 and 9192→9085 ms. These results do not establish a parser gain.

A fixed-count, result-verified map capture matched chunks by exact slot names, opcode
sequences and occurrence. Across all captured chunks, total code size was 26136/27828/27840
bytes for retained/inline/outlined modes. In the inlined-method chunk, the first-opcode-start
to last-opcode-start extent changed 5100→6164→5108 bytes, while total chunk size changed
5660→6724→6732. Outlining moved fallback bytes out of the opcode stream; it did not eliminate
their allocation. These extents exclude the final opcode and tail and do not measure dynamic
hotness or prove the cause of an application timing difference.

A final six-run, three-pair classic-suite comparison measured retained behavior versus
outlined fallback. Scores are higher-is-better:

| Benchmark | Retained behavior | Outlined fallback |
| --- | ---: | ---: |
| Richards | 23553 | 23638 |
| DeltaBlue | 3584 | 3551 |
| Crypto | 24045 | 24040 |
| RayTrace | 6240 | 6221 |
| EarleyBoyer | 3632 | 3579 |
| RegExp | 1646 | 1634 |
| Splay | 10243 | 10398 |
| NavierStokes | 38173 | 38249 |
| Composite | 8588 | 8567 |

The composite decreases 0.24%, from 8588 to 8567, with all pairs lower: 8602→8567,
8588→8567 and 8572→8569. This is a small difference, but the experiment establishes no
improvement in either the suite composite or parser target. The strong alternating-shape
kernel gains therefore do not justify enabling it as progress toward the current within-2×
objective. Both fallback implementations, their extraction and their feature-specific tests
were removed. Production returned exactly to the previously validated retained implementation.

The saved experiment uses `LUMEN_JIT_NO_COMPACT_LIVE_PIC=1` for retained behavior,
`LUMEN_JIT_NO_OUTLINE_COMPACT_PIC=1` for inline fallback, and neither for outlined fallback.
These flags are not retained production features. All timings ran without overlapping Lumen
builds, tests or profiling. The external optimizer directory preserves `compact-live-pic-*`
and `compact-outlined-pic-*` results, summaries, metadata, validation, source ZIPs and exact
executables, including `compact-outlined-pic-v8-*` and `compact-outlined-pic-maps.*` artifacts.
Hashes were checked before removing the experiment. The next proposed direction is guarded
mixed object/numeric regions with branches and numeric-field writes; its static design is
preserved separately as `mixed-object-region-design.md`, not claimed as implemented or faster.

### Guarded numeric branches and field writes (September 7, 2026)

The experiment extends native numeric regions across bounded acyclic comparisons and
branches. Every path ends with one existing ordinary named numeric-field write and joins
the same original bytecode continuation. Borrowed object roots and numeric expressions
stay in registers. All fallible checks precede the write, so a guard miss can restart the
original region without replaying an observable effect. Original bytecode entry points
remain available. The region accepts live guarded lexical/global roots, including object
roots such as DeltaBlue's Direction constant object.

The commit excludes numeric property keys, exotic receivers, accessors, non-writable
properties and nonnumeric old values. This avoids dense-element mirror changes and
object-owner release. NaNs are canonicalized before the packed store. Comparison emission
preserves unordered-number semantics. Existing VM operand prefixes remain rooted and
untouched through both success and fallback. The bounded planner admits at most 64 visited
operations, fewer than four branch levels, eight object homes and sixteen numeric homes
per expression. It does not yet optimize complete mixed object/numeric loops.

Seven new tests exercise actual native success, aliases, NaN comparisons, global replacement,
getters and coercion, strict failures, different closure environments, fresh TDZ bindings,
the eighth object register across name lookup, and an owned operand prefix during getter GC.
Validation passes 692 unit and 34 integration tests; conformance remains 21001/21003 with the
same two known failures. Differential testing reports 1996 agreements and four budget skips.
Clippy has the same 86 library/88 library-test baseline errors; formatting and the strict
six-module structure audit pass.

Three rotated rounds against Node 24.18.0 and Bun 1.3.14 produced 60 result-verified runs.
The first six rows are microseconds per 10000 invocations; applications are milliseconds.

| Workload | Region disabled | Region enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Field diamond | 236.67 | 203.33 | 37.80 | 11.80 |
| Nested diamond | 313.33 | 260.00 | 15.00 | 44.00 |
| Nested numeric return | 102.00 | 102.00 | 4.31 | 2.31 |
| Nested numeric write | 182.50 | 180.00 | 2.83 | 9.90 |
| Numeric fields | 7.60 | 7.60 | 8.67 | 6.50 |
| Enclosing numbers | 7.60 | 7.73 | 7.47 | 7.20 |
| Djot | 3939 | 3944 | 232 | 143 |
| DeltaBlue | 9163 | 9124 | 202 | 305 |

Field and nested diamonds reduce median time by 14.1% and 17.0%, respectively; all pairs
improve. DeltaBlue improves only 0.43%, with pairs 9072→9015, 9163→9124 and 9183→9150 ms.
Djot is effectively flat: 3878→3857, 3939→3960 and 3944→3944 ms. The enabled parser remains
17.0× Node and 27.6× Bun. The separate Delta workload remains 45.2× Node and 29.9× Bun.
These kernel wins do not establish a material reduction in the overall engine gap.

An untimed generation diagnostic confirms a region spanning DeltaBlue PCs 102–127 and
joining at 128. Generation alone does not prove runtime guard success in that application.
The diagnostic timing is excluded because it overlapped validation. All measured timing
runs exclude concurrent Lumen builds, tests and profiling. External optimizer artifacts
use the guarded-write-region prefix and preserve source, executable hashes, validation logs,
raw results, summaries and provenance. LUMEN_JIT_NO_GUARDED_WRITE_REGION=1 selects the control.

A further 12-run, three-rotation classic-suite comparison includes the enabled implementation.
Scores are higher-is-better:

| Benchmark | Region disabled | Region enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23684 | 23564 | 65619 | 72874 |
| DeltaBlue | 3597 | 3626 | 153724 | 111521 |
| Crypto | 24014 | 24001 | 92145 | 119348 |
| RayTrace | 6332 | 6313 | 136602 | 307909 |
| EarleyBoyer | 3639 | 3634 | 148039 | 159297 |
| RegExp | 1643 | 1635 | 22866 | 30579 |
| Splay | 10520 | 10374 | 81369 | 95296 |
| NavierStokes | 38436 | 38617 | 71232 | 70123 |
| Score (version 7) | 8635 | 8644 | 84073 | 98790 |

The composite changes 8635→8644 (+0.10%), with mixed pairs 8799→8668, 8635→8644 and
8601→8600. This is effectively flat. Splay loses 1.39% by median and RayTrace loses 0.30%;
RayTrace decreases in every pair, so the implementation is not a universal improvement.
The enabled composite remains 9.73× behind Node and 11.43× behind Bun. NavierStokes is
within 2× both, but the suite composite and parser are still far outside the target.

The implementation is retained for its guarded branch/write capability, consistent focused
kernel improvements and small three-pair Delta application gain. No material overall
speedup is claimed. The next extension needs exact exits after earlier writes have committed;
restarting the original region entry would replay effects. A reviewed bounded design is
saved externally as guarded-write-exit-design.md; it is not yet implemented.

### Exact exits across consecutive numeric writes (September 7, 2026)

The guarded-write backend now admits bounded straight-line sequences containing at least
two existing ordinary named Number-to-Number stores. Value-producing `SetProp` is supported,
so a chained assignment can keep an outer receiver and the assignment result in native
registers after committing the inner write. Numeric expressions and borrowed object roots
remain in fixed, distinct homes until the sequence finishes or exits.

Each original opcode has a pre-op virtual operand snapshot. Every guard branches before
changing a register needed by that snapshot. After a prior commit, the new `region_exit`
module clones each logical Object operand, publishes ordered wide Values above the untouched
owned stack prefix, advances the stack pointer and resumes that exact original instruction.
Failures through the first store can instead resume the original region entry, because no
write or owner change has occurred yet. Pure operations have no guard exits. Locals,
inline-frame owners and object-valued graph edges remain unchanged. A later failure therefore
does not replay an earlier store. Separate fallback labels bypass write-region selection,
preventing a failed entry guard from immediately retrying itself. All original targets remain
available and are protected from baseline fusion before emission reaches them.

Property reads are never reused. Every numeric write invalidates name-root reuse, since the
receiver can alias the global object. Existing immutable physical local/this roots can still
be borrowed across writes. Store guards retain the ordinary receiver, non-index name, live
plain/shape/slot/data/writable checks and Number-only replacement restriction. No helper,
allocation, owner release or GC occurs inside a successful sequence. The planner rejects
handlers, calls, local assignments, control transfers and unsupported operations. It stops
when at least two stores have completed and the relative stack is empty, and bounds scanning
to 32 original operations.
It does not yet implement mixed object loops or dirty Object-local restoration.

The scope follows actual application candidates: Djot's hot `skipSpace()` assigns `indent`
and then `pos`; DeltaBlue's captured `markInputs()` bytecode chains assignments through two
object-valued fields. Untimed diagnostics generate write sequences in both application runs,
including DeltaBlue PCs 9–13. Generation is not a measurement of runtime success rate; those
diagnostic timings are excluded because they overlap validation.

Six new tests include real native stack publication with duplicate Object ownership and
signed zero, plus all-tier fixtures for pending receivers in chained assignments, getters
and setters with GC after a prior commit, aliasing and global-name reloads, strict failures,
and an owned outer operand prefix. All-tier fixtures assert actual native commits and
actual post-commit exits. Validation passes 698 unit and 34 integration tests. Conformance
remains 21001/21003 with the same two known failures; differential testing gives 1996
agreements and four budget skips. Clippy matches the existing 86 library/88 library-test
error baseline, and formatting plus the strict nine-module structure audit pass.

The external optimizer directory preserves `write-sequence-*` workloads, drivers, source
ZIP, raw results, summaries, validation logs and provenance. The same-binary control sets
`LUMEN_JIT_NO_WRITE_SEQUENCE=1`; the earlier guarded branch/write regions stay enabled.

The first 36-run comparison used a precise exit stub for every opcode. Sequential and
chained fields reduced median time 210→162.5 µs (22.6%) and 306.67→186.67 µs (39.1%).
Djot was flat at 3906→3905 ms; DeltaBlue had mixed pairs (9092→9269, 9104→9025,
9052→9074 ms), establishing no application gain.

The final emitter omits unused pure-operation stubs and routes all precommit guards to
one original-entry destination. Only fallible post-commit operations retain precise
materialization stubs. A fixed-count, verified map comparison matched chunks by exact
slot names, opcode sequences and occurrence. Total captured code changed 44728→44248 bytes;
the matched sequential function changed 7084→7004 and the chained function 7616→7456 bytes.
These are emitted code sizes, not executed-byte counts or evidence of the cause of a timing
change.

A further 45 verified runs compared feature-disabled and compact behavior in the same
binary, the saved initial full-exit binary, Node 24.18.0 and Bun 1.3.14. Three rounds rotated
engine order; no Lumen build, test or diagnostic overlapped timings. Kernels are microseconds
per 10000 invocations and applications are milliseconds.

| Workload | Disabled | Full exits | Compact exits | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| SequentialFields | 212.50 | 162.50 | 165.00 | 8.64 | 3.68 |
| ChainedFields | 313.33 | 190.00 | 190.00 | 3.32 | 5.10 |
| Djot | 3938.00 | 3947.00 | 3902.00 | 228.00 | 150.00 |
| DeltaBlue | 9120.00 | 9148.00 | 9073.00 | 197.00 | 306.00 |

Compact sequential fields improve 22.4% and chained fields 39.4% versus disabled, with all
pairs improving. Compact sequential fields are 1.5% slower by median than the original
full-exit implementation, also slower in every pair; removing code is not a universal speedup.
Djot improves 0.91% versus disabled, with pairs 3996→3883, 3938→3902 and 3930→3908 ms.
It improves 1.14% versus full exits, also in all pairs. This is a small three-round result,
not a large parser breakthrough. DeltaBlue improves 0.52% by median but its pairs remain
mixed: 9098→9204, 9120→9073 and 9133→9068 ms.

The compact parser still takes 17.1× Node and 26.0× Bun in this comparison. Changes in those
ratios across separate comparison sessions are not cumulative optimization gains; Bun's
median here is 150 ms, versus 143 ms in the initial sequence comparison. The within-2×
application target remains unmet.

A final six-run, three-pair classic-suite comparison measured disabled versus compact behavior.
Scores are higher-is-better:

| Benchmark | Disabled | Compact exits |
| --- | ---: | ---: |
| Richards | 23455 | 23435 |
| DeltaBlue | 3603 | 3633 |
| Crypto | 23917 | 24017 |
| RayTrace | 6301 | 6473 |
| EarleyBoyer | 3605 | 3591 |
| RegExp | 1629 | 1623 |
| Splay | 10512 | 10358 |
| NavierStokes | 38397 | 38101 |
| Score (version 7) | 8598 | 8612 |

The composite changes 8598→8612 (+0.16%), with all pairs slightly positive: 8618→8620,
8565→8591 and 8598→8612. This is effectively flat. RayTrace improves 2.73% by median,
with every pair higher, while Splay decreases 1.47%, also in every pair. The optimization
therefore has a measurable counter-regression, not a universal gain.

The compact sequence implementation is retained for the small repeated parser gain,
substantial verified sequence-kernel improvements and the precise post-write exit capability.
No broad breakthrough is claimed. The original full-exit implementation remains archived,
not selectable in production. Existing `LUMEN_JIT_NO_GUARDED_WRITE_REGION` disables both
branch/write and sequence paths; `LUMEN_JIT_NO_WRITE_SEQUENCE` isolates the new extension.

This turn completed 87 result-verified timing runs across the initial comparison, compact
comparison and classic suite. The next architectural requirement is restoring changing
Object locals across native loop iterations, alongside precise numeric-local and operand
snapshots. The external `mixed-loop-local-exits.md` design identifies the existing scheduler
materializer as an ownership precedent and requires multiple completed native iterations;
that broader loop implementation remains future work. The within-2× suite/parser goal is
still unfulfilled.

### Borrowed shadow frames across object-heavy native loops (2026-09-07)

The next extension keeps a bounded natural-loop CFG in native code while its Object and
Number locals change. Selection uses the existing CFG/SSA model, supported effects and
Object uses, without function-name or bytecode-position matching. Up to 16 wide locals,
eight operand cells, 64 blocks and 256 original operations are admitted. Ordinary named
Number-to-Number writes, numeric operations, guarded own property reads, ordinary inherited
method lookup and dense Object elements are supported. Unsupported calls, accessors,
exotic behavior and values become exact exits into the original baseline code.

The initial implementation uses a borrowed wide-Value shadow frame on the native stack.
Physical locals, receiver and environment keep the original ownership graph alive while
native instructions update shadow locals and operands. Native heap writes cannot sever
Object edges. Every exit clones all live shadow locals and operands, publishes their
owners, then drops displaced locals. Popped shadow cells are never dropped. This ordering
handles aliases, swaps, stale last owners, hidden inline locals and pending Object/Number
operands after prior observable writes. A 1024-backedge budget exits through the original
baseline path so existing polling remains reachable. Every admitted backwards Jump consumes
the budget, including nested-loop backedges; backwards conditional branches are excluded.
HTMLDDA and other non-numeric/non-Boolean conditions take checked baseline truthiness.

Tests exercise actual native backedges, rather than compilation alone: changing Object
locals; mixed inherited methods; getters and GC after prior writes; dense holes and inherited
index getters; inherited field accessors; prototype and method replacement; strict read-only
writes; nested loops with continue/break and budget exits; and cold calls with both a pending
destination Object and numeric operand. Direct native publication tests check aliases,
self-assignment, wide tags, stack boundaries and destruction of a displaced ownership graph.
A polymorphic fixture initially failed to enter because direct root-AST calls do not advance
the callee's inline-recompilation counter; a compiled caller supplies the required warmup.
The coverage assertions were preserved.

`LUMEN_JIT_NO_MIXED_LOOP=1` disables only this extension. Optional
`LUMEN_JIT_MIXED_STATS=1` adds native entry/backedge/exit-PC counters. Instrumented timings
are invalid for performance comparison. Counter storage intentionally survives TLS teardown
in diagnostic mode so embedded native pointers cannot dangle; disabled mode allocates no
counter registry and emits no counter instructions. Backedge counts include the jump that
exhausts the budget, and are not necessarily outer-loop iteration counts.

A fresh mapped DeltaBlue run selects its main plan loop as 113 admitted operations and
12 locals. A separate diagnostic run records 549,987 entries, 50,651,451 native backward
jumps, 549,986 normal exits at PC 134 and one exit at PC 118. This establishes sustained
execution in the real application; frequent early fallback does not explain the remaining
overhead. The previously archived opcode map was not used as current coverage evidence.

The external optimizer directory contains `mixed-loop.js`, `compare-mixed-loop.py`,
`summarize-mixed-loop.py`, their results/metadata/summary, and fresh mapped/statistics logs.
The verified kernels visit 20,000 distinct objects per batch and perform 600 verified,
untimed wrapper invocations before calibration to reach the inline tier. Three rotated
rounds compare the same binary with the extension disabled/enabled, Node v24.18.0 and Bun.
All inherited `LUMEN_*` variables are removed; only the disabled mode adds its flag.
No own builds, tests or profiling overlap the timings. All 36 runs complete and validate.

| Workload (lower is better) | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| ObjectArray, microseconds / 20k visits | 255 | 255 | 26 | 20.4 |
| PolymorphicMethods, microseconds / 20k visits | 680 | 315 | 78.46 | 30 |
| Verified Djot, milliseconds | 3864 | 3868 | 226 | 143 |
| Verified DeltaBlue, milliseconds | 9113 | 8741 | 194 | 299 |

PolymorphicMethods decreases 53.68% by median, with all three pairs improving 52–54%.
ObjectArray is flat in all pairs. Node's polymorphic kernel is variable (78.46, 22,
78.46 microseconds), so its median comparison alone is not a stable universal engine ratio.
DeltaBlue decreases 4.08%; pairs are 9029→8844, 9113→8729 and 9154→8741, all faster.
Djot changes +0.10% by median with mixed paired changes and is effectively flat.
Its remaining gap is 17.1× Node and 27.0× Bun in this session. The Bun ratio differs from
the previous session because Bun's measured median changed; this is not a cumulative Lumen
regression. The standalone repeated Delta workload remains 45.1× Node and 29.2× Bun.

A separate 12-run rotated classic v8-v7 comparison uses the same measured binary, with all
nine scores verified per run. Results, CPU snapshots, driver and metadata are saved under
`mixed-loop-classic-*`. Median scores (higher is better):

| Benchmark | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23596 | 23478 | 65397 | 72736 |
| DeltaBlue | 3623 | 3666 | 152038 | 110232 |
| Crypto | 24025 | 23934 | 91491 | 119913 |
| RayTrace | 6265 | 6406 | 134826 | 304579 |
| EarleyBoyer | 3557 | 3640 | 147975 | 155473 |
| RegExp | 1648 | 1631 | 22684 | 30397 |
| Splay | 10260 | 10284 | 79535 | 90748 |
| NavierStokes | 38139 | 37915 | 70271 | 70787 |
| Score (version 7) | 8597 | 8607 | 83234 | 97449 |

The composite changes +0.12%, with mixed pairs (8637→8596, 8597→8607, 8567→8614),
and is effectively flat. EarleyBoyer improves 2.33% by median with all pairs higher.
Richards decreases 0.50%, Crypto 0.38% and NavierStokes 0.59%, with every respective pair
lower. Other workload directions are mixed; Splay's first pair is 4.51% lower despite its
slightly higher aggregate median. This is not a universal speedup. The remaining composite
gap is 9.67× Node and 11.32× Bun; NavierStokes alone meets the 2× comparison in this session,
which does not establish suite or application parity.

The extension is retained for the repeated standalone DeltaBlue reduction, substantial
polymorphic-kernel gain and verified precise publication across changing Object locals.
The full-suite/parser goal remains unfulfilled. Next, remove redundant branches between
physically adjacent native operations, then measure comparison/branch fusion and conservative
static shadow-tag knowledge. Register allocation requires a separate exit-materialization
proof and is not part of this initial shadow-frame implementation.

All 48 measured runs use local binary SHA-256
`f7d4886db3246758bb4b8b29cb471ae66e9a25657dfcf7e5c50c0b50a905fb7c`.
`mixed-loop-measured-source.zip` preserves the measured source including new untracked files;
source hashes in the metadata describe invocation-time workspace state. Subsequent production
source edits before commit only clarify the already-enforced publication comments.

Final validation passes 711 unit and 34 integration tests, formatting and the strict
structural audit for all 14 new Rust files. The core backend's conformance run passes
21,001/21,003 selected tests with the same two pre-existing arrow-arguments/dynamic-import
failures; differential fuzzing reports 1,996 agreements and four budget skips. Optional
statistics were added afterward and validated by the final tests and the real diagnostic
Delta run; the disabled handle emits no instructions. Clippy matches the existing 86
library/88 library-test diagnostics exactly, including after the statistics addition; it is
not warning-clean. Logs and binary/source provenance are archived in `mixed-loop-validation.json`.

### Rejected native fallthrough experiment (2026-09-07)

A follow-up removed unconditional branches whose successor is the physically next admitted
native label. All label bindings, gaps, outlined exits, explicit Jump/JumpIfFalse/InlineGuard
control and backedge budgeting were preserved. A same-binary flag restored the original
emission. The tested patch, binary, validation and rotated results are archived externally
under `fallthrough-*` and `lumen-fallthrough*`; the production emitter is restored.

All 711 unit/34 integration tests pass, including 11 focused native-loop tests; formatting
and strict structure checks pass. A real diagnostic DeltaBlue run retains exactly the same
549,987 entries, 50,651,451 backwards jumps and exit-PC counts as the prior implementation.
With diagnostics enabled in both maps, the full plan-function allocation shrinks from
122,092 to 121,744 bytes (348 bytes, 87 branches). This includes its retained baseline code;
original-PC map intervals are not a disassembly of the native prefix.

Thirty-six verified runs compare original branches, fallthrough, Node and Bun in three
rotated rounds, with diagnostics disabled and no overlapping own builds/tests/profiling.
Median times:

| Workload | Original branches | Fallthrough | Change |
| --- | ---: | ---: | ---: |
| ObjectArray, microseconds / 20k visits | 260 | 265 | +1.92% |
| PolymorphicMethods, microseconds / 20k visits | 320 | 330 | +3.13% |
| Djot, milliseconds | 3931 | 3919 | -0.31% |
| DeltaBlue, milliseconds | 8845 | 8827 | -0.20% |

The polymorphic kernel regresses 320→330 in every pair. ObjectArray regresses in two pairs
and ties in one. DeltaBlue improves in all pairs, but its median gain is only 0.20%; Djot's
pairs are mixed, including a 1.98% regression. Fewer emitted branches therefore do not
establish a useful speedup. No hardware cause is inferred from code size alone. The
experiment is rejected; the prepared full classic comparison is not run because these
results already fail the retention case. Comparison/branch fusion remains a separate next
candidate and does not depend on this experiment.

### Numeric comparison/branch fusion in native shadow loops (2026-09-07)

This candidate fuses a numeric comparison immediately followed by JumpIfFalse, bypassing
wide Boolean materialization and the subsequent generic truthiness dispatch. Selection
requires both operations in the admitted plan and the same CFG block, a forward conditional
edge, and consistent comparison/branch/successor stack depths. Independent branch targets
are CFG leaders, so the same-block test conservatively preserves any separately produced
Boolean input. Both original operand cells remain intact until numeric guards succeed;
a failure resumes at the comparison PC after publishing all prior writes and live operands.
Success branches directly using the established floating-point condition codes. Logical
successor depth discards both operands; popped shadow copies remain non-owning.

The original baseline code and label bindings remain present. The consumed private branch
label has no independent native predecessor. No fallthrough experiment changes are included.
`LUMEN_JIT_NO_MIXED_LOOP_COMPARE=1` restores the separate comparison/branch emission while
keeping the mixed-loop backend enabled. Emission control is split into a bounded step helper
so the entrypoint retains region construction and exact exit publication responsibilities.

All 714 unit and 34 integration tests pass. Fourteen focused mixed-loop tests include actual
successful fusion, all eight comparisons with NaN/signed zero/infinities, a real ternary CFG
whose independently targeted branch is rejected, and nonnumeric coercion after an earlier
heap write. The coercion test requires an actual fused numeric-guard failure, rather than
allowing property-read fallback alone to satisfy coverage. Formatting and strict structural
checks pass; Clippy matches the existing 86/88 diagnostic multiset exactly.

A fresh instrumented DeltaBlue run preserves the prior 549,987 entries, 50,651,451 native
backedges and normal/PC118 exit counts. Its plan-function allocation changes from 122,092 to
121,404 bytes with diagnostics enabled in both maps. This is coverage/code-size evidence,
not a speed claim. Comparison timings and the retention decision follow below.

The rebuilt conformance runner passes 21,001/21,003 selected tests with the same two known
failures; the rebuilt differential fuzzer reports 1,996 agreements and four budget skips.
Thirty-six result-verified timing runs use three rotated rounds of the same binary with
fusion disabled/enabled, Node v24.18.0 and Bun. Diagnostics and inherited `LUMEN_*` variables
are disabled; only the original-emission mode adds the comparison-disable flag. No own
builds, tests or profiling overlap timing runs. External artifacts use `mixed-compare-*`.

| Workload (lower is better) | Separate operations | Fused | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| ObjectArray, microseconds / 20k visits | 260 | 235 | 26.4 | 20.8 |
| PolymorphicMethods, microseconds / 20k visits | 320 | 305 | 78.46 | 30.4 |
| Djot, milliseconds | 3920 | 3925 | 227 | 144 |
| DeltaBlue, milliseconds | 8921 | 8763 | 195 | 310 |

ObjectArray time decreases 9.62% by median, PolymorphicMethods 4.69% and DeltaBlue 1.77%,
with every pair faster. DeltaBlue pairs are 8921→8744, 8865→8777 and 8977→8763.
Djot changes +0.13% with mixed pairs and remains effectively flat, at 17.3× Node and
27.3× Bun in this session. These are incremental comparisons to the existing shadow-loop
backend, not a combined claim including the rejected fallthrough experiment. The full
suite/parser objective remains unfulfilled; parser-specific work is still required.

A separate 12-run, three-round classic v8-v7 comparison completes with every score verified.
Its `mixed-compare-classic-*` artifacts preserve raw results and CPU snapshots. Median
scores (higher is better):

| Benchmark | Separate operations | Fused | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23628 | 23452 | 65556 | 72606 |
| DeltaBlue | 3712 | 3712 | 154366 | 109511 |
| Crypto | 23989 | 24025 | 91785 | 119663 |
| RayTrace | 6400 | 6307 | 136750 | 303099 |
| EarleyBoyer | 3635 | 3620 | 148383 | 156821 |
| RegExp | 1649 | 1638 | 22797 | 30366 |
| Splay | 10309 | 10292 | 80627 | 95475 |
| NavierStokes | 38139 | 38397 | 70787 | 71309 |
| Score (version 7) | 8663 | 8626 | 83721 | 98987 |

The composite decreases 0.43% by median, with pairs changing -0.84%, -2.82% and +0.70%.
Every workload has mixed paired directions; the middle enabled run includes a 12.16%
EarleyBoyer regression and a 4.09% Crypto regression. The medians must not hide those results,
nor do the CPU snapshots establish a particular cause. No broad suite speedup is claimed.
Fusion is retained for its repeated kernel improvements and standalone DeltaBlue reduction,
with the full-suite result recorded as a limitation. Remaining composite gaps are 9.71× Node
and 11.48× Bun. The measured binary SHA-256 is
`edf061d7b7ca0d4354f1896fa74e7a5ac836e3aacd39ea14d1c74900516dd112`;
`mixed-compare-measured-source.zip` includes new source files, and
`mixed-compare-validation.json` records validation logs and executable hashes.

This goal turn completes 84 verified timing runs: 36 for rejected fallthrough, then 36
application/kernel and 12 classic runs for comparison fusion. The next native-loop candidate
is conservative block-local shadow-tag knowledge, preserved externally and not applied.
A separate parser-specific substring candidate is also prepared, not applied.

After all timing runs finish, a fresh six-second Djot sample on the same build verifies
40,000 parse/render outputs (12,640,000 HTML characters). The instrumented timing is invalid
for performance comparison. Artifacts are `djot-mixed-compare.sample`, the associated
profile maps/stdout/metadata and `profile-mixed-compare-djot.py`. No mixed-loop statistics
records are emitted for this workload, consistent with the loop extension not reaching its
parser work. This fresh profile, rather than the earlier archived sample alone, informs the
next allocation/string investigation.

The fresh profile contains 4,623 main-thread samples. Exclusive named top-of-stack counts
include Value destruction 206, GC collection 175, property lookup 117, prototype-chain
lookup 101, object construction 68, regex matching 34 and regexp_exec 24. Free/custom
allocation/tiny-allocation symbols contribute 102/83/44 samples respectively; memcmp 135
and memmove 84 cannot safely be assigned specifically to strings. Named UTF-16 conversion
routines have only eight and five samples. Inlining, anonymous native code and the sample
report's five-hit threshold limit attribution. The ASCII substring candidate is a bounded
experiment, not an established major bottleneck. Regex result-object materialization and
ownership traffic remain stronger architectural hypotheses requiring caller attribution.

### ASCII substring experiment and parser caller attribution (2026-09-07)

A fresh caller attribution parses the complete sample-tree indentation, subtracting immediate
children to compute exclusive residuals and counting ancestry unions without recursive double
counting. All residuals are nonnegative and sum to the 4,623 main-thread samples. RegExp exec
accounts for 374 samples (8.09%) inclusive; its disjoint make_array/set_data descendant union
is 172 (3.72% overall). Matcher ancestry contributes 78 samples (1.69%). GC ancestry contributes
553 samples (11.96%) outside the regexp stack; deferred GC cannot be assigned to its original
allocating caller from CPU stacks. The exec_text_discard_shared wrapper delegates to
exec_text_shared, explaining the sampled symbol without implying a source change.

The same attribution finds array_iter_next at 512 samples (11.08%). Disjoint immediate child
paths include get_member 181, set_member 109, set_data 109, new_object 22, direct self 31 and
remaining paths 60. This points to internal iterator-state access and result construction,
while not separating target-length lookup from internal-field lookup inside get_member.
Artifacts are `djot-regexp-attribution.md`, its JSON, the reproducible parser script and
`djot-other-native-attribution.json`/`djot-array-next-children.json`. Categories such as
property and allocation ancestry overlap and must not be added together.

The isolated substring candidate extracts the existing builtin into `string_substring.rs`.
A proven ASCII hint permits byte slicing directly into the ordinary contiguous LStr result,
without materializing UTF-16 units or an intermediate Rust String. A clear hint retains the
original UTF-16 path, including lone/split surrogate behavior. Receiver conversion precedes
start/end coercion, and clamping, truncation and bound swapping remain unchanged. The source
string is retained through callbacks and GC. Long inputs could already reuse a cached unit
buffer; avoiding a fresh input conversion is not a universal per-call saving.

The A/B switch `LUMEN_NO_ASCII_SUBSTRING=1` is sampled at realm installation and selects a
plain function pointer to a const-generic implementation. There is no getenv or captured
closure dispatch on each call. The prepared candidate's capturing closure was corrected
because the builtin API accepts NativeFn pointers. Output still allocates/copies an LStr;
this is not a substring-view representation.

Validation passes 717 unit/34 integration tests, all 46 targeted substring conformance tests,
and 1,996 differential-fuzz agreements with four budget skips. Tests require actual ASCII
and UTF-16 path execution across all tiers and cover empty/long strings, fractional/NaN/
infinite/signed-zero bounds, receiver/start/end coercion ordering and short-circuit errors.
Clippy matches the existing 86/88 diagnostic baseline, and formatting/strict structural
checks pass. The prior broad conformance baseline is not presented as a new run here.

The external `ascii-substring.js` verifies every result in four 10,000-call kernels: short
prefixes, clamped/swapped short ranges, a repeated 6,656-byte ASCII source, and UTF-16 slices
including surrogate halves. The timing includes those checks and uses calibrated batches.
Thirty-six sequential runs compare that kernel, verified Djot and DeltaBlue across three
rotated rounds of disabled/enabled Lumen, Node v24.18.0 and Bun. Drivers strip inherited
LUMEN_* settings, record versions/source/executable/workload hashes and CPU snapshots, and
validate output. No own builds, tests or profiling overlap timings. Median times:

| Workload (lower is better) | UTF-16 path | ASCII enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| ShortPrefix, microseconds / 10k calls | 940 | 720 | 83.53 | 111.11 |
| ShortRanges, microseconds / 10k calls | 960 | 760 | 92.31 | 151.43 |
| LongAscii, microseconds / 10k calls | 860 | 680 | 204 | 135 |
| UtfSixteen, microseconds / 10k calls | 940 | 940 | 67.69 | 127.5 |
| Djot, milliseconds | 3910 | 3927 | 230 | 143 |
| DeltaBlue, milliseconds | 8808 | 8865 | 198 | 314 |

ASCII kernels improve 20.8–23.4% by median with all pairs faster. UTF-16 median is unchanged
(two ties and one 2.13% slower pair). Djot changes +0.43%, with pairs 3890→3927, 3941→3944
and 3910→3889; DeltaBlue changes +0.65%, with pairs 8808→8804, 8899→8937 and 8750→8865.
Neither application shows a reliable gain. The parser remains 17.1× Node and 27.5× Bun in
this session. Node has variable short-prefix and UTF-16 kernel samples, so median kernel
ratios are workload observations, not stable overall engine comparisons.

Twelve additional rotated classic v8-v7 runs verify all nine scores. Median scores (higher
is better), archived under `ascii-substring-v8-*`:

| Benchmark | UTF-16 path | ASCII enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23648 | 23709 | 65679 | 72020 |
| DeltaBlue | 3689 | 3703 | 153625 | 105577 |
| Crypto | 23883 | 24032 | 92347 | 119540 |
| RayTrace | 6512 | 6441 | 137564 | 305245 |
| EarleyBoyer | 3633 | 3566 | 147782 | 158027 |
| RegExp | 1644 | 1636 | 22707 | 30275 |
| Splay | 10243 | 10309 | 79307 | 94326 |
| NavierStokes | 38287 | 38249 | 69975 | 71606 |
| Score (version 7) | 8643 | 8628 | 83819 | 98483 |

The composite changes -0.17%, with pairs -0.64%, -0.62% and +0.14%. RayTrace decreases
1.09% and NavierStokes 0.10% by median with all respective pairs lower. Other workloads have
mixed paired directions; EarleyBoyer decreases 1.84% by median. The implementation is
retained for the repeated substantial substring-kernel gain, with application/suite limits
and counter-regressions recorded. No parser or overall speedup is claimed. Composite gaps
remain 9.71× Node and 11.41× Bun, well outside the full objective.

All 48 timing runs and validation records are preserved in `ascii-substring-*` artifacts;
`ascii-substring-measured-source.zip` includes the new source module. The next prepared,
unapplied candidate uses guarded direct Array Iterator state access, preserving reentrant
length-getter mutations and exact fallback ordering. Result construction, branding changes,
and the pre-existing keys-iterator element-fetch behavior are outside that candidate.

### Guarded Array Iterator state experiment (2026-09-07)

The current Djot profile attributes 512/4623 samples (11.08% inclusive) to
Array Iterator `next()`. Its immediate property-read, property-write and result
construction paths account for 181, 109 and 109 samples respectively; these
counts do not identify which individual internal state reads dominate.

`builtins/array_iterator.rs` extracts the existing implementation and adds a
pure own-data snapshot for ordinary, `ic_plain` receivers with numeric index
and kind fields. It preserves the existing own-kind brand check and falls back
before any observable operation when the snapshot is ineligible. The target is
moved out of the snapshot without a second reference-count increment. Target
length reads, numeric coercions, typed-array length checks, element reads and
result construction retain their original ordering.

After the length getter and result allocation, the index write checks the live
own property again. Only a writable data property whose old value is numeric
is overwritten directly. A getter that replaces the descriptor, deletes the
property, changes writability or installs an object value causes only the
pending generic write to run; state reads and length getters are never replayed.
Clearing an exhausted target stays generic. The existing keys-mode element Get
and support for forged internal-slot-style receivers are preserved, rather than
changed as part of this performance experiment. Realm setup selects the baseline
implementation when `LUMEN_NO_ARRAY_ITERATOR_STATE` is set, with no hot getenv.

Seven colocated tests run across Interp, Bytecode and JIT, asserting actual
snapshot, direct-write and late-fallback counters. They cover ordinary values,
exhaustion, coercion ordering, reentrant length getters, GC after descriptor and
owner changes, proxy targets/receivers, readonly/deleted indices and inherited
setters, plus detached and resized typed-array views. The same seven tests pass
with the optimization disabled. A forged-receiver readonly case exposed an
existing native strictness difference: Interp returns normally while Bytecode
and JIT throw from the strict caller, identically in both implementations. The
test preserves this baseline; the optimization does not resolve that separate
correctness issue.

Validation passes 724 unit and 34 integration tests, 122 targeted iterator
conformance tests and 1,996 differential agreements with four budget skips.
Formatting and the new module's strict structural audit pass. Strict Clippy
retains the exact pre-existing 86-library/88-library-test error-message multiset.
The release build predates final test-only fixture edits and formatting; its
runtime implementation is unchanged. Measured source, executable hashes and
validation logs are archived under `array-iterator-state-*` and
`lumen-array-iterator-state-*` in the external optimizer directory.

Thirty-six sequential, output-verified runs compare the same executable with state
access disabled/enabled, Node and Bun across three rotated rounds. No own builds,
tests or profiling overlap timings. Medians (lower is better):

| Workload | Baseline | State enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| IteratorValues (µs / 10k visits) | 2200 | 1620 | 6.3 | 10.24 |
| IteratorKeys (µs / 10k visits) | 2205.88 | 1600 | 6.3 | 8.96 |
| IteratorEntries (µs / 10k visits) | 3058.82 | 2441.18 | 30 | 29.7436 |
| PairDestructure (µs / 10k visits) | 9900 | 8000 | 15.8824 | 21.1765 |
| Djot (ms) | 3948 | 3865 | 232 | 147 |
| DeltaBlue (ms) | 8848 | 8791 | 203 | 306 |

The four iterator kernels improve 19.2–27.5% by median, with every paired run
faster. Djot improves 2.10%, with pairs 3952→3884, 3923→3778 and 3948→3865 ms
(1.72–3.70% less time). This is a repeated application gain in the measured
session. DeltaBlue changes -0.64% by median, with mixed pairs (-1.47%, +0.45%,
-0.64%); it is not presented as a reliable application improvement. The parser
still takes 16.66× Node and 26.29× Bun time. The much larger iterator-kernel gaps
are measurements of these particular checked loops, not overall engine ratios;
eliminating temporary iterator/result objects in compiled code remains a separate
structural opportunity.

Twelve additional rotated classic v8-v7 runs verify all nine scores. Medians
(higher is better):

| Benchmark | Baseline | State enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23509 | 23670 | 65644 | 71960 |
| DeltaBlue | 3699 | 3762 | 152746 | 109610 |
| Crypto | 23915 | 23840 | 91517 | 120153 |
| RayTrace | 6492 | 6393 | 137490 | 305245 |
| EarleyBoyer | 3629 | 3650 | 147525 | 157029 |
| RegExp | 1646 | 1633 | 22752 | 30548 |
| Splay | 10252 | 10349 | 80138 | 93582 |
| NavierStokes | 37729 | 38139 | 70864 | 70342 |
| Score (version 7) | 8558 | 8664 | 83509 | 98519 |

The composite increases 1.24% by median, with mixed pairs (-0.02%, +2.62%,
+1.24%). The second baseline run has notably lower Splay and NavierStokes
scores; CPU snapshots are retained but do not establish the cause. Richards,
classic DeltaBlue, EarleyBoyer and Splay have higher scores in all paired runs.
Crypto (-0.31%), RayTrace (-1.52%) and RegExp (-0.79%) have lower medians but
mixed paired directions. These results do not establish a consistent overall
suite gain. The change is retained for the repeated parser and iterator-kernel
improvements, with these counter-regressions and variability recorded.
Composite score ratios remain 9.64× Node and 11.37× Bun, far outside the goal.

All 48 timing runs are archived. The next prepared element-read candidate is
unapplied; it reuses the existing checked dense-element helper after callbacks.
A larger separate design specializes the existing flat array-destructuring
opcode to avoid transient iterator/results entirely when intrinsic-method,
dense-data and iterator-close guards prove that no user callback is bypassed.
Neither follow-up is included in the measured implementation.

### Bounded ordinary-array destructuring experiment (2026-09-07)

The existing `DestructureArr(n)` opcode already batches flat identifiers and
elisions whose bindings occupy uncaptured local slots; defaults, nested patterns
and rest bindings do not use this lowering. Its VM and JIT checked paths normally
create an iterator, call `next` for each element, read result properties and close
the iterator unless exhaustion was observed. The new shared
`bytecode/array_destructure.rs` helper bypasses that protocol for at most 16
bindings only after a pure preflight proves that no callback will be skipped.

The proof requires an ordinary `ic_plain` array, valid own data length, the exact
original Array values function at `Symbol.iterator`, the exact original native
iterator `next`, and present own data elements for the yielded prefix. Ordinary
prototype walks inspect live descriptors, admit Array.prototype, reject accessors
and exotic behavior, and conservatively stop after eight objects. Installed
method identities are retained in the existing rooted, realm-switched
`extra_protos` map. Comparing against mutable current prototype methods would not
be sufficient when user code replaces both `values` and `Symbol.iterator`.
When `n <= length`, including empty patterns and exact-length bindings, the
iterator prototype chain must prove `return` absent, undefined or null. When
`n > length`, the original path observes exhaustion and does not close.

After every guard passes, the helper clones outputs while the input remains
owned, fills exhausted outputs with undefined, and publishes them through the
existing opcode stack boundary. It creates no JavaScript iterator or result
objects; the current Rust implementation still allocates a symbol-key String and
an output Vec. Any guard failure executes the unchanged complete protocol.
Compiler eligibility, outer for-of handling and callback fallback ordering are
unchanged. Realm setup omits the retained values identity when
`LUMEN_NO_ARRAY_DESTRUCTURE` is set; no hot getenv is added.

Six colocated tests run in Interp, Bytecode and JIT and assert exact successful
fast-path counts in compiled tiers, with zero in Interp or disabled mode. They
cover empty/short/exact/long patterns, skipped elements versus holes and explicit
undefined, aliased object outputs surviving GC, custom and accessor iterator
methods, foreign intrinsics, invalid close results, and element getters that
change later elements and install return getters/methods during GC. The latter
checks the exact first-element, second-element, return-getter, return-call and
binding-body order. All six also pass with the optimization disabled.

Validation passes 730 unit and 34 integration tests. The expanded conformance
selection passes 21,047/21,049 with the same two known lexical-arguments and
async import-cycle failures. Differential fuzzing records 1,996 agreements and
four budget skips. Formatting and the new module's strict structural audit pass;
strict Clippy retains the exact pre-existing 86-library/88-library-test
error-message multiset.

A temporary test-only diagnostic executes the complete output-verified Djot
workload and records 60,000 successful scalar replacements across 10,000 parses.
This establishes actual parser coverage. The diagnostic's debug configuration
and timing are not performance results. Its source and log are archived, and the
exact pre-diagnostic source hash was restored before timing. The measured release
predates only final test additions/formatting; runtime implementation is unchanged.

Thirty-six sequential verified timings compare the same binary with the
optimization disabled/enabled, Node and Bun in three rotated rounds. Builds,
tests and profiling do not overlap timing. Medians (lower is better):

| Workload | Baseline | Dense binding enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| DensePair (µs / 10k bindings) | 5888.89 | 880 | 13.4 | 12.8 |
| ObjectPair (µs / 10k bindings) | 6000 | 960 | 16.2 | 17.0667 |
| ShortArray (µs / 10k bindings) | 5555.56 | 680 | 9.86667 | 11.8 |
| HolePattern (µs / 10k bindings) | 7625 | 930 | 14.6 | 15.4667 |
| Djot (ms) | 3780 | 3783 | 226 | 142 |
| DeltaBlue (ms) | 8716 | 8749 | 195 | 306 |

The four binding kernels take 84.0–87.8% less time by median, with all paired
runs faster. Djot is effectively flat at +0.08% median, with mixed pairs
3759→3751, 3789→3937 and 3780→3783 ms (-0.21%, +3.91%, +0.08%). DeltaBlue
changes +0.38%, with pairs +0.96%, +0.94% and -0.33%. No application gain is
claimed. The parser remains 16.74× Node and 26.64× Bun time. The separate
60,000-hit diagnostic establishes coverage, not a performance benefit.

Twelve additional rotated classic runs verify all nine scores. Medians (higher
is better):

| Benchmark | Baseline | Dense binding enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23497 | 23465 | 65775 | 72443 |
| DeltaBlue | 3749 | 3578 | 154022 | 110153 |
| Crypto | 24030 | 23983 | 91582 | 120014 |
| RayTrace | 6412 | 6387 | 134308 | 304949 |
| EarleyBoyer | 3622 | 3638 | 145810 | 159410 |
| RegExp | 1624 | 1622 | 22934 | 30882 |
| Splay | 10325 | 10300 | 80342 | 95524 |
| NavierStokes | 38211 | 38249 | 71013 | 61141 |
| Score (version 7) | 8649 | 8602 | 83421 | 97899 |

The composite decreases 0.54% by median, with pairs -4.09%, -0.47% and +0.37%.
The first enabled run is lower across all eight workloads; the records do not
establish a cause. RayTrace (-0.39%) and RegExp (-0.12%) have lower median scores
with all respective paired runs lower. Classic DeltaBlue decreases 4.56% by
median but has mixed pairs (-8.84%, -4.56%, +2.50%). Other workloads also have
mixed paired directions. These counter-regressions are retained in the record;
neither an overall suite gain nor a parser gain is claimed.

The implementation is retained for the repeated 84–88% binding-kernel time
reduction and its general guarded removal of iterator/result objects, with the
application and suite limitations above. Composite score ratios remain 9.70×
Node and 11.38× Bun, well outside the objective. All 48 verified timing runs,
source ZIP, binary hashes and validation artifacts are archived under
`array-destructure-*` and `lumen-array-destructure-*` in the optimizer directory.
The next prepared, unapplied `IterStepL` candidate retains the real iterator
while avoiding yielded result objects for guarded ordinary-array steps;
exhaustion and close behavior remain on existing paths.

### Guarded yielded Array Iterator steps (2026-09-08)

`bytecode/array_iterator_step.rs` specializes successful ordinary-array yields
at the existing VM/JIT `IterStepL` boundary. It retains the real iterator and
captured next local, so the existing exhaustion, normal-close and throw-close
paths continue to operate on the exact iterator visible to user code.

The pure preflight requires the captured next function to be the originally
installed intrinsic, an ordinary `ic_plain` iterator with own data kind zero,
a writable own numeric index, and an own data array target. The live target
must be an ordinary `ic_plain` array with a valid own data length and a present
own data element at that index. Undefined is an eligible yielded value; holes,
accessors, proxies, foreign methods, readonly state and exhaustion fall back.
The helper clones the element into an owned Value, revalidates the numeric
index descriptor, then performs one Number-to-Number update. No fallible action
or fallback remains after that update. It allocates no iterator result and
skips the native call plus generic done/value reads for that yielded step.

Both dispatches borrow the rooted iterator/next slots only during the pure
helper. Slot owners remain intact, and the helper cannot execute JS, collect
GC or relocate frames. A miss ends those borrows and clones both values before
the callback-capable original iterator_step. This also removes two reference-
count increment/decrement pairs from successful yields. The guard checks the
captured next identity, not mutable current prototype properties: changing
prototype.next after GetIterator does not replace the already captured method.
`LUMEN_NO_ARRAY_ITERATOR_STEP` selects the baseline at realm setup independently
of the destructuring flag, with no hot getenv.

Eight colocated tests run in Interp, Bytecode and JIT with exact successful-step
counts, and all eight also pass disabled. They cover captured-method replacement,
live growth/shrink, holes/inherited getters/proxies, real iterator identity during
break and throw close, readonly index and length-getter fallbacks, undefined
versus exhaustion, aliased objects surviving GC, body-installed element getters,
foreign next methods, and configurable index getter/setter fallback followed by
fast resumption. Validation passes 738 unit and 34 integration tests, plus
21,047/21,049 selected conformance tests with the same two known failures.
Differential fuzzing yields 1,996 agreements and four budget skips. Formatting
and the strict new-module audit pass; Clippy retains the exact pre-existing
86-library/88-library-test error-message multiset.

A temporary test-only full-Djot diagnostic records 1,510,000 successful yields
across 10,000 output-verified parses. Its timing is invalid for performance
comparison. The diagnostic source/log are archived and the exact original
source hash was restored before timing. The measured release includes the
borrowed-slot dispatch; the earlier unborrowed build was not benchmarked.

Thirty-six sequential, verified timings use three rotated rounds of the same
binary disabled/enabled, Node and Bun, without own builds/tests/profiling in
parallel. Medians (lower is better):

| Workload | Baseline | Yield fast path | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| ForOfNumbers (µs / 10k yields) | 2235.29 | 700 | 7.1 | 7.73333 |
| ForOfObjects (µs / 10k yields) | 2160 | 610 | 10.8 | 15.4667 |
| ForOfUndefined (µs / 10k yields) | 2320 | 740 | 20.4 | 12.8 |
| ForOfBreak (µs / 5k yields) | 1140 | 366.667 | 3.8 | 3.72 |
| Djot (ms) | 3767 | 3507 | 228 | 144 |
| DeltaBlue (ms) | 8758 | 8768 | 195 | 310 |

All paired iteration kernels improve, with 67.8–71.8% less median time. Djot
improves 6.90%, with pairs 3722→3488, 3786→3507 and 3767→3529 ms (6.29–7.37%
less time). This is a repeated parser improvement in the measured session.
DeltaBlue remains effectively flat at +0.11% median, with mixed pairs (+1.54%,
-0.43%, +0.68%). The parser still takes 15.38× Node and 24.35× Bun time, well
outside the full objective. Kernel ratios describe these particular verified
loops and are not overall engine speed ratios.

Twelve rotated classic runs verify all nine scores. Medians (higher is better):

| Benchmark | Baseline | Yield fast path | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23571 | 23431 | 64754 | 71116 |
| DeltaBlue | 3835 | 3769 | 150835 | 107759 |
| Crypto | 23944 | 24037 | 91917 | 119171 |
| RayTrace | 6431 | 6438 | 136454 | 303469 |
| EarleyBoyer | 3548 | 3629 | 147803 | 159395 |
| RegExp | 1635 | 1627 | 22434 | 30397 |
| Splay | 10154 | 10243 | 79837 | 94758 |
| NavierStokes | 38139 | 37729 | 70490 | 61438 |
| Score (version 7) | 8673 | 8641 | 83460 | 97919 |

The composite changes -0.37% by median, with mixed pairs (-0.24%, +0.27%,
-0.37%). Richards (-0.59%), classic DeltaBlue (-1.72%) and RegExp (-0.49%) have
lower median scores with every respective pair lower. Crypto (+0.39%) and
EarleyBoyer (+2.28%) improve in all pairs. Remaining directions are mixed.
No overall suite gain is claimed. The implementation is retained for the
repeated 6.90% parser and 67.8–71.8% iteration-kernel time reductions, with these
suite counter-regressions recorded. Remaining composite score ratios are
9.66× Node and 11.33× Bun; parser ratios remain 15.38×/24.35×.

All 48 timings, source ZIP, executable hashes and validation records are
archived under `array-iterator-step-*` and `lumen-array-iterator-step-*` in the
external optimizer directory. The next diagnostic is a fresh Delta profile
with accurate mixed-loop entry/body/exit ranges resolved after assembler branch
relaxation. Older pre-mixed-loop profiles do not establish the current dominant
CPU mechanism. Existing counters establish sustained native backedges, while
source inspection suggests redundant wide-Value shadow traffic; their actual
runtime share must be established before a register-home implementation.

### Fresh Delta native-region attribution (2026-09-08)

A diagnostic build from f9a1bfe captures exact mixed-loop entry/body/exit ranges.
Four labels bracket the sections without emitting instructions. The assembler
resolves their byte offsets after conditional-branch relaxation. A focused test
forces an imm19 branch expansion, checks the shifted boundaries, and verifies
that mapped and unmapped builds emit identical words. The diagnostic changes
are archived externally and the exact original source hashes were restored;
none of this instrumentation is retained in the production tree.

`profile-delta-iterator-step.py` runs the assertion-preserving standalone Delta
workload for 20,000 repetitions using the separate diagnostic binary. It samples
six seconds after 1.1 seconds of process CPU, captures same-process final code
words, maps and counters, then waits for verified completion. Engine and sample
both exit zero. Printed elapsed time is not a benchmark: mixed-loop counters,
mapping and region diagnostics are enabled. Binary SHA-256 is
`aa8992e06d671cb788afbc2f7dbf0e044da412dcb29e66b37c699f103e90f8bc`.

Independent accounting finds one main-thread root and 5,139 exclusive samples.
All 3,600 anonymous JIT samples map uniquely; 93 allocation records have no
reused or overlapping ranges. The exact disjoint attribution is:

| Location | Exclusive samples | Whole sample |
| --- | ---: | ---: |
| Mixed-loop body | 1462 | 28.45% |
| Other generated code | 2138 | 41.60% |
| Native helpers | 1539 | 29.95% |

The `i|c|(inline this)|(inline this)` chunk has 1,464 samples, of which 1,462
are in its mixed body. No sampled instruction lands in the marked entry/exit
sections; this does not imply those sections have zero cost. Counters record
2,199,987 entries, 202,451,451 backward jumps, 2,199,986 normal PC134 exits and
one PC118 exit. Frequent fallback is not supported as this run's main problem.

GC has 130 direct gc_collect samples (2.53%) and 388 samples under its ancestry
(7.55%, including the direct samples); these numbers must not be added. Native
exclusive categories include allocator routines 250, destruction routines 250,
GC scanning symbols 187, hashing 67 and intrinsic helpers 107. Constructor or
intrinsic ancestry also contains descendant JavaScript execution and is not an
allocation-CPU measure.

`analyze-delta-native-traffic.py` extracts the exact 30,351 final words for the
matched allocation, checks the following map record, assembles those words and
disassembles them with otool. The mixed body spans byte offsets 420..42512 and
contains 10,523 instructions. Direct x23 addressing identifies shadow memory in
this body. Static and sampled-position categories are:

| Instruction category | Static instructions | Body sample positions |
| --- | ---: | ---: |
| Shadow loads | 201 | 250 |
| Shadow stores | 144 | 43 |
| Other memory loads | 2985 | 872 |
| Other memory stores | 4 | 0 |
| Branches | 3289 | 107 |
| Other instructions | 3900 | 190 |

Sampled instruction positions are not stall attribution or a removable-cost
estimate. Static counts include cold alternative branches. Nevertheless, the
large non-shadow load/guard footprint changes the next investigation: arithmetic
register homes alone do not address most sampled body positions. The next proof
to investigate is reuse of invariant object, array and prototype facts inside
the existing no-helper, numeric-write-only region. Numeric values can still
change through aliases; shape identity alone does not prove per-instance
attributes; neither may be cached without stronger proof. Register allocation
remains relevant but should follow measured probe/guard attribution rather than
an assumption that wide shadow copies dominate the entire loop.

Independent inspection of pointer setup and probe templates attributes 292 body
samples to object exotic/plain/shape checks and 204 to entry length/accessor
checks: together 33.9% of body samples. Prototype-link loads account for another
40 samples and packed property value loads for 44. These counts identify sampled
instructions, not their latency or a guaranteed optimization benefit.

The bounded next candidate is reusing proofs for the same receiver and stable
object chains within one native-region invocation. The admitted numeric-only
stores preserve object edges, descriptors, prototype links and array layout.
Varying receivers must retain per-instance checks, and possibly aliasing writes
require numeric payloads to reload. Every exit discards the proofs; reentry must
validate them again. The external `mixed-loop-invariant-guards-design.md` records
the proposed analysis and correctness checks; it is not yet implemented.

Raw workload, sample, maps, summary, code words/disassembly and traffic JSON are
archived under `delta-iterator-step-profile*` and `delta-native-*`; diagnostic
source, tests and binary are in `mixed-native-ranges-candidate`. This is new
profiling evidence, not an additional speedup or achievement of the Node/Bun goal.

### Invocation-local property-entry proofs (2026-09-08)

The next candidate addresses repeated own-property and method validation within
the existing mixed native loop. A conservative SSA provenance pass identifies
reads from `this`, unchanged entry locals and conditional Object-valued property
chains. Copies preserve identity; non-header phi parameters qualify only when
every admitted incoming identity agrees. Varying array elements do not become
invariant receivers. This analysis selects profitable sites; it never substitutes
for runtime receiver identity or the initial live descriptor/prototype checks.

Up to 32 selected read sites receive borrowed `(receiver, entry address)` pairs
after the existing native shadow/control area. Every region invocation clears
the receiver words. A first read or receiver mismatch runs the existing full
probe and publishes the pair only after all guards succeed. A receiver match
reuses the validated entry address, reloads its current packed payload and keeps
the existing Number/Object decode guards. Numeric values are never memoized.
Method inlining still executes its original target guard. Dense-element probes
are unchanged.

This relies on the region's helper-free effect contract: admitted heap writes
only replace an existing ordinary Number with a Number. They cannot change entry
storage, descriptors, prototypes, object edges or array layout. A separate
explicit opcode effect whitelist disables all proofs if future supported effects
have not opted into that stronger contract. Original physical owners retain the
borrowed graph. Every normal, guard or budget exit discards the proof area;
publication still clones only live shadow locals and operands before releasing
displaced owners. The maximum frame is 928 bytes, including all 32 proof pairs.

`LUMEN_JIT_NO_MIXED_READ_PROOFS=1` disables selection and extra frame allocation
for same-binary comparisons. Release code has no new execution counters. Tests
use a hit counter to prove execution, including a bound that requires all 32
cells to be used across a 1,100-iteration budget crossing. Five analysis tests
cover equivalent and conflicting identities, mutable roots, method outputs and
the site cap. Five runtime tests cover numeric aliases, differing receivers,
readonly data reads, descriptor/getter and object-graph changes with GC,
array resizing, method/prototype replacement, exact fallback effects and the
maximum frame. They pass across all tiers and with the feature disabled.

Validation passes 748 unit and 34 integration tests. The broad conformance run
passes 21,047/21,049 with the same lexical-arguments and dynamic-import failures;
differential testing agrees on 1,996 cases with four budget skips. Formatting and
the strict module audit pass. Clippy's error-message multiset exactly matches
the existing 86-library/88-library-test baseline; it is not a clean Clippy run.

The same release binary (`db194412a2dd351716dad42662ccf76e7434519b6cd921c78379d9e4c7c1c9d1`)
completed 48 sequential, verified timings in three rotated rounds. The first
36 compare proof reuse off/on with Node 24.18.0 and Bun on the existing kernels,
10,000-parse Djot workload and 5,000-iteration standalone Delta workload:

| Workload (lower is better) | Off median | On median | Time change | Paired time changes |
| --- | ---: | ---: | ---: | --- |
| ObjectArray, µs / 20,000 visits | 237.5 | 228 | -4.00% | -0.87%, -4.00%, -9.02% |
| PolymorphicMethods, µs / 20,000 visits | 305 | 290 | -4.92% | -5.00%, -4.92%, -4.92% |
| Djot, ms | 3493 | 3509 | +0.46% | +0.18%, +0.29%, +0.77% |
| DeltaBlue, ms | 8817 | 8320 | -5.64% | -6.02%, -9.99%, -5.40% |

Every kernel and standalone Delta pair improves. Every parser pair is slower;
no parser gain is claimed. Twelve additional classic-suite runs give these
medians (scores are higher-is-better):

| Benchmark | Off | On | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23568 | 23412 | 65994 | 72913 |
| DeltaBlue | 3782 | 3947 | 154187 | 111085 |
| Crypto | 24008 | 24067 | 92037 | 120066 |
| RayTrace | 6438 | 6505 | 136750 | 307169 |
| EarleyBoyer | 3641 | 3605 | 146883 | 159394 |
| RegExp | 1642 | 1655 | 22547 | 30973 |
| Splay | 10325 | 10390 | 80749 | 96355 |
| NavierStokes | 38321 | 38655 | 70935 | 72122 |
| Score (version 7) | 8682 | 8750 | 83617 | 100229 |

Classic Delta improves 4.36%, with all three pairs improving 4.34–4.56%.
Richards declines 0.66%, with all pairs declining 0.59–0.66%. The composite
median improves 0.78%, but pairs are mixed (-0.31%, +0.98%, +0.48%); this is not
a consistent overall-suite gain. Other nonzero score directions are mixed,
including EarleyBoyer's 0.99% median decline. The candidate is retained for
repeatable standalone and classic Delta gains, with these counter-results
preserved. Current composite score ratios remain 9.56× Node / 11.45× Bun, and
parser time ratios remain 15.39× / 24.37×. The within-2× goal is not achieved.

After every timing process finished, a separate assertion-preserving diagnostic
enabled mixed-loop counters and region logging in the same binary. Delta emits
one 113-op native plan with nine proof sites, 549,987 entries and 50,651,451
backward jumps; exits are 549,986 at PC134 and one at PC118. Djot emits **zero
mixed-loop plans**. Thus direct proof-cache execution or frame initialization
does not explain the observed parser difference. The timing difference remains
recorded, without an unsupported causal attribution. Diagnostic elapsed times
are not benchmark evidence.

Both schedules, strict outputs, medians and executable/driver/workload hashes
were independently audited. Source, timing rows, summaries, diagnostic logs and
validation are archived under `mixed-read-proofs-*` and `lumen-mixed-read-proofs-*`
in the external optimizer directory. The follow-up dense-element layout design
is still only a proposal; its small static load savings are not a measured gain.

### Builtin array construction from owned values (2026-09-08)

A fresh profile of f6ac3e6 runs 40,000 fully verified Djot parses and samples six
seconds after a two-second startup delay, with same-process JIT maps. Engine and
sampler exit zero; the output verifies 12,640,000 HTML characters. One main-thread
root contains 4,638 samples, and exclusive residuals reconcile to that total.
Regex execution has 463 ancestry samples (9.98%), partitioned without overlap
into matching 95 (2.05%), `make_array`/`set_data` construction 215 (4.64%), and
other descendants 153 (3.30%). GC ancestry is 630 (13.58%), including 198 direct
collection samples; these must not be added. Array-iterator builtin ancestry is
108 (2.33%), while the fast-yield helper has 94 (2.03%). Older iterator ancestry
figures no longer establish the current bottleneck. Inlining can hide helper
boundaries, and these counts do not measure individual allocation latency.

The profile exposes an existing construction mismatch. Small JIT array literals
already use a direct packed constructor, but builtin-created arrays use repeated
insertion and separately allocated packed buffers. Regex capture-index pairs and
`Object.entries` pairs are examples. `Interp::make_array` now routes lengths
1–32 through a shared owned-value constructor. Lengths 1–10 use existing inline
packed storage; lengths 11–32 retain the existing heap-packed representation.
Empty arrays and lengths above 32 retain the previous construction path.
`LUMEN_NO_COMPACT_BUILTIN_ARRAYS=1`, read once per process, disables this selection.

Array construction moves into `interpreter/arrays.rs`; packed property-map
construction moves into `value/props/array_builder.rs`. The existing raw JIT
constructor delegates through the same bounded builder. Its initialized values
are moved exactly once. The safe constructor consumes an exact-size iterator,
and inline storage advances its initialized length after each write so unwinding
releases completed elements. The length shape, descriptors, prototypes, hole
semantics, indexed ordering and GC edges retain their existing representation.
Regex strings and observable result properties are still materialized normally.

There is a known performance tradeoff: some generated element reads recognize
heap-packed storage but not inline storage. A new small builtin array can take
the existing helper fallback until mutation or numeric-region preparation
promotes its storage. Rust lookup, reflection and GC already support both forms.
Direct native reads of inline packed elements are a separate proposed follow-up,
not part of this change or its measured gains.

Validation passes 754 unit and 34 integration tests, including 103 focused array
tests. The two new runtime tests also pass with the feature disabled. New tests
cover storage boundaries, duplicate owners, construction unwinding, holes versus
Undefined, reflection, growth/truncation, getter changes, GC-held aliases, regex
capture descriptors and Unicode indices. Expanded before/after conformance adds
all Array and RegExp tests: both binaries pass 26,007/26,009 with identical full
failure lists (the two existing lexical-arguments/dynamic-import failures).
Differential testing agrees on 1,996 cases with four budget skips. Formatting and
new-module audits pass; Clippy exactly matches the existing error-message
multiset, rather than passing cleanly.

Sixty sequential timing runs cover three rotated rounds of the retained before
binary, candidate disabled/enabled, Node v24.18.0 and Bun 1.3.14. All outputs are
verified; source archives, executable hashes, schedules and raw rows are kept.
The primary comparison is before versus enabled, including the shared raw
constructor refactor. No builds, tests or profiling overlap these timings.

| Workload (lower is better) | Before | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Capture indices, µs/2,000 calls | 1460 | 1460 | 1340 | 200 | 140 |
| Capture strings, µs/2,000 calls | 760 | 780 | 760 | 52.8 | 11 |
| Object entries, µs/2,000 calls | 890 | 910 | 810 | 154.29 | 108.89 |
| String split, µs/2,000 calls | 386.67 | 386.67 | 380 | 34.8 | 18.13 |
| Djot 10,000 verified parses, ms | 3526 | 3530 | 3460 | 230 | 145 |
| Delta 5,000 verified iterations, ms | 8257 | 8310 | 8326 | 197 | 307 |

Before/enabled capture-index time improves 8.22% and object-entry time improves
8.99%, with all three pairs improving. Capture strings are flat; split improves
1.72%, with two improving pairs and one tie. Djot improves 1.87%, with all pairs
improving 1.87–2.61%. Its third round is slower across every engine; all rows
remain included without assigning a host-level cause. Standalone Delta regresses
0.84%, with every pair slower (0.52–3.15%). Disabled/enabled parser time improves
1.98%; the larger third-round disabled comparison is not the primary evidence.

| Classic benchmark (higher is better) | Before | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Richards | 23504 | 23635 | 23501 | 65814 | 72298 |
| DeltaBlue | 3947 | 3908 | 3947 | 152138 | 109531 |
| Crypto | 23852 | 23937 | 24033 | 92204 | 119913 |
| RayTrace | 6364 | 6460 | 6441 | 136010 | 307317 |
| EarleyBoyer | 3600 | 3644 | 3648 | 146671 | 158336 |
| RegExp | 1629 | 1650 | 1655 | 22729 | 30730 |
| Splay | 10284 | 9812 | 9958 | 80562 | 95727 |
| NavierStokes | 37656 | 37581 | 38173 | 70420 | 71606 |
| Score (version 7) | 8661 | 8660 | 8674 | 83669 | 99588 |

The composite median improves only 0.15%, with mixed pairs (-0.13%, -1.00%,
+4.04%); this is not a consistent overall-suite gain. Splay's median declines
3.17%, also with mixed pairs (-4.91%, -3.68%, +4.54%). The change is retained for
repeatable parser, capture-index and object-entry gains, with these regressions
preserved. Current composite ratios are 9.65× Node / 11.48× Bun; Djot time ratios
are 15.04× / 23.86×. The within-2× goal remains unmet. These ratios describe these
workloads, not general engine equivalence, and gains from separate experimental
batches must not be added into a cumulative percentage.

Evidence is archived under `compact-builtin-arrays-*` and
`lumen-compact-builtin-arrays-*` in the external optimizer directory, including
the fresh profile, conformance comparison, source snapshots and validation logs.

### Rejected empty-literal storage omission (2026-09-08)

A candidate removed the unused dense-storage box from raw JIT `[]` construction,
leaving existing mutations to allocate it lazily. It preserved array length,
shape, descriptors and mirror flags, with an independent process disable switch.
Native reads already check absent storage, and array objects remain excluded
from named-property creation ICs by the outer ordinary-object type guard.

The candidate passed 756 unit and 34 integration tests, including actual compiled
omission counters, owned/raw construction, lazy growth, ownership, prototype
accessors, freezing and GC cycles. All six focused construction tests also passed
with omission disabled. Expanded conformance matched the retained binary exactly:
26,007 passes, two known failures, zero skips and identical full failure lists.
Differential testing agreed on 1,996 cases with four budget skips. Formatting and
module audit passed; Clippy matched its existing error-message multiset.

Forty-five sequential verified timing runs used three rotated rounds of the
retained 498896d binary, candidate disabled/enabled, Node v24.18.0 and Bun 1.3.14.
No builds, tests or profiling overlapped timing. The primary comparison includes
the whole change, using the retained executable rather than only flag-off.

| Workload (lower is better) | Retained | Disabled | Candidate | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Retained empty arrays, µs/2,000 | 124.44 | 133.33 | 120 | 9.8 | 8.27 |
| Empty arrays with named property, µs/2,000 | 232 | 242.5 | 228 | 12 | 23.2 |
| Empty arrays immediately pushed, µs/2,000 | 300 | 310 | 305 | 20.8 | 10.4 |
| Djot 10,000 verified parses, ms | 3516 | 3523 | 3517 | 230 | 145 |
| Delta 5,000 verified iterations, ms | 8304 | 8386 | 8331 | 195 | 306 |

Empty-array retention improves 3.57% with all pairs improving 3.57–5.88%; adding a
named property improves 1.72% with two wins and one tie. Immediate push regresses
1.67%, with two losses and one tie. Djot has no gain: its median increases 0.03%,
with all pairs slower by 0.03–0.95%. Delta's median increases 0.33%, with mixed
pairs (-0.05%, +1.03%, +0.33%). The larger disabled/enabled allocation gain does not
represent the full change against the retained binary.

The candidate is rejected: a targeted allocation win does not compensate for
absent parser/application progress and the immediate-growth regression. Its
source and test additions are archived, and production code is restored exactly
to 498896d. The planned classic-suite runs are unnecessary for this rejection and
were not started; no new classic-suite claim follows from this experiment.
Sources, binaries, hashes, schedules, raw results and validation logs remain under
`empty-array-sidecar-*` and `lumen-empty-array-sidecar-*` in the external optimizer
directory. Native reads of inline packed values are the next candidate, with
confirmed Djot capture-index reads currently falling back to helpers.

### Native reads of inline packed elements (2026-09-08)

Djot's verified `find` path reads both endpoints of a regex capture-index pair.
The compact builtin constructor stores such pairs inline, but the general JIT
read templates previously recognized only heap-packed or classic storage, so
these reads fell back to helpers. A shared `jit/packed_element.rs` selector now
serves stack and local-keyed reads. It prefers existing heap-packed storage,
then checks the live inline initialized length and computes a property address.
Zero inline length selects classic storage; an out-of-bounds inline index exits
to the original helper. `LUMEN_JIT_NO_INLINE_PACKED_READS=1` disables the new arm
at code-generation time and preserves the previous heap/classic instructions.

Inline offsets are derived using nested Rust `offset_of!` operations in the
storage owner. Encoding gates validate the offsets and existing property stride.
The selector changes only x14/x15, preserving the index, dense base and receiver.
Callers retain descriptor checks, hole/prototype fallback, packed-value decoding,
owner acquisition and subsequent receiver release. No helper, GC or storage
mutation occurs between selection and decoding, and each new read reloads the
current representation. Numeric-region heap-header preparation and writes retain
their existing contracts. A heap hit gains a branch over the inline arm, so
regression measurements must include existing heap-backed consumers.

Colocated tests separately count actual stack/local inline address selections;
these counters run after bounds checks but before descriptor/value decoding, so
they are not counts of completed loads. The fixtures also verify resulting values,
lengths 1/10/11/32, strings, Undefined, NaN, BigInt fallback, readonly properties,
alias ownership, GC, sparse/prototype/accessor fallback, shrinking and promotion,
and proxy or invalid-index reads. The independent source review found no blocker.

Validation passes 756 unit and 34 integration tests, including both new runtime
tests; the two also pass with native inline reads disabled. Formatting and the
new module/storage audits pass. Clippy exactly matches the existing error-message
multiset. Release compilation succeeds for Lumen and both validation tools.

Expanded before/after conformance matches exactly: 26,007 passes, two known
failures, zero skips and identical full failure lists. Differential testing
agrees on 1,996 cases with four budget skips.

The initial 45 verified workload runs use three rotated rounds of retained
498896d, disabled/enabled candidate, Node v24.18.0 and Bun 1.3.14. Source snapshots
include all Rust files and executable hashes. No builds, tests or profiling
interleave with timing. Full-change before/enabled comparisons are primary.

| Workload (lower is better) | Before | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Capture indices, µs/2,000 calls | 1380 | 1340 | 1160 | 200 | 141.43 |
| Capture strings, µs/2,000 calls | 770 | 760 | 700 | 53.53 | 11.2 |
| Object entries, µs/2,000 calls | 810 | 800 | 630 | 154.29 | 108.89 |
| String split, µs/2,000 calls | 380 | 380 | 315 | 34.8 | 18.4 |
| Djot 10,000 verified parses, ms | 3508 | 3507 | 3467 | 230 | 146 |
| Delta 5,000 verified iterations, ms | 8271 | 8331 | 8331 | 195 | 309 |

All three full-change pairs improve for each array kernel: median gains are
15.94%, 9.09%, 22.22% and 17.11%, respectively. Djot's median improves 1.17%, but
its pairs are mixed (-1.31%, -1.94%, +0.37%); this is not an all-round parser gain.
Standalone Delta regresses 0.73%, with every pair slower (+0.19%, +2.62%, +0.73%).
Disabled/enabled Delta is flat by median and mixed by pair; it does not replace
the old-binary regression result. Initial parser time ratios are 15.07× Node and
23.75× Bun. The classic comparison follows below.

Fifteen additional sequential runs complete the classic comparison (60 total).

| Classic benchmark (higher is better) | Before | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Richards | 23431 | 23506 | 23501 | 65489 | 71818 |
| DeltaBlue | 3894 | 3947 | 3934 | 154379 | 109346 |
| Crypto | 23837 | 24031 | 24017 | 90863 | 118741 |
| RayTrace | 6380 | 6460 | 6447 | 134604 | 304061 |
| EarleyBoyer | 3634 | 3604 | 3648 | 146365 | 155724 |
| RegExp | 1651 | 1645 | 1645 | 22843 | 30730 |
| Splay | 9877 | 9958 | 9803 | 81238 | 94831 |
| NavierStokes | 38101 | 37990 | 38139 | 70193 | 60931 |
| Score (version 7) | 8637 | 8639 | 8669 | 83475 | 96636 |

The full-change composite improves 0.37%, with every pair improving 0.32–0.56%.
Crypto and RayTrace also improve in every before/enabled pair, with median gains
0.76% and 1.05%. Other individual score directions are mixed. Splay's median
falls 0.75%, and RegExp falls 0.36%; these counter-results remain included. The
same-binary disabled/enabled composite improves 0.35% by median but has mixed
pairs, so the full-change result is not a precise attribution to the inline arm.

The change is retained for repeatable array-workload gains and a small consistent
full-change suite improvement. The parser result remains mixed, and standalone
Delta's regression is accepted explicitly; no broad application-speedup claim
follows. Current composite ratios are 9.63× Node / 11.15× Bun, while parser
time ratios remain 15.07× / 23.75×. The within-2× goal is not achieved. Source,
binary hashes, schedules, strict outputs, summaries and validation are archived
under `inline-packed-reads-*` and `lumen-inline-packed-reads-*` in the external
optimizer directory. The next collector counting-pass proposal remains unmeasured.

### Rejected borrowed object-edge counting (2026-09-08)

A candidate counted object edges directly through immutable borrows, avoiding
scratch-vector writes and temporary reference-count increments/decrements in the
collector's counting pass. Checked packed Object payloads used scoped
`ManuallyDrop<Gc>` handles; prototype, data, getter/setter and bound-function
physical edges retained duplicates. Marking, scope traversal order, root tests,
pin accounting, registry restoration and sweeping retained the existing paths.
An independent `LUMEN_NO_BORROWED_GC_COUNT=1` comparator selected old counting.

It passed 760 unit and 34 integration tests, including 17 focused GC tests. New
coverage verified exact physical counts, unchanged owners, self/bound/accessor
edges, packed duplicates/scalar exclusion, actual enabled/disabled counting calls,
closures/mapped arguments/JIT aliases, pinned unreachable-cycle reclamation,
external aliases and registry reuse after sweep. Both runtime tests passed with
the path disabled. Expanded conformance matched the retained binary exactly:
26,007 passes, two known failures, zero skips and identical full failure lists.
Differential testing agreed on 1,996 cases with four budget skips. Formatting and
module audits passed; Clippy matched its existing error-message multiset.

Forty-five sequential verified timings used three rotated rounds of retained
bbe471f, disabled/enabled candidate, Node v24.18.0 and Bun 1.3.14. No builds,
tests or profiling overlapped timing. All source and binary hashes were preserved.

| Workload (lower is better) | Retained | Disabled | Candidate | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Capture indices, µs/2,000 calls | 1160 | 1180 | 1180 | 200 | 145.71 |
| Capture strings, µs/2,000 calls | 700 | 710 | 700 | 52.35 | 11.4 |
| Object entries, µs/2,000 calls | 630 | 630 | 630 | 157.14 | 108.89 |
| String split, µs/2,000 calls | 315 | 320 | 320 | 34.8 | 18.4 |
| Djot 10,000 verified parses, ms | 3486 | 3539 | 3507 | 230 | 144 |
| Delta 5,000 verified iterations, ms | 8312 | 8347 | 8287 | 195 | 307 |

Full-change Djot median time increases 0.60%, with mixed pairs (-7.60%, +0.60%,
-0.61%). The first retained parser run is 3,802 ms, versus 3,486 and 3,471 ms in
later rounds; all results remain included without assigning a cause. Candidate
runs are 3,513, 3,507 and 3,450 ms. Disabled/enabled parser time improves 0.90%
with all pairs improving, but that is not a full-change win against the retained
executable. Delta improves only 0.30% by median, with mixed pairs (-0.45%, -1.16%,
+0.41%). Capture indices regress 1.72% (two losses/one tie); capture strings and
object entries are flat by median, and split regresses 1.59% with mixed pairs.

The candidate is rejected because it does not establish repeatable target-workload
progress. Production code is restored exactly to bbe471f. The planned classic
runs were not started; no new classic-suite claim follows. Candidate sources,
including the untracked runtime module, binaries, timing rows, summaries and
validation remain archived under `borrowed-gc-count-*` and
`lumen-borrowed-gc-count-*` in the external optimizer directory. A fresh retained
engine profile should reassess remaining application costs before another GC
change; removed operations alone were not enough to justify this candidate.

### Fresh retained parser profile and iterator-result target (2026-09-08)

After rejecting borrowed GC counting, a fresh diagnostic profiles the retained
bbe471f executable, verified against its recorded binary hash. Forty thousand
parses verify 12,640,000 HTML characters. The sampler starts after two seconds
and records six seconds; engine and sampler exit zero. Instrumented elapsed time
is not benchmark evidence. Independent tree analysis reconciles 5,130 samples
from one main thread, with no negative or unaccounted exclusive residuals.

Regex execution has 432 ancestry samples (8.42%), partitioned without overlap
into matching 103 (2.01%), result-array/property construction 169 (3.29%) and other
execution descendants 160 (3.12%). GC has 661 ancestry samples (12.88%), including
216 direct collector samples; these are not additive or an estimate of removable
counting overhead. Name-path helpers have 207 exclusive samples (4.04%). Separate
disjoint leaf categorization assigns 494 samples (9.63%) to destruction and 398
(7.76%) to allocator code. Symbol inlining and category boundaries limit causal
attribution; these totals do not predict gains from one allocation change.

All 1,160 unknown native-code leaves (22.61%) map into the same-process JIT ranges.
The largest generated chunk contributes 197 samples (3.84% of the whole profile),
so there is no single tiny arithmetic site dominating execution. Mapped families
include CallWithThis 152, GetPropLocal 97, GetProp 88, GetMethod 86 and LoadName 54.
These are last-start instruction spans, not perfect opcode boundaries: the final
ReturnUndef span can include outlined tails and must not be treated as return cost.

Helper attribution uses the nearest JIT ancestor above the helper boundary and
return address minus four. It distinguishes direct helper leaves from ancestry
that includes descendant JavaScript. The input/options parser's IterStepL212 has
52 direct destruction samples and 16 direct dispatch samples; its much larger
dispatch ancestry is not dispatcher self time. The main parser's IterStepL179 has
27 direct property samples. A large call48 allocation/destruction attribution
overlaps collection and cannot be assigned to that JavaScript callee's allocations.

Source inspection resolves a concrete remaining protocol path. Djot's
`EventParser[Symbol.iterator]` returns a custom object with `next()`, which returns
fresh `{value: ..., done: false}` results consumed by `for (const event of parser)`.
`Interp::iterator_step` in eval.rs still calls generic `get_member` for `done` and,
when false, `value`. The retained array-iterator specialization does not apply to
this custom iterator. A bounded next candidate is a pure own-data result probe:
ordinary receiver checks, existing truthiness for done, no value access when done,
and exact ordered fallback for missing/accessor/exotic cases. Cloning the selected
value first avoids mixing protocol lookup optimization with unique-owner mutation.
This is a proposal, not an implemented or measured improvement.

A native deeper-name-cache design is also archived, but template binding storage
needs a stable owner-defined native view; generation zero alone does not prove its
Rust enum layout. Iterator-result lookup is therefore the next implementation
priority. Profile maps, raw samples, reproducible analyzers, independent analysis,
JIT attribution and binary validation are archived under
`djot-inline-reads-current-*`, `djot-inline-jit-*` and related scripts in the external
optimizer directory. This diagnostic changes the next action, not the measured
Node/Bun gap or the within-2× completion status.

### Direct ordinary iterator-result reads (2026-09-08, rejected)

The rejected candidate made generic `iterator_step` probe an ordinary own-data result after calling
`next` and validating that its return is an object. The new `eval/iterator_result`
module checks side-table exotic exclusions and the object's ordinary/plain state,
requires own data `done`, and uses existing truthiness including HTMLDDA. Truthy
`done` returns completion without even probing `value`. Falsy `done` requires own
data `value`, which is cloned while its result object remains borrowed and alive.
An outer miss leaves generic ordered property access unchanged. Accessors, absent
own fields and exotic receivers retain normal behavior; no callback or mutation
runs inside the probe. `LUMEN_NO_DIRECT_ITERATOR_RESULT=1` disables it per process.

Colocated all-tier tests assert exact completed-probe counts while checking falsy
and truthy values, GC-held aliases, skipped value getters, getter mutation, thrown
done/value getters, proxy trap order and inherited fields. The implementation
clones results normally; unique-owner extraction and result allocation elimination
are not part of this candidate. Independent source review found no blocker.

Validation passes 759 unit and 34 integration tests. The three new runtime tests
also pass with the feature disabled. Formatting/module checks pass; Clippy matches
the existing error-message multiset exactly.

Expanded before/after conformance matches exactly: 26,007 passes, two known
failures, zero skips and identical full failure lists. Differential testing
agrees on 1,996 cases with four budget skips.

Forty-five sequential verified workload runs use three rotated rounds of retained
bbe471f, candidate disabled/enabled, Node v24.18.0 and Bun 1.3.14. Source snapshots,
executable hashes and strict outputs are preserved. No builds, tests or profiling
overlap the timing runs. Full-change retained/enabled comparisons are primary.

| Workload (lower is better) | Retained | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fresh results, µs/2,000 values | 212 | 212 | 174.29 | 4.16 | 4.6 |
| Shared results, µs/2,000 values | 180 | 177.14 | 142.5 | 2 | 1.8 |
| Accessor results, µs/2,000 values | 780 | 770 | 790 | 24 | 4.16 |
| Djot 10,000 verified parses, ms | 3492 | 3488 | 3474 | 229 | 146 |
| Delta 5,000 verified iterations, ms | 8309 | 8341 | 8318 | 196 | 305 |

Fresh and shared result medians improve 17.79% and 20.83%, with all three pairs
improving. Accessor results regress 1.28%, with two slower pairs and one tie.
Djot's median improves 0.52%, but its pairs are mixed (+0.63%, -0.52%, -0.26%).
Delta's median regresses 0.11%, also mixed (+1.95%, -0.18%, -0.70%). These results
do not establish a broad application gain. Candidate parser time ratios were
15.17× Node / 23.79× Bun.


The final 15 classic-suite runs complete the 60-run batch:

| Classic score (higher is better) | Retained | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Richards | 23457 | 23557 | 23437 | 66039 | 72178 |
| DeltaBlue | 3963 | 3958 | 3852 | 153857 | 111144 |
| Crypto | 24045 | 23956 | 24008 | 92424 | 119672 |
| RayTrace | 6412 | 6454 | 6473 | 137860 | 303691 |
| EarleyBoyer | 3601 | 3642 | 3631 | 147975 | 159602 |
| RegExp | 1645 | 1627 | 1604 | 22866 | 30821 |
| Splay | 9901 | 9877 | 9844 | 81972 | 95059 |
| NavierStokes | 38359 | 37842 | 38469 | 70787 | 71457 |
| Score (version 7) | 8661 | 8690 | 8637 | 84060 | 99277 |

The composite median declines 0.28%; paired changes are -1.63%, -0.28%
and +0.80%. RegExp declines 2.49%, with all three pairs worse. Focused iterator
improvements do not justify retaining this change given the mixed parser results
and suite regressions. The candidate is archived and production source is restored
exactly to the retained implementation. All runs remain in the evidence; no host
load explanation is assumed.

In this batch the retained engine scores 8661 against Node's 84060 and Bun's
99277: 9.71× and 11.46× higher reference scores. Retained Djot takes 3492 ms
against 229 ms and 146 ms: 15.25× and 23.92× longer. These are workload-specific
comparisons, and the within-2× goal remains unmet. Artifacts, candidate binaries,
source snapshots, rejected source, validation and independent review are under
`direct-iterator-result-*` in the external optimizer directory.


### Native deeper-name coverage (2026-09-08, diagnostic)

A temporary instrumented build of retained production source counts only successful
`name_path::jit::load_cached` lookups, after the checked path returns its live value.
It classifies the successful cache's holder, guard count, layout-guard count and
BigInt value status. It does not count fills, misses, direct native NameIc hits or
all lexical accesses. The helper and diagnostic inspect the same cache without JS
or mutation between them. Counters dump every category at process exit.

The verified 10,000-parse Djot workload completes with 3,160,000 HTML characters:

| Completed helper-hit category | Count | Share |
| --- | ---: | ---: |
| Exact binding, all guards exact | 12,000,361 | 85.36% |
| Exact binding, at least one layout guard | 1,378,940 | 9.81% |
| Global property holder | 680,000 | 4.84% |
| Template binding holder | 0 | 0% |
| Total | 14,059,301 | 100% |

The exact-only category spans two, four, six and seven guards. No successful hit
returned BigInt. This is measured workload coverage, not a reason to omit BigInt's
checked fallback. Delta's verified 5,000-iteration workload exits successfully and
records zero successful deeper-name helper hits. Its bottleneck needs other work.

This evidence changes the next implementation from a broad template/native-binding
view to exact-scope guards first: stable per-cache publication, live parent-chain
identity and generation checks, live binding initialization/import checks, and reuse
of the existing native value decoder. Layout and global paths keep the checked
helper. This avoids adding fields and mutation bookkeeping to every VarMap for a
first candidate that can cover 85.36% of measured successful parser helper hits.
The profile attributes about 4% of parser samples to the name-path family; neither
that attribution nor coverage predicts the resulting application speedup.

The release diagnostic binary hash is
`b6ae3aca09365d9cb38f6fada5e427a6e59b99ca2f0020acbe1c13beab87e5c4`.
Only `LUMEN_NAME_PATH_COVERAGE=1` and `LUMEN_JIT_OPSTAT=1` are enabled after clearing
inherited LUMEN flags. Instrumented elapsed times are not performance comparisons.
The original driver incorrectly required nonempty coverage rows and failed after
Delta had passed exit-code and strict-output checks. That driver and traceback are
preserved; zero was finalized from the existing output without rerunning the engine.
The corrected driver accepts zero. Source snapshots include the untracked diagnostic
module; raw outputs, binary, source patch and metadata are archived under
`name-path-coverage-*` in the external optimizer directory. Instrumentation is removed
from production source. No new production speedup or correctness-suite result is
claimed by this diagnostic; the within-2× target remains unmet.


Independent artifact review verifies counts, source/binary/workload hashes and the
zero-hit finalization. Owner review identifies a remaining ABI boundary:
`Binding.import_ref` is an `Option<(Env, String)>` without a promised native tag
layout, and mutable binding access can change it without bumping generation.
The first implementation should keep a small checked final-binding helper for live
initialization/import validation while moving the repeated exact scope traversal
into generated code. It must also reject conflicting live RefCell borrows. This
preserves the current checks without guessing Rust enum storage or widening the
binding-map mutation contract. The helper's residual cost must be measured in the
full before/after comparison.


### Native exact name paths (2026-09-08, rejected)

The rejected candidate moved exact lexical scope traversal into generated AArch64 code.
Each name cache owns a stable 144-byte record in a fixed boxed slice. Existing
NamePath weak owners keep cached scope identities from being recycled. Fill clears
publication before replacing weak owners and publishes a count only after all exact
guards and the binding pointer are installed; layout/global paths remain inactive.
The same record storage exists with `LUMEN_JIT_NO_NATIVE_NAME_PATHS=1`, so the retained
binary comparison includes allocation and publication costs absent from the off/on
comparison.

On a direct NameIc miss, an outlined probe checks the live complete chain: identity,
nonnegative RefCell borrow state, absence of a with ancestor, and VarMap generation
at every hop. Owner-probed parent offsets and Rc conversion walk only strongly owned
live ancestors; cached weak identities are never dereferenced. A small checked Rust
helper validates live initialized/import state and rejects BigInt, then returns a
borrowed Value pointer. The existing decoder acquires output ownership and supplies
Undefined for free-name call receivers. No callback, GC or output mutation precedes
a failing guard. Direct NameIc hits retain their original instruction path.

Direct machine-code tests exercise late publication, scope mutation/removal, changed
ancestors, shared/exclusive borrows, with state, TDZ, import mutation, eight scopes
and over-depth invalidation. An all-tier JavaScript fixture uses stable eval-created
ancestors to ensure actual native hits, checks live scalar and owned values, GC and
call receivers, and confirms zero hits with the feature disabled. Tests pass:
764 unit plus 34 integration. Formatting and the four-module structural audit pass;
Clippy's error-message multiset matches the existing baseline exactly.

Expanded retained/candidate conformance matches: 26,007 passes, two known failures,
zero skips and identical full failure lists. Differential testing agrees on 1,996
cases with four budget skips. Candidate binary/source hashes, reviews, test outputs
and comparison drivers are archived under `native-name-path-*` in the external optimizer directory.


The initial focused timing batch stopped after five runs: Bun's module execution
context did not expose eval-created `var` bindings used by the fixture. All original
rows, driver, workload, source snapshot and terminal failure are preserved in
`native-name-path-failed-eval-context`. The corrected fixture explicitly creates a
sloppy function with the standard Function constructor, then validates on retained
Lumen, candidate Lumen, Node and Bun before a fresh timing batch. The failed batch
is excluded from comparisons and is not silently replaced or treated as a timeout.


The corrected 45-run workload batch uses three rotated rounds of retained bbe471f,
candidate off/on, Node v24.18.0 and Bun 1.3.14. All outputs are strictly verified;
no builds, tests or profiling overlap the accepted timings.

| Workload (lower is better) | Retained | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Numeric lookup, µs/2,000 iterations | 98.00 | 100.00 | 100.00 | 152.50 | 115.56 |
| Object lookup, µs/2,000 iterations | 120.00 | 120.00 | 110.00 | 228.00 | 168.57 |
| Call lookup, µs/2,000 iterations | 162.86 | 165.00 | 140.00 | 393.33 | 313.33 |
| Djot 10,000 verified parses, ms | 3391.00 | 3398.00 | 3421.00 | 225.00 | 142.00 |
| Delta 5,000 verified iterations, ms | 8283.00 | 8245.00 | 8274.00 | 194.00 | 301.00 |

Full-change object and call medians improve 8.33% and 14.04%, with all three pairs
improving. Numeric lookup regresses 2.04%, with all three pairs worse; same-binary
off/on is exactly flat on that kernel. This does not establish native execution
coverage for every kernel.

Djot's median regresses 0.88%, with mixed pairs (-0.27%, +1.03%, -0.49%). Delta's
median improves 0.11%, also mixed (-0.25%, -0.11%, +3.26%). All raw rows remain,
including Delta's slower third-round disabled/enabled measurements. No causal host
load explanation is assumed. These kernel wins alone do not justify retention.


All 15 classic runs are complete:

| Classic score (higher is better) | Retained | Disabled | Enabled | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| Richards | 23443 | 23603 | 23649 | 66025 | 72803 |
| DeltaBlue | 3974 | 3934 | 3910 | 152805 | 109967 |
| Crypto | 23902 | 23962 | 23943 | 92019 | 119488 |
| RayTrace | 6499 | 6441 | 6351 | 135566 | 306651 |
| EarleyBoyer | 3592 | 3565 | 3629 | 147434 | 159991 |
| RegExp | 1655 | 1629 | 1640 | 22570 | 30700 |
| Splay | 9917 | 9746 | 9746 | 80472 | 95214 |
| NavierStokes | 38436 | 38139 | 38469 | 70490 | 71826 |
| Score (version 7) | 8670 | 8658 | 8640 | 83705 | 99635 |

The composite declines 0.35%, with every pair worse (-0.36%, -0.74%, -0.02%).
RayTrace declines 2.28%, Splay 1.72%, and RegExp 0.91%, each with all pairs worse.
Richards improves 0.88% and Crypto 0.17%, each with all pairs better. These gains
and focused object/call wins do not offset inconsistent parser results and the
composite regression. The implementation is archived and rejected; production source
is restored exactly to the retained implementation. No new engine gain is claimed.


A separately built diagnostic after all timing runs verifies 12,000,361 accepted
native binding reads in the full Djot workload. The same diagnostic binary with the
feature disabled records zero; Delta also records zero. All three runs exit normally
and satisfy their output checks. These counts exactly match the earlier eligible
exact-only parser category, so the intended paths were reached. They do not establish
a speedup: the retained/candidate timing comparison still rejects this implementation.

Diagnostic binary hash:
`42c16515a7d955ef00fb7ac43cd5dd2916106e6d2653e23e7bc62748dd70ae62`.
Instrumentation counts only accepted binding checks after TDZ/import/BigInt rejection.
The source was restored byte-for-byte and every Rust hash matched the measured build
before the rejected source was archived. The complete measured source snapshot,
all eight changed/new source files, tracked patch, raw timings, test logs, independent
audits and coverage artifacts are preserved in the external optimizer directory.
No diagnostic build or run overlapped a timing run.

Current retained-binary gaps in this batch are 9.65× Node / 11.49× Bun by classic
score, and 15.07× / 23.88× by Djot runtime (3391 ms against 225 ms and 142 ms).
These workload-specific ratios do not show a new retained improvement; the within-2×
goal remains unmet.

The next bounded memory-layout experiment comes from owner review: ordinary Binding
values currently reserve inline space for `Option<(Env, String)>` import metadata.
Moving the rare present payload behind Box could shrink scope binding storage while
preserving physical Env ownership and clone behavior. Actual layout size, GC edge
accounting, imported-binding clone costs and application timings must be checked.
This is a proposal; no import-layout change is implemented here.


### Boxed import metadata (2026-09-08, rejected)

Binding's rare live-import tuple moves from `Option<(Env, String)>` to
`Option<Box<(Env, String)>>`. The binding type and its tests move into the focused
`interpreter/binding.rs` module; existing interpreter type paths remain re-exported.
On this 64-bit host, before/after probes measure Binding at 56/32 bytes and the
optional import field at 32/8 bytes. This is a 42.86% reduction in each binding's
storage, not a measured reduction in whole-engine memory or execution time.

Each present import still owns exactly one physical exporter Env handle. Cloning a
Binding clones its Box and exporter handle independently, preserving GC accounting.
Both collector scope passes borrow the tuple through `as_deref()` and retain their
existing physical-edge count/mark behavior, including uninitialized bindings and
simultaneous object values. Import-read snapshots use `as_deref().cloned()` so they
retain the previous Env/String clone behavior without adding a temporary Box per
read. Linking a present import and cloning an imported Binding add Box allocations;
ordinary bindings avoid the inline tuple space. Existing JIT offsets and captured
binding strides derive from the new owner layout.

Four focused tests verify size and physical clone ownership; rooted/unrooted import
cycles, TDZ and object edges through collection; live aliases/default imports and
reexports across GC; readonly imports; and imported typeof TDZ followed by exporter
initialization. Runtime fixtures cover interpreter, bytecode and JIT tiers. Full
validation passes 760 unit and 34 integration tests. Formatting/module checks pass;
Clippy matches the existing error-message multiset exactly.

A proposed cyclic-module TDZ fixture exposed an existing discrepancy: retained Lumen
and the candidate complete it, while installed Node and Bun throw. The original
fixture, comparison outputs and failed test are preserved under
`boxed-import-existing-*` and `boxed-import-tdz-fixture`. The direct imported-TDZ test
passes independently of that module-cycle behavior. This experiment does not claim
to fix that existing discrepancy.

This is a compile-time layout change with no runtime off variant. Comparison drivers
use retained bbe471f, the candidate, Node v24.18.0 and Bun 1.3.14 in three rotated
rounds: 36 verified workload runs and 12 classic-suite runs. Scope kernels create
2,000 escaping captured closures or churn 2,000 mutable captured scopes and validate
every warmup/calibration/timed invocation. An additional 12 cold-process module runs
load 2,000 imported mutable bindings and verify a live export update; their metric
includes process startup, loading, checks and exit, without clearing filesystem caches.


Expanded conformance, including module-code tests, matches retained/candidate exactly:
26,606 passes, two known failures, zero skips and identical full failure lists.
Differential testing agrees on 1,996 cases with four budget skips. Prechecks validate
both scope kernels and the module fixture on all four engines before timing.

The 36 workload runs are complete:

| Workload (lower is better) | Retained | Candidate | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Escaping closures, µs/2,000 | 1580.00 | 1560.00 | 28.40 | 25.60 |
| Mutable scopes, µs/2,000 | 1320.00 | 1320.00 | 9.47 | 12.80 |
| Djot 10,000 parses, ms | 3390.00 | 3391.00 | 224.00 | 143.00 |
| Delta 5,000 iterations, ms | 8270.00 | 8296.00 | 197.00 | 299.00 |

Escaping closures improve 1.27%, with one winning pair and two ties. Mutable scopes
are flat in every pair. Djot's median regresses 0.03%, with mixed pairs (+0.27%,
-0.18%, +2.45%). Delta regresses 0.31%, with all three pairs worse (+0.49%, +0.07%,
+0.60%). These results do not establish a repeatable application speed gain from the
smaller layout. All 60 timing runs are now complete.


| Classic suite (higher is better) | Retained | Candidate | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Richards | 23659 | 23455 | 66082 | 72027 |
| DeltaBlue | 3908 | 3941 | 154399 | 111250 |
| Crypto | 23959 | 23941 | 92331 | 120252 |
| RayTrace | 6499 | 6480 | 135196 | 308501 |
| EarleyBoyer | 3620 | 3577 | 147387 | 160007 |
| RegExp | 1650 | 1629 | 22410 | 30791 |
| Splay | 9942 | 10015 | 81092 | 94790 |
| NavierStokes | 38584 | 38655 | 70271 | 71457 |
| Score (version 7) | 8677 | 8682 | 83882 | 99867 |

The classic composite changes +0.058%, with mixed pairs (-1.239%, -0.069%,
+0.058%). RayTrace and RegExp lose in every pair; Splay wins in every pair.

| Cold module process (lower is better) | Retained | Candidate | Node | Bun |
| --- | ---: | ---: | ---: | ---: |
| Wall time, ms | 18.943417 | 19.291917 | 26.101958 | 15.156875 |

Cold module time regresses 1.840% at the median, with mixed pairs (-3.972%,
+3.298%, -1.458%). This measures startup, module loading, validation and exit;
it does not establish warm execution parity with either engine.

Rejected for this speed goal: the deterministic storage reduction produces no
repeatable classic-suite or parser gain, and Delta is slower in every pair.
All candidate source changes were restored to HEAD after verifying every measured
Rust source hash. The complete measured source ZIP, rejected source copies and
patch, raw timing rows, drivers, validation logs and independent all-60-run audit
are preserved under `/Volumes/XEX-VM/codex-builds/lumen-optimizer/boxed-import-*`.
No build, test or diagnostic run overlapped accepted timings.

The retained bbe471f binary remains 9.667× behind Node and 11.509× behind Bun by
classic score, and 15.134× / 23.706× by Djot runtime in this batch. There is no
new retained engine improvement from this experiment; the within-2× goal remains
unmet. Further work needs to remove larger combined allocation and call costs.


### Partial iterator entry coverage (2026-09-08, diagnostic)

After rejecting the boxed import layout, the next investigation targets combined
call, allocation and result-consumption costs. An external diagnostic copy of the
verified Djot workload counts the custom EventParser iterator's branches. It runs
on retained bbe471f with inherited LUMEN flags removed and preserves all 10,000
full HTML equality checks (3,160,000 characters). The counters change generated
code and overhead; elapsed output is not accepted benchmark evidence.

| Iterator outcome | Calls | Share of all calls |
| --- | ---: | ---: |
| Queued return immediately at entry | 260,000 | 56.52% |
| Queued return after cold parsing | 80,000 | 17.39% |
| Final-drain return | 110,000 | 23.91% |
| Completed | 10,000 | 2.17% |
| Total | 460,000 | 100% |

There are 90,000 cold invocations and 90,000 cold-loop passes: 80,000 end in the
queued return and 10,000 in final drain. Both reconciliation identities pass.
Only the 260,000 immediate queued returns qualify for the proposed entry-prefix
slice. Counting all queued returns would incorrectly claim 73.91% coverage.

A separate diagnostic of the original, unmodified workload dumps the retained
callee's actual bytecode and also passes all HTML checks. The callee has 483 ops;
the candidate entry region occupies PCs 0–35. Branches at PCs 4 and 19 leave for
completion/cold parsing. PC 5 performs a local TDZ reset; PC 25 writes the numeric
iterator state. PCs 26–32 then resolve names, read properties and fetch the yielded
element, followed by MakeObject at 34 and Return at 35.

This ordering makes restart-after-write unsafe. A first implementation must prove
all guards and value acquisition can precede the numeric commit without changing
alias, getter, proxy or exception behavior; otherwise it needs an exact callee
continuation with reconstructed state. Existing whole-function inline admission
uses the entire callee size and requires global or shared closure environments for
free names. Raising its budget alone does not implement the required captured
entry region. Existing RegionIr side exits describe one frame's locals and stack,
so they do not by themselves prove cross-frame fallback correctness.

The measured immediate-return frequency supports investigating a bounded partial
inline region and virtual returned record. It is not a measured compiler hit rate,
allocation reduction or speedup. No production engine change is retained here,
and the within-2× goal remains unmet. Source/binary/driver hashes, instrumented
source, counts and original bytecode are archived in the external optimizer
directory as `djot-next-paths.*`, `diagnose-djot-next-paths.py` and
`partial-iterator-original-*`.


Independent owner review recommends a proof-only partial-region planner as the
next implementation step. It must represent the numeric write as a pending effect,
forward later reads of that exact entry, prove other reads disjoint, and preserve
the original IEEE754 Add/Sub operations rather than simplifying `(old + 1) - 1`.
Acquire the yielded owner before the final commit; reject any remaining fallible
operation. IterStepL also needs dedicated callee feedback because its implicit
next call is not represented in ordinary Call/CallWithThis inline feedback.
The detailed API review and rejection conditions are archived in
`partial-iterator-entry-review.md`.


### Partial iterator entry analysis (2026-09-08, proof-only foundation)

A focused `jit_ir/iterator_entry` analysis models a bounded callee entry path,
fallthrough branch conditions, symbolic values, one deferred property write and a
nonescaping iterator-result record. Its output is a candidate with unresolved
runtime obligations, not permission to execute a specialized step. Production
iterator dispatch remains unchanged; no runtime path invokes this analysis yet.

Post-write own-property reads require exact entry-address forwarding or a proof
of disjointness. Dense reads require real Array storage and guarded initialized
elements; arithmetic retains ordered numeric operations. All runtime guards,
root acquisition and normal call safepoint/depth policy must be handled before a
future final write. Existing caller slots retain the iterator and captured next;
the future adapter must resolve inputs in that next function's live environment.


Four focused tests pass: the recorded 483-op callee's 0–35 entry prefix, a renamed
fixture compiled from JavaScript, alias requirements for differently named reads,
and rejection of calls, multiple stores, backward/invalid branches, stack underflow,
handlers, closures, unsupported record keys, non-false done values and over-budget
entries. Tests preserve JumpIfFalsePeek/Pop behavior and the ordered Add/Sub nodes.
Binary operand Number guards are distinct from a Number guard on the store RHS:
a comparison can have numeric operands while producing a Boolean value.

Validation passes 760 unit and 34 integration tests. Formatting and strict audits
of both new modules pass with zero structural signals. Clippy's full error-message
multiset matches the existing baseline; it is not lint-clean. The oversized parent
jit_ir module retains its three baseline structural signals and gains only module
wiring. A test-fixture ParseError formatting compile error was corrected before the
successful run; the initial failure log remains archived.

Independent review found no blocker to this non-executing analysis. Its output is
not an executable proof or a cross-frame continuation recipe. Dedicated IterStepL
feedback, live captured-environment guards, prepared property writes, native code,
success/fallback counters and off/on application benchmarks remain to be implemented.
The next integration must observe the captured next actually invoked, retain weak
callee ownership, and preserve normal depth/GC checks before borrowed probes.
Detailed reviews and validation logs are archived as `iterator-entry-*` under the
external optimizer directory. This commit makes no new runtime speedup claim;
the measured Node/Bun gaps and the within-2× goal remain unchanged.


### Live iterator candidate feedback (2026-09-08, experimental)

The iterator-entry analysis is connected to both bytecode and JIT fallback
consumers behind `LUMEN_ITERATOR_ENTRY_FEEDBACK`. Observation follows a successful
normal iterator step and uses the original owned next function, preserving
reentrant mutation, exceptions and ordinary call safepoints. The intrinsic Array
iterator fast path bypasses observation. No candidate executes specialized code.

Per-site feedback retains weak callee identity, selected base/optimized version
and copied analysis data. It does not retain a captured environment, callee Chunk
or raw code pointer. Diagnostic totals classify successful fallback observations;
accepted candidates are not successful queued entries or eliminated allocations.
Default-disabled storage adds one nullable Box pointer per Chunk; enabled sites
are indexed only by IterStepL PCs. Diagnostic labels are cached per classification,
avoiding per-observation String construction. Realm membership is rechecked before
reuse.


Nine focused tests pass (five feedback/selection tests and four existing planner
tests). Coverage includes both actual bytecode/JIT dispatches, captured next despite
replacement of the iterator property, intrinsic array-yield bypass, thrown/nonobject
results, distinct closures from one AST, weak reclamation through GC, cold-to-base-to-
optimized refresh and foreign-realm rejection. The version test verifies optimized
literal 9 replaces base literal 7; stable compiled observations do not replan.

Full validation passes 765 unit and 34 integration tests with feedback disabled,
and the same suite passes with feedback enabled. Formatting and strict audits of
the two new modules pass. Clippy matches the existing error-message multiset exactly;
it remains baseline-failing. Initial test-only Abrupt formatting compilation errors
and two new lint findings were corrected; their original logs are archived.

The release binary verifies the unchanged 10,000-parse Djot workload and every HTML
output with feedback both enabled and disabled (3,160,000 characters each). Disabled
mode emits no diagnostic output. Enabled mode reports:

| Successful fallback classification | Observations |
| --- | ---: |
| Accepted: 483 ops, return PC 35, store PC 25, three branches | 460,000 |
| Non-user callable | 340,000 |

The intended live parser callee is structurally admitted on all 460,000 observed
calls. This does not mean all 460,000 can take a fast entry: the separately measured
immediate queued branch has 260,000 calls. Live environment, property, alias, numeric,
rooting and safepoint obligations remain unresolved for native execution. No next
calls or result allocations are eliminated by this feedback-only implementation.
Elapsed diagnostic output is not accepted timing evidence or a new Node/Bun comparison.

Binary SHA-256:
`78d8c60ec3bb42f92cc88ebd99134d36cc8e53d302065e6078617cc31efd160e`.
Build source hashes and ZIP, normal/enabled test logs, classification review,
strict diagnostic driver, stdout/stderr and parsed results are preserved as
`iterator-feedback-*`, `diagnose-iterator-feedback.py` and
`iterator-entry-feedback-independent-review.md` in the external optimizer directory.
The next implementation must discharge these obligations and execute a guarded
entry before measuring eliminated calls/allocations and off/on application timing.
The within-2× goal remains unmet.


### Guarded iterator entry execution (2026-09-08, rejected)

The experimental `LUMEN_ITERATOR_ENTRY_EXEC` flag enabled a callback-free Rust
evaluator for admitted entry plans in both bytecode and JIT consumers. This was
compiled Rust reference execution, not an emitted JIT region. The executor roots
intermediate Values,
resolves live lexical bindings without global-object lookup or callbacks, guards
ordinary own data and Array elements, forwards the pending numeric write on exact
receiver/key equality, and commits only after every guard and yielded-owner clone.
Accessors, proxies, imports/with/TDZ, holes, coercive arithmetic, readonly fields,
unsupported callee entries and class constructors fall back before any state change.
The original Add/Sub IEEE754 sequence remains intact.

Normal calls and attempted entries share one extracted logical-call boundary:
depth increment/check, one amortized GC poll, then attempt or ordinary call_inner.
A miss does not poll a second time. The existing proper-tail trampoline and depth
restoration apply to both outcomes. The consumer resolves done/value only after an
ordinary result and restored depth; successful entries yield directly with no
intermediate iterator-result object. Fresh identity, selected code version and
realm checks follow the safepoint. Feedback and execution have independent flags;
reference execution was disabled by default during the experiment.

Validation passes 15 focused iterator tests plus three shared-call-boundary tests,
and 774 unit / 34 integration tests with execution both disabled and enabled.
Tests cover actual successful entries in both consumers, surviving yielded objects,
post-store getter fallback with one increment and one getter call, exact alias
forwarding, disjoint objects, IEEE rounding, readonly/proxy/hole/coercion rejection,
call depth/tick counts, overflow, errors and proper-tail handling. Fixture setup and
state checks explicitly reject JavaScript throws. Initial positive fixtures used
unsupported global-object variables; switching their inputs to lexical bindings
exercises the intended guarded path without weakening their assertions.

Formatting and strict audits of all six affected focused modules pass. Clippy
matches the existing baseline error-message multiset exactly. Expanded conformance
matches baseline totals and complete failure lists: 26,606 passes, two known failures,
zero skips. Differential testing agrees on 1,996 seeds with four budget skips.

The unchanged verified Djot workload passes every HTML check (10,000 parses,
3,160,000 characters) with execution and feedback enabled. Diagnostic totals are:

| Event | Count |
| --- | ---: |
| Guarded Rust evaluator success | 260,000 |
| Prepared-call miss | 540,000 |
| Ordinary successful fallback: admitted parser callee | 200,000 |
| Ordinary successful fallback: non-user callable | 340,000 |

Successes match the independently counted immediate queued branch frequency.
Misses and fallback classifications describe the same 540,000 calls and must not
be added together. Each success bypasses the next body and fresh result-record
construction; it is not a generated-native-instruction counter or a measured speedup.
The custom-iterator kernel also verifies actual executor successes before timing.

Release binary SHA-256:
`a793933be6eca997c53051ee81a71659f75391e9308ce87b6430aa2c2a5489f6`.
Drivers compare retained f056c51, same-binary execution off/on, Node and Bun in three
rotated rounds: 45 kernel/application runs and 15 classic runs. Only on sets EXEC;
all timing runs strip inherited LUMEN flags and disable feedback. Strict outputs,
hashes, source snapshots and raw rows are archived under `iterator-exec-*` in the
external optimizer directory.

The 45 kernel/application comparisons are complete. Median elapsed time (lower is
better), with three rotated runs per cell:

| Workload | Retained f056c51 | Candidate off | Candidate on | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| QueuedObjects, µs / verified 2,000 yields | 270 | 280 | 690 | 4.4 | 5.1 |
| Djot, ms / 10,000 verified parses | 3,397 | 3,403 | 3,646 | 226 | 144 |
| DeltaBlue, ms / 5,000 iterations | 8,222 | 8,216 | 8,197 | 196 | 298 |

Enabled reference execution increases kernel time 155.56% and parser time 7.33%
against retained, with losses in every paired round. Relative to candidate off,
the increases are 146.43% and 7.14%, also losses in every round. Delta improves
0.304% against retained in every round, but off/on changes are mixed. Call/result
elision alone has not delivered a useful application speedup. The disabled
candidate is also 3.70% slower on this kernel in every round; its shared-call
extraction cannot be described as zero-cost. Disabled parser time rises 0.177%
(all pairs slower), while Delta time falls 0.073% (mixed pairs).

The retained engine in this batch takes 15.03× Node / 23.59× Bun time on Djot,
and 41.95× / 27.59× on Delta. These are workload-specific elapsed-time ratios,
not an overall-engine score.

All 15 classic runs also complete with valid scores. Composite median scores
(higher is better) are 8,750 retained, 8,761 off, 8,751 on, 83,474 Node and
100,391 Bun. Enabled versus retained changes just +0.0114%, with mixed paired
changes of -1.6706%, +0.6904% and -0.2629%. Disabled versus retained changes
+0.1257%; enabled versus disabled changes -0.1141%. RayTrace loses in every
enabled/retained pair; other component comparisons are mixed. There is no
consistent composite improvement to offset the target workload regressions.
The retained classic score gap is 9.54× Node / 11.47× Bun in this batch.

**Rejected.** The candidate source and its tests are preserved in
`iterator-exec-rejected-source.zip` (all seven changed/new Rust files), the tracked
patch and the complete measured source ZIP. Production is restored exactly to
f056c51; neither the reference executor nor the universal call-wrapper change is
retained. Full artifact validation, restoration hashes and independent review
are preserved as `iterator-exec-validation.json`, `iterator-exec-restoration.json`
and `iterator-exec-independent-audit.md`. The within-2× goal remains unmet.

An independently reviewed next-step design is archived as
`iterator-entry-native-emitter-design.md`. It proposes actual generated code
inside an iterator-specific prepared call boundary, borrowed typed expression homes,
lexical identity/generation guards, guarded own-property/Array probes and one
owned output acquisition before the numeric commit. Existing property probes
are partly reusable; name probes require explicit live-environment inputs and
Array probes require stronger length/type guards. Raw embedded cache addresses
must have an explicit owner independent of weak callee identity. This is a design,
not implemented machine code or evidence of a future speedup. The follow-up
should leave ordinary `call()` unchanged; the rejected reference tests provide
a starting point for ownership, alias, safepoint and fallback validation.


### Generated iterator entry experiment (2026-09-08, candidate)

`LUMEN_ITERATOR_ENTRY_NATIVE` opts into standalone ARM64 code for admitted
iterator entry prefixes. Other targets decline this optimization. The ordinary
`Interp::call` implementation is unchanged. An iterator-only boundary performs
normal depth checking and one GC poll before a fresh callee/version/realm check
and native attempt; misses invoke the original next body without another poll.

The generated frame holds borrowed scalar/Object values in bounded spill slots.
Captured-name guards use the live definition environment, property reads use
owned cache copies with live receiver/data guards, and dense reads require actual
Arrays with valid own length and present own elements. The original numeric
operation sequence and Boolean branch guards are preserved. The only mutation
is a deferred writable ordinary numeric field; post-store reads forward only on
physical entry equality. All fallible checks precede yielded Object ownership
acquisition and commit. NaN stores use canonical packed bits.

The executable owns its property cache storage and weak references to its source
Chunk and captured scopes, avoiding closure/Chunk retention cycles. After a miss,
cache copies refresh from a briefly upgraded live Chunk. Native names use a bounded
checked helper for live binding metadata. Nonzero scope generations can wrap,
so these paths also re-resolve every name position before using a binding; only
all-zero paths use a cached binding address. Structural mutations never restore
zero. An emitted-code test simulates generation revival, intermediate shadowing,
and replacement of holder storage to verify stale pointers are not followed.

Twenty-one focused tests and 780 unit / 34 integration tests pass with the flag
disabled by default; focused integration tests explicitly enable native execution
in both VM and JIT consumers. Tests include actual yielded owners surviving GC,
weak cache reclamation, selected-code invalidation, exact/disjoint aliases,
2^53 rounding, NaN, negative zero, readonly/accessor/proxy/hole misses and unchanged
state on rejection. All JavaScript fixture assertions check completion explicitly.
The three iterator-call tests verify depth, poll count, errors and proper tails.

Formatting and strict audits of all 11 affected focused modules pass. Clippy's
error-message multiset matches the pre-existing baseline exactly. Initial fixture
warmup failures, a test-only Debug formatting error and one needless borrow lint
were corrected and their original logs archived. The full 814-test suite also
passes with native entries enabled globally. Expanded conformance matches baseline
totals and complete failure lists: 26,606 passes, two known failures, zero skips.
Differential testing agrees on 1,996 seeds with four budget skips.

The release binary verifies all 10,000 Djot outputs (3,160,000 HTML characters).
Diagnostics show 260,000 successful generated entries and 540,000 misses. Misses
comprise the same 200,000 admitted-parser ordinary calls and 340,000 non-user calls
seen in the reference experiment; do not add these classifications again. The
kernel also verifies actual generated-code hits. These diagnostic counters are
successful entries, not instruction counts or elapsed-time evidence.

Release binary SHA-256:
`724032fb566a3144bb0a75ea28c1640d6058877604b7b82d9bc18cb3fd8b5392`.
Source hashes/ZIPs, binaries, all validation logs and diagnostic drivers/results
are archived as `iterator-native-*` in the external optimizer directory.

All 60 controlled runs complete: three rotated rounds of retained f056c51, the
same candidate binary off/on, Node 24.18.0 and Bun 1.3.14. Every timing run strips
inherited LUMEN flags and disables feedback. Medians:

| Workload | Retained | Candidate off | Candidate on | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| QueuedObjects, µs / 2,000 verified yields | 270 | 280 | 176.667 | 4.4 | 5.2 |
| Djot, ms / 10,000 verified parses | 3,439 | 3,436 | 3,601 | 224 | 141 |
| DeltaBlue, ms / 5,000 iterations | 8,240 | 8,196 | 8,277 | 199 | 305 |
| Classic composite score, higher is better | 8,748 | 8,660 | 8,681 | 83,759 | 99,955 |

Native execution reduces kernel time 34.568% against retained and 36.905% against
off, with improvements in every paired round. Djot increases 4.711% / 4.802%,
with regressions in every pair. Delta increases 0.449% against retained (mixed).
Classic composite score falls 0.7659% against retained in all pairs
(-5.6772%, -0.7659%, -0.3659%); Richards, EarleyBoyer and RegExp also lose in
all pairs, while the other components are mixed. All rows remain included.

Disabled mode is not cost-free: kernel time rises 3.704% and classic score falls
1.0059%, both consistently. Disabled parser time falls 0.0872% with mixed pairs;
Delta falls 0.5340% with improvements in all pairs. The default-off candidate's
classic score gap is 9.672× Node / 11.542× Bun; Djot takes 15.339× / 24.369×
as long. These workload-specific ratios do not establish overall engine parity.

**Experimental foundation only, not approved for default enablement.** The native
backend and regression tests are retained behind the opt-in flag for continued
work. Its kernel gain establishes useful generated execution, but parser and
classic regressions prevent calling this a broad performance improvement. The
next work must address disabled-path overhead and measure compilation/setup cost
before deciding whether code reuse across fresh closures is justified. The design
is archived as `iterator-entry-code-reuse-design.md`; repeated compilation is
currently a hypothesis, not an attributed timing cause. Independent artifact and
ownership review is in `iterator-native-independent-review.md`. The within-2×
goal remains unmet.

### Iterator compilation cost diagnostic (2026-09-08)

Feedback-enabled native compilation now counts successful/declined preparations,
cumulative emitted code bytes and preparation nanoseconds. Timing collection and
counter updates occur only with feedback logging; the timed comparisons above
used no logging. Existing integration coverage checks one generated compilation
and positive emitted bytes alongside two successful native entries.

The instrumented release verifies the unchanged 10,000-parse workload and reports
10,000 successful compilations, 125,800,000 cumulative code bytes, 125,066,604 ns
in native preparation, and the same 260,000 generated entries / 540,000 misses.
Cumulative code is not peak resident memory. Preparation time includes entry
eligibility, lexical guards, cache copying, assembly and executable allocation;
it is diagnostic evidence, not an isolated causal estimate of the parser regression.

This supports investigating reuse across fresh closures sharing bytecode. Such
reuse must retain live callee/version/realm checks, update captured scope records
without rooting old closures, and reject incompatible binding paths before entry.
It does not justify enabling the current backend or predict a measured speedup.

The 21 focused tests and full 814-test suite pass; Clippy matches the baseline
error-message multiset. Instrumented binary SHA-256:
`cdbaa43304b3d493d9574c6d612b08e7c8172aa99ab107d478022c1a7638c100`.
Source ZIP/hashes, exact patch, validation logs and diagnostic driver/results are
archived as `iterator-native-setup-*` in the external optimizer directory.

The instrumented binary/source archive precedes a behavior-preserving extraction
of the counters into `iterator_entry/diagnostics.rs`. The final extracted source
was separately validated by `iterator-native-setup-final-*` test and Clippy logs.


### Same-site native iterator code reuse (2026-09-08)

The disabled experimental backend now reuses an executable across fresh closures
with the identical selected bytecode Chunk. Rebinding prepares all lexical paths
before changing any metadata, retains stable executable/cache/metadata addresses,
and transfers weak scope ownership without rooting the old captured environment.
Different Chunks, lexical depths or binding-helper modes reject reuse. Every
native attempt still checks live callee identity, selected version, realm and
eligibility after the ordinary single depth/GC poll. Nonzero lexical generations
retain fresh name resolution, including generation-wrap protection.

The ordinary disabled fallback again calls the small iterator-step path directly.
Enabled chunks without iterator sites no longer allocate empty feedback records.
The integration tests exercise fresh captures, switching back to still-live
captures, selected-code invalidation and yielded-object ownership. Native tests
also check unchanged executable addresses, failed late preparation leaving the
old entry usable, incompatible Chunks, TDZ and old environment reclamation.

The unchanged verified 10,000-parse diagnostic reports one compilation, 9,999
reuses, 12,472 emitted bytes and the same 260,000 successful native entries /
540,000 misses. Its compilation timer excludes rebind preparation; the 17,125 ns
single-compilation value is not the total preparation cost or a speedup estimate.
All HTML output checks pass. Both disabled and enabled suites pass 819 tests,
including 26 focused tests. Clippy matches the existing error-message multiset;
it is not clean. Enabled conformance matches the saved baseline exactly at
26,606 passes and two known failures; differential seeds 1..2001 produce 1,996
agreements and four budget skips.

The saved release binary SHA-256 is
`6f0843494eadcb594230bc96bdb7e19a05992bea51233f9a5b74eed9f4ba0ae7`.
Build/source archives, validation, diagnostics, controlled timing artifacts and
independent review use `iterator-reuse-*` in the external optimizer directory.

Three rotated rounds compare saved feedback-only `f056c51`, this candidate with
native execution off/on, Node 24.18.0 and Bun 1.3.14. All 60 measurements are
retained, with strict outputs, unchanged workloads, CPU snapshots and exact
source/binary hashes. No feedback logging, build, test or profile overlaps them.

| Metric (median) | Before | Off | On | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| QueuedObjects µs / 2,000 verified yields | 270 | 270 | 168.5714 | 4.4 | 5.2 |
| Djot ms / 10,000 verified parses | 3,427 | 3,423 | 3,504 | 229 | 147 |
| DeltaBlue ms / 5,000 | 8,353 | 8,327 | 8,408 | 196 | 314 |
| Classic score, higher is better | 8,691 | 8,669 | 8,693 | 83,608 | 99,932 |

Native-on kernel time improves 37.566% against before, with all three paired
rounds faster. Djot regresses 2.247% and DeltaBlue regresses 0.658%, with all
three before/on pairs slower for each. Classic score changes only +0.023%, with
mixed pair directions. On/off parser and DeltaBlue directions are also mixed.
These data do not justify default enablement or a broad engine speedup claim.
Repeated compilation is eliminated, but its removal has not made the full parser
faster than the pre-experiment baseline. Do not subtract results from the earlier
native batch to claim a controlled old-native/new-native gain.

Disabled kernel time matches before in all three rounds. Disabled classic score
is 0.253% lower by medians with mixed paired directions; it is not a proven
zero-overhead configuration. The current default remains 9.644× behind Node /
11.528× behind Bun by classic score and takes 14.948× / 23.286× their Djot time.
The within-2× goal remains unmet. Further investigation targets ordinary misses
that unnecessarily enter the native call wrapper and duplicated live eligibility
checks; their timing benefit is not yet measured.

### Rejected native iterator negative-hint routing (2026-09-08)

A measured candidate checked native-entry presence and Weak callee identity before
entering the native call wrapper. A negative hint used ordinary iterator_step,
released the site borrow before callbacks and observed only successful protocol
completion. A positive hint retained the full live validation after one normal
GC/depth poll. New VM/JIT coverage exercised ordinary next re-entering the same
warmed consumer. No eligibility checks were consolidated in this experiment.

All 820 tests passed with the experiment disabled and enabled, including 27
focused tests. Clippy matched the baseline error-message multiset. Enabled
conformance matched the saved baseline at 26,606 passes / two known failures;
differential seeds 1..2001 produced 1,996 agreements / four budget skips. The
verified 10,000-parse diagnostic retained 260,000 native successes, one compilation
and 9,999 reuses. It recorded 350,000 bypasses and 190,000 actual native misses.
Bypasses count routed attempts, including potential throws; their reduction of
the old miss counter is routing, not improved generated guard coverage.

All 60 timings were retained and independently audited. Unlike the preceding
comparison, BEFORE is saved d1d65c9 with native execution ENABLED, and ON is the
routing candidate with native execution ENABLED. OFF is the candidate disabled.
OFF/BEFORE therefore does not isolate default-disabled overhead. Node 24.18.0 and
Bun 1.3.14, workloads, three rotated rounds and absence of feedback logging remain
explicitly recorded. No builds, tests or profiles overlap the measurements.

| Metric (median) | Previous native-on | Candidate off | Candidate on | Node | Bun |
| --- | ---: | ---: | ---: | ---: | ---: |
| QueuedObjects µs / 2,000 verified yields | 168.5714 | 270 | 171.4286 | 4.4 | 5.2 |
| Djot ms / 10,000 verified parses | 3,488 | 3,436 | 3,490 | 228 | 143 |
| DeltaBlue ms / 5,000 | 8,307 | 8,335 | 8,324 | 200 | 311 |
| Classic score, higher is better | 8,698 | 8,674 | 8,710 | 83,813 | 99,515 |

Routing slows the iterator kernel 1.695%, with all three paired rounds slower.
Djot changes +0.057% in time and DeltaBlue +0.205%, both with mixed directions.
Classic score improves 0.138%, with all three pairs higher: +3.972%, +0.149% and
+0.138%. The lower first previous-native score of 8,359 remains included; no host
cause is inferred. The small classic gain does not justify the consistent kernel
loss and absent parser gain from this added iterator path. The routing code is
rejected, archived, and all Rust sources restored byte-for-byte to d1d65c9.

Candidate binary SHA-256:
`5ab9d816d0df7d7505b863ce7e7701a5ee639b71ce4611fcdc78f74b92705015`.
The rejected three-source ZIP, exact tracked patch, built/measured archives,
validation, diagnostics, timing rows and summaries, independent review and exact
restoration proof are under `iterator-routing-*` in the external optimizer
directory. The retained implementation remains disabled by default; its retained
comparison and goal gaps are the previous section, not this discarded candidate.

After all timing processes ended, an existing LUMEN_TIER_LOG diagnostic on the
saved d1d65c9 default binary verified the unchanged full parser output. It reports
21 free-name/non-global-closure rejection events, 17 optimized-body-budget events,
17 arrow/strictness events, 10 size/shape events, four callee-op events and one
each for self-recursion and parameter shape. These are compilation events, not
runtime call counts; repeated events need not represent distinct eligible targets.
Artifacts are `inline-admission-djot.*` and `diagnose-inline-admission.py`. The
next admission investigation must intersect all remaining guards and hotness
before extending cross-environment call inlining. The within-2× goal is unmet.

The subsequent caller summaries in the log associate those 21 events with handleEvent
(two), pushContainer (three) and main parse (16), but existing messages omit
recursive depth, callee identity and cache way. Remaining-budget consumption and
later receiver/shadowing/lowering checks still follow the lexical rejection.
`inline-admission-review.md` records this limitation and the required precise
site/target eligibility plus call-frequency diagnostic; no cross-environment
inlining implementation is justified by the event total alone.


### Exact lexical inline admission diagnostics (2026-09-08)

The existing planner is extracted into `bytecode/inline_plan.rs`, with unchanged
gate order, shared budget, recursion depth, base-code selection and adjacent-only
PIC deduplication. `LUMEN_INLINE_ADMISSION=1` enables structured rejection records
in the owning diagnostics module. Records include original PIC way, closure and
Function identities, definition environments, full source text, caller Chunk,
call PC/argc/receiver shape, free names, slots, current budget and inline cost.
Identities are process-local diagnostics, never retained roots. The flag is read
once per planner invocation; disabled records do not format source or JSON.

The unchanged 10,000-parse workload verifies all HTML and reproduces the retained
d1d65c9 tier log exactly, alongside 21 valid JSON rejection records. These map to
four Function ASTs and unique original source locations. Thirteen records fit
the budget individually; eight already fail it. All recorded call sites meet the
argument/receiver bounds. No reported slot overlap is an exact proof of lexical
visibility or final splice eligibility.

| Target | Source line | Caller / site | Ways | Budget / cost |
| --- | ---: | --- | ---: | ---: |
| topContainer | 2,194 | handleEvent / 5 | 2 | 185 / 18 |
| addBlockAttributes | 2,152 | pushContainer / 0 | 3 | 303 / 38 |
| topContainer | 2,194 | main parse / 4 | 4 | 18 / 18 |
| popContainer | 2,181 | main parse / 5 | 4 | 18 / 26 |
| addChildToTip | 2,209 | main parse / 6 | 4 | 18 / 19 |
| topContainer | 2,194 | main parse / 7 | 4 | 18 / 18 |

The repeated main-parse budget is not independent capacity: its 18 remaining
operations can fund only one 18-op way, not all eight individually fitting ways.
Accepted ways, final splice filtering and runtime guard coverage need separate
attribution before declaring any target solely blocked by lexical access.

A separately derived diagnostic inserts entry counters into those four uniquely
matched helper bodies. Lumen, Node and Bun all verify the full 10,000-parse HTML
output and agree on 350,000 topContainer, 160,000 addBlockAttributes, 160,000
popContainer and 270,000 addChildToTip entries. These aggregate every caller and
closure instance; instrumentation changes the compiled bodies. They are neither
inline-miss counts nor an estimate of obtainable time savings. Exact derived
source, original/derived hashes, target mapping, patch and outputs are archived
as `inline-admission-body-counts-*`.

Source inspection confirms exact-object inline guards are shared through
once-published code2, while these local closures are recreated per parse. But the
two handleEvent and three pushContainer rejections may be old PIC ways while a
current same-environment way is already admitted. The current rejected-only
records do not establish that relationship. Main parse also requires distinguishing
its definition environment from its active captured-binding environment. The
next diagnostic must identify accepted and emitted targets before changing guard
identity or adding a separate lexical context. `inline-admission-identity-review.md`
records these constraints; no cross-environment optimization is enabled here.

Both ordinary and diagnostic-enabled suites pass 820 tests. Clippy matches the
baseline error-message multiset; enabled conformance matches 26,606 passes and
two known failures, and differential seeds 1..2001 yield 1,996 agreements / four
budget skips. Independent review verifies extraction semantics and ownership.
The new release SHA-256 is
`883f230b6571e1c68ce3795f9613f697021d792c0f664ea5cb2135fcc9c8c8d3`.
Build/source archives, validation, detailed records and analysis use
`inline-admission-details-*`. This is diagnostic infrastructure, not a measured
engine speedup, and the within-2× goal remains unmet.


### Accepted and emitted inline targets (2026-09-08)

`LUMEN_INLINE_ADMISSION=1` now also records accepted PIC ways and the targets
remaining after optimized bytecode compilation. Accepted records distinguish
budget after the direct callee from budget after recursive planning. Final
records identify the root Function/Chunk, callee, expected environment and every
remaining InlineGuard PC. These are compilation events, not runtime guard hits;
they do not establish whether native code executes an inline path.

The verified unchanged Djot workload produces 21 existing rejection records,
19 accepted-way records and 19 compiled-target records. Its ordinary tier log
still exactly matches the retained baseline. Independent review confirms:

| Helper / caller | Accepted PIC way | Final target / guard PC |
| --- | ---: | --- |
| topContainer / handleEvent | 2 | 0 / 65 |
| addBlockAttributes / pushContainer | 3 | 0 / 9 |

At each of these sites, the accepted way shares the caller's definition
environment; the preceding two or three rejected ways have different
environments. Thus rejected ways coexist with an emitted same-environment
target. The records do not prove runtime coverage across fresh parser closures.
Neither popContainer nor addChildToTip has an accepted or final target. The main
parse's remaining 18-op budget and definition-versus-active environment
distinction still constrain any proposed expansion.

The next measurement is actual guard outcomes for these emitted targets,
including an explicit account of any fused native regions. Widening identity
checks also requires preserving the actual callable in inline reflection and GC
roots; changing the guard alone is insufficient.

Ordinary and diagnostic-enabled suites pass 820 tests. Clippy matches the
existing error-message multiset (86 library / 88 library-test errors), rather
than passing. Enabled conformance retains 26,606 passes and the same two known
failures; differential fuzzing reports 1,996 agreements and four budget skips.
Release diagnostic output verifies all 10,000 parser results. The final release
SHA-256 is `53bd565685cfcacaee298340224d35ff62c49c8b9cb427b7771e1de92b55fd93`.
Sources, binaries, raw results and independent helper analysis are archived as
`inline-admission-targets-*`; the initial build with a subsequently fixed lint
issue is separately preserved as `inline-admission-targets-pre-lint-*`.
No runtime optimization or measured speedup is claimed for this increment.


### Fresh activation-call reflection identity (2026-09-08)

Review for wider closure reuse exposed an existing correctness error in the
Function-keyed activation-call retry. It supplied the fresh closure's environment
but retained the cached closure's identity when pushing the physical reflection
frame. After warming one closure, invoking a fresh instance could therefore
return the earlier closure from a callee's `caller` property.

The retry now updates its local copied cache entry with the actual callee and
live environment before commitment. The shared cache and eligibility stay
unchanged. A regression warms an anonymous activation-bearing closure, then
checks reflection through fresh instances in bytecode and JIT tiers. The saved
pre-fix release reproduces `fresh caller`; the fixed regression passes. Named
function expressions would bail out of compilation and would not exercise this
bug, so the regression deliberately uses an anonymous function expression.

An isolated export of the parent plus exactly the two changed interpreter files
passes all 821 tests and matches the existing Clippy error-message baseline.
Independent ownership review confirms the physical frame now follows the
actual operand-stack callee. Evidence is saved under `fresh-caller-*`; this
correctness prerequisite does not claim a measured speed improvement.


### Actual native inline-guard outcomes (2026-09-08)

`LUMEN_JIT_INLINE_GUARD_COVERAGE=1` instruments the ordinary native InlineGuard
emitter without changing its predicates or optimizer selection. Disabled code
retains the original instruction sequence. Enabled branches count after choosing
hit/miss, preserving caller-save integer and scalar floating-point registers.
Globally unique numeric IDs join copied compilation metadata to thread-local
counts; no engine objects or TLS addresses are embedded in the counters. The
first execution on a thread may allocate diagnostic storage with Rust's
allocator. Totals are reported at thread teardown, so unfinished threads and
abnormal process termination cannot be assumed fully reported.

Final-op listings, installed/template flags, callee identities and source support
attribution. Generic coverage is explicitly ordinary-only: native regions and
method fusion can bypass this template. For the two parser targets below,
independent review excludes those alternatives against their complete final
opcode listings and selected-region logs. Neither has adjacent method fusion;
the first has no loop and the second's only backedge is after its guard.

| Native site | Hits | Misses |
| --- | ---: | ---: |
| handleEvent → topContainer, PC65 | 13 | 159,952 |
| pushContainer → addBlockAttributes, PC9 | 7 | 159,744 |

All 10,000 parser results are verified. These are actual executions of these two
compiled predicates, not counts of all helper calls. The counters do not identify
which predicate failed or timestamp each closure instance. The overwhelming
fallback rate supports investigating fresh-closure code reuse; source lifecycle
and static guards suggest that cause without proving a miss-reason breakdown.
A Function-keyed physical-call retry offers a smaller experiment than widening
inline guards, which additionally requires dynamic inline-frame ownership.

Both emitted diagnostic tests pass: guard decisions/counts (including dead pins)
and all caller-save scalar registers across the counting helper, including a
second OS thread running the same machine code. Ordinary and enabled full suites
pass 823 tests; Clippy matches its existing baseline. Enabled conformance retains
26,606 passes and the same two known failures; differential fuzzing reports
1,996 agreements and four budget skips. Release SHA-256:
`fe3e00f1272b5eaf1d8fff013eeb6f4741ee60932891a4d4a8329f9a98d88633`.
Source archives, binaries, counts and independent completeness review are saved
under `inline-guard-coverage-*`. Instrumented elapsed time is not a performance
result, and no speedup is claimed for this diagnostic increment.


### Draft checkpoint: fresh closure call-cache retries (2026-09-08)

`interpreter/fresh_call.rs` extracts the existing activation-bearing closure
retry and adds an experimental no-activation retry, disabled unless
`LUMEN_JIT_FRESH_CLOSURE_RETRY=1` is present at its first use. The new path checks
live callable eligibility, realm, Function identity, current code and cache
epoch, plus frame dimensions and strictness/receiver flags. Matching recycled
addresses cannot authorize stale frame metadata. Only the returned local IC
copy receives the actual callee and environment; ordinary physical frames,
ownership, exception handling and inline recompilation remain in effect.

The first complete comparison used three rotated rounds of retained/off/on/Node/
Bun, with verified fresh-closure batches, Djot, DeltaBlue and the classic suite.
Medians for the retained engine, candidate disabled and candidate enabled were:

| Workload | Retained | Disabled | Enabled |
| --- | ---: | ---: | ---: |
| Fresh captured closures, microseconds / 2,000 | 850 | 850 | 820 |
| Fresh shared-environment closures, microseconds / 2,000 | 580 | 550 | 540 |
| Reused-function control, microseconds / 2,000 | 17.333 | 17.333 | 17.333 |
| Djot, milliseconds / 10,000 verified parses | 3,459 | 3,429 | 3,398 |
| DeltaBlue, milliseconds / 5,000 iterations | 8,373 | 8,383 | 8,321 |
| Classic suite score (higher is better) | 8,685 | 8,688 | 8,639 |

Parser time improved in all three enabled/disabled pairs, but classic score was
lower in two of three pairs and its median fell 0.56%. This version was not
approved for default enablement. The retained engine in this batch remained
about 9.6x/11.5x behind Node/Bun on classic score and 15.1x/23.7x on Djot.

The current revision rejects sites without a plausible cached user Function
before inspecting object/environment eligibility, preserving all safety checks.
This ordering is intended to reduce unsuccessful admission work; that cause and
its performance benefit are not established. Its benchmark was stopped at the
user's request after 17 of 45 main-workload runs, before the classic comparison.
Those partial measurements are not an accepted performance result. Both native
iterator entries and fresh-closure retries remain disabled by default.

The exact current Rust source matches the archived release build. Ordinary and
enabled suites each pass 827 tests, including four actual-hit/metadata regressions
colocated with the retry. The JIT-dependent tests are gated to supported AArch64
platforms. Clippy matches the existing 86/88 error-message baseline; enabled
conformance retains 26,606 passes and the same two known failures; fuzzing reports
1,996 agreements and four budget skips. Formatting and the focused structural
audit pass. Release SHA-256:
`4ad7fa27a10fb55bb0628a9e3f928fad27a1def38505d62d1e523d72b25b82b8`.

The local evidence archive uses `fresh-call-*` for the initial complete candidate
and `fresh-call-gated-*` for the current revision. The stopped batch is explicitly
marked `stopped_by_user`; pre-lint artifacts are separately preserved. These raw
archives live outside the repository. This draft preserves the experiment for
review, not as a completed performance milestone. The within-2x objective is unmet.

### Bounded fresh-closure identity-cache refresh (2026-09-08)

Branch `perf/closure-call-inlining`, based on fast-forward-pulled local main at
`6976d91`, adds an opt-in follow-up in `2faa9d7`. A validated no-activation retry
can publish a bounded, weakly pinned identity entry, allowing repeated calls to
the same fresh closure to use ordinary native dispatch. Native addresses and
dispatch flags are rebuilt from live code. Public `CallSite` layout is preserved;
replaceable pins belong to the chunk and cannot authorize baked optimizer pointers.

Three rotated final comparisons reduce a verified 1.6-million-call closure fixture
from 111 to 78 ms. Djot changes from 3457 to 3402 ms against the same binary with
refresh disabled; DeltaBlue changes from 8269 to 8288 ms, and classic score from
8662 to 8735. Host variation and the small broader changes limit these conclusions.
`LUMEN_JIT_REFRESH_CLOSURE_CACHE=1` remains opt-in; the within-2x objective is unmet.

The [focused report](closure-call-cache.md) contains raw results, reproduction,
ownership details, and validation, including inherited HTTP/2 and Clippy failures
and the identical enabled/disabled selected-conformance failure.
