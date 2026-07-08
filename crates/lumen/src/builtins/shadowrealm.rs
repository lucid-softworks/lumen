//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

/// `ShadowRealm`: each instance owns a fully isolated sub-interpreter. `evaluate` runs source in it and
/// only lets primitive completion values cross back (callables are wrapped; objects are a TypeError).
pub(super) fn install_shadow_realm(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("ShadowRealm", proto.clone());
    set_to_string_tag(it, &proto, "ShadowRealm");
    it.def_method(&proto, "evaluate", 1, shadow_evaluate);
    it.def_method(&proto, "importValue", 2, shadow_import_value);

    let ctor = it.make_native("ShadowRealm", 0, |i, _t, _a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "ShadowRealm constructor requires 'new'"));
        }
        let obj = Object::new(i.extra_protos.get("ShadowRealm").cloned());
        let p = Rc::as_ptr(&obj) as usize;
        i.shadow_realms.insert(p, Box::new(Interp::new()));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.global
        .borrow_mut()
        .props
        .insert("ShadowRealm", Property::builtin(Value::Obj(ctor)));
}

fn shadow_evaluate(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this)
        .filter(|p| i.shadow_realms.contains_key(p))
        .ok_or_else(|| {
            i.make_error(
                "TypeError",
                "ShadowRealm.prototype.evaluate called on a non-ShadowRealm",
            )
        })?;
    let src = match arg(a, 0) {
        Value::Str(s) => s.to_string(),
        _ => {
            return Err(i.make_error(
                "TypeError",
                "ShadowRealm.prototype.evaluate expects a string",
            ))
        }
    };
    // A parse failure is a SyntaxError of the *calling* realm (not wrapped).
    let body = match crate::parser::parse_script(&src, false) {
        Ok(b) => b,
        Err(e) => return Err(i.make_error("SyntaxError", e.message)),
    };
    let subptr = &mut **i.shadow_realms.get_mut(&ptr).unwrap() as *mut Interp;
    // SAFETY: pinned Box target; re-entrant evaluation is sequenced by the JS call stack.
    let sub: &mut Interp = unsafe { &mut *subptr };
    let result = {
        // PerformShadowRealmEval: like eval code — `var`s instantiate in the realm's global,
        // lexicals live in a fresh declarative environment per evaluation.
        sub.strict = matches!(
            body.first(),
            Some(crate::ast::Stmt::Expr(crate::ast::Expr::Str(d))) if &**d == "use strict"
        );
        let genv = sub.global_env.clone();
        let scope = crate::interpreter::new_scope(Some(genv.clone()));
        sub.hoist(&body, &genv, &[]);
        sub.declare_block_lexicals(&body, &scope, false);
        let r = sub.run_stmt_list(&body, &scope).map(|v| match v {
            Value::Empty => Value::Undefined,
            other => other,
        });
        sub.drain_microtasks();
        r
    };
    match result {
        // Primitives cross the realm boundary directly; callables wrap; other objects throw.
        Ok(v) => ab(i.make_shadow_result(ptr, v)),
        // An error thrown inside the shadow realm is re-thrown as a TypeError of the calling realm.
        Err(_) => Err(i.make_error(
            "TypeError",
            "ShadowRealm evaluate: the provided source threw an error",
        )),
    }
}

/// ShadowRealm.prototype.importValue: load `specifier` as a module inside the sub-realm and
/// marshal its `exportName` export out (primitive or wrapped callable).
fn shadow_import_value(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    // Brand check, specifier ToString, and exportName validation all throw synchronously.
    let ptr = map_ptr(&this)
        .filter(|p| i.shadow_realms.contains_key(p))
        .ok_or_else(|| {
            i.make_error(
                "TypeError",
                "ShadowRealm.prototype.importValue called on a non-ShadowRealm",
            )
        })?;
    let spec = ab(i.to_string(&arg(a, 0)))?.to_string();
    let name = match arg(a, 1) {
        Value::Str(s) => s.to_string(),
        _ => return Err(i.make_error("TypeError", "importValue exportName must be a string")),
    };
    let promise = i.new_promise();
    let outcome = (|i: &mut Interp| -> Result<Value, Value> {
        let loader = i
            .module_loader
            .clone()
            .ok_or_else(|| i.make_error("TypeError", "no module loader available"))?;
        let base = i.import_base.clone();
        let (key, src) = loader(&spec, &base, None)
            .ok_or_else(|| i.make_error("TypeError", format!("module not found: {spec}")))?;
        let subptr = &mut **i.shadow_realms.get_mut(&ptr).unwrap() as *mut Interp;
        // SAFETY: pinned Box target (see shadow_evaluate).
        let sub: &mut Interp = unsafe { &mut *subptr };
        sub.module_loader = Some(loader);
        sub.import_base = base;
        let loaded = {
            let r = sub.load_module(&key, &src);
            sub.drain_microtasks();
            r
        };
        let ns = sub.module_namespace(&key);
        let value = match (&loaded, ns) {
            (Ok(_), Some(ns)) => sub.get_member(&ns, &name),
            _ => {
                return Err(i.make_error("TypeError", "importValue: module evaluation failed"));
            }
        };
        match value {
            Ok(Value::Undefined) => Err(i.make_error(
                "TypeError",
                format!("importValue: no export named '{name}'"),
            )),
            Ok(v) => ab(i.make_shadow_result(ptr, v)),
            Err(_) => Err(i.make_error("TypeError", "importValue: export access failed")),
        }
    })(i);
    match outcome {
        Ok(v) => i.resolve_promise(&promise, v),
        Err(e) => i.reject_promise(&promise, e),
    }
    Ok(promise)
}
