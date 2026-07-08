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
| qjs (quickjs-ng) | 8.2 s | from issue #12, different machine |
| **lumen** (`--tier=jit`) | **82 s** | was **55.8 min** before the fixes below |
| lumen (default tier) | 93 s | the remaining cost is memcpy, so the tiers converge |

Two engine pathologies accounted for essentially all of the original 55.8 minutes, both
O(n²) over the player size:

1. **Per-call UTF-16 materialization.** lumen stores strings as UTF-8; every
   `charCodeAt`/`s[i]`/`.length` walked or converted the *whole* string per call —
   O(n) per character read, O(n²) for meriyah's scanner. Fixed with a pointer-keyed
   cache of each string's UTF-16 view (ASCII strings index their bytes directly);
   first access per string is O(n), every one after is O(1). 55.8 min → 2.7 min.
2. **Double-copy concatenation.** Every `a + b` built a `String` (copy one) and then an
   `Rc<str>` from it (copy two). The append loop that rebuilds the transformed player
   copies the accumulated string each step regardless, but halving the bytes moved
   halved the time. 2.7 min → 82 s.

The remaining gap to node/qjs is the append loop itself: `s += x` with immutable
`Rc<str>` strings is inherently O(n²) — the same problem the original QuickJS solved by
appending in place when the string is uniquely referenced and has spare capacity
(quickjs-ng/quickjs#1002 tracks merging that). That needs a capacity-carrying string
representation in `Value::Str` and is tracked as follow-up work on issue #12.
