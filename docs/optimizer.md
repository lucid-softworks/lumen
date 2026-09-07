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
