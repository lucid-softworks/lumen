//! WebAssembly: a from-scratch, std-only engine — binary decoder (`parse`), a bytecode interpreter
//! (`exec`), and the JS `WebAssembly.*` API assembled over native ops in `lib.rs`/`js/wasm.js`.
//! Supports the MVP instruction set plus common post-MVP ops (multi-value, sign-extension,
//! saturating conversions, bulk memory). Not supported: SIMD, threads/atomics, exceptions, GC.

pub mod exec;
pub mod parse;

pub use parse::{ExportKind, ImportKind, Module};

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
    use std::rc::Rc;

    use super::exec::{Host, Imports, Store, Val};
    use super::*;

    struct NoHost;
    impl Host for NoHost {
        fn call_host(&mut self, _id: usize, _a: &[Val], _r: &[parse::ValType]) -> Result<Vec<Val>, String> {
            Err("no imports".into())
        }
    }

    fn instance_of(bytes: &[u8]) -> (Store, usize) {
        let module = decode(bytes).expect("decode");
        let mut store = Store::default();
        let idx = store.instantiate(module, Imports::default()).expect("instantiate");
        (store, idx)
    }

    fn call(store: &mut Store, inst: usize, name: &str, args: Vec<Val>) -> Vec<Val> {
        let (_, addr) = store.export_addr(inst, name).expect("export");
        store.invoke(addr, args, &mut NoHost, 0).expect("invoke")
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
        let (mut store, inst) = instance_of(ADD);
        let r = call(&mut store, inst, "add", vec![Val::I32(2), Val::I32(3)]);
        assert_eq!(r[0].i32(), 5);
        let r = call(&mut store, inst, "add", vec![Val::I32(-10), Val::I32(4)]);
        assert_eq!(r[0].i32(), -6);
    }

    #[test]
    fn runs_recursion_and_control_flow() {
        let (mut store, inst) = instance_of(FAC);
        let r = call(&mut store, inst, "fac", vec![Val::I64(5)]);
        assert_eq!(r[0].i64(), 120);
        let r = call(&mut store, inst, "fac", vec![Val::I64(10)]);
        assert_eq!(r[0].i64(), 3628800);
    }

    #[test]
    fn two_instances_share_one_store() {
        // The same module instantiated twice in one store yields independent instances with
        // distinct function addresses, both callable.
        let module = decode(ADD).expect("decode");
        let mut store = Store::default();
        let a = store.instantiate(Rc::clone(&module), Imports::default()).unwrap();
        let b = store.instantiate(module, Imports::default()).unwrap();
        assert_ne!(a, b);
        let (_, addr_a) = store.export_addr(a, "add").unwrap();
        let (_, addr_b) = store.export_addr(b, "add").unwrap();
        assert_ne!(addr_a, addr_b);
        assert_eq!(store.invoke(addr_a, vec![Val::I32(1), Val::I32(2)], &mut NoHost, 0).unwrap()[0].i32(), 3);
        assert_eq!(store.invoke(addr_b, vec![Val::I32(40), Val::I32(2)], &mut NoHost, 0).unwrap()[0].i32(), 42);
    }

    #[test]
    fn validate_accepts_and_rejects() {
        assert!(validate(ADD));
        assert!(!validate(b"not wasm"));
        assert!(!validate(&[0x00, 0x61, 0x73, 0x6d, 0x02, 0, 0, 0])); // bad version
    }
}

pub use parse::{decode, validate};
