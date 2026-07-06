//! WebAssembly binary-format decoder (the MVP + a few post-MVP proposals: multi-value results,
//! sign-extension, non-trapping conversions, bulk memory). Produces a [`Module`] the interpreter in
//! `exec.rs` runs. Structural validation only (well-formed binary, section order, index ranges);
//! full stack-type validation is not performed.

use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    FuncRef,
    ExternRef,
}

#[derive(Debug, Clone)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Debug, Clone)]
pub enum ImportKind {
    Func(u32), // type index
    Table(TableType),
    Memory(Limits),
    Global(GlobalType),
}

#[derive(Debug, Clone)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: ImportKind,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct TableType {
    pub elem: ValType,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy)]
pub struct GlobalType {
    pub val: ValType,
    pub mutable: bool,
}

#[derive(Debug, Clone)]
pub struct Global {
    pub ty: GlobalType,
    pub init: Vec<u8>, // a constant init expression (raw bytes, ending in `end`)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

#[derive(Debug, Clone)]
pub struct Export {
    pub name: String,
    pub kind: ExportKind,
    pub index: u32,
}

#[derive(Debug, Clone)]
pub struct FuncBody {
    pub locals: Vec<ValType>, // flattened (each declared local, expanded from run-length groups)
    pub code: Vec<u8>,        // raw instruction bytes, up to and excluding the final `end`
}

#[derive(Debug, Clone)]
pub struct DataSegment {
    pub active: Option<(u32, Vec<u8>)>, // (memory index, offset init expr) for active segments
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ElemSegment {
    pub table: u32,
    pub offset: Vec<u8>, // offset init expr
    pub func_indices: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    /// Type index for each *defined* function (imports excluded).
    pub func_types: Vec<u32>,
    pub tables: Vec<TableType>,
    pub memories: Vec<Limits>,
    pub globals: Vec<Global>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elems: Vec<ElemSegment>,
    pub code: Vec<FuncBody>,
    pub data: Vec<DataSegment>,
    /// Number of imported functions (defined funcs are indexed after these).
    pub imported_func_count: u32,
    pub imported_table_count: u32,
    pub imported_mem_count: u32,
    pub imported_global_count: u32,
}

// ---- byte reader with LEB128 ------------------------------------------------------------------

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

type R<T> = Result<T, String>;

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }
    pub fn byte(&mut self) -> R<u8> {
        let b = *self.data.get(self.pos).ok_or("wasm: unexpected end of input")?;
        self.pos += 1;
        Ok(b)
    }
    pub fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or("wasm: length overflow")?;
        let s = self.data.get(self.pos..end).ok_or("wasm: unexpected end of input")?;
        self.pos = end;
        Ok(s)
    }
    pub fn u32(&mut self) -> R<u32> {
        Ok(self.u64_leb()? as u32)
    }
    /// Unsigned LEB128.
    pub fn u64_leb(&mut self) -> R<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err("wasm: LEB128 overflow".into());
            }
        }
    }
    /// Signed LEB128.
    pub fn i64_leb(&mut self) -> R<i64> {
        let mut result = 0i64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            result |= ((b & 0x7f) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && (b & 0x40) != 0 {
                    result |= -1i64 << shift; // sign-extend
                }
                return Ok(result);
            }
            if shift >= 64 {
                return Err("wasm: LEB128 overflow".into());
            }
        }
    }
    pub fn i32(&mut self) -> R<i32> {
        Ok(self.i64_leb()? as i32)
    }
    pub fn f32(&mut self) -> R<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> R<f64> {
        Ok(f64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    pub fn name(&mut self) -> R<String> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "wasm: invalid utf-8 in name".into())
    }
}

fn val_type(b: u8) -> Result<ValType, String> {
    match b {
        0x7f => Ok(ValType::I32),
        0x7e => Ok(ValType::I64),
        0x7d => Ok(ValType::F32),
        0x7c => Ok(ValType::F64),
        0x70 => Ok(ValType::FuncRef),
        0x6f => Ok(ValType::ExternRef),
        other => Err(format!("wasm: unknown value type 0x{other:x}")),
    }
}

fn limits(r: &mut Reader) -> Result<Limits, String> {
    let flag = r.byte()?;
    let min = r.u32()?;
    let max = if flag & 1 != 0 { Some(r.u32()?) } else { None };
    Ok(Limits { min, max })
}

fn table_type(r: &mut Reader) -> Result<TableType, String> {
    let elem = val_type(r.byte()?)?;
    Ok(TableType { elem, limits: limits(r)? })
}

fn global_type(r: &mut Reader) -> Result<GlobalType, String> {
    let val = val_type(r.byte()?)?;
    let mutable = r.byte()? != 0;
    Ok(GlobalType { val, mutable })
}

/// Read a constant/offset init expression (raw bytes) up to and including the terminating `end`.
fn read_const_expr(r: &mut Reader) -> Result<Vec<u8>, String> {
    let start = r.pos;
    let mut depth = 0;
    loop {
        let op = r.byte()?;
        match op {
            0x41 => {
                r.i64_leb()?;
            } // i32.const
            0x42 => {
                r.i64_leb()?;
            } // i64.const
            0x43 => {
                r.bytes(4)?;
            } // f32.const
            0x44 => {
                r.bytes(8)?;
            } // f64.const
            0x23 => {
                r.u32()?;
            } // global.get
            0xd0 => {
                r.byte()?;
            } // ref.null t
            0xd2 => {
                r.u32()?;
            } // ref.func
            0x0b => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => return Err(format!("wasm: unsupported opcode 0x{op:x} in const expr")),
        }
    }
    Ok(r.data[start..r.pos].to_vec())
}

/// Skip past an instruction's immediate operands during a section scan (unused here but kept for
/// clarity of the const-expr reader's structure).
pub fn decode(data: &[u8]) -> Result<Rc<Module>, String> {
    let mut r = Reader::new(data);
    if r.bytes(4)? != b"\0asm" {
        return Err("wasm: bad magic".into());
    }
    if r.bytes(4)? != 1u32.to_le_bytes() {
        return Err("wasm: unsupported version".into());
    }

    let mut m = Module::default();
    let mut last_section = 0u8;
    while !r.eof() {
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let end = r.pos + size;
        if end > r.data.len() {
            return Err("wasm: section overruns input".into());
        }
        // Section ordering (custom sections, id 0, may appear anywhere and are ignored).
        if id != 0 {
            if id <= last_section {
                return Err("wasm: sections out of order".into());
            }
            last_section = id;
        }
        match id {
            0 => {} // custom section — skip
            1 => decode_types(&mut r, &mut m)?,
            2 => decode_imports(&mut r, &mut m)?,
            3 => decode_functions(&mut r, &mut m)?,
            4 => {
                for _ in 0..r.u32()? {
                    m.tables.push(table_type(&mut r)?);
                }
            }
            5 => {
                for _ in 0..r.u32()? {
                    m.memories.push(limits(&mut r)?);
                }
            }
            6 => decode_globals(&mut r, &mut m)?,
            7 => decode_exports(&mut r, &mut m)?,
            8 => m.start = Some(r.u32()?),
            9 => decode_elems(&mut r, &mut m)?,
            10 => decode_code(&mut r, &mut m)?,
            11 => decode_data(&mut r, &mut m)?,
            12 => {
                r.u32()?;
            } // DataCount section — advisory, skip
            other => return Err(format!("wasm: unknown section id {other}")),
        }
        if r.pos != end {
            return Err(format!("wasm: section {id} size mismatch"));
        }
    }
    Ok(Rc::new(m))
}

fn decode_types(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        if r.byte()? != 0x60 {
            return Err("wasm: expected func type (0x60)".into());
        }
        let mut params = Vec::new();
        for _ in 0..r.u32()? {
            params.push(val_type(r.byte()?)?);
        }
        let mut results = Vec::new();
        for _ in 0..r.u32()? {
            results.push(val_type(r.byte()?)?);
        }
        m.types.push(FuncType { params, results });
    }
    Ok(())
}

fn decode_imports(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let module = r.name()?;
        let name = r.name()?;
        let kind = match r.byte()? {
            0x00 => {
                let t = r.u32()?;
                m.imported_func_count += 1;
                ImportKind::Func(t)
            }
            0x01 => {
                m.imported_table_count += 1;
                ImportKind::Table(table_type(r)?)
            }
            0x02 => {
                m.imported_mem_count += 1;
                ImportKind::Memory(limits(r)?)
            }
            0x03 => {
                m.imported_global_count += 1;
                ImportKind::Global(global_type(r)?)
            }
            other => return Err(format!("wasm: unknown import kind {other}")),
        };
        m.imports.push(Import { module, name, kind });
    }
    Ok(())
}

fn decode_functions(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let t = r.u32()?;
        if t as usize >= m.types.len() {
            return Err("wasm: function type index out of range".into());
        }
        m.func_types.push(t);
    }
    Ok(())
}

fn decode_globals(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let ty = global_type(r)?;
        let init = read_const_expr(r)?;
        m.globals.push(Global { ty, init });
    }
    Ok(())
}

fn decode_exports(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let name = r.name()?;
        let kind = match r.byte()? {
            0x00 => ExportKind::Func,
            0x01 => ExportKind::Table,
            0x02 => ExportKind::Memory,
            0x03 => ExportKind::Global,
            other => return Err(format!("wasm: unknown export kind {other}")),
        };
        let index = r.u32()?;
        m.exports.push(Export { name, kind, index });
    }
    Ok(())
}

fn decode_elems(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let flags = r.u32()?;
        // Support the common active-segment forms (flags 0 and 2); others are rejected.
        match flags {
            0 => {
                let offset = read_const_expr(r)?;
                let mut func_indices = Vec::new();
                for _ in 0..r.u32()? {
                    func_indices.push(r.u32()?);
                }
                m.elems.push(ElemSegment { table: 0, offset, func_indices });
            }
            2 => {
                let table = r.u32()?;
                let offset = read_const_expr(r)?;
                let _elemkind = r.byte()?;
                let mut func_indices = Vec::new();
                for _ in 0..r.u32()? {
                    func_indices.push(r.u32()?);
                }
                m.elems.push(ElemSegment { table, offset, func_indices });
            }
            other => return Err(format!("wasm: unsupported element segment kind {other}")),
        }
    }
    Ok(())
}

fn decode_code(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let size = r.u32()? as usize;
        let body_end = r.pos + size;
        let mut locals = Vec::new();
        for _ in 0..r.u32()? {
            let count = r.u32()?;
            let ty = val_type(r.byte()?)?;
            for _ in 0..count {
                locals.push(ty);
            }
        }
        // Remaining bytes (minus the trailing `end`) are the instruction stream.
        if body_end == 0 || body_end > r.data.len() {
            return Err("wasm: bad code body".into());
        }
        let code = r.data[r.pos..body_end - 1].to_vec();
        if r.data[body_end - 1] != 0x0b {
            return Err("wasm: function body not terminated by end".into());
        }
        r.pos = body_end;
        m.code.push(FuncBody { locals, code });
    }
    if m.code.len() != m.func_types.len() {
        return Err("wasm: code/function count mismatch".into());
    }
    Ok(())
}

fn decode_data(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let flags = r.u32()?;
        match flags {
            0 => {
                let offset = read_const_expr(r)?;
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                m.data.push(DataSegment { active: Some((0, offset)), bytes });
            }
            1 => {
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                m.data.push(DataSegment { active: None, bytes });
            }
            2 => {
                let memidx = r.u32()?;
                let offset = read_const_expr(r)?;
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                m.data.push(DataSegment { active: Some((memidx, offset)), bytes });
            }
            other => return Err(format!("wasm: unsupported data segment kind {other}")),
        }
    }
    Ok(())
}

/// Structural validation: decode succeeds and export indices are in range.
pub fn validate(data: &[u8]) -> bool {
    match decode(data) {
        Ok(m) => {
            let total_funcs = m.imported_func_count as usize + m.func_types.len();
            m.exports.iter().all(|e| match e.kind {
                ExportKind::Func => (e.index as usize) < total_funcs,
                ExportKind::Global => {
                    (e.index as usize) < m.imported_global_count as usize + m.globals.len()
                }
                ExportKind::Memory => {
                    (e.index as usize) < m.imported_mem_count as usize + m.memories.len()
                }
                ExportKind::Table => {
                    (e.index as usize) < m.imported_table_count as usize + m.tables.len()
                }
            })
        }
        Err(_) => false,
    }
}
