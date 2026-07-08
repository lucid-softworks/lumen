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

const EXTENSIONS: [&str; 5] = [".mjs", ".js", ".jsx", ".json", ".cjs"];

/// Build the loader closure `eval_module` wants. It owns everything (`'static`); the engine
/// caches results by the canonical key we return, so returning a stable realpath per file is
/// what dedupes shared dependencies.
pub fn make_loader(
    builtins: BuiltinModules,
) -> impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> {
    move |specifier, referrer, attr_type| resolve(specifier, referrer, &builtins, attr_type)
}

fn resolve(
    specifier: &str,
    referrer: &str,
    builtins: &BuiltinModules,
    attr_type: Option<&str>,
) -> Option<(String, String)> {
    // Builtins: `node:fs` or a bare `fs`/`path`/… name.
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    if let Some(src) = builtins.0.get(&format!("node:{bare}")) {
        return Some((format!("node:{bare}"), src.clone()));
    }

    // A dynamic import's referrer is `import.meta.url`, a `file://` URL — reduce it (and a
    // `file://` specifier) to a plain path for the filesystem resolver.
    let referrer = referrer.strip_prefix("file://").unwrap_or(referrer);
    let specifier = specifier.strip_prefix("file://").unwrap_or(specifier);

    // A `with { type: "json" | "text" | "bytes" }` import wants the file's RAW contents — the
    // engine synthesizes the wrapper module itself. No CJS/ESM classification, no JSX transform:
    // the attribute defines the module type, whatever the extension says (importing a `.js` file
    // as text is a spec-tested case).
    if matches!(attr_type, Some("json" | "text" | "bytes")) {
        let file = if specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
        {
            let base = Path::new(referrer).parent()?.join(specifier);
            resolve_file_or_dir(&normalize(&base))?
        } else {
            resolve_node_modules(specifier, Path::new(referrer).parent()?)?.0
        };
        let key = std::fs::canonicalize(&file)
            .unwrap_or_else(|_| file.to_path_buf())
            .to_string_lossy()
            .into_owned();
        return Some((key, read_raw(&file, attr_type)?));
    }

    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        let base = Path::new(referrer).parent()?.join(specifier);
        let file = resolve_file_or_dir(&normalize(&base))?;
        // A relative `.js` file is CommonJS unless its nearest package.json is `type: module` —
        // Node's own rule. (A misresolved-as-ESM CJS file would expose no named exports.)
        let cjs = !file_is_esm(&file);
        return load_as_module(&file, cjs);
    }

    // Bare package name.
    let from = Path::new(referrer).parent()?;
    let (file, is_esm_pkg) = resolve_node_modules(specifier, from)?;
    load_as_module(&file, !is_esm_pkg)
}

/// Escape `text` as a JavaScript double-quoted string literal (for synthesized module source).
fn js_string_literal(text: &str) -> String {
    let mut lit = String::with_capacity(text.len() + 2);
    lit.push('"');
    for c in text.chars() {
        match c {
            '"' => lit.push_str("\\\""),
            '\\' => lit.push_str("\\\\"),
            '\n' => lit.push_str("\\n"),
            '\r' => lit.push_str("\\r"),
            '\u{2028}' => lit.push_str("\\u2028"),
            '\u{2029}' => lit.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                lit.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => lit.push(c),
        }
    }
    lit.push('"');
    lit
}

/// Read a file for an attribute import. `text`/`json` decode as UTF-8 the way the web platform's
/// "UTF-8 decode" does — invalid sequences become U+FFFD and a leading BOM is stripped. `bytes`
/// must round-trip exactly, so non-UTF-8 content is latin-1-decoded (one char per byte; the
/// engine re-extracts the original bytes when it builds the `Uint8Array`).
fn read_raw(file: &Path, attr_type: Option<&str>) -> Option<String> {
    let bytes = std::fs::read(file).ok()?;
    Some(match attr_type {
        Some("bytes") => match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
        },
        _ => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text)
        }
    })
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
        let target = d.join("node_modules").join(name);
        if target.is_dir() {
            if let Some(f) = resolve_directory(&target) {
                // The resolved file's own nearest package.json decides ESM-ness — a package can
                // ship an ESM build under `dist/es/` with its own `{"type":"module"}`.
                let esm = file_is_esm(&f);
                return Some((f, esm));
            }
        }
        // A bare specifier can also point straight at a file (`pkg/sub.js`).
        if let Some(f) = resolve_file(&target) {
            let esm = file_is_esm(&f);
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
            // Route through JSON.parse rather than embedding the text as an expression: an
            // object literal would give `"__proto__"` keys prototype-SETTING semantics (a
            // pollution vector), where JSON.parse creates a plain own data property — the same
            // semantics as a `with { type: "json" }` import.
            let text = std::fs::read_to_string(file).ok()?;
            Some((key, format!("export default JSON.parse({});", js_string_literal(&text))))
        }
        // `.mjs` is always ESM regardless of package type; `.cjs` is always CommonJS.
        "mjs" => Some((key.clone(), std::fs::read_to_string(file).ok()?)),
        // `.jsx` is JSX-over-ESM: transpile to plain JS, then load as a module.
        "jsx" => {
            let text = std::fs::read_to_string(file).ok()?;
            let js = crate::jsx::transform(&text).unwrap_or(text);
            Some((key, js))
        }
        "cjs" => Some((key.clone(), cjs_wrapper(&key, source_of(file)))),
        _ if cjs_default => Some((key.clone(), cjs_wrapper(&key, source_of(file)))),
        _ => {
            let text = std::fs::read_to_string(file).ok()?;
            Some((key, text))
        }
    }
}

/// Whether a resolved file loads as ESM: `.mjs` always, `.cjs` never, and `.js` per the nearest
/// enclosing `package.json` `"type"` (absent ⇒ CommonJS, Node's default).
fn file_is_esm(file: &Path) -> bool {
    match file.extension().and_then(|e| e.to_str()) {
        Some("mjs") => true,
        Some("cjs") => false,
        _ => {
            let mut dir = file.parent();
            while let Some(d) = dir {
                if d.join("package.json").is_file() {
                    return package_is_esm(d);
                }
                dir = d.parent();
            }
            false
        }
    }
}

/// Read a file's source (empty on failure — the wrapper still yields a working default export).
fn source_of(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap_or_default()
}

/// A synthetic ESM module bridging a CommonJS file: `require(path)`'s result is the default
/// export, and each export name statically discovered in the source becomes a live named export.
/// This is the cjs-module-lexer interop that lets `import { x } from './cjs-file'` link.
fn cjs_wrapper(abs_path: &str, source: String) -> String {
    let mut out = format!(
        "const __m = globalThis.require({});\nexport default __m;\n",
        js_string(abs_path)
    );
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_cjs_exports(&source, Path::new(abs_path), 0, &mut names, &mut seen);
    for name in names {
        // `export const NAME = __m["NAME"];` — a live binding onto the CJS export (undefined if
        // the static scan over-approximated, which is harmless).
        out.push_str(&format!(
            "export const {name} = __m[{}];\n",
            js_string(&name)
        ));
    }
    out
}

/// Discover a CJS module's export names, following `module.exports = require('./x')` /
/// `Object.assign(module.exports, require('./x'))` re-exports transitively (bounded depth) — the
/// pattern that indirection files like `react-dom/server.js` use. `file` is the module being
/// scanned, so relative re-export targets can be resolved and read.
fn collect_cjs_exports(
    src: &str,
    file: &Path,
    depth: u32,
    names: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    for name in scan_cjs_exports(src) {
        add_export(names, seen, &name);
    }
    if depth >= 4 {
        return; // guard against cycles / pathological chains
    }
    for spec in reexport_requires(src) {
        if !spec.starts_with('.') {
            continue; // only follow relative re-exports (a bare package is its own module)
        }
        if let Some(target) = file.parent().and_then(|d| resolve_relative_cjs(&d.join(&spec))) {
            if let Ok(sub) = std::fs::read_to_string(&target) {
                collect_cjs_exports(&sub, &target, depth + 1, names, seen);
            }
        }
    }
}

/// Resolve a relative `require` target to a file (exact, then `.js`/`.cjs`/`.json` *appended*,
/// then `index.js`), for re-export following. Extensions are appended, not substituted, so
/// `require('./server.node')` resolves to `server.node.js` rather than `server.js`.
fn resolve_relative_cjs(base: &Path) -> Option<PathBuf> {
    let base = normalize(base);
    if base.is_file() {
        return Some(base);
    }
    for ext in [".js", ".cjs", ".json"] {
        let mut s = base.as_os_str().to_os_string();
        s.push(ext);
        let cand = PathBuf::from(s);
        if cand.is_file() {
            return Some(cand);
        }
    }
    let index = base.join("index.js");
    index.is_file().then_some(index)
}

/// The relative specifiers a module re-exports wholesale: `module.exports = require('X')` and
/// `Object.assign(module.exports, require('X'))`.
fn reexport_requires(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in src.match_indices("require(") {
        // Look back for a `module.exports =` or `Object.assign(module.exports,` just before.
        let before = src[..i].trim_end();
        let is_reexport = before.ends_with("module.exports =")
            || before.ends_with("module.exports=")
            || before.ends_with("Object.assign(module.exports,")
            || before.ends_with("Object.assign(exports,");
        if !is_reexport {
            continue;
        }
        let after = &src[i + "require(".len()..];
        if let Some(spec) = leading_string_literal(after.trim_start()) {
            out.push(spec);
        }
    }
    out
}

/// Statically discover a CommonJS module's export names — the cjs-module-lexer heuristic. Handles
/// the two dominant shapes: direct assignment (`exports.X =` / `module.exports.X =`) and the
/// `Object.defineProperty(exports, "X", …)` that transpiled ESM emits. A miss just means a name
/// isn't re-exported (same as before); a false positive is a harmless `undefined` export.
fn scan_cjs_exports(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // `exports.NAME =` and `module.exports.NAME =` (the `module.` prefix is subsumed).
    for (i, _) in src.match_indices("exports.") {
        let start = i + "exports.".len();
        let name = read_ident(bytes, start);
        if !name.is_empty() && next_is_assignment(bytes, start + name.len()) {
            add_export(&mut names, &mut seen, &name);
        }
    }

    // `Object.defineProperty(exports, "NAME", …)` — the transpiler pattern.
    for (i, _) in src.match_indices("defineProperty(") {
        let rest = &src[i + "defineProperty(".len()..];
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("module.exports")
            .or_else(|| rest.strip_prefix("exports"));
        if let Some(rest) = rest {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(',') {
                if let Some(name) = leading_string_literal(rest.trim_start()) {
                    add_export(&mut names, &mut seen, &name);
                }
            }
        }
    }

    // `module.exports = { a, b: … }` — including the `0 && (module.exports = { … })` hint that
    // bundlers (esbuild, tsc) emit specifically for CJS export lexers.
    for (i, _) in src.match_indices("module.exports") {
        let rest = src[i + "module.exports".len()..].trim_start();
        if let Some(rest) = rest.strip_prefix('=') {
            if let Some(rest) = rest.trim_start().strip_prefix('{') {
                for key in object_key_list(rest) {
                    add_export(&mut names, &mut seen, &key);
                }
            }
        }
    }

    // `__export(target, { name: () => name, … })` — the esbuild/tsc re-export helper.
    for (i, _) in src.match_indices("__export(") {
        let rest = &src[i + "__export(".len()..];
        if let Some(comma) = rest.find(',') {
            if let Some(obj) = rest[comma + 1..].trim_start().strip_prefix('{') {
                for key in object_key_list(obj) {
                    add_export(&mut names, &mut seen, &key);
                }
            }
        }
    }

    names
}

/// The top-level keys of an object literal, given the text just past its opening `{`. Handles
/// shorthand (`{ a, b }`), `key:` pairs, string keys, and nested braces/brackets/parens/strings.
fn object_key_list(after_brace: &str) -> Vec<String> {
    let b = after_brace.as_bytes();
    let mut keys = Vec::new();
    let mut depth = 1usize; // already inside the outer `{`
    let mut expect_key = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'{' | b'[' | b'(' => {
                depth += 1;
                expect_key = false;
                i += 1;
            }
            b'}' | b']' | b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            b'"' | b'\'' | b'`' => {
                let quote = c;
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != quote {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                if expect_key && depth == 1 {
                    keys.push(String::from_utf8_lossy(&b[start..j.min(b.len())]).into_owned());
                    expect_key = false;
                }
                i = j + 1;
            }
            b',' if depth == 1 => {
                expect_key = true;
                i += 1;
            }
            b':' if depth == 1 => {
                expect_key = false;
                i += 1;
            }
            _ => {
                if expect_key && depth == 1 && (c.is_ascii_alphabetic() || c == b'_' || c == b'$') {
                    let name = read_ident(b, i);
                    i += name.len();
                    keys.push(name);
                    expect_key = false;
                } else {
                    i += 1;
                }
            }
        }
    }
    keys
}

fn add_export(names: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, name: &str) {
    if is_export_ident(name) && seen.insert(name.to_string()) {
        names.push(name.to_string());
    }
}

/// Read a JS identifier starting at `pos`.
fn read_ident(bytes: &[u8], pos: usize) -> String {
    let mut end = pos;
    while end < bytes.len() {
        let c = bytes[end];
        let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
        if !ok {
            break;
        }
        end += 1;
    }
    String::from_utf8_lossy(&bytes[pos..end]).into_owned()
}

/// Whether the next non-space token at `pos` is a plain `=` (assignment) rather than `==`/`=>`.
fn next_is_assignment(bytes: &[u8], mut pos: usize) -> bool {
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    pos < bytes.len()
        && bytes[pos] == b'='
        && bytes.get(pos + 1) != Some(&b'=')
        && bytes.get(pos + 1) != Some(&b'>')
}

/// The contents of a leading `"…"`/`'…'` string literal (no escapes handled — export names are
/// plain identifiers in practice).
fn leading_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &s[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

/// A valid, non-reserved identifier usable as an `export const` name (excludes `default` and
/// the transpiler marker `__esModule`).
fn is_export_ident(name: &str) -> bool {
    if name.is_empty() || name == "default" || name == "__esModule" {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    !is_reserved_word(name)
}

/// ES reserved words that cannot be `export const` binding names.
fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "break" | "case" | "catch" | "class" | "const" | "continue" | "debugger" | "default"
            | "delete" | "do" | "else" | "enum" | "export" | "extends" | "false" | "finally"
            | "for" | "function" | "if" | "import" | "in" | "instanceof" | "new" | "null"
            | "return" | "super" | "switch" | "this" | "throw" | "true" | "try" | "typeof"
            | "var" | "void" | "while" | "with" | "yield" | "let" | "static" | "await"
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
