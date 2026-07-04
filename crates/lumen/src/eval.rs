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
                    // A throw from `next` marks the record done (IteratorClose skipped); a throw from
                    // binding a sub-pattern leaves it not-done, so the iterator is still closed.
                    macro_rules! step {
                        () => {
                            match me.iterator_step(&iter, &next) {
                                Ok(v) => v,
                                Err(e) => {
                                    done = true;
                                    return Err(e);
                                }
                            }
                        };
                    }
                    for el in elems {
                        match el {
                            ArrayPatElem::Hole => {
                                if !done && step!().is_none() {
                                    done = true;
                                }
                            }
                            ArrayPatElem::Elem { pattern, default } => {
                                let mut v = if done {
                                    Value::Undefined
                                } else {
                                    match step!() {
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
                                    match step!() {
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
                // IteratorClose: propagate its abrupt on normal completion, swallow on abrupt.
                match result {
                    Ok(()) => {
                        if !done {
                            self.iterator_close_normal(&iter)?;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        if !done {
                            if matches!(e, Abrupt::Throw(_)) {
                                self.iterator_close(&iter);
                            } else {
                                // A non-throw completion (return/break/continue): a throwing or
                                // non-object `return` replaces it; otherwise it propagates.
                                self.iterator_close_normal(&iter)?;
                            }
                        }
                        Err(e)
                    }
                }
            }
            Pattern::Object(objpat) => {
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(self.throw("TypeError", "cannot destructure null or undefined"));
                }
                let mut used: Vec<String> = Vec::new();
                for prop in &objpat.props {
                    let key = self.eval_prop_key(&prop.key, env)?;
                    used.push(key.clone());
                    // KeyedBindingInitialization order for a var-mode identifier target:
                    // ResolveBinding *before* GetV (observable through a `with` env's has trap).
                    let var_ref = if matches!(mode, BindMode::Var) {
                        if let Pattern::Ident(name) = &prop.value {
                            let e = Expr::Ident(name.clone());
                            Some(self.resolve_reference(&e, env)?)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let mut v = self.get_member(&value, &key)?;
                    if matches!(v, Value::Undefined) {
                        if let Some(d) = &prop.default {
                            v = self.eval(d, env)?;
                            if let (Pattern::Ident(n), true) = (&prop.value, is_anonymous_fn(d)) {
                                self.set_fn_name(&v, n);
                            }
                        }
                    }
                    match var_ref {
                        Some(mut r) => self.put_reference(&mut r, v)?,
                        None => self.bind_pattern(&prop.value, v, env, mode)?,
                    }
                }
                if let Some(rest_name) = &objpat.rest {
                    let obj = self.copy_data_properties(&value, &used)?;
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
                    if !matches!(v, Value::Empty) {
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
        // StatementList completion: the last non-EMPTY statement value (V); an abrupt
        // break/continue escaping the block carries V per UpdateEmpty.
        let mut v = Value::Empty;
        let mut result = Ok(Value::Empty);
        for s in stmts {
            match self.exec_stmt(s, &scope) {
                Ok(sv) => {
                    if !matches!(sv, Value::Empty) {
                        v = sv;
                    }
                    result = Ok(v.clone());
                }
                Err(e) => {
                    result = Err(crate::interpreter::update_abrupt_empty(e, v.clone()));
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
            // An evaluated `await using x = null` still records a pending Await for the
            // block's end (an empty async disposal awaits once).
            if is_async {
                if self.using_stack.is_empty() {
                    self.using_stack.push(Vec::new());
                }
                self.using_stack.last_mut().unwrap().push(Disposable {
                    value: Value::Undefined,
                    method: Value::Undefined,
                    method_is_async: true,
                });
            }
            return Ok(());
        }
        let (method, method_is_async) = self.dispose_method(value, is_async)?;
        if self.using_stack.is_empty() {
            self.using_stack.push(Vec::new());
        }
        self.using_stack.last_mut().unwrap().push(Disposable {
            value: value.clone(),
            method,
            method_is_async,
        });
        Ok(())
    }

    /// GetDisposeMethod: `@@asyncDispose` (falling back to `@@dispose`) for `await using`, else
    /// `@@dispose`. Throws if the resolved method isn't callable.
    fn dispose_method(&mut self, value: &Value, is_async: bool) -> Result<(Value, bool), Abrupt> {
        let mut m = Value::Undefined;
        let mut from_async = false;
        if is_async {
            if let Some(k) = crate::builtins::well_known_key(self, "asyncDispose") {
                m = self.get_member(value, &k)?;
                from_async = m.is_callable();
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
        Ok((m, from_async))
    }

    /// Dispose a frame's resources in reverse order. An error thrown while disposing either becomes
    /// the completion (if it was previously normal) or is folded into a `SuppressedError` chain.
    pub(crate) fn dispose_frame(
        &mut self,
        mut frame: Vec<Disposable>,
        result: Completion,
    ) -> Completion {
        // A null/undefined `await using` resource left an await-only marker (no method): the
        // block's end still performs one Await even with nothing to dispose.
        let mut await_pending = false;
        frame.retain(|d| {
            if d.method_is_async && !d.method.is_callable() {
                await_pending = true;
                return false;
            }
            true
        });
        let mut completion = result;
        while let Some(r) = frame.pop() {
            // Only an `@@asyncDispose` method's result is awaited; a sync `@@dispose` (even the
            // fallback inside `await using`) has any returned promise ignored. Inside a
            // coroutine the await genuinely parks, so job interleaving matches Await.
            let disposed = self
                .call(r.method.clone(), r.value.clone(), &[])
                .and_then(|p| {
                    if r.method_is_async {
                        if crate::coroutine::in_coroutine() {
                            match crate::coroutine::coroutine_await(self, p) {
                                crate::coroutine::Resume::Next(x) => Ok(x),
                                crate::coroutine::Resume::Throw(e) => Err(Abrupt::Throw(e)),
                                crate::coroutine::Resume::Return(rv) => Err(Abrupt::Return(rv)),
                            }
                        } else {
                            self.await_value(p)
                        }
                    } else {
                        Ok(p)
                    }
                });
            match disposed {
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
        if await_pending && crate::coroutine::in_coroutine() {
            // Await(undefined) — one real tick.
            let tick = self.new_promise();
            self.resolve_promise(&tick, Value::Undefined);
            self.coro_await(tick)?;
        }
        completion
    }

    /// Run one `for-of` iteration: bind the element and execute the body. For a `using`/`await using`
    /// binding (`dispose` is `Some`), the element is a per-iteration disposal resource — its dispose
    /// method runs (awaited for `await using`) at the end of the iteration.
    fn for_of_iteration(
        &mut self,
        left: &Pattern,
        v: Value,
        mode: BindMode,
        body: &Stmt,
        iter_env: &Env,
        dispose: Option<bool>,
    ) -> Completion {
        match dispose {
            None => {
                self.bind_pattern(left, v, iter_env, mode)?;
                self.exec_stmt(body, iter_env)
            }
            Some(is_async) => {
                self.using_stack.push(Vec::new());
                let step = (|me: &mut Self| -> Completion {
                    me.add_disposable(&v, is_async)?;
                    me.bind_pattern(left, v.clone(), iter_env, mode)?;
                    me.exec_stmt(body, iter_env)
                })(self);
                let frame = self.using_stack.pop().unwrap_or_default();
                self.dispose_frame_maybe_async(frame, step, is_async)
            }
        }
    }

    /// Dispose a frame, awaiting each `@@asyncDispose` result when `is_async` (an `await using`
    /// boundary). For a sync boundary this is exactly `dispose_frame`.
    pub(crate) fn dispose_frame_maybe_async(
        &mut self,
        frame: Vec<Disposable>,
        result: Completion,
        _is_async: bool,
    ) -> Completion {
        self.dispose_frame(frame, result)
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
                            let mut b = scope.borrow_mut();
                            b.lexical_names.push(name.clone());
                            b.vars.insert(
                                name,
                                Binding {
                                    value: Value::Undefined,
                                    mutable: true,
                                    strict_immutable: false,
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
                                strict_immutable: false,
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
                        scope.borrow_mut().lexical_names.push(name.clone());
                        scope.borrow_mut().vars.insert(
                            name.clone(),
                            Binding {
                                value: Value::Undefined,
                                mutable: true,
                                strict_immutable: false,
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
            Stmt::Empty | Stmt::Debugger => Ok(Value::Empty),
            Stmt::FuncDecl(func) => {
                // Annex B.3.3 web-compat: evaluating a sloppy block/if-position function
                // declaration copies the block binding's function into the function-scope var
                // binding created at instantiation (see `hoist_block_funcs`).
                if let Some(name) = &func.name {
                    if self
                        .annexb_fn_sync
                        .contains_key(&(Rc::as_ptr(func) as usize))
                    {
                        self.annexb_fn_sync_eval(name, func, env);
                    }
                }
                Ok(Value::Empty)
            }
            Stmt::Expr(e) => self.eval(e, env),
            Stmt::Block(body) => self.exec_block(body, env),
            Stmt::VarDecl { kind, decls } => {
                for (pat, init) in decls {
                    match kind {
                        DeclKind::Var => {
                            // `var x;` (no init) keeps the hoisted binding untouched. For an
                            // identifier target, the binding Reference resolves BEFORE the
                            // initializer runs (a `with` base captured then is written even if
                            // the initializer deletes the property).
                            if let Some(e) = init {
                                if let Pattern::Ident(n) = pat {
                                    if matches!(e, Expr::Class(c) if c.name.is_none()) {
                                        self.pending_fn_name = Some(n.clone());
                                    }
                                    let mut lref =
                                        self.resolve_reference(&Expr::Ident(n.clone()), env)?;
                                    let value = self.eval(e, env)?;
                                    self.pending_fn_name = None;
                                    if is_anonymous_fn(e) {
                                        self.set_fn_name(&value, n);
                                    }
                                    self.put_reference(&mut lref, value)?;
                                } else {
                                    let value = self.eval(e, env)?;
                                    self.bind_pattern(pat, value, env, BindMode::Var)?;
                                }
                            }
                        }
                        DeclKind::Let | DeclKind::Const => {
                            if let (Pattern::Ident(n), Some(e)) = (pat, init) {
                                if matches!(e, Expr::Class(c) if c.name.is_none()) {
                                    self.pending_fn_name = Some(n.clone());
                                }
                            }
                            let value = match init {
                                Some(e) => self.eval(e, env)?,
                                None => Value::Undefined,
                            };
                            self.pending_fn_name = None;
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
                            // A `using` binding is immutable: assignment is a TypeError.
                            self.bind_pattern(pat, value, env, BindMode::Lexical(true))?;
                        }
                    }
                }
                Ok(Value::Empty)
            }
            Stmt::Return(arg) => {
                // A strict ordinary function's `return <expr>` walks the expression's tail
                // positions (conditional branches, logical right sides, the last of a comma
                // sequence): a call there is a proper tail call, handed to the trampoline in
                // Interp::call so the current frame unwinds first.
                if let Some(e) = arg {
                    if self.tco_ok && !self.using_stack.iter().any(|f| !f.is_empty()) {
                        return match self.eval_return_expr(e, env)? {
                            TailEval::Tail(f, t, a) => {
                                self.pending_tail = Some((f, t, a));
                                Err(Abrupt::Return(Value::Undefined))
                            }
                            TailEval::Val(v) => Err(Abrupt::Return(v)),
                        };
                    }
                }
                let v = match arg {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                // In an async generator's own body, `return <expr>` awaits the expression (a
                // bare `return`, an implicit completion, or a nested plain function does not).
                let v = if arg.is_some()
                    && self.in_async_gen_body
                    && crate::coroutine::in_async_gen()
                {
                    self.coro_await(v)?
                } else {
                    v
                };
                Err(Abrupt::Return(v))
            }
            Stmt::Throw(e) => {
                let v = self.eval(e, env)?;
                Err(Abrupt::Throw(v))
            }
            Stmt::If { test, cons, alt } => {
                // IfStatement completion: UpdateEmpty(branch, undefined) — never EMPTY, and an
                // abrupt break/continue leaving the branch has its empty value filled too.
                let t = self.eval(test, env)?;
                let r = if self.to_boolean(&t) {
                    self.exec_stmt(cons, env)
                } else if let Some(a) = alt {
                    self.exec_stmt(a, env)
                } else {
                    Ok(Value::Empty)
                };
                match r {
                    Ok(v) => Ok(crate::interpreter::update_empty(v)),
                    Err(e) => Err(crate::interpreter::update_abrupt_empty(e, Value::Undefined)),
                }
            }
            Stmt::While { test, body } => self.run_loop(None, env, |me, env| {
                let t = me.eval(test, env)?;
                if !me.to_boolean(&t) {
                    return Ok(LoopStep::Done(Value::Empty));
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
                            return Ok(LoopStep::Done(Value::Empty));
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
            Stmt::Break(label) => Err(Abrupt::Break(label.clone(), Value::Empty)),
            Stmt::Continue(label) => Err(Abrupt::Continue(label.clone(), Value::Empty)),
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
                match self.exec_stmt(body, &with_env) {
                    Ok(v) => Ok(crate::interpreter::update_empty(v)),
                    Err(e) => Err(crate::interpreter::update_abrupt_empty(e, Value::Undefined)),
                }
            }
            Stmt::ClassDecl(class) => {
                let value = self.eval_class(class, env)?;
                if let Some(name) = &class.name {
                    self.init_lexical(name, value, false, env);
                }
                Ok(Value::Empty)
            }
            // Module declarations: imports are resolved at link time (runtime no-op); exports run
            // their inner declaration (the export itself is link-time metadata).
            Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => Ok(Value::Empty),
            Stmt::ExportDecl(inner) => self.exec_stmt(inner, env),
            Stmt::ExportDefault(inner) => match &**inner {
                Stmt::Expr(e) => {
                    if matches!(e, Expr::Class(c) if c.name.is_none()) {
                        self.pending_fn_name = Some("default".to_string());
                    }
                    let v = self.eval(e, env)?;
                    self.pending_fn_name = None;
                    // NamedEvaluation: an anonymous function/class default export is named "default".
                    if is_anonymous_fn(e) {
                        self.set_fn_name(&v, "default");
                    }
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Empty)
                }
                // `export default function(){}` / `class{}` with no name: the value is bound to the
                // synthetic `*default*` local (which the "default" export resolves to).
                Stmt::FuncDecl(f) if f.name.is_none() => {
                    let v = self.make_function(f.clone(), env.clone());
                    self.set_fn_name(&v, "default");
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Empty)
                }
                Stmt::ClassDecl(c) if c.name.is_none() => {
                    self.pending_fn_name = Some("default".to_string());
                    let v = self.eval_class(c, env)?;
                    self.pending_fn_name = None;
                    self.set_fn_name(&v, "default");
                    self.init_lexical("*default*", v, false, env);
                    Ok(Value::Empty)
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
            Err(Abrupt::Break(Some(l), bv)) if l == label => Ok(bv),
            other => other,
        }
    }

    fn run_loop(
        &mut self,
        label: Option<&str>,
        env: &Env,
        mut step: impl FnMut(&mut Interp, &Env) -> Result<LoopStep, Abrupt>,
    ) -> Completion {
        // The loop's completion value: the most recent non-EMPTY body completion (UpdateEmpty);
        // V starts at undefined per ForBodyEvaluation, so a value-less loop completes undefined.
        let mut v = Value::Undefined;
        let keep = |bv: Value, v: &mut Value| {
            if !matches!(bv, Value::Empty) {
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
                Err(Abrupt::Break(None, bv)) => {
                    keep(bv, &mut v);
                    return Ok(v);
                }
                Err(Abrupt::Break(Some(l), bv)) if Some(l.as_str()) == label => {
                    keep(bv, &mut v);
                    return Ok(v);
                }
                Err(Abrupt::Continue(None, bv)) => keep(bv, &mut v),
                Err(Abrupt::Continue(Some(l), bv)) if Some(l.as_str()) == label => keep(bv, &mut v),
                // A break/continue targeting an outer label: thread this loop's V outward.
                Err(e) => return Err(crate::interpreter::update_abrupt_empty(e, v)),
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
        // A `for (using x = r; …)` head is a disposal boundary: its resources are disposed once,
        // when the whole loop completes (normally or abruptly).
        let dispose_async = match init.as_deref() {
            Some(ForInit::VarDecl {
                kind: DeclKind::Using,
                ..
            }) => Some(false),
            Some(ForInit::VarDecl {
                kind: DeclKind::AwaitUsing,
                ..
            }) => Some(true),
            _ => None,
        };
        if dispose_async.is_some() {
            self.using_stack.push(Vec::new());
        }
        let result = self.exec_c_for_body(init, test, update, body, &loop_env, label);
        if let Some(is_async) = dispose_async {
            let frame = self.using_stack.pop().unwrap_or_default();
            return self.dispose_frame_maybe_async(frame, result, is_async);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_c_for_body(
        &mut self,
        init: &Option<Box<ForInit>>,
        test: &Option<Expr>,
        update: &Option<Expr>,
        body: &Stmt,
        loop_env: &Env,
        label: Option<&str>,
    ) -> Completion {
        if let Some(init) = init {
            match init.as_ref() {
                ForInit::Expr(e) => {
                    self.eval(e, loop_env)?;
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
                                        strict_immutable: false,
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
                        DeclKind::Let => BindMode::Lexical(false),
                        // const and using/await-using bindings are immutable.
                        _ => BindMode::Lexical(true),
                    };
                    let is_using = matches!(kind, DeclKind::Using | DeclKind::AwaitUsing);
                    let is_async = matches!(kind, DeclKind::AwaitUsing);
                    for (pat, e) in decls {
                        let v = match e {
                            Some(e) => self.eval(e, loop_env)?,
                            None => Value::Undefined,
                        };
                        // A `using`/`await using` init captures its dispose method now.
                        if is_using {
                            self.add_disposable(&v, is_async)?;
                        }
                        self.bind_pattern(pat, v, loop_env, mode)?;
                    }
                }
            }
        }
        // CreatePerIterationEnvironment: a `let` head re-binds its names each round, copying the
        // previous round's values, so closures made in the test/body/update capture that round's
        // bindings. (`const` heads and expression inits share one environment.)
        let per_iter: Vec<String> = match init.as_deref() {
            Some(ForInit::VarDecl {
                kind: DeclKind::Let,
                decls,
            }) => {
                let mut names = Vec::new();
                for (pat, _) in decls {
                    pattern_idents(pat, &mut names);
                }
                names
            }
            _ => Vec::new(),
        };
        let copy_env = |from: &Env| -> Env {
            let parent = from.borrow().parent.clone();
            let e = new_scope(parent);
            for n in &per_iter {
                let b = from.borrow().vars.get(n).cloned();
                if let Some(b) = b {
                    e.borrow_mut().vars.insert(n.clone(), b);
                }
            }
            e
        };
        let mut cur_env = if per_iter.is_empty() {
            loop_env.clone()
        } else {
            copy_env(loop_env)
        };
        let mut first = true;
        self.run_loop(label, loop_env, |me, _env| {
            if !first {
                if !per_iter.is_empty() {
                    cur_env = copy_env(&cur_env);
                }
                if let Some(u) = update {
                    me.eval(u, &cur_env)?;
                }
            }
            first = false;
            if let Some(t) = test {
                let tv = me.eval(t, &cur_env)?;
                if !me.to_boolean(&tv) {
                    return Ok(LoopStep::Done(Value::Empty));
                }
            }
            let bv = me.exec_stmt(body, &cur_env)?;
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
        // A lexical head's bound names are already in scope — uninitialized (TDZ) — while the
        // RHS evaluates, so `for (const x of x)` is a ReferenceError.
        let rhs = if matches!(
            decl,
            Some(DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing)
        ) {
            let tdz = new_scope(Some(env.clone()));
            let mut names = Vec::new();
            pattern_idents(left, &mut names);
            for n in names {
                tdz.borrow_mut().vars.insert(
                    n,
                    Binding {
                        value: Value::Undefined,
                        mutable: true,
                        strict_immutable: false,
                        initialized: false,
                        import_ref: None,
                        deletable: false,
                    },
                );
            }
            self.eval(right, &tdz)?
        } else {
            self.eval(right, env)?
        };
        // No-decl form assigns to an existing binding; a declaration creates a fresh one per round.
        let mode = match decl {
            Some(DeclKind::Var) | None => BindMode::Var,
            Some(DeclKind::Let) => BindMode::Lexical(false),
            // const and using/await-using bindings are immutable.
            Some(_) => BindMode::Lexical(true),
        };
        // A `for (using x of y)` / `for await (await using x of y)` head disposes each element at
        // the end of its iteration.
        let dispose = match decl {
            Some(DeclKind::Using) => Some(false),
            Some(DeclKind::AwaitUsing) => Some(true),
            _ => None,
        };
        if of && is_await {
            // `for await (x of asyncIterable)`: drive the @@asyncIterator (or a sync iterator
            // wrapped async-from-sync), awaiting each `next()` result — and, for a sync source,
            // each value, closing the sync iterator when that await rejects.
            let akey = crate::builtins::async_iterator_key(self);
            let method = match &akey {
                Some(k) => self.get_member(&rhs, k)?,
                None => Value::Undefined,
            };
            // GetMethod: a non-callable, non-nullish @@asyncIterator is a TypeError.
            if !matches!(method, Value::Undefined | Value::Null) && !method.is_callable() {
                return Err(self.throw("TypeError", "@@asyncIterator is not callable"));
            }
            let (iter, from_sync) = if method.is_callable() {
                let it = self.call(method, rhs.clone(), &[])?;
                if !matches!(it, Value::Obj(_)) {
                    return Err(self.throw("TypeError", "@@asyncIterator did not return an object"));
                }
                (it, false)
            } else {
                (self.get_iterator(&rhs)?.0, true)
            };
            let next = self.get_member(&iter, "next")?;
            let iter_close = iter.clone();
            // A failure in the iteration step itself marks the iterator done — only abrupt
            // completions from the loop body (or an early break/return) close it afterwards.
            let (mut exhausted, mut step_failed) = (false, false);
            let result = self.run_loop(label, env, |me, env| {
                step_failed = true;
                let res = me.call(next.clone(), iter.clone(), &[])?;
                // Await the step result by parking the coroutine (real microtask ticks).
                let res = if from_sync { res } else { me.coro_await(res)? };
                if !matches!(res, Value::Obj(_)) {
                    return Err(me.throw("TypeError", "iterator result is not an object"));
                }
                let done = me.get_member(&res, "done")?;
                let done = me.to_boolean(&done);
                let v = if from_sync {
                    let raw = me.get_member(&res, "value")?;
                    // AsyncFromSyncIteratorContinuation: PromiseResolve(%Promise%, value) reads
                    // the value's constructor observably; an abrupt read rejects the wrapper
                    // promise, so the throw lands a tick later at the await. A rejection closes
                    // the sync iterator (closeOnRejection) before rethrowing.
                    let px = match me.promise_resolve_checked(raw) {
                        // The wrapper promise settles via its own reaction job (one tick), and
                        // the outer Await adds another — two ticks per unwrapped value, like the
                        // spec's PerformPromiseThen into the AsyncFromSyncIterator capability.
                        Ok(p) => me.promise_then(&p, Value::Undefined, Value::Undefined),
                        // IfAbruptRejectPromise: the wrapper rejects directly (no extra hop).
                        Err(e) => {
                            let p = me.new_promise();
                            me.reject_promise(&p, e);
                            p
                        }
                    };
                    match me.coro_await(px) {
                        Ok(v) => v,
                        Err(e) => {
                            if !done {
                                me.iterator_close(&iter);
                            }
                            return Err(e);
                        }
                    }
                } else if done {
                    Value::Undefined
                } else {
                    me.get_member(&res, "value")?
                };
                if done {
                    exhausted = true;
                    return Ok(LoopStep::Done(Value::Empty));
                }
                step_failed = false;
                let iter_env = new_scope(Some(env.clone()));
                let bv = me.for_of_iteration(left, v, mode, body, &iter_env, dispose)?;
                Ok(LoopStep::Continue(bv))
            });
            if !(exhausted || step_failed && result.is_err()) {
                // AsyncIteratorClose: a throw completion keeps its error (close failures are
                // swallowed); close failures surface for a normal or return completion.
                let close = self.async_for_await_close(&iter_close, from_sync);
                if !matches!(result, Err(Abrupt::Throw(_))) {
                    close?;
                }
            }
            return result;
        }
        if of {
            // Step the iterator lazily; close it if the loop exits early (break/return/throw).
            let (iter, next) = self.get_iterator(&rhs)?;
            let iter_close = iter.clone();
            let mut exhausted = false;
            // A failure in the iteration step itself (next throwing, a non-object result, or a
            // `value` getter throwing) marks the iterator done: it is NOT closed.
            let mut step_failed = false;
            let result = self.run_loop(label, env, |me, env| {
                step_failed = true;
                let v = match me.iterator_step(&iter, &next)? {
                    Some(x) => x,
                    None => {
                        exhausted = true;
                        return Ok(LoopStep::Done(Value::Empty));
                    }
                };
                step_failed = false;
                let iter_env = new_scope(Some(env.clone()));
                let bv = me.for_of_iteration(left, v, mode, body, &iter_env, dispose)?;
                Ok(LoopStep::Continue(bv))
            });
            if !(exhausted || step_failed && result.is_err()) {
                match &result {
                    // A throw completion swallows close errors; every other completion (normal,
                    // break, return) propagates them and requires an Object result.
                    Err(Abrupt::Throw(_)) => self.iterator_close(&iter_close),
                    _ => self.iterator_close_normal(&iter_close)?,
                }
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
            // A property deleted while the enumeration is under way is not visited.
            let v = loop {
                if idx >= items.len() {
                    return Ok(LoopStep::Done(Value::Empty));
                }
                let v = items[idx].clone();
                idx += 1;
                if let Value::Str(k) = &v {
                    if !me.js_has_property(&rhs, k)? {
                        continue;
                    }
                }
                break v;
            };
            let iter_env = new_scope(Some(env.clone()));
            me.bind_pattern(left, v, &iter_env, mode)?;
            let bv = me.exec_stmt(body, &iter_env)?;
            Ok(LoopStep::Continue(bv))
        })
    }

    /// `yield value`: park the coroutine, then resume per the driver's signal.
    fn yield_one(&mut self, value: Value) -> Completion {
        // AsyncGeneratorYield: an async generator awaits its operand before suspending, so a
        // rejected/thrown awaited value makes the `yield` itself complete abruptly.
        let value = if crate::coroutine::in_async_gen() {
            match crate::coroutine::coroutine_await(self, value) {
                crate::coroutine::Resume::Next(v) => v,
                crate::coroutine::Resume::Throw(e) => return Err(Abrupt::Throw(e)),
                crate::coroutine::Resume::Return(v) => return Err(Abrupt::Return(v)),
            }
        } else {
            value
        };
        match crate::coroutine::coroutine_yield(self, value) {
            crate::coroutine::Resume::Next(v) => Ok(v),
            crate::coroutine::Resume::Return(v) => {
                // AsyncGeneratorUnwrapYieldResumption: a return() value is awaited before the
                // yield completes; its rejection becomes a (catchable) throw at the yield.
                if crate::coroutine::in_async_gen() {
                    match crate::coroutine::coroutine_await(self, v) {
                        crate::coroutine::Resume::Next(x) | crate::coroutine::Resume::Return(x) => {
                            Err(Abrupt::Return(x))
                        }
                        crate::coroutine::Resume::Throw(e) => Err(Abrupt::Throw(e)),
                    }
                } else {
                    Err(Abrupt::Return(v))
                }
            }
            crate::coroutine::Resume::Throw(e) => Err(Abrupt::Throw(e)),
        }
    }

    /// `yield* iterable`: delegate to the inner iterator, forwarding next/return/throw (14.4.14).
    fn yield_delegate(&mut self, value: &Value) -> Completion {
        use crate::coroutine::Resume;
        let (iterator, next) = self.get_iterator(value)?;
        let mut received = Resume::Next(Value::Undefined);
        loop {
            let (result, returning) = match received {
                Resume::Next(v) => (self.call(next.clone(), iterator.clone(), &[v])?, false),
                Resume::Throw(e) => {
                    // GetMethod(iterator, "throw"): a non-callable non-nullish value throws; an
                    // absent method is a protocol violation — close the inner iterator (close
                    // errors propagate) and then throw TypeError.
                    let throw = self.get_member(&iterator, "throw")?;
                    if matches!(throw, Value::Undefined | Value::Null) {
                        self.iterator_close_normal(&iterator)?;
                        return Err(
                            self.throw("TypeError", "the delegated iterator has no 'throw' method")
                        );
                    }
                    if !throw.is_callable() {
                        return Err(self.throw("TypeError", "iterator 'throw' is not callable"));
                    }
                    (self.call(throw, iterator.clone(), &[e])?, false)
                }
                Resume::Return(v) => {
                    let ret = self.get_member(&iterator, "return")?;
                    if matches!(ret, Value::Undefined | Value::Null) {
                        return Err(Abrupt::Return(v));
                    }
                    if !ret.is_callable() {
                        return Err(self.throw("TypeError", "iterator 'return' is not callable"));
                    }
                    (self.call(ret, iterator.clone(), &[v])?, true)
                }
            };
            if !matches!(result, Value::Obj(_)) {
                return Err(self.throw("TypeError", "iterator result is not an object"));
            }
            let done = self.get_member(&result, "done")?;
            if self.to_boolean(&done) {
                let v = self.get_member(&result, "value")?;
                return if returning {
                    Err(Abrupt::Return(v))
                } else {
                    Ok(v)
                };
            }
            // GeneratorYield forwards the inner result object as-is — its `value` is not read
            // here, and the driver must not re-wrap it.
            self.yield_raw_result = true;
            received = crate::coroutine::coroutine_yield(self, result);
        }
    }

    /// AsyncIteratorClose for the inlined `for await` loop: fetch `return`, call it with no
    /// argument, and (per the async-from-sync wrapper) validate/unwrap its result.
    fn async_for_await_close(&mut self, iter: &Value, from_sync: bool) -> Result<(), Abrupt> {
        let ret = self.get_member(iter, "return")?;
        if matches!(ret, Value::Undefined | Value::Null) {
            return Ok(());
        }
        if !ret.is_callable() {
            return Err(self.throw("TypeError", "iterator 'return' is not callable"));
        }
        let r = self.call(ret, iter.clone(), &[])?;
        let r = if from_sync { r } else { self.await_value(r)? };
        if !matches!(r, Value::Obj(_)) {
            return Err(self.throw("TypeError", "iterator result is not an object"));
        }
        if from_sync {
            let raw = self.get_member(&r, "value")?;
            self.await_value(raw)?;
        }
        Ok(())
    }

    /// Await `v`: park the coroutine until it settles, surfacing the resume signal. Outside a
    /// coroutine (top-level await in module code) this is the synchronous event-loop drive.
    fn coro_await(&mut self, v: Value) -> Completion {
        if !crate::coroutine::in_coroutine() {
            return self.await_value(v);
        }
        match crate::coroutine::coroutine_await(self, v) {
            crate::coroutine::Resume::Next(x) => Ok(x),
            crate::coroutine::Resume::Throw(e) => Err(Abrupt::Throw(e)),
            crate::coroutine::Resume::Return(rv) => Err(Abrupt::Return(rv)),
        }
    }

    /// AsyncGeneratorYield for `yield*` delegation: await `value`, suspend producing it, and return
    /// the driver's resume signal to continue the delegation loop.
    fn async_gen_yield_resume(&mut self, value: Value) -> Result<crate::coroutine::Resume, Abrupt> {
        use crate::coroutine::Resume;
        // The delegated value passes through unawaited: `yield*` never unwraps a promise
        // produced by the inner iterator.
        Ok(match crate::coroutine::coroutine_yield(self, value) {
            // AsyncGeneratorUnwrapYieldResumption: await a return() value before the delegation
            // loop sees it; a rejection turns into a throw resumption.
            Resume::Return(v) => match crate::coroutine::coroutine_await(self, v) {
                Resume::Next(x) | Resume::Return(x) => Resume::Return(x),
                Resume::Throw(e) => Resume::Throw(e),
            },
            other => other,
        })
    }

    /// `yield* iterable` inside an *async* generator (14.4.14, async path): drive the inner async
    /// iterator (or a sync one wrapped async-from-sync), awaiting each step result and value, and
    /// re-yielding through AsyncGeneratorYield.
    fn yield_delegate_async(&mut self, value: &Value) -> Completion {
        use crate::coroutine::Resume;
        // GetIterator(value, async): prefer @@asyncIterator; else a sync iterator whose values are
        // awaited (CreateAsyncFromSyncIterator).
        let akey = crate::builtins::async_iterator_key(self);
        let amethod = match &akey {
            Some(k) => self.get_member(value, k)?,
            None => Value::Undefined,
        };
        // GetMethod: a non-callable, non-nullish @@asyncIterator is a TypeError (no sync fallback).
        if !matches!(amethod, Value::Undefined | Value::Null) && !amethod.is_callable() {
            return Err(self.throw("TypeError", "@@asyncIterator is not callable"));
        }
        let (iterator, next, from_sync) = if amethod.is_callable() {
            let it = self.call(amethod, value.clone(), &[])?;
            if !matches!(it, Value::Obj(_)) {
                return Err(self.throw("TypeError", "@@asyncIterator did not return an object"));
            }
            let n = self.get_member(&it, "next")?;
            (it, n, false)
        } else {
            let (it, n) = self.get_iterator(value)?;
            (it, n, true)
        };
        // Read a settled iterator result: await the call result; validate it is an object; for a
        // sync source also await the `value` field. Returns (done, value).
        macro_rules! settle {
            ($result:expr) => {{
                let r = self.coro_await($result)?;
                if !matches!(r, Value::Obj(_)) {
                    return Err(self.throw("TypeError", "iterator result is not an object"));
                }
                let done = self.get_member(&r, "done")?;
                let done = self.to_boolean(&done);
                let mut v = self.get_member(&r, "value")?;
                if from_sync {
                    // AsyncFromSyncIteratorContinuation with closeOnRejection: an abrupt unwrap of
                    // a live (not-done) step closes the sync iterator before rethrowing.
                    v = match self.coro_await(v) {
                        Ok(x) => x,
                        Err(e) => {
                            if !done {
                                self.iterator_close(&iterator);
                            }
                            return Err(e);
                        }
                    };
                }
                (done, v)
            }};
        }
        let mut received = Resume::Next(Value::Undefined);
        loop {
            match received {
                Resume::Next(v) => {
                    let result = self.call(next.clone(), iterator.clone(), &[v])?;
                    let (done, inner) = settle!(result);
                    if done {
                        return Ok(inner);
                    }
                    received = self.async_gen_yield_resume(inner)?;
                }
                Resume::Throw(e) => {
                    let throw = self.get_member(&iterator, "throw")?;
                    if !throw.is_callable() {
                        // No `throw` method: close the iterator (awaiting the close) then TypeError.
                        let ret = self.get_member(&iterator, "return")?;
                        if ret.is_callable() {
                            if let Ok(r) = self.call(ret, iterator.clone(), &[]) {
                                let _ = self.coro_await(r);
                            }
                        }
                        return Err(
                            self.throw("TypeError", "the delegated iterator has no 'throw' method")
                        );
                    }
                    let result = self.call(throw, iterator.clone(), &[e])?;
                    let (done, inner) = settle!(result);
                    if done {
                        return Ok(inner);
                    }
                    received = self.async_gen_yield_resume(inner)?;
                }
                Resume::Return(v) => {
                    let ret = self.get_member(&iterator, "return")?;
                    if !ret.is_callable() {
                        return Err(Abrupt::Return(self.coro_await(v)?));
                    }
                    let result = self.call(ret, iterator.clone(), &[v])?;
                    let (done, inner) = settle!(result);
                    if done {
                        return Err(Abrupt::Return(inner));
                    }
                    received = self.async_gen_yield_resume(inner)?;
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
        // GetIterator only *reads* `next`; it is validated as callable when actually called
        // (IteratorNext), so a missing/non-callable `next` doesn't fail at open time.
        let next = self.get_member(&iter, "next")?;
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
            // TypedArray elements enumerate first (they live outside the property map).
            if let Some(info) = self.typed_arrays.get(&(Rc::as_ptr(&o) as usize)).copied() {
                for idx in 0..self.ta_len(&info).unwrap_or(0) {
                    let k = idx.to_string();
                    if seen.insert(k.clone()) {
                        out.push(k);
                    }
                }
            }
            let (level, parent) = {
                let b = o.borrow();
                let level: Vec<(String, bool)> = b
                    .props
                    .ordered_keys()
                    .into_iter()
                    .filter(|k| !Interp::is_sym_key(k) && !Interp::is_private_key(k))
                    .map(|k| {
                        let e = b.props.get(&k).map(|p| p.enumerable).unwrap_or(false);
                        (k.to_string(), e)
                    })
                    .collect();
                (level, b.proto.clone())
            };
            for (k, enumerable) in level {
                // A non-enumerable own property still *shadows* an enumerable prototype one.
                if seen.insert(k.clone()) && enumerable {
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
        // HasCallInTailPosition: the try block is never a tail position (its calls must run
        // inside the protection of the catch/finally); a catch block is one only when there is
        // no finalizer.
        let saved_tco = self.tco_ok;
        self.tco_ok = false;
        let result = self.exec_block(block, env);
        self.tco_ok = saved_tco;
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
                    let mut v = Value::Empty;
                    let mut last = Ok(Value::Empty);
                    self.declare_block_lexicals(body, &body_env, true);
                    let catch_tco = self.tco_ok && finalizer.is_none();
                    let saved_tco = std::mem::replace(&mut self.tco_ok, catch_tco);
                    for s in body {
                        match self.exec_stmt(s, &body_env) {
                            Ok(sv) => {
                                if !matches!(sv, Value::Empty) {
                                    v = sv;
                                }
                                last = Ok(v.clone());
                            }
                            Err(e) => {
                                last = Err(crate::interpreter::update_abrupt_empty(e, v.clone()));
                                break;
                            }
                        }
                    }
                    self.tco_ok = saved_tco;
                    last
                } else {
                    Err(Abrupt::Throw(ex))
                }
            }
            other => other,
        };
        if let Some(fin) = finalizer {
            // An abrupt completion in `finally` overrides the try/catch completion; its normal
            // value is discarded (the try/catch completion stands). A pending tail call from the
            // try/catch is parked during the finalizer (which may schedule its own) and dropped
            // if the finalizer overrides the completion.
            let parked = self.pending_tail.take();
            // A finalizer IS a tail-position context (its `return f()` overrides everything
            // after it), so tco_ok is inherited here.
            match self.exec_block(fin, env) {
                Ok(_) => {
                    if parked.is_some() {
                        self.pending_tail = parked;
                    }
                }
                Err(e) => {
                    // The finalizer overrides the try/catch completion: the parked tail call
                    // is dropped, but one the finalizer itself scheduled stays pending. The
                    // TryStatement's UpdateEmpty(F, undefined) still applies.
                    return Err(crate::interpreter::update_abrupt_empty(e, Value::Undefined));
                }
            }
        }
        // TryStatement completion: UpdateEmpty(result, undefined) — an EMPTY value (normal or in
        // an escaping break/continue) becomes undefined.
        match after_catch {
            Ok(v) => Ok(crate::interpreter::update_empty(v)),
            Err(e) => Err(crate::interpreter::update_abrupt_empty(e, Value::Undefined)),
        }
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
        let mut v = Value::Empty;
        for case in &cases[start..] {
            for s in &case.body {
                match self.exec_stmt(s, &scope) {
                    Ok(sv) => {
                        if !matches!(sv, Value::Empty) {
                            v = sv;
                        }
                    }
                    Err(Abrupt::Break(None, bv)) => {
                        if !matches!(bv, Value::Empty) {
                            v = bv;
                        }
                        return Ok(crate::interpreter::update_empty(v));
                    }
                    // Abrupt exits thread the accumulated V per UpdateEmpty.
                    Err(e) => return Err(crate::interpreter::update_abrupt_empty(e, v)),
                }
            }
        }
        // SwitchStatement completion is UpdateEmpty(V, undefined).
        Ok(crate::interpreter::update_empty(v))
    }

    // ----- variable binding -------------------------------------------------------------------

    fn init_lexical(&mut self, name: &str, value: Value, is_const: bool, env: &Env) {
        env.borrow_mut().vars.insert(
            name.to_string(),
            Binding {
                value,
                mutable: !is_const,
                strict_immutable: is_const,
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

    /// Object-environment HasBinding for a `with (obj)` scope: HasProperty, then an object-valued
    /// `obj[@@unscopables]` whose `name` property is truthy blocks the binding.
    fn with_has_binding(&mut self, obj: &Value, name: &str) -> Result<bool, Abrupt> {
        if !self.js_has_property(obj, name)? {
            return Ok(false);
        }
        if let Some(k) = crate::builtins::well_known_key(self, "unscopables") {
            let uns = self.get_member(obj, &k)?;
            if matches!(uns, Value::Obj(_)) {
                let blocked = self.get_member(&uns, name)?;
                if self.to_boolean(&blocked) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub fn get_var(&mut self, name: &str, env: &Env) -> Result<Value, Abrupt> {
        self.get_var_with(name, env).map(|(v, _)| v)
    }

    /// Resolve like [`Interp::get_var`], also yielding the `with` object the binding came from —
    /// the implicit call receiver for `f()` resolved through a with scope.
    pub(crate) fn get_var_with(
        &mut self,
        name: &str,
        env: &Env,
    ) -> Result<(Value, Option<Value>), Abrupt> {
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
                        return Ok((self.get_var(&local, &src_env)?, None));
                    }
                    return Ok((binding.value.clone(), None));
                }
                (b.with_obj.clone(), b.parent.clone())
            };
            // `with (obj)`: resolve against the object's properties if it has the name (the proxy
            // `has` trap and @@unscopables participate here).
            if let Some(obj @ Value::Obj(_)) = &with_obj {
                if self.with_has_binding(obj, name)? {
                    // GetBindingValue re-checks HasProperty (the property may have vanished while
                    // @@unscopables ran): strict code throws, sloppy reads undefined.
                    if !self.js_has_property(obj, name)? {
                        if self.strict {
                            return Err(
                                self.throw("ReferenceError", format!("{name} is not defined"))
                            );
                        }
                        return Ok((Value::Undefined, Some(obj.clone())));
                    }
                    return Ok((self.get_member(obj, name)?, Some(obj.clone())));
                }
            }
            cur = parent;
        }
        // Fall back to a property of the global object (where builtins live).
        let g = Value::Obj(self.global.clone());
        if self.has_property(&self.global.clone(), name) {
            return Ok((self.get_member(&g, name)?, None));
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

    /// Resolve a source-level private name (`#x`) to this class evaluation's runtime key through
    /// the scope chain (each class evaluation binds its private names in its class scope — the
    /// spec's PrivateEnvironment). An unresolved name keeps its literal spelling.
    fn resolve_private(&self, name: &str, env: &Env) -> String {
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            let b = s.borrow();
            if let Some(binding) = b.vars.get(name) {
                if let Value::Str(k) = &binding.value {
                    return k.to_string();
                }
            }
            cur = b.parent.clone();
        }
        name.to_string()
    }

    /// Validate a dynamic import's already-evaluated options value, returning the `with.type`
    /// attribute. Non-object options/attributes and non-string attribute values are TypeErrors
    /// (which the caller turns into a rejected promise).
    fn import_attributes(&mut self, o: &Value) -> Result<Option<String>, Abrupt> {
        if matches!(o, Value::Undefined) {
            return Ok(None);
        }
        if !matches!(o, Value::Obj(_)) {
            return Err(self.throw("TypeError", "import options must be an object"));
        }
        let with = self.get_member(o, "with")?;
        if matches!(with, Value::Undefined) {
            return Ok(None);
        }
        if !matches!(with, Value::Obj(_)) {
            return Err(self.throw("TypeError", "import attributes must be an object"));
        }
        // EnumerableOwnProperties(with, key+value): own string keys in order (through a proxy's
        // ownKeys/getOwnPropertyDescriptor traps), re-checking each descriptor before its Get;
        // every attribute value must be a string.
        let mut ty = None;
        if let Some((t, h)) = crate::builtins::proxy_pair(self, &with) {
            let keys = crate::builtins::proxy_own_keys(self, &t, &h).map_err(Abrupt::Throw)?;
            for k in keys {
                let pk = self.to_property_key(&k)?;
                if Interp::is_sym_key(&pk) {
                    continue;
                }
                let desc =
                    crate::builtins::proxy_gopd_value(self, &t, &h, &pk).map_err(Abrupt::Throw)?;
                if matches!(desc, Value::Undefined) {
                    continue;
                }
                let e = self.get_member(&desc, "enumerable")?;
                if !self.to_boolean(&e) {
                    continue;
                }
                let v = self.get_member(&with, &pk)?;
                let Value::Str(sv) = v else {
                    return Err(self.throw("TypeError", "import attribute values must be strings"));
                };
                if pk == "type" {
                    ty = Some(sv.to_string());
                }
            }
            return Ok(ty);
        }
        let keys: Vec<std::rc::Rc<str>> = with
            .as_obj()
            .map(|obj| {
                obj.borrow()
                    .props
                    .ordered_keys()
                    .into_iter()
                    .filter(|k| !Interp::is_sym_key(k))
                    .collect()
            })
            .unwrap_or_default();
        for k in keys {
            let live = with
                .as_obj()
                .and_then(|obj| obj.borrow().props.get(&k).map(|p| p.enumerable));
            if live != Some(true) {
                continue;
            }
            let v = self.get_member(&with, &k)?;
            let Value::Str(sv) = v else {
                return Err(self.throw("TypeError", "import attribute values must be strings"));
            };
            if &*k == "type" {
                ty = Some(sv.to_string());
            }
        }
        Ok(ty)
    }

    /// Annex B.3.3 sync step: copy the block-scope binding of `name` (or a freshly-made function
    /// for a bare `if (x) function f(){}` position) into the nearest variable environment's
    /// binding — or the global object's property for global code.
    fn annexb_fn_sync_eval(&mut self, name: &str, func: &Rc<Function>, env: &Env) {
        // The value: the block's own binding of the name (instantiated at block entry). A bare
        // `if (x) function f(){}` position has no block scope — per B.3.4 it acts as an implicit
        // block, so a fresh function is made here. Only the *immediate* scope counts: an ancestor
        // binding (a catch parameter, an outer var) is not the block binding.
        let own = {
            let b = env.borrow();
            if b.var_boundary {
                None
            } else {
                b.vars.get(name).map(|x| x.value.clone())
            }
        };
        let val = own.unwrap_or_else(|| {
            // B.3.4: the bare if-position declaration acts as `{ function f(){} }` — the function
            // closes over an implicit block scope binding its own name, so `f = ...` inside the
            // body rebinds the block binding, not the promoted var.
            let block = crate::interpreter::new_scope(Some(env.clone()));
            let f = self.make_function(func.clone(), block.clone());
            bind(&block, name, f.clone());
            f
        });
        let mut cur = Some(env.clone());
        while let Some(s) = cur {
            if s.borrow().var_boundary {
                if let Some(b) = s.borrow_mut().vars.get_mut(name) {
                    b.value = val;
                    return;
                }
                break;
            }
            let parent = s.borrow().parent.clone();
            cur = parent;
        }
        // Global code: the var binding lives as a global-object property.
        if let Some(p) = self.global.borrow_mut().props.get_mut(name) {
            if p.writable && !p.accessor {
                p.value = val;
            }
        }
    }

    /// PrivateGet: read a private field/method after a brand check. An object that was not
    /// constructed with this private name in scope (`#x` absent) is a TypeError, not `undefined`.
    fn get_private_member(&mut self, base: &Value, name: &str) -> Completion {
        let prop = match base {
            Value::Obj(o) => o.borrow().props.get(name).cloned(),
            _ => None,
        };
        match prop {
            Some(p) if p.accessor => match &p.get {
                Some(g) => self.call(g.clone(), base.clone(), &[]),
                None => {
                    let shown = private_display(name);
                    Err(self.throw(
                        "TypeError",
                        format!("private accessor {shown} has no getter"),
                    ))
                }
            },
            Some(p) => Ok(p.value),
            None => {
                let shown = private_display(name);
                Err(self.throw(
                    "TypeError",
                    format!("cannot read private member {shown} from an object whose class did not declare it"),
                ))
            }
        }
    }

    /// PrivateSet: write a private field (or invoke a private setter) after a brand check. A
    /// private *method* (non-writable) or a getter-only private accessor is a TypeError — never a
    /// silent sloppy-mode no-op.
    fn set_private_member(&mut self, base: &Value, name: &str, value: Value) -> Result<(), Abrupt> {
        let shown = private_display(name).to_string();
        let (o, prop) = match base {
            Value::Obj(o) => (o.clone(), o.borrow().props.get(name).cloned()),
            _ => {
                return Err(self.throw(
                    "TypeError",
                    format!("cannot write private member {shown} to an object whose class did not declare it"),
                ))
            }
        };
        match prop {
            Some(p) if p.accessor => {
                let Some(setter) = p.set.clone() else {
                    return Err(self.throw(
                        "TypeError",
                        format!("private accessor {shown} has no setter"),
                    ));
                };
                self.call(setter, base.clone(), &[value])?;
                Ok(())
            }
            Some(p) => {
                if !p.writable {
                    return Err(self.throw(
                        "TypeError",
                        format!("private method {shown} is not writable"),
                    ));
                }
                if let Some(p) = o.borrow_mut().props.get_mut(name) {
                    p.value = value;
                }
                Ok(())
            }
            None => Err(self.throw(
                "TypeError",
                format!("cannot write private member {shown} to an object whose class did not declare it"),
            )),
        }
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
                        // A const always throws; a non-strict immutable binding (a named function
                        // expression's own name) is a silent no-op in sloppy code.
                        if binding.strict_immutable || self.strict {
                            return Err(
                                self.throw("TypeError", format!("assignment to constant '{name}'"))
                            );
                        }
                        return Ok(());
                    }
                    binding.value = value;
                    binding.initialized = true;
                    return Ok(());
                }
                (b.with_obj.clone(), b.parent.clone())
            };
            if let Some(obj @ Value::Obj(_)) = &with_obj {
                if self.with_has_binding(obj, name)? {
                    // SetMutableBinding re-checks HasProperty; strict code throws if the
                    // property vanished while @@unscopables ran.
                    if !self.js_has_property(obj, name)? && self.strict {
                        return Err(self.throw("ReferenceError", format!("{name} is not defined")));
                    }
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
        // A deferred namespace evaluates on [[HasProperty]] with a string key.
        self.defer_trigger(&o, Some(key))?;
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
            // The trap receives the original property key — a symbol stays a symbol.
            let key_val = self
                .sym_from_key(key)
                .unwrap_or_else(|| Value::from_string(key.to_string()));
            let res = self.call(trap, handler, &[target.clone(), key_val])?;
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
        // A String wrapper's `length` and in-range indices are own exotic properties.
        if let crate::value::Exotic::StrWrap(s) = o.borrow().exotic.clone() {
            if key == "length" {
                return Ok(true);
            }
            if let Ok(idx) = key.parse::<usize>() {
                if idx < crate::jstr::unit_len(&s) {
                    return Ok(true);
                }
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
            Expr::BigInt(n) => Ok(Value::BigInt(n.clone())),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::ToStr(inner) => {
                let v = self.eval(inner, env)?;
                Ok(Value::Str(self.to_string(&v)?))
            }
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => self.get_var(name, env),
            Expr::This => {
                // A TDZ read (derived constructor before super()) must surface as a
                // ReferenceError; only a genuinely absent binding reads undefined.
                let mut cur = Some(env.clone());
                while let Some(scope) = cur {
                    if scope.borrow().vars.contains_key("this") {
                        return self.get_var("this", env);
                    }
                    let parent = scope.borrow().parent.clone();
                    cur = parent;
                }
                Ok(Value::Undefined)
            }
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
                    if crate::coroutine::in_async_gen() {
                        self.yield_delegate_async(&value)
                    } else {
                        self.yield_delegate(&value)
                    }
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
            Expr::ImportMeta => Ok(self
                .peek_binding("%importmeta%", env)
                .or_else(|| self.import_meta.clone())
                .unwrap_or(Value::Undefined)),
            Expr::NewTarget => Ok(self
                .peek_binding("%newtarget%", env)
                .unwrap_or(Value::Undefined)),
            Expr::ImportCall {
                spec,
                phase,
                options,
            } => {
                let specifier = self.eval(spec, env)?;
                // The options argument evaluates synchronously (abrupt completions propagate,
                // before any promise exists); its validation happens inside the promise.
                let opts_val = match options {
                    Some(o) => Some(self.eval(o, env)?),
                    None => None,
                };
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
                    ImportPhase::Evaluation | ImportPhase::Defer => {
                        // `{ with: { type: ... } }` selects a JSON/text/bytes module. An abrupt
                        // attributes validation rejects the promise like the specifier coercion.
                        let mut attr_type = None;
                        if let Some(o) = &opts_val {
                            match self.import_attributes(o) {
                                Ok(t) => attr_type = t,
                                Err(e) => {
                                    let p = self.new_promise();
                                    let reason = crate::interpreter::abrupt_value(e);
                                    self.reject_promise(&p, reason);
                                    return Ok(p);
                                }
                            }
                        }
                        Ok(self.dynamic_import(
                            &s,
                            attr_type.as_deref(),
                            matches!(phase, ImportPhase::Defer),
                        ))
                    }
                }
            }
            Expr::PrivateIn { name, obj } => {
                let o = self.eval(obj, env)?;
                let k = self.resolve_private(name, env);
                match o {
                    // Private fields, methods and accessors are all own properties.
                    Value::Obj(obj) => Ok(Value::Bool(obj.borrow().props.contains(k.as_str()))),
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
                    // GetThisBinding first (TDZ ReferenceError), then Get(base, key, actualThis):
                    // a getter on the super prototype sees the current `this`.
                    let this = self.get_var("this", env)?;
                    let home = self.super_base(env)?;
                    if matches!(home, Value::Undefined | Value::Null) {
                        return Err(self.throw(
                            "TypeError",
                            format!("cannot read property '{prop}' of a null super base"),
                        ));
                    }
                    return crate::builtins::reflect_ordinary_get(self, &home, prop, &this)
                        .map_err(Abrupt::Throw);
                }
                let base = self.eval(obj, env)?;
                if self.short_circuit {
                    return Ok(Value::Undefined); // an earlier `?.` link short-circuited
                }
                if *optional && matches!(base, Value::Undefined | Value::Null) {
                    self.short_circuit = true;
                    return Ok(Value::Undefined);
                }
                if prop.starts_with('#') {
                    let k = self.resolve_private(prop, env);
                    return self.get_private_member(&base, &k);
                }
                self.get_member(&base, prop)
            }
            Expr::Index {
                obj,
                index,
                optional,
            } => {
                if matches!(**obj, Expr::Super) {
                    // GetThisBinding, the key expression, GetSuperBase, then ToPropertyKey.
                    let this = self.get_var("this", env)?;
                    let idx = self.eval(index, env)?;
                    let home = self.super_base(env)?;
                    let key = self.to_property_key(&idx)?;
                    if matches!(home, Value::Undefined | Value::Null) {
                        return Err(self.throw(
                            "TypeError",
                            format!("cannot read property '{key}' of a null super base"),
                        ));
                    }
                    return crate::builtins::reflect_ordinary_get(self, &home, &key, &this)
                        .map_err(Abrupt::Throw);
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
                // GetValue: ToObject(base) throws before ToPropertyKey coerces the key.
                if matches!(base, Value::Undefined | Value::Null) {
                    return Err(
                        self.throw("TypeError", "cannot read property of null or undefined")
                    );
                }
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
        // Elements are created as own data properties (CreateDataProperty), so accessors on
        // Array.prototype are never consulted.
        let arr = self.make_array(Vec::new());
        let Value::Obj(ao) = &arr else { unreachable!() };
        let mut idx: usize = 0;
        for e in elems {
            match e {
                ArrayElem::Item(e) => {
                    let v = self.eval(e, env)?;
                    ao.borrow_mut()
                        .props
                        .insert(idx.to_string(), crate::value::Property::plain(v));
                    idx += 1;
                }
                ArrayElem::Hole => idx += 1,
                ArrayElem::Spread(e) => {
                    let v = self.eval(e, env)?;
                    for item in self.iterate(&v)? {
                        ao.borrow_mut()
                            .props
                            .insert(idx.to_string(), crate::value::Property::plain(item));
                        idx += 1;
                    }
                }
            }
        }
        if let Some(pr) = ao.borrow_mut().props.get_mut("length") {
            pr.value = Value::Num(idx as f64);
        }
        Ok(arr)
    }

    /// The NamedEvaluation name for a property key: a symbol key names the function "[desc]"
    /// (or "" without a description); a string key is used as-is.
    fn fn_name_for_key(&self, key: &str) -> String {
        if Interp::is_sym_key(key) {
            match self.sym_from_key(key) {
                Some(Value::Sym(d)) => match &d.description {
                    Some(desc) => format!("[{desc}]"),
                    None => String::new(),
                },
                _ => String::new(),
            }
        } else {
            key.to_string()
        }
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
                        let name = self.fn_name_for_key(&k);
                        self.set_fn_name(&v, &name);
                    }
                    obj.borrow_mut().props.insert(k, Property::plain(v));
                }
                PropDef::Method { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), home_env.clone());
                    let name = self.fn_name_for_key(&k);
                    self.set_fn_name(&f, &name);
                    obj.borrow_mut().props.insert(k, Property::plain(f));
                }
                PropDef::Getter { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), home_env.clone());
                    let name = self.fn_name_for_key(&k);
                    self.set_fn_name(&f, &format!("get {name}"));
                    self.define_accessor(&obj, &k, Some(f), None);
                }
                PropDef::Setter { key, func } => {
                    let k = self.eval_prop_key(key, env)?;
                    let f = self.make_function(func.clone(), home_env.clone());
                    let name = self.fn_name_for_key(&k);
                    self.set_fn_name(&f, &format!("set {name}"));
                    self.define_accessor(&obj, &k, None, Some(f));
                }
                PropDef::Spread(e) => {
                    // CopyDataProperties (no excluded names) — proxy traps, symbols and string
                    // indices included.
                    let v = self.eval(e, env)?;
                    self.copy_data_properties_into(&obj, &v, &[])?;
                }
                // `__proto__: value` sets the prototype when value is an Object or Null; any other
                // value type is ignored (no property is created).
                PropDef::Proto(e) => {
                    let v = self.eval(e, env)?;
                    match v {
                        Value::Obj(p) => obj.borrow_mut().proto = Some(p),
                        Value::Null => obj.borrow_mut().proto = None,
                        _ => {}
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
        let strings = self.template_object(quasis)?;
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

    /// GetTemplateObject: one frozen strings array (with a frozen `.raw`) per template *site* —
    /// re-evaluating the same site passes the identical object.
    fn template_object(&mut self, quasis: &[(Option<String>, String)]) -> Result<Value, Abrupt> {
        let site = quasis.as_ptr() as usize;
        let strings = match self.template_cache.get(&site) {
            Some(v) => v.clone(),
            None => {
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
                if let Value::Obj(so) = &strings {
                    // `raw` is a non-enumerable data property (then frozen with the rest).
                    so.borrow_mut().props.insert(
                        "raw",
                        crate::value::Property::data(raw_arr, false, false, false),
                    );
                }
                self.freeze_object(&strings);
                if let Value::Obj(o) = &strings {
                    self.gc_pin(o);
                }
                self.template_cache.insert(site, strings.clone());
                strings
            }
        };
        Ok(strings)
    }

    /// Evaluate a `return` operand, treating the expression's tail positions as proper tail
    /// calls. Anything else evaluates normally to a value.
    fn eval_return_expr(&mut self, e: &Expr, env: &Env) -> Result<TailEval, Abrupt> {
        match e {
            Expr::Call { .. } => {
                if let Some((f, t, a)) = self.eval_tail_call(e, env)? {
                    return Ok(TailEval::Tail(f, t, a));
                }
                Ok(TailEval::Val(self.eval(e, env)?))
            }
            Expr::TaggedTemplate { .. } => {
                if let Some((f, t, a)) = self.eval_tail_tagged(e, env)? {
                    return Ok(TailEval::Tail(f, t, a));
                }
                Ok(TailEval::Val(self.eval(e, env)?))
            }
            Expr::Cond { test, cons, alt } => {
                let tv = self.eval(test, env)?;
                if self.to_boolean(&tv) {
                    self.eval_return_expr(cons, env)
                } else {
                    self.eval_return_expr(alt, env)
                }
            }
            Expr::Seq(v) if !v.is_empty() => {
                for x in &v[..v.len() - 1] {
                    self.eval(x, env)?;
                }
                self.eval_return_expr(&v[v.len() - 1], env)
            }
            Expr::Logical { op, left, right } => {
                let lv = self.eval(left, env)?;
                let take_right = match *op {
                    "&&" => self.to_boolean(&lv),
                    "||" => !self.to_boolean(&lv),
                    "??" => matches!(lv, Value::Undefined | Value::Null),
                    _ => false,
                };
                if take_right {
                    self.eval_return_expr(right, env)
                } else {
                    Ok(TailEval::Val(lv))
                }
            }
            _ => Ok(TailEval::Val(self.eval(e, env)?)),
        }
    }

    /// If `e` is a plain call usable as a proper tail call, evaluate its callee, `this` and
    /// arguments and return them for the trampoline; `None` falls back to a normal evaluation.
    fn eval_tail_call(
        &mut self,
        e: &Expr,
        env: &Env,
    ) -> Result<Option<(Value, Value, Vec<Value>)>, Abrupt> {
        let Expr::Call {
            callee,
            args,
            optional: false,
        } = e
        else {
            return Ok(None);
        };
        let (func, this) = match &**callee {
            // A direct `eval`, `super(...)`, private-name or super-property callee stays on the
            // normal path.
            Expr::Ident(name) => {
                // A callee resolved through a `with (obj)` environment gets `obj` as `this`.
                let (f, recv) = self.get_var_with(name, env)?;
                // A callee *named* eval only disqualifies when it is the real eval function
                // (a direct eval isn't a call at all).
                if name == "eval" && recv.is_none() {
                    if let (Value::Obj(fo), Some(ef)) = (&f, &self.eval_fn) {
                        if Rc::ptr_eq(fo, ef) {
                            return Ok(None);
                        }
                    }
                }
                (f, recv.unwrap_or(Value::Undefined))
            }
            Expr::Member { obj, prop, .. }
                if !matches!(**obj, Expr::Super) && !prop.starts_with('#') =>
            {
                let base = self.eval(obj, env)?;
                if matches!(base, Value::Undefined | Value::Null) {
                    return Ok(None);
                }
                let f = self.get_member(&base, prop)?;
                (f, base)
            }
            // A call-expression callee (`return getF()(n)`) evaluates generically.
            c @ Expr::Call { .. } => {
                let f = self.eval(c, env)?;
                if self.short_circuit {
                    return Ok(None);
                }
                (f, Value::Undefined)
            }
            _ => return Ok(None),
        };
        if !func.is_callable() {
            return Ok(None);
        }
        let argv = self.eval_args(args, env)?;
        Ok(Some((func, this, argv)))
    }

    /// A tagged template in tail position: evaluate the tag and assemble the argument list for
    /// the trampoline.
    fn eval_tail_tagged(
        &mut self,
        e: &Expr,
        env: &Env,
    ) -> Result<Option<(Value, Value, Vec<Value>)>, Abrupt> {
        let Expr::TaggedTemplate { tag, quasis, subs } = e else {
            return Ok(None);
        };
        let (func, this) = match &**tag {
            Expr::Ident(_) | Expr::Call { .. } => (self.eval(tag, env)?, Value::Undefined),
            Expr::Member { obj, prop, .. } if !matches!(**obj, Expr::Super) => {
                let base = self.eval(obj, env)?;
                if matches!(base, Value::Undefined | Value::Null) {
                    return Ok(None);
                }
                let f = self.get_member(&base, prop)?;
                (f, base)
            }
            _ => return Ok(None),
        };
        if !func.is_callable() {
            return Ok(None);
        }
        let strings = self.template_object(quasis)?;
        let mut argv = vec![strings];
        for s in subs {
            argv.push(self.eval(s, env)?);
        }
        Ok(Some((func, this, argv)))
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
            // `eval?.(...)` is NOT a direct eval — it runs indirectly, in the global scope.
            if name == "eval" && !optional {
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
            // anything reached via a direct `eval` from those) is illegal — but an arrow created
            // inside the constructor keeps its lexical super-call capability even when invoked
            // later (BindThisValue then throws ReferenceError instead).
            if !self.super_call_ok
                && (self.peek_binding("%superclass%", env).is_none()
                    || self.peek_binding("%thisctor%", env).is_none())
            {
                return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
            }
            if matches!(self.get_var("%superclass%", env)?, Value::Undefined) {
                return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
            }
            // GetSuperConstructor: the *live* [[GetPrototypeOf]] of the running class
            // constructor (Object.setPrototypeOf(C, ...) between definition and the call is
            // honored). The IsConstructor check waits until the arguments have evaluated.
            let this_ctor_for_parent = self.get_var("%thisctor%", env)?;
            let parent = crate::builtins::js_get_prototype_of(self, &this_ctor_for_parent)
                .map_err(Abrupt::Throw)?;
            // Read the `this` binding directly (it is in TDZ until this very call completes);
            // an already-initialized binding means super() ran twice.
            let this_env = {
                let mut cur = Some(env.clone());
                let mut found = None;
                while let Some(scope) = cur {
                    if scope.borrow().vars.contains_key("this") {
                        found = Some(scope);
                        break;
                    }
                    let parent_scope = scope.borrow().parent.clone();
                    cur = parent_scope;
                }
                found
            };
            let Some(this_env) = this_env else {
                return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
            };
            let this = this_env
                .borrow()
                .vars
                .get("this")
                .map(|b| b.value.clone())
                .unwrap();
            let argv = self.eval_args(args, env)?;
            // A null (extends null) or non-constructor parent is a TypeError — after
            // ArgumentListEvaluation.
            if !self.value_is_constructor(&parent) {
                return Err(self.throw("TypeError", "super constructor is not a constructor"));
            }
            // Construct(parent, args, GetNewTarget()): the parent runs with the derived
            // constructor's active newTarget.
            self.pending_new_target = self.new_target.clone();
            let returned = self.run_constructor_on(&parent, &this, &argv)?;
            // BindThisValue: an already-initialized `this` (a second super()) is a
            // ReferenceError — but only after the arguments and parent construct ran.
            {
                let b = this_env.borrow();
                let bd = b.vars.get("this").unwrap();
                if bd.initialized {
                    return Err(self.throw("ReferenceError", "super() may only be called once"));
                }
            }
            // A base constructor that returns an object overrides `this` for the derived
            // constructor (and everything downstream: field initializers, super.x accesses,
            // the implicit return).
            let this = match (&returned, &this) {
                (Value::Obj(r), Value::Obj(t)) if !Rc::ptr_eq(r, t) => {
                    let mut cur = Some(env.clone());
                    while let Some(scope) = cur {
                        let done = {
                            let mut b = scope.borrow_mut();
                            if let Some(bd) = b.vars.get_mut("this") {
                                bd.value = returned.clone();
                                true
                            } else {
                                false
                            }
                        };
                        if done {
                            break;
                        }
                        let parent_scope = scope.borrow().parent.clone();
                        cur = parent_scope;
                    }
                    returned.clone()
                }
                _ => this,
            };
            if let Some(bd) = this_env.borrow_mut().vars.get_mut("this") {
                bd.initialized = true;
            }
            let this_ctor = self.get_var("%thisctor%", env)?;
            self.init_instance_fields(&this_ctor, &this)?;
            // The value of a `super(...)` expression is the (possibly overridden) `this`.
            return Ok(this);
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
        // A parenthesized optional chain as the callee still resolves like the chain (the
        // method receiver is preserved), but a short-circuited chain yields undefined and
        // calling it throws.
        if let Expr::OptionalChain(inner) = callee {
            let saved = self.short_circuit;
            self.short_circuit = false;
            let r = self.eval_call(inner, args, optional, env);
            let short = std::mem::replace(&mut self.short_circuit, saved);
            if short && r.is_ok() {
                self.eval_args(args, env)?;
                return Err(self.throw("TypeError", "callee is not a function"));
            }
            return r;
        }
        // Determine `this` for method calls (`obj.m()` → this = obj); a callee resolved
        // through a `with (obj)` environment is called with `this` = obj.
        let (func, this) = match callee {
            Expr::Ident(name) => {
                let (f, recv) = self.get_var_with(name, env)?;
                (f, recv.unwrap_or(Value::Undefined))
            }
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
                let f = if prop.starts_with('#') {
                    let k = self.resolve_private(prop, env);
                    self.get_private_member(&base, &k)?
                } else {
                    self.get_member(&base, prop)?
                };
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
            Value::Obj(_) => {
                // Await of a plain object goes through the promise resolution procedure: a
                // thenable is adopted (its `then` called once), anything else settles as-is.
                let then = self.get_member(&v, "then")?;
                if then.is_callable() {
                    let p = self.new_promise();
                    let (res, rej) = self.make_resolver_pair(&p);
                    if let Err(Abrupt::Throw(e)) = self.call(then, v.clone(), &[res, rej]) {
                        self.reject_promise(&p, e);
                    }
                    return self.await_value(p);
                }
                return Ok(v);
            }
            _ => return Ok(v),
        };
        // Await → PromiseResolve(%Promise%, v): the `constructor` read on a native promise is
        // observable, and its abrupt completion is the await's.
        self.get_member(&v, "constructor")?;
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
        self.gc_pin(&obj);
        self.promises.insert(p, PromiseState::default());
        Value::Obj(obj)
    }

    /// One resolving function of a pair; both share `flag` — their [[AlreadyResolved]] record.
    pub(crate) fn make_resolver_with(
        &mut self,
        promise: &Value,
        fulfilling: bool,
        flag: &crate::value::Gc,
    ) -> Value {
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
            args: vec![Value::Obj(flag.clone())],
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

    /// The standard resolver pair (shared [[AlreadyResolved]]).
    pub(crate) fn make_resolver_pair(&mut self, promise: &Value) -> (Value, Value) {
        let flag = Object::new(None);
        (
            self.make_resolver_with(promise, true, &flag),
            self.make_resolver_with(promise, false, &flag),
        )
    }

    pub(crate) fn resolve_promise(&mut self, promise: &Value, value: Value) {
        // Follow a subclass graft's forwarding link (see run_constructor_on's native arm).
        let mut promise = promise.clone();
        if let Value::Obj(o) = &promise {
            if let Some(f) = self.promise_forward.get(&(Rc::as_ptr(o) as usize)) {
                promise = f.clone();
            }
        }
        let promise = &promise;
        let ptr = match promise {
            Value::Obj(o) => Rc::as_ptr(o) as usize,
            _ => return,
        };
        if self.promises.get(&ptr).map(|s| s.status).unwrap_or(1) != 0 {
            return;
        }
        // Resolving a promise with ITSELF is a TypeError rejection (chaining cycle).
        if let (Value::Obj(p), Value::Obj(v)) = (promise, &value) {
            if Rc::ptr_eq(p, v) {
                let e = crate::interpreter::abrupt_value(
                    self.throw("TypeError", "Chaining cycle detected for promise"),
                );
                self.reject_promise(promise, e);
                return;
            }
        }
        // Adopt a thenable's eventual state; a throwing `then` getter rejects. The `then` CALL
        // itself happens in a microtask (PromiseResolveThenableJob).
        if matches!(value, Value::Obj(_)) {
            match self.get_member(&value, "then") {
                Ok(then) if then.is_callable() => {
                    let (res, rej) = self.make_resolver_pair(promise);
                    let runner =
                        crate::builtins::make_thenable_job(self, then, value.clone(), res, rej);
                    self.microtasks.push_back(crate::interpreter::Job {
                        handler: runner,
                        result: Value::Undefined,
                        value: Value::Undefined,
                        fulfilled: true,
                    });
                    return;
                }
                Ok(_) => {}
                Err(Abrupt::Throw(e)) => {
                    self.reject_promise(promise, e);
                    return;
                }
                Err(_) => {}
            }
        }
        self.settle(promise, value, true);
    }

    pub(crate) fn reject_promise(&mut self, promise: &Value, reason: Value) {
        let mut promise = promise.clone();
        if let Value::Obj(o) = &promise {
            if let Some(f) = self.promise_forward.get(&(Rc::as_ptr(o) as usize)) {
                promise = f.clone();
            }
        }
        self.settle(&promise, reason, false);
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
        while !self.pending_async_waits.is_empty() || !self.pending_timers.is_empty() {
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
            // Fire due host timers ($262.agent.setTimeout), earliest first.
            let now = std::time::Instant::now();
            let mut due: Vec<Value> = Vec::new();
            self.pending_timers.retain(|(f, deadline)| {
                if *deadline <= now {
                    due.push(f.clone());
                    false
                } else {
                    true
                }
            });
            for f in due {
                let _ = self.call(f, Value::Undefined, &[]);
                resolved_any = true;
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
        // GetPrototypeFromConstructor: `new` with a subclass/cross-realm newTarget overrides the
        // instance prototype (falling back to newTarget's realm's %RegExp.prototype%).
        let proto = if self.constructing {
            match &self.new_target.clone() {
                nt @ Value::Obj(_) => match self.get_member(nt, "prototype")? {
                    Value::Obj(p) => Some(p),
                    _ => crate::builtins::regexp_realm_proto(self, nt)
                        .or_else(|| self.extra_protos.get("RegExp").cloned()),
                },
                _ => self.extra_protos.get("RegExp").cloned(),
            }
        } else {
            self.extra_protos.get("RegExp").cloned()
        };
        let obj = Object::new(proto);
        let ptr = Rc::as_ptr(&obj) as usize;
        // source/flags/global/... are accessor getters on RegExp.prototype (computed from the
        // matcher); only `lastIndex` is an own writable data property.
        obj.borrow_mut().props.insert(
            "lastIndex",
            Property::data(Value::Num(0.0), true, false, false),
        );
        self.gc_pin(&obj);
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
        // A direct eval's code may reference any private name visible at the call site: collect
        // the `#name` bindings on the caller's scope chain to seed the parser's validation.
        let mut private_names: Vec<String> = Vec::new();
        if direct {
            let mut cur = Some(caller_env.clone());
            while let Some(s) = cur {
                let b = s.borrow();
                for k in b.vars.keys() {
                    if k.starts_with('#') {
                        private_names.push(k.clone());
                    }
                }
                cur = b.parent.clone();
            }
        }
        // A direct eval may contain a SuperProperty only when its caller is method code (a home
        // object is in scope) — otherwise it's an early SyntaxError, before anything evaluates.
        let allow_super = direct
            && (self.peek_binding("%homeobject%", caller_env).is_some()
                || self.peek_binding("%superproto%", caller_env).is_some());
        let body = crate::parser::parse_script_eval(
            code,
            base_strict,
            allow_new_target,
            allow_super,
            &private_names,
        )
        .map_err(|e| self.throw("SyntaxError", e.message))?;
        // A direct `eval` inherits the caller's super-call context: a `super(...)` in the eval is an
        // early SyntaxError unless the eval sits directly inside a derived constructor body. (Caught
        // here, before any of the eval body runs, so side effects preceding the `super()` don't.)
        if direct && !self.super_call_ok && stmts_have_super_call(&body) {
            return Err(self.throw("SyntaxError", "'super' keyword unexpected here"));
        }
        // A direct eval from class-field-initializer code may not reference `arguments` (also an
        // early error, checked before the body runs). The context is *lexical*: an arrow created
        // in an initializer keeps it, while an ordinary function (whose scope binds `arguments`)
        // shields it — so walk the caller's scope chain.
        let in_field_init = direct && {
            let mut found = false;
            let mut cur = Some(caller_env.clone());
            while let Some(sc) = cur {
                let b = sc.borrow();
                if b.vars.contains_key("arguments") {
                    break;
                }
                if b.vars.contains_key("%fieldinit%") {
                    found = true;
                    break;
                }
                cur = b.parent.clone();
            }
            found
        };
        if in_field_init && stmts_have_arguments_ref(&body) {
            return Err(self.throw(
                "SyntaxError",
                "'arguments' is not allowed in class field initializer code",
            ));
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
        self.hoist(body, &probe, &[]);
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
            // The variable environment itself may hold body-level lexicals (our function body
            // scope carries both); a var may not hoist over one of those either.
            for name in &var_names {
                if var_env.borrow().lexical_names.iter().any(|n| n == name) {
                    return Err(self.throw(
                        "SyntaxError",
                        format!("Identifier '{name}' has already been declared"),
                    ));
                }
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
        // The probe scope sits in every hoisted function's closure chain; empty it so their
        // variable references resolve through to the real environments (the probe's stale
        // `undefined` bindings must not shadow the caller's vars).
        probe.borrow_mut().vars.clear();
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
                    if !matches!(v, Value::Empty) {
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
        // The class scope opens before the heritage evaluates: a named class's own name is in
        // scope there — uninitialized (TDZ) until the constructor exists, and immutable.
        let outer_class_env = new_scope(Some(env.clone()));
        if let Some(n) = &class.name {
            outer_class_env.borrow_mut().vars.insert(
                n.clone(),
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
        let env = &outer_class_env;
        // Superclass and the prototype / static parents it implies.
        let parent = match &class.superclass {
            Some(e) => Some(self.eval(e, env)?),
            None => None,
        };
        // (IsConstructor runs BEFORE any `prototype` read; a callable non-constructor like
        // %IsHTMLDDA% is a TypeError without observable gets.)
        let (proto_parent, ctor_parent): (Option<Gc>, Option<Value>) = match &parent {
            None => (Some(self.object_proto.clone()), None),
            Some(Value::Null) => (None, None),
            Some(v @ Value::Obj(pc)) if self.value_is_constructor(v) => {
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
        // The spec's PrivateEnvironment: each class *evaluation* mints fresh runtime keys for its
        // private names and binds source name -> key in the class scope. `#x` in this class's code
        // resolves through the scope chain, so an instance of a different evaluation of the same
        // class source fails the brand check (and nested classes shadow outer private names).
        for m in &class.members {
            let src = match &m.key {
                PropKey::Ident(n) if n.starts_with('#') => n.clone(),
                _ => continue,
            };
            let seen = class_env.borrow().vars.contains_key(src.as_str());
            if !seen {
                self.accessor_seq += 1;
                let runtime = format!("{}\u{1}{}", src, self.accessor_seq);
                bind(&class_env, &src, Value::str(runtime.as_str()));
            }
        }
        let inst_env = new_scope(Some(class_env.clone()));
        // Instance members' [[HomeObject]] is the prototype: `super.x` resolves against its
        // *live* [[GetPrototypeOf]].
        bind(&inst_env, "%homeobject%", Value::Obj(proto.clone()));
        bind(&inst_env, "%superproto%", opt_obj(&proto_parent));
        // `extends null` binds Null (a super() call is a TypeError, not a SyntaxError).
        bind(
            &inst_env,
            "%superclass%",
            ctor_parent.clone().unwrap_or(if derived {
                Value::Null
            } else {
                Value::Undefined
            }),
        );
        let static_env = new_scope(Some(class_env.clone()));
        // Static elements' [[HomeObject]] is the constructor: their super base is the parent
        // constructor, or %Function.prototype% for a base class.
        bind(
            &static_env,
            "%superproto%",
            ctor_parent
                .clone()
                .unwrap_or_else(|| Value::Obj(self.function_proto.clone())),
        );

        // Build the constructor object on `proto`.
        let ctor_val = self.make_function(ctor_func, inst_env.clone());
        let ctor_obj = ctor_val.as_obj().unwrap().clone();
        // Static members' [[HomeObject]] is the constructor itself (bound after it exists; the
        // scope cell is shared with the already-captured static member environments).
        bind(&static_env, "%homeobject%", ctor_val.clone());
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
        // NamedEvaluation: an anonymous class takes its target name *before* any static
        // initializer runs.
        if class.name.is_none() {
            if let Some(n) = self.pending_fn_name.take() {
                self.set_fn_name(&ctor_val, &n);
            }
        }
        // A named class binds its own name (initialized) in the class scope, so methods, static
        // blocks, field initializers and decorators can reference the class itself.
        if let Some(n) = &class.name {
            if let Some(b) = outer_class_env.borrow_mut().vars.get_mut(n) {
                b.value = ctor_val.clone();
                b.initialized = true;
            }
        }

        // Methods, accessors and fields.
        let mut inst_fields: Vec<FieldInit> = Vec::new();
        // Instance private methods/accessors: stamped onto each instance when `this` is created
        // (PrivateMethodOrAccessorAdd), not placed on the prototype — so brand checks, double
        // initialization, and return-override semantics fall out of own-property checks.
        let mut priv_members: Vec<(String, Property)> = Vec::new();
        // Static elements (field initializers and static blocks) defer until every member's
        // computed key has been evaluated, then run in declaration order.
        type StaticEl = (Option<Rc<Function>>, String, Option<Expr>, Vec<Value>);
        let mut static_els: Vec<StaticEl> = Vec::new();
        let mut instance_inits: Vec<Value> = Vec::new();
        let mut static_inits: Vec<Value> = Vec::new();
        for m in &class.members {
            if m.kind == MemberKind::Constructor {
                continue;
            }
            let key = self.eval_prop_key(&m.key, env)?;
            // A *computed* static member key evaluating to "prototype" is a runtime TypeError
            // (the syntactic form is already an early error).
            if m.is_static && key == "prototype" && matches!(m.key, PropKey::Computed(_)) {
                return Err(self.throw(
                    "TypeError",
                    "classes may not have a static property named 'prototype'",
                ));
            }
            // Only a syntactic PrivateIdentifier is private; a computed key that merely
            // evaluates to a "#..." string is an ordinary property name.
            let is_private = matches!(&m.key, PropKey::Ident(n) if n.starts_with('#'));
            let key = if is_private {
                self.resolve_private(&key, &class_env)
            } else {
                key
            };
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
                    if is_private && !m.is_static {
                        priv_members.push((
                            key.clone(),
                            Property {
                                value: Value::Undefined,
                                get: Some(getter),
                                set: Some(setter),
                                accessor: true,
                                writable: false,
                                enumerable: false,
                                configurable: false,
                            },
                        ));
                    } else {
                        self.define_class_accessor(&target, &key, Some(getter), Some(setter));
                    }
                    if m.is_static {
                        static_els.push((None, backing.to_string(), m.value.clone(), transforms));
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
                        let name = self.fn_name_for_key(private_display(&key));
                        fo.borrow_mut().props.insert(
                            "name",
                            Property::data(Value::from_string(name), false, false, true),
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
                    let prop = if is_private {
                        // A private method is not writable: PrivateSet on it must TypeError.
                        Property::data(f, false, false, false)
                    } else {
                        Property::builtin(f)
                    };
                    if is_private && !m.is_static {
                        priv_members.push((key, prop));
                    } else {
                        target.borrow_mut().props.insert(key, prop);
                    }
                }
                MemberKind::Get | MemberKind::Set => {
                    let mut f = self.make_function(m.func.clone().unwrap(), menv.clone());
                    let is_get = m.kind == MemberKind::Get;
                    if let Value::Obj(fo) = &f {
                        let prefix = if is_get { "get " } else { "set " };
                        let name = self.fn_name_for_key(private_display(&key));
                        fo.borrow_mut().props.insert(
                            "name",
                            Property::data(
                                Value::from_string(format!("{prefix}{name}")),
                                false,
                                false,
                                true,
                            ),
                        );
                    }
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
                    if is_private && !m.is_static {
                        if let Some((_, p)) = priv_members.iter_mut().find(|(k, _)| *k == key) {
                            if get.is_some() {
                                p.get = get;
                            }
                            if set.is_some() {
                                p.set = set;
                            }
                        } else {
                            priv_members.push((
                                key,
                                Property {
                                    value: Value::Undefined,
                                    get,
                                    set,
                                    accessor: true,
                                    writable: false,
                                    enumerable: false,
                                    configurable: false,
                                },
                            ));
                        }
                    } else {
                        self.define_class_accessor(&target, &key, get, set);
                    }
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
                        static_els.push((None, key, m.value.clone(), transforms));
                    } else {
                        inst_fields.push(FieldInit {
                            key,
                            init: m.value.clone(),
                            transforms,
                        });
                    }
                }
                MemberKind::StaticBlock => {
                    if let Some(func) = &m.func {
                        static_els.push((Some(func.clone()), String::new(), None, Vec::new()));
                    }
                }
                MemberKind::Constructor => {}
            }
        }

        for (block, key, init, transforms) in static_els {
            let scope = new_scope(Some(static_env.clone()));
            bind(&scope, "this", ctor_val.clone());
            if block.is_none() {
                bind(&scope, "%fieldinit%", Value::Bool(true));
            }
            if let Some(func) = block {
                // A static block instantiates its declarations like a function body,
                // including a `using` disposal frame.
                self.hoist(&func.body, &scope, &[]);
                self.declare_block_lexicals(&func.body, &scope, false);
                let has_using = func.body.iter().any(stmt_declares_using);
                if has_using {
                    self.using_stack.push(Vec::new());
                }
                let mut result: Completion = Ok(Value::Undefined);
                for stmt in &func.body {
                    if let Err(e) = self.exec_stmt(stmt, &scope) {
                        result = Err(e);
                        break;
                    }
                }
                if has_using {
                    let frame = self.using_stack.pop().unwrap_or_default();
                    result = self.dispose_frame(frame, result);
                }
                result?;
                continue;
            }
            // A static field initializer is field-initializer code: no super() and no
            // `arguments` (even through a direct eval); an anonymous function value takes
            // the field's name.
            let saved_super = self.super_call_ok;
            let saved_field = self.in_field_init_code;
            self.super_call_ok = false;
            self.in_field_init_code = true;
            let v = match &init {
                Some(e) => {
                    let v = self.eval(e, &scope);
                    if let (Ok(v), true) = (&v, is_anonymous_fn(e)) {
                        self.set_fn_name(v, private_display(&key));
                    }
                    v
                }
                None => Ok(Value::Undefined),
            };
            self.super_call_ok = saved_super;
            self.in_field_init_code = saved_field;
            let mut v = v?;
            for tr in &transforms {
                v = self.call(tr.clone(), ctor_val.clone(), &[v])?;
            }
            // Static PrivateFieldAdd: a non-extensible constructor (the initializer may have
            // sealed it) or a duplicate is a TypeError.
            if Interp::is_private_key(&key)
                && (ctor_obj.borrow().props.contains(key.as_str()) || !ctor_obj.borrow().extensible)
            {
                return Err(self.throw(
                    "TypeError",
                    "cannot add a private field to a non-extensible object",
                ));
            }
            ctor_obj.borrow_mut().props.insert(key, Property::plain(v));
        }

        self.gc_pin(&ctor_obj);
        self.class_info.insert(
            Rc::as_ptr(&ctor_obj) as usize,
            ClassInfo {
                fields: inst_fields,
                field_env: inst_env,
                derived,
                instance_initializers: instance_inits,
                private_members: priv_members,
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
            // A private member's context name is its source spelling, not the runtime key.
            Value::from_string(private_display(key).to_string())
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
        let is_private = Interp::is_private_key(key);
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
    ) -> Result<Value, Abrupt> {
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
            // A bound-function parent constructs through its target with the bound arguments
            // prepended (BoundFunction [[Construct]]).
            Callable::Bound {
                target,
                args: bargs,
                ..
            } => {
                let mut all = bargs.clone();
                all.extend_from_slice(args);
                self.run_constructor_on(&Value::Obj(target), this, &all)
            }
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
                r
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
                        // A Function (or GeneratorFunction/AsyncFunction) subclass instance is
                        // itself callable: carry the built-in's [[Call]] behavior onto `this`.
                        let src_call = src.borrow().call.clone();
                        if !matches!(src_call, Callable::None)
                            && matches!(dst.borrow().call, Callable::None)
                        {
                            let is_ctor = src.borrow().is_constructor;
                            let mut db = dst.borrow_mut();
                            db.call = src_call;
                            db.is_constructor = is_ctor;
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
                        self.gc_pin(dst);
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
                            // The native ctor's resolvers are bound to `src`; forward them.
                            self.promise_forward.insert(sp, this.clone());
                        }
                        if let Some(v) = self.temporal.remove(&sp) {
                            self.temporal.insert(dp, v);
                        }
                    }
                }
                Ok(Value::Undefined)
            }
            _ => Err(self.throw("TypeError", "super target is not a constructor")),
        }
    }

    /// CopyDataProperties(rest, ToObject(value), excludedNames): own keys in [[OwnPropertyKeys]]
    /// order (through a proxy's ownKeys/getOwnPropertyDescriptor traps, symbols included), copying
    /// each enumerable non-excluded property.
    pub(crate) fn copy_data_properties(
        &mut self,
        value: &Value,
        excluded: &[String],
    ) -> Result<Gc, Abrupt> {
        let rest = self.new_object();
        self.copy_data_properties_into(&rest, value, excluded)?;
        Ok(rest)
    }

    fn copy_data_properties_into(
        &mut self,
        rest: &Gc,
        value: &Value,
        excluded: &[String],
    ) -> Result<(), Abrupt> {
        let is_excluded = |k: &str| excluded.iter().any(|x| x == k);
        if let Some((t, h)) = crate::builtins::proxy_pair(self, value) {
            let keys = crate::builtins::proxy_own_keys(self, &t, &h).map_err(Abrupt::Throw)?;
            for k in keys {
                let pk = self.to_property_key(&k)?;
                if is_excluded(&pk) {
                    continue;
                }
                let desc =
                    crate::builtins::proxy_gopd_value(self, &t, &h, &pk).map_err(Abrupt::Throw)?;
                if matches!(desc, Value::Undefined) {
                    continue;
                }
                let e = self.get_member(&desc, "enumerable")?;
                if self.to_boolean(&e) {
                    let v = self.get_member(value, &pk)?;
                    rest.borrow_mut()
                        .props
                        .insert(pk, crate::value::Property::plain(v));
                }
            }
            return Ok(());
        }
        match value {
            Value::Obj(src) => {
                let keys = src.borrow().props.ordered_keys();
                for k in keys {
                    if is_excluded(&k) {
                        continue;
                    }
                    let enumerable = src
                        .borrow()
                        .props
                        .get(&k)
                        .map(|p| p.enumerable)
                        .unwrap_or(false);
                    if enumerable {
                        let v = self.get_member(value, &k)?;
                        rest.borrow_mut()
                            .props
                            .insert(k, crate::value::Property::plain(v));
                    }
                }
            }
            // ToObject(string): copy the enumerable index properties.
            Value::Str(s) => {
                for (idx, ch) in s.chars().enumerate() {
                    let k = idx.to_string();
                    if is_excluded(&k) {
                        continue;
                    }
                    set_data(rest, &k, Value::from_string(ch.to_string()));
                }
            }
            // Other primitives wrap to an object with no own enumerable properties.
            _ => {}
        }
        Ok(())
    }

    fn init_instance_fields(&mut self, ctor: &Value, this: &Value) -> Result<(), Abrupt> {
        let obj = match ctor {
            Value::Obj(o) => o.clone(),
            _ => return Ok(()),
        };
        let ptr = Rc::as_ptr(&obj) as usize;
        let (fields, field_env, initializers, priv_members) = match self.class_info.get(&ptr) {
            Some(i) => (
                i.fields
                    .iter()
                    .map(|f| (f.key.clone(), f.init.clone(), f.transforms.clone()))
                    .collect::<Vec<_>>(),
                i.field_env.clone(),
                i.instance_initializers.clone(),
                i.private_members.clone(),
            ),
            None => return Ok(()),
        };
        // PrivateMethodOrAccessorAdd: stamp the class's private methods/accessors on the instance
        // before any field initializer runs; a second initialization (return-override tricks
        // constructing over the same object twice) is a TypeError.
        if let Value::Obj(o) = this {
            if let Some((k, _)) = priv_members.first() {
                if o.borrow().props.contains(k.as_str()) {
                    return Err(self.throw(
                        "TypeError",
                        "cannot initialize private methods of a class twice on the same object",
                    ));
                }
            }
            // Extensibility applies to every private element, methods and accessors included.
            if !priv_members.is_empty() && !o.borrow().extensible {
                return Err(self.throw(
                    "TypeError",
                    "cannot add private members to a non-extensible object",
                ));
            }
            for (k, p) in &priv_members {
                o.borrow_mut().props.insert(k.as_str(), p.clone());
            }
        }
        // A field initializer is not a constructor: a `super(...)` reached from here (e.g. through a
        // direct `eval`) is illegal, so clear the flag for the duration of the initializers.
        let saved_super = self.super_call_ok;
        let saved_field = self.in_field_init_code;
        self.super_call_ok = false;
        self.in_field_init_code = true;
        let result = (|me: &mut Self| -> Result<(), Abrupt> {
            for (key, init, transforms) in fields {
                let scope = new_scope(Some(field_env.clone()));
                bind(&scope, "this", this.clone());
                // Field-initializer code: a direct eval from here (or from an arrow created
                // here) may not reference `arguments`.
                bind(&scope, "%fieldinit%", Value::Bool(true));
                let mut v = match init {
                    Some(e) => {
                        let v = me.eval(&e, &scope)?;
                        if is_anonymous_fn(&e) {
                            me.set_fn_name(&v, private_display(&key));
                        }
                        v
                    }
                    None => Value::Undefined,
                };
                // Decorator-supplied field initializers transform the value in turn.
                for t in &transforms {
                    v = me.call(t.clone(), this.clone(), &[v])?;
                }
                if Interp::is_private_key(&key) {
                    // PrivateFieldAdd: stamped directly on the object (bypassing proxy traps);
                    // a second add or a non-extensible receiver is a TypeError.
                    let Value::Obj(o) = this else {
                        return Err(me.throw("TypeError", "cannot add a private field"));
                    };
                    if o.borrow().props.contains(key.as_str()) {
                        return Err(me.throw(
                            "TypeError",
                            "cannot initialize the same private field twice",
                        ));
                    }
                    if !o.borrow().extensible {
                        return Err(me.throw(
                            "TypeError",
                            "cannot add a private field to a non-extensible object",
                        ));
                    }
                    o.borrow_mut().props.insert(
                        key.as_str(),
                        crate::value::Property::data(v, true, false, false),
                    );
                } else {
                    // DefineField: CreateDataPropertyOrThrow (an own data property, even over a
                    // setter — and a [[DefineOwnProperty]] on a deferred namespace receiver).
                    crate::builtins::cdp_or_throw(me, this, &key, v).map_err(Abrupt::Throw)?;
                }
            }
            // Decorator addInitializer callbacks run after the fields, with `this` = the instance.
            for init in &initializers {
                me.call(init.clone(), this.clone(), &[])?;
            }
            Ok(())
        })(self);
        self.super_call_ok = saved_super;
        self.in_field_init_code = saved_field;
        result
    }

    fn eval_unary(&mut self, op: &str, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        if op == "typeof" {
            // typeof on an unresolved identifier yields "undefined" rather than throwing.
            if let Expr::Ident(name) = arg {
                match self.get_var(name, env) {
                    Ok(v) if self.is_htmldda(&v) => return Ok(Value::str("undefined")),
                    Ok(v) => return Ok(Value::from_string(v.type_of().to_string())),
                    // A binding in its temporal dead zone still throws; only a truly-unresolved
                    // name yields "undefined".
                    Err(e) if self.binding_in_tdz(name, env) => return Err(e),
                    Err(_) => return Ok(Value::str("undefined")),
                }
            }
            let v = self.eval(arg, env)?;
            if self.is_htmldda(&v) {
                return Ok(Value::str("undefined"));
            }
            return Ok(Value::from_string(v.type_of().to_string()));
        }
        if op == "delete" {
            return self.eval_delete(arg, env);
        }
        let v = self.eval(arg, env)?;
        // `-`/`~` apply ToNumeric: a BigInt (or BigInt wrapper object) stays a BigInt.
        let v = if matches!(op, "-" | "~") && matches!(v, Value::Obj(_)) {
            self.to_primitive(&v, Hint::Number)?
        } else {
            v
        };
        if let Value::BigInt(n) = v {
            return match op {
                "!" => Ok(Value::Bool(n.is_zero())),
                "-" => Ok(Value::BigInt(n.neg())),
                "~" => Ok(Value::BigInt(n.not())),
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

    /// Whether `key` names a non-configurable own property of a string primitive ("length" or an
    /// in-range canonical integer index).
    fn string_own_key(&self, s: &std::rc::Rc<str>, key: &str) -> bool {
        if key == "length" {
            return true;
        }
        match key.parse::<usize>() {
            Ok(i) => (key.len() == 1 || !key.starts_with('0')) && i < crate::jstr::unit_len(s),
            Err(_) => false,
        }
    }

    fn eval_delete(&mut self, arg: &Expr, env: &Env) -> Result<Value, Abrupt> {
        match arg {
            Expr::Member { obj, prop, .. } => {
                // `delete super.x` is a runtime ReferenceError (the reference is a super
                // reference) — after GetThisBinding (TDZ ReferenceError first).
                if matches!(**obj, Expr::Super) {
                    self.get_var("this", env)?;
                    return Err(self.throw("ReferenceError", "cannot delete a super property"));
                }
                let base = self.eval(obj, env)?;
                if !self.short_circuit && matches!(base, Value::Undefined | Value::Null) {
                    return Err(self.throw(
                        "TypeError",
                        format!("cannot delete property '{prop}' of null or undefined"),
                    ));
                }
                if let Value::Obj(o) = &base {
                    self.defer_trigger(o, Some(prop))?;
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
                        self.unmap_argument(Rc::as_ptr(o) as usize, prop);
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
                // A string primitive's `length` and in-range indices are non-configurable own
                // properties of the string exotic wrapper: [[Delete]] is false (TypeError strict).
                if let Value::Str(sv) = &base {
                    if self.string_own_key(sv, prop) {
                        if self.strict {
                            return Err(self.throw(
                                "TypeError",
                                format!("cannot delete non-configurable property '{prop}'"),
                            ));
                        }
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            Expr::Index { obj, index, .. } => {
                if matches!(**obj, Expr::Super) {
                    // GetThisBinding first (TDZ ReferenceError), then the key expression
                    // evaluates — without ToPropertyKey — before the ReferenceError.
                    self.get_var("this", env)?;
                    self.eval(index, env)?;
                    return Err(self.throw("ReferenceError", "cannot delete a super property"));
                }
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                let key = self.to_property_key(&idx)?;
                if !self.short_circuit && matches!(base, Value::Undefined | Value::Null) {
                    return Err(self.throw(
                        "TypeError",
                        format!("cannot delete property '{key}' of null or undefined"),
                    ));
                }
                if let Value::Obj(o) = &base {
                    self.defer_trigger(o, Some(&key))?;
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
                        self.unmap_argument(Rc::as_ptr(o) as usize, &key);
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
                if let Value::Str(sv) = &base {
                    if self.string_own_key(sv, &key) {
                        if self.strict {
                            return Err(self.throw(
                                "TypeError",
                                format!("cannot delete non-configurable property '{key}'"),
                            ));
                        }
                        return Ok(Value::Bool(false));
                    }
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
            // Any other operand evaluates (for its side effects) and deletes to `true`.
            other => {
                self.eval(other, env)?;
                Ok(Value::Bool(true))
            }
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
        // Resolve the reference once, then GetValue/PutValue through it (spec Reference semantics).
        let mut lref = self.resolve_reference(arg, env)?;
        let old = self.get_reference(&mut lref)?;
        if let Value::BigInt(n) = old {
            let one = crate::bigint::JsBigInt::from_u64(1);
            let new = if op == "++" { n.add(&one) } else { n.sub(&one) };
            self.put_reference(&mut lref, Value::BigInt(new.clone()))?;
            return Ok(Value::BigInt(if prefix { new } else { n }));
        }
        let n = self.to_number(&old)?;
        let new = if op == "++" { n + 1.0 } else { n - 1.0 };
        self.put_reference(&mut lref, Value::Num(new))?;
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
            // A destructuring target evaluates the RHS first, then iterates the pattern.
            if matches!(target, Expr::Array(_) | Expr::Object(_)) {
                let v = self.eval(value, env)?;
                self.assign_to_target(target, v.clone(), env)?;
                return Ok(v);
            }
            // A simple target evaluates its Reference (base + computed key expression) BEFORE the
            // RHS; ToPropertyKey and the base's RequireObjectCoercible are deferred to PutValue.
            let mut lref = self.resolve_reference(target, env)?;
            if let Expr::Ident(n) = target {
                if matches!(value, Expr::Class(c) if c.name.is_none()) {
                    self.pending_fn_name = Some(n.clone());
                }
            }
            let v = self.eval(value, env)?;
            self.pending_fn_name = None;
            // `f = function(){}` names the anonymous function after the target identifier.
            if let Expr::Ident(n) = target {
                if is_anonymous_fn(value) {
                    self.set_fn_name(&v, n);
                }
            }
            self.put_reference(&mut lref, v.clone())?;
            return Ok(v);
        }
        // The LHS reference is resolved once and reused for GetValue + PutValue, so a `with`-object
        // getter that mutates the binding (or a member base with side effects) is evaluated once.
        let mut lref = self.resolve_reference(target, env)?;
        // Logical assignment (&&=, ||=, ??=) short-circuits.
        if matches!(op, "&&=" | "||=" | "??=") {
            let cur = self.get_reference(&mut lref)?;
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
            // `x ||= function(){}` names the anonymous function after an identifier target.
            if let Expr::Ident(n) = target {
                if is_anonymous_fn(value) {
                    self.set_fn_name(&v, n);
                }
            }
            self.put_reference(&mut lref, v.clone())?;
            return Ok(v);
        }
        // Compound arithmetic/bitwise: a op= b  ≡  a = a <op> b.
        let cur = self.get_reference(&mut lref)?;
        let rhs = self.eval(value, env)?;
        let bin_op = &op[..op.len() - 1];
        let result = self.binary(bin_op, cur, rhs)?;
        self.put_reference(&mut lref, result.clone())?;
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
                if prop.starts_with('#') {
                    let k = self.resolve_private(prop, env);
                    return self.set_private_member(&base, &k, value);
                }
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
                    // A throw from `next` (IteratorStep) marks the record done, so IteratorClose
                    // is skipped; a throw from a target assignment leaves it not-done (still closes).
                    macro_rules! step {
                        () => {
                            match me.iterator_step(&iter, &next) {
                                Ok(v) => v,
                                Err(e) => {
                                    done = true;
                                    return Err(e);
                                }
                            }
                        };
                    }
                    // One iterator element (undefined past the end).
                    macro_rules! next_value {
                        () => {
                            if done {
                                Value::Undefined
                            } else {
                                match step!() {
                                    Some(x) => x,
                                    None => {
                                        done = true;
                                        Value::Undefined
                                    }
                                }
                            }
                        };
                    }
                    for el in elems {
                        match el {
                            ArrayElem::Hole => {
                                if !done && step!().is_none() {
                                    done = true;
                                }
                            }
                            ArrayElem::Spread(t) => {
                                // A non-literal rest target's Reference is evaluated BEFORE the
                                // iterator is drained (AssignmentRestElement step 1).
                                if matches!(t, Expr::Array(_) | Expr::Object(_)) {
                                    let mut rest = Vec::new();
                                    while !done {
                                        match step!() {
                                            Some(x) => rest.push(x),
                                            None => done = true,
                                        }
                                    }
                                    let arr = me.make_array(rest);
                                    me.assign_to_target(t, arr, env)?;
                                } else {
                                    let mut lref = me.resolve_reference(t, env)?;
                                    let mut rest = Vec::new();
                                    while !done {
                                        match step!() {
                                            Some(x) => rest.push(x),
                                            None => done = true,
                                        }
                                    }
                                    let arr = me.make_array(rest);
                                    me.put_reference(&mut lref, arr)?;
                                }
                            }
                            ArrayElem::Item(t) => {
                                // Split an optional `= default`; a non-literal target's Reference is
                                // evaluated BEFORE the iterator step (AssignmentElement step 1).
                                let (core, default) = match t {
                                    Expr::Assign {
                                        op: "=",
                                        target,
                                        value,
                                    } => (&**target, Some(&**value)),
                                    _ => (t, None),
                                };
                                if matches!(core, Expr::Array(_) | Expr::Object(_)) {
                                    let mut v = next_value!();
                                    if matches!(v, Value::Undefined) {
                                        if let Some(d) = default {
                                            v = me.eval(d, env)?;
                                        }
                                    }
                                    me.assign_to_target(core, v, env)?;
                                } else {
                                    let mut lref = me.resolve_reference(core, env)?;
                                    let mut v = next_value!();
                                    if matches!(v, Value::Undefined) {
                                        if let Some(d) = default {
                                            v = me.eval(d, env)?;
                                            if let (Expr::Ident(n), true) =
                                                (core, is_anonymous_fn(d))
                                            {
                                                me.set_fn_name(&v, n);
                                            }
                                        }
                                    }
                                    me.put_reference(&mut lref, v)?;
                                }
                            }
                        }
                    }
                    Ok(())
                })(self);
                // IteratorClose: on normal completion propagate its abrupt (a throwing/non-object
                // `return`); on abrupt completion close but keep the original error.
                match result {
                    Ok(()) => {
                        if !done {
                            self.iterator_close_normal(&iter_close)?;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        if !done {
                            if matches!(e, Abrupt::Throw(_)) {
                                self.iterator_close(&iter_close);
                            } else {
                                // A non-throw completion (return/break/continue): a throwing or
                                // non-object `return` replaces it; otherwise it propagates.
                                self.iterator_close_normal(&iter_close)?;
                            }
                        }
                        Err(e)
                    }
                }
            }
            Expr::Object(props) => {
                if matches!(value, Value::Undefined | Value::Null) {
                    return Err(self.throw("TypeError", "cannot destructure null or undefined"));
                }
                let mut taken: Vec<String> = Vec::new();
                for prop in props {
                    match prop {
                        PropDef::KeyValue { .. } | PropDef::Proto(_) => {
                            // KeyedDestructuringAssignmentEvaluation: evaluate the property name,
                            // then the target Reference, THEN GetV(value, name) — for a non-literal
                            // target the reference is evaluated before the source is read. As a
                            // pattern, `__proto__: t` is a normal keyed target (not a proto-setter).
                            let (k, t) = match prop {
                                PropDef::KeyValue { key, value } => {
                                    (self.propkey_to_string(key, env)?, value)
                                }
                                PropDef::Proto(value) => ("__proto__".to_string(), value),
                                _ => unreachable!(),
                            };
                            taken.push(k.clone());
                            let (core, default) = match t {
                                Expr::Assign {
                                    op: "=",
                                    target,
                                    value,
                                } => (&**target, Some(&**value)),
                                _ => (t, None),
                            };
                            if matches!(core, Expr::Array(_) | Expr::Object(_)) {
                                let mut v = self.get_member(&value, &k)?;
                                if matches!(v, Value::Undefined) {
                                    if let Some(d) = default {
                                        v = self.eval(d, env)?;
                                    }
                                }
                                self.assign_to_target(core, v, env)?;
                            } else {
                                let mut lref = self.resolve_reference(core, env)?;
                                let mut v = self.get_member(&value, &k)?;
                                if matches!(v, Value::Undefined) {
                                    if let Some(d) = default {
                                        v = self.eval(d, env)?;
                                        if let (Expr::Ident(n), true) = (core, is_anonymous_fn(d)) {
                                            self.set_fn_name(&v, n);
                                        }
                                    }
                                }
                                self.put_reference(&mut lref, v)?;
                            }
                        }
                        PropDef::Spread(t) => {
                            // CopyDataProperties(rest, ToObject(value), excludedNames = taken).
                            let rest = self.copy_data_properties(&value, &taken)?;
                            self.assign_to_target(t, Value::Obj(rest), env)?;
                        }
                        _ => return Err(self.throw("SyntaxError", "invalid destructuring target")),
                    }
                }
                Ok(())
            }
            // Annex B web compat: assigning to a CallExpression evaluates the call, then throws.
            Expr::Call { .. } => {
                self.eval(target, env)?;
                Err(self.throw("ReferenceError", "invalid assignment target"))
            }
            _ => Err(self.throw("ReferenceError", "invalid assignment target")),
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
            "+" | "-" | "*" | "/" | "%" | "**" | "&" | "|" | "^" | "<<" | ">>" | ">>>"
        ) {
            // `+` primes both operands with ToPrimitive first (string concatenation dispatch);
            // every other operator applies ToNumeric to the left operand *completely* before
            // touching the right one.
            if op != "+" {
                let lp = self.to_primitive(&l, Hint::Number)?;
                let ln = if matches!(lp, Value::BigInt(_)) {
                    lp
                } else {
                    Value::Num(self.to_number(&lp)?)
                };
                let rp = self.to_primitive(&r, Hint::Number)?;
                let rn = if matches!(rp, Value::BigInt(_)) {
                    rp
                } else {
                    Value::Num(self.to_number(&rp)?)
                };
                if matches!(ln, Value::BigInt(_)) || matches!(rn, Value::BigInt(_)) {
                    if let (Value::BigInt(x), Value::BigInt(y)) = (&ln, &rn) {
                        return self.bigint_binop(op, x, y);
                    }
                    return Err(self.throw(
                        "TypeError",
                        "Cannot mix BigInt and other types, use explicit conversions",
                    ));
                }
                let (a, b) = match (&ln, &rn) {
                    (Value::Num(a), Value::Num(b)) => (*a, *b),
                    _ => unreachable!(),
                };
                return match op {
                    "-" => Ok(Value::Num(a - b)),
                    "*" => Ok(Value::Num(a * b)),
                    "/" => Ok(Value::Num(a / b)),
                    "%" => Ok(Value::Num(js_mod(a, b))),
                    // Number::exponentiate: a NaN exponent is NaN even for base 1, and |base| 1
                    // with an infinite exponent is NaN (powf disagrees on both).
                    "**" => Ok(Value::Num(
                        if b.is_nan() || (a.abs() == 1.0 && b.is_infinite()) {
                            f64::NAN
                        } else {
                            a.powf(b)
                        },
                    )),
                    "&" => Ok(Value::Num(
                        (self.to_int32(&ln)? & self.to_int32(&rn)?) as f64,
                    )),
                    "|" => Ok(Value::Num(
                        (self.to_int32(&ln)? | self.to_int32(&rn)?) as f64,
                    )),
                    "^" => Ok(Value::Num(
                        (self.to_int32(&ln)? ^ self.to_int32(&rn)?) as f64,
                    )),
                    "<<" => Ok(Value::Num(
                        (self.to_int32(&ln)?.wrapping_shl(self.to_uint32(&rn)? & 31)) as f64,
                    )),
                    ">>" => Ok(Value::Num(
                        (self.to_int32(&ln)? >> (self.to_uint32(&rn)? & 31)) as f64,
                    )),
                    ">>>" => Ok(Value::Num(
                        (self.to_uint32(&ln)? >> (self.to_uint32(&rn)? & 31)) as f64,
                    )),
                    _ => unreachable!(),
                };
            }
            let hint = Hint::Default;
            let lp = self.to_primitive(&l, hint)?;
            let rp = self.to_primitive(&r, hint)?;
            if op == "+" && (matches!(lp, Value::Str(_)) || matches!(rp, Value::Str(_))) {
                let ls = self.to_string(&lp)?;
                let rs = self.to_string(&rp)?;
                if ls.len() + rs.len() > MAX_STR_LEN {
                    return Err(self.throw("RangeError", "Invalid string length"));
                }
                return Ok(Value::from_string(crate::jstr::concat(&ls, &rs)));
            }
            if matches!(lp, Value::BigInt(_)) || matches!(rp, Value::BigInt(_)) {
                if let (Value::BigInt(x), Value::BigInt(y)) = (&lp, &rp) {
                    return self.bigint_binop(op, x, y);
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
            ">>>" => unreachable!("handled by the numeric block"),
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

    fn bigint_binop(
        &self,
        op: &str,
        x: &crate::bigint::JsBigInt,
        y: &crate::bigint::JsBigInt,
    ) -> Result<Value, Abrupt> {
        use crate::bigint::JsBigInt;
        let v = match op {
            "+" => x.add(y),
            "-" => x.sub(y),
            "*" => x.mul(y),
            "/" => x
                .div(y)
                .ok_or_else(|| self.throw("RangeError", "Division by zero"))?,
            "%" => x
                .rem(y)
                .ok_or_else(|| self.throw("RangeError", "Division by zero"))?,
            "**" => x
                .pow(y)
                .ok_or_else(|| self.throw("RangeError", "Exponent must be non-negative"))?,
            "&" => x.bitand(y),
            "|" => x.bitor(y),
            "^" => x.bitxor(y),
            // A negative shift count shifts the other way; the rightward shift is arithmetic
            // (flooring toward negative infinity).
            "<<" | ">>" => {
                let leftward = (op == "<<") != y.is_negative();
                let count = y.to_i128().map(|v| v.unsigned_abs()).unwrap_or(u128::MAX);
                if leftward {
                    if x.is_zero() {
                        JsBigInt::zero()
                    } else if count > (1 << 30) {
                        return Err(self.throw("RangeError", "BigInt is too large to allocate"));
                    } else {
                        x.shl(count as u64)
                    }
                } else if count >= (1 << 30) {
                    // Every bit shifted out: floor(x / 2^count) is 0 or -1 by sign.
                    if x.is_negative() {
                        JsBigInt::from_i128(-1)
                    } else {
                        JsBigInt::zero()
                    }
                } else {
                    x.shr(count as u64)
                }
            }
            _ => return Err(self.throw("TypeError", "unsupported BigInt operator")),
        };
        Ok(Value::BigInt(v))
    }

    /// ToBigInt: BigInt stays; Boolean → 0n/1n; String parses (SyntaxError if malformed); Number /
    /// undefined / null / Symbol throw TypeError.
    pub(crate) fn to_bigint(&mut self, v: &Value) -> Result<crate::bigint::JsBigInt, Abrupt> {
        let p = self.to_primitive(v, Hint::Number)?;
        match p {
            Value::BigInt(n) => Ok(n),
            Value::Bool(b) => Ok(crate::bigint::JsBigInt::from_u64(b as u64)),
            Value::Str(s) => string_to_bigint(&s)
                .ok_or_else(|| self.throw("SyntaxError", "Cannot convert string to a BigInt")),
            Value::Num(_) => Err(self.throw("TypeError", "Cannot convert a Number to a BigInt")),
            _ => Err(self.throw("TypeError", "Cannot convert value to a BigInt")),
        }
    }

    fn compare(&mut self, op: &str, l: Value, r: Value) -> Result<Value, Abrupt> {
        let lp = self.to_primitive(&l, Hint::Number)?;
        let rp = self.to_primitive(&r, Hint::Number)?;
        if let (Value::Str(a), Value::Str(b)) = (&lp, &rp) {
            // String relational comparison is per UTF-16 code unit (which differs from `str`
            // byte order once supplementary-plane characters are involved).
            let ord = crate::jstr::cmp_units(a, b);
            let res = match op {
                "<" => ord.is_lt(),
                ">" => ord.is_gt(),
                "<=" => ord.is_le(),
                ">=" => ord.is_ge(),
                _ => unreachable!(),
            };
            return Ok(Value::Bool(res));
        }
        // BigInt operands compare exactly (never through f64, which would lose precision).
        let ord: Option<std::cmp::Ordering> = match (&lp, &rp) {
            (Value::BigInt(a), Value::BigInt(b)) => Some(a.cmp(b)),
            (Value::BigInt(a), Value::Str(t)) => {
                // StringToBigInt: an unparsable string makes the comparison undefined (false).
                string_to_bigint(t).map(|b| a.cmp(&b))
            }
            (Value::Str(t), Value::BigInt(b)) => string_to_bigint(t).map(|a| a.cmp(b)),
            (Value::BigInt(a), _) => {
                let n = self.to_number(&rp)?;
                a.cmp_f64(n)
            }
            (_, Value::BigInt(b)) => {
                let n = self.to_number(&lp)?;
                b.cmp_f64(n).map(std::cmp::Ordering::reverse)
            }
            _ => {
                let a = self.to_number(&lp)?;
                let b = self.to_number(&rp)?;
                if a.is_nan() || b.is_nan() {
                    None
                } else {
                    a.partial_cmp(&b)
                }
            }
        };
        let res = match ord {
            None => false,
            Some(ord) => match op {
                "<" => ord.is_lt(),
                ">" => ord.is_gt(),
                "<=" => ord.is_le(),
                ">=" => ord.is_ge(),
                _ => unreachable!(),
            },
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
            Value::Undefined | Value::Empty | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::BigInt(n) => !n.is_zero(),
            Value::Str(s) => !s.is_empty(),
            // The [[IsHTMLDDA]] object is the one falsy Object.
            Value::Obj(_) => !self.is_htmldda(v),
            Value::Sym(_) => true,
        }
    }

    pub fn to_number(&mut self, v: &Value) -> Result<f64, Abrupt> {
        Ok(match v {
            Value::Undefined | Value::Empty => f64::NAN,
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
            Value::Undefined | Value::Empty => Rc::from("undefined"),
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

    /// IsConstructor: whether `v` has a [[Construct]] internal method.
    pub(crate) fn value_is_constructor(&self, v: &Value) -> bool {
        let Value::Obj(o) = v else { return false };
        if let Some((target, _)) = self.proxies.get(&(Rc::as_ptr(o) as usize)) {
            // A proxy is a constructor exactly when its target is.
            return self.value_is_constructor(&target.clone());
        }
        let b = o.borrow();
        match &b.call {
            Callable::User(f, _) => !(f.is_arrow || f.is_method || f.is_generator || f.is_async),
            Callable::Native(_) => {
                b.is_constructor
                    || b.props
                        .get("prototype")
                        .map(|p| !p.accessor)
                        .unwrap_or(false)
            }
            // A bound function constructs exactly when its target does.
            Callable::Bound { target, .. } => {
                self.value_is_constructor(&Value::Obj(target.clone()))
            }
            _ => false,
        }
    }

    pub fn loose_equals(&mut self, a: &Value, b: &Value) -> Result<bool, Abrupt> {
        // [[IsHTMLDDA]] compares loosely equal to undefined/null (and to itself, as an object).
        if matches!(a, Value::Undefined | Value::Null) && self.is_htmldda(b)
            || matches!(b, Value::Undefined | Value::Null) && self.is_htmldda(a)
        {
            return Ok(true);
        }
        Ok(match (a, b) {
            (Value::Undefined | Value::Null, Value::Undefined | Value::Null) => true,
            (Value::BigInt(x), Value::BigInt(y)) => x == y,
            (Value::BigInt(x), Value::Num(y)) | (Value::Num(y), Value::BigInt(x)) => x.eq_f64(*y),
            (Value::BigInt(x), Value::Str(s)) | (Value::Str(s), Value::BigInt(x)) => {
                // StringToBigInt: an unparsable string compares unequal.
                string_to_bigint(s).map(|n| n == *x).unwrap_or(false)
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
            (Value::Obj(_), Value::Num(_) | Value::Str(_) | Value::Sym(_)) => {
                let ap = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(&ap, b)?
            }
            (Value::Num(_) | Value::Str(_) | Value::Sym(_), Value::Obj(_)) => {
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
pub(crate) fn bind(env: &Env, name: &str, value: Value) {
    env.borrow_mut().vars.insert(
        name.to_string(),
        Binding {
            value,
            mutable: true,
            strict_immutable: false,
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
        is_fn_expr: false,
        source: None,
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

/// `Contains` over eval code: search for a node matching `pred`, descending into arrow functions
/// (their parameter defaults and bodies) but not into ordinary functions or class bodies.
fn stmts_contain(stmts: &[Stmt], pred: fn(&Expr) -> bool) -> bool {
    stmts.iter().any(|s| stmt_contains(s, pred))
}

fn stmt_contains(s: &Stmt, pred: fn(&Expr) -> bool) -> bool {
    let e = |x: &Expr| expr_contains(x, pred);
    match s {
        Stmt::Expr(x) | Stmt::Throw(x) => e(x),
        Stmt::Return(x) => x.as_ref().is_some_and(&e),
        Stmt::VarDecl { decls, .. } => decls.iter().any(|(_, init)| init.as_ref().is_some_and(&e)),
        Stmt::If { test, cons, alt } => {
            e(test)
                || stmt_contains(cons, pred)
                || alt.as_deref().is_some_and(|s| stmt_contains(s, pred))
        }
        Stmt::Block(b) => stmts_contain(b, pred),
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            e(test) || stmt_contains(body, pred)
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_deref().is_some_and(|i| match i {
                ForInit::Expr(x) => e(x),
                ForInit::VarDecl { decls, .. } => {
                    decls.iter().any(|(_, x)| x.as_ref().is_some_and(&e))
                }
            }) || test.as_ref().is_some_and(&e)
                || update.as_ref().is_some_and(&e)
                || stmt_contains(body, pred)
        }
        Stmt::ForInOf { right, body, .. } => e(right) || stmt_contains(body, pred),
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            stmts_contain(block, pred)
                || handler
                    .as_ref()
                    .is_some_and(|(_, b)| stmts_contain(b, pred))
                || finalizer.as_ref().is_some_and(|b| stmts_contain(b, pred))
        }
        Stmt::Switch { disc, cases } => {
            e(disc)
                || cases
                    .iter()
                    .any(|c| c.test.as_ref().is_some_and(&e) || stmts_contain(&c.body, pred))
        }
        Stmt::Labeled { body, .. } | Stmt::With { body, .. } => stmt_contains(body, pred),
        // A (non-arrow) function or class declaration opens its own context.
        _ => false,
    }
}

fn expr_contains(x: &Expr, pred: fn(&Expr) -> bool) -> bool {
    if pred(x) {
        return true;
    }
    let e = |x: &Expr| expr_contains(x, pred);
    match x {
        Expr::Call { callee, args, .. } => e(callee) || call_args_contain(args, pred),
        Expr::New { callee, args } => e(callee) || call_args_contain(args, pred),
        Expr::Unary { arg, .. } | Expr::Update { arg, .. } | Expr::Await(arg) => e(arg),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => e(left) || e(right),
        Expr::Assign { target, value, .. } => e(target) || e(value),
        Expr::Cond { test, cons, alt } => e(test) || e(cons) || e(alt),
        Expr::Member { obj, .. } | Expr::OptionalChain(obj) => e(obj),
        Expr::Index { obj, index, .. } => e(obj) || e(index),
        Expr::Seq(v) => v.iter().any(e),
        Expr::Array(elems) => arr_elems_contain(elems, pred),
        Expr::Yield { arg, .. } => arg.as_deref().is_some_and(e),
        Expr::ImportCall { spec, .. } => e(spec),
        Expr::PrivateIn { obj, .. } => e(obj),
        Expr::TaggedTemplate { tag, subs, .. } => e(tag) || subs.iter().any(e),
        Expr::Object(props) => props.iter().any(|p| match p {
            PropDef::KeyValue { value, .. } => e(value),
            PropDef::Spread(x) => e(x),
            // Methods/getters/setters open their own context.
            _ => false,
        }),
        // `Contains` descends into arrow functions; an ordinary function/class does not.
        Expr::Func(f) if f.is_arrow => {
            f.params.iter().any(|p| p.default.as_ref().is_some_and(&e))
                || stmts_contain(&f.body, pred)
        }
        _ => false,
    }
}

fn stmts_have_super_call(stmts: &[Stmt]) -> bool {
    stmts_contain(
        stmts,
        |e| matches!(e, Expr::Call { callee, .. } if matches!(**callee, Expr::Super)),
    )
}

/// ContainsArguments: an `arguments` identifier reference (arrow-descending, like `Contains`).
fn stmts_have_arguments_ref(stmts: &[Stmt]) -> bool {
    stmts_contain(stmts, |e| matches!(e, Expr::Ident(n) if n == "arguments"))
}

fn call_args_contain(args: &[ArrayElem], pred: fn(&Expr) -> bool) -> bool {
    arr_elems_contain(args, pred)
}

fn arr_elems_contain(elems: &[ArrayElem], pred: fn(&Expr) -> bool) -> bool {
    elems.iter().any(|el| match el {
        ArrayElem::Item(e) | ArrayElem::Spread(e) => expr_contains(e, pred),
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

/// A `static { … }` block may not contain `arguments` or a `super(...)` call (the scan stops at
/// nested ordinary functions and classes).
pub(crate) fn static_block_error(func: &crate::ast::Function) -> Option<&'static str> {
    fi_stmts(&func.body, true)
}

/// A non-constructor method body may not contain a `super(...)` call (only a derived constructor
/// can). Descends into arrow functions (which inherit the method's super context).
/// A non-constructor method may not contain a `super(...)` call in its parameter list *or* its body.
/// Script/global code may not contain a `super(...)` call anywhere outside a class.
pub(crate) fn top_level_super_call_error(body: &[Stmt]) -> Option<&'static str> {
    fi_stmts(body, false)
}

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
            PropDef::Spread(e) | PropDef::Proto(e) => fi_expr(e, args),
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

/// The resolver pair's shared [[AlreadyResolved]] cell (`args[0]`): true (and mark) on first use.
fn promise_mark_already(flag: &Value) -> bool {
    if let Value::Obj(o) = flag {
        let used = o.borrow().props.contains("__called");
        if used {
            return false;
        }
        o.borrow_mut().props.insert(
            "__called",
            crate::value::Property::data(Value::Bool(true), true, false, true),
        );
        return true;
    }
    true
}

fn promise_resolve_native(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    if promise_mark_already(&arg_at(args, 0)) {
        i.resolve_promise(&this, arg_at(args, 1));
    }
    Ok(Value::Undefined)
}

fn promise_reject_native(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    if promise_mark_already(&arg_at(args, 0)) {
        i.reject_promise(&this, arg_at(args, 1));
    }
    Ok(Value::Undefined)
}

fn arg_at(args: &[Value], k: usize) -> Value {
    args.get(k).cloned().unwrap_or(Value::Undefined)
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
    // Radix literals take no sign and may exceed i64, so fold digit-by-digit into an f64.
    let radix = |digits: &str, r: u32| -> f64 {
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut acc = 0f64;
        for c in digits.chars() {
            match c.to_digit(r) {
                Some(d) => acc = acc * r as f64 + d as f64,
                None => return f64::NAN,
            }
        }
        acc
    };
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return radix(hex, 16);
    }
    if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return radix(oct, 8);
    }
    if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return radix(bin, 2);
    }
    // Rust's f64 parser accepts "inf"/"nan" spellings the StrDecimalLiteral grammar doesn't,
    // so validate the shape first.
    if is_decimal_literal(t) {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// StrDecimalLiteral (sans Infinity): `[+-]? (Digits ('.' Digits?)? | '.' Digits) ([eE][+-]?Digits)?`.
fn is_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let fs = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - fs;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let es = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == es {
            return false;
        }
    }
    i == b.len()
}

/// Where an identifier `Reference`'s binding lives, resolved exactly once.
enum RefBase {
    /// A binding in this environment's `vars`.
    Scope(Env),
    /// A `with (obj)` object environment record that has the name.
    With(Value),
    /// A property of the global object.
    Global,
    /// Not found anywhere: GetValue throws ReferenceError; PutValue creates a global (sloppy).
    Unresolvable,
}

/// A member reference's property key. For a computed `base[expr]` the key stays a `Raw` value
/// until the first GetValue/PutValue, so `ToPropertyKey` runs *after* the base's
/// RequireObjectCoercible check (a null base throws before the key's `toString` runs) and only once.
enum RefKey {
    Static(String),
    Raw(Value),
}

/// A resolved reference — computed once so a compound/logical assignment reuses the same base
/// for both GetValue and PutValue (matching spec Reference semantics), rather than re-resolving.
enum Reference {
    Var(RefBase, String),
    /// `base.key` (Static) or `base[expr]` (Raw, coerced lazily).
    Prop(Value, RefKey),
    /// `super.key`: reads through `proto` with `receiver` as the this-value, writes to `receiver`.
    Super {
        proto: Value,
        receiver: Value,
        key: RefKey,
    },
}

impl Interp {
    /// Evaluate `target` to a `Reference` exactly once (its base object/binding location and,
    /// for member/index targets, its property key).
    fn resolve_reference(&mut self, target: &Expr, env: &Env) -> Result<Reference, Abrupt> {
        match target {
            Expr::Ident(name) => {
                let mut cur = Some(env.clone());
                while let Some(s) = cur {
                    let (has_binding, with_obj, parent) = {
                        let b = s.borrow();
                        (
                            b.vars.contains_key(name),
                            b.with_obj.clone(),
                            b.parent.clone(),
                        )
                    };
                    if has_binding {
                        return Ok(Reference::Var(RefBase::Scope(s), name.clone()));
                    }
                    if let Some(obj @ Value::Obj(_)) = &with_obj {
                        if self.with_has_binding(obj, name)? {
                            return Ok(Reference::Var(RefBase::With(obj.clone()), name.clone()));
                        }
                    }
                    cur = parent;
                }
                if self.has_property(&self.global.clone(), name) {
                    return Ok(Reference::Var(RefBase::Global, name.clone()));
                }
                Ok(Reference::Var(RefBase::Unresolvable, name.clone()))
            }
            Expr::Member { obj, prop, .. } => {
                if matches!(**obj, Expr::Super) {
                    let proto = self.super_base(env)?;
                    let receiver = self.get_var("this", env)?;
                    return Ok(Reference::Super {
                        proto,
                        receiver,
                        key: RefKey::Static(prop.clone()),
                    });
                }
                let base = self.eval(obj, env)?;
                let key = if prop.starts_with('#') {
                    self.resolve_private(prop, env)
                } else {
                    prop.clone()
                };
                Ok(Reference::Prop(base, RefKey::Static(key)))
            }
            Expr::Index { obj, index, .. } => {
                if matches!(**obj, Expr::Super) {
                    let proto = self.super_base(env)?;
                    let receiver = self.get_var("this", env)?;
                    // ToPropertyKey is deferred to GetValue/PutValue (after both sides evaluate).
                    let idx = self.eval(index, env)?;
                    return Ok(Reference::Super {
                        proto,
                        receiver,
                        key: RefKey::Raw(idx),
                    });
                }
                // The index expression is evaluated now, but ToPropertyKey is deferred to GetValue.
                let base = self.eval(obj, env)?;
                let idx = self.eval(index, env)?;
                Ok(Reference::Prop(base, RefKey::Raw(idx)))
            }
            // Annex B web compat: a CallExpression target evaluates the call, then throws.
            Expr::Call { .. } => {
                self.eval(target, env)?;
                Err(self.throw("ReferenceError", "invalid assignment target"))
            }
            // Other targets never reach compound/logical assignment (a SyntaxError at parse).
            _ => Err(self.throw("ReferenceError", "invalid assignment target")),
        }
    }

    /// Coerce a reference key via `ToPropertyKey`, caching the result so it runs at most once.
    fn coerce_ref_key(&mut self, key: &mut RefKey) -> Result<String, Abrupt> {
        match key {
            RefKey::Static(s) => Ok(s.clone()),
            RefKey::Raw(v) => {
                let v = v.clone();
                let k = self.to_property_key(&v)?;
                *key = RefKey::Static(k.clone());
                Ok(k)
            }
        }
    }

    /// Coerce a member reference's property key, deferring `ToPropertyKey` until after the base's
    /// RequireObjectCoercible check and caching the result so it runs at most once.
    fn ref_prop_key(&mut self, base: &Value, key: &mut RefKey) -> Result<String, Abrupt> {
        if matches!(key, RefKey::Raw(_)) && matches!(base, Value::Null | Value::Undefined) {
            return Err(self.throw("TypeError", "cannot access property of null or undefined"));
        }
        self.coerce_ref_key(key)
    }

    /// GetValue on a resolved reference.
    fn get_reference(&mut self, r: &mut Reference) -> Result<Value, Abrupt> {
        match r {
            Reference::Var(base, name) => match base {
                RefBase::Scope(s) => {
                    let (initialized, value, import) = {
                        let b = s.borrow();
                        match b.vars.get(name) {
                            Some(bd) => (bd.initialized, bd.value.clone(), bd.import_ref.clone()),
                            None => return self.get_var(name, s),
                        }
                    };
                    if !initialized {
                        return Err(self.throw(
                            "ReferenceError",
                            format!("cannot access '{name}' before initialization"),
                        ));
                    }
                    if let Some((src_env, local)) = import {
                        return self.get_var(&local, &src_env);
                    }
                    Ok(value)
                }
                RefBase::With(obj) => {
                    // GetBindingValue: HasProperty runs again before the Get.
                    let obj = obj.clone();
                    if !self.js_has_property(&obj, name)? {
                        if self.strict {
                            return Err(
                                self.throw("ReferenceError", format!("{name} is not defined"))
                            );
                        }
                        return Ok(Value::Undefined);
                    }
                    self.get_member(&obj, name)
                }
                RefBase::Global => self.get_member(&Value::Obj(self.global.clone()), name),
                RefBase::Unresolvable => {
                    Err(self.throw("ReferenceError", format!("{name} is not defined")))
                }
            },
            Reference::Prop(base, key) => {
                let k = self.ref_prop_key(&base.clone(), key)?;
                if Interp::is_private_key(&k) {
                    return self.get_private_member(&base.clone(), &k);
                }
                self.get_member(base, &k)
            }
            Reference::Super {
                proto,
                receiver,
                key,
            } => {
                // PutValue/GetValue step: ToObject(V.[[Base]]) — a null/undefined super base
                // (a null home-object prototype) throws a TypeError.
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Err(self.throw("TypeError", "cannot read property of null super base"));
                }
                let k = self.coerce_ref_key(key)?;
                let proto = proto.clone();
                let receiver = receiver.clone();
                self.get_member_recv(&proto, &k, receiver)
            }
        }
    }

    /// PutValue on a resolved reference.
    fn put_reference(&mut self, r: &mut Reference, value: Value) -> Result<(), Abrupt> {
        match r {
            Reference::Var(base, name) => match base {
                RefBase::Scope(s) => {
                    let found = {
                        let mut b = s.borrow_mut();
                        match b.vars.get_mut(name) {
                            Some(bd) => {
                                if bd.import_ref.is_some() {
                                    // An import binding is immutable: reads are live through the
                                    // exporting module, but assignment is always a TypeError.
                                    return Err(self.throw(
                                        "TypeError",
                                        format!("assignment to import binding '{name}'"),
                                    ));
                                }
                                // A let/const still in its temporal dead zone: assigning to it
                                // is a ReferenceError (this path is assignment, never the
                                // declaration's own initialization).
                                if !bd.initialized {
                                    return Err(self.throw(
                                        "ReferenceError",
                                        format!("cannot access '{name}' before initialization"),
                                    ));
                                }
                                if !bd.mutable {
                                    // A const (strict immutable) always throws; a named
                                    // function-expression's own name (non-strict immutable)
                                    // is a silent no-op in sloppy code, a throw under strict.
                                    if bd.strict_immutable || self.strict {
                                        return Err(self.throw(
                                            "TypeError",
                                            format!("assignment to constant '{name}'"),
                                        ));
                                    }
                                    return Ok(());
                                }
                                bd.value = value.clone();
                                bd.initialized = true;
                                true
                            }
                            None => false,
                        }
                    };
                    if found {
                        Ok(())
                    } else {
                        self.assign_var(name, value, s)
                    }
                }
                RefBase::With(obj) => {
                    // Object env record SetMutableBinding: HasProperty runs again before the Set
                    // (strict code throws if the property vanished).
                    let obj = obj.clone();
                    if !self.js_has_property(&obj, name)? && self.strict {
                        return Err(self.throw("ReferenceError", format!("{name} is not defined")));
                    }
                    self.set_member(&obj, name, value)
                }
                RefBase::Global => {
                    let g = Value::Obj(self.global.clone());
                    if self.strict && !self.has_property(&self.global.clone(), name) {
                        return Err(self.throw("ReferenceError", format!("{name} is not defined")));
                    }
                    self.set_member(&g, name, value)
                }
                RefBase::Unresolvable => {
                    if self.strict {
                        return Err(self.throw("ReferenceError", format!("{name} is not defined")));
                    }
                    self.set_member(&Value::Obj(self.global.clone()), name, value)
                }
            },
            Reference::Prop(base, key) => {
                let k = self.ref_prop_key(&base.clone(), key)?;
                if Interp::is_private_key(&k) {
                    return self.set_private_member(&base.clone(), &k, value);
                }
                self.set_member(base, &k, value)
            }
            Reference::Super {
                proto,
                receiver,
                key,
            } => {
                if matches!(proto, Value::Null | Value::Undefined) {
                    return Err(self.throw("TypeError", "cannot set property on null super base"));
                }
                let k = self.coerce_ref_key(key)?;
                // OrdinarySet from the super base: accessors on the base's chain win, and the
                // final write lands on the receiver.
                let proto = proto.clone();
                let receiver = receiver.clone();
                self.set_member_recv(&proto, &k, value, receiver)
                    .map(|_| ())
            }
        }
    }
}

/// The source spelling of a private name: strips the per-class-evaluation `\u{1}<serial>` suffix
/// from a runtime key (see `Interp::resolve_private`).
pub(crate) fn private_display(key: &str) -> &str {
    key.split('\u{1}').next().unwrap_or(key)
}

/// StringToBigInt: trimmed decimal / 0x / 0o / 0b text (empty is 0); None when unparsable.
fn string_to_bigint(s: &str) -> Option<crate::bigint::JsBigInt> {
    use crate::bigint::JsBigInt;
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    if t.is_empty() {
        return Some(JsBigInt::zero());
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return JsBigInt::parse_radix(h, 16);
    }
    if let Some(o) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return JsBigInt::parse_radix(o, 8);
    }
    if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return JsBigInt::parse_radix(b, 2);
    }
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = JsBigInt::parse_dec(digits)?;
    Some(if neg { v.neg() } else { v })
}

/// The outcome of evaluating a `return` operand: a pending proper tail call, or a plain value.
enum TailEval {
    Tail(Value, Value, Vec<Value>),
    Val(Value),
}
