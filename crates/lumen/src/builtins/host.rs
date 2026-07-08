//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

/// The test262 `$262` host object. Only the portions lumen can support are provided (`global`,
/// `gc`, `evalScript`, best-effort `detachArrayBuffer`); `agent`/`createRealm` are omitted.
pub(super) fn install_host(it: &mut Interp) {
    let host = make_262(it, None);
    set_builtin(&it.global, "$262", host);
}

/// Build a `$262` host object. `realm_global` is the global of the realm this `$262` controls
/// (None for the main realm, in which case the live `it.global` is used and `evalScript` runs in the
/// current realm).
fn make_262(it: &mut Interp, realm_global: Option<Value>) -> Value {
    let host = Object::new(Some(it.object_proto.clone()));
    let global = realm_global
        .clone()
        .unwrap_or_else(|| Value::Obj(it.global.clone()));
    set_builtin(&host, "global", global.clone());
    it.def_method(&host, "gc", 0, |i, _t, _a| {
        i.gc_collect();
        Ok(Value::Undefined)
    });
    // $262.IsHTMLDDA: callable, returns null with no (or one string "") argument.
    {
        let ddda = it.make_native("IsHTMLDDA", 0, |_i, _t, a| match a.first() {
            None => Ok(Value::Null),
            Some(Value::Str(s)) if s.is_empty() => Ok(Value::Null),
            _ => Ok(Value::Null),
        });
        it.htmldda.push(ddda.clone());
        // An [[IsHTMLDDA]] object is falsy and compares loosely equal to undefined/null; the
        // JIT's inline Not / loose-equality templates assume neither of an ordinary object, and
        // gate on the receiver's `ic_plain` byte — clear it so this object always takes the
        // checked helper.
        ddda.borrow().ic_plain.set(false);
        set_builtin(&host, "IsHTMLDDA", Value::Obj(ddda));
    }
    it.def_method(&host, "createRealm", 0, |i, _t, _a| {
        let g = i.create_realm();
        Ok(make_262(i, Some(g)))
    });
    // evalScript runs in this $262's realm (the main realm's $262 keeps using the current global).
    let rg = realm_global.clone();
    if let Some(rg) = rg {
        set_internal(&host, "__realm_global", rg);
    }
    it.def_method(&host, "evalScript", 1, |i, this, args| {
        let code = match arg(args, 0) {
            Value::Str(s) => s,
            other => return Ok(other),
        };
        let rg = ab(i.get_member(&this, "__realm_global"))?;
        if let Value::Obj(_) = &rg {
            return ab(i.eval_in_realm(&rg, &code));
        }
        let body = crate::parser::parse_script(&code, false)
            .map_err(|e| i.make_error("SyntaxError", e.message))?;
        // A script runs with full GlobalDeclarationInstantiation (clash checks, global-object
        // own properties for var/function declarations).
        i.run_program(&body)
    });
    it.def_method(&host, "detachArrayBuffer", 1, |i, _t, args| {
        if let Value::Obj(o) = arg(args, 0) {
            let p = Rc::as_ptr(&o) as usize;
            // Truly detach: drop the backing store (so views see it as detached) and zero the views.
            i.array_buffers.remove(&p);
            let views: Vec<usize> = i
                .typed_arrays
                .iter()
                .filter(|(_, info)| info.buffer == p)
                .map(|(k, _)| *k)
                .collect();
            for vp in views {
                if let Some(info) = i.typed_arrays.get_mut(&vp) {
                    info.len = 0;
                }
            }
        }
        Ok(Value::Undefined)
    });
    install_agent(it, &host);
    set_builtin(
        &host,
        "AbstractModuleSource",
        make_abstract_module_source(it),
    );
    Value::Obj(host)
}

/// The %AbstractModuleSource% intrinsic exposed as `$262.AbstractModuleSource`: an abstract
/// constructor (throws when called), whose `.prototype` carries the `@@toStringTag` getter used by
/// module-source objects. lumen has no concrete module-source objects, so the getter always yields
/// `undefined`.
fn make_abstract_module_source(it: &mut Interp) -> Value {
    let ctor = it.make_native("AbstractModuleSource", 0, |i, _t, _a| {
        Err(i.make_error(
            "TypeError",
            "Abstract class AbstractModuleSource not directly constructable",
        ))
    });
    let proto = Object::new(Some(it.object_proto.clone()));
    // `@@toStringTag` getter: returns the source's name for a real module-source object, else undefined.
    if let Some(key) = well_known_key(it, "toStringTag") {
        let getter = it.make_native("get [Symbol.toStringTag]", 0, |_i, _t, _a| {
            Ok(Value::Undefined)
        });
        proto.borrow_mut().props.insert(
            key,
            Property {
                value: Value::Undefined,
                get: Some(Value::Obj(getter)),
                set: None,
                accessor: true,
                writable: false,
                enumerable: false,
                configurable: true,
            },
        );
    }
    proto.borrow_mut().props.insert(
        "constructor",
        Property::data(Value::Obj(ctor.clone()), true, false, true),
    );
    // `.prototype` is non-writable, non-enumerable, non-configurable — and the prototype of every
    // source-phase ModuleSource object (see Interp::module_source_of).
    it.extra_protos
        .insert("%AbstractModuleSourceProto%", proto.clone());
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto), false, false, false),
    );
    Value::Obj(ctor)
}

/// Reconstruct a SharedArrayBuffer object in this agent that aliases the global shared block `id`.
fn agent_make_shared(i: &mut Interp, id: u64, len: usize) -> Value {
    let obj = Object::new(i.extra_protos.get("SharedArrayBuffer").cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
    i.array_buffers.insert(p, vec![0u8; len]); // length placeholder; bytes live in the registry
    set_internal(&obj, "__abMaxByteLength", Value::Num(len as f64));
    set_internal(&obj, "__abResizable", Value::Bool(false));
    set_internal(&obj, "__sab_id", Value::Num(id as f64));
    i.shared_buffers.insert(p, id);
    Value::Obj(obj)
}

/// `$262.agent`: the multi-agent harness (real OS threads sharing SharedArrayBuffer memory).
pub(super) fn install_agent(it: &mut Interp, host: &Gc) {
    let agent = Object::new(Some(it.object_proto.clone()));
    it.def_method(&agent, "start", 1, |i, _t, a| {
        let src = ab(i.to_string(&arg(a, 0)))?.to_string();
        // Lazily create the main agent's report channel.
        if i.agent.is_none() {
            let (report_tx, report_rx) = std::sync::mpsc::channel();
            i.agent = Some(Box::new(crate::interpreter::AgentChannels {
                agent_broadcast_txs: Vec::new(),
                report_rx: Some(report_rx),
                report_tx,
                broadcast_rx: None,
            }));
        }
        let (bcast_tx, bcast_rx) = std::sync::mpsc::channel();
        let report_tx = i.agent.as_ref().unwrap().report_tx.clone();
        i.agent.as_mut().unwrap().agent_broadcast_txs.push(bcast_tx);
        std::thread::spawn(move || {
            let mut eng = crate::Engine::new();
            eng.run_as_agent(&src, bcast_rx, report_tx);
        });
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "broadcast", 2, |i, _t, a| {
        let sab = arg(a, 0);
        // Accept a SharedArrayBuffer directly, or a TypedArray view over one.
        let p = match sab.as_obj() {
            Some(o) => {
                let p = Rc::as_ptr(o) as usize;
                if i.shared_buffers.contains_key(&p) {
                    p
                } else if let Some(info) = i.typed_arrays.get(&p) {
                    info.buffer
                } else {
                    return Err(i.make_error("TypeError", "broadcast requires a SharedArrayBuffer"));
                }
            }
            None => return Err(i.make_error("TypeError", "broadcast requires a SharedArrayBuffer")),
        };
        let id = *i
            .shared_buffers
            .get(&p)
            .ok_or_else(|| i.make_error("TypeError", "broadcast requires a SharedArrayBuffer"))?;
        let len = i.array_buffers.get(&p).map(|b| b.len()).unwrap_or(0);
        if let Some(ag) = &i.agent {
            for tx in &ag.agent_broadcast_txs {
                let _ = tx.send((id, len));
            }
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "getReport", 0, |i, _t, _a| {
        // Block briefly for the next report (the producing agent typically reports very soon).
        if let Some(ag) = &i.agent {
            if let Some(rx) = &ag.report_rx {
                return Ok(match rx.recv_timeout(std::time::Duration::from_secs(4)) {
                    Ok(s) => Value::from_string(s),
                    Err(_) => Value::Null,
                });
            }
        }
        Ok(Value::Null)
    });
    it.def_method(&agent, "sleep", 1, |i, _t, a| {
        let ms = ab(i.to_number(&arg(a, 0)))?;
        if ms.is_finite() && ms > 0.0 {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "setTimeout", 2, |i, _t, a| {
        let f = arg(a, 0);
        if !f.is_callable() {
            return Err(i.make_error("TypeError", "setTimeout requires a callable"));
        }
        let ms = ab(i.to_number(&arg(a, 1)))?.max(0.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms as u64);
        i.pending_timers.push((f, deadline));
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "monotonicNow", 0, |_i, _t, _a| {
        Ok(Value::Num(crate::interpreter::monotonic_now_ms()))
    });
    it.def_method(&agent, "receiveBroadcast", 1, |i, _t, a| {
        let cb = arg(a, 0);
        let received = {
            match i.agent.as_ref().and_then(|ag| ag.broadcast_rx.as_ref()) {
                Some(rx) => rx.recv().ok(),
                None => None,
            }
        };
        if let Some((id, len)) = received {
            let sab = agent_make_shared(i, id, len);
            ab(i.call(cb, Value::Undefined, &[sab]))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "report", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?.to_string();
        if let Some(ag) = &i.agent {
            let _ = ag.report_tx.send(s);
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "leaving", 0, |_i, _t, _a| Ok(Value::Undefined));
    set_data(host, "agent", Value::Obj(agent));
}
