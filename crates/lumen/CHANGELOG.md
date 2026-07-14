# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/lucid-softworks/lumen/compare/v0.1.2...v0.1.3) - 2026-07-14

### Added

- *(lumen)* add cross-platform JIT backends
- *(bun)* back jsc diagnostics with lumen heap
- *(bun)* implement bun:ffi via a libffi-free trampoline
- default to JIT tier and add a CLI help menu ([#17](https://github.com/lucid-softworks/lumen/pull/17))
- *(lumen)* inline DestructureGuard and cached-absence prop reads
- *(lumen)* depth-1 name ICs through fresh activations
- *(lumen)* home captured once-per-call block lexicals in the activation
- *(lumen)* compile delete, spread calls, and destructuring for-of heads
- *(lumen)* computed string-key reads through the property ICs
- *(lumen)* dedicated GetProp helper; document the direct-call arity cap
- *(lumen)* dedicated MakeObject/SetProp helpers + under-application direct calls
- *(lumen)* absent-property ICs + Tdz/string-Const templates
- *(lumen)* thread-local size-class caching allocator
- *(lumen)* func-keyed IC retry for fresh closure identities
- *(lumen)* call ICs for needs-env callees + Rc<str> scope keys
- *(lumen)* pre-shaped map templates for closures and object literals
- *(lumen)* native call ICs, charCodeAt intrinsic, string-method ICs
- *(lumen)* x23-x28 join the loop-chain pin pool (NavierStokes +6%)
- *(lumen)* zero-copy aliases + leaner guards in loop chains (NavierStokes +13%)
- *(lumen)* free names in loop chains + receiver pins (NavierStokes +45%)
- *(lumen)* direct shared-ctx JIT→JIT calls on by default
- *(lumen)* LUMEN_GC_DUMP=1 prints external-rooted objects and scopes
- *(lumen)* probe all 4 call-IC ways inline + direct-call teardown stub
- *(lumen)* widen property ICs to 4 ways
- *(lumen)* direct shared-ctx JIT→JIT calls (opt-in: LUMEN_JIT_DIRECT=1)
- *(cli)* nightly builds stamp --version with the commit hash
- *(lumen)* direct-call scaffolding — handler_floor, CallIc.direct gates, jit_direct_finish
- *(cli)* lumen --version / -v on both binaries
- *(lumen)* inline way-1 call-IC probe in the JIT call template (arc 3a)
- *(lumen)* InterpLayout — runtime-probed Interp field offsets for the asm call thunk
- *(lumen-runtime)* reportError + global onerror/onunhandledrejection — WinterTC 56/56
- *(lumen)* JSON modules — JSON.parse semantics for attribute-less .json imports, tests
- *(lumen)* import-bytes modules — Uint8Array over an immutable buffer, plus the missing indexed-write guard
- *(lumen)* import-text modules end to end — attribute-aware host loader
- *(lumen)* put Intl/ECMA-402 behind a default-on 'intl' cargo feature — 32% smaller binaries without it

### Fixed

- *(lumen)* revalidate direct calls after GC polls
- *(node)* detect proxy and key object types
- *(lumen)* keep transient computed keys out of the pointer-keyed stub cache
- *(lumen)* resolve helper-op envs through the swapped env_raw
- *(lumen)* gate pointer-layout asserts to the JIT platform — wasm32 CI break
- *(lumen)* cfg-gate loop-chain planner types for non-aarch64 targets
- *(lumen)* NameIc global-mode packing is u64, not usize — unbreaks the wasm32 build

### Other

- *(lumen)* inline hot x64 JIT operations
- *(lumen)* poll direct calls by allocation pressure
- *(lumen)* widen direct-call gc polling window
- *(lumen)* compact warmed property probes
- *(lumen)* amortize jit call gc polling
- *(lumen)* inline bounded polymorphic call chains
- *(lumen)* recycle object registry slots
- *(lumen)* move activation calls into fixed jit frames
- *(lumen)* inline packed numeric element writes
- *(lumen)* inline packed dense element reads
- *(lumen)* inline constructor field creation
- *(lumen)* inline ordinary instanceof checks
- *(lumen)* resume direct calls after GC polls
- *(lumen)* compile regular expression literals
- *(lumen)* compile free-name compound assignments
- *(lumen)* embed constructor creation guards
- *(lumen)* right-size constructor field storage
- *(lumen)* fast-path dense apply arguments
- *(lumen)* protect default instanceof dispatch
- *(lumen)* cache well-known symbol property keys
- *(lumen)* inline hot property lookup primitives
- *(lumen)* streamline ordinary instanceof checks
- *(lumen)* pack resident property values
- *(lumen)* right-size small property maps
- *(lumen)* shrink object allocations to 128 bytes
- *(lumen)* compact sparse runtime vectors
- *(lumen)* compact runtime values
- *(lumen)* pack property descriptor flags
- *(lumen)* release dead object storage sooner
- *(lumen)* pack small dense numeric arrays
- *(lumen)* compact property map metadata
- *(lumen)* reduce dense array memory
- *(lumen)* speed up v8 hot paths and bound memory
- *(lumen)* capture-scan bail log names the function
- *(lumen)* LUMEN_JIT_LOOPLOG reports vec pins and names
- *(lumen)* FnFrame repr(C) + offset asserts (arc 3b prep)
- *(lumen)* assert JitCtx.interp at asm-visible offset 72 (arc 3b prep)
- *(lumen)* split call_jit_cached into probe + call_jit_committed
- *(lumen)* inline the shared-reference decrement in run_moved's slot drops
- *(lumen)* identity-cached JIT construct — construct_jit_fast
- *(lumen)* set-side stub cache + raw-pointer realm-root walk
- *(lumen)* key-checked JIT property arm + two-way set template
- *(lumen)* call-path trims — boxed pending_tail, refcount-free env borrow
- *(lumen)* array shape stability + key-checked ICs + dense push/pop + stub cache
- *(lumen)* halve Property — box the accessor pair (80B → 40B)
- *(lumen)* receiver-direct property writes — SetPropThisDrop/SetPropLocalDrop
- *(lumen)* receiver-direct property reads (GetPropThis / GetPropLocal)
- *(lumen)* two-way property ICs + polymorphic and nested splicing
- *(lumen)* inline template for Op::Undef
- *(lumen)* splice free-name, index, and plain call sites too
- *(lumen)* mirror-slim element stores in per-op chains
- *(lumen)* mirror element reads in the standalone element templates
- *(lumen)* raw-f64 dense element mirror + guard-free int element loads
- *(lumen)* speculative inlining of hot monomorphic callees
- *(lumen)* loop-spanning JIT register chains with integer lowering
- *(lumen)* inline store/eq/call/creation ICs in the JIT — v8-v7 composite 1,023 -> 1,222, Richards 670 -> 1,150, DeltaBlue 472 -> 659
- *(lumen)* compile optional chains, for-of, body-level destructuring, templates — ejs 15 s -> 8.7 s; fixes a compiled-tier handler leak
- *(lumen)* capacity-carrying strings (LStr) + in-place append — yt-dlp/ejs 82 s -> 15 s (223x total vs issue #12)
- *(lumen)* kill two O(n^2) string pathologies — yt-dlp/ejs 55.8 min -> 82 s
- *(lumen)* chain-level receiver caching — NavierStokes ~14k, composite holds past 1,000
- *(lumen)* Crypto 531 -> ~3,300, composite past 1,000 — chain hygiene, dense pad, global name IC, JIT-to-JIT fast call
- *(lumen)* 10x NavierStokes (1,041 -> 11,222) — register chains, name IC, element templates, loop tier-up

## [0.1.2](https://github.com/lucid-softworks/lumen/compare/v0.1.1...v0.1.2) - 2026-07-08

### Added

- *(lumen)* ARM64 template JIT tier (--tier=jit) for macOS/Apple Silicon
- *(lumen)* ESM↔CJS interop, file:// import.meta, async-safe import()
- *(lumen-node)* expand N-API to rollup's full surface
- *(lumen-node)* load N-API native addons (.node)
- *(lumen)* data-carrying native callable (Callable::NativeData)
- *(lumen-node)* node:child_process (real subprocesses)
- *(lumen-runtime)* report unhandled promise rejections
- *(lumen)* embed helpers for the i64/number value bridge
- *(lumen)* capture call-stack frames for Error.stack
- *(lumen)* AST snapshot codec for precompiled glue
- *(lumen)* typed-array byte bridge on the embed surface
- *(lumen)* eval_value + precise incomplete-input detection for the REPL
- *(lumen)* curated embedder surface behind an `embed` feature
- *(lumen)* interactive REPL — persistent realm, real multi-line continuation
- *(lumen)* bytecode execution tier v0 — opt-in, oracle-checked, 247 on v8-v7
- *(lumen)* wasm playground — engine compiled to WebAssembly + GitHub Pages site

### Fixed

- *(lumen)* correct the JIT inline-cache object-graph offsets (they were disabling the IC)
- *(lumen)* gate JIT Value-layout asserts to aarch64-macos (wasm build)
- *(lumen)* hoist var declarations out of switch cases
- *(lumen)* resolve CLDR unit patterns by category-prefixed id
- *(lumen)* run every/some hole check through proxy [[HasProperty]]
- *(lumen)* round Number.prototype.toFixed half-up on exact ties
- *(lumen)* thread label into while/do-while loops so labelled continue works
- *(lumen)* keep coroutine return values when tail-call state leaks in
- *(lumen)* [Symbol.asyncDispose] calls return() with no arguments

### Other

- *(lumen)* walk the scope chain by raw pointer in get_var_with (no per-hop Rc clone)
- *(lumen)* extract run_compiled_chunk / bind_compiled_this from the lean call path
- *(lumen)* inline property caches in JIT machine code (GetProp / GetMethod)
- *(lumen)* object shapes (hidden classes) for O(1) inline-cache validation
- *(lumen)* walk the property IC prototype chain by raw pointer, no per-hop Gc clones
- *(lumen)* index-free small property maps, interned function keys, FxHash GC scope index
- *(lumen)* regex scan prescan + byte-mode subjects, deferred RegExp statics, O(n) array truncation
- *(lumen)* closures, constructs, proto-ICs, and lean calls in the bytecode VM
- *(lumen)* std-only benchmark harness + engine and internals suites
- *(lumen)* split builtins/mod.rs into per-object modules
- *(lumen)* drop dead non-trap-aware has_property
- *(lumen)* compile labelled break/continue on the bytecode VM
- *(lumen)* compile try/catch in the bytecode VM (incl. across await)
- *(lumen)* run async functions on the bytecode VM (no OS-thread coroutine)
- *(lumen)* inline caches for bytecode property get/set
- *(lumen)* pool coroutine worker threads instead of one-per-call
- *(lumen-bytecode)* in-place ++/-- op and discard-mode stores
- *(lumen)* dense array elements + in-place local updates — 186 on v8-v7 (from 167)
- *(lumen)* +44% on the classic V8 suite — string fast paths, cached hoisting, leaner hot loops

## [0.1.1](https://github.com/lucid-softworks/lumen/compare/v0.1.0...v0.1.1) - 2026-07-05

### Other

- update Cargo.lock dependencies
