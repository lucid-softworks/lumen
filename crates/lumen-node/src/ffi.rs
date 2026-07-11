//! `bun:ffi` backing — a real foreign-function bridge with no libffi and no third-party crate.
//!
//! Native calls are made through monomorphized `extern "C"` function-pointer trampolines selected
//! at runtime by the argument register classes; see `generate_ffi_trampolines` in `build.rs` for
//! how the (int-count, float-width) dispatch table is generated and why reordering integer args
//! ahead of float args is ABI-correct on x86-64 SysV and arm64 AAPCS.
//!
//! # What is real
//! - `dlopen`/`dlsym` via [`crate::dylib::DynLib`] (the same loader N-API uses).
//! - Calling arbitrary symbols with 0..=8 register-class arguments (i8..u64, f32/f64, bool,
//!   pointer, cstring, function-pointer), including mixed int/float orders.
//! - `ptr`, `read.*`, `CString`, `toArrayBuffer`, `toBuffer`, `CFunction`.
//! - `JSCallback` for integer/pointer-class signatures (the qsort-comparator family), re-entering
//!   JS on the loop thread.
//!
//! # What honestly throws
//! - `>8` arguments, struct-by-value, and varargs (no register-class lowering exists here).
//! - `JSCallback` with floating-point arguments/return, or invoked from a foreign thread.
//! - `cc` (runtime C compilation) and `viewSource`.
//!
//! # Safety
//! Every call speaks a raw C ABI over a `transmute`d pointer, so this module is unavoidably
//! `unsafe`. Like N-API (`napi.rs`), JSCallback re-entry recovers `&mut Ctx` from a thread-local
//! raw pointer set for the duration of an outgoing native call; the engine is single-threaded, so
//! the nested borrow is sound as long as callbacks stay on the loop thread (enforced: a null
//! thread-local means "no live call context" and the callback returns 0 rather than fault).

#![allow(clippy::missing_transmute_annotations)]
#![allow(clippy::too_many_arguments)]

use std::cell::Cell;
use std::os::raw::c_void;

use lumen_host::{Ctx, Value};

use crate::dylib::DynLib;

/// One floating-point argument, carrying its declared width so the trampoline can place the right
/// bit pattern in the SIMD register.
#[derive(Clone, Copy)]
pub enum FArg {
    F32(f32),
    F64(f64),
}

impl FArg {
    #[inline]
    fn f32v(self) -> f32 {
        match self {
            FArg::F32(x) => x,
            FArg::F64(x) => x as f32,
        }
    }
    #[inline]
    fn f64v(self) -> f64 {
        match self {
            FArg::F64(x) => x,
            FArg::F32(x) => x as f64,
        }
    }
}

/// Bitmask of which float arguments are `f64` (bit *i* set ⇒ argument *i* is `f64`).
#[inline]
fn fmask(floats: &[FArg]) -> u32 {
    let mut m = 0u32;
    for (i, f) in floats.iter().enumerate() {
        if matches!(f, FArg::F64(_)) {
            m |= 1 << i;
        }
    }
    m
}

// The generated `call_int`/`call_f32`/`call_f64` dispatchers and the `JSCB_THUNKS` pool.
include!(concat!(env!("OUT_DIR"), "/ffi_trampolines.rs"));

/// Must match `build.rs`.
const MAX_ARGS: usize = 8;
const JSCB_POOL: usize = 16;

// ---- FFIType numeric codes (verbatim from bun_ffi.js / Bun v1.2.21) ---------------------------

const T_CHAR: u8 = 0;
const T_I8: u8 = 1;
const T_U8: u8 = 2;
const T_I16: u8 = 3;
const T_U16: u8 = 4;
const T_I32: u8 = 5;
const T_U32: u8 = 6;
const T_I64: u8 = 7;
const T_U64: u8 = 8;
const T_F64: u8 = 9;
const T_F32: u8 = 10;
const T_BOOL: u8 = 11;
const T_PTR: u8 = 12;
const T_VOID: u8 = 13;
const T_CSTRING: u8 = 14;
const T_I64_FAST: u8 = 15;
const T_U64_FAST: u8 = 16;
const T_FUNCTION: u8 = 17;
const T_NAPI_ENV: u8 = 18;
const T_NAPI_VALUE: u8 = 19;
const T_BUFFER: u8 = 20;

/// Is this type carried in the floating-point register file?
fn is_float(code: u8) -> bool {
    code == T_F32 || code == T_F64
}

/// Is this type a pointer (integer register file, but the JS value is a pointer/typed array)?
fn is_pointer(code: u8) -> bool {
    matches!(
        code,
        T_PTR | T_CSTRING | T_FUNCTION | T_BUFFER | T_NAPI_ENV | T_NAPI_VALUE
    )
}

// ---- host state -------------------------------------------------------------------------------

struct CbEntry {
    func: Value,
    arg_codes: Vec<u8>,
    ret_code: u8,
}

/// Per-engine FFI state: opened libraries (kept mapped for the process lifetime, like N-API
/// addons) and the JSCallback registry (a `[arity][slot]` matrix mirroring `JSCB_THUNKS`).
#[derive(Default)]
struct FfiState {
    libs: Vec<Option<DynLib>>,
    callbacks: Vec<Vec<Option<CbEntry>>>,
}

fn ffi_state(ctx: &mut Ctx) -> &mut FfiState {
    if ctx.host_mut::<FfiState>().is_none() {
        let mut st = FfiState::default();
        st.callbacks = (0..=MAX_ARGS).map(|_| (0..JSCB_POOL).map(|_| None).collect()).collect();
        ctx.op_state().put(st);
    }
    ctx.host_mut::<FfiState>().unwrap()
}

thread_local! {
    /// The engine pointer for the currently-executing outgoing native call, so a JSCallback thunk
    /// can re-enter JS. Null outside any FFI call (a foreign-thread invocation).
    static CURRENT_CTX: Cell<*mut Ctx> = const { Cell::new(std::ptr::null_mut()) };
}

// ---- small arg helpers ------------------------------------------------------------------------

fn ptr_num_arg(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<u64, Value> {
    value_to_pointer(ctx, args.get(i).unwrap_or(&Value::Undefined))
}

fn type_error(ctx: &mut Ctx, msg: impl Into<String>) -> Value {
    ctx.make_error("TypeError", msg.into())
}

fn plain_error(ctx: &mut Ctx, msg: impl Into<String>) -> Value {
    ctx.make_error("Error", msg.into())
}

/// Convert a JS value used as a pointer/cstring/function argument into a raw address, matching
/// Bun's acceptance rules: numbers are addresses, typed arrays contribute their backing pointer,
/// null/undefined are 0, and strings/bigints are refused with Bun's exact messages.
fn value_to_pointer(ctx: &mut Ctx, v: &Value) -> Result<u64, Value> {
    match v {
        Value::Num(n) => Ok(*n as i64 as u64),
        Value::Null | Value::Undefined => Ok(0),
        Value::Str(_) => Err(type_error(
            ctx,
            "To convert a string to a pointer, encode it as a buffer",
        )),
        Value::BigInt(b) => Err(type_error(
            ctx,
            format!("Unable to convert {} to a pointer", b.to_i128_wrapping()),
        )),
        Value::Obj(_) => {
            if let Some((_, _, p)) = ctx.typed_array_raw(v) {
                Ok(p as u64)
            } else {
                Err(type_error(
                    ctx,
                    "To convert a value to a pointer, it must be a TypedArray, number, or null",
                ))
            }
        }
        _ => Ok(0),
    }
}

/// Marshal one JS argument into either the integer or float register stream, per its declared type.
fn marshal_arg(
    ctx: &mut Ctx,
    code: u8,
    v: &Value,
    ints: &mut Vec<u64>,
    floats: &mut Vec<FArg>,
) -> Result<(), Value> {
    if is_float(code) {
        let n = ctx.coerce_number(v)?;
        floats.push(if code == T_F32 { FArg::F32(n as f32) } else { FArg::F64(n) });
        return Ok(());
    }
    if is_pointer(code) {
        ints.push(value_to_pointer(ctx, v)?);
        return Ok(());
    }
    // Integer register class.
    let bits = match code {
        T_I64 | T_U64 | T_I64_FAST | T_U64_FAST => match v {
            Value::BigInt(b) => b.to_i128_wrapping() as u64,
            _ => {
                let n = ctx.coerce_number(v)?;
                if n.fract() != 0.0 || !n.is_finite() {
                    return Err(type_error(ctx, "Not an integer"));
                }
                n as i64 as u64
            }
        },
        _ => {
            // char/i8/u8/i16/u16/i32/u32/bool: ToNumber then truncate; only the low bits of the
            // declared width reach the callee, so wrapping matches Bun's `| 0`-style coercion.
            let n = ctx.coerce_number(v)?;
            n as i64 as u64
        }
    };
    ints.push(bits);
    Ok(())
}

/// Decode a native return value (raw register bits) into the JS value the declared type implies.
fn marshal_return(raw: RawRet, code: u8) -> Value {
    match code {
        T_VOID => Value::Undefined,
        T_CHAR | T_I8 => Value::Num((raw.int as u8 as i8) as f64),
        T_U8 => Value::Num((raw.int as u8) as f64),
        T_I16 => Value::Num((raw.int as u16 as i16) as f64),
        T_U16 => Value::Num((raw.int as u16) as f64),
        T_I32 => Value::Num((raw.int as u32 as i32) as f64),
        T_U32 => Value::Num((raw.int as u32) as f64),
        T_BOOL => Value::Bool(raw.int & 1 != 0),
        T_I64 => Value::bigint_from_i128((raw.int as i64) as i128),
        T_U64 => Value::bigint_from_u64(raw.int),
        T_I64_FAST => Value::Num((raw.int as i64) as f64),
        T_U64_FAST => Value::Num(raw.int as f64),
        T_F32 => Value::Num(raw.f32 as f64),
        T_F64 => Value::Num(raw.f64),
        // A cstring return hands the raw address back so the JS wrapper can build a `CString`.
        T_CSTRING => Value::Num(raw.int as f64),
        // Other pointer returns are numbers, except a null pointer, which Bun surfaces as `null`.
        _ if is_pointer(code) => {
            if raw.int == 0 {
                Value::Null
            } else {
                Value::Num(raw.int as f64)
            }
        }
        _ => Value::Num(raw.int as f64),
    }
}

/// The three return register classes a trampoline can produce.
struct RawRet {
    int: u64,
    f32: f32,
    f64: f64,
}

/// Run the trampoline for `(fnptr, ints, floats)` selecting the dispatcher by the return class.
unsafe fn invoke_native(fnptr: *const c_void, ints: &[u64], floats: &[FArg], ret_code: u8) -> RawRet {
    match ret_code {
        T_F32 => RawRet { int: 0, f32: call_f32(fnptr, ints, floats), f64: 0.0 },
        T_F64 => RawRet { int: 0, f32: 0.0, f64: call_f64(fnptr, ints, floats) },
        _ => RawRet { int: call_int(fnptr, ints, floats), f32: 0.0, f64: 0.0 },
    }
}

// ---- ops --------------------------------------------------------------------------------------

fn arr_len(ctx: &mut Ctx, v: &Value) -> usize {
    match ctx.member_get(v, "length") {
        Ok(Value::Num(n)) => n as usize,
        _ => 0,
    }
}

/// `__ffi.dlopen(path)` → an opaque library id (kept mapped for the process lifetime).
pub fn op_dlopen(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let path = ctx.coerce_string(args.first().unwrap_or(&Value::Undefined))?.to_string();
    let lib = DynLib::open(&path)
        .map_err(|e| plain_error(ctx, format!("cannot open library '{path}': {e}")))?;
    let st = ffi_state(ctx);
    st.libs.push(Some(lib));
    Ok(Value::Num((st.libs.len() - 1) as f64))
}

/// `__ffi.dlsym(libId, name)` → the symbol's raw address as a number, or a Bun-style throw.
pub fn op_dlsym(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as usize;
    let name = ctx.coerce_string(args.get(1).unwrap_or(&Value::Undefined))?.to_string();
    let addr = {
        let st = ffi_state(ctx);
        st.libs.get(id).and_then(|l| l.as_ref()).and_then(|l| l.symbol(&name))
    };
    match addr {
        Some(p) => Ok(Value::Num(p as usize as f64)),
        None => Err(plain_error(ctx, format!("symbol \"{name}\" not found"))),
    }
}

/// `__ffi.dlclose(libId)` — unmap a library (its symbols dangle afterward).
pub fn op_dlclose(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as usize;
    let st = ffi_state(ctx);
    if let Some(slot) = st.libs.get_mut(id) {
        *slot = None;
    }
    Ok(Value::Undefined)
}

/// `__ffi.call(fnPtr, retCode, [argCodes], [args])` — marshal, trampoline, and marshal back.
pub fn op_call(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let fnptr = ptr_num_arg(ctx, args, 0)? as *const c_void;
    let ret_code = ctx.coerce_number(args.get(1).unwrap_or(&Value::Undefined))? as u8;
    let codes_v = args.get(2).cloned().unwrap_or(Value::Undefined);
    let vals_v = args.get(3).cloned().unwrap_or(Value::Undefined);

    let n = arr_len(ctx, &codes_v);
    if n > MAX_ARGS {
        return Err(plain_error(
            ctx,
            format!("bun:ffi call with {n} arguments is not supported in lumen (max {MAX_ARGS}; struct-by-value and varargs are unsupported)"),
        ));
    }

    let mut ints: Vec<u64> = Vec::with_capacity(n);
    let mut floats: Vec<FArg> = Vec::with_capacity(n);
    for i in 0..n {
        let code = ctx.member_get(&codes_v, &i.to_string())?.as_num_opt().unwrap_or(0.0) as u8;
        let v = ctx.member_get(&vals_v, &i.to_string())?;
        marshal_arg(ctx, code, &v, &mut ints, &mut floats)?;
    }

    // Publish the engine pointer so a JSCallback fired *during* this call can re-enter JS.
    let prev = CURRENT_CTX.with(|c| c.replace(ctx as *mut Ctx));
    let raw = unsafe { invoke_native(fnptr, &ints, &floats, ret_code) };
    CURRENT_CTX.with(|c| c.set(prev));

    Ok(marshal_return(raw, ret_code))
}

/// `__ffi.ptr(typedArray, byteOffset?)` → the backing address as a number.
pub fn op_ptr(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().cloned().unwrap_or(Value::Undefined);
    let off = ctx.coerce_number(args.get(1).unwrap_or(&Value::Num(0.0)))? as usize;
    match ctx.typed_array_raw(&v) {
        Some((_, _, p)) => Ok(Value::Num((p as usize + off) as f64)),
        None => Err(type_error(
            ctx,
            "bun:ffi ptr expects a TypedArray (ArrayBuffer views are not supported in lumen)",
        )),
    }
}

/// `__ffi.read(ptr, offset, kind)` — a DataView-style read from a raw address (little-endian, as
/// on both supported targets). `kind`: 0=u8 1=i8 2=u16 3=i16 4=u32 5=i32 6=u64 7=i64 8=f32 9=f64
/// 10=ptr 11=intptr.
pub fn op_read(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let base = ptr_num_arg(ctx, args, 0)?;
    let off = ctx.coerce_number(args.get(1).unwrap_or(&Value::Num(0.0)))? as usize;
    let kind = ctx.coerce_number(args.get(2).unwrap_or(&Value::Undefined))? as u8;
    let p = (base as usize + off) as *const u8;
    unsafe {
        let v = match kind {
            0 => Value::Num(p.read_unaligned() as f64),
            1 => Value::Num((p.read_unaligned() as i8) as f64),
            2 => Value::Num((p as *const u16).read_unaligned() as f64),
            3 => Value::Num((p as *const i16).read_unaligned() as f64),
            4 => Value::Num((p as *const u32).read_unaligned() as f64),
            5 => Value::Num((p as *const i32).read_unaligned() as f64),
            6 => Value::bigint_from_u64((p as *const u64).read_unaligned()),
            7 => Value::bigint_from_i128((p as *const i64).read_unaligned() as i128),
            8 => Value::Num((p as *const f32).read_unaligned() as f64),
            9 => Value::Num((p as *const f64).read_unaligned()),
            10 | 11 => Value::Num((p as *const usize).read_unaligned() as f64),
            _ => Value::Undefined,
        };
        Ok(v)
    }
}

/// `__ffi.readCString(ptr, byteOffset, byteLength)` — decode UTF-8 (`byteLength < 0` ⇒ NUL-scan).
pub fn op_read_cstring(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let base = ptr_num_arg(ctx, args, 0)?;
    let off = ctx.coerce_number(args.get(1).unwrap_or(&Value::Num(0.0)))? as usize;
    let len = ctx.coerce_number(args.get(2).unwrap_or(&Value::Num(-1.0)))?;
    if base == 0 {
        return Ok(Value::str(""));
    }
    let p = (base as usize + off) as *const u8;
    let bytes: &[u8] = unsafe {
        if len < 0.0 {
            let mut n = 0usize;
            while *p.add(n) != 0 {
                n += 1;
            }
            std::slice::from_raw_parts(p, n)
        } else {
            std::slice::from_raw_parts(p, len as usize)
        }
    };
    Ok(Value::from_string(String::from_utf8_lossy(bytes).into_owned()))
}

/// `__ffi.toArrayBuffer(ptr, byteOffset, byteLength)` — a *copy* of native memory as an
/// ArrayBuffer. (Bun returns a zero-copy view; lumen's ArrayBuffers own their storage, so this
/// snapshots the bytes — reads match, writes do not propagate back to native memory.)
pub fn op_to_array_buffer(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let base = ptr_num_arg(ctx, args, 0)?;
    let off = ctx.coerce_number(args.get(1).unwrap_or(&Value::Num(0.0)))? as usize;
    let len = ctx.coerce_number(args.get(2).unwrap_or(&Value::Num(0.0)))? as usize;
    let bytes: Vec<u8> = if base == 0 || len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts((base as usize + off) as *const u8, len).to_vec() }
    };
    let ta = ctx.make_uint8array(&bytes)?;
    ctx.member_get(&ta, "buffer")
}

/// `__ffi.toBuffer(ptr, byteOffset, byteLength)` — like [`op_to_array_buffer`] but a Uint8Array
/// copy (the JS glue rewraps it as a Node `Buffer`).
pub fn op_to_buffer(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let base = ptr_num_arg(ctx, args, 0)?;
    let off = ctx.coerce_number(args.get(1).unwrap_or(&Value::Num(0.0)))? as usize;
    let len = ctx.coerce_number(args.get(2).unwrap_or(&Value::Num(0.0)))? as usize;
    let bytes: Vec<u8> = if base == 0 || len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts((base as usize + off) as *const u8, len).to_vec() }
    };
    ctx.make_uint8array(&bytes)
}

/// `__ffi.registerCallback(fn, [argCodes], retCode)` → `[thunkPtr, id]`, or an honest throw for
/// signatures the pooled integer-only thunks can't serve.
pub fn op_register_callback(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let func = args.first().cloned().unwrap_or(Value::Undefined);
    if !func.is_callable() {
        return Err(type_error(ctx, "bun:ffi JSCallback expects a function"));
    }
    let codes_v = args.get(1).cloned().unwrap_or(Value::Undefined);
    let ret_code = ctx.coerce_number(args.get(2).unwrap_or(&Value::Num(f64::from(T_VOID))))? as u8;

    let n = arr_len(ctx, &codes_v);
    if n > MAX_ARGS {
        return Err(plain_error(
            ctx,
            format!("bun:ffi JSCallback with {n} arguments is not supported in lumen (max {MAX_ARGS})"),
        ));
    }
    let mut arg_codes = Vec::with_capacity(n);
    for i in 0..n {
        let code = ctx.member_get(&codes_v, &i.to_string())?.as_num_opt().unwrap_or(0.0) as u8;
        if is_float(code) {
            return Err(plain_error(
                ctx,
                "bun:ffi JSCallback with floating-point arguments is not supported in lumen",
            ));
        }
        arg_codes.push(code);
    }
    if is_float(ret_code) {
        return Err(plain_error(
            ctx,
            "bun:ffi JSCallback with a floating-point return is not supported in lumen",
        ));
    }

    let st = ffi_state(ctx);
    let slot = st.callbacks[n].iter().position(|c| c.is_none());
    let Some(k) = slot else {
        return Err(plain_error(
            ctx,
            format!("bun:ffi JSCallback pool exhausted ({JSCB_POOL} live callbacks of arity {n})"),
        ));
    };
    st.callbacks[n][k] = Some(CbEntry { func, arg_codes, ret_code });
    let ptr = jscb_thunk_ptr(n, k) as usize as f64;
    let id = (n * JSCB_POOL + k) as f64;
    Ok(ctx.make_array(vec![Value::Num(ptr), Value::Num(id)]))
}

/// `__ffi.unregisterCallback(id)` — free a JSCallback slot.
pub fn op_unregister_callback(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let id = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))? as usize;
    let (n, k) = (id / JSCB_POOL, id % JSCB_POOL);
    let st = ffi_state(ctx);
    if let Some(row) = st.callbacks.get_mut(n) {
        if let Some(slot) = row.get_mut(k) {
            *slot = None;
        }
    }
    Ok(Value::Undefined)
}

/// Forward a native invocation of thunk `(n, k)` into its registered JS function. Called from the
/// generated `extern "C"` thunks; re-enters the engine via the thread-local call context.
fn jscb_dispatch(n: usize, k: usize, raw_args: &[u64]) -> u64 {
    let ptr = CURRENT_CTX.with(|c| c.get());
    if ptr.is_null() {
        // No live call context: a foreign thread invoked us. We cannot safely touch the
        // single-threaded engine, so decline rather than fault.
        return 0;
    }
    // SAFETY: single-threaded engine; the outgoing native call in `op_call` is on this same stack
    // and is not using `&mut Ctx` while the callee runs (mirrors the N-API re-entry contract).
    let ctx = unsafe { &mut *ptr };

    let (func, arg_codes, ret_code) = {
        let st = ffi_state(ctx);
        match st.callbacks.get(n).and_then(|r| r.get(k)).and_then(|c| c.as_ref()) {
            Some(e) => (e.func.clone(), e.arg_codes.clone(), e.ret_code),
            None => return 0,
        }
    };

    let mut js_args = Vec::with_capacity(arg_codes.len());
    for (i, &code) in arg_codes.iter().enumerate() {
        let raw = raw_args.get(i).copied().unwrap_or(0);
        js_args.push(cb_arg_to_value(raw, code));
    }

    match ctx.invoke(func, Value::Undefined, &js_args) {
        Ok(v) => cb_return_to_raw(ctx, &v, ret_code),
        Err(e) => {
            let msg = ctx.coerce_string(&e).map(|s| s.to_string()).unwrap_or_default();
            eprintln!("uncaught exception in bun:ffi JSCallback: {msg}");
            0
        }
    }
}

/// Present a raw integer-register argument to the JS callback as Bun would: pointers as numbers
/// (0 ⇒ null), 64-bit ints as BigInt, narrower ints as numbers, bool as boolean.
fn cb_arg_to_value(raw: u64, code: u8) -> Value {
    if is_pointer(code) {
        return if raw == 0 { Value::Null } else { Value::Num(raw as f64) };
    }
    match code {
        T_CHAR | T_I8 => Value::Num((raw as u8 as i8) as f64),
        T_U8 => Value::Num((raw as u8) as f64),
        T_I16 => Value::Num((raw as u16 as i16) as f64),
        T_U16 => Value::Num((raw as u16) as f64),
        T_I32 => Value::Num((raw as u32 as i32) as f64),
        T_U32 => Value::Num((raw as u32) as f64),
        T_BOOL => Value::Bool(raw & 1 != 0),
        T_I64 | T_I64_FAST => Value::bigint_from_i128((raw as i64) as i128),
        T_U64 | T_U64_FAST => Value::bigint_from_u64(raw),
        _ => Value::Num(raw as f64),
    }
}

/// Reduce a JS callback's return value to the raw register bits the native caller expects.
fn cb_return_to_raw(ctx: &mut Ctx, v: &Value, ret_code: u8) -> u64 {
    if ret_code == T_VOID {
        return 0;
    }
    if is_pointer(ret_code) {
        return value_to_pointer(ctx, v).unwrap_or(0);
    }
    match v {
        Value::BigInt(b) => b.to_i128_wrapping() as u64,
        Value::Bool(b) => *b as u64,
        _ => ctx.coerce_number(v).map(|n| n as i64 as u64).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    //! Exercise the generated trampolines directly against real libc symbols — no engine needed.
    //! This validates the ABI-class dispatch (including the integers-first reordering) on the host
    //! architecture, which is the whole point of the monomorphization.
    use super::*;
    use crate::dylib::DynLib;

    /// The system C (and math) library that carries `strlen`/`pow`/`ldexp`/`getpid`.
    fn libc() -> DynLib {
        #[cfg(target_os = "macos")]
        let path = "libSystem.B.dylib";
        #[cfg(target_os = "linux")]
        let path = "libm.so.6"; // pulls in libc's symbols too via the loader
        #[cfg(windows)]
        let path = "msvcrt.dll";
        DynLib::open(path).expect("open system C library")
    }

    fn sym(lib: &DynLib, name: &str) -> *const c_void {
        lib.symbol(name).unwrap_or_else(|| panic!("resolve {name}")) as *const c_void
    }

    #[test]
    fn call_int_strlen() {
        let lib = libc();
        let f = sym(&lib, "strlen");
        let s = std::ffi::CString::new("hello, ffi").unwrap();
        let n = unsafe { call_int(f, &[s.as_ptr() as u64], &[]) };
        assert_eq!(n, 10);
    }

    #[test]
    fn call_int_no_args_getpid() {
        let lib = libc();
        // getpid lives in libc; on linux libm forwards, on macos it's in libSystem.
        let f = match lib.symbol("getpid") {
            Some(p) => p as *const c_void,
            None => return, // platform without a directly-resolvable getpid: skip
        };
        let pid = unsafe { call_int(f, &[], &[]) } as i32;
        assert_eq!(pid, std::process::id() as i32);
    }

    #[test]
    fn call_f64_pow_all_float() {
        let lib = libc();
        let f = sym(&lib, "pow");
        let r = unsafe { call_f64(f, &[], &[FArg::F64(2.0), FArg::F64(10.0)]) };
        assert_eq!(r, 1024.0);
    }

    #[test]
    fn call_f64_ldexp_reorders_int_ahead_of_float() {
        // ldexp(double x, int exp) mixes classes: canonical dispatch must place `exp` in an integer
        // register and `x` in a float register regardless of their source order. ldexp(3.0, 4) = 48.
        let lib = libc();
        let f = sym(&lib, "ldexp");
        let r = unsafe { call_f64(f, &[4u64], &[FArg::F64(3.0)]) };
        assert_eq!(r, 48.0);
    }

    #[test]
    fn call_int_masks_narrow_return() {
        // labs(-5) -> 5 (i64). Verifies a signed integer return round-trips through the u64 path.
        let lib = libc();
        let f = sym(&lib, "labs");
        let r = unsafe { call_int(f, &[(-5i64) as u64], &[]) } as i64;
        assert_eq!(r, 5);
    }

    #[test]
    fn fmask_encodes_widths() {
        assert_eq!(fmask(&[]), 0);
        assert_eq!(fmask(&[FArg::F32(0.0)]), 0);
        assert_eq!(fmask(&[FArg::F64(0.0)]), 1);
        assert_eq!(fmask(&[FArg::F32(0.0), FArg::F64(0.0), FArg::F32(0.0)]), 0b010);
    }
}
