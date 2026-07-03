//! ES module loading, linking, and evaluation.
//!
//! lumen links modules in two passes, mirroring the spec's `Link` (Instantiate) and `Evaluate`
//! phases:
//!
//!   * **Instantiate** parses every reachable module, builds its export tables, creates its scope
//!     and (frozen) namespace object, hoists its top-level bindings, and wires each import to a
//!     *live* binding cell in the exporting module's scope. No user code runs.
//!   * **Evaluate** runs module bodies depth-first (dependencies before dependents, each once),
//!     so a live import observes the exporter's latest value even across circular dependencies.
//!
//! Specifier resolution + source fetching is delegated to a host loader (`Interp::module_loader`)
//! so the engine stays filesystem-agnostic.

use crate::ast::*;
use crate::interpreter::{new_scope, Abrupt, Binding, Env, Interp};
use crate::value::{Object, Property, Value};
use std::collections::HashMap;
use std::rc::Rc;

/// How a module-namespace property reads its current value.
#[derive(Clone)]
pub enum NsBinding {
    /// A live binding: read `local` from the exporting module's scope each time.
    Live(Env, String),
    /// A stable value (a star-as namespace re-export).
    Static(Value),
}

/// A parsed + linked module: its body, scope, namespace object, resolved export tables, and
/// evaluation status. Keyed by canonical specifier in `Interp::module_recs`.
pub(crate) struct ModuleRec {
    body: Rc<Vec<Stmt>>,
    env: Env,
    ns: Value,
    meta: Value,
    /// Dependency keys in source order (drives depth-first evaluation).
    dep_keys: Vec<String>,
    /// Specifier → resolved canonical key, for this module's own import/export-from clauses.
    resolved: HashMap<String, String>,
    /// `export name → local name` for names declared/defined in this module.
    local_exports: HashMap<String, String>,
    /// Local names that are themselves imports, so a re-export resolves through to the origin
    /// binding (spec: `import * as x; export {x}` resolves to the imported module's namespace).
    imports: HashMap<String, ImportOrigin>,
    /// `export name → (dependency key, imported name)` for `export { x } from 'dep'`.
    indirect: HashMap<String, (String, String)>,
    /// `export name → dependency key` for `export * as name from 'dep'`.
    star_as: HashMap<String, String>,
    /// Dependency keys of `export * from 'dep'` clauses.
    stars: Vec<String>,
    linked: bool,
    evaluated: bool,
    evaluating: bool,
    eval_error: Option<Value>,
    /// Dependency keys imported *only* via `import defer` — skipped during this module's
    /// evaluation phase (they evaluate on first namespace access instead).
    deferred_deps: Vec<String>,
}

/// The origin of a local name that is an import binding (so re-exports resolve to the source).
enum ImportOrigin {
    /// `import * as x from 'dep'` — resolves to `dep`'s namespace object.
    Namespace(String),
    /// `import { y as x } from 'dep'` / `import x from 'dep'` — resolves to `dep`'s export `y`.
    Named(String, String),
}

/// The result of resolving an export name (spec ResolveExport).
enum Resolution {
    /// A concrete binding: `local` in the given module scope.
    Local(Env, String),
    /// A namespace object (a star-as re-export target).
    Ns(Value),
    /// Two star re-exports provide the name with different bindings.
    Ambiguous,
    /// The name is not exported.
    NotFound,
}

impl Interp {
    /// Load, link, and evaluate the module identified by canonical `key` (with initial `src`),
    /// returning its namespace object.
    pub(crate) fn load_module(&mut self, key: &str, src: &str) -> Result<Value, Abrupt> {
        // Phase 1: parse the whole graph (so every module's export tables exist before any linking).
        self.parse_and_register(key, Some(src.to_string()))?;
        // Phase 2: link (hoist bindings, wire live imports, build namespaces) depth-first.
        self.link_module(key)?;
        // Phase 3: evaluate module bodies depth-first.
        self.evaluate_module(key)?;
        Ok(self.module_recs[key].ns.clone())
    }

    // --- Parse phase --------------------------------------------------------------------------

    /// Parse `key` and, transitively, every dependency — registering each module's environment,
    /// namespace object, and export tables. No user code runs and no linking happens yet, so a later
    /// ResolveExport can see the whole graph. Idempotent per key.
    fn parse_and_register(&mut self, key: &str, src: Option<String>) -> Result<(), Abrupt> {
        if self.module_recs.contains_key(key) {
            return Ok(());
        }
        let src = match src {
            Some(s) => s,
            None => return Err(self.throw("TypeError", format!("module not found: {key}"))),
        };
        let body =
            crate::parser::parse_module(&src).map_err(|e| self.throw("SyntaxError", e.message))?;
        let body = Rc::new(body);

        // Resolve every dependency specifier to a canonical key up front (fetching its source), so
        // the export tables can name dependencies by key. Duplicate specifiers resolve once.
        let mut resolved: HashMap<String, String> = HashMap::new();
        let mut dep_keys: Vec<String> = Vec::new();
        let mut dep_srcs: Vec<(String, String)> = Vec::new();
        // A dependency evaluates lazily only if *every* clause naming it is an `import defer`.
        let mut defer_specs: HashMap<String, bool> = HashMap::new();
        for stmt in body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    decl.source.to_string(),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (src.to_string(), false),
                _ => continue,
            };
            let e = defer_specs.entry(spec).or_insert(true);
            *e = *e && defers;
        }
        for (spec, attr_type) in module_dependency_specifiers(&body) {
            if resolved.contains_key(&spec) {
                continue;
            }
            let (canon, dsrc) = self.fetch_module(&spec, key)?;
            // A `with { type: ... }` dependency synthesizes a wrapper module (JSON/text/bytes) —
            // keyed separately from any ordinary module of the same file.
            let canon = match attr_type.as_deref() {
                Some(t @ ("json" | "text" | "bytes")) => format!("{canon}#{t}"),
                _ => canon,
            };
            let dsrc = typed_module_source(dsrc, attr_type.as_deref());
            resolved.insert(spec.clone(), canon.clone());
            if !dep_keys.contains(&canon) {
                dep_keys.push(canon.clone());
                if !self.module_recs.contains_key(&canon) {
                    dep_srcs.push((canon, dsrc));
                }
            }
        }

        // Create the scope + namespace object, then build the export tables. This record must exist
        // before we recurse into dependencies so an import cycle resolves back to it.
        let env = new_scope(Some(self.global_env.clone()));
        // Top-level `this` in a module is `undefined` (not the global object).
        env.borrow_mut().vars.insert(
            "this".to_string(),
            Binding::data(Value::Undefined, false, true),
        );
        let ns_obj = Object::new(None);
        let ns = Value::Obj(ns_obj.clone());
        self.modules.insert(key.to_string(), ns.clone());
        let meta = self.build_import_meta(key);

        let tables = build_export_tables(&body, &resolved);
        let deferred_deps: Vec<String> = resolved
            .iter()
            .filter(|(spec, _)| defer_specs.get(*spec).copied().unwrap_or(false))
            .map(|(_, canon)| canon.clone())
            .collect();
        self.module_recs.insert(
            key.to_string(),
            ModuleRec {
                body: body.clone(),
                env: env.clone(),
                ns,
                meta,
                dep_keys,
                resolved,
                local_exports: tables.local_exports,
                imports: tables.imports,
                indirect: tables.indirect,
                star_as: tables.star_as,
                stars: tables.stars,
                linked: false,
                evaluated: false,
                evaluating: false,
                eval_error: None,
                deferred_deps,
            },
        );

        // Recurse: parse each dependency (fetched during specifier resolution above).
        for (canon, dsrc) in dep_srcs {
            self.parse_and_register(&canon, Some(dsrc))?;
        }
        Ok(())
    }

    // --- Link phase ---------------------------------------------------------------------------

    /// Link `key` and its dependencies depth-first (once each): instantiate top-level bindings
    /// (functions initialized, lexicals in their temporal dead zone), wire imports to live cells,
    /// validate indirect exports, and build the namespace object.
    fn link_module(&mut self, key: &str) -> Result<(), Abrupt> {
        if self.module_recs[key].linked {
            return Ok(());
        }
        self.module_recs.get_mut(key).unwrap().linked = true;

        let (body, env, ns) = {
            let rec = &self.module_recs[key];
            (rec.body.clone(), rec.env.clone(), rec.ns.clone())
        };
        let dep_keys = self.module_recs[key].dep_keys.clone();
        for dep in &dep_keys {
            self.link_module(dep)?;
        }

        self.hoist(&body, &env, &[]);
        self.declare_block_lexicals(&body, &env, false);
        self.declare_default_placeholder(&body, &env);
        self.validate_indirect_exports(key)?;
        self.link_imports(key, &body, &env)?;
        if let Value::Obj(ns_obj) = ns {
            self.build_namespace(key, &ns_obj)?;
        }
        Ok(())
    }

    /// Every `export { x } from 'dep'` entry must resolve to a single binding (spec: unresolvable or
    /// ambiguous indirect exports are a link-time SyntaxError).
    fn validate_indirect_exports(&mut self, key: &str) -> Result<(), Abrupt> {
        let names: Vec<String> = self.module_recs[key].indirect.keys().cloned().collect();
        for name in names {
            match self.resolve_export(key, &name, &mut Vec::new()) {
                Resolution::Local(..) | Resolution::Ns(..) => {}
                Resolution::Ambiguous => {
                    return Err(self.throw(
                        "SyntaxError",
                        format!("the requested module provides an ambiguous export named '{name}'"),
                    ))
                }
                Resolution::NotFound => {
                    return Err(self.throw(
                        "SyntaxError",
                        format!("the requested module does not provide an export named '{name}'"),
                    ))
                }
            }
        }
        Ok(())
    }

    /// Fetch a dependency's `(canonical_key, source)` via the host loader.
    fn fetch_module(
        &mut self,
        specifier: &str,
        referrer: &str,
    ) -> Result<(String, String), Abrupt> {
        let loader = match &self.module_loader {
            Some(l) => l.clone(),
            None => return Err(self.throw("TypeError", "no module loader configured")),
        };
        match loader(specifier, referrer) {
            Some(pair) => Ok(pair),
            None => Err(self.throw("TypeError", format!("module not found: {specifier}"))),
        }
    }

    /// Build a module's `import.meta` object (`{ url }`, extensible, no prototype).
    fn build_import_meta(&mut self, key: &str) -> Value {
        let meta = Object::new(None);
        meta.borrow_mut().props.insert(
            "url",
            Property::data(Value::from_string(key.to_string()), true, true, true),
        );
        Value::Obj(meta)
    }

    /// Declare an uninitialized `*default*` binding for `export default <expression>` (and anonymous
    /// default function/class), so an importer can wire a live binding to it during linking.
    fn declare_default_placeholder(&mut self, body: &[Stmt], env: &Env) {
        for stmt in body {
            let Stmt::ExportDefault(inner) = stmt else {
                continue;
            };
            // `*default*` gets an uninitialized (TDZ) binding for an `export default <expression>` or
            // anonymous `class`. A named function/class binds its own name; an anonymous function is a
            // hoistable declaration already bound (initialized) by `hoist`.
            let needs_tdz = matches!(&**inner, Stmt::Expr(_))
                || matches!(&**inner, Stmt::ClassDecl(c) if c.name.is_none());
            if needs_tdz {
                env.borrow_mut().vars.insert(
                    "*default*".to_string(),
                    Binding {
                        value: Value::Undefined,
                        mutable: false,
                        strict_immutable: true,
                        initialized: false,
                        import_ref: None,
                        deletable: false,
                    },
                );
            }
        }
    }

    /// Wire every `import` binding in `body` to a live cell (or namespace object) of its dependency.
    fn link_imports(&mut self, key: &str, body: &[Stmt], env: &Env) -> Result<(), Abrupt> {
        for stmt in body {
            let Stmt::Import(decl) = stmt else { continue };
            let dep = self.resolved_key(key, &decl.source);
            for spec in &decl.specs {
                match spec {
                    ImportSpec::Namespace(local) => {
                        let ns = self.module_recs[&dep].ns.clone();
                        self.bind_local(env, local, ns);
                    }
                    ImportSpec::DeferNamespace(local) => {
                        let dns = self.make_deferred_ns(&dep);
                        self.bind_local(env, local, dns);
                    }
                    ImportSpec::Default(local) => {
                        self.link_named(env, local, &dep, "default")?;
                    }
                    ImportSpec::Named { imported, local } => {
                        self.link_named(env, local, &dep, imported)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Wire `local` to the binding that `dep` exports as `name` (a link-time SyntaxError if the
    /// export is missing or ambiguous).
    fn link_named(&mut self, env: &Env, local: &str, dep: &str, name: &str) -> Result<(), Abrupt> {
        match self.resolve_export(dep, name, &mut Vec::new()) {
            Resolution::Local(src_env, src_local) => {
                env.borrow_mut().vars.insert(
                    local.to_string(),
                    Binding {
                        value: Value::Undefined,
                        mutable: false,
                        strict_immutable: true,
                        initialized: true,
                        import_ref: Some((src_env, src_local)),
                        deletable: false,
                    },
                );
                Ok(())
            }
            Resolution::Ns(ns) => {
                self.bind_local(env, local, ns);
                Ok(())
            }
            Resolution::Ambiguous => Err(self.throw(
                "SyntaxError",
                format!("the requested module provides an ambiguous export named '{name}'"),
            )),
            Resolution::NotFound => Err(self.throw(
                "SyntaxError",
                format!("the requested module does not provide an export named '{name}'"),
            )),
        }
    }

    fn bind_local(&self, env: &Env, local: &str, value: Value) {
        env.borrow_mut()
            .vars
            .insert(local.to_string(), Binding::data(value, false, true));
    }

    fn resolved_key(&self, referrer: &str, specifier: &str) -> String {
        self.module_recs[referrer]
            .resolved
            .get(specifier)
            .cloned()
            .unwrap_or_else(|| specifier.to_string())
    }

    // --- Export resolution (spec ResolveExport / GetExportedNames) -----------------------------

    /// Resolve `name` exported by module `key` to a concrete binding, following indirect and star
    /// re-exports. `seen` guards against cyclic re-export chains.
    fn resolve_export(
        &self,
        key: &str,
        name: &str,
        seen: &mut Vec<(String, String)>,
    ) -> Resolution {
        let pair = (key.to_string(), name.to_string());
        if seen.contains(&pair) {
            return Resolution::NotFound;
        }
        seen.push(pair);
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Resolution::NotFound,
        };
        if let Some(local) = rec.local_exports.get(name) {
            // A re-exported import resolves to its origin binding (so a namespace re-exported by two
            // paths compares equal, and a named re-export follows through to the real module).
            if let Some(origin) = rec.imports.get(local) {
                return match origin {
                    ImportOrigin::Namespace(dep) => match self.module_recs.get(dep) {
                        Some(d) => Resolution::Ns(d.ns.clone()),
                        None => Resolution::NotFound,
                    },
                    ImportOrigin::Named(dep, imported) => {
                        let (dep, imported) = (dep.clone(), imported.clone());
                        self.resolve_export(&dep, &imported, seen)
                    }
                };
            }
            return Resolution::Local(rec.env.clone(), local.clone());
        }
        if let Some((dep, imported)) = rec.indirect.get(name) {
            let (dep, imported) = (dep.clone(), imported.clone());
            return self.resolve_export(&dep, &imported, seen);
        }
        if let Some(dep) = rec.star_as.get(name) {
            if let Some(drec) = self.module_recs.get(dep) {
                return Resolution::Ns(drec.ns.clone());
            }
            return Resolution::NotFound;
        }
        if name == "default" {
            // `default` is never provided by a `export *` re-export.
            return Resolution::NotFound;
        }
        let stars = rec.stars.clone();
        let mut star_resolution = Resolution::NotFound;
        for dep in &stars {
            match self.resolve_export(dep, name, seen) {
                Resolution::Ambiguous => return Resolution::Ambiguous,
                Resolution::NotFound => {}
                r => match &star_resolution {
                    Resolution::NotFound => star_resolution = r,
                    existing => {
                        if !same_binding(existing, &r) {
                            return Resolution::Ambiguous;
                        }
                    }
                },
            }
        }
        star_resolution
    }

    /// All names module `key` exports (spec GetExportedNames), excluding `default` from stars.
    fn exported_names(&self, key: &str, seen: &mut Vec<String>) -> Vec<String> {
        if seen.contains(&key.to_string()) {
            return Vec::new();
        }
        seen.push(key.to_string());
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let mut names: Vec<String> = Vec::new();
        let push = |n: &str, names: &mut Vec<String>| {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        };
        for n in rec.local_exports.keys() {
            push(n, &mut names);
        }
        for n in rec.indirect.keys() {
            push(n, &mut names);
        }
        for n in rec.star_as.keys() {
            push(n, &mut names);
        }
        let stars = rec.stars.clone();
        for dep in &stars {
            for n in self.exported_names(dep, seen) {
                if n != "default" {
                    push(&n, &mut names);
                }
            }
        }
        names
    }

    /// Populate `ns` with one entry per unambiguously-resolvable export, sorted by name, and record
    /// how each reads its live value. Namespace objects are frozen and prototype-less.
    fn build_namespace(&mut self, key: &str, ns: &crate::value::Gc) -> Result<(), Abrupt> {
        let mut names = self.exported_names(key, &mut Vec::new());
        names.sort();
        let mut live: HashMap<String, NsBinding> = HashMap::new();
        for name in &names {
            match self.resolve_export(key, name, &mut Vec::new()) {
                Resolution::Local(env, local) => {
                    live.insert(name.clone(), NsBinding::Live(env, local));
                    // A placeholder own property makes the name enumerable / own-key-visible; reads
                    // go through the live map (see `get_member`).
                    ns.borrow_mut().props.insert(
                        name.as_str(),
                        Property::data(Value::Undefined, true, true, false),
                    );
                }
                Resolution::Ns(v) => {
                    live.insert(name.clone(), NsBinding::Static(v.clone()));
                    ns.borrow_mut()
                        .props
                        .insert(name.as_str(), Property::data(v, true, true, false));
                }
                // Ambiguous / unresolvable star names are omitted from the namespace.
                _ => {}
            }
        }
        // @@toStringTag = "Module": non-writable, non-enumerable, non-configurable.
        if let Some(tag) = crate::builtins::to_string_tag_key(self) {
            ns.borrow_mut().props.insert(
                tag,
                Property::data(
                    Value::from_string("Module".to_string()),
                    false,
                    false,
                    false,
                ),
            );
        }
        ns.borrow_mut().extensible = false;
        self.module_ns.insert(Rc::as_ptr(ns) as usize, live);
        Ok(())
    }

    /// Build the distinct deferred-namespace object for `import defer * as ns`: same exports and
    /// live bindings as the module's ordinary namespace, but a separate identity, a
    /// "Deferred Module" @@toStringTag, and evaluation-on-first-string-keyed-access.
    fn make_deferred_ns(&mut self, dep: &str) -> Value {
        let base = self.module_recs[dep].ns.clone();
        let Value::Obj(base_o) = &base else {
            return base;
        };
        let dns = Object::new(None);
        {
            let src = base_o.borrow();
            let mut dst = dns.borrow_mut();
            for (k, p) in src.props.iter() {
                let mut p = p.clone();
                if Interp::is_sym_key(k) {
                    if let Value::Str(tag) = &p.value {
                        if &**tag == "Module" {
                            p.value = Value::from_string("Deferred Module".to_string());
                        }
                    }
                }
                dst.props.insert(k.clone(), p);
            }
            dst.extensible = false;
        }
        if let Some(live) = self.module_ns.get(&(Rc::as_ptr(base_o) as usize)).cloned() {
            self.module_ns.insert(Rc::as_ptr(&dns) as usize, live);
        }
        self.deferred_ns
            .insert(Rc::as_ptr(&dns) as usize, dep.to_string());
        Value::Obj(dns)
    }

    // --- Evaluate phase -----------------------------------------------------------------------

    /// Evaluate module `key` and its dependencies depth-first (each body runs at most once). A body
    /// that throws poisons the module so later imports observe the same error.
    /// Deferred-namespace trigger: evaluate a module on first access of its namespace.
    pub(crate) fn evaluate_deferred(&mut self, key: &str) -> Result<(), Abrupt> {
        if self.module_recs.contains_key(key) {
            self.evaluate_module(key)?;
        }
        Ok(())
    }

    fn evaluate_module(&mut self, key: &str) -> Result<(), Abrupt> {
        {
            let rec = &self.module_recs[key];
            if let Some(err) = &rec.eval_error {
                return Err(Abrupt::Throw(err.clone()));
            }
            if rec.evaluated || rec.evaluating {
                return Ok(());
            }
        }
        self.module_recs.get_mut(key).unwrap().evaluating = true;

        let dep_keys = self.module_recs[key].dep_keys.clone();
        let deferred = self.module_recs[key].deferred_deps.clone();
        for dep in &dep_keys {
            // A dependency imported only via `import defer` evaluates lazily, on first
            // namespace access.
            if deferred.contains(dep) {
                continue;
            }
            self.evaluate_module(dep)?;
        }

        let (body, env, meta) = {
            let rec = &self.module_recs[key];
            (rec.body.clone(), rec.env.clone(), rec.meta.clone())
        };
        let saved_meta = self.import_meta.take();
        let saved_strict = self.strict;
        self.import_meta = Some(meta);
        self.strict = true;
        let result = self.run_stmt_list(&body, &env);
        self.import_meta = saved_meta;
        self.strict = saved_strict;

        let rec = self.module_recs.get_mut(key).unwrap();
        rec.evaluating = false;
        match result {
            Ok(_) => {
                rec.evaluated = true;
                Ok(())
            }
            Err(a) => {
                let v = crate::interpreter::abrupt_value(a);
                rec.eval_error = Some(v.clone());
                rec.evaluated = true;
                Err(Abrupt::Throw(v))
            }
        }
    }

    // --- Namespace exotic-object behaviour ----------------------------------------------------

    /// Whether `ptr` (an object pointer) is a module namespace exotic object.
    pub(crate) fn is_namespace(&self, ptr: usize) -> bool {
        self.module_ns.contains_key(&ptr)
    }

    /// A module namespace's `[[GetOwnProperty]]` for a string key: `Some(property)` with the export's
    /// *current* value (writable, enumerable, non-configurable) if `key` is an exported name, or
    /// `Some(Err(ReferenceError))` if the underlying binding is still uninitialized. `None` means the
    /// key is not an export (the caller falls back to ordinary lookup — e.g. `@@toStringTag`).
    pub(crate) fn namespace_own_property(
        &mut self,
        ptr: usize,
        key: &str,
    ) -> Option<Result<Property, Abrupt>> {
        let binding = self.module_ns.get(&ptr)?.get(key)?.clone();
        let value = match binding {
            NsBinding::Live(env, local) => match self.get_var(&local, &env) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            },
            NsBinding::Static(v) => v,
        };
        Some(Ok(Property::data(value, true, true, false)))
    }

    /// `import(specifier)`: synchronously load the module and return an already-resolved promise of
    /// its namespace (or a rejected promise if loading throws).
    pub(crate) fn dynamic_import(&mut self, specifier: &str, attr_type: Option<&str>) -> Value {
        let promise = self.new_promise();
        let referrer = match &self.import_meta {
            Some(m) => match self.get_member(&m.clone(), "url") {
                Ok(Value::Str(s)) => s.to_string(),
                _ => self.import_base.clone(),
            },
            None => self.import_base.clone(),
        };
        let result = (|| {
            let (canon, src) = self.fetch_module(specifier, &referrer)?;
            let canon = match attr_type {
                Some(t @ ("json" | "text" | "bytes")) => format!("{canon}#{t}"),
                _ => canon,
            };
            let src = typed_module_source(src, attr_type);
            self.load_module(&canon, &src)
        })();
        match result {
            Ok(ns) => self.resolve_promise(&promise, ns),
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                self.reject_promise(&promise, reason);
            }
        }
        promise
    }
}

/// Whether two export resolutions denote the same binding (so a name re-exported by two star paths
/// is not ambiguous).
fn same_binding(a: &Resolution, b: &Resolution) -> bool {
    match (a, b) {
        (Resolution::Local(e1, l1), Resolution::Local(e2, l2)) => Rc::ptr_eq(e1, e2) && l1 == l2,
        (Resolution::Ns(Value::Obj(o1)), Resolution::Ns(Value::Obj(o2))) => Rc::ptr_eq(o1, o2),
        _ => false,
    }
}

/// Every module specifier this body imports/re-exports from (with duplicates).
fn module_dependency_specifiers(body: &[Stmt]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for stmt in body {
        match stmt {
            Stmt::Import(decl) => out.push((decl.source.to_string(), decl.attr_type.clone())),
            Stmt::ExportNamed {
                source: Some(src), ..
            }
            | Stmt::ExportAll { source: src, .. } => out.push((src.to_string(), None)),
            _ => {}
        }
    }
    out
}

/// The source text as a JS string literal (escaped).
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

/// Synthesize the module source for a `with { type: ... }` dependency: json parses the text, text
/// exports it verbatim, bytes exports it as a Uint8Array of its UTF-8 bytes. An unknown type keeps
/// the source as-is (an ordinary module).
pub(crate) fn typed_module_source(text: String, attr_type: Option<&str>) -> String {
    match attr_type {
        Some("json") => format!("export default JSON.parse({});", js_string_literal(&text)),
        Some("text") => format!("export default {};", js_string_literal(&text)),
        Some("bytes") => {
            let bytes: Vec<String> = text.bytes().map(|b| b.to_string()).collect();
            format!(
                "export default (() => {{ const a = new Uint8Array([{}]); Object.freeze(a.buffer); return a; }})();",
                bytes.join(",")
            )
        }
        _ => text,
    }
}

/// A module's parsed export/import tables.
struct ExportTables {
    local_exports: HashMap<String, String>,
    imports: HashMap<String, ImportOrigin>,
    indirect: HashMap<String, (String, String)>,
    star_as: HashMap<String, String>,
    stars: Vec<String>,
}

/// Build a module's export/import tables from its parsed body and resolved specifier→key map.
fn build_export_tables(body: &[Stmt], resolved: &HashMap<String, String>) -> ExportTables {
    let mut local_exports: HashMap<String, String> = HashMap::new();
    let mut imports: HashMap<String, ImportOrigin> = HashMap::new();
    let mut indirect: HashMap<String, (String, String)> = HashMap::new();
    let mut star_as: HashMap<String, String> = HashMap::new();
    let mut stars: Vec<String> = Vec::new();
    let key_of = |src: &str| {
        resolved
            .get(src)
            .cloned()
            .unwrap_or_else(|| src.to_string())
    };

    for stmt in body {
        match stmt {
            Stmt::Import(decl) => {
                let dep = key_of(&decl.source);
                for spec in &decl.specs {
                    match spec {
                        ImportSpec::Namespace(local) | ImportSpec::DeferNamespace(local) => {
                            imports.insert(local.clone(), ImportOrigin::Namespace(dep.clone()));
                        }
                        ImportSpec::Default(local) => {
                            imports.insert(
                                local.clone(),
                                ImportOrigin::Named(dep.clone(), "default".to_string()),
                            );
                        }
                        ImportSpec::Named { imported, local } => {
                            imports.insert(
                                local.clone(),
                                ImportOrigin::Named(dep.clone(), imported.clone()),
                            );
                        }
                    }
                }
            }
            Stmt::ExportDecl(inner) => {
                for name in exported_decl_names(inner) {
                    local_exports.insert(name.clone(), name);
                }
            }
            Stmt::ExportDefault(inner) => {
                let local = match &**inner {
                    Stmt::FuncDecl(f) if f.name.is_some() => f.name.clone().unwrap(),
                    Stmt::ClassDecl(c) if c.name.is_some() => c.name.clone().unwrap(),
                    _ => "*default*".to_string(),
                };
                local_exports.insert("default".to_string(), local);
            }
            Stmt::ExportNamed { specs, source } => {
                for spec in specs {
                    match source {
                        Some(src) => {
                            indirect
                                .insert(spec.exported.clone(), (key_of(src), spec.local.clone()));
                        }
                        None => {
                            local_exports.insert(spec.exported.clone(), spec.local.clone());
                        }
                    }
                }
            }
            Stmt::ExportAll { source, exported } => match exported {
                Some(name) => {
                    star_as.insert(name.clone(), key_of(source));
                }
                None => stars.push(key_of(source)),
            },
            _ => {}
        }
    }
    ExportTables {
        local_exports,
        imports,
        indirect,
        star_as,
        stars,
    }
}

/// Names introduced by an `export <decl>` statement's inner declaration.
fn exported_decl_names(inner: &Stmt) -> Vec<String> {
    match inner {
        Stmt::VarDecl { decls, .. } => {
            let mut out = Vec::new();
            for (pat, _) in decls {
                crate::interpreter::pattern_idents(pat, &mut out);
            }
            out
        }
        Stmt::FuncDecl(f) => f.name.clone().into_iter().collect(),
        Stmt::ClassDecl(c) => c.name.clone().into_iter().collect(),
        _ => Vec::new(),
    }
}
