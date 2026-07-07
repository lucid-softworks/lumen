//! Native ops backing the `WebAssembly.*` JS API (see `js/wasm.js`). All wasm entities live in a
//! single shared [`Store`] in OpState; the JS handles (Instance/Memory/Table/Global) carry integer
//! *store addresses*, so entities can be imported and shared across instances (cross-module
//! linking). Imported JS functions are called back through [`CtxHost`].

use std::collections::HashMap;
use std::rc::Rc;

use lumen_host::{Ctx, Value};

use crate::wasm;
use crate::wasm::exec::{Host, Imports, Store, Val};
use crate::wasm::parse::{Module, ValType};

#[derive(Default)]
pub(crate) struct WasmStore {
    next_module: u32,
    modules: HashMap<u32, Rc<Module>>,
    store: Store,
    /// Imported JS callbacks, indexed by the host id stored in `FuncEntity::Host`.
    host_funcs: Vec<Value>,
}

// ---- value conversion -------------------------------------------------------------------------

fn val_to_js(v: Val) -> Value {
    match v {
        Val::I32(x) => Value::Num(x as f64),
        Val::I64(x) => Value::bigint_from_i64(x),
        Val::F32(x) => Value::Num(x as f64),
        Val::F64(x) => Value::Num(x),
        Val::Ref(_) => Value::Null,
    }
}

fn js_to_val(ctx: &mut Ctx, v: &Value, ty: ValType) -> Val {
    match ty {
        ValType::I32 => Val::I32(ctx.coerce_number(v).unwrap_or(0.0) as i64 as i32),
        ValType::I64 => Val::I64(v.bigint_as_i64().unwrap_or_else(|| ctx.coerce_number(v).unwrap_or(0.0) as i64)),
        ValType::F32 => Val::F32(ctx.coerce_number(v).unwrap_or(0.0) as f32),
        ValType::F64 => Val::F64(ctx.coerce_number(v).unwrap_or(0.0)),
        ValType::FuncRef | ValType::ExternRef => Val::Ref(None),
    }
}

fn valtype_of(s: &str) -> ValType {
    match s {
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        _ => ValType::I32,
    }
}

fn num(a: &[Value], i: usize) -> f64 {
    a.get(i).and_then(Value::as_num_opt).unwrap_or(0.0)
}

// ---- host bridge ------------------------------------------------------------------------------

struct CtxHost<'a> {
    ctx: &'a mut Ctx,
    host_funcs: &'a [Value],
    error: Option<Value>,
}

impl Host for CtxHost<'_> {
    fn call_host(&mut self, id: usize, args: &[Val], results: &[ValType]) -> Result<Vec<Val>, String> {
        let callback = self.host_funcs.get(id).cloned().ok_or("wasm: bad import id")?;
        let js_args: Vec<Value> = args.iter().map(|v| val_to_js(*v)).collect();
        let ret = match self.ctx.invoke(callback, Value::Undefined, &js_args) {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                return Err("wasm: imported function threw".into());
            }
        };
        match results.len() {
            0 => Ok(vec![]),
            1 => Ok(vec![js_to_val(self.ctx, &ret, results[0])]),
            _ => {
                let mut out = Vec::with_capacity(results.len());
                for (i, &ty) in results.iter().enumerate() {
                    let el = self.ctx.get_member(&ret, &i.to_string()).unwrap_or(Value::Undefined);
                    out.push(js_to_val(self.ctx, &el, ty));
                }
                Ok(out)
            }
        }
    }
}

/// Run store function `func_addr`, moving the store (and host callbacks) out of OpState so the
/// interpreter can borrow them mutably while `CtxHost` re-enters JS for imports; then move back.
fn run_func(ctx: &mut Ctx, func_addr: usize, args: Vec<Val>) -> Result<Vec<Val>, Value> {
    let (mut store, host_funcs) = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        (std::mem::take(&mut ws.store), std::mem::take(&mut ws.host_funcs))
    };
    let (result, host_err) = {
        let mut host = CtxHost { ctx: &mut *ctx, host_funcs: &host_funcs, error: None };
        let r = store.invoke(func_addr, args, &mut host, 0);
        (r, host.error)
    };
    {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        ws.store = store;
        ws.host_funcs = host_funcs;
    }
    result.map_err(|msg| host_err.unwrap_or_else(|| ctx.make_error("Error", format!("RuntimeError: {msg}"))))
}

// ---- ops --------------------------------------------------------------------------------------

fn arg_bytes(ctx: &mut Ctx, args: &[Value]) -> Result<Vec<u8>, Value> {
    ctx.typed_array_bytes(args.first().unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "WebAssembly: expected a BufferSource"))
}

pub(crate) fn op_validate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let bytes = arg_bytes(ctx, a)?;
    Ok(Value::Bool(wasm::validate(&bytes)))
}

pub(crate) fn op_compile(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let bytes = arg_bytes(ctx, a)?;
    match wasm::decode(&bytes) {
        Ok(module) => {
            let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
            let id = ws.next_module;
            ws.next_module += 1;
            ws.modules.insert(id, module);
            Ok(Value::Num(id as f64))
        }
        Err(e) => Err(ctx.make_error("Error", format!("CompileError: {e}"))),
    }
}

fn module_of(ctx: &mut Ctx, id: u32) -> Result<Rc<Module>, Value> {
    ctx.host_mut::<WasmStore>()
        .and_then(|s| s.modules.get(&id).cloned())
        .ok_or_else(|| ctx.make_error("Error", "wasm: unknown module"))
}

pub(crate) fn op_module_exports(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let module = module_of(ctx, num(a, 0) as u32)?;
    let items: Vec<Value> = wasm::export_descriptors(&module)
        .into_iter()
        .map(|(name, kind)| {
            let o = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&o, "name", Value::from_string(name));
            let _ = ctx.set_member(&o, "kind", Value::str(kind));
            o
        })
        .collect();
    Ok(ctx.make_array(items))
}

pub(crate) fn op_module_imports(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let module = module_of(ctx, num(a, 0) as u32)?;
    let items: Vec<Value> = wasm::import_descriptors(&module)
        .into_iter()
        .map(|(m, name, kind)| {
            let o = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&o, "module", Value::from_string(m));
            let _ = ctx.set_member(&o, "name", Value::from_string(name));
            let _ = ctx.set_member(&o, "kind", Value::str(kind));
            o
        })
        .collect();
    Ok(ctx.make_array(items))
}

// Standalone entity allocation (for `new WebAssembly.Memory/Table/Global`).
pub(crate) fn op_alloc_memory(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let min = num(a, 0) as usize;
    let max = a.get(1).and_then(Value::as_num_opt).map(|n| n as u32);
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    Ok(Value::Num(ws.store.alloc_memory(min, max) as f64))
}
pub(crate) fn op_alloc_table(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let min = num(a, 0) as usize;
    let max = a.get(1).and_then(Value::as_num_opt).map(|n| n as u32);
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    Ok(Value::Num(ws.store.alloc_table(min, max) as f64))
}
pub(crate) fn op_alloc_global(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let ty = valtype_of(&ctx.coerce_string(a.get(2).unwrap_or(&Value::Undefined))?);
    let val = js_to_val(ctx, a.first().unwrap_or(&Value::Undefined), ty);
    let mutable = matches!(a.get(1), Some(Value::Bool(true)));
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    Ok(Value::Num(ws.store.alloc_global(val, mutable) as f64))
}

/// `(moduleId, resolvedImports) -> { inst, exports: [{name, kind, addr}] }`. `resolvedImports` is a
/// flat array in module-import order: `{fn}` | `{memAddr}` | `{tableAddr}` | `{globalAddr}`.
pub(crate) fn op_instantiate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let module = module_of(ctx, num(a, 0) as u32)?;
    let resolved = a.get(1).cloned().unwrap_or(Value::Undefined);

    // Read the JS import descriptors first (borrows ctx only).
    enum Parsed {
        Func(Value, wasm::parse::FuncType),
        Mem(usize),
        Table(usize),
        Global(usize),
    }
    let mut parsed = Vec::new();
    for (i, imp) in module.imports.iter().enumerate() {
        let entry = ctx.get_member(&resolved, &i.to_string()).unwrap_or(Value::Undefined);
        match &imp.kind {
            wasm::ImportKind::Func(tyidx) => {
                let f = ctx.get_member(&entry, "fn").unwrap_or(Value::Undefined);
                if !f.is_callable() {
                    return Err(ctx.make_error("Error", format!("LinkError: import {}.{} is not a function", imp.module, imp.name)));
                }
                parsed.push(Parsed::Func(f, module.types[*tyidx as usize].clone()));
            }
            wasm::ImportKind::Memory(_) => {
                parsed.push(Parsed::Mem(ctx.get_member(&entry, "memAddr").ok().and_then(|v| v.as_num_opt()).unwrap_or(0.0) as usize));
            }
            wasm::ImportKind::Table(_) => {
                parsed.push(Parsed::Table(ctx.get_member(&entry, "tableAddr").ok().and_then(|v| v.as_num_opt()).unwrap_or(0.0) as usize));
            }
            wasm::ImportKind::Global(_) => {
                parsed.push(Parsed::Global(ctx.get_member(&entry, "globalAddr").ok().and_then(|v| v.as_num_opt()).unwrap_or(0.0) as usize));
            }
        }
    }

    // Build Imports + register host callbacks, then link.
    let inst_result = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        let mut imports = Imports::default();
        for p in parsed {
            match p {
                Parsed::Func(f, ty) => {
                    let id = ws.host_funcs.len();
                    ws.host_funcs.push(f);
                    imports.funcs.push((id, ty));
                }
                Parsed::Mem(addr) => imports.mem_addr = Some(addr),
                Parsed::Table(addr) => imports.table_addr = Some(addr),
                Parsed::Global(addr) => imports.global_addrs.push(addr),
            }
        }
        ws.store.instantiate(Rc::clone(&module), imports)
    };
    let inst_idx = inst_result.map_err(|e| ctx.make_error("Error", format!("LinkError: {e}")))?;

    // Run the start function, if any.
    if let Some(start) = module.start {
        let start_addr = {
            let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
            ws.store.instances[inst_idx].func_addrs[start as usize]
        };
        run_func(ctx, start_addr, Vec::new())?;
    }

    // Export metadata: name, kind, and store address.
    let names: Vec<(String, wasm::ExportKind, usize)> = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        module
            .exports
            .iter()
            .filter_map(|e| ws.store.export_addr(inst_idx, &e.name).map(|(k, addr)| (e.name.clone(), k, addr)))
            .collect()
    };
    let exports: Vec<Value> = names
        .into_iter()
        .map(|(name, kind, addr)| {
            let kind = match kind {
                wasm::ExportKind::Func => "function",
                wasm::ExportKind::Memory => "memory",
                wasm::ExportKind::Global => "global",
                wasm::ExportKind::Table => "table",
            };
            let o = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&o, "name", Value::from_string(name));
            let _ = ctx.set_member(&o, "kind", Value::str(kind));
            let _ = ctx.set_member(&o, "addr", Value::Num(addr as f64));
            o
        })
        .collect();
    let exports = ctx.make_array(exports);

    let result = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&result, "inst", Value::Num(inst_idx as f64));
    let _ = ctx.set_member(&result, "exports", exports);
    Ok(result)
}

/// `(funcAddr, argsArray) -> resultsArray`.
pub(crate) fn op_call(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let func_addr = num(a, 0) as usize;
    let args_arr = a.get(1).cloned().unwrap_or(Value::Undefined);

    let params = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        match ws.store.funcs.get(func_addr) {
            Some(f) => f.ty().params.clone(),
            None => return Err(ctx.make_error("Error", "wasm: bad function address")),
        }
    };
    let mut args = Vec::with_capacity(params.len());
    for (i, &ty) in params.iter().enumerate() {
        let v = ctx.get_member(&args_arr, &i.to_string()).unwrap_or(Value::Undefined);
        args.push(js_to_val(ctx, &v, ty));
    }

    let results = run_func(ctx, func_addr, args)?;
    let js: Vec<Value> = results.into_iter().map(val_to_js).collect();
    Ok(ctx.make_array(js))
}

// ---- memory / table / global accessors (by store address) -------------------------------------

pub(crate) fn op_mem_bytes(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let bytes = ctx
        .host_mut::<WasmStore>()
        .and_then(|ws| ws.store.memories.get(addr))
        .map(|m| m.bytes.clone())
        .ok_or_else(|| ctx.make_error("Error", "wasm: bad memory address"))?;
    ctx.make_uint8array(&bytes)
}

pub(crate) fn op_mem_write(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let offset = num(a, 1) as usize;
    let bytes = ctx
        .typed_array_bytes(a.get(2).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "memWrite expects bytes"))?;
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    match ws.store.memories.get_mut(addr) {
        Some(m) if offset + bytes.len() <= m.bytes.len() => {
            m.bytes[offset..offset + bytes.len()].copy_from_slice(&bytes);
            Ok(Value::Undefined)
        }
        Some(_) => Err(ctx.make_error("RangeError", "memWrite out of bounds")),
        None => Err(ctx.make_error("Error", "wasm: bad memory address")),
    }
}

pub(crate) fn op_mem_grow(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let delta = num(a, 1) as i32;
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    if addr >= ws.store.memories.len() {
        return Err(ctx.make_error("Error", "wasm: bad memory address"));
    }
    Ok(Value::Num(ws.store.mem_grow(addr, delta) as f64))
}

pub(crate) fn op_table_get(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let i = num(a, 1) as usize;
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    let slot = ws.store.tables.get(addr).and_then(|t| t.elems.get(i)).copied();
    match slot {
        Some(Some(faddr)) => Ok(Value::Num(faddr as f64)),
        Some(None) => Ok(Value::Num(-1.0)),
        None => Err(ctx.make_error("RangeError", "table index out of bounds")),
    }
}

pub(crate) fn op_table_set(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let i = num(a, 1) as usize;
    let faddr = a.get(2).and_then(Value::as_num_opt);
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    match ws.store.tables.get_mut(addr).and_then(|t| t.elems.get_mut(i)) {
        Some(slot) => {
            *slot = faddr.filter(|n| *n >= 0.0).map(|n| n as usize);
            Ok(Value::Undefined)
        }
        None => Err(ctx.make_error("RangeError", "table index out of bounds")),
    }
}

pub(crate) fn op_table_size(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    Ok(Value::Num(ws.store.tables.get(addr).map(|t| t.elems.len()).unwrap_or(0) as f64))
}

pub(crate) fn op_global_get(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let v = ctx
        .host_mut::<WasmStore>()
        .and_then(|ws| ws.store.globals.get(addr))
        .map(|g| g.val)
        .ok_or_else(|| ctx.make_error("Error", "wasm: bad global address"))?;
    Ok(val_to_js(v))
}

pub(crate) fn op_global_set(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let addr = num(a, 0) as usize;
    let raw = a.get(1).cloned().unwrap_or(Value::Undefined);
    // Coerce to the global's existing value type.
    let ty = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        match ws.store.globals.get(addr) {
            Some(g) => match g.val {
                Val::I64(_) => ValType::I64,
                Val::F32(_) => ValType::F32,
                Val::F64(_) => ValType::F64,
                _ => ValType::I32,
            },
            None => return Err(ctx.make_error("Error", "wasm: bad global address")),
        }
    };
    let val = js_to_val(ctx, &raw, ty);
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    if let Some(g) = ws.store.globals.get_mut(addr) {
        if !g.mutable {
            return Err(ctx.make_error("TypeError", "cannot set an immutable global"));
        }
        g.val = val;
    }
    Ok(Value::Undefined)
}
