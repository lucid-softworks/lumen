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
    GlueFile { name: "fs.js", wrap: false },
    GlueFile { name: "events.js", wrap: true },
    GlueFile { name: "diagnostics_channel.js", wrap: true },
    GlueFile { name: "domain.js", wrap: true },
    GlueFile { name: "trace_events.js", wrap: true },
    GlueFile { name: "util.js", wrap: true },
    GlueFile { name: "crypto.js", wrap: true },
    GlueFile { name: "shims.js", wrap: true },
    GlueFile { name: "stream.js", wrap: true },
    GlueFile { name: "net.js", wrap: true },
    GlueFile { name: "http.js", wrap: true },
    GlueFile { name: "http2.js", wrap: true },
    GlueFile { name: "child_process.js", wrap: true },
    GlueFile { name: "dns.js", wrap: true },
    GlueFile { name: "stdlib_extras.js", wrap: true },
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
    println!("cargo:rerun-if-changed=build.rs");
}
