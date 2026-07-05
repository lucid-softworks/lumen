# lumen

A from-scratch JavaScript engine in Rust — std only, zero dependencies, no unsafe JIT tricks:
a lexer, parser, and tree-walking interpreter with generators and `async`/`await` running on
stackful coroutines, full `RegExp` (including `\p{…}` and inline modifiers), typed arrays,
`Proxy`/`Reflect`, ES modules (top-level await, `import defer`, source phase), `Intl`, and
`Temporal`.

**Passes 100% of [tc39/test262](https://github.com/tc39/test262): 53,376/53,376** (including
annexB, intl402, and staging).

Extracted from — and used by — the [lucid-softworks/browser](https://github.com/lucid-softworks/browser)
engine as its JS backend (`backend-lumen`), with full git history.

## Usage

```sh
cargo build --release -p lumen --bin lumen
./target/release/lumen file.js [more.js ...]   # minimal shell: evaluate files in one engine
```

## Conformance

```sh
scripts/test262-clone.sh    # one-time: clone the suite into ./test262
scripts/run-test262.sh      # run it (see crates/test262-runner for env knobs)
```

## Benchmarks

```sh
scripts/run-v8bench.sh      # classic V8 suite (v8-v7); downloads on first run
```
