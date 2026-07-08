# yt-dlp/ejs on lumen

[yt-dlp/ejs](https://github.com/yt-dlp/ejs) is yt-dlp's external JavaScript challenge
solver: it bundles **meriyah** (a JavaScript parser written in JavaScript) and **astring**
(a code generator), parses YouTube's multi-megabyte player source, transforms it, and
solves the sig/nsig challenges. As a workload it is brutal on an engine's string
implementation — a scanner reading a ~3 MB source one `charCodeAt` at a time, tokens
`slice`d out by the thousand, and the transformed source rebuilt through repeated string
concatenation.

That made it a perfect stress test: [issue #12](https://github.com/lucid-softworks/lumen/issues/12)
reported the benchmark below taking **55.8 minutes** on lumen.

## Running it

```sh
./fetch.sh        # downloads the benchmark bundle (solver + embedded player + challenges)
./bench.sh        # times it on lumen (and node/bun/qjs when installed)
```

The bundle is the exact reproducer from issue #12 — the ejs core solver with a player and
two `n`-challenges baked in; it prints the solved challenge results as JSON. To build a
fresh bundle from upstream instead, see yt-dlp/ejs's own build docs (`rollup` via
`pnpm`/`deno`/`bun`; release assets ship the prebuilt `yt.solver.core.js`).

## Numbers (Apple M-series, 2026-07)

| runtime | time | notes |
|---|---:|---|
| node 22 | 0.44 s | |
| bun 1.3 | 0.47 s | |
| qjs (quickjs-ng, brew) | 3.4 s | (the 8.2 s in issue #12 was a slower machine) |
| **lumen** (`--tier=jit`) | **8.7 s** | was **55.8 min** at the time of issue #12 |

What the 55.8 minutes was made of, in the order it fell:

1. **Per-call UTF-16 materialization** — every `charCodeAt`/`s[i]`/`.length` walked or
   converted the *whole* UTF-8 string per call: O(n²) for meriyah's scanner. Fixed with a
   pointer-keyed cache of each string's UTF-16 view (ASCII indexes bytes directly).
   55.8 min → 2.7 min.
2. **Double-copy concatenation** — each `a + b` copied the accumulation twice.
   2.7 min → 82 s.
3. **The append loop itself** — `s += x` on immutable exactly-sized strings is inherently
   O(n²). The engine string is now a thin refcounted buffer *with spare capacity* (`LStr`,
   the design original QuickJS uses), and the fused `obj.k += v` op appends **in place** when the
   accumulator is uniquely referenced — astring's `write()` becomes amortized O(1).
   82 s → 16 s.
4. **Compiled-tier coverage and inline caches** — object-destructuring declarations (at
   block *and* function-body level), simple parameter defaults, optional chains (`a?.b`,
   `r?.m(x)`), `for…of` (with exact IteratorClose semantics on break/return/throw), and
   template literals now compile; and the inline property caches are guarded by a
   *per-object* side-table flag instead of a global latch (one `Uint8Array` existing no
   longer disables every cache in the program). meriyah's parser now runs almost entirely
   on the compiled tiers. → 8.7 s.

(Chasing this also surfaced and fixed a real compiled-tier bug: `break` out of a `try`
block leaked its handler, silently swallowing unrelated later throws.)

The remaining ~2.5× to qjs is call overhead (meriyah's mutually recursive parse
functions), AST-node allocation/GC, and refcount churn — tracked on issue #12.
