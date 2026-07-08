# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
