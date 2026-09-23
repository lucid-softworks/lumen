# JIT call-dispatch audit: dense spreads

This audit starts from `origin/main` at `15f1232` and follows the constructor work already merged
in PR #26. It covers native builtins, bound functions, `call`/`apply`, trailing spread calls, and
remaining polymorphic call helpers. It does not change closure dispatch.

## Profile and candidate selection

The focused matrix is `crates/lumen/benches/call_dispatch_matrix.js`. It executes two million
operations for each direct-native, four-way native and user-polymorphic, bound-user,
bound-native, user/native `call`, user/native `apply`, and user/native spread shape.

Diagnostic counters on the unmodified base reported 4,060,025 native call-IC entries and
4,060,000 `CallSpreadThis` helper entries. A native sample of the matrix's spread phase was
dominated by `Interp::call_dispatch`, `Interp::iterator_step`, Array Iterator `next`, property-map
work, and iterator-result object construction. The ordinary four-way call IC was already active;
`Function.prototype.call` and dense-array `apply` already had dedicated JIT paths. Bound calls
were slower than direct calls, but their target and bound-argument reconstruction did not offer a
comparably narrow semantics-safe change. Dense trailing spread was the clear outlier at about two
seconds per two million calls.

## Optimization

The JIT slow helper now scalar-replaces a complete Array Iterator walk for arrays of at most 16
elements when all of these guards hold:

- the spread value is an ordinary, plain Array with an own data `length`;
- every visited element is an own data property, with no holes or accessors;
- `@@iterator` is the active realm's original `Array.prototype.values` by identity; and
- the active realm's Array Iterator `next` is still the original function by identity.

Any miss is side-effect-free and runs the prior iterator protocol unchanged. This keeps proxies,
custom iterators, getters, inherited holes, mutated iterator methods, and foreign-realm
intrinsics on the observable path. A hit still enters one logical recursion-limit and amortized
GC-poll boundary for `values`, every yielded `next`, and the final done-producing `next`. The
eventual target continues through the existing call path, preserving callable and proxy identity,
receiver handling, argument order and ownership, target exceptions, reflective frames, tail
calls, and target recursion/GC behavior.

## Measurements

Seven fresh-process rounds alternated clean release builds of `15f1232` and this change on Apple
Silicon. Values are medians in milliseconds; lower is better. Checksums matched in every round.

| Two million operations | Before | After | Change |
| --- | ---: | ---: | ---: |
| Native direct | 46 | 43 | -6.5% |
| Native polymorphic (4 targets) | 74 | 70 | -5.4% |
| User polymorphic (4 targets) | 67 | 65 | -3.0% |
| Bound user / native | 174 / 152 | 179 / 154 | +2.9% / +1.3% |
| `call` user / native | 80 / 73 | 82 / 75 | +2.5% / +2.7% |
| `apply` user / native | 121 / 211 | 125 / 215 | +3.3% / +1.9% |
| Spread user | 1,996 | 263 | **-86.8%** |
| Spread native | 1,977 | 249 | **-87.4%** |

The non-spread controls moved by at most four milliseconds and do not execute the changed path.
Seven rotated full V8-v7 runs had composite medians of 8,316 before and 8,267 after (-0.6%),
against wide observed ranges of 7,680–8,581 and 5,877–8,541. This is flat within run-to-run
variance; that ES5-era suite has no representative hot spread-call workload, so no aggregate gain
is claimed.

## Validation scope

Focused tests require a JIT hit and cover receiver and argument evaluation order, owned object
arguments across forced GC, proxy callable targets, target exceptions, custom `@@iterator`,
replaced Array Iterator `next`, element accessors, inherited holes, proxy iterables, foreign-realm
arrays, exact poll counts, and recursion-limit restoration. The full engine tests and benchmark
suites remain the final regression gate.
