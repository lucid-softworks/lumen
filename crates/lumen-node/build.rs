//! Precompile the `node:` compat JS glue to an AST snapshot at build time (see
//! `lumen-web/build.rs` for the rationale — parsing the static glue on every boot is the
//! dominant cold-start cost). Assembles the same IIFE `lib.rs` used to `concat!` and writes the
//! source + snapshot to `OUT_DIR`, the single source of truth.

use std::path::PathBuf;

/// Order matters: preamble first; each builtin registers itself into `__builtins` as it loads, so
/// dependencies must come first (events ← stream ← http; util/crypto/shims before their users);
/// module.js is LAST because it snapshots `__builtins.keys()` as the core-module set.
///
/// `wrap` puts a file's body in its own `{ }` block so its top-level `const`/`class` names stay
/// private (the whole glue is one IIFE, so unwrapped files share a scope and would collide). The
/// original files kept names globally unique; the newer module files reuse names (EventEmitter,
/// Readable, …) and rely on block isolation, talking to each other only through `__builtins`.
struct GlueFile {
    name: &'static str,
    wrap: bool,
}
const JS_FILES: &[GlueFile] = &[
    GlueFile { name: "preamble.js", wrap: false },
    GlueFile { name: "buffer.js", wrap: false },
    GlueFile { name: "path.js", wrap: false },
    GlueFile { name: "os.js", wrap: false },
    GlueFile { name: "events.js", wrap: true },
    GlueFile { name: "diagnostics_channel.js", wrap: true },
    GlueFile { name: "domain.js", wrap: true },
    GlueFile { name: "trace_events.js", wrap: true },
    GlueFile { name: "util.js", wrap: true },
    GlueFile { name: "console.js", wrap: true },
    GlueFile { name: "timers.js", wrap: true },
    GlueFile { name: "crypto.js", wrap: true },
    GlueFile { name: "punycode.js", wrap: true },
    GlueFile { name: "shims.js", wrap: true },
    GlueFile { name: "stream.js", wrap: true },
    GlueFile { name: "net.js", wrap: true },
    // fs.js is unwrapped (shares the outer scope) but its ReadStream/WriteStream/StatWatcher
    // extend the `stream`/`events` builtins, so it must load after them.
    GlueFile { name: "fs.js", wrap: false },
    GlueFile { name: "http.js", wrap: true },
    GlueFile { name: "http2_codec.js", wrap: true },
    GlueFile { name: "http2.js", wrap: true },
    GlueFile { name: "child_process.js", wrap: true },
    GlueFile { name: "dns.js", wrap: true },
    GlueFile { name: "stdlib_extras.js", wrap: true },
    GlueFile { name: "tls.js", wrap: true },
    // Loaded after stdlib_extras so the real implementation replaces its compatibility stub.
    GlueFile { name: "worker_threads.js", wrap: true },
    GlueFile { name: "vm.js", wrap: true },
    GlueFile { name: "repl.js", wrap: true },
    GlueFile { name: "cluster.js", wrap: true },
    GlueFile { name: "dgram.js", wrap: true },
    GlueFile { name: "wasi.js", wrap: true },
    GlueFile { name: "constants.js", wrap: true },
    GlueFile { name: "bun_ffi.js", wrap: true },
    GlueFile { name: "bun_jsc.js", wrap: true },
    GlueFile { name: "bun_sqlite.js", wrap: true },
    GlueFile { name: "bun_redis.js", wrap: true },
    GlueFile { name: "bun_cookies.js", wrap: true },
    GlueFile { name: "bun_router.js", wrap: true },
    GlueFile { name: "bun.js", wrap: true },
    GlueFile { name: "typescript_strip.js", wrap: true },
    GlueFile { name: "module.js", wrap: false },
];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest.join("src/js");

    let mut glue = String::from("(() => {\n");
    for file in JS_FILES {
        let path = src_dir.join(file.name);
        println!("cargo:rerun-if-changed={}", path.display());
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if file.wrap {
            glue.push_str("{\n");
            glue.push_str(&body);
            glue.push_str("\n}\n");
        } else {
            glue.push_str(&body);
        }
    }
    glue.push_str("\n})();");

    let snapshot = lumen::compile_snapshot(&glue)
        .unwrap_or_else(|e| panic!("node glue failed to parse for snapshotting: {e}"));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("node_glue.js"), &glue).unwrap();
    std::fs::write(out.join("node_glue.snap"), &snapshot).unwrap();

    std::fs::write(out.join("ffi_trampolines.rs"), generate_ffi_trampolines()).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}

/// Maximum FFI argument count and the JSCallback thunk-pool size per arity. Kept in sync with the
/// `MAX_ARGS`/`JSCB_POOL` consts in `src/ffi.rs`.
const MAX_ARGS: usize = 8;
const JSCB_POOL: usize = 16;

/// Emit the libffi-free trampoline dispatch (`src/ffi.rs` `include!`s this).
///
/// # The ABI-class monomorphization
///
/// A native call whose argument register classes are only known at runtime is normally handled by
/// libffi. lumen has no libffi, so instead we *monomorphize*: for every register-class signature we
/// support, we emit a concrete `extern "C" fn(..) -> T` pointer type, `transmute` the resolved
/// symbol to it, and call it — letting the Rust/LLVM backend place each argument in the register the
/// platform ABI dictates.
///
/// The key trick that keeps this finite: on both x86-64 SysV and arm64 AAPCS, integer/pointer
/// arguments and floating-point arguments are assigned to *independent* register files (SysV
/// INTEGER vs SSE; AAPCS GPR/NGRN vs SIMD/NSRN). For register-class-only calls within the register
/// budget (≤8 of each here), an argument's home depends solely on how many prior arguments of the
/// *same* class there were — never on the interleaving. So an interleaved signature like
/// `f(int, double, int)` places its args identically to the canonical `f(int, int, double)`. The
/// caller therefore reorders actual arguments into "all integers first (in order), then all floats
/// (in order)", and we only enumerate canonical shapes: an integer count plus a float-width
/// sequence. That collapses 3^n interleavings to ~1000 shapes.
///
/// Floats still need per-position `f32`/`f64` types (they share a SIMD register but differ in the
/// bit pattern the callee reads), so the float suffix is enumerated over width bitmasks.
///
/// Returns fold to three kinds — integer/pointer/void returns all come back through `-> u64` (the
/// caller masks to the declared width; a `void` callee just leaves the register garbage we ignore),
/// plus `-> f32` and `-> f64`.
fn generate_ffi_trampolines() -> String {
    let mut s = String::new();
    s.push_str("// @generated by build.rs — bun:ffi trampoline dispatch. Do not edit.\n\n");

    for (fname, rt) in [("call_int", "u64"), ("call_f32", "f32"), ("call_f64", "f64")] {
        s.push_str(&format!(
            "/// Trampoline into a native function returning the `{rt}` register class.\n\
             pub unsafe fn {fname}(f: *const core::ffi::c_void, ints: &[u64], floats: &[FArg]) -> {rt} {{\n\
             \x20   match (ints.len(), floats.len(), fmask(floats)) {{\n"
        ));
        for i in 0..=MAX_ARGS {
            for fc in 0..=(MAX_ARGS - i) {
                for mask in 0u32..(1u32 << fc) {
                    let mut params = Vec::new();
                    let mut argx = Vec::new();
                    for a in 0..i {
                        params.push("u64".to_string());
                        argx.push(format!("ints[{a}]"));
                    }
                    for j in 0..fc {
                        if mask & (1 << j) != 0 {
                            params.push("f64".to_string());
                            argx.push(format!("floats[{j}].f64v()"));
                        } else {
                            params.push("f32".to_string());
                            argx.push(format!("floats[{j}].f32v()"));
                        }
                    }
                    s.push_str(&format!(
                        "        ({i}, {fc}, {mask}) => {{ let g: extern \"C\" fn({}) -> {rt} = core::mem::transmute(f); g({}) }}\n",
                        params.join(", "),
                        argx.join(", "),
                    ));
                }
            }
        }
        s.push_str("        _ => unreachable!(\"ffi signature outside the supported register budget\"),\n");
        s.push_str("    }\n}\n\n");
    }

    // JSCallback thunk pool: a fixed matrix of static `extern "C"` functions, one per (arity, slot),
    // each forwarding into the JS-re-entry dispatcher with its baked-in coordinates. All arguments
    // arrive as `u64` (integer/pointer register class only — float-arg callbacks are refused at
    // registration, so no float thunks are needed) and every thunk returns `u64`.
    let ncols = JSCB_POOL;
    for n in 0..=MAX_ARGS {
        for k in 0..ncols {
            let params: Vec<String> = (0..n).map(|a| format!("a{a}: u64")).collect();
            let argslice: Vec<String> = (0..n).map(|a| format!("a{a}")).collect();
            s.push_str(&format!(
                "extern \"C\" fn jscb_{n}_{k}({}) -> u64 {{ jscb_dispatch({n}, {k}, &[{}]) }}\n",
                params.join(", "),
                argslice.join(", "),
            ));
        }
    }
    s.push_str(
        "\n/// The raw address of thunk `(arity, slot)` — handed to native code as the callback pointer.\n\
         #[allow(clippy::fn_to_numeric_cast_any)]\n\
         pub fn jscb_thunk_ptr(n: usize, k: usize) -> *const core::ffi::c_void {\n\
         \x20   match (n, k) {\n",
    );
    for n in 0..=MAX_ARGS {
        for k in 0..ncols {
            s.push_str(&format!(
                "        ({n}, {k}) => jscb_{n}_{k} as *const core::ffi::c_void,\n"
            ));
        }
    }
    s.push_str("        _ => core::ptr::null(),\n    }\n}\n");
    s
}
