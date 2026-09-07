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
