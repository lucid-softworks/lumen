//! ES module loading, linking, and evaluation.
//!
//! lumen loads modules eagerly and depth-first: a module's dependencies are fully evaluated before
//! its own body runs, and each module's exported bindings are snapshotted into a frozen namespace
//! object once it finishes. Specifier resolution + source fetching is delegated to a host loader
//! (`Interp::module_loader`) so the engine stays filesystem-agnostic.

use crate::ast::*;
use crate::interpreter::{new_scope, Abrupt, Env, Interp};
use crate::value::{Object, Property, Value};

impl Interp {
    /// Parse + evaluate a module identified by canonical `key`, returning its namespace object.
    /// Results are cached (and a re-entrant load returns the in-progress namespace to break cycles).
    pub(crate) fn load_module(&mut self, key: &str, src: &str) -> Result<Value, Abrupt> {
        if let Some(ns) = self.modules.get(key) {
            return Ok(ns.clone());
        }
        let body = crate::parser::parse_module(src).map_err(|e| self.throw("SyntaxError", e.message))?;

        // The module's scope and namespace are created up front so circular imports resolve to the
        // (still-populating) namespace rather than recursing forever.
        let mod_env = new_scope(Some(self.global_env.clone()));
        let ns_obj = Object::new(None);
        let ns = Value::Obj(ns_obj.clone());
        crate::builtins::set_to_string_tag(self, &ns_obj, "Module");
        self.modules.insert(key.to_string(), ns.clone());

        // Phase 1: load every dependency (import / export-from / export-* sources).
        for stmt in &body {
            match stmt {
                Stmt::Import(decl) => {
                    let dep = self.resolve_and_load(&decl.source, key)?;
                    self.bind_imports(&decl.specs, &dep, &mod_env)?;
                }
                Stmt::ExportNamed { source: Some(src), .. } | Stmt::ExportAll { source: src, .. } => {
                    self.resolve_and_load(src, key)?;
                }
                _ => {}
            }
        }

        // Phase 2: run the module body in its (strict) scope with its own import.meta.
        let saved_meta = self.import_meta.take();
        let saved_strict = self.strict;
        self.strict = true;
        let meta = self.new_object();
        meta.borrow_mut().props.insert("url", Property::data(Value::from_string(key.to_string()), true, true, true));
        self.import_meta = Some(Value::Obj(meta));
        let result = self.eval_in_scope(&body, &mod_env);
        self.import_meta = saved_meta;
        self.strict = saved_strict;
        result?;

        // Phase 3: snapshot the exported bindings into the namespace object.
        self.populate_namespace(&body, &mod_env, key, &ns_obj)?;
        Ok(ns)
    }

    /// Resolve `specifier` (relative to `referrer`) via the host loader and load that module.
    fn resolve_and_load(&mut self, specifier: &str, referrer: &str) -> Result<Value, Abrupt> {
        let loader = match &self.module_loader {
            Some(l) => l.clone(),
            None => return Err(self.throw("TypeError", "no module loader configured")),
        };
        match loader(specifier, referrer) {
            Some((canon, src)) => {
                if let Some(ns) = self.modules.get(&canon) {
                    return Ok(ns.clone());
                }
                self.load_module(&canon, &src)
            }
            None => Err(self.throw("TypeError", format!("module not found: {specifier}"))),
        }
    }

    /// Bind a module's import specifiers into its scope (snapshotting the dependency's exports).
    fn bind_imports(&mut self, specs: &[ImportSpec], dep_ns: &Value, env: &Env) -> Result<(), Abrupt> {
        for spec in specs {
            match spec {
                ImportSpec::Default(local) => {
                    let v = self.get_member(dep_ns, "default")?;
                    self.declare_import_binding(local, v, env);
                }
                ImportSpec::Namespace(local) => {
                    self.declare_import_binding(local, dep_ns.clone(), env);
                }
                ImportSpec::Named { imported, local } => {
                    // Resolution error: the dependency must actually export this name.
                    let exists = matches!(dep_ns, Value::Obj(o) if o.borrow().props.contains(imported.as_str()));
                    if !exists {
                        return Err(self.throw(
                            "SyntaxError",
                            format!("the requested module does not provide an export named '{imported}'"),
                        ));
                    }
                    let v = self.get_member(dep_ns, imported)?;
                    self.declare_import_binding(local, v, env);
                }
            }
        }
        Ok(())
    }

    fn declare_import_binding(&self, name: &str, value: Value, env: &Env) {
        env.borrow_mut().vars.insert(
            name.to_string(),
            crate::interpreter::Binding { value, mutable: false, initialized: true },
        );
    }

    /// Build the module namespace: one data property per export name, in sorted order.
    fn populate_namespace(
        &mut self,
        body: &[Stmt],
        env: &Env,
        key: &str,
        ns: &crate::value::Gc,
    ) -> Result<(), Abrupt> {
        let mut entries: Vec<(String, Value)> = Vec::new();
        for stmt in body {
            match stmt {
                Stmt::ExportDecl(inner) => {
                    for name in exported_decl_names(inner) {
                        let v = self.get_var(&name, env).unwrap_or(Value::Undefined);
                        entries.push((name, v));
                    }
                }
                Stmt::ExportDefault(inner) => {
                    let v = match &**inner {
                        Stmt::FuncDecl(f) => f.name.as_ref().and_then(|n| self.get_var(n, env).ok()),
                        Stmt::ClassDecl(c) => c.name.as_ref().and_then(|n| self.get_var(n, env).ok()),
                        _ => self.get_var("*default*", env).ok(),
                    };
                    entries.push(("default".to_string(), v.unwrap_or(Value::Undefined)));
                }
                Stmt::ExportNamed { specs, source } => {
                    for spec in specs {
                        let v = match source {
                            Some(src) => {
                                let dep = self.resolve_and_load(src, key)?;
                                self.get_member(&dep, &spec.local)?
                            }
                            None => self.get_var(&spec.local, env).unwrap_or(Value::Undefined),
                        };
                        entries.push((spec.exported.clone(), v));
                    }
                }
                Stmt::ExportAll { source, exported } => {
                    let dep = self.resolve_and_load(source, key)?;
                    match exported {
                        Some(name) => entries.push((name.clone(), dep)),
                        None => {
                            // Re-export every name of the dependency (except `default`).
                            if let Value::Obj(o) = &dep {
                                let keys = o.borrow().props.keys();
                                for k in keys {
                                    if &*k != "default" && !Interp::is_sym_key(&k) {
                                        let v = self.get_member(&dep, &k)?;
                                        entries.push((k.to_string(), v));
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Namespace properties are enumerable, non-writable, non-configurable, sorted by name.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        for (name, value) in entries {
            ns.borrow_mut().props.insert(name.as_str(), Property::data(value, false, true, false));
        }
        ns.borrow_mut().extensible = false;
        Ok(())
    }

    /// `import(specifier)`: synchronously load the module and return an already-resolved promise of
    /// its namespace (or a rejected promise if loading throws).
    pub(crate) fn dynamic_import(&mut self, specifier: &str) -> Value {
        let promise = self.new_promise();
        let meta = self.import_meta.clone();
        let referrer = match meta {
            Some(m) => match self.get_member(&m, "url") {
                Ok(Value::Str(s)) => s.to_string(),
                _ => String::new(),
            },
            None => String::new(),
        };
        match self.resolve_and_load(specifier, &referrer) {
            Ok(ns) => self.resolve_promise(&promise, ns),
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                self.reject_promise(&promise, reason);
            }
        }
        promise
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
