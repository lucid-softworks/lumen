//! Precompile the web platform's JS glue to an AST snapshot at build time, so the runtime
//! *decodes* it on boot instead of re-lexing/parsing it — parsing the static glue is the
//! dominant cold-start cost. This assembles the one IIFE that used to live as a `concat!` in
//! `lib.rs` and writes both the assembled source (`web_glue.js`) and its snapshot blob
//! (`web_glue.snap`) to `OUT_DIR`; `lib.rs` `include_*!`s both. Assembling here makes this the
//! single source of truth — the source and its snapshot can't drift.
//!
//! `lumen` is a build-dependency purely for `compile_snapshot` (parse + encode). If the glue
//! ever fails to parse, the build fails here with a clear message.

use std::path::PathBuf;

/// Order matters: `preamble` captures and deletes the raw `__*` namespaces before the standard
/// classes are defined over them. Keep in sync with what each file expects.
const JS_FILES: &[&str] = &[
    "preamble.js",
    "events.js",
    "encoding.js",
    "url.js",
    "streams.js",
    "blob.js",
    "fetch.js",
    "server.js",
    "crypto.js",
    "platform.js",
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
        .unwrap_or_else(|e| panic!("web glue failed to parse for snapshotting: {e}"));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("web_glue.js"), &glue).unwrap();
    std::fs::write(out.join("web_glue.snap"), &snapshot).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
