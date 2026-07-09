//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_errors(it: &mut Interp) {
    // Base Error first (its prototype's proto is Object.prototype).
    let names = [
        "Error",
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
    ];
    // Create Error.prototype.
    let error_proto = Object::new(Some(it.object_proto.clone()));
    set_builtin(&error_proto, "name", Value::str("Error"));
    set_builtin(&error_proto, "message", Value::str(""));
    it.def_method(&error_proto, "toString", 0, |i, this, _| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "Error.prototype.toString called on a non-object",
            ));
        }
        let name = match ab(i.get_member(&this, "name"))? {
            Value::Undefined => "Error".to_string(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let msg = match ab(i.get_member(&this, "message"))? {
            Value::Undefined => String::new(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        Ok(Value::from_string(if msg.is_empty() {
            name
        } else if name.is_empty() {
            msg
        } else {
            format!("{name}: {msg}")
        }))
    });
    // Error.prototype.stack accessor (error-stack-accessor proposal). The frames are snapshotted
    // at construction (Exotic::Error); the getter formats V8-style: `Name: message` followed by the
    // `\n    at <fn>` lines. get stack: non-object → TypeError; an object without [[ErrorData]] →
    // undefined; an Error instance → the formatted (implementation-defined) trace. The setter
    // shadows this with an own data property, so `err.stack = x` caches as usual.
    let get_stack = it.make_native("get stack", 0, |i, this, _| {
        let frames = match &this {
            Value::Obj(o) => match &o.borrow().exotic {
                Exotic::Error(frames) => frames.clone(),
                _ => return Ok(Value::Undefined),
            },
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Error.prototype.stack getter called on a non-object",
                ))
            }
        };
        let name = match ab(i.get_member(&this, "name"))? {
            Value::Undefined => "Error".to_string(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let msg = match ab(i.get_member(&this, "message"))? {
            Value::Undefined => String::new(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let head = if msg.is_empty() {
            name
        } else if name.is_empty() {
            msg
        } else {
            format!("{name}: {msg}")
        };
        Ok(Value::from_string(format!("{head}{frames}")))
    });
    // set stack: SetterThatIgnoresPrototypeProperties(this, %Error.prototype%, "stack", v).
    let set_stack = it.make_native("set stack", 1, |i, this, a| {
        let o = match &this {
            Value::Obj(o) => o.clone(),
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Error.prototype.stack setter requires an object",
                ))
            }
        };
        // The value must be a String.
        let v = arg(a, 0);
        if !matches!(v, Value::Str(_)) {
            return Err(i.make_error(
                "TypeError",
                "Error.prototype.stack setter requires a string value",
            ));
        }
        // Setting on %Error.prototype% itself throws (it emulates a non-writable home property).
        if Rc::ptr_eq(&o, &i.error_protos["Error"]) {
            return Err(i.make_error("TypeError", "cannot set stack on %Error.prototype%"));
        }
        // [[GetOwnProperty]]("stack") (proxy-aware): does the receiver already have its own stack?
        let has_own = if let Some((t, h)) = proxy_pair(i, &this) {
            !matches!(proxy_gopd_value(i, &t, &h, "stack")?, Value::Undefined)
        } else {
            o.borrow().props.contains("stack")
        };
        if has_own {
            // Set(this, "stack", v) with Throw=true.
            if let Some((t, h)) = proxy_pair(i, &this) {
                // Surface the proxy [[Set]] result: a falsy trap return throws under Throw=true.
                let trap = ab(i.get_member(&h, "set"))?;
                if trap.is_callable() {
                    let res = ab(i.call(
                        trap,
                        h.clone(),
                        &[t.clone(), Value::str("stack"), v.clone(), this.clone()],
                    ))?;
                    if !i.to_boolean(&res) {
                        return Err(
                            i.make_error("TypeError", "proxy set of 'stack' returned false")
                        );
                    }
                } else {
                    ab(i.set_member(&t, "stack", v))?;
                }
            } else {
                assign_set(i, &this, "stack", v)?;
            }
        } else {
            // CreateDataPropertyOrThrow(this, "stack", v).
            let desc = i.new_object();
            set_data(&desc, "value", v);
            set_data(&desc, "writable", Value::Bool(true));
            set_data(&desc, "enumerable", Value::Bool(true));
            set_data(&desc, "configurable", Value::Bool(true));
            let ok = if let Some((t, h)) = proxy_pair(i, &this) {
                ab(proxy_define_property(i, &t, &h, "stack", &Value::Obj(desc)))?
            } else {
                ab(define_own_property(i, &o, "stack", &Value::Obj(desc)))?
            };
            if !ok {
                return Err(i.make_error("TypeError", "cannot define stack"));
            }
        }
        Ok(Value::Undefined)
    });
    error_proto.borrow_mut().props.insert(
        "stack",
        Property::accessor_prop(
            Some(Value::Obj(get_stack)),
            Some(Value::Obj(set_stack)),
            false,
            true,
        ),
    );
    it.error_protos.insert("Error", error_proto.clone());

    let mut error_ctor: Option<Gc> = None;
    for name in names {
        let proto = if name == "Error" {
            error_proto.clone()
        } else {
            let p = Object::new(Some(error_proto.clone()));
            set_builtin(&p, "name", Value::str(name));
            set_builtin(&p, "message", Value::str(""));
            it.error_protos.insert(name, p.clone());
            p
        };
        // A distinct native constructor per error kind (fn pointers can't capture the name).
        let ctor_fn: NativeFn = match name {
            "Error" => |i, _t, a| make_err(i, "Error", a),
            "TypeError" => |i, _t, a| make_err(i, "TypeError", a),
            "RangeError" => |i, _t, a| make_err(i, "RangeError", a),
            "ReferenceError" => |i, _t, a| make_err(i, "ReferenceError", a),
            "SyntaxError" => |i, _t, a| make_err(i, "SyntaxError", a),
            "EvalError" => |i, _t, a| make_err(i, "EvalError", a),
            "URIError" => |i, _t, a| make_err(i, "URIError", a),
            _ => unreachable!(),
        };
        let ctor = it.make_native(name, 1, ctor_fn);
        if name == "Error" {
            it.def_method(&ctor, "isError", 1, |_i, _t, a| {
                Ok(Value::Bool(
                    matches!(arg(a, 0), Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Error(_))),
                ))
            });
            error_ctor = Some(ctor.clone());
        } else if let Some(ec) = &error_ctor {
            // A native error subtype's [[Prototype]] is the Error constructor (it subclasses Error).
            ctor.borrow_mut().proto = Some(ec.clone());
        }
        ctor.borrow_mut().props.insert(
            "prototype",
            Property::data(Value::Obj(proto.clone()), false, false, false),
        );
        proto
            .borrow_mut()
            .props
            .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
        set_builtin(&it.global, name, Value::Obj(ctor));
    }

    // AggregateError(errors, message): an Error subclass carrying an `errors` array.
    let agg_proto = Object::new(Some(error_proto.clone()));
    set_builtin(&agg_proto, "name", Value::str("AggregateError"));
    set_builtin(&agg_proto, "message", Value::str(""));
    it.error_protos.insert("AggregateError", agg_proto.clone());
    let agg_ctor = it.make_native("AggregateError", 2, |i, _t, a| {
        let err = i.make_error("AggregateError", "");
        // OrdinaryCreateFromConstructor: prototype from new.target (cross-realm aware).
        if matches!(i.new_target, Value::Obj(_)) {
            let nt = i.new_target.clone();
            let proto = match i.get_member(&nt, "prototype") {
                Ok(Value::Obj(p)) => Some(p),
                _ => ctor_realm_proto(i, &nt, "AggregateError")?
                    .or_else(|| i.error_protos.get("AggregateError").cloned()),
            };
            if let (Value::Obj(o), Some(p)) = (&err, proto) {
                o.borrow_mut().proto = Some(p);
            }
        }
        // The spec order is: message, then cause, then the (iterated) errors list — last.
        if !matches!(arg(a, 1), Value::Undefined) {
            let s = ab(i.to_string(&arg(a, 1)))?;
            if let Some(o) = err.as_obj() {
                o.borrow_mut()
                    .props
                    .insert("message", Property::builtin(Value::Str(s)));
            }
        }
        install_error_cause(i, &err, a.get(2))?;
        let errors = ab(i.iterate(&arg(a, 0)))?;
        let arr = i.make_array(errors);
        // `errors` is { writable:true, enumerable:false, configurable:true }.
        if let Some(o) = err.as_obj() {
            o.borrow_mut()
                .props
                .insert("errors", Property::data(arr, true, false, true));
        }
        Ok(err)
    });
    if let Some(ec) = &error_ctor {
        agg_ctor.borrow_mut().proto = Some(ec.clone());
    }
    agg_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(agg_proto.clone()), false, false, false),
    );
    agg_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(agg_ctor.clone())),
    );
    set_builtin(&it.global, "AggregateError", Value::Obj(agg_ctor));

    // SuppressedError(error, suppressed, message): an Error subclass carrying `error` and
    // `suppressed` (the error thrown during disposal and the one it superseded).
    let sup_proto = Object::new(Some(error_proto.clone()));
    set_builtin(&sup_proto, "name", Value::str("SuppressedError"));
    set_builtin(&sup_proto, "message", Value::str(""));
    it.error_protos.insert("SuppressedError", sup_proto.clone());
    let sup_ctor = it.make_native("SuppressedError", 3, |i, _t, a| {
        let err = i.make_error("SuppressedError", "");
        // OrdinaryCreateFromConstructor: prototype from new.target (cross-realm aware).
        if matches!(i.new_target, Value::Obj(_)) {
            let nt = i.new_target.clone();
            let proto = match i.get_member(&nt, "prototype") {
                Ok(Value::Obj(p)) => Some(p),
                _ => ctor_realm_proto(i, &nt, "SuppressedError")?
                    .or_else(|| i.error_protos.get("SuppressedError").cloned()),
            };
            if let (Value::Obj(o), Some(p)) = (&err, proto) {
                o.borrow_mut().proto = Some(p);
            }
        }
        if !matches!(arg(a, 2), Value::Undefined) {
            let s = ab(i.to_string(&arg(a, 2)))?;
            if let Some(o) = err.as_obj() {
                o.borrow_mut()
                    .props
                    .insert("message", Property::builtin(Value::Str(s)));
            }
        }
        // `error` and `suppressed` are non-enumerable, writable, configurable data properties.
        if let Some(o) = err.as_obj() {
            o.borrow_mut()
                .props
                .insert("error", Property::builtin(arg(a, 0)));
            o.borrow_mut()
                .props
                .insert("suppressed", Property::builtin(arg(a, 1)));
        }
        install_error_cause(i, &err, a.get(3))?;
        Ok(err)
    });
    sup_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(sup_proto.clone()), false, false, false),
    );
    sup_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(sup_ctor.clone())),
    );
    if let Some(ec) = &error_ctor {
        sup_ctor.borrow_mut().proto = Some(ec.clone());
    }
    set_builtin(&it.global, "SuppressedError", Value::Obj(sup_ctor));
}

fn make_err(i: &mut Interp, kind: &str, args: &[Value]) -> Result<Value, Value> {
    let err = i.make_error(kind, "");
    // OrdinaryCreateFromConstructor: a subclass / Reflect.construct new.target sets the prototype.
    if matches!(i.new_target, Value::Obj(_)) {
        let nt = i.new_target.clone();
        let proto = match i.get_member(&nt, "prototype") {
            Ok(Value::Obj(p)) => Some(p),
            _ => ctor_realm_proto(i, &nt, kind)?.or_else(|| i.error_protos.get(kind).cloned()),
        };
        if let (Value::Obj(o), Some(p)) = (&err, proto) {
            o.borrow_mut().proto = Some(p);
        }
    }
    if let Some(msg) = args.first() {
        if !matches!(msg, Value::Undefined) {
            // ToString(message) may throw (e.g. a Symbol, or a throwing toString) — propagate it.
            let s = ab(i.to_string(msg))?;
            // The own `message` is { writable:true, enumerable:false, configurable:true }.
            if let Some(e) = err.as_obj() {
                e.borrow_mut()
                    .props
                    .insert("message", Property::builtin(Value::Str(s)));
            }
        }
    }
    install_error_cause(i, &err, args.get(1))?;
    Ok(err)
}

/// InstallErrorCause: when `options` is an object with a `cause` property, copy it to a
/// non-enumerable own `cause` data property. Both HasProperty and Get are observable (proxy traps)
/// and propagate their throws.
pub(super) fn install_error_cause(
    i: &mut Interp,
    err: &Value,
    options: Option<&Value>,
) -> Result<(), Value> {
    if let Some(opts @ Value::Obj(_)) = options {
        if ab(i.js_has_property(opts, "cause"))? {
            let cause = ab(i.get_member(opts, "cause"))?;
            if let Some(e) = err.as_obj() {
                e.borrow_mut()
                    .props
                    .insert("cause", Property::data(cause, true, false, true));
            }
        }
    }
    Ok(())
}
