//! Precompile the `node:` compat JS glue to an AST snapshot at build time (see
//! `lumen-web/build.rs` for the rationale — parsing the static glue on every boot is the
//! dominant cold-start cost). Assembles the same IIFE `lib.rs` used to `concat!` and writes the
//! source + snapshot to `OUT_DIR`, the single source of truth.

use std::path::PathBuf;

/// Order matters (preamble first; module.js's require builds on the others).
const JS_FILES: &[&str] = &[
    "preamble.js",
    "buffer.js",
    "path.js",
    "os.js",
    "fs.js",
    "module.js",
];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest.join("src/js");

    let mut glue = String::from("(() => {\n");
    for file in JS_FILES {
        let path = src_dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        glue.push_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
        );
    }
    glue.push_str("\n})();");

    let snapshot = lumen::compile_snapshot(&glue)
        .unwrap_or_else(|e| panic!("node glue failed to parse for snapshotting: {e}"));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("node_glue.js"), &glue).unwrap();
    std::fs::write(out.join("node_glue.snap"), &snapshot).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
