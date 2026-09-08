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

For now, shared-closure guards call a Rust predicate from native code. Calls within
these inlines use checked helpers, and affected chunks exclude loop regions and
mixed object/numeric regions whose ownership publication does not yet model the
hidden callee. These conservative choices limit whole-application gains. This is
an experimental call-path improvement, not a claim of Node/Bun parity.

The fixture warms one sibling closure pair, then measures 1.6 million calls across
50,000 fresh pairs, checking the aggregate result. Colocated engine tests cover
fresh captures and reflected identities in native and bytecode execution; invalid
function/environment/callable guards; nested getters, exceptions and GC lifetime;
and object versus boxed primitive receivers with missing arguments.
