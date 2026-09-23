# JIT call and constructor dispatch audit

This audit deliberately excludes the fresh/shared-closure caching and inlining work in PR #23.
It covers stable direct calls, method calls, native calls, recursion, and construction on `main`
at `f887240`.

## Profile

Stable ordinary calls already settle into the four-way call IC. With
`LUMEN_JIT_CALLSTAT=1`, two million direct calls, two million method calls, two million native
calls, and 600,000 recursive calls produced only 1,145 periodic GC-maintenance fallbacks and 300
initial tier-settling fallbacks. The warmed direct-call path otherwise remained in generated code.

Simple base-class construction did not use the constructor IC at all. A ten-second native sample
of 50 million `new Pair(left, right)` operations repeatedly entered
`construct`, `construct_dispatch`, `run_constructor_on`, and `call_user`, even though the class had
no fields, private members, decorator initializers, or derived `super()` setup. Two million such
constructions took several times as long as the direct and method-call controls.

## Optimization

Fieldless base classes can now enter the existing identity-cached JIT constructor path. The
constructor-only cache records this eligibility after validating the class metadata. Derived
classes and classes with fields, private members, or decorator initializers continue through the
full class-construction path.

The fast entry keeps the existing live prototype read, realm and proxy guards, recursion limit,
GC polling, argument ownership transfer, reflective frame, `this` and `new.target` setup,
exception propagation, return override, and tail-call drain. It also clears and restores the
derived-constructor `super()` permission around the base-class body, matching
`run_constructor_on`.

## Measurements

The focused benchmark is `crates/lumen/benches/call_dispatch.js`. Seven fresh-process rounds were
rotated between clean release builds of `f887240` and this change on Apple Silicon. Values are
medians in milliseconds; lower is better.

| Two million operations | Before | After | Change |
| --- | ---: | ---: | ---: |
| Stable direct calls | 43 | 43 | 0% |
| Stable method calls | 46 | 46 | 0% |
| Fieldless base-class construction | 257 | 97 | -62% |
| Stable native calls | 44 | 44 | 0% |
| Recursive-call control | 10 | 11 | timer-level noise |

Three rotated full V8-v7 runs had composite medians of 7,116 before and 6,993 after (-1.7%). The
individual composite ranges were 6,592–7,206 and 5,048–7,612 respectively, so this broad suite is
flat within the machine's observed run-to-run variance. It does not contain a comparable hot
fieldless-class construction workload, and no broad-suite gain is claimed.

## Validation scope

Focused tests cover all execution tiers, recursive construction, argument-before-body evaluation
order, thrown-error frame capture and recovery, class-call rejection, prototype mutation,
constructor return overrides, field initializer errors, private members, `new.target`, proxies,
and cross-realm guard misses. The repository's full engine suite and script tests remain the final
regression gate.
