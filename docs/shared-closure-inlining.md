# Shared-closure inlining

`LUMEN_JIT_INLINE_CLOSURES=1` enables guarded inlining across fresh instances of a
function sharing its caller's live, non-global lexical environment. It is off by
default. Run it independently or with `LUMEN_JIT_REFRESH_CLOSURE_CACHE=1`:

```sh
cargo build --release -p lumen
LUMEN_JIT_INLINE_CLOSURES=1 target/release/lumen crates/lumen/benches/fixtures/shared_closure_calls.js
```

The existing inliner compares the original closure's object and environment
addresses. A recreated closure misses even when its AST and body are unchanged.
The optional guard instead checks the shared AST Function and the current lexical
environment. A weak Function pin prevents address reuse; proxy, class, non-plain,
`with`, mismatched-environment and incompatible receiver cases use the original
call. Existing size, strictness, argument-shape and body admission limits apply.
Global-function inlines retain their existing identity guards.

A successful splice moves the actual callee into a hidden local until its return.
Before an observable helper runs, reflection metadata snapshots that closure's
weak identity from the activation's locals. Nested `caller` reflection and GC
therefore resolve the live closure, rather than the original compilation target.
Successful returns reset the hidden owner; exception paths use ordinary frame
ownership. Snapshots own no strong references and the physical frame ABI stays
32 bytes. Both wide and packed local representations are supported by the recorder.

Native guards first try the original weak-pinned identity and environment checks;
fresh instances call a Rust predicate to validate the broader proof. Calls within
these inlines use checked helpers, and affected chunks exclude loop regions and
mixed object/numeric regions whose ownership publication does not yet model the
hidden callee. These conservative choices limit whole-application gains. This is
an experimental call-path improvement, not a claim of Node/Bun parity.

The fixture warms one sibling closure pair, then measures 1.6 million calls across
50,000 fresh pairs, checking the aggregate result. Colocated engine tests cover
fresh captures and reflected identities in native and bytecode execution; invalid
function/environment/callable guards; nested getters, exceptions and GC lifetime;
and object versus boxed primitive receivers with missing arguments.

## Measured results (2026-09-08)

Implementation commits: `09f7025` and `835d2fd` on
`perf/closure-call-inlining`. The retained executable is from `2faa9d7` (the
preceding cache-refresh implementation). The final candidate is from `835d2fd`.
[Raw samples, executable/fixture hashes and validation counts](benchmarks/shared-closure-inlining-2026-09-08.json)
are checked in. Measurements used an Apple M4, macOS arm64, release builds, three
rotated fresh-process runs per mode, and no overlapping task builds, tests or
profiles. Other activity on the machine was not controlled.

`Off` has both switches unset. `Inline` sets `LUMEN_JIT_INLINE_CLOSURES=1`.
`Combined` also sets `LUMEN_JIT_REFRESH_CLOSURE_CACHE=1`. Each timed fixture checks
its result; Djot parses 10,000 documents and verifies 3,160,000 HTML characters,
and standalone DeltaBlue performs 5,000 iterations. The classic score uses the
unchanged checked-in V8-v7 suite. Djot and classic drivers are the same bundled
and concatenated inputs as the [previous comparison](closure-call-cache.md).

| Median | Retained | Off | Inline | Combined |
| --- | ---: | ---: | ---: | ---: |
| Shared-closure fixture, ms | 102 | 101 | 55 | 56 |
| Earlier closure fixture, ms | 113 | 112 | 114 | 79 |
| Djot, ms | 3449 | 3457 | 3499 | 3427 |
| DeltaBlue, ms | 8297 | 8317 | 8323 | 8333 |
| Classic score (higher is better) | 8712 | 8595 | 8549 | 8557 |

Shared-closure inlining reduces the focused fixture's time by **45.5%** against
Off. Combining it with cache refresh also preserves the earlier fixture's win.
There is no demonstrated broad engine win: Djot takes 1.2% more time with Inline,
DeltaBlue is essentially flat, and the classic aggregate is slightly lower.
The NavierStokes subscore falls from 38507 to 36545 (5.1%) with Inline and to
35989 (6.5%) with Combined. All per-suite samples are in the raw report; some
baseline classic runs also show substantial variation. The switch stays off by
default, and region ownership/publication needs further work before promotion.

A separate diagnostic run counts **159,965 hits / 0 misses** at
`handleEvent -> topContainer` and **159,751 hits / 0 misses** at
`pushContainer -> addBlockAttributes`. The earlier identity guards had 13/159,952
and 7/159,744 hits/misses respectively. These are ordinary native guard-edge
counts, collected outside timing, and demonstrate that recreated closures now
enter the splice. They do not measure the time saved there.

Fresh-process reference runs of the same Djot input give medians of **224 ms**
for Node v24.18.0 and **141 ms** for Bun 1.3.14. Combined Lumen remains about
15.3x and 24.3x slower respectively. Reference runtimes were measured after,
rather than interleaved with, Lumen. The within-2x objective remains unmet.
The tiny Node/Bun shared-fixture times (1–5 ms) are retained in the raw report,
but their millisecond timer resolution makes fine comparisons inappropriate.

## Validation and remaining scope

- Engine unit tests: **801 pass**, both disabled and enabled; the final identity
  fast path also passes all 801 with the feature enabled.
- Workspace with the feature enabled at `09f7025`: **1087 pass, 2 ignored**.
  Three runtime integration targets were excluded: `http2_client`,
  `http2_secure_client`, and `http2_server`. Their Node interoperability failures
  or hangs were already reproduced on exported main during the preceding work;
  see the previous report. Node was on PATH for the selected integration checks.
- Selected Test262 expressions/statements on the final release binary:
  **20438/20439 pass** both off and on. The same existing failure is
  `async: TestError: TestError: The import of C`; the runner rounds this to 100%.
- Differential checking on the final enabled binary: **1996 agreements,
  4 budget skips**, seeds 1 through 2000.
- Formatting and focused structure audits pass. Strict all-target/all-feature
  Clippy has the identical diagnostic-message multiset as exported main:
  **86 lib / 88 lib-test errors**, with no added diagnostics.

Call splicing now belongs to `bytecode/inlining.rs`, guard admission to
`bytecode/inline_closure.rs`, and dynamic reflection to the existing frame
modules. The focused modules introduce no structural audit signals. The legacy
`bytecode.rs` and `jit.rs` remain oversized; native call-template emission is a
cohesive next extraction. Runtime test reports remain in the existing ignored
`test262-report/` directory; raw benchmark samples needed for review are tracked.
