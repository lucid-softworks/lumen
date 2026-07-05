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
use crate::builtins::make_bound_len;
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
    pub ns: Value,
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
    /// For a dynamically-imported module evaluating in a coroutine (top-level await): the promise
    /// that settles when the body finishes.
    top_promise: Option<Value>,
    /// This batch has visited the module (spec [[Status]] >= evaluating).
    started: bool,
    /// Count of not-yet-finished async-evaluating dependencies ([[PendingAsyncDependencies]]).
    pending_async: usize,
    /// Position in the async execution queue ([[AsyncEvaluationOrder]]).
    async_order: Option<u64>,
    /// Importers waiting on this module ([[AsyncParentModules]]).
    async_parents: Vec<String>,
    /// Tarjan bookkeeping for the evaluation DFS ([[DFSIndex]] / [[DFSAncestorIndex]]).
    dfs_index: Option<usize>,
    dfs_anc: usize,
    on_stack: bool,
    /// The root of this module's strongly-connected component ([[CycleRoot]]).
    cycle_root: Option<String>,
}

/// The origin of a local name that is an import binding (so re-exports resolve to the source).
enum ImportOrigin {
    /// `import * as x from 'dep'` — resolves to `dep`'s namespace object.
    Namespace(String),
    /// `import defer * as x from 'dep'` — resolves to `dep`'s DEFERRED namespace object.
    DeferNamespace(String),
    /// `import { y as x } from 'dep'` / `import x from 'dep'` — resolves to `dep`'s export `y`.
    Named(String, String),
}

/// The result of resolving an export name (spec ResolveExport).
enum Resolution {
    /// A concrete binding: `local` in the given module scope.
    Local(Env, String),
    /// A re-exported `import defer * as ns` binding: the dep's DEFERRED namespace.
    DeferNs(String),
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
        // Phase 3: evaluate module bodies depth-first. A graph containing top-level await
        // evaluates through the async machinery (awaits interleave with the job queue, an async
        // module doesn't block siblings, ancestors run in [[AsyncEvaluationOrder]]); a fully
        // synchronous graph runs directly.
        // Pre-create every graph member's evaluation promise, so a dynamic import of a
        // not-yet-executed batch member waits for the batch instead of executing it early.
        let mut graph = Vec::new();
        self.collect_graph(key, &mut graph);
        for m in &graph {
            if !self.module_recs[m].evaluated && self.module_recs[m].top_promise.is_none() {
                let p = self.new_promise();
                self.module_recs.get_mut(m).unwrap().top_promise = Some(p);
            }
        }
        let top = self.module_recs[key].top_promise.clone().unwrap();
        let mut stack = Vec::new();
        self.inner_module_evaluation_async(key, &mut stack, &mut 0);
        self.run_agent_event_loop();
        if let Value::Obj(o) = &top {
            if let Some(ps) = self.promises.get(&(Rc::as_ptr(o) as usize)) {
                if ps.status == 2 {
                    let reason = ps.value.clone();
                    return Err(Abrupt::Throw(reason));
                }
            }
        }
        Ok(self.module_recs[key].ns.clone())
    }

    /// Every module in `key`'s dependency graph (including itself), depth-first.
    fn collect_graph(&self, key: &str, out: &mut Vec<String>) {
        if out.iter().any(|k| k == key) {
            return;
        }
        out.push(key.to_string());
        if let Some(rec) = self.module_recs.get(key) {
            for d in rec.dep_keys.clone() {
                self.collect_graph(&d, out);
            }
        }
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
            // The host-defined '<module source>' specifier resolves only in the source phase: it
            // gets a ModuleSource object but no module record.
            if spec == "<module source>" {
                resolved.insert(spec.clone(), spec);
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
        // `import.meta` resolves lexically: a function exported from this module keeps seeing
        // this module's object no matter who calls it.
        crate::eval::bind(&env, "%importmeta%", meta.clone());

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
                top_promise: None,
                started: false,
                pending_async: 0,
                async_order: None,
                async_parents: Vec::new(),
                dfs_index: None,
                dfs_anc: 0,
                on_stack: false,
                cycle_root: None,
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
                Resolution::Local(..) | Resolution::Ns(..) | Resolution::DeferNs(..) => {}
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
                    ImportSpec::Source(local) => {
                        // GetModuleSource of a Source Text Module Record throws a SyntaxError:
                        // only the host-defined '<module source>' module has a ModuleSource.
                        if dep != "<module source>" {
                            return Err(self.throw(
                                "SyntaxError",
                                "source-text modules have no module source",
                            ));
                        }
                        let src_obj = self.module_source_of(&dep);
                        self.bind_local(env, local, src_obj);
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
            Resolution::DeferNs(dep) => {
                let dns = self.make_deferred_ns(&dep);
                self.bind_local(env, local, dns);
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

    /// The (cached, per canonical key) ModuleSource object a source-phase import binds: an
    /// ordinary object whose prototype is %AbstractModuleSource%.prototype.
    pub(crate) fn module_source_of(&mut self, dep: &str) -> Value {
        if let Some(v) = self.module_source_objs.get(dep) {
            return v.clone();
        }
        let proto = self
            .extra_protos
            .get("%AbstractModuleSourceProto%")
            .cloned();
        let obj = Object::new(proto);
        let v = Value::Obj(obj);
        self.module_source_objs.insert(dep.to_string(), v.clone());
        v
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
        // A source-phase re-export resolves to the requested module's ModuleSource object.
        if name == "~source~" {
            return match self.module_source_objs.get(key) {
                Some(v) => Resolution::Ns(v.clone()),
                None => Resolution::NotFound,
            };
        }
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
                    ImportOrigin::DeferNamespace(dep) => {
                        if self.module_recs.contains_key(dep) {
                            Resolution::DeferNs(dep.clone())
                        } else {
                            Resolution::NotFound
                        }
                    }
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
                Resolution::DeferNs(dep) => {
                    let v = self.make_deferred_ns(&dep);
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
        // One deferred namespace per module: every `import defer` of the same module (and every
        // re-export of such a binding) observes the same object.
        if let Some(v) = self.deferred_ns_objs.get(dep) {
            return v.clone();
        }
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
        self.deferred_ns_objs
            .insert(dep.to_string(), Value::Obj(dns.clone()));
        Value::Obj(dns)
    }

    // --- Evaluate phase -----------------------------------------------------------------------

    /// Evaluate module `key` and its dependencies depth-first (each body runs at most once). A body
    /// that throws poisons the module so later imports observe the same error.
    /// Deferred-namespace trigger: evaluate a module on first access of its namespace.
    pub(crate) fn evaluate_deferred(&mut self, key: &str) -> Result<(), Abrupt> {
        if self.module_recs.contains_key(key) {
            // ReadyForSyncExecution: touching a deferred namespace while its module — or any
            // module in its dependency graph — is still evaluating is a TypeError.
            if !self.ready_for_sync(key, &mut Vec::new()) {
                return Err(self.throw(
                    "TypeError",
                    "cannot access a deferred namespace while its module graph is evaluating",
                ));
            }
            self.evaluate_module(key)?;
        }
        Ok(())
    }

    /// Whether `key`'s whole (non-deferred) dependency graph is free of mid-evaluation modules.
    fn ready_for_sync(&self, key: &str, seen: &mut Vec<String>) -> bool {
        if seen.iter().any(|k| k == key) {
            return true;
        }
        seen.push(key.to_string());
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return true,
        };
        if rec.evaluated && rec.eval_error.is_none() {
            return true;
        }
        // evaluating, evaluating-async (started and not finished), or parked at an await.
        if rec.evaluating || (rec.started && !rec.evaluated) {
            return false;
        }
        if !rec.evaluated && body_has_tla(&rec.body) {
            return false;
        }
        let deps = rec.dep_keys.clone();
        let deferred = rec.deferred_deps.clone();
        deps.iter()
            .filter(|d| !deferred.contains(d))
            .all(|d| self.ready_for_sync(d, seen))
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
        // Evaluate dependencies at the position of their first NON-defer clause (an
        // `import defer` earlier in the file must not pull the module's evaluation forward).
        let body_order = self.eager_dep_order(key);
        // Only deps in dep_keys evaluate here (source-phase pseudo-modules are excluded there).
        let order: Vec<String> = if body_order.is_empty() {
            dep_keys
                .iter()
                .filter(|d| !deferred.contains(*d))
                .cloned()
                .collect()
        } else {
            body_order
                .into_iter()
                .filter(|d| dep_keys.contains(d))
                .collect()
        };
        for dep in &order {
            if deferred.contains(dep) {
                // A dependency imported only via `import defer` evaluates lazily — but its
                // ASYNC (top-level-await) transitive dependencies still evaluate eagerly.
                self.evaluate_async_subgraph(dep, &mut Vec::new())?;
                continue;
            }
            self.evaluate_module(dep)?;
        }
        for dep in &dep_keys {
            if deferred.contains(dep) && !order.contains(dep) {
                self.evaluate_async_subgraph(dep, &mut Vec::new())?;
            }
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

    /// This module's dependencies in evaluation order: each dep at the position of its first
    /// non-defer import/export-from clause; defer-only deps at their first (defer) position.
    fn eager_dep_order(&self, key: &str) -> Vec<String> {
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let body = rec.body.clone();
        let resolved = rec.resolved.clone();
        let mut eager: Vec<String> = Vec::new();
        let mut defer_seen: Vec<String> = Vec::new();
        let push = |list: &mut Vec<String>, k: &str| {
            if !list.iter().any(|x| x == k) {
                list.push(k.to_string());
            }
        };
        for stmt in body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    decl.source.to_string(),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, crate::ast::ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (src.to_string(), false),
                _ => continue,
            };
            let Some(k) = resolved.get(&spec) else {
                continue;
            };
            if defers {
                push(&mut defer_seen, k);
            } else {
                push(&mut eager, k);
            }
        }
        // A defer-only dep (never named eagerly) keeps its SOURCE position — its async
        // subgraph evaluates there. A dep imported both ways evaluates at its first EAGER
        // position (the defer clause never pulls it forward).
        let mut merged: Vec<String> = Vec::new();
        for stmt in body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    decl.source.to_string(),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, crate::ast::ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (src.to_string(), false),
                _ => continue,
            };
            if let Some(k) = resolved.get(&spec) {
                let is_defer_only = defer_seen.contains(k) && !eager.contains(k);
                let place = if is_defer_only { true } else { !defers };
                if place && !merged.iter().any(|x| x == k) {
                    merged.push(k.clone());
                }
            }
        }
        merged
    }

    /// InnerModuleEvaluation for a graph containing top-level await. Dependencies process in
    /// source order; a module executes once its [[PendingAsyncDependencies]] hits zero — in a
    /// coroutine for a TLA body (each await interleaves with the job queue), synchronously
    /// otherwise. Completion cascades through [[AsyncParentModules]] in [[AsyncEvaluationOrder]].
    fn inner_module_evaluation_async(
        &mut self,
        key: &str,
        stack: &mut Vec<String>,
        index: &mut usize,
    ) {
        if self.module_recs[key].started || self.module_recs[key].evaluated {
            return;
        }
        {
            let rec = self.module_recs.get_mut(key).unwrap();
            rec.started = true;
            rec.dfs_index = Some(*index);
            rec.dfs_anc = *index;
            rec.on_stack = true;
        }
        *index += 1;
        stack.push(key.to_string());

        let dep_keys = self.module_recs[key].dep_keys.clone();
        let deferred = self.module_recs[key].deferred_deps.clone();
        let body_order = self.eager_dep_order(key);
        let order: Vec<String> = if body_order.is_empty() {
            dep_keys
                .iter()
                .filter(|d| !deferred.contains(*d))
                .cloned()
                .collect()
        } else {
            body_order
                .into_iter()
                .filter(|d| dep_keys.contains(d))
                .collect()
        };
        for dep in &order {
            if deferred.contains(dep) {
                self.defer_async_deps(key, dep, stack, index);
                continue;
            }
            self.inner_module_evaluation_async(dep, stack, index);
            if self.module_recs[dep.as_str()].on_stack {
                // Still on the DFS stack: same strongly-connected component. An in-cycle
                // dependency that already parked at a top-level await still counts as pending.
                let danc = self.module_recs[dep.as_str()].dfs_anc;
                {
                    let rec = self.module_recs.get_mut(key).unwrap();
                    rec.dfs_anc = rec.dfs_anc.min(danc);
                }
                let dep_parked = {
                    let d = &self.module_recs[dep.as_str()];
                    d.async_order.is_some() && !d.evaluated
                };
                if dep_parked {
                    self.module_recs.get_mut(key).unwrap().pending_async += 1;
                    self.module_recs
                        .get_mut(dep.as_str())
                        .unwrap()
                        .async_parents
                        .push(key.to_string());
                }
                continue;
            }
            // Completed (or async-parked) dependency: waiting attaches to its CYCLE ROOT.
            let root = self.module_recs[dep.as_str()]
                .cycle_root
                .clone()
                .unwrap_or_else(|| dep.to_string());
            let (dep_err, dep_done, dep_async) = {
                let d = &self.module_recs[&root];
                (d.eval_error.clone(), d.evaluated, d.async_order.is_some())
            };
            if let Some(err) = dep_err {
                self.finish_scc(key, stack);
                self.async_module_rejected(key, err);
                return;
            }
            if !dep_done && dep_async {
                self.module_recs.get_mut(key).unwrap().pending_async += 1;
                self.module_recs
                    .get_mut(&root)
                    .unwrap()
                    .async_parents
                    .push(key.to_string());
            }
        }
        for dep in &dep_keys {
            if deferred.contains(dep) && !order.contains(dep) {
                self.defer_async_deps(key, dep, stack, index);
            }
        }
        let has_tla = body_has_tla(&self.module_recs[key].body);
        if self.module_recs[key].pending_async > 0 || has_tla {
            self.module_async_seq += 1;
            self.module_recs.get_mut(key).unwrap().async_order = Some(self.module_async_seq);
        }
        if self.module_recs[key].pending_async == 0 {
            self.module_execute_async(key);
        }
        self.finish_scc(key, stack);
    }

    /// `import defer`: the deferred module's own evaluation waits for a namespace access, but
    /// every module in its subgraph with top-level await evaluates NOW — and the deferring
    /// parent pends on each (so the later synchronous evaluation meets no pending async dep).
    fn defer_async_deps(
        &mut self,
        parent: &str,
        dep: &str,
        stack: &mut Vec<String>,
        index: &mut usize,
    ) {
        let mut graph = Vec::new();
        self.collect_graph(dep, &mut graph);
        for m in graph {
            if !body_has_tla(&self.module_recs[&m].body) {
                continue;
            }
            if !self.module_recs[&m].evaluated && self.module_recs[&m].top_promise.is_none() {
                let p = self.new_promise();
                self.module_recs.get_mut(&m).unwrap().top_promise = Some(p);
            }
            self.inner_module_evaluation_async(&m, stack, index);
            if self.module_recs[&m].on_stack {
                continue;
            }
            let root = self.module_recs[&m]
                .cycle_root
                .clone()
                .unwrap_or_else(|| m.clone());
            let (dep_err, dep_done, dep_async) = {
                let d = &self.module_recs[&root];
                (d.eval_error.clone(), d.evaluated, d.async_order.is_some())
            };
            if dep_err.is_some() || dep_done || !dep_async {
                continue;
            }
            let already = self.module_recs[&root]
                .async_parents
                .iter()
                .any(|p| p == parent);
            if !already {
                self.module_recs.get_mut(parent).unwrap().pending_async += 1;
                self.module_recs
                    .get_mut(&root)
                    .unwrap()
                    .async_parents
                    .push(parent.to_string());
            }
        }
    }

    /// If `key` is its component's root, pop the SCC off the DFS stack and stamp each member's
    /// [[CycleRoot]].
    fn finish_scc(&mut self, key: &str, stack: &mut Vec<String>) {
        let (di, danc, on_stack) = {
            let r = &self.module_recs[key];
            (r.dfs_index, r.dfs_anc, r.on_stack)
        };
        if !on_stack {
            return;
        }
        let Some(di) = di else { return };
        if danc != di {
            return;
        }
        while let Some(m) = stack.pop() {
            let rec = self.module_recs.get_mut(&m).unwrap();
            rec.on_stack = false;
            rec.cycle_root = Some(key.to_string());
            if m == key {
                break;
            }
        }
    }

    /// Execute `key`'s own body (all dependencies done): a TLA body parks in a coroutine whose
    /// completion runs the ancestor cascade; a synchronous body runs now.
    fn module_execute_async(&mut self, key: &str) {
        let top = self.module_recs[key].top_promise.clone();
        let (body, env, meta) = {
            let rec = &self.module_recs[key];
            (rec.body.clone(), rec.env.clone(), rec.meta.clone())
        };
        if body_has_tla(&body) {
            self.module_recs.get_mut(key).unwrap().evaluating = true;
            let module_key = key.to_string();
            let closure: Box<dyn FnOnce(&mut Interp) -> crate::coroutine::Suspend> =
                Box::new(move |i| {
                    let saved_meta = i.import_meta.take();
                    let saved_strict = i.strict;
                    i.import_meta = Some(meta);
                    i.strict = true;
                    let result = i.run_stmt_list(&body, &env);
                    i.import_meta = saved_meta;
                    i.strict = saved_strict;
                    match result {
                        Ok(_) => {
                            i.finish_dynamic_module(&module_key, None);
                            crate::coroutine::Suspend::Done(Value::Undefined)
                        }
                        Err(a) => {
                            let v = crate::interpreter::abrupt_value(a);
                            i.finish_dynamic_module(&module_key, Some(v.clone()));
                            crate::coroutine::Suspend::Throw(v)
                        }
                    }
                });
            let ptr = self as *mut Interp;
            let coro = crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(closure));
            let top = match top {
                Some(t) => t,
                None => self.new_promise(),
            };
            if let Value::Obj(o) = &top {
                self.generators.insert(Rc::as_ptr(o) as usize, coro);
            }
            let top_key = match &top {
                Value::Obj(o) => Rc::as_ptr(o) as usize,
                _ => return,
            };
            self.drive_async(
                top_key,
                top,
                crate::coroutine::Resume::Next(Value::Undefined),
            );
            return;
        }
        let saved_meta = self.import_meta.take();
        let saved_strict = self.strict;
        self.import_meta = Some(meta);
        self.strict = true;
        self.module_recs.get_mut(key).unwrap().evaluating = true;
        let result = self.run_stmt_list(&body, &env);
        self.import_meta = saved_meta;
        self.strict = saved_strict;
        self.module_recs.get_mut(key).unwrap().evaluating = false;
        match result {
            Ok(_) => {
                self.module_recs.get_mut(key).unwrap().evaluated = true;
                if let Some(t) = self.module_recs[key].top_promise.clone() {
                    self.resolve_promise(&t, Value::Undefined);
                }
            }
            Err(a) => {
                let v = crate::interpreter::abrupt_value(a);
                self.async_module_rejected(key, v);
            }
        }
    }

    /// AsyncModuleExecutionFulfilled: run every ancestor whose pending count reaches zero, in
    /// [[AsyncEvaluationOrder]] (ascending).
    pub(crate) fn async_module_fulfilled(&mut self, key: &str) {
        let mut exec: Vec<(u64, String)> = Vec::new();
        self.gather_available_ancestors(key, &mut exec);
        exec.sort_by_key(|(o, _)| *o);
        for (_, m) in exec {
            if self.module_recs[&m].evaluated || self.module_recs[&m].eval_error.is_some() {
                continue;
            }
            self.module_execute_async(&m);
        }
    }

    fn gather_available_ancestors(&mut self, key: &str, out: &mut Vec<(u64, String)>) {
        let parents = self.module_recs[key].async_parents.clone();
        for parent in parents {
            let (done, err, pending) = {
                let p = &self.module_recs[&parent];
                (p.evaluated, p.eval_error.is_some(), p.pending_async)
            };
            if done || err || pending == 0 {
                continue;
            }
            let p = self.module_recs.get_mut(&parent).unwrap();
            p.pending_async -= 1;
            if p.pending_async == 0 {
                let order = p.async_order.unwrap_or(u64::MAX);
                out.push((order, parent.clone()));
                // A synchronous ancestor completes as part of this cascade, so its own parents
                // become available too (GatherAvailableAncestors' recursion).
                if !body_has_tla(&self.module_recs[&parent].body) {
                    self.gather_available_ancestors(&parent, out);
                }
            }
        }
    }

    /// AsyncModuleExecutionRejected: the error propagates to every waiting ancestor.
    pub(crate) fn async_module_rejected(&mut self, key: &str, err: Value) {
        {
            let rec = self.module_recs.get_mut(key).unwrap();
            if rec.evaluated || rec.eval_error.is_some() {
                return;
            }
            rec.eval_error = Some(err.clone());
            rec.evaluated = true;
            rec.evaluating = false;
        }
        if let Some(t) = self.module_recs[key].top_promise.clone() {
            self.reject_promise(&t, err.clone());
        }
        for parent in self.module_recs[key].async_parents.clone() {
            self.async_module_rejected(&parent, err.clone());
        }
    }

    /// Evaluate every module with top-level await (plus its own dependencies) in `key`'s graph —
    /// the eager part of an `import defer`.
    fn evaluate_async_subgraph(&mut self, key: &str, seen: &mut Vec<String>) -> Result<(), Abrupt> {
        if seen.iter().any(|k| k == key) {
            return Ok(());
        }
        seen.push(key.to_string());
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Ok(()),
        };
        if rec.evaluated || rec.evaluating {
            return Ok(());
        }
        if body_has_tla(&rec.body) {
            // The async module (and its own graph) evaluates through the batch machinery — the
            // deferring parent does NOT wait on it.
            let mut graph = Vec::new();
            self.collect_graph(key, &mut graph);
            for m in &graph {
                if !self.module_recs[m].evaluated && self.module_recs[m].top_promise.is_none() {
                    let p = self.new_promise();
                    self.module_recs.get_mut(m).unwrap().top_promise = Some(p);
                }
            }
            self.inner_module_evaluation_async(key, &mut Vec::new(), &mut 0);
            return Ok(());
        }
        let deps = rec.dep_keys.clone();
        let deferred = rec.deferred_deps.clone();
        for dep in deps {
            if deferred.contains(&dep) {
                continue;
            }
            self.evaluate_async_subgraph(&dep, seen)?;
        }
        Ok(())
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
    pub(crate) fn dynamic_import(
        &mut self,
        specifier: &str,
        attr_type: Option<&str>,
        defer: bool,
    ) -> Value {
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
            self.parse_and_register(&canon, Some(src))?;
            self.link_module(&canon)?;
            Ok(canon)
        })();
        match result {
            Ok(canon) if defer => {
                // import.defer: link only; resolve with the (shared) deferred namespace. The
                // async subgraph still evaluates eagerly.
                let r = self.evaluate_async_subgraph(&canon, &mut Vec::new());
                match r {
                    Ok(()) => {
                        let dns = self.make_deferred_ns(&canon);
                        self.resolve_promise(&promise, dns);
                    }
                    Err(e) => {
                        let reason = crate::interpreter::abrupt_value(e);
                        self.reject_promise(&promise, reason);
                    }
                }
            }
            Ok(canon) => {
                // Evaluation may suspend at a top-level await: chain the import promise onto the
                // module's evaluation promise, resolving with the namespace.
                let top = self.evaluate_module_dynamic(&canon);
                let ns = self.module_recs[&canon].ns.clone();
                let on_f =
                    make_bound_len(self, dynamic_import_fulfil, vec![promise.clone(), ns], 1.0);
                let on_r = make_bound_len(self, dynamic_import_reject, vec![promise.clone()], 1.0);
                self.promise_then(&top, on_f, on_r);
            }
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                self.reject_promise(&promise, reason);
            }
        }
        promise
    }

    /// Evaluate a dynamically-imported module, returning a promise that settles when its body
    /// (which may use top-level await) completes. Dependencies evaluate synchronously first; the
    /// module's own body runs in a coroutine so a top-level await parks it.
    fn evaluate_module_dynamic(&mut self, key: &str) -> Value {
        let (top, err, settled) = {
            let rec = &self.module_recs[key];
            (
                rec.top_promise.clone(),
                rec.eval_error.clone(),
                rec.evaluated || rec.evaluating,
            )
        };
        if let Some(top) = top {
            return top;
        }
        if let Some(err) = err {
            let p = self.new_promise();
            self.reject_promise(&p, err);
            return p;
        }
        if settled {
            let p = self.new_promise();
            self.resolve_promise(&p, Value::Undefined);
            return p;
        }
        // Same batch machinery as a top-level Evaluate(): pre-create the graph's promises and
        // run InnerModuleEvaluation (awaits park; siblings continue; ancestors cascade).
        let mut graph = Vec::new();
        self.collect_graph(key, &mut graph);
        for m in &graph {
            if !self.module_recs[m].evaluated && self.module_recs[m].top_promise.is_none() {
                let p = self.new_promise();
                self.module_recs.get_mut(m).unwrap().top_promise = Some(p);
            }
        }
        self.inner_module_evaluation_async(key, &mut Vec::new(), &mut 0);
        self.module_recs[key]
            .top_promise
            .clone()
            .unwrap_or_else(|| {
                let p = self.new_promise();
                match self.module_recs[key].eval_error.clone() {
                    Some(e) => self.reject_promise(&p, e),
                    None => self.resolve_promise(&p, Value::Undefined),
                }
                p
            })
    }

    /// Record a dynamically-evaluated module's completion (run from inside its coroutine).
    fn finish_dynamic_module(&mut self, key: &str, error: Option<Value>) {
        match error {
            None => {
                if let Some(rec) = self.module_recs.get_mut(key) {
                    rec.evaluated = true;
                    rec.evaluating = false;
                }
                // The module's own promise settles first (the coroutine driver resolves it when
                // this returns), then ancestors execute in [[AsyncEvaluationOrder]].
                self.async_module_fulfilled(key);
            }
            Some(err) => {
                if let Some(rec) = self.module_recs.get_mut(key) {
                    rec.evaluated = false; // async_module_rejected records the error
                }
                self.async_module_rejected(key, err);
            }
        }
    }
}

/// Whether two export resolutions denote the same binding (so a name re-exported by two star paths
/// is not ambiguous).
fn same_binding(a: &Resolution, b: &Resolution) -> bool {
    match (a, b) {
        (Resolution::Local(e1, l1), Resolution::Local(e2, l2)) => Rc::ptr_eq(e1, e2) && l1 == l2,
        (Resolution::Ns(Value::Obj(o1)), Resolution::Ns(Value::Obj(o2))) => Rc::ptr_eq(o1, o2),
        (Resolution::DeferNs(d1), Resolution::DeferNs(d2)) => d1 == d2,
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

/// Reaction for a dynamic import: the module evaluated — resolve the import promise (`args[0]`)
/// with the namespace (`args[1]`).
fn dynamic_import_fulfil(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let p = args.first().cloned().unwrap_or(Value::Undefined);
    let ns = args.get(1).cloned().unwrap_or(Value::Undefined);
    i.resolve_promise(&p, ns);
    Ok(Value::Undefined)
}

/// Reaction for a dynamic import: evaluation failed — reject the import promise (`args[0]`) with
/// the reason (`args[1]`).
fn dynamic_import_reject(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let p = args.first().cloned().unwrap_or(Value::Undefined);
    let reason = args.get(1).cloned().unwrap_or(Value::Undefined);
    i.reject_promise(&p, reason);
    Ok(Value::Undefined)
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
            // The text was decoded latin-1 style (char == byte) when non-UTF-8; a UTF-8 source
            // re-encodes to its original bytes either way.
            let bytes: Vec<String> = if text.chars().all(|c| (c as u32) < 0x100) {
                text.chars().map(|c| (c as u32).to_string()).collect()
            } else {
                text.bytes().map(|b| b.to_string()).collect()
            };
            format!(
                "export default new Uint8Array(new Uint8Array([{}]).buffer.sliceToImmutable());",
                bytes.join(",")
            )
        }
        _ => text,
    }
}

/// Whether a module body contains top-level `await` (a for-await head, an `await using`, or an
/// Await expression outside any function/class body) — the async-module test for `import defer`.
pub(crate) fn body_has_tla(body: &[Stmt]) -> bool {
    fn stmt(s: &Stmt) -> bool {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => expr(e),
            Stmt::Return(v) => v.as_ref().map(expr).unwrap_or(false),
            Stmt::VarDecl { kind, decls } => {
                matches!(kind, crate::ast::DeclKind::AwaitUsing)
                    || decls
                        .iter()
                        .any(|(_, init)| init.as_ref().map(expr).unwrap_or(false))
            }
            Stmt::If { test, cons, alt } => {
                expr(test) || stmt(cons) || alt.as_ref().map(|a| stmt(a)).unwrap_or(false)
            }
            Stmt::Block(b) => b.iter().any(stmt),
            Stmt::While { test, body } | Stmt::DoWhile { body, test } => expr(test) || stmt(body),
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                init.as_ref()
                    .map(|i| match i.as_ref() {
                        crate::ast::ForInit::VarDecl { decls, .. } => decls
                            .iter()
                            .any(|(_, e)| e.as_ref().map(expr).unwrap_or(false)),
                        crate::ast::ForInit::Expr(e) => expr(e),
                    })
                    .unwrap_or(false)
                    || test.as_ref().map(expr).unwrap_or(false)
                    || update.as_ref().map(expr).unwrap_or(false)
                    || stmt(body)
            }
            Stmt::ForInOf {
                right,
                body,
                is_await,
                ..
            } => *is_await || expr(right) || stmt(body),
            Stmt::Try {
                block,
                handler,
                finalizer,
                ..
            } => {
                block.iter().any(stmt)
                    || handler
                        .as_ref()
                        .map(|(_, h)| h.iter().any(stmt))
                        .unwrap_or(false)
                    || finalizer
                        .as_ref()
                        .map(|f| f.iter().any(stmt))
                        .unwrap_or(false)
            }
            Stmt::Switch { disc, cases } => {
                expr(disc)
                    || cases.iter().any(|c| {
                        c.test.as_ref().map(expr).unwrap_or(false) || c.body.iter().any(stmt)
                    })
            }
            Stmt::Labeled { body, .. } => stmt(body),
            Stmt::With { obj, body } => expr(obj) || stmt(body),
            Stmt::ExportDefault(inner) | Stmt::ExportDecl(inner) => stmt(inner),
            _ => false,
        }
    }
    fn expr(e: &Expr) -> bool {
        match e {
            Expr::Await(_) => true,
            Expr::ToStr(x) | Expr::OptionalChain(x) => expr(x),
            Expr::Unary { arg, .. } | Expr::Update { arg, .. } => expr(arg),
            Expr::Binary { left, right, .. }
            | Expr::Logical { left, right, .. }
            | Expr::Assign {
                target: left,
                value: right,
                ..
            } => expr(left) || expr(right),
            Expr::Cond { test, cons, alt } => expr(test) || expr(cons) || expr(alt),
            Expr::Call { callee, args, .. } => {
                expr(callee)
                    || args.iter().any(|a| match a {
                        crate::ast::ArrayElem::Item(e) | crate::ast::ArrayElem::Spread(e) => {
                            expr(e)
                        }
                        _ => false,
                    })
            }
            Expr::New { callee, args } => {
                expr(callee)
                    || args.iter().any(|a| match a {
                        crate::ast::ArrayElem::Item(e) | crate::ast::ArrayElem::Spread(e) => {
                            expr(e)
                        }
                        _ => false,
                    })
            }
            Expr::Member { obj, .. } => expr(obj),
            Expr::Index { obj, index, .. } => expr(obj) || expr(index),
            Expr::Seq(v) => v.iter().any(expr),
            Expr::Array(elems) => elems.iter().any(|a| match a {
                crate::ast::ArrayElem::Item(e) | crate::ast::ArrayElem::Spread(e) => expr(e),
                _ => false,
            }),
            Expr::Object(props) => props.iter().any(|p| match p {
                crate::ast::PropDef::KeyValue { value, .. } => expr(value),
                crate::ast::PropDef::Spread(e) => expr(e),
                _ => false,
            }),
            Expr::Yield { arg, .. } => arg.as_ref().map(|a| expr(a)).unwrap_or(false),
            Expr::TaggedTemplate { tag, .. } => expr(tag),
            Expr::ImportCall { spec, options, .. } => {
                expr(spec) || options.as_ref().map(|o| expr(o)).unwrap_or(false)
            }
            _ => false,
        }
    }
    body.iter().any(stmt)
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
                        ImportSpec::Namespace(local) => {
                            imports.insert(local.clone(), ImportOrigin::Namespace(dep.clone()));
                        }
                        ImportSpec::DeferNamespace(local) => {
                            imports
                                .insert(local.clone(), ImportOrigin::DeferNamespace(dep.clone()));
                        }
                        ImportSpec::Source(local) => {
                            imports.insert(
                                local.clone(),
                                ImportOrigin::Named(dep.clone(), "~source~".to_string()),
                            );
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
