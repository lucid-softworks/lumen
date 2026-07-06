//! Native ops backing the `WebAssembly.*` JS API (see `js/wasm.js`). Modules and instances live in
//! a [`WasmStore`] in OpState; the JS glue holds only integer ids. Imported JS functions are called
//! back through [`CtxHost`], which bridges the interpreter's host trait to `ctx.invoke`.

use std::collections::HashMap;
use std::rc::Rc;

use lumen_host::{Ctx, Value};

use crate::wasm;
use crate::wasm::exec::{Host, Instance, Val};
use crate::wasm::parse::{Module, ValType};

#[derive(Default)]
pub(crate) struct WasmStore {
    next: u32,
    modules: HashMap<u32, Rc<Module>>,
    instances: HashMap<u32, InstanceEntry>,
}

struct InstanceEntry {
    instance: Instance,
    /// JS callbacks for imported functions, indexed by host id.
    imports: Vec<Value>,
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
        ValType::I64 => Val::I64(
            v.bigint_as_i64()
                .unwrap_or_else(|| ctx.coerce_number(v).unwrap_or(0.0) as i64),
        ),
        ValType::F32 => Val::F32(ctx.coerce_number(v).unwrap_or(0.0) as f32),
        ValType::F64 => Val::F64(ctx.coerce_number(v).unwrap_or(0.0)),
        ValType::FuncRef | ValType::ExternRef => Val::Ref(None),
    }
}

// ---- host bridge ------------------------------------------------------------------------------

struct CtxHost<'a> {
    ctx: &'a mut Ctx,
    imports: &'a [Value],
    /// A JS exception thrown by an imported function, surfaced past the interpreter's String errors.
    error: Option<Value>,
}

impl Host for CtxHost<'_> {
    fn call_host(&mut self, id: usize, args: &[Val], results: &[ValType]) -> Result<Vec<Val>, String> {
        let callback = self.imports.get(id).cloned().ok_or("wasm: bad import id")?;
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
                // Multi-value: the JS function returns an array.
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

/// Run an instance function (taking the instance out of the store so the interpreter can borrow it
/// mutably while `CtxHost` re-enters JS for imports), then put it back.
fn run_func(ctx: &mut Ctx, inst_id: u32, func_idx: usize, args: Vec<Val>) -> Result<Vec<Val>, Value> {
    let entry = ctx
        .host_mut::<WasmStore>()
        .and_then(|s| s.instances.remove(&inst_id))
        .ok_or_else(|| ctx.make_error("Error", "wasm: unknown instance"))?;
    let InstanceEntry { mut instance, imports } = entry;

    let (result, host_err) = {
        let mut host = CtxHost { ctx: &mut *ctx, imports: &imports, error: None };
        let r = instance.invoke(func_idx, args, &mut host, 0);
        (r, host.error)
    };

    ctx.host_mut::<WasmStore>()
        .expect("store")
        .instances
        .insert(inst_id, InstanceEntry { instance, imports });

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

/// `(bytes) -> moduleId`. Errors carry a `CompileError:` prefix the JS glue turns into a
/// `WebAssembly.CompileError`.
pub(crate) fn op_compile(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let bytes = arg_bytes(ctx, a)?;
    match wasm::decode(&bytes) {
        Ok(module) => {
            let store = ctx.host_mut::<WasmStore>().expect("store");
            let id = store.next;
            store.next += 1;
            store.modules.insert(id, module);
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
    let id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let module = module_of(ctx, id)?;
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
    let id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let module = module_of(ctx, id)?;
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

/// `(moduleId, importsObj) -> { id, exports: [{name, kind, index}] }`.
pub(crate) fn op_instantiate(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let module_id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let imports_obj = a.get(1).cloned().unwrap_or(Value::Undefined);
    let module = module_of(ctx, module_id)?;

    let mut resolved = wasm::ResolvedImports::default();
    let mut import_values: Vec<Value> = Vec::new();
    for imp in &module.imports {
        let ns = ctx.get_member(&imports_obj, &imp.module).unwrap_or(Value::Undefined);
        let val = ctx.get_member(&ns, &imp.name).unwrap_or(Value::Undefined);
        match &imp.kind {
            wasm::ImportKind::Func(tyidx) => {
                if !val.is_callable() {
                    return Err(ctx.make_error("Error", format!("LinkError: import {}.{} is not a function", imp.module, imp.name)));
                }
                let id = import_values.len();
                import_values.push(val);
                resolved.funcs.push((id, module.types[*tyidx as usize].clone()));
            }
            wasm::ImportKind::Global(gt) => {
                // Accept a WebAssembly.Global (has .value) or a bare number.
                let raw = ctx.get_member(&val, "value").ok().filter(|v| !matches!(v, Value::Undefined)).unwrap_or(val);
                resolved.globals.push((js_to_val(ctx, &raw, gt.val), *gt));
            }
            wasm::ImportKind::Memory(_) | wasm::ImportKind::Table(_) => {
                return Err(ctx.make_error("Error", "LinkError: imported memory/table not supported yet (define and export them instead)"));
            }
        }
    }

    let instance = wasm::build_instance(Rc::clone(&module), resolved)
        .map_err(|e| ctx.make_error("Error", format!("LinkError: {e}")))?;

    let inst_id = {
        let store = ctx.host_mut::<WasmStore>().expect("store");
        let id = store.next;
        store.next += 1;
        store.instances.insert(id, InstanceEntry { instance, imports: import_values });
        id
    };

    // Run the start function, if any.
    if let Some(start) = module.start {
        run_func(ctx, inst_id, start as usize, Vec::new())?;
    }

    // Export metadata for the JS side to build `instance.exports`.
    let exports: Vec<Value> = module
        .exports
        .iter()
        .map(|e| {
            let o = Value::Obj(ctx.new_object());
            let kind = match e.kind {
                wasm::ExportKind::Func => "function",
                wasm::ExportKind::Memory => "memory",
                wasm::ExportKind::Global => "global",
                wasm::ExportKind::Table => "table",
            };
            let _ = ctx.set_member(&o, "name", Value::from_string(e.name.clone()));
            let _ = ctx.set_member(&o, "kind", Value::str(kind));
            let _ = ctx.set_member(&o, "index", Value::Num(e.index as f64));
            o
        })
        .collect();
    let exports = ctx.make_array(exports);

    let result = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&result, "id", Value::Num(inst_id as f64));
    let _ = ctx.set_member(&result, "exports", exports);
    Ok(result)
}

/// `(instanceId, funcIndex, argsArray) -> resultsArray`. Coerces args to the function's parameter
/// types and results back to JS.
pub(crate) fn op_call(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let inst_id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let func_idx = a.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as usize;
    let args_arr = a.get(2).cloned().unwrap_or(Value::Undefined);

    // Look up the function's parameter/result types.
    let (params, _results) = {
        let store = ctx.host_mut::<WasmStore>().expect("store");
        let entry = store.instances.get(&inst_id).ok_or(());
        match entry {
            Ok(e) => match e.instance.funcs.get(func_idx) {
                Some(wasm::exec::FuncInst::Wasm(c)) => (c.ty.params.clone(), c.ty.results.clone()),
                Some(wasm::exec::FuncInst::Host { ty, .. }) => (ty.params.clone(), ty.results.clone()),
                None => return Err(ctx.make_error("Error", "wasm: bad function index")),
            },
            Err(_) => return Err(ctx.make_error("Error", "wasm: unknown instance")),
        }
    };

    let mut args = Vec::with_capacity(params.len());
    for (i, &ty) in params.iter().enumerate() {
        let v = ctx.get_member(&args_arr, &i.to_string()).unwrap_or(Value::Undefined);
        args.push(js_to_val(ctx, &v, ty));
    }

    let results = run_func(ctx, inst_id, func_idx, args)?;
    let js: Vec<Value> = results.into_iter().map(val_to_js).collect();
    Ok(ctx.make_array(js))
}

/// `(instanceId) -> Uint8Array` snapshot of the instance's linear memory (backs `Memory.buffer`).
pub(crate) fn op_mem_bytes(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let inst_id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let bytes = ctx
        .host_mut::<WasmStore>()
        .and_then(|s| s.instances.get(&inst_id))
        .map(|e| e.instance.memory.clone())
        .ok_or_else(|| ctx.make_error("Error", "wasm: unknown instance"))?;
    ctx.make_uint8array(&bytes)
}

/// `(instanceId, offset, bytes)` — write into linear memory (for passing data into a module).
pub(crate) fn op_mem_write(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let inst_id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let offset = a.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as usize;
    let bytes = ctx
        .typed_array_bytes(a.get(2).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "memWrite expects bytes"))?;
    let store = ctx.host_mut::<WasmStore>().expect("store");
    let entry = store.instances.get_mut(&inst_id).ok_or(());
    match entry {
        Ok(e) => {
            if offset + bytes.len() > e.instance.memory.len() {
                return Err(ctx.make_error("RangeError", "memWrite out of bounds"));
            }
            e.instance.memory[offset..offset + bytes.len()].copy_from_slice(&bytes);
            Ok(Value::Undefined)
        }
        Err(_) => Err(ctx.make_error("Error", "wasm: unknown instance")),
    }
}

/// `(instanceId, globalIndex) -> value`.
pub(crate) fn op_global_get(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let inst_id = a.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let idx = a.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as usize;
    let v = ctx
        .host_mut::<WasmStore>()
        .and_then(|s| s.instances.get(&inst_id))
        .and_then(|e| e.instance.globals.get(idx).copied())
        .ok_or_else(|| ctx.make_error("Error", "wasm: unknown global"))?;
    Ok(val_to_js(v))
}
