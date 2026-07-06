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
//!   default-only CJS interop via `require`. A bare subpath (`hono/logger`) resolves as a
//!   literal file first, then via the package's `exports` map (`"./logger"` -> its
//!   `import`/`default` target).
//!
//! Deferred (documented, not silently wrong): named imports from a CommonJS *package* (Node
//! uses source static-analysis we don't), subpath-*pattern* `exports` (`"./*"`), import maps.

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
        // `type:module` lives on the package root, so a bare subpath (`pkg/sub.js`) inherits it
        // too rather than defaulting to CJS.
        let esm_pkg = package_is_esm(&package_dir(d, name));
        let target = d.join("node_modules").join(name);
        if target.is_dir() {
            if let Some(f) = resolve_directory(&target) {
                let esm = esm_pkg || f.extension().is_some_and(|e| e == "mjs");
                return Some((f, esm));
            }
        }
        // A bare specifier can also point straight at a file (`pkg/sub.js`).
        if let Some(f) = resolve_file(&target) {
            let esm = esm_pkg || f.extension().is_some_and(|e| e == "mjs");
            return Some((f, esm));
        }
        // A subpath that isn't a literal file may be mapped by the package's `exports`
        // (`hono/logger` -> `"./logger": { "import": "./dist/middleware/logger/index.js" }`).
        // Only the common non-pattern case; pattern subpaths (`"./*"`) stay deferred.
        if let Some(subpath) = package_subpath(name) {
            let pkg_dir = package_dir(d, name);
            if let Some(entry) = std::fs::read_to_string(pkg_dir.join("package.json"))
                .ok()
                .and_then(|t| exports_subpath(&t, &subpath))
            {
                let mapped = normalize(&pkg_dir.join(&entry));
                if let Some(f) =
                    resolve_file(&mapped).or_else(|| resolve_file(&mapped.join("index")))
                {
                    // Resolved through the `import`/`default` condition, which names the ES
                    // module build regardless of the file's extension.
                    return Some((f, true));
                }
            }
        }
        dir = d.parent();
    }
    None
}

/// The package root for a bare specifier under `<parent>/node_modules`: the first path segment,
/// or the first two for a scoped `@scope/name`.
fn package_dir(parent: &Path, name: &str) -> PathBuf {
    let mut parts = name.split('/');
    let mut pkg = String::new();
    if let Some(first) = parts.next() {
        pkg.push_str(first);
        if first.starts_with('@') {
            if let Some(scope_name) = parts.next() {
                pkg.push('/');
                pkg.push_str(scope_name);
            }
        }
    }
    parent.join("node_modules").join(pkg)
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

/// The ESM entry from `package.json`: the `exports` `import`/`default` condition, else the
/// `module` field, else `main`. A zero-dep scan of a practical `exports` subset — a bare string,
/// or the first `import`/`default` string condition (covers the common `"."` object shape). Real
/// subpath-pattern `exports` are still deferred.
fn pkg_entry(pkg_json: &str) -> Option<String> {
    if let Some(entry) = exports_entry(pkg_json) {
        return Some(entry);
    }
    if let Some(module) = json_string_field(pkg_json, "module") {
        return Some(module);
    }
    json_string_field(pkg_json, "main")
}

/// The `exports` field's ESM target: a bare `"exports": "./x.js"` string, or the first
/// `import`/`default` string condition inside its object form.
fn exports_entry(pkg_json: &str) -> Option<String> {
    let start = pkg_json.find("\"exports\"")?;
    let after = pkg_json[start + "\"exports\"".len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    if after.starts_with('"') {
        return parse_string(after);
    }
    // Object form: prefer the `import` condition, then `default`.
    ["import", "default"]
        .into_iter()
        .find_map(|cond| json_string_field(after, cond))
}

/// The subpath of a bare specifier, if any: `hono/logger` -> `logger`, `@sc/pkg/a/b` -> `a/b`,
/// and `hono` / `@sc/pkg` (bare package roots) -> `None`.
fn package_subpath(name: &str) -> Option<String> {
    let mut parts = if name.starts_with('@') {
        name.splitn(3, '/')
    } else {
        name.splitn(2, '/')
    };
    parts.next()?; // package (or scope)
    if name.starts_with('@') {
        parts.next()?; // scoped name
    }
    parts.next().map(str::to_string)
}

/// The `exports` target for one subpath key (`"./logger"`): its bare string, or the first
/// `import`/`default` string condition of its object form. Non-pattern keys only.
fn exports_subpath(pkg_json: &str, subpath: &str) -> Option<String> {
    let start = pkg_json.find("\"exports\"")?;
    let region = &pkg_json[start..];
    let key = format!("\"./{subpath}\"");
    let after = region[region.find(&key)? + key.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    if after.starts_with('"') {
        return parse_string(after);
    }
    ["import", "default"]
        .into_iter()
        .find_map(|cond| json_string_field(after, cond))
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

/// A minimal JSON scan for a `"field": "value"` string — enough for the few `package.json` keys
/// we read, without a JSON dependency (the workspace is zero-dep). Matches the field as a *key*
/// (a `"field"` followed by `:`), so it skips occurrences of the same text used as a value — e.g.
/// the `"module"` in `"type": "module"` is not mistaken for a `"module"` key.
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut from = 0;
    while let Some(pos) = json[from..].find(&needle) {
        let after = json[from + pos + needle.len()..].trim_start();
        if let Some(value) = after.strip_prefix(':') {
            // A key: return its string value (`None` if the value isn't a string).
            return parse_string(value);
        }
        // Matched the text as a value or substring; keep looking for the real key.
        from += pos + needle.len();
    }
    None
}

/// Parse a JSON string literal from `s` (leading whitespace allowed, then the opening `"`).
fn parse_string(s: &str) -> Option<String> {
    let rest = s.trim_start().strip_prefix('"')?;
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
    fn json_field_matches_key_not_value() {
        // `"module"` appears first as the *value* of `"type"`, then as a real key. The scan must
        // return the key's value, not choke on the value occurrence (the hono package.json shape).
        let pkg = r#"{ "main": "dist/cjs/index.js", "type": "module", "module": "dist/index.js" }"#;
        assert_eq!(json_string_field(pkg, "module").as_deref(), Some("dist/index.js"));
        assert_eq!(json_string_field(pkg, "type").as_deref(), Some("module"));
    }

    #[test]
    fn pkg_entry_prefers_exports_import_condition() {
        // hono-shaped: `main` is CJS, but the ESM entry lives under exports' `import` condition.
        let pkg = r#"{
            "main": "dist/cjs/index.js",
            "type": "module",
            "module": "dist/index.js",
            "exports": { ".": {
                "types": "./dist/types/index.d.ts",
                "import": "./dist/index.js",
                "require": "./dist/cjs/index.js"
            } }
        }"#;
        assert_eq!(pkg_entry(pkg).as_deref(), Some("./dist/index.js"));
    }

    #[test]
    fn pkg_entry_exports_string_then_module_then_main() {
        assert_eq!(
            pkg_entry(r#"{ "exports": "./e.js", "main": "./m.js" }"#).as_deref(),
            Some("./e.js")
        );
        assert_eq!(
            pkg_entry(r#"{ "module": "./mod.js", "main": "./m.js" }"#).as_deref(),
            Some("./mod.js")
        );
        assert_eq!(pkg_entry(r#"{ "main": "./m.js" }"#).as_deref(), Some("./m.js"));
    }

    #[test]
    fn package_subpath_splits_plain_and_scoped() {
        assert_eq!(package_subpath("hono"), None);
        assert_eq!(package_subpath("hono/logger").as_deref(), Some("logger"));
        assert_eq!(package_subpath("hono/dist/x.js").as_deref(), Some("dist/x.js"));
        assert_eq!(package_subpath("@scope/pkg"), None);
        assert_eq!(package_subpath("@scope/pkg/sub").as_deref(), Some("sub"));
    }

    #[test]
    fn exports_subpath_reads_condition_and_string_forms() {
        // hono-shaped middleware subpath: an object with an `import` condition.
        let pkg = r#"{ "exports": {
            ".": { "import": "./dist/index.js" },
            "./logger": {
                "types": "./dist/types/middleware/logger/index.d.ts",
                "import": "./dist/middleware/logger/index.js",
                "require": "./dist/cjs/middleware/logger/index.js"
            }
        } }"#;
        assert_eq!(
            exports_subpath(pkg, "logger").as_deref(),
            Some("./dist/middleware/logger/index.js")
        );
        assert_eq!(exports_subpath(pkg, "cors"), None);
        // Bare-string subpath form.
        assert_eq!(
            exports_subpath(r#"{ "exports": { "./x": "./lib/x.js" } }"#, "x").as_deref(),
            Some("./lib/x.js")
        );
    }

    #[test]
    fn package_dir_handles_plain_and_scoped_subpaths() {
        let root = Path::new("/app");
        assert_eq!(package_dir(root, "hono"), PathBuf::from("/app/node_modules/hono"));
        assert_eq!(
            package_dir(root, "hono/dist/index.js"),
            PathBuf::from("/app/node_modules/hono")
        );
        assert_eq!(
            package_dir(root, "@scope/pkg/sub.js"),
            PathBuf::from("/app/node_modules/@scope/pkg")
        );
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
