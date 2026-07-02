//! Statement execution, expression evaluation, and the ECMAScript abstract operations. Split out
//! of `interpreter.rs` to keep each file readable; this is the same `impl Interp`.

use crate::ast::*;
use crate::interpreter::*;
use crate::value::*;
use std::rc::Rc;

impl Interp {
    // ----- statements -------------------------------------------------------------------------

    /// Bind a (possibly destructuring) pattern to `value` in `env`. `Var` assigns to the hoisted
    /// binding; `Lexical` creates fresh bindings (params, `let`/`const`).
    pub(crate) fn bind_pattern(
        &mut self,
        pat: &Pattern,
        value: Value,
        env: &Env,
        mode: BindMode,
    ) -> Result<(), Abrupt> {
        match pat {
            Pattern::Ident(name) => {
                match mode {
                    BindMode::Lexical(is_const) => self.init_lexical(name, value, is_const, env),
                    BindMode::Var => self.assign_var(name, value, env)?,
                }
                Ok(())
            }
            Pattern::Array(elems) => {
                // Iterate lazily, pulling only what the pattern needs and closing the iterator when
                // we stop early (or on an abrupt completion).
                let (iter, next) = self.get_iterator(&value)?;
                let mut done = false;
                let result = (|me: &mut Self| -> Result<(), Abrupt> {
                    for el in elems {
                        match el {
                            ArrayPatElem::Hole => {
                                if !done && me.iterator_step(&iter, &next)?.is_none() {
                                    done = true;
                                }
                            }
                            ArrayPatElem::Elem { pattern, default } => {
                                let mut v = if done {
                                    Value::Undefined
                                } else {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => x,
                                        None => {
                                            done = true;
                                            Value::Undefined
                                        }
                                    }
                                };
                                if matches!(v, Value::Undefined) {
                                    if let Some(d) = default {
                                        v = me.eval(d, env)?;
                                        if let (Pattern::Ident(n), true) =
                                            (pattern, is_anonymous_fn(d))
                                        {
                                            me.set_fn_name(&v, n);
                                        }
                                    }
                                }
                                me.bind_pattern(pattern, v, env, mode)?;
                            }
                            ArrayPatElem::Rest(pattern) => {
                                let mut rest = Vec::new();
                                while !done {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => rest.push(x),
                                        None => done = true,
                                    }
                                }
                                let arr = me.make_array(rest);
                                me.bind_pattern(pattern, arr, env, mode)?;
                            }
                        }
                    }
                    Ok(())
                })(self);
                if !done {
                    self.iterator_close(&iter);
                }
                result?;
                Ok(())
            }
            Pattern::Object(objpat) => {
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(self.throw("TypeError", "cannot destructure null or undefined"));
                }
                let mut used: Vec<String> = Vec::new();
                for prop in &objpat.props {
                    let key = self.eval_prop_key(&prop.key, env)?;
                    used.push(key.clone());
                    let mut v = self.get_member(&value, &key)?;
                    if matches!(v, Value::Undefined) {
                        if let Some(d) = &prop.default {
                            v = self.eval(d, env)?;
                            if let (Pattern::Ident(n), true) = (&prop.value, is_anonymous_fn(d)) {
                                self.set_fn_name(&v, n);
                            }
                        }
                    }
                    self.bind_pattern(&prop.value, v, env, mode)?;
                }
                if let Some(rest_name) = &objpat.rest {
                    let obj = self.new_object();
                    if let Value::Obj(src) = &value {
                        let keys: Vec<_> = src
                            .borrow()
                            .props
                            .iter()
                            .filter(|(_, p)| p.enumerable)
                            .map(|(k, _)| k.clone())
                            .collect();
                        for k in keys {
                            if !used.iter().any(|u| u.as_str() == &*k) {
                                let v = self.get_member(&value, &k)?;
                                set_data(&obj, &k, v);
                            }
                        }
                    }
                    self.bind_pattern(
                        &Pattern::Ident(rest_name.clone()),
                        Value::Obj(obj),
                        env,
                        mode,
                    )?;
                }
                Ok(())
            }
            // A member target (`o.p`/`o[k]`): assign to it (never a declaration).
            Pattern::Member(target) => self.assign_to_target(target, value, env),
        }
    }

    /// Run a parsed script body in `env` (used by `eval`): hoist, declare lexicals, execute, and
    /// return the completion value (the value of the last value-producing statement).
    pub(crate) fn eval_in_scope(&mut self, body: &[Stmt], env: &Env) -> Result<Value, Abrupt> {
        self.hoist(body, env, true);
        self.declare_block_lexicals(body, env, false);
        self.run_stmt_list(body, env)
    }

    /// Run a statement list that has already had its bindings instantiated (hoisting + lexical
    /// declaration done by the caller). Used by module evaluation, where the module's environment is
    /// set up during the separate Instantiate phase before any body runs.
    pub(crate) fn run_stmt_list(&mut self, body: &[Stmt], env: &Env) -> Result<Value, Abrupt> {
        let has_using = body.iter().any(stmt_declares_using);
        if has_using {
            self.using_stack.push(Vec::new());
        }
        let mut last = Value::Undefined;
        let mut result: Completion = Ok(Value::Undefined);
        for stmt in body {
            match self.exec_stmt(stmt, env) {
                Ok(v) => {
                    if !matches!(v, Value::Undefined) {
                        last = v;
                    }
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if has_using {
            let frame = self.using_stack.pop().unwrap_or_default();
            result = self.dispose_frame(frame, result);
        }
        result?;
        Ok(last)
    }

    pub fn exec_block(&mut self, stmts: &[Stmt], parent: &Env) -> Completion {
        let scope = new_scope(Some(parent.clone()));
        self.declare_block_lexicals(stmts, &scope, true);
        // A block is a disposal boundary only when it actually declares a `using` resource.
        let has_using = stmts.iter().any(stmt_declares_using);
        if has_using {
            self.using_stack.push(Vec::new());
        }
        let mut result = Ok(Value::Undefined);
        for s in stmts {
            match self.exec_stmt(s, &scope) {
                Ok(v) => result = Ok(v),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if has_using {
            let frame = self.using_stack.pop().unwrap_or_default();
            result = self.dispose_frame(frame, result);
        }
        result
    }

    /// Capture a `using` resource's dispose method for disposal at scope exit. `null`/`undefined`
    /// resources are ignored; a non-callable dispose method is a TypeError.
    fn add_disposable(&mut self, value: &Value, is_async: bool) -> Result<(), Abrupt> {
        if matches!(value, Value::Undefined | Value::Null) {
            return Ok(());
        }
        let method = self.dispose_method(value, is_async)?;
        if self.using_stack.is_empty() {
            self.using_stack.push(Vec::new());
        }
        self.using_stack.last_mut().unwrap().push(Disposable {
            value: value.clone(),
            method,
        });
        Ok(())
    }

    /// GetDisposeMethod: `@@asyncDispose` (falling back to `@@dispose`) for `await using`, else
    /// `@@dispose`. Throws if the resolved method isn't callable.
    fn dispose_method(&mut self, value: &Value, is_async: bool) -> Result<Value, Abrupt> {
        let mut m = Value::Undefined;
        if is_async {
            if let Some(k) = crate::builtins::well_known_key(self, "asyncDispose") {
                m = self.get_member(value, &k)?;
            }
        }
        if matches!(m, Value::Undefined | Value::Null) {
            if let Some(k) = crate::builtins::well_known_key(self, "dispose") {
                m = self.get_member(value, &k)?;
            }
        }
        if !m.is_callable() {
            return Err(self.throw("TypeError", "value is not disposable"));
        }
        Ok(m)
    }

    /// Dispose a frame's resources in reverse order. An error thrown while disposing either becomes
    /// the completion (if it was previously normal) or is folded into a `SuppressedError` chain.
    pub(crate) fn dispose_frame(
        &mut self,
        mut frame: Vec<Disposable>,
        result: Completion,
    ) -> Completion {
        let mut completion = result;
        while let Some(r) = frame.pop() {
            match self.call(r.method.clone(), r.value.clone(), &[]) {
                Ok(_) => {}
                Err(Abrupt::Throw(new_err)) => {
                    completion = match completion {
                        Err(Abrupt::Throw(prev)) => {
                            Err(Abrupt::Throw(self.make_suppressed(new_err, prev)))
                        }
                        // Disposal throwing over a normal / return / break completion: the throw wins.
                        Ok(_) => Err(Abrupt::Throw(new_err)),
                        Err(other) => Err(other),
                    };
                }
                Err(other) => return Err(other),
            }
        }
        completion
    }

    /// Build a `SuppressedError(error, suppressed)`.
    fn make_suppressed(&mut self, error: Value, suppressed: Value) -> Value {
        let err = self.make_error("SuppressedError", "");
        if let Some(o) = err.as_obj() {
            o.borrow_mut().proto = self.error_protos.get("SuppressedError").cloned();
            o.borrow_mut()
                .props
                .insert("error", crate::value::Property::builtin(error));
            o.borrow_mut()
                .props
                .insert("suppressed", crate::value::Property::builtin(suppressed));
        }
        err
    }

    /// Pre-declare `let`/`const` (uninitialised — TDZ) and, when `with_functions`, block-level
    /// function declarations (initialised) for the statements directly in a block.
    pub fn declare_block_lexicals(&mut self, stmts: &[Stmt], scope: &Env, with_functions: bool) {
        for s in stmts {
            match crate::interpreter::unwrap_export(s) {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    decls,
                } => {
                    for (pat, _) in decls {
                        let mut names = Vec::new();
                        pattern_idents(pat, &mut names);
                        for name in names {
                            scope.borrow_mut().vars.insert(
                                name,
                                Binding {
                                    value: Value::Undefined,
                                    mutable: true,
                                    initialized: false,
                                    import_ref: None,
                                    deletable: false,
                                },
                            );
                        }
                    }
                }
                Stmt::FuncDecl(func) if with_functions => {
                    if let Some(name) = &func.name {
                        let f = self.make_function(func.clone(), scope.clone());
                        scope.borrow_mut().vars.insert(
                            name.clone(),
                            Binding {
                                value: f,
                                mutable: true,
                                initialized: true,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                }
                Stmt::ClassDecl(class) => {
                    if let Some(name) = &class.name {
                        // Classes are lexically scoped with a TDZ until the declaration executes.
                        scope.borrow_mut().vars.insert(
                            name.clone(),
                            Binding {
                                value: Value::Undefined,
                                mutable: true,
                                initialized: false,
                                import_ref: None,
                                deletable: false,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    pub fn exec_stmt(&mut self, stmt: &Stmt, env: &Env) -> Completion {
        match stmt {
            Stmt::Empty | Stmt::Debugger | Stmt::FuncDecl(_) => Ok(Value::Undefined),
            Stmt::Expr(e) => self.eval(e, env),
            Stmt::Block(body) => self.exec_block(body, env),
            Stmt::VarDecl { kind, decls } => {
                for (pat, init) in decls {
                    match kind {
                        DeclKind::Var => {
                            // `var x;` (no init) keeps the hoisted binding untouched.
                            if let Some(e) = init {
                                let value = self.eval(e, env)?;
                                if let Pattern::Ident(n) = pat {
                                    if is_anonymous_fn(e) {
                                        self.set_fn_name(&value, n);
                                    }
                                }
                                self.bind_pattern(pat, value, env, BindMode::Var)?;
                            }
                        }
                        DeclKind::Let | DeclKind::Const => {
                            let value = match init {
                                Some(e) => self.eval(e, env)?,
                                None => Value::Undefined,
                            };
                            if let (Pattern::Ident(n), Some(e)) = (pat, init) {
                                if is_anonymous_fn(e) {
                                    self.set_fn_name(&value, n);
                                }
                            }
                            self.bind_pattern(
                                pat,
                                value,
                                env,
                                BindMode::Lexical(*kind == DeclKind::Const),
                            )?;
                        }
                        DeclKind::Using | DeclKind::AwaitUsing => {
                            let is_async = *kind == DeclKind::AwaitUsing;
                            let value = match init {
                                Some(e) => self.eval(e, env)?,
                                None => Value::Undefined,
                            };
                            if let (Pattern::Ident(n), Some(e)) = (pat, init) {
                                if is_anonymous_fn(e) {
                                    self.set_fn_name(&value, n);
                                }
                            }
                            // Capture the dispose method now (TypeError if not disposable).
                            self.add_disposable(&value, is_async)?;
                            self.bind_pattern(pat, value, env, BindMode::Lexical(false))?;
                        }
                    }
                }
                Ok(Value::Undefined)
            }
            Stmt::Return(arg) => {
                let v = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                Err(Abrupt::Return(v))
            }
            Stmt::Throw(e) => {
                let v = self.eval(e, env)?;
                Err(Abrupt::Throw(v))
            }
            Stmt::If { test, cons, alt } => {
                let t = self.eval(test, env)?;
                if self.to_boolean(&t) {
                    self.exec_stmt(cons, env)
                } else if let Some(a) = alt {
                    self.exec_stmt(a, env)
                } else {
                    Ok(Value::Undefined)
                }
            }
            Stmt::While { test, body } => self.run_loop(None, env, |me, env| {
                let t = me.eval(test, env)?;
                if !me.to_boolean(&t) {
                    return Ok(LoopStep::Done(Value::Undefined));
                }
                let bv = me.exec_stmt(body, env)?;
                Ok(LoopStep::Continue(bv))
            }),
            Stmt::DoWhile { body, test } => {
                let mut first = true;
                self.run_loop(None, env, |me, env| {
                    if !first {
                        let t = me.eval(test, env)?;
                        if !me.to_boolean(&t) {
                            return Ok(LoopStep::Done(Value::Undefined));
                        }
                    }
                    first = false;
                    let bv = me.exec_stmt(body, env)?;
                    let t = me.eval(test, env)?;
                    if !me.to_boolean(&t) {
                        Ok(LoopStep::Done(bv))
                    } else {
                        Ok(LoopStep::Continue(bv))
                    }
                })
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.exec_for(init, test, update, body, env, None),
            Stmt::ForInOf {
                decl,
                left,
                right,
                of,
                is_await,
                body,
            } => self.exec_for_in_of(*decl, left, right, *of, *is_await, body, env, None),
            Stmt::Break(label) => Err(Abrupt::Break(label.clone())),
            Stmt::Continue(label) => Err(Abrupt::Continue(label.clone())),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => self.exec_try(block, handler, finalizer, env),
            Stmt::Switch { disc, cases } => self.exec_switch(disc, cases, env),
            Stmt::Labeled { label, body } => self.exec_labeled(label, body, env),
            Stmt::With { obj, body } => {
                let o = self.eval(obj, env)?;
                if matches!(o, Value::Undefined | Value::Null) {
                    return Err(
                        self.throw("TypeError", "Cannot convert undefined or null to object")
                    );
                }
                let with_env = crate::interpreter::new_with_scope(env.clone(), o);
                self.exec_stmt(body, &with_env)
            }
            Stmt::ClassDecl(class) => {
                let value = self.eval_class(class, env)?;
                if let Some(name) = &class.name {
                    self.init_lexical(name, value, false, env);
                }
                Ok(Value::Undefined)
            }
            // Module declarations: imports are resolved at link time (runtime no-op); exports run
            // their inner declaration (the export itself is link-time metadata).
            Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => {
                Ok(Value::Undefined)
            }
            Stmt::ExportDecl(inner) => self.exec_stmt(inner, env),
            Stmt::ExportDefault(inner) => match &**inner {
                Stmt::Expr(e) => {
                    let v = self.eval(e, env)?;
                    // NamedEvaluation: an anonymous function/class default export is named "default".
                    if is_anonymous_fn(e) {
                        self.set_fn_name(&v, "default");
                    }
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Undefined)
                }
                // `export default function(){}` / `class{}` with no name: the value is bound to the
                // synthetic `*default*` local (which the "default" export resolves to).
                Stmt::FuncDecl(f) if f.name.is_none() => {
                    let v = self.make_function(f.clone(), env.clone());
                    self.set_fn_name(&v, "default");
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Undefined)
                }
                Stmt::ClassDecl(c) if c.name.is_none() => {
                    let v = self.eval_class(c, env)?;
                    self.set_fn_name(&v, "default");
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Undefined)
                }
                other => self.exec_stmt(other, env),
            },
        }
    }

    fn exec_labeled(&mut self, label: &str, body: &Stmt, env: &Env) -> Completion {
        // For loops, push the label so labeled break/continue can target them.
        let result = match body {
            Stmt::For {
                init,
                test,
                update,
                body,
            } => self.exec_for(init, test, update, body, env, Some(label)),
            Stmt::ForInOf {
                decl,
                left,
                right,
                of,
                is_await,
                body,
            } => self.exec_for_in_of(*decl, left, right, *of, *is_await, body, env, Some(label)),
            Stmt::While { .. } | Stmt::DoWhile { .. } => self.exec_stmt(body, env),
            other => self.exec_stmt(other, env),
        };
        match result {
            Err(Abrupt::Break(Some(l))) if l == label => Ok(Value::Undefined),
            other => other,
        }
    }

    fn run_loop(
        &mut self,
        label: Option<&str>,
        env: &Env,
        mut step: impl FnMut(&mut Interp, &Env) -> Result<LoopStep, Abrupt>,
    ) -> Completion {
        // The loop's completion value: the most recent non-empty body completion (UpdateEmpty).
        let mut v = Value::Undefined;
        let keep = |bv: Value, v: &mut Value| {
            if !matches!(bv, Value::Undefined) {
                *v = bv;
            }
        };
        loop {
            // Safe point: run the cycle collector / enforce the live-object ceiling. Catches tight
            // loops (`for(;;){ x = {}; }`) that never call a function.
            self.gc_check()?;
            match step(self, env) {
                Ok(LoopStep::Continue(bv)) => keep(bv, &mut v),
                Ok(LoopStep::Done(bv)) => {
                    keep(bv, &mut v);
                    return Ok(v);
                }
                Err(Abrupt::Break(None)) => return Ok(v),
                Err(Abrupt::Break(Some(l))) if Some(l.as_str()) == label => return Ok(v),
                Err(Abrupt::Continue(None)) => {}
                Err(Abrupt::Continue(Some(l))) if Some(l.as_str()) == label => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn exec_for(
        &mut self,
        init: &Option<Box<ForInit>>,
        test: &Option<Expr>,
        update: &Option<Expr>,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Completion {
        let loop_env = new_scope(Some(env.clone()));
        if let Some(init) = init {
            match init.as_ref() {
                ForInit::Expr(e) => {
                    self.eval(e, &loop_env)?;
                }
                ForInit::VarDecl { kind, decls } => {
                    if matches!(kind, DeclKind::Let | DeclKind::Const) {
                        for (pat, _) in decls {
                            let mut names = Vec::new();
                            pattern_idents(pat, &mut names);
                            for name in names {
                                loop_env.borrow_mut().vars.insert(
                                    name,
                                    Binding {
                                        value: Value::Undefined,
                                        mutable: true,
                                        initialized: false,
                                        import_ref: None,
                                        deletable: false,
                                    },
                                );
                            }
                        }
                    }
                    let mode = match kind {
                        DeclKind::Var => BindMode::Var,
                        k => BindMode::Lexical(*k == DeclKind::Const),
                    };
                    for (pat, e) in decls {
                        let v = match e {
                            Some(e) => self.eval(e, &loop_env)?,
                            None => Value::Undefined,
                        };
                        self.bind_pattern(pat, v, &loop_env, mode)?;
                    }
                }
            }
        }
        let mut first = true;
        self.run_loop(label, &loop_env, |me, env| {
            if !first {
                if let Some(u) = update {
                    me.eval(u, env)?;
                }
            }
            first = false;
            if let Some(t) = test {
                let tv = me.eval(t, env)?;
                if !me.to_boolean(&tv) {
                    return Ok(LoopStep::Done(Value::Undefined));
                }
            }
            let bv = me.exec_stmt(body, env)?;
            Ok(LoopStep::Continue(bv))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_for_in_of(
        &mut self,
        decl: Option<DeclKind>,
        left: &Pattern,
        right: &Expr,
        of: bool,
        is_await: bool,
        body: &Stmt,
        env: &Env,
        label: Option<&str>,
    ) -> Completion {
        let rhs = self.eval(right, env)?;
        // No-decl form assigns to an existing binding; a declaration creates a fresh one per round.
        let mode = match decl {
            Some(DeclKind::Var) | None => BindMode::Var,
            Some(k) => BindMode::Lexical(k == DeclKind::Const),
        };
        if of && is_await {
            // `for await (x of asyncIterable)`: drive the @@asyncIterator (or a sync iterator over
            // promises), awaiting each `next()` result and value.
            let akey = crate::builtins::async_iterator_key(self);
            let method = match &akey {
                Some(k) => self.get_member(&rhs, k)?,
                None => Value::Undefined,
            };
            let iter = if method.is_callable() {
                self.call(method, rhs.clone(), &[])?
            } else {
                self.get_iterator(&rhs)?.0
            };
            let next = self.get_member(&iter, "next")?;
            return self.run_loop(label, env, |me, env| {
                let res = me.call(next.clone(), iter.clone(), &[])?;
                let res = me.await_value(res)?;
                let done = me.get_member(&res, "done")?;
                if me.to_boolean(&done) {
                    return Ok(LoopStep::Done(Value::Undefined));
                }
                let raw = me.get_member(&res, "value")?;
                let v = me.await_value(raw)?;
                let iter_env = new_scope(Some(env.clone()));
                me.bind_pattern(left, v, &iter_env, mode)?;
                let bv = me.exec_stmt(body, &iter_env)?;
                Ok(LoopStep::Continue(bv))
            });
        }
        if of {
            // Step the iterator lazily; close it if the loop exits early (break/return/throw).
            let (iter, next) = self.get_iterator(&rhs)?;
            let iter_close = iter.clone();
            let mut exhausted = false;
            let result = self.run_loop(label, env, |me, env| {
                let v = match me.iterator_step(&iter, &next)? {
                    Some(x) => x,
                    None => {
                        exhausted = true;
                        return Ok(LoopStep::Done(Value::Undefined));
                    }
                };
                let iter_env = new_scope(Some(env.clone()));
                me.bind_pattern(left, v, &iter_env, mode)?;
                let bv = me.exec_stmt(body, &iter_env)?;
                Ok(LoopStep::Continue(bv))
            });
            if !exhausted {
                self.iterator_close(&iter_close);
            }
            return result;
        }
        // for-in: enumerable string keys along the prototype chain (own first, deduped).
        // A module namespace's [[GetOwnProperty]] runs during enumeration, so an uninitialized export
        // makes the loop throw ReferenceError before any iteration.
        if let Value::Obj(o) = &rhs {
            let ptr = std::rc::Rc::as_ptr(o) as usize;
            if self.is_namespace(ptr) {
                for k in self.enum_keys(&rhs)? {
                    if let Some(res) = self.namespace_own_property(ptr, &k) {
                        res?;
                    }
                }
            }
        }
        let items: Vec<Value> = self
            .enum_keys(&rhs)?
            .into_iter()
            .map(Value::from_string)
            .collect();
        let mut idx = 0;
        self.run_loop(label, env, |me, env| {
            if idx >= items.len() {
                return Ok(LoopStep::Done(Value::Undefined));
            }
            let v = items[idx].clone();
            idx += 1;
            let iter_env = new_scope(Some(env.clone()));
            me.bind_pattern(left, v, &iter_env, mode)?;
            let bv = me.exec_stmt(body, &iter_env)?;
            Ok(LoopStep::Continue(bv))
        })
    }

    /// `yield value`: park the coroutine, then resume per the driver's signal.
    fn yield_one(&mut self, value: Value) -> Completion {
        match crate::coroutine::coroutine_yield(self, value) {
            crate::coroutine::Resume::Next(v) => Ok(v),
            crate::coroutine::Resume::Return(v) => Err(Abrupt::Return(v)),
            crate::coroutine::Resume::Throw(e) => Err(Abrupt::Throw(e)),
        }
    }

    /// `yield* iterable`: delegate to the inner iterator, forwarding next/return/throw (14.4.14).
    fn yield_delegate(&mut self, value: &Value) -> Completion {
        use crate::coroutine::Resume;
        let (iterator, next) = self.get_iterator(value)?;
        let mut received = Resume::Next(Value::Undefined);
        loop {
            match received {
                Resume::Next(v) => {
                    let result = self.call(next.clone(), iterator.clone(), &[v])?;
                    let done = self.get_member(&result, "done")?;
                    if self.to_boolean(&done) {
                        return self.get_member(&result, "value");
                    }
                    let inner = self.get_member(&result, "value")?;
                    received = crate::coroutine::coroutine_yield(self, inner);
                }
                Resume::Throw(e) => {
                    let throw = self.get_member(&iterator, "throw")?;
                    if !throw.is_callable() {
                        self.iterator_close(&iterator);
                        return Err(
                            self.throw("TypeError", "the delegated iterator has no 'throw' method")
                        );
                    }
                    let result = self.call(throw, iterator.clone(), &[e])?;
                    let done = self.get_member(&result, "done")?;
                    if self.to_boolean(&done) {
                        return self.get_member(&result, "value");
                    }
                    let inner = self.get_member(&result, "value")?;
                    received = crate::coroutine::coroutine_yield(self, inner);
                }
                Resume::Return(v) => {
                    let ret = self.get_member(&iterator, "return")?;
                    if !ret.is_callable() {
                        return Err(Abrupt::Return(v));
                    }
                    let result = self.call(ret, iterator.clone(), &[v])?;
                    let done = self.get_member(&result, "done")?;
                    if self.to_boolean(&done) {
                        let v = self.get_member(&result, "value")?;
                        return Err(Abrupt::Return(v));
                    }
                    let inner = self.get_member(&result, "value")?;
                    received = crate::coroutine::coroutine_yield(self, inner);
                }
            }
        }
    }

    /// GetIterator: returns (iterator, next-method).
    pub(crate) fn get_iterator(&mut self, v: &Value) -> Result<(Value, Value), Abrupt> {
        // A primitive string iterates by code point (it has no own @@iterator method here).
        if let Value::Str(s) = v {
            let chars: Vec<Value> = s
                .chars()
                .map(|c| Value::from_string(c.to_string()))
                .collect();
            let arr = self.make_array(chars);
            return self.get_iterator(&arr);
        }
        let key = match &self.iterator_sym {
            Some(s) => Interp::sym_key(s),
            None => return Err(self.throw("TypeError", "no iterator symbol")),
        };
        let itfn = self.get_member(v, &key)?;
        if !itfn.is_callable() {
            return Err(self.throw("TypeError", "value is not iterable"));
        }
        let iter = self.call(itfn, v.clone(), &[])?;
        let next = self.get_member(&iter, "next")?;
        if !next.is_callable() {
            return Err(self.throw("TypeError", "iterator.next is not a function"));
        }
        Ok((iter, next))
    }
    /// IteratorStep: `Some(value)` or `None` when done.
    pub(crate) fn iterator_step(
        &mut self,
        iter: &Value,
        next: &Value,
    ) -> Result<Option<Value>, Abrupt> {
        let res = self.call(next.clone(), iter.clone(), &[])?;
        if !matches!(res, Value::Obj(_)) {
            return Err(self.throw("TypeError", "iterator result is not an object"));
        }
        let done = self.get_member(&res, "done")?;
        if self.to_boolean(&done) {
            Ok(None)
        } else {
            Ok(Some(self.get_member(&res, "value")?))
        }
    }
    /// IteratorClose: call `return()` if present (swallowing its result/most errors).
    pub(crate) fn iterator_close(&mut self, iter: &Value) {
        if let Ok(ret) = self.get_member(iter, "return") {
            if ret.is_callable() {
                let _ = self.call(ret, iter.clone(), &[]);
            }
        }
    }

    /// IteratorClose for a *normal* completion: errors from reading or calling `return` propagate
    /// (unlike the throw-completion `iterator_close`, which swallows them).
    pub(crate) fn iterator_close_normal(&mut self, iter: &Value) -> Result<(), Abrupt> {
        let ret = self.get_member(iter, "return")?;
        if !matches!(ret, Value::Undefined | Value::Null) {
            if !ret.is_callable() {
                return Err(self.throw("TypeError", "iterator 'return' is not callable"));
            }
            let r = self.call(ret, iter.clone(), &[])?;
            if !matches!(r, Value::Obj(_)) {
                return Err(self.throw("TypeError", "iterator 'return' must return an object"));
            }
        }
        Ok(())
    }

    /// Collect every value an iterable yields. Strings and plain arrays use a fast path; everything
    /// else goes through the `Symbol.iterator` protocol (call `@@iterator`, then drain `.next()`).
    pub(crate) fn iterate(&mut self, v: &Value) -> Result<Vec<Value>, Abrupt> {
        match v {
            Value::Str(s) => {
                return Ok(s
                    .chars()
                    .map(|c| Value::from_string(c.to_string()))
                    .collect())
            }
            Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array) => {
                let len = self.checked_array_len(o)?;
                let mut out = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    out.push(self.get_member(v, &i.to_string())?);
                }
                return Ok(out);
            }
            _ => {}
        }
        // General iterator protocol.
        if let Some(sym) = self.iterator_sym.clone() {
            let key = Interp::sym_key(&sym);
            let itfn = self.get_member(v, &key)?;
            if itfn.is_callable() {
                return self.iterate_with(v, itfn);
            }
        }
        Err(self.throw("TypeError", "value is not iterable"))
    }

    /// Drive the iterator protocol on `v` using an already-resolved `@@iterator` method `itfn`
    /// (so callers that fetched it via GetMethod don't re-read the property).
    pub(crate) fn iterate_with(&mut self, v: &Value, itfn: Value) -> Result<Vec<Value>, Abrupt> {
        let iter = self.call(itfn, v.clone(), &[])?;
        let next = self.get_member(&iter, "next")?;
        if !next.is_callable() {
            return Err(self.throw("TypeError", "iterator.next is not a function"));
        }
        let mut out = Vec::new();
        loop {
            let res = self.call(next.clone(), iter.clone(), &[])?;
            if !matches!(res, Value::Obj(_)) {
                return Err(self.throw("TypeError", "iterator result is not an object"));
            }
            let done = self.get_member(&res, "done")?;
            if self.to_boolean(&done) {
                break;
            }
            out.push(self.get_member(&res, "value")?);
            if out.len() > crate::interpreter::MAX_ARRAY_OP_LEN {
                return Err(self.throw("RangeError", "iterator produced too many values"));
            }
        }
        Ok(out)
    }

    /// Whether `v` is iterable (has a callable `@@iterator`). Used by `Array.from` to choose between
    /// the iterator protocol and the array-like fallback.
    pub(crate) fn has_iterator(&mut self, v: &Value) -> bool {
        if let Some(sym) = self.iterator_sym.clone() {
            let key = Interp::sym_key(&sym);
            if let Ok(f) = self.get_member(v, &key) {
                return f.is_callable();
            }
        }
        false
    }

    fn enum_keys(&mut self, v: &Value) -> Result<Vec<String>, Abrupt> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut cur = match v {
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        };
        while let Some(o) = cur {
            let ov = Value::Obj(o.clone());
            // A proxy level enumerates via its [[OwnPropertyKeys]] filtered by [[GetOwnProperty]]'s
            // enumerable flag, then walks its [[GetPrototypeOf]].
            if self.proxies.contains_key(&(Rc::as_ptr(&o) as usize)) {
                let keys =
                    crate::builtins::proxy_enum_string_keys(self, &ov).map_err(Abrupt::Throw)?;
                for k in keys {
                    if let Value::Str(ks) = k {
                        if seen.insert(ks.to_string()) {
                            out.push(ks.to_string());
                        }
                    }
                }
                let parent =
                    crate::builtins::js_get_prototype_of(self, &ov).map_err(Abrupt::Throw)?;
                cur = match parent {
                    Value::Obj(p) => Some(p),
                    _ => None,
                };
                continue;
            }
            // for-in visits own enumerable string keys in spec order, then up the prototype chain.
            let (level, parent) = {
                let b = o.borrow();
                let level: Vec<(String, bool)> = b
                    .props
                    .ordered_keys()
                    .into_iter()
                    .filter(|k| !Interp::is_sym_key(k))
                    .map(|k| {
                        let e = b.props.get(&k).map(|p| p.enumerable).unwrap_or(false);
                        (k.to_string(), e)
                    })
                    .collect();
                (level, b.proto.clone())
            };
            for (k, enumerable) in level {
                if enumerable && seen.insert(k.clone()) {
                    out.push(k);
                }
            }
            cur = parent;
        }
        Ok(out)
    }

    fn exec_try(
        &mut self,
        block: &[Stmt],
        handler: &Option<(Option<Pattern>, Vec<Stmt>)>,
        finalizer: &Option<Vec<Stmt>>,
        env: &Env,
    ) -> Completion {
        let result = self.exec_block(block, env);
        let after_catch = match result {
            Err(Abrupt::Throw(ex)) => {
                if let Some((param, body)) = handler {
                    // The catch parameter lives in its own environment (flagged so a sloppy `eval`'s
                    // var-hoisting walk skips it); the body's lexicals + statements run in a child
                    // block environment.
                    let body_env = if let Some(pat) = param {
                        let catch_env = new_catch_scope(env.clone());
                        self.bind_pattern(pat, ex, &catch_env, BindMode::Lexical(false))?;
                        new_scope(Some(catch_env))
                    } else {
                        new_scope(Some(env.clone()))
                    };
                    let mut last = Ok(Value::Undefined);
                    self.declare_block_lexicals(body, &body_env, true);
                    for s in body {
                        match self.exec_stmt(s, &body_env) {
                            Ok(v) => last = Ok(v),
                            Err(e) => {
                                last = Err(e);
                                break;
                            }
                        }
                    }
                    last
                } else {
                    Err(Abrupt::Throw(ex))
                }
            }
            other => other,
        };
        if let Some(fin) = finalizer {
            // An abrupt completion in `finally` overrides the try/catch completion; its normal
            // value is discarded (the try/catch completion stands).
            self.exec_block(fin, env)?;
        }
        after_catch
    }

    fn exec_switch(&mut self, disc: &Expr, cases: &[SwitchCase], env: &Env) -> Completion {
        let d = self.eval(disc, env)?;
        let scope = new_scope(Some(env.clone()));
        for case in cases {
            for s in &case.body {
                self.declare_block_lexicals(std::slice::from_ref(s), &scope, true);
            }
        }
        let mut matched = None;
        for (i, case) in cases.iter().enumerate() {
            if let Some(test) = &case.test {
                let t = self.eval(test, &scope)?;
                if self.strict_equals(&d, &t) {
                    matched = Some(i);
                    break;
                }
            }
        }
        let start = match matched.or_else(|| cases.iter().position(|c| c.test.is_none())) {
            Some(i) => i,
            None => return Ok(Value::Undefined),
        };
        let mut last = Value::Undefined;
        for case in &cases[start..] {
            for s in &case.body {
                match self.exec_stmt(s, &scope) {
                    Ok(v) => last = v,
                    Err(Abrupt::Break(None)) => return Ok(last),
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(last)
    }

    // ----- variable binding -------------------------------------------------------------------

    fn init_lexical(&mut self, name: &str, value: Value, is_const: bool, env: &Env) {
        env.borrow_mut().vars.insert(
            name.to_string(),
            Binding {
                value,
                mutable: !is_const,
                initialized: true,
                import_ref: None,
                deletable: false,
            },
        );
    }

    /// Whether `name` resolves to a declared-but-uninitialized (TDZ) lexical binding — following a
    /// live module import through to the exporter's binding (so `typeof importedName` throws when the
    /// exporter has not yet initialized it).
    fn binding_in_tdz(&self, name: &str, env: &Env) -> bool {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let (import_ref, uninit, found) = {
                let b = s.borrow();
                match b.vars.get(name) {
                    Some(binding) => (binding.import_ref.clone(), !binding.initialized, true),
                    None => (None, false, false),
                }
            };
            if found {
                if let Some((src_env, local)) = import_ref {
                    return self.binding_in_tdz(&local, &src_env);
                }
                return uninit;
            }
            cur = s.borrow().parent.clone();
        }
        false
    }

    pub fn get_var(&mut self, name: &str, env: &Env) -> Result<Value, Abrupt> {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let (with_obj, parent) = {
                let b = s.borrow();
                if let Some(binding) = b.vars.get(name) {
                    if !binding.initialized {
                        return Err(self.throw(
                            "ReferenceError",
                            format!("cannot access '{name}' before initialization"),
                        ));
                    }
                    // A live module import reads through to the exporter's binding.
                    if let Some((src_env, local)) = &binding.import_ref {
                        let (src_env, local) = (src_env.clone(), local.clone());
                        drop(b);
                        return self.get_var(&local, &src_env);
                    }
                    return Ok(binding.value.clone());
                }
                (b.with_obj.clone(), b.parent.clone())
            };
            // `with (obj)`: resolve against the object's properties if it has the name (the proxy
            // `has` trap participates here).
            if let Some(obj @ Value::Obj(_)) = &with_obj {
                if self.js_has_property(obj, name)? {
                    return self.get_member(obj, name);
                }
            }
            cur = parent;
        }
        // Fall back to a property of the global object (where builtins live).
        let g = Value::Obj(self.global.clone());
        if self.has_property(&self.global.clone(), name) {
            return self.get_member(&g, name);
        }
        Err(self.throw("ReferenceError", format!("{name} is not defined")))
    }

    /// Walk the scope chain for an initialized binding without the `with`/global fallback or the
    /// not-defined throw. Used for internal `%...%` markers that only ever live in scopes.
    /// Whether `env` is inside an ordinary (non-arrow) function, which is what supplies `new.target`.
    /// Non-arrow functions bind `this` in their scope; arrows do not (they inherit it), so an arrow
    /// at the top level is *not* function code for `new.target` purposes. Governs `new.target`
    /// validity in a direct eval.
    fn in_function_code(&self, env: &Env) -> bool {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            if Rc::ptr_eq(&s, &self.global_env) {
                return false;
            }
            let (has_this, parent) = {
                let b = s.borrow();
                (b.vars.contains_key("this"), b.parent.clone())
            };
            if has_this {
                return true;
            }
            cur = parent;
        }
        false
    }

    fn peek_binding(&self, name: &str, env: &Env) -> Option<Value> {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let b = s.borrow();
            if let Some(binding) = b.vars.get(name) {
                return Some(binding.value.clone());
            }
            cur = b.parent.clone();
        }
        None
    }

    /// The object `super.x` reads/writes through: `GetPrototypeOf([[HomeObject]])` when a home
    /// object is in scope (object-literal & class methods bound via `%homeobject%`), else the
    /// statically-resolved `%superproto%` carried by older class-method environments.
    fn super_base(&mut self, env: &Env) -> Result<Value, Abrupt> {
        if let Some(Value::Obj(home)) = self.peek_binding("%homeobject%", env) {
            return Ok(match home.borrow().proto.clone() {
                Some(p) => Value::Obj(p),
                None => Value::Null,
            });
        }
        // No home object in scope (e.g. `super.x` reached through an `eval` in a non-method): a
        // super property reference here is an early SyntaxError.
        match self.peek_binding("%superproto%", env) {
            Some(v) => Ok(v),
            None => Err(self.throw("SyntaxError", "'super' keyword unexpected here")),
        }
    }

    pub fn assign_var(&mut self, name: &str, value: Value, env: &Env) -> Result<(), Abrupt> {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let (with_obj, parent) = {
                let mut b = s.borrow_mut();
                if let Some(binding) = b.vars.get_mut(name) {
                    if !binding.mutable && binding.initialized {
                        return Err(
                            self.throw("TypeError", format!("assignment to constant '{name}'"))
                        );
                    }
                    binding.value = value;
                    binding.initialized = true;
                    return Ok(());
                }
                (b.with_obj.clone(), b.parent.clone())
            };
            if let Some(obj @ Value::Obj(_)) = &with_obj {
                if self.js_has_property(obj, name)? {
                    return self.set_member(obj, name, value);
                }
            }
            cur = parent;
        }
        // A declared global `var`/`function` lives as a property of the global object.
        if self.has_property(&self.global.clone(), name) {
            return self.set_member(&Value::Obj(self.global.clone()), name, value);
        }
        // Truly undeclared: strict → ReferenceError; sloppy → create a global property.
        if self.strict {
            return Err(self.throw("ReferenceError", format!("{name} is not defined")));
        }
        let g = Value::Obj(self.global.clone());
        self.set_member(&g, name, value)
    }

    /// CreateListFromArrayLike: read `obj.length` then elements `0..length` into a Vec (used by the
    /// Proxy `ownKeys` trap, which returns an array-like, not an iterable).
    pub(crate) fn create_list_from_arraylike(&mut self, obj: &Value) -> Result<Vec<Value>, Abrupt> {
        let len_v = self.get_member(obj, "length")?;
        let len = self.to_number(&len_v)?;
        let len = if len.is_nan() || len < 0.0 {
            0
        } else {
            len as usize
        };
        let mut out = Vec::with_capacity(len.min(1024));
        for k in 0..len {
            out.push(self.get_member(obj, &k.to_string())?);
        }
        Ok(out)
    }

    /// `[[HasProperty]]`: the trap-aware `in` check. Handles a proxy anywhere on the chain (its `has`
    /// trap, or forwarding to the target's own `[[HasProperty]]`), TypedArray index slots, then the
    /// ordinary own-property + prototype walk.
    pub(crate) fn js_has_property(&mut self, obj: &Value, key: &str) -> Result<bool, Abrupt> {
        let o = match obj {
            Value::Obj(o) => o.clone(),
            _ => return Ok(false),
        };
        let ptr = Rc::as_ptr(&o) as usize;
        if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
            if matches!(handler, Value::Null) {
                return Err(self.throw("TypeError", "cannot perform 'has' on a revoked proxy"));
            }
            let trap = self.get_member(&handler, "has")?;
            if matches!(trap, Value::Undefined | Value::Null) {
                return self.js_has_property(&target, key);
            }
            if !trap.is_callable() {
                return Err(self.throw("TypeError", "proxy 'has' trap is not callable"));
            }
            let res = self.call(
                trap,
                handler,
                &[target.clone(), Value::from_string(key.to_string())],
            )?;
            let present = self.to_boolean(&res);
            if !present {
                if let Value::Obj(t) = &target {
                    let p = t.borrow().props.get(key).cloned();
                    if let Some(p) = p {
                        if !p.configurable || !t.borrow().extensible {
                            return Err(self.throw(
                                "TypeError",
                                "proxy 'has' trap hid a non-configurable property",
                            ));
                        }
                    }
                }
            }
            return Ok(present);
        }
        if let Some(info) = self.typed_arrays.get(&ptr).copied() {
            match self.ta_index_kind(&info, key) {
                TaIndex::Element(_) => return Ok(true),
                TaIndex::Exotic => return Ok(false),
                TaIndex::Ordinary => {}
            }
        }
        if o.borrow().props.contains(key) {
            return Ok(true);
        }
        let proto = o.borrow().proto.clone();
        match proto {
            Some(p) => self.js_has_property(&Value::Obj(p), key),
            None => Ok(false),
        }
    }

    pub(crate) fn has_property(&self, obj: &Gc, key: &str) -> bool {
        // A TypedArray's [[HasProperty]] resolves integer-index slots itself and never consults the
        // prototype for a canonical-numeric key (valid index → present; otherwise absent).
        if let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(obj) as usize)).copied() {
            match self.ta_index_kind(&info, key) {
                TaIndex::Element(_) => return true,
                TaIndex::Exotic => return false,
                TaIndex::Ordinary => {}
            }
        }
        let mut cur = Some(obj.clone());
        while let Some(o) = cur {
            if o.borrow().props.contains(key) {
                return true;
            }
            cur = o.borrow().proto.clone();
        }
        false
    }

    // ----- expressions ------------------------------------------------------------------------

    pub fn eval(&mut self, expr: &Expr, env: &Env) -> Result<Value, Abrupt> {
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::BigInt(n) => Ok(Value::BigInt(*n)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::ToStr(inner) => {
                let v = self.eval(inner, env)?;
                Ok(Value::Str(self.to_string(&v)?))
            }
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => self.get_var(name, env),
            Expr::This => self.get_var("this", env).or(Ok(Value::Undefined)),
            Expr::Regex { body, flags } => self.make_regexp(body, flags),
            Expr::Array(elems) => self.eval_array(elems, env),
            Expr::Object(props) => self.eval_object(props, env),
            Expr::Func(func) => Ok(self.make_function(func.clone(), env.clone())),
            Expr::Class(class) => self.eval_class(class, env),
            Expr::Yield { delegate, arg } => {
                let value = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                if !crate::coroutine::in_coroutine() {
                    return Err(self.throw("SyntaxError", "yield outside a generator"));
                }
                if *delegate {
                    self.yield_delegate(&value)
                } else {
                    self.yield_one(value)
                }
            }
            Expr::Await(e) => {
                let v = self.eval(e, env)?;
                if crate::coroutine::in_coroutine() {
                    match crate::coroutine::coroutine_await(self, v) {
                        crate::coroutine::Resume::Next(settled) => Ok(settled),
                        crate::coroutine::Resume::Throw(err) => Err(Abrupt::Throw(err)),
                        crate::coroutine::Resume::Return(rv) => Err(Abrupt::Return(rv)),
                    }
                } else {
                    self.await_value(v)
                }
            }
            Expr::Super => Err(self.throw("SyntaxError", "'super' keyword unexpected here")),
            Expr::Seq(items) => {
                let mut last = Value::Undefined;
                for e in items {
                    last = self.eval(e, env)?;
                }
                Ok(last)
            }
            Expr::Cond { test, cons, alt } => {
                let t = self.eval(test, env)?;
                if self.to_boolean(&t) {
                    self.eval(cons, env)
                } else {
                    self.eval(alt, env)
                }
            }
            Expr::Logical { op, left, right } => {
                let l = self.eval(left, env)?;
                match *op {
                    "&&" => {
                        if self.to_boolean(&l) {
                            self.eval(right, env)
                        } else {
                            Ok(l)
                        }
                    }
                    "||" => {
                        if self.to_boolean(&l) {
                            Ok(l)
                        } else {
                            self.eval(right, env)
                        }
                    }
                    "??" => {
                        if matches!(l, Value::Undefined | Value::Null) {
                            self.eval(right, env)
                        } else {
                            Ok(l)
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Expr::Unary { op, arg } => self.eval_unary(op, arg, env),
            Expr::Update { op, prefix, arg } => self.eval_update(op, *prefix, arg, env),
            Expr::Binary { op, left, right } => {
                let l = self.eval(left, env)?;
                let r = self.eval(right, env)?;
                self.binary(op, l, r)
            }
            Expr::Assign { op, target, value } => self.eval_assign(op, target, value, env),
            Expr::ImportMeta => Ok(self.import_meta.clone().unwrap_or(Value::Undefined)),
            Expr::NewTarget => Ok(self.new_target.clone()),
            Expr::ImportCall { spec, phase } => {
                let specifier = self.eval(spec, env)?;
                // ToString abruptness rejects the promise (IfAbruptRejectPromise), not a sync throw.
                let s = match self.to_string(&specifier) {
                    Ok(s) => s,
                    Err(e) => {
                        let p = self.new_promise();
                        let reason = crate::interpreter::abrupt_value(e);
                        self.reject_promise(&p, reason);
                        return Ok(p);
                    }
                };
                match phase {
                    // A Source Text Module Record's GetModuleSource always throws a SyntaxError, so
                    // `import.source(x)` rejects once the specifier has been coerced.
                    ImportPhase::Source => {
                        let p = self.new_promise();
                        let reason = crate::interpreter::abrupt_value(
                            self.throw("SyntaxError", "source phase import is not available"),
                        );
                        self.reject_promise(&p, reason);
                        Ok(p)
                    }
                    // `import.defer(x)` defers evaluation of the module; for specifier handling it
                    // behaves like a plain dynamic import.
                    ImportPhase::Evaluation | ImportPhase::Defer => Ok(self.dynamic_import(&s)),
                }
            }
            Expr::PrivateIn { name, obj } => {
                let o = self.eval(obj, env)?;
                match o {
                    // Private fields are own props; private methods live on the prototype.
                    Value::Obj(obj) => Ok(Value::Bool(self.has_property(&obj, name))),
                    _ => {
                        Err(self
                            .throw("TypeError", "the right-hand side of 'in' must be an object"))
                    }
                }
            }
            Expr::OptionalChain(inner) => {
                let saved = self.short_circuit;
                self.short_circuit = false;
                let v = self.eval(inner, env);
                let short = self.short_circuit;
                self.short_circuit = saved;
                if short {
                    Ok(Value::Undefined)
                } else {
                    v
                }
            }
            Expr::Member {
                obj,
                prop,
                optional,
            } => {
                if matches!(**obj, Expr::Super) {
                    let home = self.super_base(env)?;
                    return self.get_member(&home, prop);
                }
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined); // an earlier `?.` link short-circuited
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                self.get_member(&base, prop)
            }
            Expr::Index {
                obj,
                index,
                optional,
            } => {
                if matches!(**obj, Expr::Super) {
                    let home = self.super_base(env)?;
                    let idx = self.eval(index, env)?;
                    let key = self.to_property_key(&idx)?;
                    return self.get_member(&home, &key);
                }
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                self.get_member(&base, &key)
            }
            Expr::Call {
                callee,
                args,
                optional,
            } => self.eval_call(callee, args, *optional, env),
            Expr::TaggedTemplate { tag, quasis, subs } => {
                self.eval_tagged_template(tag, quasis, subs, env)
            }
            Expr::New { callee, args } => {
                let c = self.eval(callee, env)?;
                let argv = self.eval_args(args, env)?;
                self.construct(c, &argv)
            }
        }
    }

    fn eval_array(&mut self, elems: &[ArrayElem], env: &Env) -> Result<Value, Abrupt> {
        // Holes leave the index absent (a real elision), not `undefined`.
        let arr = self.make_array(Vec::new());
        let mut idx: usize = 0;
        for e in elems {
            match e {
                ArrayElem::Item(e) => {
                    let v = self.eval(e, env)?;
                    self.set_member(&arr, &idx.to_string(), v)?;
                    idx += 1;
                }
                ArrayElem::Hole => idx += 1,
                ArrayElem::Spread(e) => {
                    let v = self.eval(e, env)?;
                    for item in self.iterate(&v)? {
                        self.set_member(&arr, &idx.to_string(), item)?;
                        idx += 1;
                    }
                }
            }
        }
        self.set_member(&arr, "length", Value::Num(idx as f64))?;
        Ok(arr)
    }

    /// NamedEvaluation: give an anonymous function/class the binding/property name it's assigned to,
    /// unless it already has a non-empty name.
    pub(crate) fn set_fn_name(&mut self, v: &Value, name: &str) {
        if !v.is_callable() {
            return;
        }
        if let Value::Obj(o) = v {
            let empty = o
                .borrow()
                .props
                .get("name")
                .map(|p| matches!(&p.value, Value::Str(s) if s.is_empty()))
                .unwrap_or(true);
            if empty {
                o.borrow_mut().props.insert(
                    "name".to_string(),
                    Property::data(Value::from_string(name.to_string()), false, false, true),
                );
            }
        }
    }

    fn eval_object(&mut self, props: &[PropDef], env: &Env) -> Result<Value, Abrupt> {
        let obj = self.new_object();
        // Methods/getters/setters carry a [[HomeObject]] (the literal itself) so `super.x` resolves
        // against the object's *current* prototype, evaluated dynamically at access time.
        let home_env = new_scope(Some(env.clone()));
        bind(&home_env, "%homeobject%", Value::Obj(obj.clone()));
        for prop in props {
            match prop {
                PropDef::KeyValue { key, value } => {
                    let k = self.eval_prop_key(key, env)?;
                    let v = self.eval(value, env)?;
                    if is_anonymous_fn(value) {
                        self.set_fn_name(&v, &k);
                    }
                    obj.borrow_mut().props.insert(k, Property::plain(v));
                }
                PropDef::Method { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), home_env.clone());
                    self.set_fn_name(&f, &k);
                    obj.borrow_mut().props.insert(k, Property::plain(f));
                }
                PropDef::Getter { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), home_env.clone());
                    self.set_fn_name(&f, &format!("get {k}"));
                    self.define_accessor(&obj, &k, Some(f), None);
                }
                PropDef::Setter { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), home_env.clone());
                    self.set_fn_name(&f, &format!("set {k}"));
                    self.define_accessor(&obj, &k, None, Some(f));
                }
                PropDef::Spread(e) => {
                    let v = self.eval(e, env)?;
                    if let Value::Obj(src) = &v {
                        let keys: Vec<Rc<str>> = {
                            let b = src.borrow();
                            b.props
                                .ordered_keys()
                                .into_iter()
                                .filter(|k| b.props.get(k).map(|p| p.enumerable).unwrap_or(false))
                                .collect()
                        };
                        for k in keys {
                            let pv = self.get_member(&v, &k)?;
                            obj.borrow_mut().props.insert(k, Property::plain(pv));
                        }
                    }
                }
            }
        }
        Ok(Value::Obj(obj))
    }

    fn define_accessor(&self, obj: &Gc, key: &str, get: Option<Value>, set: Option<Value>) {
        let mut b = obj.borrow_mut();
        if let Some(p) = b.props.get_mut(key) {
            if p.accessor {
                if get.is_some() {
                    p.get = get;
                }
                if set.is_some() {
                    p.set = set;
                }
                return;
            }
        }
        b.props.insert(
            key,
            Property {
                value: Value::Undefined,
                get,
                set,
                accessor: true,
                writable: false,
                enumerable: true,
                configurable: true,
            },
        );
    }

    fn eval_prop_key(&mut self, key: &PropKey, env: &Env) -> Result<String, Abrupt> {
        match key {
            PropKey::Ident(s) => Ok(s.clone()),
            PropKey::Str(s) => Ok(s.to_string()),
            PropKey::Num(n) => Ok(self.num_to_str(*n)),
            PropKey::Computed(e) => {
                let v = self.eval(e, env)?;
                self.to_property_key(&v)
            }
        }
    }

    fn eval_args(&mut self, args: &[ArrayElem], env: &Env) -> Result<Vec<Value>, Abrupt> {
        let mut out = Vec::new();
        for a in args {
            match a {
                ArrayElem::Item(e) => out.push(self.eval(e, env)?),
                ArrayElem::Spread(e) => {
                    let v = self.eval(e, env)?;
                    out.extend(self.iterate(&v)?);
                }
                ArrayElem::Hole => out.push(Value::Undefined),
            }
        }
        Ok(out)
    }

    /// Freeze an object in place (non-extensible, all own props non-writable/non-configurable).
    pub(crate) fn freeze_object(&self, v: &Value) {
        if let Value::Obj(o) = v {
            o.borrow_mut().extensible = false;
            let keys = o.borrow().props.keys();
            for k in keys {
                if let Some(p) = o.borrow_mut().props.get_mut(&k) {
                    p.writable = false;
                    p.configurable = false;
                }
            }
        }
    }

    fn eval_tagged_template(
        &mut self,
        tag: &Expr,
        quasis: &[(Option<String>, String)],
        subs: &[Expr],
        env: &Env,
    ) -> Result<Value, Abrupt> {
        // The template object: a frozen array of cooked strings with a frozen `.raw` array.
        let cooked: Vec<Value> = quasis
            .iter()
            .map(|(c, _)| {
                c.as_ref()
                    .map(|s| Value::from_string(s.clone()))
                    .unwrap_or(Value::Undefined)
            })
            .collect();
        let raw: Vec<Value> = quasis
            .iter()
            .map(|(_, r)| Value::from_string(r.clone()))
            .collect();
        let strings = self.make_array(cooked);
        let raw_arr = self.make_array(raw);
        self.freeze_object(&raw_arr);
        self.set_member(&strings, "raw", raw_arr)?;
        self.freeze_object(&strings);
        // Evaluate the tag callee, capturing `this` for method tags (`obj.tag\`...\``).
        let (func, this) = match tag {
            Expr::Member { obj, prop, .. } => {
                let base = self.eval(obj, env)?;
                let f = self.get_member(&base, prop)?;
                (f, base)
            }
            Expr::Index { obj, index, .. } => {
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                let f = self.get_member(&base, &key)?;
                (f, base)
            }
            _ => (self.eval(tag, env)?, Value::Undefined),
        };
        if !func.is_callable() {
            return Err(self.throw("TypeError", "tag is not a function"));
        }
        let mut argv = vec![strings];
        for s in subs {
            argv.push(self.eval(s, env)?);
        }
        self.call(func, this, &argv)
    }

    fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[ArrayElem],
        optional: bool,
        env: &Env,
    ) -> Result<Value, Abrupt> {
        // Direct eval: `eval(src)` called by that exact name runs the code in the *caller's* scope
        // (so it can see/define local bindings). Any other way of reaching eval is indirect and runs
        // in the global scope (handled by the global `eval` native).
        if let Expr::Ident(name) = callee {
            if name == "eval" {
                if let (Ok(Value::Obj(f)), Some(ef)) =
                    (self.get_var("eval", env), self.eval_fn.clone())
                {
                    if Rc::ptr_eq(&f, &ef) {
                        let argv = self.eval_args(args, env)?;
                        return self.direct_eval(argv.first(), env);
                    }
                }
            }
        }
        // `super(...)`: invoke the parent constructor on the current `this`, then run this class's
        // instance-field initializers.
        if matches!(callee, Expr::Super) {
            // A super-call outside a derived constructor body (a method, a field initializer, or
            // anything reached via a direct `eval` from those) is illegal — and re-entering field
            // initialization would recurse without bound.
            if !self.super_call_ok {
                return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
            }
            let parent = self.get_var("%superclass%", env)?;
            if matches!(parent, Value::Undefined) {
                return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
            }
            let this = self.get_var("this", env)?;
            let argv = self.eval_args(args, env)?;
            self.run_constructor_on(&parent, &this, &argv)?;
            let this_ctor = self.get_var("%thisctor%", env)?;
            self.init_instance_fields(&this_ctor, &this)?;
            return Ok(Value::Undefined);
        }
        // `super.m(...)` / `super[k](...)`: method on the super prototype, called with current `this`.
        if let Expr::Member { obj, prop, .. } = callee {
            if matches!(**obj, Expr::Super) {
                let home = self.super_base(env)?;
                let f = self.get_member(&home, prop)?;
                let this = self.get_var("this", env)?;
                let argv = self.eval_args(args, env)?;
                return self.call(f, this, &argv);
            }
        }
        if let Expr::Index { obj, index, .. } = callee {
            if matches!(**obj, Expr::Super) {
                let home = self.super_base(env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                let f = self.get_member(&home, &key)?;
                let this = self.get_var("this", env)?;
                let argv = self.eval_args(args, env)?;
                return self.call(f, this, &argv);
            }
        }
        // Determine `this` for method calls (`obj.m()` → this = obj).
        let (func, this) = match callee {
            Expr::Member {
                obj,
                prop,
                optional,
            } => {
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let f = self.get_member(&base, prop)?;
                (f, base)
            }
            Expr::Index {
                obj,
                index,
                optional,
            } => {
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                let f = self.get_member(&base, &key)?;
                (f, base)
            }
            _ => {
                let f = self.eval(callee, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined);
                }
                (f, Value::Undefined)
            }
        };
        // `f?.()` short-circuits the whole chain when the callee is nullish.
        if optional && matches!(func, Value::Undefined | Value::Null) {
            self.short_circuit = true;
            return Ok(Value::Undefined);
        }
        let argv = self.eval_args(args, env)?;
        if !func.is_callable() {
            let desc = describe_callee(callee);
            return Err(self.throw("TypeError", format!("{desc} is not a function")));
        }
        self.call(func, this, &argv)
    }

    /// `await v`: if `v` is a promise, drain microtasks to settle it, then return its value (or
    /// throw its reason). A non-promise is returned as-is. A still-pending promise yields undefined
    /// (lumen cannot truly suspend).
    pub(crate) fn await_value(&mut self, v: Value) -> Result<Value, Abrupt> {
        let ptr = match &v {
            Value::Obj(o) if self.promises.contains_key(&(Rc::as_ptr(o) as usize)) => {
                Rc::as_ptr(o) as usize
            }
            _ => return Ok(v),
        };
        self.drain_microtasks();
        match self.promises.get(&ptr) {
            Some(s) if s.status == 1 => Ok(s.value.clone()),
            Some(s) if s.status == 2 => Err(Abrupt::Throw(s.value.clone())),
            _ => Ok(Value::Undefined),
        }
    }

    // ----- promises ---------------------------------------------------------------------------

    pub(crate) fn new_promise(&mut self) -> Value {
        let obj = Object::new(self.extra_protos.get("Promise").cloned());
        let p = Rc::as_ptr(&obj) as usize;
        self.promises.insert(p, PromiseState::default());
        Value::Obj(obj)
    }

    /// A bound function that settles `promise` (fulfilling or rejecting) when called.
    pub(crate) fn make_resolver(&mut self, promise: &Value, fulfilling: bool) -> Value {
        let target = self.make_native(
            if fulfilling { "resolve" } else { "reject" },
            1,
            if fulfilling {
                promise_resolve_native
            } else {
                promise_reject_native
            },
        );
        let bound = Object::new(Some(self.function_proto.clone()));
        bound.borrow_mut().call = Callable::Bound {
            target,
            this: promise.clone(),
            args: Vec::new(),
        };
        // A promise resolving function has `length` 1 and an empty `name`.
        bound.borrow_mut().props.insert(
            "length",
            crate::value::Property::data(Value::Num(1.0), false, false, true),
        );
        bound.borrow_mut().props.insert(
            "name",
            crate::value::Property::data(Value::str(""), false, false, true),
        );
        Value::Obj(bound)
    }

    pub(crate) fn resolve_promise(&mut self, promise: &Value, value: Value) {
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return,
        };
        if self.promises.get(&ptr).map(|s| s.status).unwrap_or(1) != 0 {
            return;
        }
        // Adopt a thenable's eventual state.
        if matches!(value, Value::Obj(_)) {
            if let Ok(then) = self.get_member(&value, "then") {
                if then.is_callable() {
                    let res = self.make_resolver(promise, true);
                    let rej = self.make_resolver(promise, false);
                    if let Err(Abrupt::Throw(e)) = self.call(then, value.clone(), &[res, rej]) {
                        self.reject_promise(promise, e);
                    }
                    return;
                }
            }
        }
        self.settle(promise, value, true);
    }

    pub(crate) fn reject_promise(&mut self, promise: &Value, reason: Value) {
        self.settle(promise, reason, false);
    }

    fn settle(&mut self, promise: &Value, value: Value, fulfilled: bool) {
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return,
        };
        let reactions = match self.promises.get_mut(&ptr) {
            Some(s) if s.status == 0 => {
                s.status = if fulfilled { 1 } else { 2 };
                s.value = value.clone();
                std::mem::take(&mut s.reactions)
            }
            _ => return,
        };
        for (on_f, on_r, result) in reactions {
            let handler = if fulfilled { on_f } else { on_r };
            self.microtasks.push_back(Job {
                handler,
                result,
                value: value.clone(),
                fulfilled,
            });
        }
    }

    /// The `then` operation: register reactions, returning a new dependent promise.
    pub(crate) fn promise_then(&mut self, promise: &Value, on_f: Value, on_r: Value) -> Value {
        let result = self.new_promise();
        self.promise_then_into(promise, on_f, on_r, result.clone());
        result
    }

    /// PerformPromiseThen with a caller-supplied result promise (so `Promise.prototype.then` can
    /// route through SpeciesConstructor + NewPromiseCapability).
    pub(crate) fn promise_then_into(
        &mut self,
        promise: &Value,
        on_f: Value,
        on_r: Value,
        result: Value,
    ) {
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return,
        };
        let status = self.promises.get(&ptr).map(|s| s.status).unwrap_or(0);
        match status {
            0 => {
                if let Some(s) = self.promises.get_mut(&ptr) {
                    s.reactions.push((on_f, on_r, result.clone()));
                }
            }
            1 => {
                let v = self.promises[&ptr].value.clone();
                self.microtasks.push_back(Job {
                    handler: on_f,
                    result: result.clone(),
                    value: v,
                    fulfilled: true,
                });
            }
            _ => {
                let v = self.promises[&ptr].value.clone();
                self.microtasks.push_back(Job {
                    handler: on_r,
                    result: result.clone(),
                    value: v,
                    fulfilled: false,
                });
            }
        }
    }

    /// Drain the microtask queue (called after the main script). Bounded to avoid an unbounded loop.
    /// Drive microtasks plus any pending `Atomics.waitAsync` operations to completion: resolve each
    /// async wait as its waiter thread reports, running the scheduled reactions in between.
    pub(crate) fn run_agent_event_loop(&mut self) {
        self.drain_microtasks();
        let mut spins = 0u32;
        while !self.pending_async_waits.is_empty() {
            let mut resolved_any = false;
            let mut i = 0;
            while i < self.pending_async_waits.len() {
                if let Ok(res) = self.pending_async_waits[i].1.try_recv() {
                    let (promise, _) = self.pending_async_waits.remove(i);
                    self.resolve_promise(&promise, Value::str(res));
                    resolved_any = true;
                } else {
                    i += 1;
                }
            }
            if resolved_any {
                self.drain_microtasks();
                spins = 0;
            } else {
                spins += 1;
                // A safety bound (~30s at 1ms) so a never-completing wait can't hang the process.
                if spins > 4_000 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    pub(crate) fn drain_microtasks(&mut self) {
        let mut budget = 100_000u32;
        while let Some(job) = self.microtasks.pop_front() {
            budget -= 1;
            if budget == 0 {
                self.microtasks.clear();
                break;
            }
            if job.handler.is_callable() {
                match self.call(
                    job.handler.clone(),
                    Value::Undefined,
                    std::slice::from_ref(&job.value),
                ) {
                    Ok(r) => self.resolve_promise(&job.result, r),
                    Err(Abrupt::Throw(e)) => self.reject_promise(&job.result, e),
                    Err(_) => {}
                }
            } else if job.fulfilled {
                self.resolve_promise(&job.result, job.value);
            } else {
                self.reject_promise(&job.result, job.value);
            }
        }
    }

    /// Compile a regular expression and build a RegExp object (its metadata stored as own props,
    /// the compiled program in the `regexps` side table). A bad pattern throws a SyntaxError.
    pub(crate) fn make_regexp(&mut self, source: &str, flags: &str) -> Result<Value, Abrupt> {
        let re =
            crate::regex::Regex::new(source, flags).map_err(|e| self.throw("SyntaxError", e))?;
        let obj = Object::new(self.extra_protos.get("RegExp").cloned());
        let ptr = Rc::as_ptr(&obj) as usize;
        // source/flags/global/... are accessor getters on RegExp.prototype (computed from the
        // matcher); only `lastIndex` is an own writable data property.
        obj.borrow_mut().props.insert(
            "lastIndex",
            Property::data(Value::Num(0.0), true, false, false),
        );
        self.regexps.insert(ptr, Rc::new(re));
        Ok(Value::Obj(obj))
    }

    /// Direct eval: a non-string argument is returned unchanged; a string is parsed and executed.
    fn direct_eval(&mut self, arg: Option<&Value>, env: &Env) -> Result<Value, Abrupt> {
        let code = match arg {
            Some(Value::Str(s)) => s.clone(),
            Some(other) => return Ok(other.clone()),
            None => return Ok(Value::Undefined),
        };
        self.perform_eval(&code, env, true)
    }

    /// PerformEval: parse `code`, set up its variable + lexical environments, run
    /// EvalDeclarationInstantiation, then execute the body.
    ///
    /// A *direct* eval (`direct` = true) inherits the caller's strictness and runs in `caller_env`:
    /// sloppy code hoists its `var`/function declarations into the caller's nearest variable
    /// environment while its lexical declarations stay private to a fresh scope. An *indirect* eval
    /// always runs in the global scope and is only strict via its own `"use strict"` directive.
    pub(crate) fn perform_eval(
        &mut self,
        code: &str,
        caller_env: &Env,
        direct: bool,
    ) -> Result<Value, Abrupt> {
        let base_strict = direct && self.strict;
        // `new.target` is valid at the top level of a direct eval whose caller is in function code.
        let allow_new_target = direct && self.in_function_code(caller_env);
        // A direct eval may contain a SuperProperty; the runtime `super_base` lookup enforces that a
        // home object is actually in scope (throwing SyntaxError otherwise), so parse permissively.
        let body = crate::parser::parse_script_eval(code, base_strict, allow_new_target, direct)
            .map_err(|e| self.throw("SyntaxError", e.message))?;
        // A direct `eval` inherits the caller's super-call context: a `super(...)` in the eval is an
        // early SyntaxError unless the eval sits directly inside a derived constructor body. (Caught
        // here, before any of the eval body runs, so side effects preceding the `super()` don't.)
        if direct && !self.super_call_ok && stmts_have_super_call(&body) {
            return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
        }
        let directive_strict = matches!(
            body.first(),
            Some(Stmt::Expr(Expr::Str(s))) if &**s == "use strict"
        );
        let strict = base_strict || directive_strict;

        // PerformEval steps: choose the variable and lexical environments.
        let (var_env, lex_env) = if !direct {
            if strict {
                // Strict indirect eval: its own environments — top-level declarations don't leak to
                // the global object.
                let e = new_var_scope(Some(self.global_env.clone()));
                (e.clone(), e)
            } else {
                // Sloppy indirect eval runs in the global scope.
                (
                    self.global_env.clone(),
                    new_scope(Some(self.global_env.clone())),
                )
            }
        } else if strict {
            // Strict direct eval: its own variable + lexical environment — nothing leaks out.
            let e = new_var_scope(Some(caller_env.clone()));
            (e.clone(), e)
        } else {
            // Sloppy direct eval: `var`/function declarations hoist into the caller's nearest
            // variable environment; lexical declarations stay in a fresh, private lexical scope.
            (
                nearest_var_env(caller_env),
                new_scope(Some(caller_env.clone())),
            )
        };

        self.eval_declaration_instantiation(&body, &var_env, &lex_env, strict)?;

        let saved = self.strict;
        self.strict = strict;
        let result = self.run_eval_body(&body, &lex_env);
        self.strict = saved;
        result
    }

    /// EvalDeclarationInstantiation: validate the eval body's `var`/function declarations against the
    /// surrounding environments (throwing `SyntaxError`/`TypeError` on a conflict), then create them.
    fn eval_declaration_instantiation(
        &mut self,
        body: &[Stmt],
        var_env: &Env,
        lex_env: &Env,
        strict: bool,
    ) -> Result<(), Abrupt> {
        // VarDeclaredNames (every `var` binding name plus hoisted function names, top-level and Annex
        // B.3.3 block-scoped) together with their instantiation values — gathered by hoisting into a
        // throwaway scope, which mirrors the interpreter's own var-scoping and function-precedence
        // rules exactly. Its parent is the eval's lexical environment so any function closes over it.
        // The hoist runs in the eval's strict mode so Annex B block functions apply only when sloppy.
        let probe = new_scope(Some(lex_env.clone()));
        let saved_strict = self.strict;
        self.strict = strict;
        self.hoist(body, &probe, true);
        self.strict = saved_strict;
        let var_names: Vec<String> = probe.borrow().vars.keys().cloned().collect();
        // A callable hoisted value is a function declaration (which becomes a global *function*
        // binding); everything else is a plain `var`.
        let is_func = |name: &str| {
            probe
                .borrow()
                .vars
                .get(name)
                .map(|b| b.value.is_callable())
                .unwrap_or(false)
        };
        let is_global = Rc::ptr_eq(var_env, &self.global_env);

        if !strict {
            // A sloppy eval must not hoist a `var` over a same-named global lexical declaration...
            if is_global {
                for name in &var_names {
                    if name != "this" && self.global_env.borrow().vars.contains_key(name) {
                        return Err(self.throw(
                            "SyntaxError",
                            format!("Identifier '{name}' has already been declared"),
                        ));
                    }
                }
            }
            // ...nor over a like-named lexical binding in any scope between the eval and its variable
            // environment (block `let`/`const`, a parameter scope's `arguments`/parameters, etc.).
            let mut cur = Some(lex_env.clone());
            while let Some(s) = cur {
                if Rc::ptr_eq(&s, var_env) {
                    break;
                }
                let (skip, parent) = {
                    let b = s.borrow();
                    // A `with` object environment holds no lexical declarations, and a `catch`
                    // parameter environment is exempt from the var/lexical conflict check.
                    (b.with_obj.is_some() || b.catch_param, b.parent.clone())
                };
                if !skip {
                    for name in &var_names {
                        if s.borrow().vars.contains_key(name) {
                            return Err(self.throw(
                                "SyntaxError",
                                format!("Identifier '{name}' has already been declared"),
                            ));
                        }
                    }
                }
                cur = parent;
            }
        }

        // A global variable environment can refuse a declaration (non-extensible global, or a
        // non-configurable same-named property) with a TypeError.
        if is_global {
            for name in &var_names {
                let ok = if is_func(name) {
                    self.can_declare_global_function(name)
                } else {
                    self.can_declare_global_var(name)
                };
                if !ok {
                    return Err(self.throw(
                        "TypeError",
                        format!("cannot declare global binding '{name}'"),
                    ));
                }
            }
        }

        // Instantiate each declared name from the value the probe hoist computed (a function object —
        // top-level or Annex B.3.3 block-scoped, later hoists winning — or `undefined` for a plain
        // `var`). A pre-existing `var` binding keeps its value; a function binding always overwrites.
        for name in &var_names {
            let value = probe
                .borrow()
                .vars
                .get(name)
                .map(|b| b.value.clone())
                .unwrap_or(Value::Undefined);
            if is_global {
                if is_func(name) {
                    self.create_global_function_binding(name, value);
                } else {
                    self.create_global_var_binding(name);
                }
            } else if is_func(name) {
                var_env.borrow_mut().vars.insert(name.clone(), {
                    let mut b = Binding::data(value, true, true);
                    b.deletable = true;
                    b
                });
            } else if !var_env.borrow().vars.contains_key(name) {
                var_env.borrow_mut().vars.insert(name.clone(), {
                    let mut b = Binding::data(Value::Undefined, true, true);
                    b.deletable = true;
                    b
                });
            }
        }
        // Lexical declarations (`let`/`const`/`class`) stay in the eval's private lexical scope.
        self.declare_block_lexicals(body, lex_env, false);
        Ok(())
    }

    /// CanDeclareGlobalVar: a global `var` is definable if the property already exists or the global
    /// object is extensible.
    fn can_declare_global_var(&self, name: &str) -> bool {
        if self.global.borrow().props.contains(name) {
            return true;
        }
        self.global.borrow().extensible
    }

    /// CanDeclareGlobalFunction: definable if an existing property is configurable (or a writable,
    /// enumerable data property), or — when absent — the global object is extensible.
    fn can_declare_global_function(&self, name: &str) -> bool {
        let g = self.global.borrow();
        match g.props.get(name) {
            None => g.extensible,
            Some(p) => {
                p.configurable || (p.get.is_none() && p.set.is_none() && p.writable && p.enumerable)
            }
        }
    }

    /// CreateGlobalFunctionBinding(name, value, deletable=true): define (or overwrite) a configurable
    /// global function property.
    fn create_global_function_binding(&mut self, name: &str, value: Value) {
        let existing = self
            .global
            .borrow()
            .props
            .get(name)
            .map(|p| (p.configurable, p.get.is_none() && p.set.is_none()));
        match existing {
            Some((false, true)) => {
                // A pre-existing non-configurable data property keeps its attributes; only its value
                // is replaced.
                if let Some(p) = self.global.borrow_mut().props.get_mut(name) {
                    p.value = value;
                }
            }
            _ => {
                self.global
                    .borrow_mut()
                    .props
                    .insert(name, Property::data(value, true, true, true));
            }
        }
    }

    /// CreateGlobalVarBinding(name, deletable=true): create a configurable global var property if the
    /// global object doesn't already have one.
    fn create_global_var_binding(&mut self, name: &str) {
        if self.global.borrow().props.contains(name) {
            return;
        }
        if self.global.borrow().extensible {
            self.global
                .borrow_mut()
                .props
                .insert(name, Property::data(Value::Undefined, true, true, true));
        }
    }

    /// Execute an eval body's statements in its lexical environment (declarations already
    /// instantiated), returning the completion value of the last value-producing statement.
    fn run_eval_body(&mut self, body: &[Stmt], lex_env: &Env) -> Result<Value, Abrupt> {
        let has_using = body.iter().any(stmt_declares_using);
        if has_using {
            self.using_stack.push(Vec::new());
        }
        let mut last = Value::Undefined;
        let mut result: Completion = Ok(Value::Undefined);
        for stmt in body {
            match self.exec_stmt(stmt, lex_env) {
                Ok(v) => {
                    if !matches!(v, Value::Undefined) {
                        last = v;
                    }
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if has_using {
            let frame = self.using_stack.pop().unwrap_or_default();
            result = self.dispose_frame(frame, result);
        }
        result?;
        Ok(last)
    }

    // ----- classes ----------------------------------------------------------------------------

    fn eval_class(&mut self, class: &Rc<Class>, env: &Env) -> Result<Value, Abrupt> {
        // Superclass and the prototype / static parents it implies.
        let parent = match &class.superclass {
            Some(e) => Some(self.eval(e, env)?),
            None => None,
        };
        let (proto_parent, ctor_parent): (Option<Gc>, Option<Value>) = match &parent {
            None => (Some(self.object_proto.clone()), None),
            Some(Value::Null) => (None, None),
            Some(v @ Value::Obj(pc)) if v.is_callable() => {
                let pp = self.get_member(v, "prototype")?;
                let pp = match pp {
                    Value::Obj(o) => Some(o),
                    Value::Null => None,
                    _ => {
                        return Err(self.throw(
                            "TypeError",
                            "Class extends value does not have a valid prototype property",
                        ))
                    }
                };
                (pp, Some(Value::Obj(pc.clone())))
            }
            _ => {
                return Err(self.throw(
                    "TypeError",
                    "Class extends value is not a constructor or null",
                ))
            }
        };
        let derived = parent.is_some();

        let proto = Object::new(proto_parent.clone());

        // The constructor: explicit member, or a synthesized default.
        let ctor_func = class
            .members
            .iter()
            .find(|m| m.kind == MemberKind::Constructor)
            .and_then(|m| m.func.clone())
            .unwrap_or_else(|| Rc::new(default_constructor(derived)));

        // Environments that carry the `super` bindings into methods/fields.
        let class_env = new_scope(Some(env.clone()));
        let inst_env = new_scope(Some(class_env.clone()));
        bind(&inst_env, "%superproto%", opt_obj(&proto_parent));
        bind(
            &inst_env,
            "%superclass%",
            ctor_parent.clone().unwrap_or(Value::Undefined),
        );
        let static_env = new_scope(Some(class_env.clone()));
        bind(
            &static_env,
            "%superproto%",
            ctor_parent.clone().unwrap_or(Value::Undefined),
        );

        // Build the constructor object on `proto`.
        let ctor_val = self.make_function(ctor_func, inst_env.clone());
        let ctor_obj = ctor_val.as_obj().unwrap().clone();
        {
            let mut b = ctor_obj.borrow_mut();
            b.props.insert(
                "prototype",
                Property::data(Value::Obj(proto.clone()), false, false, false),
            );
            b.proto = match &ctor_parent {
                Some(Value::Obj(p)) => Some(p.clone()),
                _ => Some(self.function_proto.clone()),
            };
            if let Some(n) = &class.name {
                b.props.insert(
                    "name",
                    Property::data(Value::from_string(n.clone()), false, false, true),
                );
            }
        }
        proto
            .borrow_mut()
            .props
            .insert("constructor", Property::builtin(ctor_val.clone()));
        bind(&inst_env, "%thisctor%", ctor_val.clone());
        // A named class binds its own name (initialized) in the class scope, so methods, static
        // blocks, field initializers and decorators can reference the class itself.
        if let Some(n) = &class.name {
            bind(&class_env, n, ctor_val.clone());
        }

        // Methods, accessors and fields.
        let mut inst_fields: Vec<FieldInit> = Vec::new();
        let mut instance_inits: Vec<Value> = Vec::new();
        let mut static_inits: Vec<Value> = Vec::new();
        for m in &class.members {
            if m.kind == MemberKind::Constructor {
                continue;
            }
            let key = self.eval_prop_key(&m.key, env)?;
            let is_private = key.starts_with('#');
            let menv = if m.is_static { &static_env } else { &inst_env };
            let target = if m.is_static {
                ctor_obj.clone()
            } else {
                proto.clone()
            };
            match m.kind {
                MemberKind::Accessor => {
                    // An auto-accessor: a private backing field plus a brand-checked getter/setter.
                    // The backing key is globally unique so a subclass accessor never collides with
                    // a superclass one on the same instance.
                    self.accessor_seq += 1;
                    let backing: Rc<str> =
                        Rc::from(format!("#\u{0}acc{}", self.accessor_seq).as_str());
                    let mut getter = self.make_accessor_fn(&key, &backing, true);
                    let mut setter = self.make_accessor_fn(&key, &backing, false);
                    let mut transforms = Vec::new();
                    if !m.decorators.is_empty() {
                        let sink = if m.is_static {
                            &mut static_inits
                        } else {
                            &mut instance_inits
                        };
                        let (g, s, t) = self.decorate_accessor(
                            &m.decorators,
                            env,
                            &key,
                            m.is_static,
                            getter,
                            setter,
                            sink,
                        )?;
                        getter = g;
                        setter = s;
                        transforms = t;
                    }
                    self.define_class_accessor(&target, &key, Some(getter), Some(setter));
                    if m.is_static {
                        let scope = new_scope(Some(static_env.clone()));
                        bind(&scope, "this", ctor_val.clone());
                        let mut v = match &m.value {
                            Some(e) => self.eval(e, &scope)?,
                            None => Value::Undefined,
                        };
                        for tr in &transforms {
                            v = self.call(tr.clone(), ctor_val.clone(), &[v])?;
                        }
                        ctor_obj
                            .borrow_mut()
                            .props
                            .insert(backing.to_string(), Property::plain(v));
                    } else {
                        inst_fields.push(FieldInit {
                            key: backing.to_string(),
                            init: m.value.clone(),
                            transforms,
                        });
                    }
                }
                MemberKind::Method => {
                    let mut f = self.make_function(m.func.clone().unwrap(), menv.clone());
                    if let Value::Obj(fo) = &f {
                        fo.borrow_mut().props.insert(
                            "name",
                            Property::data(Value::from_string(key.clone()), false, false, true),
                        );
                    }
                    if !m.decorators.is_empty() {
                        let sink = if m.is_static {
                            &mut static_inits
                        } else {
                            &mut instance_inits
                        };
                        f = self.decorate_callable(
                            &m.decorators,
                            env,
                            f,
                            "method",
                            &key,
                            m.is_static,
                            is_private,
                            sink,
                        )?;
                    }
                    target.borrow_mut().props.insert(key, Property::builtin(f));
                }
                MemberKind::Get | MemberKind::Set => {
                    let mut f = self.make_function(m.func.clone().unwrap(), menv.clone());
                    let is_get = m.kind == MemberKind::Get;
                    if !m.decorators.is_empty() {
                        let sink = if m.is_static {
                            &mut static_inits
                        } else {
                            &mut instance_inits
                        };
                        let kind = if is_get { "getter" } else { "setter" };
                        f = self.decorate_callable(
                            &m.decorators,
                            env,
                            f,
                            kind,
                            &key,
                            m.is_static,
                            is_private,
                            sink,
                        )?;
                    }
                    let (get, set) = if is_get {
                        (Some(f), None)
                    } else {
                        (None, Some(f))
                    };
                    self.define_class_accessor(&target, &key, get, set);
                }
                MemberKind::Field => {
                    let transforms = if m.decorators.is_empty() {
                        Vec::new()
                    } else {
                        let sink = if m.is_static {
                            &mut static_inits
                        } else {
                            &mut instance_inits
                        };
                        self.decorate_field(
                            &m.decorators,
                            env,
                            &key,
                            m.is_static,
                            is_private,
                            sink,
                        )?
                    };
                    if m.is_static {
                        let scope = new_scope(Some(static_env.clone()));
                        bind(&scope, "this", ctor_val.clone());
                        let mut v = match &m.value {
                            Some(e) => self.eval(e, &scope)?,
                            None => Value::Undefined,
                        };
                        for tr in &transforms {
                            v = self.call(tr.clone(), ctor_val.clone(), &[v])?;
                        }
                        ctor_obj.borrow_mut().props.insert(key, Property::plain(v));
                    } else {
                        inst_fields.push(FieldInit {
                            key,
                            init: m.value.clone(),
                            transforms,
                        });
                    }
                }
                MemberKind::StaticBlock => {
                    let scope = new_scope(Some(static_env.clone()));
                    bind(&scope, "this", ctor_val.clone());
                    if let Some(func) = &m.func {
                        for stmt in &func.body {
                            self.exec_stmt(stmt, &scope)?;
                        }
                    }
                }
                MemberKind::Constructor => {}
            }
        }

        self.class_info.insert(
            Rc::as_ptr(&ctor_obj) as usize,
            ClassInfo {
                fields: inst_fields,
                field_env: inst_env,
                derived,
                instance_initializers: instance_inits,
            },
        );

        // Class decorators apply after the body is built; a callable return replaces the class.
        let mut class_value = ctor_val;
        if !class.decorators.is_empty() {
            let name = class.name.clone().unwrap_or_default();
            class_value = self.decorate_callable(
                &class.decorators,
                env,
                class_value,
                "class",
                &name,
                false,
                false,
                &mut static_inits,
            )?;
        }
        // Static element initializers and class initializers run with `this` = the class.
        for init in &static_inits {
            self.call(init.clone(), class_value.clone(), &[])?;
        }
        Ok(class_value)
    }

    /// Build an auto-accessor's getter (`is_get`) or setter as a function object backed by the
    /// private `backing` field, with a spec-shaped `name` (`get x`/`set x`) and `length`.
    fn make_accessor_fn(&self, name: &str, backing: &Rc<str>, is_get: bool) -> Value {
        let o = Object::new(Some(self.function_proto.clone()));
        {
            let mut b = o.borrow_mut();
            b.call = if is_get {
                Callable::AccessorGet(backing.clone())
            } else {
                Callable::AccessorSet(backing.clone())
            };
            let prefix = if is_get { "get " } else { "set " };
            b.props.insert(
                "name",
                Property::data(
                    Value::from_string(format!("{prefix}{name}")),
                    false,
                    false,
                    true,
                ),
            );
            b.props.insert(
                "length",
                Property::data(
                    Value::Num(if is_get { 0.0 } else { 1.0 }),
                    false,
                    false,
                    true,
                ),
            );
        }
        Value::Obj(o)
    }

    fn define_class_accessor(
        &self,
        target: &Gc,
        key: &str,
        get: Option<Value>,
        set: Option<Value>,
    ) {
        let mut b = target.borrow_mut();
        if let Some(p) = b.props.get_mut(key) {
            if p.accessor {
                if get.is_some() {
                    p.get = get;
                }
                if set.is_some() {
                    p.set = set;
                }
                return;
            }
        }
        b.props.insert(
            key,
            Property {
                value: Value::Undefined,
                get,
                set,
                accessor: true,
                writable: false,
                enumerable: false,
                configurable: true,
            },
        );
    }

    /// A function object wrapping an internal [`Callable`] (used for accessor get/set and decorator
    /// `access` helpers), with a spec-shaped `name`/`length`.
    fn make_callable(&self, call: Callable, name: &str, len: f64) -> Value {
        let o = Object::new(Some(self.function_proto.clone()));
        {
            let mut b = o.borrow_mut();
            b.call = call;
            b.props.insert(
                "name",
                Property::data(Value::from_string(name.to_string()), false, false, true),
            );
            b.props.insert(
                "length",
                Property::data(Value::Num(len), false, false, true),
            );
        }
        Value::Obj(o)
    }

    /// Build a decorator context object: `{ kind, name, static, private, access, addInitializer,
    /// metadata }`. `access.get/set` read/write the element's property on a receiver.
    fn make_decorator_context(
        &mut self,
        kind: &str,
        key: &str,
        is_static: bool,
        is_private: bool,
    ) -> Value {
        let ctx = self.new_object();
        let cv = Value::Obj(ctx.clone());
        let key_rc: Rc<str> = Rc::from(key);
        let name = if !is_private && Self::is_sym_key(key) {
            self.sym_from_key(key).unwrap_or(Value::Undefined)
        } else {
            Value::from_string(key.to_string())
        };
        let access = self.new_object();
        if matches!(kind, "method" | "getter" | "field" | "accessor") {
            let g = self.make_callable(Callable::PropGet(key_rc.clone()), "get", 1.0);
            access.borrow_mut().props.insert("get", Property::plain(g));
        }
        if matches!(kind, "setter" | "field" | "accessor") {
            let s = self.make_callable(Callable::PropSet(key_rc.clone()), "set", 2.0);
            access.borrow_mut().props.insert("set", Property::plain(s));
        }
        let add_init = self.make_native("addInitializer", 1, dec_add_initializer);
        {
            let mut b = ctx.borrow_mut();
            b.props.insert("kind", Property::plain(Value::str(kind)));
            b.props.insert("name", Property::plain(name));
            b.props
                .insert("static", Property::plain(Value::Bool(is_static)));
            b.props
                .insert("private", Property::plain(Value::Bool(is_private)));
            b.props
                .insert("access", Property::plain(Value::Obj(access)));
            b.props
                .insert("addInitializer", Property::plain(Value::Obj(add_init)));
            b.props
                .insert("metadata", Property::plain(Value::Undefined));
        }
        cv
    }

    /// Apply a list of decorators (innermost/last first) to a callable element (method/getter/setter
    /// or whole class), folding each non-undefined callable return in as the replacement.
    fn decorate_callable(
        &mut self,
        decorators: &[Expr],
        env: &Env,
        mut value: Value,
        kind: &str,
        key: &str,
        is_static: bool,
        is_private: bool,
        inits: &mut Vec<Value>,
    ) -> Result<Value, Abrupt> {
        for d in decorators.iter().rev() {
            let dec = self.eval(d, env)?;
            if !dec.is_callable() {
                return Err(self.throw("TypeError", "decorator is not callable"));
            }
            let ctx = self.make_decorator_context(kind, key, is_static, is_private);
            let r = self.call(dec, Value::Undefined, &[value.clone(), ctx])?;
            inits.append(&mut std::mem::take(&mut self.decorator_initializers));
            match r {
                Value::Undefined => {}
                v if v.is_callable() => value = v,
                _ => {
                    return Err(
                        self.throw("TypeError", "decorator must return a function or undefined")
                    )
                }
            }
        }
        Ok(value)
    }

    /// Apply field decorators, returning the initializer transforms they contribute.
    fn decorate_field(
        &mut self,
        decorators: &[Expr],
        env: &Env,
        key: &str,
        is_static: bool,
        is_private: bool,
        inits: &mut Vec<Value>,
    ) -> Result<Vec<Value>, Abrupt> {
        let mut transforms = Vec::new();
        for d in decorators.iter().rev() {
            let dec = self.eval(d, env)?;
            if !dec.is_callable() {
                return Err(self.throw("TypeError", "decorator is not callable"));
            }
            let ctx = self.make_decorator_context("field", key, is_static, is_private);
            let r = self.call(dec, Value::Undefined, &[Value::Undefined, ctx])?;
            inits.append(&mut std::mem::take(&mut self.decorator_initializers));
            match r {
                Value::Undefined => {}
                v if v.is_callable() => transforms.push(v),
                _ => {
                    return Err(self.throw(
                        "TypeError",
                        "field decorator must return a function or undefined",
                    ))
                }
            }
        }
        Ok(transforms)
    }

    /// Apply accessor decorators, threading the `{get, set}` pair and collecting `init` transforms.
    #[allow(clippy::type_complexity)]
    fn decorate_accessor(
        &mut self,
        decorators: &[Expr],
        env: &Env,
        key: &str,
        is_static: bool,
        mut get: Value,
        mut set: Value,
        inits: &mut Vec<Value>,
    ) -> Result<(Value, Value, Vec<Value>), Abrupt> {
        let is_private = key.starts_with('#');
        let mut transforms = Vec::new();
        for d in decorators.iter().rev() {
            let dec = self.eval(d, env)?;
            if !dec.is_callable() {
                return Err(self.throw("TypeError", "decorator is not callable"));
            }
            let ctx = self.make_decorator_context("accessor", key, is_static, is_private);
            let pair = self.new_object();
            pair.borrow_mut()
                .props
                .insert("get", Property::plain(get.clone()));
            pair.borrow_mut()
                .props
                .insert("set", Property::plain(set.clone()));
            let r = self.call(dec, Value::Undefined, &[Value::Obj(pair), ctx])?;
            inits.append(&mut std::mem::take(&mut self.decorator_initializers));
            match r {
                Value::Undefined => {}
                Value::Obj(_) => {
                    let ng = self.get_member(&r, "get")?;
                    if ng.is_callable() {
                        get = ng;
                    }
                    let ns = self.get_member(&r, "set")?;
                    if ns.is_callable() {
                        set = ns;
                    }
                    let init = self.get_member(&r, "init")?;
                    if init.is_callable() {
                        transforms.push(init);
                    }
                }
                _ => {
                    return Err(self.throw(
                        "TypeError",
                        "accessor decorator must return an object or undefined",
                    ))
                }
            }
        }
        Ok((get, set, transforms))
    }

    /// Run a constructor's body against an already-allocated `this`, used by both `construct` and
    /// `super(...)`. Handles base-class field init, derived classes (their `super()` does the work),
    /// plain function constructors, and native parents (e.g. `extends Error`).
    pub fn run_constructor_on(
        &mut self,
        ctor: &Value,
        this: &Value,
        args: &[Value],
    ) -> Result<(), Abrupt> {
        let obj = match ctor {
            Value::Obj(o) => o.clone(),
            _ => return Err(self.throw("TypeError", "super target is not a constructor")),
        };
        let ptr = Rc::as_ptr(&obj) as usize;
        let is_class = self.class_info.contains_key(&ptr);
        let derived = self
            .class_info
            .get(&ptr)
            .map(|i| i.derived)
            .unwrap_or(false);
        let call = obj.borrow().call.clone();
        match call {
            Callable::User(func, cenv) => {
                // A base class initializes its fields before its body runs; a derived class does so
                // inside its own `super()`.
                if is_class && !derived {
                    self.init_instance_fields(ctor, this)?;
                }
                // A `super(...)` call is legal only directly within a *derived* constructor body.
                let saved_super = self.super_call_ok;
                self.super_call_ok = is_class && derived;
                let r = self.call_user(&func, cenv, this.clone(), args, true, &obj);
                self.super_call_ok = saved_super;
                r?;
                Ok(())
            }
            Callable::Native(f) => {
                // Native parent (e.g. Error/Map): a super() call is a construct, so set the flag for
                // constructors that require `new`. Run it, then graft its own props onto `this`.
                let saved = self.constructing;
                self.constructing = true;
                let made = f(self, this.clone(), args).map_err(Abrupt::Throw);
                self.constructing = saved;
                let made = made?;
                if let (Value::Obj(src), Value::Obj(dst)) = (&made, this) {
                    if !Rc::ptr_eq(src, dst) {
                        for k in src.borrow().props.keys() {
                            let p = src.borrow().props.get(&k).cloned().unwrap();
                            dst.borrow_mut().props.insert(k, p);
                        }
                        // A subclass instance inherits the built-in's exotic behavior (e.g. an Array
                        // subclass is itself an Array exotic).
                        let src_exotic = src.borrow().exotic.clone();
                        if !matches!(src_exotic, crate::value::Exotic::None) {
                            dst.borrow_mut().exotic = src_exotic;
                        }
                        // Move the native object's internal slots (Map/Set/TypedArray/buffer/etc.)
                        // onto `this`, so a subclass instance carries the built-in's state.
                        let (sp, dp) = (Rc::as_ptr(src) as usize, Rc::as_ptr(dst) as usize);
                        if let Some(v) = self.map_data.remove(&sp) {
                            self.map_data.insert(dp, v);
                        }
                        if let Some(v) = self.typed_arrays.remove(&sp) {
                            self.typed_arrays.insert(dp, v);
                        }
                        // The TypedArray's `buffer` slot lives in a parallel side table keyed by the
                        // view's pointer, so it must move to `this` alongside its TaInfo.
                        if let Some(v) = self.ta_buffer.remove(&sp) {
                            self.ta_buffer.insert(dp, v);
                        }
                        if let Some(v) = self.data_views.remove(&sp) {
                            self.data_views.insert(dp, v);
                        }
                        if let Some(v) = self.array_buffers.remove(&sp) {
                            self.array_buffers.insert(dp, v);
                        }
                        if let Some(v) = self.regexps.remove(&sp) {
                            self.regexps.insert(dp, v);
                        }
                        if let Some(v) = self.promises.remove(&sp) {
                            self.promises.insert(dp, v);
                        }
                        if let Some(v) = self.temporal.remove(&sp) {
                            self.temporal.insert(dp, v);
                        }
                    }
                }
                Ok(())
            }
            _ => Err(self.throw("TypeError", "super target is not a constructor")),
        }
    }

    fn init_instance_fields(&mut self, ctor: &Value, this: &Value) -> Result<(), Abrupt> {
        let obj = match ctor {
            Value::Obj(o) => o.clone(),
            _ => return Ok(()),
        };
        let ptr = Rc::as_ptr(&obj) as usize;
        let (fields, field_env, initializers) = match self.class_info.get(&ptr) {
            Some(i) => (
                i.fields
                    .iter()
                    .map(|f| (f.key.clone(), f.init.clone(), f.transforms.clone()))
                    .collect::<Vec<_>>(),
                i.field_env.clone(),
                i.instance_initializers.clone(),
            ),
            None => return Ok(()),
        };
        // A field initializer is not a constructor: a `super(...)` reached from here (e.g. through a
        // direct `eval`) is illegal, so clear the flag for the duration of the initializers.
        let saved_super = self.super_call_ok;
        self.super_call_ok = false;
        let result = (|me: &mut Self| -> Result<(), Abrupt> {
            for (key, init, transforms) in fields {
                let scope = new_scope(Some(field_env.clone()));
                bind(&scope, "this", this.clone());
                let mut v = match init {
                    Some(e) => {
                        let v = me.eval(&e, &scope)?;
                        if is_anonymous_fn(&e) {
                            me.set_fn_name(&v, &key);
                        }
                        v
                    }
                    None => Value::Undefined,
                };
                // Decorator-supplied field initializers transform the value in turn.
                for t in &transforms {
                    v = me.call(t.clone(), this.clone(), &[v])?;
                }
                me.set_member(this, &key, v)?;
            }
            // Decorator addInitializer callbacks run after the fields, with `this` = the instance.
            for init in &initializers {
                me.call(init.clone(), this.clone(), &[])?;
            }
            Ok(())
        })(self);
        self.super_call_ok = saved_super;
        result
    }

    fn eval_unary(&mut self, op: &str, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        if op == "typeof" {
            // typeof on an unresolved identifier yields "undefined" rather than throwing.
            if let Expr::Ident(name) = arg {
                match self.get_var(name, env) {
                    Ok(v) => return Ok(Value::from_string(v.type_of().to_string())),
                    // A binding in its temporal dead zone still throws; only a truly-unresolved
                    // name yields "undefined".
                    Err(e) if self.binding_in_tdz(name, env) => return Err(e),
                    Err(_) => return Ok(Value::str("undefined")),
                }
            }
            let v = self.eval(arg, env)?;
            return Ok(Value::from_string(v.type_of().to_string()));
        }
        if op == "delete" {
            return self.eval_delete(arg, env);
        }
        let v = self.eval(arg, env)?;
        if let Value::BigInt(n) = v {
            return match op {
                "!" => Ok(Value::Bool(n == 0)),
                "-" => Ok(Value::BigInt(n.wrapping_neg())),
                "~" => Ok(Value::BigInt(!n)),
                "void" => Ok(Value::Undefined),
                "+" => Err(self.throw("TypeError", "Cannot convert a BigInt value to a number")),
                _ => unreachable!("unary {op}"),
            };
        }
        match op {
            "!" => Ok(Value::Bool(!self.to_boolean(&v))),
            "-" => Ok(Value::Num(-self.to_number(&v)?)),
            "+" => Ok(Value::Num(self.to_number(&v)?)),
            "~" => Ok(Value::Num(!(self.to_int32(&v)?) as f64)),
            "void" => Ok(Value::Undefined),
            _ => unreachable!("unary {op}"),
        }
    }

    fn eval_delete(&mut self, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        match arg {
            Expr::Member { obj, prop, .. } => {
                let base = self.eval(obj, env)?;
                if let Value::Obj(o) = &base {
                    let ptr = Rc::as_ptr(o) as usize;
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        let ok = self.proxy_delete(target, handler, prop)?;
                        if !ok && self.strict {
                            return Err(
                                self.throw("TypeError", format!("cannot delete property '{prop}'"))
                            );
                        }
                        return Ok(Value::Bool(ok));
                    }
                    let configurable = o
                        .borrow()
                        .props
                        .get(prop)
                        .map(|p| p.configurable)
                        .unwrap_or(true);
                    if configurable {
                        o.borrow_mut().props.remove(prop);
                        return Ok(Value::Bool(true));
                    }
                    // [[Delete]] returned false: strict-mode `delete` throws.
                    if self.strict {
                        return Err(self.throw(
                            "TypeError",
                            format!("cannot delete non-configurable property '{prop}'"),
                        ));
                    }
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(true))
            }
            Expr::Index { obj, index, .. } => {
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                if let Value::Obj(o) = &base {
                    let ptr = Rc::as_ptr(o) as usize;
                    if let Some((target, handler)) = self.proxies.get(&ptr).cloned() {
                        let ok = self.proxy_delete(target, handler, &key)?;
                        if !ok && self.strict {
                            return Err(
                                self.throw("TypeError", format!("cannot delete property '{key}'"))
                            );
                        }
                        return Ok(Value::Bool(ok));
                    }
                    // A TypedArray integer index can't be deleted ([[Delete]] → false; strict throws);
                    // a canonical-numeric non-index reports success.
                    if let Some(info) = self.typed_arrays.get(&ptr).copied() {
                        match self.ta_index_kind(&info, &key) {
                            TaIndex::Element(_) => {
                                if self.strict {
                                    return Err(
                                        self.throw("TypeError", "cannot delete a TypedArray index")
                                    );
                                }
                                return Ok(Value::Bool(false));
                            }
                            TaIndex::Exotic => return Ok(Value::Bool(true)),
                            TaIndex::Ordinary => {}
                        }
                    }
                    let configurable = o
                        .borrow()
                        .props
                        .get(&key)
                        .map(|p| p.configurable)
                        .unwrap_or(true);
                    if configurable {
                        o.borrow_mut().props.remove(&key);
                        return Ok(Value::Bool(true));
                    }
                    if self.strict {
                        return Err(self.throw(
                            "TypeError",
                            format!("cannot delete non-configurable property '{key}'"),
                        ));
                    }
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Bool(true))
            }
            // `delete <identifier>`: an environment binding is removable only if it is `deletable`
            // (a `var`/function created by a sloppy `eval`); a global-object property follows its
            // own configurability. An unresolvable reference deletes to `true`.
            Expr::Ident(name) => {
                let mut cur = Some(env.clone());
                while let Some(s) = cur {
                    let (has, deletable, with_obj, parent) = {
                        let b = s.borrow();
                        let binding = b.vars.get(name);
                        (
                            binding.is_some(),
                            binding.map(|x| x.deletable).unwrap_or(false),
                            b.with_obj.clone(),
                            b.parent.clone(),
                        )
                    };
                    if has {
                        if deletable {
                            s.borrow_mut().vars.remove(name);
                            return Ok(Value::Bool(true));
                        }
                        return Ok(Value::Bool(false));
                    }
                    if let Some(Value::Obj(o)) = &with_obj {
                        if self.js_has_property(&Value::Obj(o.clone()), name)? {
                            let configurable = o
                                .borrow()
                                .props
                                .get(name)
                                .map(|p| p.configurable)
                                .unwrap_or(true);
                            if configurable {
                                o.borrow_mut().props.remove(name);
                                return Ok(Value::Bool(true));
                            }
                            return Ok(Value::Bool(false));
                        }
                    }
                    cur = parent;
                }
                // Fall back to a global-object property.
                let g = self.global.clone();
                let existing = g.borrow().props.get(name).map(|p| p.configurable);
                match existing {
                    Some(true) => {
                        g.borrow_mut().props.remove(name);
                        Ok(Value::Bool(true))
                    }
                    Some(false) => Ok(Value::Bool(false)),
                    None => Ok(Value::Bool(true)),
                }
            }
            _ => Ok(Value::Bool(true)),
        }
    }

    /// Proxy `[[Delete]]`: call the deleteProperty trap, or forward to the target.
    pub(crate) fn proxy_delete(
        &mut self,
        target: Value,
        handler: Value,
        key: &str,
    ) -> Result<bool, Abrupt> {
        if matches!(handler, Value::Null) {
            return Err(self.throw("TypeError", "cannot delete on a revoked proxy"));
        }
        let trap = self.get_member(&handler, "deleteProperty")?;
        if matches!(trap, Value::Undefined | Value::Null) {
            // Forward to the target's [[Delete]] (recursing if the target is itself a proxy).
            if let Value::Obj(t) = &target {
                let tptr = Rc::as_ptr(t) as usize;
                if let Some((t2, h2)) = self.proxies.get(&tptr).cloned() {
                    return self.proxy_delete(t2, h2, key);
                }
                let configurable = t
                    .borrow()
                    .props
                    .get(key)
                    .map(|p| p.configurable)
                    .unwrap_or(true);
                if configurable {
                    t.borrow_mut().props.remove(key);
                    return Ok(true);
                }
                return Ok(false);
            }
            return Ok(true);
        }
        if !trap.is_callable() {
            return Err(self.throw("TypeError", "proxy 'deleteProperty' trap is not callable"));
        }
        let kv = self
            .sym_from_key(key)
            .unwrap_or_else(|| Value::from_string(key.to_string()));
        let res = self.call(trap, handler, &[target.clone(), kv])?;
        if !self.to_boolean(&res) {
            return Ok(false);
        }
        // Invariant: a non-configurable property, or any property of a non-extensible target,
        // can't be reported as deleted.
        if let Value::Obj(t) = &target {
            let p = t.borrow().props.get(key).cloned();
            if let Some(p) = p {
                if !p.configurable {
                    return Err(self.throw(
                        "TypeError",
                        "proxy 'deleteProperty' removed a non-configurable property",
                    ));
                }
                if !t.borrow().extensible {
                    return Err(self.throw(
                        "TypeError",
                        "proxy 'deleteProperty' removed a non-extensible target's property",
                    ));
                }
            }
        }
        Ok(true)
    }

    fn eval_update(
        &mut self,
        op: &str,
        prefix: bool,
        arg: &Expr,
        env: &Env,
    ) -> Result<Value, Abrupt> {
        let old = self.eval(arg, env)?;
        if let Value::BigInt(n) = old {
            let new = if op == "++" {
                n.wrapping_add(1)
            } else {
                n.wrapping_sub(1)
            };
            self.assign_to_target(arg, Value::BigInt(new), env)?;
            return Ok(Value::BigInt(if prefix { new } else { n }));
        }
        let n = self.to_number(&old)?;
        let new = if op == "++" { n + 1.0 } else { n - 1.0 };
        self.assign_to_target(arg, Value::Num(new), env)?;
        Ok(Value::Num(if prefix { new } else { n }))
    }

    fn eval_assign(
        &mut self,
        op: &str,
        target: &Expr,
        value: &Expr,
        env: &Env,
    ) -> Result<Value, Abrupt> {
        if op == "=" {
            let v = self.eval(value, env)?;
            // `f = function(){}` names the anonymous function after the target identifier.
            if let Expr::Ident(n) = target {
                if is_anonymous_fn(value) {
                    self.set_fn_name(&v, n);
                }
            }
            self.assign_to_target(target, v.clone(), env)?;
            return Ok(v);
        }
        // Logical assignment (&&=, ||=, ??=) short-circuits.
        if matches!(op, "&&=" | "||=" | "??=") {
            let cur = self.eval(target, env)?;
            let do_assign = match op {
                "&&=" => self.to_boolean(&cur),
                "||=" => !self.to_boolean(&cur),
                "??=" => matches!(cur, Value::Undefined | Value::Null),
                _ => unreachable!(),
            };
            if !do_assign {
                return Ok(cur);
            }
            let v = self.eval(value, env)?;
            self.assign_to_target(target, v.clone(), env)?;
            return Ok(v);
        }
        // Compound arithmetic/bitwise: a op= b  ≡  a = a <op> b.
        let cur = self.eval(target, env)?;
        let rhs = self.eval(value, env)?;
        let bin_op = &op[..op.len() - 1];
        let result = self.binary(bin_op, cur, rhs)?;
        self.assign_to_target(target, result.clone(), env)?;
        Ok(result)
    }

    fn assign_to_target(&mut self, target: &Expr, value: Value, env: &Env) -> Result<(), Abrupt> {
        match target {
            Expr::Ident(name) => self.assign_var(name, value, env),
            Expr::Member { obj, prop, .. } => {
                // `super.x = v` resolves the super base for invariant checks but writes through the
                // `this` receiver (CreateDataProperty on the actual object), per [[Set]] semantics.
                if matches!(**obj, Expr::Super) {
                    self.super_base(env)?;
                    let this = self.get_var("this", env)?;
                    return self.set_member(&this, prop, value);
                }
                let base = self.eval(obj, env)?;
                self.set_member(&base, prop, value)
            }
            Expr::Index { obj, index, .. } => {
                if matches!(**obj, Expr::Super) {
                    self.super_base(env)?;
                    let idx = self.eval(index, env)?;
                    let key = self.to_property_key(&idx)?;
                    let this = self.get_var("this", env)?;
                    return self.set_member(&this, &key, value);
                }
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                self.set_member(&base, &key, value)
            }
            // Destructuring assignment: an array/object literal reinterpreted as a target.
            Expr::Array(elems) => {
                let (iter, next) = self.get_iterator(&value)?;
                let iter_close = iter.clone();
                let mut done = false;
                let result = (|me: &mut Self| -> Result<(), Abrupt> {
                    for el in elems {
                        match el {
                            ArrayElem::Hole => {
                                if !done && me.iterator_step(&iter, &next)?.is_none() {
                                    done = true;
                                }
                            }
                            ArrayElem::Spread(t) => {
                                let mut rest = Vec::new();
                                while !done {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => rest.push(x),
                                        None => done = true,
                                    }
                                }
                                let arr = me.make_array(rest);
                                me.assign_to_target(t, arr, env)?;
                            }
                            ArrayElem::Item(t) => {
                                let v = if done {
                                    Value::Undefined
                                } else {
                                    match me.iterator_step(&iter, &next)? {
                                        Some(x) => x,
                                        None => {
                                            done = true;
                                            Value::Undefined
                                        }
                                    }
                                };
                                me.assign_destructure_elem(t, v, env)?;
                            }
                        }
                    }
                    Ok(())
                })(self);
                if !done {
                    self.iterator_close(&iter_close);
                }
                result
            }
            Expr::Object(props) => {
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(self.throw("TypeError", "cannot destructure null or undefined"));
                }
                let mut taken: Vec<String> = Vec::new();
                for prop in props {
                    match prop {
                        PropDef::KeyValue { key, value: t } => {
                            let k = self.propkey_to_string(key, env)?;
                            let v = self.get_member(&value, &k)?;
                            taken.push(k);
                            self.assign_destructure_elem(t, v, env)?;
                        }
                        PropDef::Spread(t) => {
                            let rest = self.new_object();
                            if let Value::Obj(src) = &value {
                                let keys: Vec<Rc<str>> = src.borrow().props.keys();
                                for k in keys {
                                    if Interp::is_sym_key(&k)
                                        || taken.iter().any(|x| x.as_str() == &*k)
                                    {
                                        continue;
                                    }
                                    let enumerable = src
                                        .borrow()
                                        .props
                                        .get(&k)
                                        .map(|p| p.enumerable)
                                        .unwrap_or(false);
                                    if enumerable {
                                        let v = self.get_member(&value, &k)?;
                                        rest.borrow_mut()
                                            .props
                                            .insert(k, crate::value::Property::plain(v));
                                    }
                                }
                            }
                            self.assign_to_target(t, Value::Obj(rest), env)?;
                        }
                        _ => return Err(self.throw("SyntaxError", "invalid destructuring target")),
                    }
                }
                Ok(())
            }
            _ => Err(self.throw("ReferenceError", "invalid assignment target")),
        }
    }

    /// Assign one destructured element, honoring a `target = default` cover.
    fn assign_destructure_elem(&mut self, t: &Expr, v: Value, env: &Env) -> Result<(), Abrupt> {
        if let Expr::Assign {
            op: "=",
            target,
            value: dflt,
        } = t
        {
            let v = if matches!(v, Value::Undefined) {
                let dv = self.eval(dflt, env)?;
                if let (Expr::Ident(n), true) = (&**target, is_anonymous_fn(dflt)) {
                    self.set_fn_name(&dv, n);
                }
                dv
            } else {
                v
            };
            self.assign_to_target(target, v, env)
        } else {
            self.assign_to_target(t, v, env)
        }
    }

    fn propkey_to_string(&mut self, key: &PropKey, env: &Env) -> Result<String, Abrupt> {
        Ok(match key {
            PropKey::Ident(s) => s.clone(),
            PropKey::Str(s) => s.to_string(),
            PropKey::Num(n) => self.num_to_str(*n),
            PropKey::Computed(e) => {
                let kv = self.eval(e, env)?;
                self.to_property_key(&kv)?
            }
        })
    }

    // ----- operators --------------------------------------------------------------------------

    fn binary(&mut self, op: &str, l: Value, r: Value) -> Result<Value, Abrupt> {
        // Arithmetic, bitwise, and `+` convert both operands to primitives first (left then right),
        // then dispatch on BigInt / string (for `+`) / number. ToPrimitive runs before the BigInt
        // mixing check so a wrapped BigInt object coerces correctly.
        if matches!(
            op,
            "+" | "-" | "*" | "/" | "%" | "**" | "&" | "|" | "^" | "<<" | ">>"
        ) {
            let hint = if op == "+" {
                Hint::Default
            } else {
                Hint::Number
            };
            let lp = self.to_primitive(&l, hint)?;
            let rp = self.to_primitive(&r, hint)?;
            if op == "+" && (matches!(lp, Value::Str(_)) || matches!(rp, Value::Str(_))) {
                let ls = self.to_string(&lp)?;
                let rs = self.to_string(&rp)?;
                if ls.len() + rs.len() > MAX_STR_LEN {
                    return Err(self.throw("RangeError", "Invalid string length"));
                }
                return Ok(Value::from_string(format!("{ls}{rs}")));
            }
            if matches!(lp, Value::BigInt(_)) || matches!(rp, Value::BigInt(_)) {
                if let (Value::BigInt(x), Value::BigInt(y)) = (&lp, &rp) {
                    return self.bigint_binop(op, *x, *y);
                }
                return Err(self.throw(
                    "TypeError",
                    "Cannot mix BigInt and other types, use explicit conversions",
                ));
            }
            return match op {
                "+" => Ok(Value::Num(self.to_number(&lp)? + self.to_number(&rp)?)),
                "-" => Ok(Value::Num(self.to_number(&lp)? - self.to_number(&rp)?)),
                "*" => Ok(Value::Num(self.to_number(&lp)? * self.to_number(&rp)?)),
                "/" => Ok(Value::Num(self.to_number(&lp)? / self.to_number(&rp)?)),
                "%" => {
                    let a = self.to_number(&lp)?;
                    let b = self.to_number(&rp)?;
                    Ok(Value::Num(js_mod(a, b)))
                }
                "**" => Ok(Value::Num(self.to_number(&lp)?.powf(self.to_number(&rp)?))),
                "&" => Ok(Value::Num(
                    (self.to_int32(&lp)? & self.to_int32(&rp)?) as f64,
                )),
                "|" => Ok(Value::Num(
                    (self.to_int32(&lp)? | self.to_int32(&rp)?) as f64,
                )),
                "^" => Ok(Value::Num(
                    (self.to_int32(&lp)? ^ self.to_int32(&rp)?) as f64,
                )),
                "<<" => {
                    let a = self.to_int32(&lp)?;
                    let b = (self.to_uint32(&rp)?) & 31;
                    Ok(Value::Num((a.wrapping_shl(b)) as f64))
                }
                ">>" => {
                    let a = self.to_int32(&lp)?;
                    let b = (self.to_uint32(&rp)?) & 31;
                    Ok(Value::Num((a >> b) as f64))
                }
                _ => unreachable!(),
            };
        }
        match op {
            "==" => Ok(Value::Bool(self.loose_equals(&l, &r)?)),
            "!=" => Ok(Value::Bool(!self.loose_equals(&l, &r)?)),
            "===" => Ok(Value::Bool(self.strict_equals(&l, &r))),
            "!==" => Ok(Value::Bool(!self.strict_equals(&l, &r))),
            "<" | ">" | "<=" | ">=" => self.compare(op, l, r),
            "&" => Ok(Value::Num((self.to_int32(&l)? & self.to_int32(&r)?) as f64)),
            "|" => Ok(Value::Num((self.to_int32(&l)? | self.to_int32(&r)?) as f64)),
            "^" => Ok(Value::Num((self.to_int32(&l)? ^ self.to_int32(&r)?) as f64)),
            "<<" => {
                let a = self.to_int32(&l)?;
                let b = (self.to_uint32(&r)?) & 31;
                Ok(Value::Num((a.wrapping_shl(b)) as f64))
            }
            ">>" => {
                let a = self.to_int32(&l)?;
                let b = (self.to_uint32(&r)?) & 31;
                Ok(Value::Num((a >> b) as f64))
            }
            ">>>" => {
                let a = self.to_uint32(&l)?;
                let b = (self.to_uint32(&r)?) & 31;
                Ok(Value::Num((a >> b) as f64))
            }
            "instanceof" => self.instanceof(&l, &r),
            "in" => {
                if matches!(&r, Value::Obj(_)) {
                    let key = self.to_property_key(&l)?;
                    Ok(Value::Bool(self.js_has_property(&r, &key)?))
                } else {
                    Err(self.throw("TypeError", "'in' requires an object on the right"))
                }
            }
            _ => unreachable!("binary {op}"),
        }
    }

    fn bigint_binop(&self, op: &str, x: i128, y: i128) -> Result<Value, Abrupt> {
        let v = match op {
            "+" => x.wrapping_add(y),
            "-" => x.wrapping_sub(y),
            "*" => x.wrapping_mul(y),
            "/" => {
                if y == 0 {
                    return Err(self.throw("RangeError", "Division by zero"));
                }
                x.wrapping_div(y)
            }
            "%" => {
                if y == 0 {
                    return Err(self.throw("RangeError", "Division by zero"));
                }
                x.wrapping_rem(y)
            }
            "**" => {
                if y < 0 {
                    return Err(self.throw("RangeError", "Exponent must be non-negative"));
                }
                x.wrapping_pow(y.min(u32::MAX as i128) as u32)
            }
            "&" => x & y,
            "|" => x | y,
            "^" => x ^ y,
            "<<" => x.wrapping_shl(y.clamp(0, 127) as u32),
            ">>" => x.wrapping_shr(y.clamp(0, 127) as u32),
            _ => return Err(self.throw("TypeError", "unsupported BigInt operator")),
        };
        Ok(Value::BigInt(v))
    }

    /// ToBigInt: BigInt stays; Boolean → 0n/1n; String parses (SyntaxError if malformed); Number /
    /// undefined / null / Symbol throw TypeError.
    pub(crate) fn to_bigint(&mut self, v: &Value) -> Result<i128, Abrupt> {
        let p = self.to_primitive(v, Hint::Number)?;
        match p {
            Value::BigInt(n) => Ok(n),
            Value::Bool(b) => Ok(b as i128),
            Value::Str(s) => {
                let t = s.trim();
                let parsed = if t.is_empty() {
                    Some(0)
                } else if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    i128::from_str_radix(h, 16).ok()
                } else if let Some(o) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
                    i128::from_str_radix(o, 8).ok()
                } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                    i128::from_str_radix(b, 2).ok()
                } else {
                    t.parse::<i128>().ok()
                };
                parsed.ok_or_else(|| self.throw("SyntaxError", "Cannot convert string to a BigInt"))
            }
            Value::Num(_) => Err(self.throw("TypeError", "Cannot convert a Number to a BigInt")),
            _ => Err(self.throw("TypeError", "Cannot convert value to a BigInt")),
        }
    }

    fn compare(&mut self, op: &str, l: Value, r: Value) -> Result<Value, Abrupt> {
        let lp = self.to_primitive(&l, Hint::Number)?;
        let rp = self.to_primitive(&r, Hint::Number)?;
        if let (Value::Str(a), Value::Str(b)) = (&lp, &rp) {
            let res = match op {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                ">=" => a >= b,
                _ => unreachable!(),
            };
            return Ok(Value::Bool(res));
        }
        // Comparisons accept mixed BigInt/Number operands (compared as real numbers).
        let bigint_f64 = |v: &Value| -> Option<f64> {
            match v {
                Value::BigInt(n) => Some(*n as f64),
                _ => None,
            }
        };
        let a = match bigint_f64(&lp) {
            Some(n) => n,
            None => self.to_number(&lp)?,
        };
        let b = match bigint_f64(&rp) {
            Some(n) => n,
            None => self.to_number(&rp)?,
        };
        if a.is_nan() || b.is_nan() {
            return Ok(Value::Bool(false));
        }
        let res = match op {
            "<" => a < b,
            ">" => a > b,
            "<=" => a <= b,
            ">=" => a >= b,
            _ => unreachable!(),
        };
        Ok(Value::Bool(res))
    }

    fn instanceof(&mut self, l: &Value, r: &Value) -> Result<Value, Abrupt> {
        if !matches!(r, Value::Obj(_)) {
            return Err(self.throw(
                "TypeError",
                "right-hand side of instanceof is not an object",
            ));
        }
        // Defer to a `@@hasInstance` method if the RHS has one.
        if let Some(key) = crate::builtins::well_known_key(self, "hasInstance") {
            let handler = self.get_member(r, &key)?;
            if handler.is_callable() {
                let res = self.call(handler, r.clone(), std::slice::from_ref(l))?;
                return Ok(Value::Bool(self.to_boolean(&res)));
            }
        }
        if !r.is_callable() {
            return Err(self.throw("TypeError", "right-hand side of instanceof is not callable"));
        }
        Ok(Value::Bool(self.ordinary_has_instance(r, l)?))
    }

    /// OrdinaryHasInstance(C, O): unwrap bound functions to their target, then test whether `C`'s
    /// `prototype` appears on `O`'s prototype chain.
    pub fn ordinary_has_instance(&mut self, c: &Value, o: &Value) -> Result<bool, Abrupt> {
        let co = match c {
            Value::Obj(x) if !matches!(x.borrow().call, Callable::None) => x.clone(),
            _ => return Ok(false),
        };
        if let Callable::Bound { target, .. } = &co.borrow().call {
            let target = Value::Obj(target.clone());
            return self.ordinary_has_instance(&target, o);
        }
        let o_obj = match o {
            Value::Obj(x) => x.clone(),
            _ => return Ok(false),
        };
        let proto = match self.get_member(c, "prototype")? {
            Value::Obj(p) => p,
            _ => return Err(self.throw("TypeError", "prototype property is not an object")),
        };
        // Walk O's prototype chain via [[GetPrototypeOf]] (so a proxy's trap participates).
        let mut cur = Value::Obj(o_obj);
        loop {
            let next = crate::builtins::js_get_prototype_of(self, &cur).map_err(Abrupt::Throw)?;
            match next {
                Value::Obj(x) => {
                    if Rc::ptr_eq(&x, &proto) {
                        return Ok(true);
                    }
                    cur = Value::Obj(x);
                }
                _ => return Ok(false),
            }
        }
    }

    // ----- abstract operations ----------------------------------------------------------------

    pub fn to_boolean(&self, v: &Value) -> bool {
        match v {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::BigInt(n) => *n != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Sym(_) | Value::Obj(_) => true,
        }
    }

    pub fn to_number(&mut self, v: &Value) -> Result<f64, Abrupt> {
        Ok(match v {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Num(n) => *n,
            Value::BigInt(_) => {
                return Err(self.throw("TypeError", "Cannot convert a BigInt value to a number"))
            }
            Value::Str(s) => parse_number(s),
            Value::Sym(_) => {
                return Err(self.throw("TypeError", "Cannot convert a Symbol value to a number"))
            }
            Value::Obj(_) => {
                let p = self.to_primitive(v, Hint::Number)?;
                self.to_number(&p)?
            }
        })
    }

    pub fn to_int32(&mut self, v: &Value) -> Result<i32, Abrupt> {
        let n = self.to_number(v)?;
        Ok(to_int32(n))
    }
    pub fn to_uint32(&mut self, v: &Value) -> Result<u32, Abrupt> {
        let n = self.to_number(v)?;
        Ok(to_int32(n) as u32)
    }

    pub fn to_string(&mut self, v: &Value) -> Result<Rc<str>, Abrupt> {
        Ok(match v {
            Value::Undefined => Rc::from("undefined"),
            Value::Null => Rc::from("null"),
            Value::Bool(b) => Rc::from(if *b { "true" } else { "false" }),
            Value::Num(n) => Rc::from(self.num_to_str(*n).as_str()),
            Value::BigInt(n) => Rc::from(n.to_string().as_str()),
            Value::Str(s) => s.clone(),
            Value::Sym(_) => {
                return Err(self.throw("TypeError", "Cannot convert a Symbol value to a string"))
            }
            Value::Obj(_) => {
                let p = self.to_primitive(v, Hint::String)?;
                match p {
                    Value::Obj(_) => {
                        return Err(self.throw("TypeError", "cannot convert object to string"))
                    }
                    other => self.to_string(&other)?,
                }
            }
        })
    }

    pub fn to_property_key(&mut self, v: &Value) -> Result<String, Abrupt> {
        // A symbol key maps to its internal NUL-prefixed key; everything else is its string form.
        if let Value::Sym(s) = v {
            return Ok(Interp::sym_key(s));
        }
        // ToPropertyKey: ToPrimitive(hint String) first — a Symbol result stays a symbol key (rather
        // than being stringified, which would throw), so `obj[wrapperWhoseToStringReturnsASymbol]`
        // is a symbol-keyed access.
        let prim = self.to_primitive(v, Hint::String)?;
        if let Value::Sym(s) = &prim {
            return Ok(Interp::sym_key(s));
        }
        Ok(self.to_string(&prim)?.to_string())
    }

    /// The internal property key for a well-known symbol (e.g. `Symbol.toPrimitive`).
    fn well_known_sym_key(&self, name: &str) -> Option<String> {
        let sym = self
            .global
            .borrow()
            .props
            .get("Symbol")
            .map(|p| p.value.clone())?;
        if let Value::Obj(o) = sym {
            if let Some(p) = o.borrow().props.get(name) {
                if let Value::Sym(d) = &p.value {
                    return Some(Interp::sym_key(d));
                }
            }
        }
        None
    }

    pub fn to_primitive(&mut self, v: &Value, hint: Hint) -> Result<Value, Abrupt> {
        let obj = match v {
            Value::Obj(o) => o.clone(),
            _ => return Ok(v.clone()),
        };
        // A `@@toPrimitive` method takes precedence over valueOf/toString.
        if let Some(key) = self.well_known_sym_key("toPrimitive") {
            let f = self.get_member(&Value::Obj(obj.clone()), &key)?;
            // GetMethod: a present-but-non-callable @@toPrimitive (not undefined/null) is a TypeError.
            if !matches!(f, Value::Undefined | Value::Null) && !f.is_callable() {
                return Err(self.throw("TypeError", "@@toPrimitive is not callable"));
            }
            if f.is_callable() {
                let hint_str = match hint {
                    Hint::String => "string",
                    Hint::Number => "number",
                    Hint::Default => "default",
                };
                let r = self.call(f, v.clone(), &[Value::str(hint_str)])?;
                if matches!(r, Value::Obj(_)) {
                    return Err(
                        self.throw("TypeError", "Cannot convert object to a primitive value")
                    );
                }
                return Ok(r);
            }
        }
        let order: [&str; 2] = match hint {
            Hint::String => ["toString", "valueOf"],
            _ => ["valueOf", "toString"],
        };
        for method in order {
            let f = self.get_member(&Value::Obj(obj.clone()), method)?;
            if f.is_callable() {
                let r = self.call(f, v.clone(), &[])?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        Err(self.throw("TypeError", "cannot convert object to primitive value"))
    }

    /// ECMAScript `Number::toString` (base 10): the shortest round-tripping digit string, formatted
    /// fixed or exponential per the spec's exponent thresholds (≥1e21 or <1e-6 → exponential).
    pub fn num_to_str(&self, n: f64) -> String {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n == 0.0 {
            return "0".to_string();
        }
        if n.is_infinite() {
            return if n > 0.0 {
                "Infinity".to_string()
            } else {
                "-Infinity".to_string()
            };
        }
        let neg = n < 0.0;
        // Rust's `{:e}` yields the shortest round-tripping mantissa + exponent (`d[.ddd]e±E`).
        let sci = format!("{:e}", n.abs());
        let (mantissa, exp_str) = sci.split_once('e').unwrap();
        let exp: i32 = exp_str.parse().unwrap();
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        let digits = digits.trim_end_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        let k = digits.len() as i32;
        let np = exp + 1; // the spec's `n`: value = digits × 10^(n-k)
        let body = if k <= np && np <= 21 {
            format!("{}{}", digits, "0".repeat((np - k) as usize))
        } else if 0 < np && np <= 21 {
            format!("{}.{}", &digits[..np as usize], &digits[np as usize..])
        } else if -6 < np && np <= 0 {
            format!("0.{}{}", "0".repeat((-np) as usize), digits)
        } else {
            let exp_part = np - 1;
            let sign = if exp_part >= 0 { "+" } else { "-" };
            if k == 1 {
                format!("{}e{}{}", digits, sign, exp_part.abs())
            } else {
                format!(
                    "{}.{}e{}{}",
                    &digits[..1],
                    &digits[1..],
                    sign,
                    exp_part.abs()
                )
            }
        };
        if neg {
            format!("-{body}")
        } else {
            body
        }
    }

    pub fn strict_equals(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Undefined, Value::Undefined) => true,
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Num(x), Value::Num(y)) => x == y,
            (Value::BigInt(x), Value::BigInt(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Sym(x), Value::Sym(y)) => x.id == y.id,
            (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    pub fn loose_equals(&mut self, a: &Value, b: &Value) -> Result<bool, Abrupt> {
        Ok(match (a, b) {
            (Value::Undefined | Value::Null, Value::Undefined | Value::Null) => true,
            (Value::BigInt(x), Value::BigInt(y)) => x == y,
            (Value::BigInt(x), Value::Num(y)) | (Value::Num(y), Value::BigInt(x)) => {
                y.is_finite() && y.fract() == 0.0 && (*x as f64) == *y
            }
            (Value::BigInt(x), Value::Str(s)) | (Value::Str(s), Value::BigInt(x)) => {
                s.trim().parse::<i128>().map(|n| n == *x).unwrap_or(false)
            }
            (Value::BigInt(_), Value::Obj(_)) => {
                let bp = self.to_primitive(b, Hint::Default)?;
                self.loose_equals(a, &bp)?
            }
            (Value::Obj(_), Value::BigInt(_)) => {
                let ap = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(&ap, b)?
            }
            (Value::Num(_), Value::Num(_))
            | (Value::Str(_), Value::Str(_))
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Sym(_), Value::Sym(_))
            | (Value::Obj(_), Value::Obj(_)) => self.strict_equals(a, b),
            (Value::Num(_), Value::Str(_)) => {
                let bn = self.to_number(b)?;
                self.strict_equals(a, &Value::Num(bn))
            }
            (Value::Str(_), Value::Num(_)) => {
                let an = self.to_number(a)?;
                self.strict_equals(&Value::Num(an), b)
            }
            (Value::Bool(_), _) => {
                let an = self.to_number(a)?;
                self.loose_equals(&Value::Num(an), b)?
            }
            (_, Value::Bool(_)) => {
                let bn = self.to_number(b)?;
                self.loose_equals(a, &Value::Num(bn))?
            }
            (Value::Obj(_), Value::Num(_) | Value::Str(_)) => {
                let ap = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(&ap, b)?
            }
            (Value::Num(_) | Value::Str(_), Value::Obj(_)) => {
                let bp = self.to_primitive(b, Hint::Default)?;
                self.loose_equals(a, &bp)?
            }
            _ => false,
        })
    }
}

#[derive(Clone, Copy)]
pub enum Hint {
    Default,
    Number,
    String,
}

enum LoopStep {
    /// Keep looping; the value is this iteration's body completion value (for the loop's own
    /// completion value, per UpdateEmpty in ForBodyEvaluation).
    Continue(Value),
    /// Stop looping; the value is the final iteration's body completion value (or undefined).
    Done(Value),
}

/// Insert an initialized, mutable binding into `env` (used for the hidden `%super*%`/`this` slots).
fn bind(env: &Env, name: &str, value: Value) {
    env.borrow_mut().vars.insert(
        name.to_string(),
        Binding {
            value,
            mutable: true,
            initialized: true,
            import_ref: None,
            deletable: false,
        },
    );
}

fn opt_obj(o: &Option<Gc>) -> Value {
    match o {
        Some(g) => Value::Obj(g.clone()),
        None => Value::Undefined,
    }
}

/// The synthesized default class constructor. Derived: `constructor(...args) { super(...args); }`;
/// base: `constructor() {}`.
fn default_constructor(derived: bool) -> Function {
    let body = if derived {
        vec![Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Super),
            args: vec![ArrayElem::Spread(Expr::Ident("args".to_string()))],
            optional: false,
        })]
    } else {
        Vec::new()
    };
    let params = if derived {
        vec![Param {
            pattern: Pattern::Ident("args".to_string()),
            default: None,
            rest: true,
        }]
    } else {
        Vec::new()
    };
    Function {
        name: None,
        params,
        body,
        is_arrow: false,
        is_strict: true,
        expr_body: false,
        is_generator: false,
        is_async: false,
        is_method: false,
    }
}

/// Whether any statement contains a `super(...)` call within its own super-context. The walk is
/// transparent through arrow functions and ordinary control flow but stops at a (non-arrow) function
/// or class boundary, each of which establishes a fresh super-context.
/// Whether a statement directly in a block is a `using` / `await using` declaration (so the block
/// is a disposal boundary). Does not recurse — disposal scopes are per lexical block.
pub(crate) fn stmt_declares_using(s: &Stmt) -> bool {
    matches!(
        crate::interpreter::unwrap_export(s),
        Stmt::VarDecl {
            kind: DeclKind::Using | DeclKind::AwaitUsing,
            ..
        }
    )
}

fn stmts_have_super_call(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_super_call)
}

fn stmt_has_super_call(s: &Stmt) -> bool {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => expr_has_super_call(e),
        Stmt::Return(e) => e.as_ref().is_some_and(expr_has_super_call),
        Stmt::VarDecl { decls, .. } => decls
            .iter()
            .any(|(_, init)| init.as_ref().is_some_and(expr_has_super_call)),
        Stmt::If { test, cons, alt } => {
            expr_has_super_call(test)
                || stmt_has_super_call(cons)
                || alt.as_deref().is_some_and(stmt_has_super_call)
        }
        Stmt::Block(b) => stmts_have_super_call(b),
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            expr_has_super_call(test) || stmt_has_super_call(body)
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_deref().is_some_and(|i| match i {
                ForInit::Expr(e) => expr_has_super_call(e),
                ForInit::VarDecl { decls, .. } => decls
                    .iter()
                    .any(|(_, x)| x.as_ref().is_some_and(expr_has_super_call)),
            }) || test.as_ref().is_some_and(expr_has_super_call)
                || update.as_ref().is_some_and(expr_has_super_call)
                || stmt_has_super_call(body)
        }
        Stmt::ForInOf { right, body, .. } => {
            expr_has_super_call(right) || stmt_has_super_call(body)
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            stmts_have_super_call(block)
                || handler
                    .as_ref()
                    .is_some_and(|(_, b)| stmts_have_super_call(b))
                || finalizer.as_ref().is_some_and(|b| stmts_have_super_call(b))
        }
        Stmt::Switch { disc, cases } => {
            expr_has_super_call(disc)
                || cases.iter().any(|c| {
                    c.test.as_ref().is_some_and(expr_has_super_call)
                        || stmts_have_super_call(&c.body)
                })
        }
        Stmt::Labeled { body, .. } | Stmt::With { body, .. } => stmt_has_super_call(body),
        // A (non-arrow) function or class declaration opens its own super-context.
        _ => false,
    }
}

fn expr_has_super_call(e: &Expr) -> bool {
    match e {
        Expr::Call { callee, args, .. } => {
            matches!(**callee, Expr::Super)
                || expr_has_super_call(callee)
                || args_have_super_call(args)
        }
        Expr::New { callee, args } => expr_has_super_call(callee) || args_have_super_call(args),
        Expr::Unary { arg, .. } | Expr::Update { arg, .. } | Expr::Await(arg) => {
            expr_has_super_call(arg)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            expr_has_super_call(left) || expr_has_super_call(right)
        }
        Expr::Assign { target, value, .. } => {
            expr_has_super_call(target) || expr_has_super_call(value)
        }
        Expr::Cond { test, cons, alt } => {
            expr_has_super_call(test) || expr_has_super_call(cons) || expr_has_super_call(alt)
        }
        Expr::Member { obj, .. } | Expr::OptionalChain(obj) => expr_has_super_call(obj),
        Expr::Index { obj, index, .. } => expr_has_super_call(obj) || expr_has_super_call(index),
        Expr::Seq(v) => v.iter().any(expr_has_super_call),
        Expr::Array(elems) => arr_elems_have_super_call(elems),
        Expr::Yield { arg, .. } => arg.as_deref().is_some_and(expr_has_super_call),
        Expr::ImportCall { spec, .. } => expr_has_super_call(spec),
        Expr::PrivateIn { obj, .. } => expr_has_super_call(obj),
        Expr::TaggedTemplate { tag, subs, .. } => {
            expr_has_super_call(tag) || subs.iter().any(expr_has_super_call)
        }
        Expr::Object(props) => props.iter().any(|p| match p {
            PropDef::KeyValue { value, .. } => expr_has_super_call(value),
            PropDef::Spread(e) => expr_has_super_call(e),
            // Methods/getters/setters open their own super-context.
            _ => false,
        }),
        // `Func`/`Class` (incl. arrows) open a fresh super-context; literals/identifiers carry none.
        _ => false,
    }
}

fn args_have_super_call(args: &[ArrayElem]) -> bool {
    arr_elems_have_super_call(args)
}

fn arr_elems_have_super_call(elems: &[ArrayElem]) -> bool {
    elems.iter().any(|el| match el {
        ArrayElem::Item(e) | ArrayElem::Spread(e) => expr_has_super_call(e),
        ArrayElem::Hole => false,
    })
}

/// A class field initializer (or computed field name) may not contain `arguments` or a `super(...)`
/// call — both are early errors. The walk is transparent through arrow functions (which inherit the
/// field's `arguments`/super context) but stops at ordinary functions and classes. Returns the error
/// message on the first violation.
pub(crate) fn field_init_error(e: &Expr) -> Option<&'static str> {
    fi_expr(e, true)
}

/// A non-constructor method body may not contain a `super(...)` call (only a derived constructor
/// can). Descends into arrow functions (which inherit the method's super context).
/// A non-constructor method may not contain a `super(...)` call in its parameter list *or* its body.
pub(crate) fn method_super_call_error_full(func: &crate::ast::Function) -> Option<&'static str> {
    for p in &func.params {
        if let Some(d) = &p.default {
            if let Some(msg) = fi_expr(d, false) {
                return Some(msg);
            }
        }
    }
    fi_stmts(&func.body, false)
}

/// `args` also flags `arguments` (a field-initializer rule); methods omit it since they may use it.
fn fi_expr(e: &Expr, args: bool) -> Option<&'static str> {
    match e {
        Expr::Ident(n) if args && n == "arguments" => {
            Some("'arguments' is not allowed in a class field initializer")
        }
        Expr::Call {
            callee, args: a, ..
        } => {
            if matches!(**callee, Expr::Super) {
                return Some("a super call is not allowed here");
            }
            fi_expr(callee, args).or_else(|| fi_arr(a, args))
        }
        Expr::New { callee, args: a } => fi_expr(callee, args).or_else(|| fi_arr(a, args)),
        Expr::Unary { arg, .. } | Expr::Update { arg, .. } | Expr::Await(arg) => fi_expr(arg, args),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            fi_expr(left, args).or_else(|| fi_expr(right, args))
        }
        Expr::Assign { target, value, .. } => {
            fi_expr(target, args).or_else(|| fi_expr(value, args))
        }
        Expr::Cond { test, cons, alt } => fi_expr(test, args)
            .or_else(|| fi_expr(cons, args))
            .or_else(|| fi_expr(alt, args)),
        Expr::Member { obj, .. } | Expr::OptionalChain(obj) => fi_expr(obj, args),
        Expr::Index { obj, index, .. } => fi_expr(obj, args).or_else(|| fi_expr(index, args)),
        Expr::Seq(v) => v.iter().find_map(|e| fi_expr(e, args)),
        Expr::Array(elems) => fi_arr(elems, args),
        Expr::Yield { arg, .. } => arg.as_deref().and_then(|e| fi_expr(e, args)),
        Expr::ImportCall { spec, .. } => fi_expr(spec, args),
        Expr::PrivateIn { obj, .. } => fi_expr(obj, args),
        Expr::TaggedTemplate { tag, subs, .. } => {
            fi_expr(tag, args).or_else(|| subs.iter().find_map(|e| fi_expr(e, args)))
        }
        Expr::Object(props) => props.iter().find_map(|p| match p {
            PropDef::KeyValue { key, value } => fi_key(key, args).or_else(|| fi_expr(value, args)),
            PropDef::Spread(e) => fi_expr(e, args),
            // Methods/getters/setters open their own context (only their computed key inherits).
            PropDef::Method { key, .. }
            | PropDef::Getter { key, .. }
            | PropDef::Setter { key, .. } => fi_key(key, args),
        }),
        // Arrow functions inherit the surrounding context: descend into params and body.
        Expr::Func(f) if f.is_arrow => {
            fi_params(&f.params, args).or_else(|| fi_stmts(&f.body, args))
        }
        // Ordinary functions / classes establish their own context.
        _ => None,
    }
}

fn fi_key(key: &PropKey, args: bool) -> Option<&'static str> {
    match key {
        PropKey::Computed(e) => fi_expr(e, args),
        _ => None,
    }
}

fn fi_arr(elems: &[ArrayElem], args: bool) -> Option<&'static str> {
    elems.iter().find_map(|el| match el {
        ArrayElem::Item(e) | ArrayElem::Spread(e) => fi_expr(e, args),
        ArrayElem::Hole => None,
    })
}

fn fi_params(params: &[Param], args: bool) -> Option<&'static str> {
    params
        .iter()
        .find_map(|p| p.default.as_ref().and_then(|e| fi_expr(e, args)))
}

fn fi_stmts(stmts: &[Stmt], args: bool) -> Option<&'static str> {
    stmts.iter().find_map(|s| fi_stmt(s, args))
}

fn fi_stmt(s: &Stmt, args: bool) -> Option<&'static str> {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => fi_expr(e, args),
        Stmt::Return(e) => e.as_ref().and_then(|e| fi_expr(e, args)),
        Stmt::VarDecl { decls, .. } => decls
            .iter()
            .find_map(|(_, i)| i.as_ref().and_then(|e| fi_expr(e, args))),
        Stmt::If { test, cons, alt } => fi_expr(test, args)
            .or_else(|| fi_stmt(cons, args))
            .or_else(|| alt.as_deref().and_then(|s| fi_stmt(s, args))),
        Stmt::Block(b) => fi_stmts(b, args),
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            fi_expr(test, args).or_else(|| fi_stmt(body, args))
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => init
            .as_deref()
            .and_then(|i| match i {
                ForInit::Expr(e) => fi_expr(e, args),
                ForInit::VarDecl { decls, .. } => decls
                    .iter()
                    .find_map(|(_, x)| x.as_ref().and_then(|e| fi_expr(e, args))),
            })
            .or_else(|| test.as_ref().and_then(|e| fi_expr(e, args)))
            .or_else(|| update.as_ref().and_then(|e| fi_expr(e, args)))
            .or_else(|| fi_stmt(body, args)),
        Stmt::ForInOf { right, body, .. } => fi_expr(right, args).or_else(|| fi_stmt(body, args)),
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => fi_stmts(block, args)
            .or_else(|| handler.as_ref().and_then(|(_, b)| fi_stmts(b, args)))
            .or_else(|| finalizer.as_ref().and_then(|b| fi_stmts(b, args))),
        Stmt::Switch { disc, cases } => fi_expr(disc, args).or_else(|| {
            cases.iter().find_map(|c| {
                c.test
                    .as_ref()
                    .and_then(|e| fi_expr(e, args))
                    .or_else(|| fi_stmts(&c.body, args))
            })
        }),
        Stmt::Labeled { body, .. } | Stmt::With { body, .. } => fi_stmt(body, args),
        _ => None,
    }
}

/// A decorator context's `addInitializer(fn)`: records `fn` to run when the element is installed
/// (collected per-decorator in `Interp.decorator_initializers`).
fn dec_add_initializer(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    if !f.is_callable() {
        return Err(i.make_error("TypeError", "addInitializer expects a callable"));
    }
    i.decorator_initializers.push(f);
    Ok(Value::Undefined)
}

fn promise_resolve_native(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    i.resolve_promise(&this, args.first().cloned().unwrap_or(Value::Undefined));
    Ok(Value::Undefined)
}

fn promise_reject_native(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    i.reject_promise(&this, args.first().cloned().unwrap_or(Value::Undefined));
    Ok(Value::Undefined)
}

fn describe_callee(callee: &Expr) -> String {
    match callee {
        Expr::Ident(n) => n.clone(),
        Expr::Member { prop, .. } => format!("(intermediate value).{prop}"),
        _ => "expression".to_string(),
    }
}

/// Whether an expression is an anonymous function/arrow/class (eligible for NamedEvaluation).
pub(crate) fn is_anonymous_fn(e: &Expr) -> bool {
    match e {
        Expr::Func(f) => f.name.is_none(),
        Expr::Class(c) => c.name.is_none(),
        _ => false,
    }
}

fn js_mod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_nan() || b.is_nan() || a.is_infinite() {
        return f64::NAN;
    }
    if b.is_infinite() {
        return a;
    }
    if a == 0.0 {
        return a;
    }
    a % b
}

fn to_int32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let n = n.trunc();
    let m = n.rem_euclid(4294967296.0);
    if m >= 2147483648.0 {
        (m - 4294967296.0) as i32
    } else {
        m as i32
    }
}

/// Parse a string to a Number per the (simplified) StringToNumber grammar: trimmed, empty → 0,
/// supports decimals, `Infinity`, and `0x`/`0o`/`0b` radix prefixes.
fn parse_number(s: &str) -> f64 {
    // StrWhiteSpace includes U+FEFF (which Rust's char::is_whitespace omits).
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16)
            .map(|n| sign * n as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(oct) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8)
            .map(|n| sign * n as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        return i64::from_str_radix(bin, 2)
            .map(|n| sign * n as f64)
            .unwrap_or(f64::NAN);
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}
