//! WebAssembly: a from-scratch, std-only engine — binary decoder (`parse`), a bytecode interpreter
//! (`exec`), and the JS `WebAssembly.*` API assembled over native ops in `lib.rs`/`js/wasm.js`.
//! Supports the MVP instruction set plus common post-MVP ops (multi-value, sign-extension,
//! saturating conversions, bulk memory). Not supported: SIMD, threads/atomics, exceptions, GC.

pub mod exec;
pub mod parse;

use std::collections::HashMap;
use std::rc::Rc;

use exec::{eval_const_expr, scan_labels, Compiled, FuncInst, Instance, Val, PAGE_SIZE};
use parse::{FuncType, GlobalType};

pub use parse::{ExportKind, ImportKind, Module};

/// Import values resolved by the op layer (from the JS imports object), in module import order.
#[derive(Default)]
pub struct ResolvedImports {
    /// (host callback id, type) per imported function.
    pub funcs: Vec<(usize, FuncType)>,
    /// Imported linear memory (bytes, max pages), if the module imports one.
    pub memory: Option<(Vec<u8>, Option<u32>)>,
    /// Imported global values (with their declared type), in order.
    pub globals: Vec<(Val, GlobalType)>,
    /// Imported table (funcref slots, max), if any.
    pub table: Option<(Vec<Option<u32>>, Option<u32>)>,
}

/// Link a module against resolved imports into a runnable [`Instance`]: compile bodies, set up the
/// function space (imports first), initialize memory/globals/table, and apply data/elem segments.
pub fn build_instance(module: Rc<Module>, imports: ResolvedImports) -> Result<Instance, String> {
    // Functions: imported (host) first, then defined (compiled).
    let mut funcs: Vec<FuncInst> = Vec::new();
    for (id, ty) in &imports.funcs {
        funcs.push(FuncInst::Host { id: *id, ty: ty.clone() });
    }
    for (i, &type_idx) in module.func_types.iter().enumerate() {
        let ty = module.types[type_idx as usize].clone();
        let body = &module.code[i];
        let labels = scan_labels(&body.code)?;
        funcs.push(FuncInst::Wasm(Rc::new(Compiled {
            ty,
            locals: body.locals.clone(),
            code: body.code.clone(),
            labels,
        })));
    }

    // Memory: imported, or defined by the module (single memory in the MVP).
    let (memory, mem_max_pages) = if let Some((bytes, max)) = imports.memory {
        (bytes, max)
    } else if let Some(l) = module.memories.first() {
        (vec![0u8; l.min as usize * PAGE_SIZE], l.max)
    } else {
        (Vec::new(), Some(0))
    };

    // Globals: imported first, then defined (each init expr sees the globals defined so far).
    let mut globals: Vec<Val> = Vec::new();
    let mut global_types: Vec<GlobalType> = Vec::new();
    for (v, ty) in &imports.globals {
        globals.push(*v);
        global_types.push(*ty);
    }
    for g in &module.globals {
        let v = eval_const_expr(&g.init, &globals)?;
        globals.push(v);
        global_types.push(g.ty);
    }

    // Table: imported, or defined.
    let (table, table_max) = if let Some((slots, max)) = imports.table {
        (slots, max)
    } else if let Some(t) = module.tables.first() {
        (vec![None; t.limits.min as usize], t.limits.max)
    } else {
        (Vec::new(), Some(0))
    };

    let mut inst = Instance {
        module: Rc::clone(&module),
        funcs,
        memory,
        mem_max_pages,
        globals,
        global_types,
        table,
        table_max,
    };

    // Active element segments: write function indices into the table.
    for seg in &module.elems {
        let offset = eval_const_expr(&seg.offset, &inst.globals)?.i32() as usize;
        for (k, &f) in seg.func_indices.iter().enumerate() {
            let slot = offset + k;
            if slot >= inst.table.len() {
                return Err("wasm: element segment out of table bounds".into());
            }
            inst.table[slot] = Some(f);
        }
    }
    // Active data segments: copy bytes into linear memory.
    for seg in &module.data {
        if let Some((_mem, offset_expr)) = &seg.active {
            let offset = eval_const_expr(offset_expr, &inst.globals)?.i32() as u32 as usize;
            let end = offset
                .checked_add(seg.bytes.len())
                .ok_or("wasm: data segment offset overflow")?;
            if end > inst.memory.len() {
                return Err("wasm: data segment out of memory bounds".into());
            }
            inst.memory[offset..end].copy_from_slice(&seg.bytes);
        }
    }

    Ok(inst)
}

/// A map from export name to its (kind, index), for building the JS `instance.exports`.
pub fn export_map(m: &Module) -> HashMap<String, (ExportKind, u32)> {
    m.exports
        .iter()
        .map(|e| (e.name.clone(), (e.kind, e.index)))
        .collect()
}

/// Describe a module's exports as `(name, kind)` for `WebAssembly.Module.exports()`.
pub fn export_descriptors(m: &Module) -> Vec<(String, &'static str)> {
    m.exports
        .iter()
        .map(|e| (e.name.clone(), kind_str(e.kind)))
        .collect()
}

/// Describe a module's imports as `(module, name, kind)` for `WebAssembly.Module.imports()`.
pub fn import_descriptors(m: &Module) -> Vec<(String, String, &'static str)> {
    m.imports
        .iter()
        .map(|i| {
            let kind = match i.kind {
                ImportKind::Func(_) => "function",
                ImportKind::Table(_) => "table",
                ImportKind::Memory(_) => "memory",
                ImportKind::Global(_) => "global",
            };
            (i.module.clone(), i.name.clone(), kind)
        })
        .collect()
}

fn kind_str(kind: ExportKind) -> &'static str {
    match kind {
        ExportKind::Func => "function",
        ExportKind::Table => "table",
        ExportKind::Memory => "memory",
        ExportKind::Global => "global",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoHost;
    impl exec::Host for NoHost {
        fn call_host(&mut self, _id: usize, _a: &[Val], _r: &[parse::ValType]) -> Result<Vec<Val>, String> {
            Err("no imports".into())
        }
    }

    fn instance_of(bytes: &[u8]) -> Instance {
        let module = decode(bytes).expect("decode");
        build_instance(module, ResolvedImports::default()).expect("instantiate")
    }

    fn call(inst: &mut Instance, name: &str, args: Vec<Val>) -> Vec<Val> {
        let (_, idx) = export_map(&inst.module)[name];
        inst.invoke(idx as usize, args, &mut NoHost, 0).expect("invoke")
    }

    // (module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))
    const ADD: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
        0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, // type
        0x03, 0x02, 0x01, 0x00, // func
        0x07, 0x07, 0x01, 0x03, b'a', b'd', b'd', 0x00, 0x00, // export "add"
        0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b, // code
    ];

    // (module (func (export "fac") (param i64) (result i64)
    //   (if (result i64) (i64.eqz (local.get 0))
    //     (then (i64.const 1))
    //     (else (i64.mul (local.get 0) (call 0 (i64.sub (local.get 0) (i64.const 1))))))))
    const FAC: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, //
        0x01, 0x06, 0x01, 0x60, 0x01, 0x7e, 0x01, 0x7e, // type: (i64)->i64
        0x03, 0x02, 0x01, 0x00, //
        0x07, 0x07, 0x01, 0x03, b'f', b'a', b'c', 0x00, 0x00, //
        0x0a, 0x17, 0x01, 0x15, 0x00, // code section, body size 0x15, 0 locals
        0x20, 0x00, 0x50, // local.get 0; i64.eqz
        0x04, 0x7e, // if (result i64)
        0x42, 0x01, // i64.const 1
        0x05, // else
        0x20, 0x00, // local.get 0
        0x20, 0x00, 0x42, 0x01, 0x7d, // local.get 0; i64.const 1; i64.sub
        0x10, 0x00, // call 0
        0x7e, // i64.mul
        0x0b, // end if
        0x0b, // end func
    ];

    #[test]
    fn runs_add() {
        let mut inst = instance_of(ADD);
        let r = call(&mut inst, "add", vec![Val::I32(2), Val::I32(3)]);
        assert_eq!(r[0].i32(), 5);
        let r = call(&mut inst, "add", vec![Val::I32(-10), Val::I32(4)]);
        assert_eq!(r[0].i32(), -6);
    }

    #[test]
    fn runs_recursion_and_control_flow() {
        let mut inst = instance_of(FAC);
        let r = call(&mut inst, "fac", vec![Val::I64(5)]);
        assert_eq!(r[0].i64(), 120);
        let r = call(&mut inst, "fac", vec![Val::I64(10)]);
        assert_eq!(r[0].i64(), 3628800);
    }

    #[test]
    fn validate_accepts_and_rejects() {
        assert!(validate(ADD));
        assert!(!validate(b"not wasm"));
        assert!(!validate(&[0x00, 0x61, 0x73, 0x6d, 0x02, 0, 0, 0])); // bad version
    }
}

pub use parse::{decode, validate};
