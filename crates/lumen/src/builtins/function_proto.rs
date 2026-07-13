//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_function_proto(it: &mut Interp) {
    let fp = it.function_proto.clone();
    // %Function.prototype% is itself a callable function object that accepts any arguments and
    // returns undefined, with length 0 and the empty name.
    {
        let mut b = fp.borrow_mut();
        b.call = crate::value::Callable::Native(|_i, _this, _args| Ok(Value::Undefined));
        b.props.insert(
            "length",
            Property::data(Value::Num(0.0), false, false, true),
        );
        b.props
            .insert("name", Property::data(Value::str(""), false, false, true));
    }
    it.def_method(&fp, "call", 1, |i, this, args| {
        let this_arg = arg(args, 0);
        let rest = if args.is_empty() { &[][..] } else { &args[1..] };
        ab(i.call(this, this_arg, rest))
    });
    it.def_method(&fp, "apply", 2, |i, this, args| {
        let this_arg = arg(args, 0);
        let list = match arg(args, 1) {
            Value::Undefined | Value::Null => Vec::new(),
            Value::Obj(o) => {
                let len = ab(i.checked_array_len(&o))?;
                let mut v = Vec::with_capacity(len);
                for k in 0..len {
                    v.push(ab(i.get_member(&Value::Obj(o.clone()), &k.to_string()))?);
                }
                v
            }
            _ => return Err(i.make_error("TypeError", "apply: argument list must be array-like")),
        };
        ab(i.call(this, this_arg, &list))
    });
    it.def_method(&fp, "bind", 1, |i, this, args| {
        // Any callable (including a callable proxy) can be bound.
        let target = match &this {
            Value::Obj(o) if this.is_callable() => o.clone(),
            _ => return Err(i.make_error("TypeError", "bind must be called on a function")),
        };
        let bound_this = arg(args, 0);
        let bound_args = if args.is_empty() {
            Vec::new()
        } else {
            args[1..].to_vec()
        };
        // length: 0 unless the target has an OWN `length` (HasOwnProperty — proxy-trapped) whose
        // Get is a Number; +Infinity stays, -Infinity is 0, and a finite value becomes
        // max(0, ToIntegerOrInfinity(len) - boundArgs). name = "bound " + Get(target, "name").
        let has_own_len = has_own_property_trapped(i, &this, "length")?;
        let l: f64 = if has_own_len {
            match ab(i.get_member(&this, "length"))? {
                Value::Num(n) if n == f64::INFINITY => f64::INFINITY,
                Value::Num(n) if n.is_finite() => (n.trunc() - bound_args.len() as f64).max(0.0),
                _ => 0.0,
            }
        } else {
            0.0
        };
        let target_name = ab(i.get_member(&this, "name"))?;
        let name = match &target_name {
            Value::Str(s) => format!("bound {s}"),
            _ => "bound ".to_string(),
        };
        // BoundFunctionCreate: the bound function's [[Prototype]] is the target's (via
        // [[GetPrototypeOf]], so a proxy trap participates) — possibly null.
        let bound_proto = match js_get_prototype_of(i, &this)? {
            Value::Obj(p) => Some(p),
            _ => None,
        };
        let obj = Object::new(bound_proto);
        let target_is_ctor = i.value_is_constructor(&this);
        obj.borrow_mut().call = Callable::bound(target, bound_this, bound_args);
        // A bound function is a constructor exactly when its target is.
        obj.borrow_mut().is_constructor = target_is_ctor;
        obj.borrow_mut()
            .props
            .insert("length", Property::data(Value::Num(l), false, false, true));
        obj.borrow_mut().props.insert(
            "name",
            Property::data(Value::from_string(name), false, false, true),
        );
        Ok(Value::Obj(obj))
    });
    it.def_method(&fp, "toString", 0, |i, this, _args| {
        // Function.prototype.toString requires a callable `this` (a function or a callable proxy).
        if !this.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Function.prototype.toString requires that 'this' be a function",
            ));
        }
        // A user function returns the source text it was parsed from; everything else
        // (natives, bound functions, proxies) renders as a native function carrying its name.
        if let Value::Obj(o) = &this {
            if let Callable::User(user) = &o.borrow().call {
                if let Some(src) = &user.func.source {
                    return Ok(Value::from_string(src.to_string()));
                }
            }
            let name = match o.borrow().props.get("name").map(|p| p.value()) {
                Some(Value::Str(n)) => n.to_string(),
                _ => String::new(),
            };
            // Render the name only when it's a well-formed PropertyName (optionally get/set
            // prefixed, or a computed [Symbol.x] form) — a bound function's "bound f" is not.
            let is_ident = |t: &str| {
                !t.is_empty()
                    && t.chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
                    && t.chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            };
            let renderable = is_ident(&name)
                || (name.starts_with('[') && name.ends_with(']'))
                || name
                    .strip_prefix("get ")
                    .or_else(|| name.strip_prefix("set "))
                    .is_some_and(is_ident);
            let name = if renderable { name.as_str() } else { "" };
            return Ok(Value::from_string(format!(
                "function {name}() {{ [native code] }}"
            )));
        }
        Ok(Value::str("function () { [native code] }"))
    });

    // The %ThrowTypeError% poison pill: a single frozen function (length 0, name "") reused as the
    // `caller`/`arguments` accessors on Function.prototype and `callee` on strict arguments objects.
    let throw_type_error = it.make_native("", 0, |i, _t, _a| {
        Err(i.make_error(
            "TypeError",
            "'caller', 'callee', and 'arguments' may not be accessed on strict mode functions",
        ))
    });
    {
        let mut b = throw_type_error.borrow_mut();
        // length/name are non-configurable, and the function is frozen + non-extensible.
        b.props.insert(
            "length",
            Property::data(Value::Num(0.0), false, false, false),
        );
        b.props
            .insert("name", Property::data(Value::str(""), false, false, false));
        b.extensible = false;
    }
    it.extra_protos
        .insert("%ThrowTypeError%", throw_type_error.clone());
    // Function.prototype.caller / .arguments: accessor properties whose getter AND setter are
    // the single %ThrowTypeError% intrinsic (the spec requires the same function object).
    for name in ["caller", "arguments"] {
        fp.borrow_mut().props.insert(
            name,
            Property::accessor_prop(
                Some(Value::Obj(throw_type_error.clone())),
                Some(Value::Obj(throw_type_error.clone())),
                false,
                true,
            ),
        );
    }

    // The `Function` constructor: `Function(p1, p2, ..., body)` compiles a new function in the
    // global scope. We synthesize source and reuse the in-crate parser (no eval engine needed).
    let ctor = it.make_native("Function", 1, |i, _this, args| {
        create_dynamic_function(i, args, "function")
    });
    // `Function.prototype` is the shared function prototype, so `f instanceof Function` holds for
    // every function (their [[Prototype]] is `function_proto`).
    ctor.borrow_mut().proto = Some(fp.clone());
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(fp.clone()), false, false, false),
    );
    fp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Function", Value::Obj(ctor.clone()));
}

/// The %GeneratorFunction% / %AsyncFunction% / %AsyncGeneratorFunction% intrinsics. Each is a
/// constructor (reachable only as `theFn.constructor`, not a global) whose [[Prototype]] is
/// %Function%, with a `.prototype` object stored in extra_protos so `make_function` installs it as
/// the [[Prototype]] of the corresponding kind of function. Runs after Symbol so @@toStringTag works.
pub(super) fn install_generator_function_ctors(it: &mut Interp) {
    let fp = it.function_proto.clone();
    let function_ctor = match it.global.borrow().props.get("Function").map(|p| p.value()) {
        Some(Value::Obj(o)) => o,
        _ => return,
    };
    for (tag, make_fn) in [
        (
            "GeneratorFunction",
            (|i: &mut Interp, _t: Value, a: &[Value]| create_dynamic_function(i, a, "function*"))
                as NativeFn,
        ),
        (
            "AsyncFunction",
            (|i: &mut Interp, _t: Value, a: &[Value]| {
                create_dynamic_function(i, a, "async function")
            }) as NativeFn,
        ),
        (
            "AsyncGeneratorFunction",
            (|i: &mut Interp, _t: Value, a: &[Value]| {
                create_dynamic_function(i, a, "async function*")
            }) as NativeFn,
        ),
    ] {
        // The constructor's `.prototype` object; its [[Prototype]] is %Function.prototype%.
        let kind_proto = Object::new(Some(fp.clone()));
        set_to_string_tag(it, &kind_proto, tag);
        let key: &'static str = Box::leak(format!("%{tag}.prototype%").into_boxed_str());
        it.extra_protos.insert(key, kind_proto.clone());
        let kind_ctor = it.make_native(tag, 1, make_fn);
        kind_ctor.borrow_mut().proto = Some(function_ctor.clone()); // [[Prototype]] is %Function%
        kind_ctor.borrow_mut().props.insert(
            "prototype",
            Property::data(Value::Obj(kind_proto.clone()), false, false, false),
        );
        kind_proto.borrow_mut().props.insert(
            "constructor",
            Property::data(Value::Obj(kind_ctor), false, false, true),
        );
    }
}

/// Shared CreateDynamicFunction for the Function/Generator/Async/AsyncGenerator constructors:
/// synthesize `<prefix> anonymous(<params>) { <body> }`, parse it, and build the function object.
fn create_dynamic_function(i: &mut Interp, args: &[Value], prefix: &str) -> Result<Value, Value> {
    let (params, body) = if args.is_empty() {
        (String::new(), String::new())
    } else {
        // The parameters are stringified left to right BEFORE the body.
        let mut ps = Vec::new();
        for a in &args[..args.len() - 1] {
            ps.push(ab(i.to_string(a))?.to_string());
        }
        let body = ab(i.to_string(args.last().unwrap()))?.to_string();
        (ps.join(","), body)
    };
    // The parameter text must parse as FormalParameters ON ITS OWN — a "/*" or "//" in it must
    // not be able to swallow the synthesized ") {" (spec: ParseText(P, FormalParameters)).
    if !params.is_empty() {
        let probe = format!("{prefix} __check({params}\n) {{}}");
        crate::parser::parse_script(&probe, false)
            .map_err(|e| i.make_error("SyntaxError", e.message))?;
    }
    let src = format!("{prefix} anonymous({params}\n) {{\n{body}\n}}");
    let program = crate::parser::parse_script(&src, false)
        .map_err(|e| i.make_error("SyntaxError", e.message))?;
    match program.into_iter().next() {
        Some(crate::ast::Stmt::FuncDecl(f)) => {
            let env = i.global_env.clone();
            let func = i.make_function(f, env);
            // GetPrototypeFromConstructor(new.target, ...): a cross-realm `new other.Function()`
            // gets the other realm's prototype as its [[Prototype]] — including the *fallback*
            // when newTarget.prototype isn't an object (resolved in newTarget's realm).
            let nt = i.new_target.clone();
            if matches!(nt, Value::Obj(_)) {
                let kind_key = match prefix {
                    "function*" => "%GeneratorFunction.prototype%",
                    "async function" => "%AsyncFunction.prototype%",
                    "async function*" => "%AsyncGeneratorFunction.prototype%",
                    _ => "Function",
                };
                match ab(i.get_member(&nt, "prototype"))? {
                    Value::Obj(p) => {
                        if let Value::Obj(fo) = &func {
                            fo.borrow_mut().proto = Some(p);
                        }
                    }
                    _ => {
                        if let Some(p) = ctor_realm_proto(i, &nt, kind_key)? {
                            if let Value::Obj(fo) = &func {
                                fo.borrow_mut().proto = Some(p);
                            }
                        }
                    }
                }
            }
            Ok(func)
        }
        _ => Err(i.make_error("SyntaxError", "Function constructor: invalid body")),
    }
}

// ---------------------------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------------------------
