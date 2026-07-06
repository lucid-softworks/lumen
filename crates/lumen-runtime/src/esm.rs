//! The ES-module loader for the runtime: resolves an `import` specifier to a canonical key +
//! source, which the engine's `eval_module` consults for every dependency. The engine already
//! runs the module graph (linking, top-level await); this is just resolution + the
//! CommonJS/builtin interop bridge.
//!
//! Resolution:
//! - `node:x` or a bare builtin name -> a synthetic re-export module (precomputed in JS, so
//!   named imports like `import { readFileSync } from "node:fs"` work).
//! - relative/absolute -> a file on disk (`.mjs`/`.js`/`.json`/`.cjs`, directory index,
//!   `package.json` `main`). `.js`/`.mjs` load as real ESM; `.json` and `.cjs` get a synthetic
//!   default-export wrapper (`.cjs` bridges through the global CommonJS `require`).
//! - bare package -> the `node_modules` walk; an ESM entry (`.mjs`, or `package.json`
//!   `type:module` / `exports` import condition / `module`) loads as real ESM, else it's
//!   default-only CJS interop via `require`.
//!
//! Deferred (documented, not silently wrong): named imports from a CommonJS *package* (Node
//! uses source static-analysis we don't), subpath-pattern `exports`, import maps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Synthetic ESM source for each builtin (`"node:fs"` -> `export default …; export const …`),
/// generated in JS where the export names are known, and ferried here as plain strings.
pub struct BuiltinModules(pub HashMap<String, String>);

const EXTENSIONS: [&str; 4] = [".mjs", ".js", ".json", ".cjs"];

/// Build the loader closure `eval_module` wants. It owns everything (`'static`); the engine
/// caches results by the canonical key we return, so returning a stable realpath per file is
/// what dedupes shared dependencies.
pub fn make_loader(builtins: BuiltinModules) -> impl Fn(&str, &str) -> Option<(String, String)> {
    move |specifier, referrer| resolve(specifier, referrer, &builtins)
}

fn resolve(specifier: &str, referrer: &str, builtins: &BuiltinModules) -> Option<(String, String)> {
    // Builtins: `node:fs` or a bare `fs`/`path`/… name.
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    if let Some(src) = builtins.0.get(&format!("node:{bare}")) {
        return Some((format!("node:{bare}"), src.clone()));
    }

    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        let base = Path::new(referrer).parent()?.join(specifier);
        let file = resolve_file_or_dir(&normalize(&base))?;
        return load_as_module(&file, false);
    }

    // Bare package name.
    let from = Path::new(referrer).parent()?;
    let (file, is_esm_pkg) = resolve_node_modules(specifier, from)?;
    load_as_module(&file, !is_esm_pkg)
}

/// A resolved file (existing path, extension probe, or directory index/main). `None` if
/// nothing matches.
fn resolve_file_or_dir(base: &Path) -> Option<PathBuf> {
    if let Some(f) = resolve_file(base) {
        return Some(f);
    }
    if base.is_dir() {
        return resolve_directory(base);
    }
    None
}

fn resolve_file(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    for ext in EXTENSIONS {
        let mut s = base.as_os_str().to_os_string();
        s.push(ext);
        let candidate = PathBuf::from(s);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_directory(dir: &Path) -> Option<PathBuf> {
    let pkg = dir.join("package.json");
    if pkg.is_file() {
        if let Ok(text) = std::fs::read_to_string(&pkg) {
            if let Some(entry) = pkg_entry(&text) {
                let target = normalize(&dir.join(entry));
                if let Some(f) =
                    resolve_file(&target).or_else(|| resolve_file(&target.join("index")))
                {
                    return Some(f);
                }
            }
        }
    }
    resolve_file(&dir.join("index"))
}

/// The `node_modules` walk. Returns the resolved file and whether the package is ESM (so the
/// caller loads real ESM vs. a CJS default-export wrapper).
fn resolve_node_modules(name: &str, start: &Path) -> Option<(PathBuf, bool)> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.file_name().is_some_and(|n| n == "node_modules") {
            dir = d.parent();
            continue;
        }
        let pkg_dir = d.join("node_modules").join(name);
        if pkg_dir.is_dir() {
            let esm = package_is_esm(&pkg_dir);
            if let Some(f) = resolve_directory(&pkg_dir) {
                let esm = esm || f.extension().is_some_and(|e| e == "mjs");
                return Some((f, esm));
            }
        }
        // A bare specifier can also point straight at a file (`pkg/sub.js`).
        let direct = d.join("node_modules").join(name);
        if let Some(f) = resolve_file(&direct) {
            let esm = f.extension().is_some_and(|e| e == "mjs");
            return Some((f, esm));
        }
        dir = d.parent();
    }
    None
}

fn package_is_esm(pkg_dir: &Path) -> bool {
    std::fs::read_to_string(pkg_dir.join("package.json"))
        .ok()
        .is_some_and(|t| json_string_field(&t, "type").as_deref() == Some("module"))
}

/// Turn a resolved file into `(canonical_key, source)`. `.mjs`/`.js` are real ESM; `.json`
/// and (when `cjs_default`) `.cjs`/CJS packages get a synthetic default-export wrapper.
fn load_as_module(file: &Path, cjs_default: bool) -> Option<(String, String)> {
    let key = std::fs::canonicalize(file)
        .unwrap_or_else(|_| file.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => {
            let text = std::fs::read_to_string(file).ok()?;
            Some((key, format!("export default {text};")))
        }
        "cjs" => Some((key.clone(), cjs_wrapper(&key))),
        _ if cjs_default => Some((key.clone(), cjs_wrapper(&key))),
        _ => {
            let text = std::fs::read_to_string(file).ok()?;
            Some((key, text))
        }
    }
}

/// A synthetic ESM module whose default export is `require(path)`'s result — the standard
/// CJS-in-ESM interop (default only; named CJS exports aren't statically analyzed).
fn cjs_wrapper(abs_path: &str) -> String {
    format!(
        "const __m = globalThis.require({});\nexport default __m;\n",
        js_string(abs_path)
    )
}

/// `package.json` `exports` (import/default condition) or `main`.
fn pkg_entry(pkg_json: &str) -> Option<String> {
    if let Some(exports) = json_string_field(pkg_json, "module") {
        return Some(exports);
    }
    json_string_field(pkg_json, "main")
}

/// Lexically resolve `.`/`..` without touching the filesystem (the target may not exist yet).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// A minimal JSON scan for a top-level `"field": "value"` string — enough for the one or two
/// `package.json` keys we read, without a JSON dependency (the workspace is zero-dep).
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut rest = &json[json.find(&needle)? + needle.len()..];
    rest = rest.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            other => out.push(other),
        }
    }
    None
}

/// A JSON/JS string literal (for embedding an absolute path in generated source).
fn js_string(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_field_scan() {
        assert_eq!(
            json_string_field(r#"{"type":"module"}"#, "type").as_deref(),
            Some("module")
        );
        assert_eq!(
            json_string_field(r#"{ "main" : "lib/i.js" }"#, "main").as_deref(),
            Some("lib/i.js")
        );
        assert_eq!(json_string_field(r#"{"a":1}"#, "type"), None);
    }

    #[test]
    fn normalize_dotdot() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn js_string_escapes() {
        assert_eq!(js_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
