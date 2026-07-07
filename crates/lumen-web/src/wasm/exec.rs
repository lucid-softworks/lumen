//! A bytecode interpreter for the decoded [`Module`]. Stack machine with structured control flow
//! (block/loop/if resolved to jump targets by a one-time label scan). Covers the MVP numeric,
//! comparison, conversion, variable, memory, and call opcodes, plus sign-extension, saturating
//! truncation, and bulk-memory (0xFC) ops. Traps (OOB, divide-by-zero, unreachable, indirect-call
//! type mismatch) surface as `Err(String)` → a JS `WebAssembly.RuntimeError`.

use std::collections::HashMap;
use std::rc::Rc;

use super::parse::{FuncType, Module, ValType};

pub const PAGE_SIZE: usize = 65536;

#[derive(Clone, Copy, Debug)]
pub enum Val {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Ref(Option<u32>), // funcref/externref: a function index or null
}

impl Val {
    pub fn i32(self) -> i32 {
        match self {
            Val::I32(v) => v,
            _ => 0,
        }
    }
    pub fn i64(self) -> i64 {
        match self {
            Val::I64(v) => v,
            _ => 0,
        }
    }
    pub fn f32(self) -> f32 {
        match self {
            Val::F32(v) => v,
            _ => 0.0,
        }
    }
    pub fn f64(self) -> f64 {
        match self {
            Val::F64(v) => v,
            _ => 0.0,
        }
    }
    pub fn default_for(t: ValType) -> Val {
        match t {
            ValType::I32 => Val::I32(0),
            ValType::I64 => Val::I64(0),
            ValType::F32 => Val::F32(0.0),
            ValType::F64 => Val::F64(0.0),
            ValType::FuncRef | ValType::ExternRef => Val::Ref(None),
        }
    }
}

/// A callable in the store: a wasm function (with the index of its defining instance, for context)
/// or an imported host function (by id).
pub enum FuncEntity {
    Wasm { compiled: Rc<Compiled>, instance: usize },
    Host { id: usize, ty: FuncType },
}

impl FuncEntity {
    pub fn ty(&self) -> FuncType {
        match self {
            FuncEntity::Wasm { compiled, .. } => compiled.ty.clone(),
            FuncEntity::Host { ty, .. } => ty.clone(),
        }
    }
}

pub struct Compiled {
    pub ty: FuncType,
    pub locals: Vec<ValType>,
    pub code: Vec<u8>,
    pub labels: HashMap<usize, Label>,
}

#[derive(Clone, Copy)]
pub struct Label {
    pub else_ip: Option<usize>,
    pub end_ip: usize,
}

/// A table entity: funcref slots holding store function addresses (so a table can reference
/// functions from any instance — the basis for imported/shared tables).
pub struct TableEntity {
    pub elems: Vec<Option<usize>>,
    pub max: Option<u32>,
}

pub struct MemEntity {
    pub bytes: Vec<u8>,
    pub max: Option<u32>,
}

pub struct GlobalEntity {
    pub val: Val,
    pub mutable: bool,
}

/// Import values resolved by the op layer, as store addresses (for memory/table/globals — whether
/// they came from another instance or a standalone `new WebAssembly.Memory`) and host ids (for
/// imported JS functions). In module-import order per kind.
#[derive(Default)]
pub struct Imports {
    pub funcs: Vec<(usize, FuncType)>,
    pub mem_addr: Option<usize>,
    pub table_addr: Option<usize>,
    pub global_addrs: Vec<usize>,
}

/// One instantiated module: index-spaces mapping its local indices to store addresses. Entities
/// themselves live in the shared [`Store`], so instances can import and share them.
pub struct Instance {
    pub module: Rc<Module>,
    pub func_addrs: Vec<usize>,
    pub table_addrs: Vec<usize>,
    pub mem_addrs: Vec<usize>,
    pub global_addrs: Vec<usize>,
}

/// The shared store: flat address spaces for every instance's functions, tables, memories, and
/// globals (WebAssembly's Store). Execution and linking go through it.
#[derive(Default)]
pub struct Store {
    pub funcs: Vec<FuncEntity>,
    pub tables: Vec<TableEntity>,
    pub memories: Vec<MemEntity>,
    pub globals: Vec<GlobalEntity>,
    pub instances: Vec<Rc<Instance>>,
}

/// The host bridge: the op layer implements this to call imported JS functions. `results` is the
/// import's declared result types, so the host can coerce the JS return value(s) to wasm values.
pub trait Host {
    fn call_host(&mut self, id: usize, args: &[Val], results: &[ValType]) -> Result<Vec<Val>, String>;
}

// ---- label pre-scan ---------------------------------------------------------------------------

/// Pre-compute the matching `else`/`end` position for every block/loop/if in a function body, so
/// branches are O(1). Requires skipping each instruction's immediates.
pub fn scan_labels(code: &[u8]) -> Result<HashMap<usize, Label>, String> {
    let mut labels = HashMap::new();
    let mut open: Vec<usize> = Vec::new();
    let mut ip = 0;
    while ip < code.len() {
        let op = code[ip];
        let start = ip;
        ip += 1;
        match op {
            0x02 | 0x03 | 0x04 => {
                // block / loop / if — blocktype immediate
                skip_blocktype(code, &mut ip)?;
                open.push(start);
                labels.insert(start, Label { else_ip: None, end_ip: 0 });
            }
            0x05 => {
                // else — belongs to the innermost open `if`
                if let Some(&pos) = open.last() {
                    if let Some(l) = labels.get_mut(&pos) {
                        l.else_ip = Some(ip);
                    }
                }
            }
            0x0b => {
                // end
                if let Some(pos) = open.pop() {
                    if let Some(l) = labels.get_mut(&pos) {
                        l.end_ip = ip;
                    }
                }
            }
            _ => skip_immediates(code, op, &mut ip)?,
        }
    }
    Ok(labels)
}

fn skip_blocktype(code: &[u8], ip: &mut usize) -> Result<(), String> {
    let b = *code.get(*ip).ok_or("wasm: truncated blocktype")?;
    if b == 0x40 || matches!(b, 0x7f | 0x7e | 0x7d | 0x7c | 0x70 | 0x6f) {
        *ip += 1;
    } else {
        // s33 type index (signed LEB)
        read_sleb(code, ip)?;
    }
    Ok(())
}

fn read_uleb(code: &[u8], ip: &mut usize) -> Result<u64, String> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let b = *code.get(*ip).ok_or("wasm: truncated LEB")?;
        *ip += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

fn read_sleb(code: &[u8], ip: &mut usize) -> Result<i64, String> {
    let mut result = 0i64;
    let mut shift = 0;
    loop {
        let b = *code.get(*ip).ok_or("wasm: truncated SLEB")?;
        *ip += 1;
        result |= ((b & 0x7f) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            if shift < 64 && b & 0x40 != 0 {
                result |= -1i64 << shift;
            }
            return Ok(result);
        }
    }
}

/// Advance `ip` past the immediate operands of the instruction with opcode `op`.
fn skip_immediates(code: &[u8], op: u8, ip: &mut usize) -> Result<(), String> {
    match op {
        // no immediates
        0x00 | 0x01 | 0x0f | 0x1a | 0x1b => Ok(()),
        // single LEB immediate (branch depth, call, local/global idx, ref.func)
        0x0c | 0x0d | 0x10 | 0x20 | 0x21 | 0x22 | 0x23 | 0x24 | 0xd2 => {
            read_uleb(code, ip)?;
            Ok(())
        }
        0x0e => {
            // br_table: vec of labels + default
            let n = read_uleb(code, ip)?;
            for _ in 0..=n {
                read_uleb(code, ip)?;
            }
            Ok(())
        }
        0x11 => {
            // call_indirect: type idx + table idx
            read_uleb(code, ip)?;
            read_uleb(code, ip)?;
            Ok(())
        }
        0xd0 => {
            *ip += 1; // ref.null t
            Ok(())
        }
        0x41 => {
            read_sleb(code, ip)?; // i32.const
            Ok(())
        }
        0x42 => {
            read_sleb(code, ip)?; // i64.const
            Ok(())
        }
        0x43 => {
            *ip += 4; // f32.const
            Ok(())
        }
        0x44 => {
            *ip += 8; // f64.const
            Ok(())
        }
        // memory load/store: memarg (align + offset)
        0x28..=0x3e => {
            read_uleb(code, ip)?;
            read_uleb(code, ip)?;
            Ok(())
        }
        0x3f | 0x40 => {
            *ip += 1; // memory.size / memory.grow (reserved byte)
            Ok(())
        }
        0xfc => {
            let sub = read_uleb(code, ip)?;
            match sub {
                8 => {
                    read_uleb(code, ip)?;
                    *ip += 1;
                } // memory.init: dataidx, mem
                9 => {
                    read_uleb(code, ip)?;
                } // data.drop
                10 => {
                    *ip += 2;
                } // memory.copy: two mem indices
                11 => {
                    *ip += 1;
                } // memory.fill: mem
                _ => {} // trunc_sat variants: no immediate
            }
            Ok(())
        }
        // everything else (numeric/comparison/etc.) has no immediate
        _ => Ok(()),
    }
}

// ---- interpreter ------------------------------------------------------------------------------

macro_rules! binop {
    ($stack:expr, $t:ident, $variant:ident, $f:expr) => {{
        let b = $stack.pop().unwrap().$t();
        let a = $stack.pop().unwrap().$t();
        $stack.push(Val::$variant($f(a, b)));
    }};
}
macro_rules! cmpop {
    ($stack:expr, $t:ident, $f:expr) => {{
        let b = $stack.pop().unwrap().$t();
        let a = $stack.pop().unwrap().$t();
        $stack.push(Val::I32(if $f(a, b) { 1 } else { 0 }));
    }};
}

struct Ctrl {
    is_loop: bool,
    end_ip: usize,
    start_ip: usize, // for loops: branch target
    stack_height: usize,
    arity: usize,
}

impl Store {
    /// Call the store function at `func_addr` with `args`; returns its results.
    pub fn invoke(
        &mut self,
        func_addr: usize,
        args: Vec<Val>,
        host: &mut dyn Host,
        depth: usize,
    ) -> Result<Vec<Val>, String> {
        if depth > 1024 {
            return Err("wasm: call stack exhausted".into());
        }
        // Host functions run immediately; wasm functions run below, in their defining instance's
        // context. `inst`/`compiled` are cloned Rcs so the loop can borrow `self` (the store)
        // mutably for entity access without conflicting.
        let (compiled, inst) = match self.funcs.get(func_addr).ok_or("wasm: bad function index")? {
            FuncEntity::Host { id, ty } => {
                let (id, nres) = (*id, ty.results.len());
                let results = ty.results.clone();
                let r = host.call_host(id, &args, &results)?;
                if r.len() != nres {
                    return Err("wasm: host function returned wrong arity".into());
                }
                return Ok(r);
            }
            FuncEntity::Wasm { compiled, instance } => {
                (compiled.clone(), self.instances[*instance].clone())
            }
        };
        let mem_addr = inst.mem_addrs.first().copied();

        // Locals = params (from args) then declared locals (zeroed).
        let mut locals: Vec<Val> = args;
        for &lt in &compiled.locals {
            locals.push(Val::default_for(lt));
        }

        let mut stack: Vec<Val> = Vec::new();
        let mut ctrl: Vec<Ctrl> = Vec::new();
        let code = &compiled.code;
        let labels = &compiled.labels;
        let mut ip = 0;

        loop {
            if ip >= code.len() {
                break;
            }
            let op = code[ip];
            let op_start = ip;
            ip += 1;
            match op {
                0x00 => return Err("wasm: unreachable executed".into()),
                0x01 => {} // nop
                0x02 | 0x03 | 0x04 => {
                    // block / loop / if
                    let (params, results) = block_arity(&inst.module, code, &mut ip)?;
                    let label = labels[&op_start];
                    if op == 0x04 {
                        let cond = stack.pop().unwrap().i32();
                        if cond == 0 {
                            // jump to else (if any) or past end
                            match label.else_ip {
                                Some(e) => ip = e,
                                None => {
                                    ip = label.end_ip;
                                    continue;
                                }
                            }
                        }
                    }
                    ctrl.push(Ctrl {
                        is_loop: op == 0x03,
                        end_ip: label.end_ip,
                        start_ip: ip, // body start (loop branch target)
                        stack_height: stack.len().saturating_sub(params),
                        arity: if op == 0x03 { params } else { results },
                    });
                }
                0x05 => {
                    // else: reached only by falling out of the `then` arm → jump to end
                    if let Some(c) = ctrl.last() {
                        ip = c.end_ip;
                    }
                }
                0x0b => {
                    // end
                    if ctrl.pop().is_none() {
                        break; // function end
                    }
                }
                0x0c => {
                    let l = read_uleb(code, &mut ip)? as usize;
                    do_branch(&mut ctrl, &mut stack, &mut ip, l)?;
                }
                0x0d => {
                    let l = read_uleb(code, &mut ip)? as usize;
                    let cond = stack.pop().unwrap().i32();
                    if cond != 0 {
                        do_branch(&mut ctrl, &mut stack, &mut ip, l)?;
                    }
                }
                0x0e => {
                    let n = read_uleb(code, &mut ip)?;
                    let mut targets = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        targets.push(read_uleb(code, &mut ip)? as usize);
                    }
                    let default = read_uleb(code, &mut ip)? as usize;
                    let idx = stack.pop().unwrap().i32();
                    let l = if (idx as usize) < targets.len() {
                        targets[idx as usize]
                    } else {
                        default
                    };
                    do_branch(&mut ctrl, &mut stack, &mut ip, l)?;
                }
                0x0f => {
                    // return
                    break;
                }
                0x10 => {
                    let f = read_uleb(code, &mut ip)? as usize;
                    let addr = *inst.func_addrs.get(f).ok_or("wasm: bad function index")?;
                    let ty = self.funcs[addr].ty();
                    let mut args = Vec::with_capacity(ty.params.len());
                    for _ in 0..ty.params.len() {
                        args.push(stack.pop().ok_or("wasm: stack underflow on call")?);
                    }
                    args.reverse();
                    let results = self.invoke(addr, args, host, depth + 1)?;
                    stack.extend(results);
                }
                0x11 => {
                    let type_idx = read_uleb(code, &mut ip)? as usize;
                    let table_idx = read_uleb(code, &mut ip)? as usize;
                    let elem = stack.pop().unwrap().i32();
                    let table_addr = *inst.table_addrs.get(table_idx).ok_or("wasm: bad table index")?;
                    // Table slots hold store function addresses, so an imported/shared table can
                    // dispatch to functions from any instance.
                    let addr = self.tables[table_addr]
                        .elems
                        .get(elem as usize)
                        .copied()
                        .ok_or("wasm: undefined element (indirect call)")?
                        .ok_or("wasm: uninitialized table element")?;
                    let expected = inst.module.types.get(type_idx).ok_or("wasm: bad type index")?;
                    let actual = self.funcs[addr].ty();
                    if !same_type(expected, &actual) {
                        return Err("wasm: indirect call type mismatch".into());
                    }
                    let mut args = Vec::with_capacity(actual.params.len());
                    for _ in 0..actual.params.len() {
                        args.push(stack.pop().ok_or("wasm: stack underflow")?);
                    }
                    args.reverse();
                    let results = self.invoke(addr, args, host, depth + 1)?;
                    stack.extend(results);
                }
                0x1a => {
                    stack.pop();
                } // drop
                0x1b => {
                    // select
                    let c = stack.pop().unwrap().i32();
                    let b = stack.pop().unwrap();
                    let a = stack.pop().unwrap();
                    stack.push(if c != 0 { a } else { b });
                }
                0x20 => {
                    let i = read_uleb(code, &mut ip)? as usize;
                    stack.push(*locals.get(i).ok_or("wasm: bad local index")?);
                }
                0x21 => {
                    let i = read_uleb(code, &mut ip)? as usize;
                    let v = stack.pop().ok_or("wasm: stack underflow")?;
                    *locals.get_mut(i).ok_or("wasm: bad local index")? = v;
                }
                0x22 => {
                    let i = read_uleb(code, &mut ip)? as usize;
                    let v = *stack.last().ok_or("wasm: stack underflow")?;
                    *locals.get_mut(i).ok_or("wasm: bad local index")? = v;
                }
                0x23 => {
                    let i = read_uleb(code, &mut ip)? as usize;
                    let addr = *inst.global_addrs.get(i).ok_or("wasm: bad global index")?;
                    stack.push(self.globals[addr].val);
                }
                0x24 => {
                    let i = read_uleb(code, &mut ip)? as usize;
                    let addr = *inst.global_addrs.get(i).ok_or("wasm: bad global index")?;
                    let v = stack.pop().ok_or("wasm: stack underflow")?;
                    self.globals[addr].val = v;
                }
                // memory loads/stores
                0x28..=0x3e => {
                    let ma = mem_addr.ok_or("wasm: no memory")?;
                    self.mem_op(ma, op, code, &mut ip, &mut stack)?;
                }
                0x3f => {
                    ip += 1; // reserved
                    let ma = mem_addr.ok_or("wasm: no memory")?;
                    stack.push(Val::I32((self.memories[ma].bytes.len() / PAGE_SIZE) as i32));
                }
                0x40 => {
                    ip += 1;
                    let ma = mem_addr.ok_or("wasm: no memory")?;
                    let delta = stack.pop().unwrap().i32();
                    stack.push(Val::I32(self.mem_grow(ma, delta)));
                }
                0x41 => stack.push(Val::I32(read_sleb(code, &mut ip)? as i32)),
                0x42 => stack.push(Val::I64(read_sleb(code, &mut ip)?)),
                0x43 => {
                    let bytes: [u8; 4] = code[ip..ip + 4].try_into().unwrap();
                    ip += 4;
                    stack.push(Val::F32(f32::from_le_bytes(bytes)));
                }
                0x44 => {
                    let bytes: [u8; 8] = code[ip..ip + 8].try_into().unwrap();
                    ip += 8;
                    stack.push(Val::F64(f64::from_le_bytes(bytes)));
                }
                0xfc => {
                    self.op_fc(mem_addr, code, &mut ip, &mut stack)?;
                }
                _ => numeric(op, &mut stack)?,
            }
        }

        // Return the top `results` values.
        let nres = compiled.ty.results.len();
        let start = stack.len().saturating_sub(nres);
        Ok(stack.split_off(start))
    }

    /// Grow the memory at store address `mem_addr` by `delta` pages; returns the previous page
    /// count, or -1 on failure.
    pub fn mem_grow(&mut self, mem_addr: usize, delta: i32) -> i32 {
        if delta < 0 {
            return -1;
        }
        let mem = &mut self.memories[mem_addr];
        let old_pages = (mem.bytes.len() / PAGE_SIZE) as u32;
        let new_pages = old_pages.saturating_add(delta as u32);
        if let Some(max) = mem.max {
            if new_pages > max {
                return -1;
            }
        }
        if new_pages > 65536 {
            return -1;
        }
        mem.bytes.resize(new_pages as usize * PAGE_SIZE, 0);
        old_pages as i32
    }

    pub fn alloc_memory(&mut self, min_pages: usize, max: Option<u32>) -> usize {
        self.memories.push(MemEntity { bytes: vec![0u8; min_pages * PAGE_SIZE], max });
        self.memories.len() - 1
    }
    pub fn alloc_table(&mut self, min: usize, max: Option<u32>) -> usize {
        self.tables.push(TableEntity { elems: vec![None; min], max });
        self.tables.len() - 1
    }
    pub fn alloc_global(&mut self, val: Val, mutable: bool) -> usize {
        self.globals.push(GlobalEntity { val, mutable });
        self.globals.len() - 1
    }

    /// Link `module` against resolved `imports` into a new instance; returns its index in
    /// `self.instances`. Allocates the module's defined functions/memory/tables/globals into the
    /// store, references imported entities by address, and runs element/data segments.
    pub fn instantiate(&mut self, module: Rc<Module>, imports: Imports) -> Result<usize, String> {
        let inst_idx = self.instances.len();
        let mut func_addrs = Vec::new();
        let mut table_addrs = Vec::new();
        let mut mem_addrs = Vec::new();
        let mut global_addrs = Vec::new();

        // Imports first (they occupy the low indices of each space).
        let mut host_funcs = imports.funcs.into_iter();
        let mut imp_globals = imports.global_addrs.into_iter();
        for imp in &module.imports {
            match &imp.kind {
                crate::wasm::ImportKind::Func(_) => {
                    let (id, ty) = host_funcs.next().ok_or("wasm: missing function import")?;
                    self.funcs.push(FuncEntity::Host { id, ty });
                    func_addrs.push(self.funcs.len() - 1);
                }
                crate::wasm::ImportKind::Table(_) => {
                    table_addrs.push(imports.table_addr.ok_or("wasm: missing table import")?);
                }
                crate::wasm::ImportKind::Memory(_) => {
                    mem_addrs.push(imports.mem_addr.ok_or("wasm: missing memory import")?);
                }
                crate::wasm::ImportKind::Global(_) => {
                    global_addrs.push(imp_globals.next().ok_or("wasm: missing global import")?);
                }
            }
        }

        // Defined functions.
        for (i, &type_idx) in module.func_types.iter().enumerate() {
            let ty = module.types[type_idx as usize].clone();
            let body = &module.code[i];
            let labels = scan_labels(&body.code)?;
            self.funcs.push(FuncEntity::Wasm {
                compiled: Rc::new(Compiled { ty, locals: body.locals.clone(), code: body.code.clone(), labels }),
                instance: inst_idx,
            });
            func_addrs.push(self.funcs.len() - 1);
        }
        // Defined memory (single, MVP) and tables.
        if let Some(l) = module.memories.first() {
            let a = self.alloc_memory(l.min as usize, l.max);
            mem_addrs.push(a);
        }
        for t in &module.tables {
            let a = self.alloc_table(t.limits.min as usize, t.limits.max);
            table_addrs.push(a);
        }
        // Defined globals: each init expr sees the globals resolved so far (by value).
        for g in &module.globals {
            let seen: Vec<Val> = global_addrs.iter().map(|&a| self.globals[a].val).collect();
            let v = eval_const_expr(&g.init, &seen)?;
            let a = self.alloc_global(v, g.ty.mutable);
            global_addrs.push(a);
        }

        let inst = Rc::new(Instance {
            module: Rc::clone(&module),
            func_addrs,
            table_addrs,
            mem_addrs,
            global_addrs,
        });
        self.instances.push(Rc::clone(&inst));
        let seen_globals: Vec<Val> = inst.global_addrs.iter().map(|&a| self.globals[a].val).collect();

        // Active element segments: local function indices → store func addresses.
        for seg in &module.elems {
            let offset = eval_const_expr(&seg.offset, &seen_globals)?.i32() as usize;
            let table_addr = *inst
                .table_addrs
                .get(seg.table as usize)
                .ok_or("wasm: element segment references missing table")?;
            for (k, &f) in seg.func_indices.iter().enumerate() {
                let slot = offset + k;
                let faddr = *inst.func_addrs.get(f as usize).ok_or("wasm: elem func index out of range")?;
                if slot >= self.tables[table_addr].elems.len() {
                    return Err("wasm: element segment out of table bounds".into());
                }
                self.tables[table_addr].elems[slot] = Some(faddr);
            }
        }
        // Active data segments.
        for seg in &module.data {
            if let Some((_mem, offset_expr)) = &seg.active {
                let offset = eval_const_expr(offset_expr, &seen_globals)?.i32() as u32 as usize;
                let mem_addr = *inst.mem_addrs.first().ok_or("wasm: data segment but no memory")?;
                let mem = &mut self.memories[mem_addr].bytes;
                let end = offset.checked_add(seg.bytes.len()).ok_or("wasm: data offset overflow")?;
                if end > mem.len() {
                    return Err("wasm: data segment out of memory bounds".into());
                }
                mem[offset..end].copy_from_slice(&seg.bytes);
            }
        }
        Ok(inst_idx)
    }

    /// The store address of instance `inst_idx`'s export named `name`, plus its kind.
    pub fn export_addr(&self, inst_idx: usize, name: &str) -> Option<(crate::wasm::ExportKind, usize)> {
        let inst = self.instances.get(inst_idx)?;
        let e = inst.module.exports.iter().find(|e| e.name == name)?;
        let addr = match e.kind {
            crate::wasm::ExportKind::Func => *inst.func_addrs.get(e.index as usize)?,
            crate::wasm::ExportKind::Memory => *inst.mem_addrs.get(e.index as usize)?,
            crate::wasm::ExportKind::Table => *inst.table_addrs.get(e.index as usize)?,
            crate::wasm::ExportKind::Global => *inst.global_addrs.get(e.index as usize)?,
        };
        Some((e.kind, addr))
    }

    fn mem_effective_addr(
        mem_len: usize,
        code: &[u8],
        ip: &mut usize,
        stack: &mut Vec<Val>,
        size: usize,
    ) -> Result<usize, String> {
        let _align = read_uleb(code, ip)?;
        let offset = read_uleb(code, ip)? as usize;
        let base = stack.pop().ok_or("wasm: stack underflow")?.i32() as u32 as usize;
        let addr = base.checked_add(offset).ok_or("wasm: memory address overflow")?;
        if addr + size > mem_len {
            return Err("wasm: out of bounds memory access".into());
        }
        Ok(addr)
    }

    fn mem_op(&mut self, mem_addr: usize, op: u8, code: &[u8], ip: &mut usize, stack: &mut Vec<Val>) -> Result<(), String> {
        let mem = &mut self.memories[mem_addr].bytes;
        macro_rules! load {
            ($size:expr, $conv:expr) => {{
                let a = Self::mem_effective_addr(mem.len(), code, ip, stack, $size)?;
                let bytes = &mem[a..a + $size];
                stack.push($conv(bytes));
            }};
        }
        macro_rules! store {
            ($t:ident, $size:expr, $bytes:expr) => {{
                let v = stack.pop().ok_or("wasm: stack underflow")?.$t();
                let a = Self::mem_effective_addr(mem.len(), code, ip, stack, $size)?;
                let b = $bytes(v);
                mem[a..a + $size].copy_from_slice(&b);
            }};
        }
        match op {
            0x28 => load!(4, |b: &[u8]| Val::I32(i32::from_le_bytes(b.try_into().unwrap()))),
            0x29 => load!(8, |b: &[u8]| Val::I64(i64::from_le_bytes(b.try_into().unwrap()))),
            0x2a => load!(4, |b: &[u8]| Val::F32(f32::from_le_bytes(b.try_into().unwrap()))),
            0x2b => load!(8, |b: &[u8]| Val::F64(f64::from_le_bytes(b.try_into().unwrap()))),
            0x2c => load!(1, |b: &[u8]| Val::I32(b[0] as i8 as i32)),
            0x2d => load!(1, |b: &[u8]| Val::I32(b[0] as i32)),
            0x2e => load!(2, |b: &[u8]| Val::I32(i16::from_le_bytes(b.try_into().unwrap()) as i32)),
            0x2f => load!(2, |b: &[u8]| Val::I32(u16::from_le_bytes(b.try_into().unwrap()) as i32)),
            0x30 => load!(1, |b: &[u8]| Val::I64(b[0] as i8 as i64)),
            0x31 => load!(1, |b: &[u8]| Val::I64(b[0] as i64)),
            0x32 => load!(2, |b: &[u8]| Val::I64(i16::from_le_bytes(b.try_into().unwrap()) as i64)),
            0x33 => load!(2, |b: &[u8]| Val::I64(u16::from_le_bytes(b.try_into().unwrap()) as i64)),
            0x34 => load!(4, |b: &[u8]| Val::I64(i32::from_le_bytes(b.try_into().unwrap()) as i64)),
            0x35 => load!(4, |b: &[u8]| Val::I64(u32::from_le_bytes(b.try_into().unwrap()) as i64)),
            0x36 => store!(i32, 4, |v: i32| v.to_le_bytes()),
            0x37 => store!(i64, 8, |v: i64| v.to_le_bytes()),
            0x38 => store!(f32, 4, |v: f32| v.to_le_bytes()),
            0x39 => store!(f64, 8, |v: f64| v.to_le_bytes()),
            0x3a => store!(i32, 1, |v: i32| [(v as u8)]),
            0x3b => store!(i32, 2, |v: i32| (v as u16).to_le_bytes()),
            0x3c => store!(i64, 1, |v: i64| [(v as u8)]),
            0x3d => store!(i64, 2, |v: i64| (v as u16).to_le_bytes()),
            0x3e => store!(i64, 4, |v: i64| (v as u32).to_le_bytes()),
            _ => return Err("wasm: bad memory opcode".into()),
        }
        Ok(())
    }

    fn op_fc(&mut self, mem_addr: Option<usize>, code: &[u8], ip: &mut usize, stack: &mut Vec<Val>) -> Result<(), String> {
        let sub = read_uleb(code, ip)?;
        match sub {
            // saturating truncation
            0 => {
                let v = stack.pop().unwrap().f32();
                stack.push(Val::I32(sat_i32(v as f64)));
            }
            1 => {
                let v = stack.pop().unwrap().f32();
                stack.push(Val::I32(sat_u32(v as f64) as i32));
            }
            2 => {
                let v = stack.pop().unwrap().f64();
                stack.push(Val::I32(sat_i32(v)));
            }
            3 => {
                let v = stack.pop().unwrap().f64();
                stack.push(Val::I32(sat_u32(v) as i32));
            }
            4 => {
                let v = stack.pop().unwrap().f32();
                stack.push(Val::I64(sat_i64(v as f64)));
            }
            5 => {
                let v = stack.pop().unwrap().f32();
                stack.push(Val::I64(sat_u64(v as f64) as i64));
            }
            6 => {
                let v = stack.pop().unwrap().f64();
                stack.push(Val::I64(sat_i64(v)));
            }
            7 => {
                let v = stack.pop().unwrap().f64();
                stack.push(Val::I64(sat_u64(v) as i64));
            }
            10 => {
                // memory.copy
                *ip += 2;
                let mem = &mut self.memories[mem_addr.ok_or("wasm: no memory")?].bytes;
                let n = stack.pop().unwrap().i32() as usize;
                let src = stack.pop().unwrap().i32() as u32 as usize;
                let dst = stack.pop().unwrap().i32() as u32 as usize;
                if src + n > mem.len() || dst + n > mem.len() {
                    return Err("wasm: out of bounds memory.copy".into());
                }
                mem.copy_within(src..src + n, dst);
            }
            11 => {
                // memory.fill
                *ip += 1;
                let mem = &mut self.memories[mem_addr.ok_or("wasm: no memory")?].bytes;
                let n = stack.pop().unwrap().i32() as usize;
                let val = stack.pop().unwrap().i32() as u8;
                let dst = stack.pop().unwrap().i32() as u32 as usize;
                if dst + n > mem.len() {
                    return Err("wasm: out of bounds memory.fill".into());
                }
                for b in &mut mem[dst..dst + n] {
                    *b = val;
                }
            }
            other => return Err(format!("wasm: unsupported 0xfc op {other}")),
        }
        Ok(())
    }
}

fn do_branch(ctrl: &mut Vec<Ctrl>, stack: &mut Vec<Val>, ip: &mut usize, l: usize) -> Result<(), String> {
    if l >= ctrl.len() {
        return Err("wasm: branch depth out of range".into());
    }
    let target_idx = ctrl.len() - 1 - l;
    let arity = ctrl[target_idx].arity;
    let height = ctrl[target_idx].stack_height;
    let is_loop = ctrl[target_idx].is_loop;
    let target_ip = if is_loop {
        ctrl[target_idx].start_ip
    } else {
        ctrl[target_idx].end_ip
    };
    // Keep the top `arity` values, drop the rest down to the label's base height.
    let kept: Vec<Val> = stack.split_off(stack.len() - arity);
    stack.truncate(height);
    stack.extend(kept);
    // Popping to (and including) the target frame; a loop keeps its own frame.
    ctrl.truncate(if is_loop { target_idx + 1 } else { target_idx });
    *ip = target_ip;
    Ok(())
}

/// Parse a block's type immediate and return (param_count, result_count).
fn block_arity(module: &Module, code: &[u8], ip: &mut usize) -> Result<(usize, usize), String> {
    let b = code[*ip];
    if b == 0x40 {
        *ip += 1;
        Ok((0, 0))
    } else if matches!(b, 0x7f | 0x7e | 0x7d | 0x7c | 0x70 | 0x6f) {
        *ip += 1;
        Ok((0, 1))
    } else {
        let idx = read_sleb(code, ip)? as usize;
        let ty = module.types.get(idx).ok_or("wasm: bad block type index")?;
        Ok((ty.params.len(), ty.results.len()))
    }
}

fn same_type(a: &FuncType, b: &FuncType) -> bool {
    a.params == b.params && a.results == b.results
}

fn sat_i32(v: f64) -> i32 {
    if v.is_nan() {
        0
    } else if v <= i32::MIN as f64 {
        i32::MIN
    } else if v >= i32::MAX as f64 {
        i32::MAX
    } else {
        v as i32
    }
}
fn sat_u32(v: f64) -> u32 {
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= u32::MAX as f64 {
        u32::MAX
    } else {
        v as u32
    }
}
fn sat_i64(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else if v <= i64::MIN as f64 {
        i64::MIN
    } else if v >= i64::MAX as f64 {
        i64::MAX
    } else {
        v as i64
    }
}
fn sat_u64(v: f64) -> u64 {
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= u64::MAX as f64 {
        u64::MAX
    } else {
        v as u64
    }
}

/// Numeric, comparison, and conversion opcodes (no immediates, no memory/control).
fn numeric(op: u8, stack: &mut Vec<Val>) -> Result<(), String> {
    match op {
        // i32 comparisons
        0x45 => {
            let a = stack.pop().unwrap().i32();
            stack.push(Val::I32((a == 0) as i32));
        }
        0x46 => cmpop!(stack, i32, |a, b| a == b),
        0x47 => cmpop!(stack, i32, |a, b| a != b),
        0x48 => cmpop!(stack, i32, |a, b| a < b),
        0x49 => cmpop!(stack, i32, |a: i32, b: i32| (a as u32) < (b as u32)),
        0x4a => cmpop!(stack, i32, |a, b| a > b),
        0x4b => cmpop!(stack, i32, |a: i32, b: i32| (a as u32) > (b as u32)),
        0x4c => cmpop!(stack, i32, |a, b| a <= b),
        0x4d => cmpop!(stack, i32, |a: i32, b: i32| (a as u32) <= (b as u32)),
        0x4e => cmpop!(stack, i32, |a, b| a >= b),
        0x4f => cmpop!(stack, i32, |a: i32, b: i32| (a as u32) >= (b as u32)),
        // i64 comparisons
        0x50 => {
            let a = stack.pop().unwrap().i64();
            stack.push(Val::I32((a == 0) as i32));
        }
        0x51 => cmpop!(stack, i64, |a, b| a == b),
        0x52 => cmpop!(stack, i64, |a, b| a != b),
        0x53 => cmpop!(stack, i64, |a, b| a < b),
        0x54 => cmpop!(stack, i64, |a: i64, b: i64| (a as u64) < (b as u64)),
        0x55 => cmpop!(stack, i64, |a, b| a > b),
        0x56 => cmpop!(stack, i64, |a: i64, b: i64| (a as u64) > (b as u64)),
        0x57 => cmpop!(stack, i64, |a, b| a <= b),
        0x58 => cmpop!(stack, i64, |a: i64, b: i64| (a as u64) <= (b as u64)),
        0x59 => cmpop!(stack, i64, |a, b| a >= b),
        0x5a => cmpop!(stack, i64, |a: i64, b: i64| (a as u64) >= (b as u64)),
        // f32 comparisons
        0x5b => cmpop!(stack, f32, |a, b| a == b),
        0x5c => cmpop!(stack, f32, |a, b| a != b),
        0x5d => cmpop!(stack, f32, |a, b| a < b),
        0x5e => cmpop!(stack, f32, |a, b| a > b),
        0x5f => cmpop!(stack, f32, |a, b| a <= b),
        0x60 => cmpop!(stack, f32, |a, b| a >= b),
        // f64 comparisons
        0x61 => cmpop!(stack, f64, |a, b| a == b),
        0x62 => cmpop!(stack, f64, |a, b| a != b),
        0x63 => cmpop!(stack, f64, |a, b| a < b),
        0x64 => cmpop!(stack, f64, |a, b| a > b),
        0x65 => cmpop!(stack, f64, |a, b| a <= b),
        0x66 => cmpop!(stack, f64, |a, b| a >= b),
        // i32 arithmetic
        0x67 => un(stack, |a: i32| a.leading_zeros() as i32),
        0x68 => un(stack, |a: i32| a.trailing_zeros() as i32),
        0x69 => un(stack, |a: i32| a.count_ones() as i32),
        0x6a => binop!(stack, i32, I32, |a: i32, b: i32| a.wrapping_add(b)),
        0x6b => binop!(stack, i32, I32, |a: i32, b: i32| a.wrapping_sub(b)),
        0x6c => binop!(stack, i32, I32, |a: i32, b: i32| a.wrapping_mul(b)),
        0x6d => return idiv_i32(stack, true),
        0x6e => return idiv_i32(stack, false),
        0x6f => return irem_i32(stack, true),
        0x70 => return irem_i32(stack, false),
        0x71 => binop!(stack, i32, I32, |a: i32, b: i32| a & b),
        0x72 => binop!(stack, i32, I32, |a: i32, b: i32| a | b),
        0x73 => binop!(stack, i32, I32, |a: i32, b: i32| a ^ b),
        0x74 => binop!(stack, i32, I32, |a: i32, b: i32| a.wrapping_shl(b as u32)),
        0x75 => binop!(stack, i32, I32, |a: i32, b: i32| a.wrapping_shr(b as u32)),
        0x76 => binop!(stack, i32, I32, |a: i32, b: i32| ((a as u32).wrapping_shr(b as u32)) as i32),
        0x77 => binop!(stack, i32, I32, |a: i32, b: i32| a.rotate_left(b as u32)),
        0x78 => binop!(stack, i32, I32, |a: i32, b: i32| a.rotate_right(b as u32)),
        // i64 arithmetic
        0x79 => un64(stack, |a: i64| a.leading_zeros() as i64),
        0x7a => un64(stack, |a: i64| a.trailing_zeros() as i64),
        0x7b => un64(stack, |a: i64| a.count_ones() as i64),
        0x7c => binop!(stack, i64, I64, |a: i64, b: i64| a.wrapping_add(b)),
        0x7d => binop!(stack, i64, I64, |a: i64, b: i64| a.wrapping_sub(b)),
        0x7e => binop!(stack, i64, I64, |a: i64, b: i64| a.wrapping_mul(b)),
        0x7f => return idiv_i64(stack, true),
        0x80 => return idiv_i64(stack, false),
        0x81 => return irem_i64(stack, true),
        0x82 => return irem_i64(stack, false),
        0x83 => binop!(stack, i64, I64, |a: i64, b: i64| a & b),
        0x84 => binop!(stack, i64, I64, |a: i64, b: i64| a | b),
        0x85 => binop!(stack, i64, I64, |a: i64, b: i64| a ^ b),
        0x86 => binop!(stack, i64, I64, |a: i64, b: i64| a.wrapping_shl(b as u32)),
        0x87 => binop!(stack, i64, I64, |a: i64, b: i64| a.wrapping_shr(b as u32)),
        0x88 => binop!(stack, i64, I64, |a: i64, b: i64| ((a as u64).wrapping_shr(b as u32)) as i64),
        0x89 => binop!(stack, i64, I64, |a: i64, b: i64| a.rotate_left(b as u32)),
        0x8a => binop!(stack, i64, I64, |a: i64, b: i64| a.rotate_right(b as u32)),
        // f32 arithmetic
        0x8b => unf32(stack, |a: f32| a.abs()),
        0x8c => unf32(stack, |a: f32| -a),
        0x8d => unf32(stack, |a: f32| a.ceil()),
        0x8e => unf32(stack, |a: f32| a.floor()),
        0x8f => unf32(stack, |a: f32| a.trunc()),
        0x90 => unf32(stack, round_ties_even_f32),
        0x91 => unf32(stack, |a: f32| a.sqrt()),
        0x92 => binop!(stack, f32, F32, |a: f32, b: f32| a + b),
        0x93 => binop!(stack, f32, F32, |a: f32, b: f32| a - b),
        0x94 => binop!(stack, f32, F32, |a: f32, b: f32| a * b),
        0x95 => binop!(stack, f32, F32, |a: f32, b: f32| a / b),
        0x96 => binop!(stack, f32, F32, f32::min),
        0x97 => binop!(stack, f32, F32, f32::max),
        0x98 => binop!(stack, f32, F32, |a: f32, b: f32| a.copysign(b)),
        // f64 arithmetic
        0x99 => unf64(stack, |a: f64| a.abs()),
        0x9a => unf64(stack, |a: f64| -a),
        0x9b => unf64(stack, |a: f64| a.ceil()),
        0x9c => unf64(stack, |a: f64| a.floor()),
        0x9d => unf64(stack, |a: f64| a.trunc()),
        0x9e => unf64(stack, round_ties_even_f64),
        0x9f => unf64(stack, |a: f64| a.sqrt()),
        0xa0 => binop!(stack, f64, F64, |a: f64, b: f64| a + b),
        0xa1 => binop!(stack, f64, F64, |a: f64, b: f64| a - b),
        0xa2 => binop!(stack, f64, F64, |a: f64, b: f64| a * b),
        0xa3 => binop!(stack, f64, F64, |a: f64, b: f64| a / b),
        0xa4 => binop!(stack, f64, F64, f64::min),
        0xa5 => binop!(stack, f64, F64, f64::max),
        0xa6 => binop!(stack, f64, F64, |a: f64, b: f64| a.copysign(b)),
        // conversions
        0xa7 => conv(stack, |v: Val| Val::I32(v.i64() as i32)),          // i32.wrap_i64
        0xa8 => conv(stack, |v: Val| Val::I32(v.f32().trunc() as i32)),   // i32.trunc_f32_s
        0xa9 => conv(stack, |v: Val| Val::I32(v.f32().trunc() as u32 as i32)),
        0xaa => conv(stack, |v: Val| Val::I32(v.f64().trunc() as i32)),
        0xab => conv(stack, |v: Val| Val::I32(v.f64().trunc() as u32 as i32)),
        0xac => conv(stack, |v: Val| Val::I64(v.i32() as i64)),           // i64.extend_i32_s
        0xad => conv(stack, |v: Val| Val::I64(v.i32() as u32 as i64)),    // i64.extend_i32_u
        0xae => conv(stack, |v: Val| Val::I64(v.f32().trunc() as i64)),
        0xaf => conv(stack, |v: Val| Val::I64(v.f32().trunc() as u64 as i64)),
        0xb0 => conv(stack, |v: Val| Val::I64(v.f64().trunc() as i64)),
        0xb1 => conv(stack, |v: Val| Val::I64(v.f64().trunc() as u64 as i64)),
        0xb2 => conv(stack, |v: Val| Val::F32(v.i32() as f32)),
        0xb3 => conv(stack, |v: Val| Val::F32(v.i32() as u32 as f32)),
        0xb4 => conv(stack, |v: Val| Val::F32(v.i64() as f32)),
        0xb5 => conv(stack, |v: Val| Val::F32(v.i64() as u64 as f32)),
        0xb6 => conv(stack, |v: Val| Val::F32(v.f64() as f32)),
        0xb7 => conv(stack, |v: Val| Val::F64(v.i32() as f64)),
        0xb8 => conv(stack, |v: Val| Val::F64(v.i32() as u32 as f64)),
        0xb9 => conv(stack, |v: Val| Val::F64(v.i64() as f64)),
        0xba => conv(stack, |v: Val| Val::F64(v.i64() as u64 as f64)),
        0xbb => conv(stack, |v: Val| Val::F64(v.f32() as f64)),
        0xbc => conv(stack, |v: Val| Val::I32(v.f32().to_bits() as i32)),
        0xbd => conv(stack, |v: Val| Val::I64(v.f64().to_bits() as i64)),
        0xbe => conv(stack, |v: Val| Val::F32(f32::from_bits(v.i32() as u32))),
        0xbf => conv(stack, |v: Val| Val::F64(f64::from_bits(v.i64() as u64))),
        // sign extension
        0xc0 => conv(stack, |v: Val| Val::I32(v.i32() as i8 as i32)),
        0xc1 => conv(stack, |v: Val| Val::I32(v.i32() as i16 as i32)),
        0xc2 => conv(stack, |v: Val| Val::I64(v.i64() as i8 as i64)),
        0xc3 => conv(stack, |v: Val| Val::I64(v.i64() as i16 as i64)),
        0xc4 => conv(stack, |v: Val| Val::I64(v.i64() as i32 as i64)),
        other => return Err(format!("wasm: unsupported opcode 0x{other:x}")),
    }
    Ok(())
}

fn un(stack: &mut Vec<Val>, f: impl Fn(i32) -> i32) {
    let a = stack.pop().unwrap().i32();
    stack.push(Val::I32(f(a)));
}
fn un64(stack: &mut Vec<Val>, f: impl Fn(i64) -> i64) {
    let a = stack.pop().unwrap().i64();
    stack.push(Val::I64(f(a)));
}
fn unf32(stack: &mut Vec<Val>, f: impl Fn(f32) -> f32) {
    let a = stack.pop().unwrap().f32();
    stack.push(Val::F32(f(a)));
}
fn unf64(stack: &mut Vec<Val>, f: impl Fn(f64) -> f64) {
    let a = stack.pop().unwrap().f64();
    stack.push(Val::F64(f(a)));
}
fn conv(stack: &mut Vec<Val>, f: impl Fn(Val) -> Val) {
    let a = stack.pop().unwrap();
    stack.push(f(a));
}

fn round_ties_even_f32(a: f32) -> f32 {
    let r = a.round();
    if (a - a.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 {
        r - a.signum()
    } else {
        r
    }
}
fn round_ties_even_f64(a: f64) -> f64 {
    let r = a.round();
    if (a - a.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 {
        r - a.signum()
    } else {
        r
    }
}

fn idiv_i32(stack: &mut Vec<Val>, signed: bool) -> Result<(), String> {
    let b = stack.pop().unwrap().i32();
    let a = stack.pop().unwrap().i32();
    if b == 0 {
        return Err("wasm: integer divide by zero".into());
    }
    let r = if signed {
        if a == i32::MIN && b == -1 {
            return Err("wasm: integer overflow".into());
        }
        a.wrapping_div(b)
    } else {
        ((a as u32) / (b as u32)) as i32
    };
    stack.push(Val::I32(r));
    Ok(())
}
fn irem_i32(stack: &mut Vec<Val>, signed: bool) -> Result<(), String> {
    let b = stack.pop().unwrap().i32();
    let a = stack.pop().unwrap().i32();
    if b == 0 {
        return Err("wasm: integer divide by zero".into());
    }
    let r = if signed {
        a.wrapping_rem(b)
    } else {
        ((a as u32) % (b as u32)) as i32
    };
    stack.push(Val::I32(r));
    Ok(())
}
fn idiv_i64(stack: &mut Vec<Val>, signed: bool) -> Result<(), String> {
    let b = stack.pop().unwrap().i64();
    let a = stack.pop().unwrap().i64();
    if b == 0 {
        return Err("wasm: integer divide by zero".into());
    }
    let r = if signed {
        if a == i64::MIN && b == -1 {
            return Err("wasm: integer overflow".into());
        }
        a.wrapping_div(b)
    } else {
        ((a as u64) / (b as u64)) as i64
    };
    stack.push(Val::I64(r));
    Ok(())
}
fn irem_i64(stack: &mut Vec<Val>, signed: bool) -> Result<(), String> {
    let b = stack.pop().unwrap().i64();
    let a = stack.pop().unwrap().i64();
    if b == 0 {
        return Err("wasm: integer divide by zero".into());
    }
    let r = if signed {
        a.wrapping_rem(b)
    } else {
        ((a as u64) % (b as u64)) as i64
    };
    stack.push(Val::I64(r));
    Ok(())
}

/// Evaluate a constant init expression (used for globals, data/elem offsets). Only const opcodes
/// and `global.get` of an already-initialized global.
pub fn eval_const_expr(code: &[u8], globals: &[Val]) -> Result<Val, String> {
    let mut ip = 0;
    let mut result = Val::I32(0);
    while ip < code.len() {
        let op = code[ip];
        ip += 1;
        match op {
            0x41 => result = Val::I32(read_sleb(code, &mut ip)? as i32),
            0x42 => result = Val::I64(read_sleb(code, &mut ip)?),
            0x43 => {
                result = Val::F32(f32::from_le_bytes(code[ip..ip + 4].try_into().unwrap()));
                ip += 4;
            }
            0x44 => {
                result = Val::F64(f64::from_le_bytes(code[ip..ip + 8].try_into().unwrap()));
                ip += 8;
            }
            0x23 => {
                let g = read_uleb(code, &mut ip)? as usize;
                result = *globals.get(g).ok_or("wasm: const expr global out of range")?;
            }
            0xd0 => {
                ip += 1;
                result = Val::Ref(None);
            }
            0xd2 => {
                result = Val::Ref(Some(read_uleb(code, &mut ip)? as u32));
            }
            0x0b => break,
            other => return Err(format!("wasm: bad const expr opcode 0x{other:x}")),
        }
    }
    Ok(result)
}
