//! N-API (Node-API) host implementation — enough of the stable C ABI to load and run Node
//! native addons (`.node` files), with no third-party crate.
//!
//! # How it fits together
//!
//! A `.node` addon is a shared library. [`crate::dylib::DynLib`] `dlopen`s it and resolves its
//! `napi_register_module_v1` entry point; [`op_load_addon`] calls that entry with a fresh
//! [`Env`] and an empty exports object, and returns whatever the addon populated.
//!
//! The addon, in turn, calls back into the `napi_*` C functions defined below. Those symbols are
//! exported from the host executable (lumen-cli is linked with `-export_dynamic` / `-rdynamic`),
//! so the dynamic linker resolves the addon's undefined `napi_*` references against them — the
//! same mechanism the real `node` binary uses. [`keep_napi_exports`] takes each symbol's address
//! so the linker cannot drop them from the static archive.
//!
//! # The handle model
//!
//! - [`napi_env`] is a pointer to an [`Env`], which carries a raw pointer back to the engine
//!   ([`Ctx`]) plus a handle-scope arena of boxed [`Value`]s.
//! - [`napi_value`] is a pointer to one of those boxed `Value`s. Boxing gives each value a stable
//!   address even as the arena grows; the arena keeps it alive for the duration of the call.
//! - [`napi_callback_info`] is a pointer to a [`CbInfo`] holding a native call's `this`, args, and
//!   `data`.
//!
//! # Safety
//!
//! This module executes third-party machine code and speaks a raw C ABI, so it is unavoidably
//! `unsafe`. The engine is single-threaded and `!Send`; every N-API call happens synchronously on
//! the loop thread while the engine is live, so recovering `&mut Ctx` from the stored pointer is
//! sound as long as addons never stash an `env` for later or hand it to another thread (neither is
//! permitted by the N-API contract).

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};
use std::rc::Rc;

use lumen_host::{Ctx, NativeClosure, Value};

use crate::dylib::DynLib;

// ---- ABI handle types ------------------------------------------------------------------------

/// `napi_env` — passed to every N-API call; opaque to the addon.
pub type napi_env = *mut Env;
/// `napi_value` — an opaque handle to a JS value (internally `*const Value` into the env arena).
pub type napi_value = *mut c_void;
/// `napi_callback_info` — opaque handle to an in-flight native call's arguments.
pub type napi_callback_info = *mut CbInfo;
/// `napi_status` — `0` (`napi_ok`) on success.
pub type napi_status = c_int;
/// `napi_handle_scope` — opaque; our arena makes scopes no-ops, so this is a non-null token.
pub type napi_handle_scope = *mut c_void;
/// The addon's C callback: `(env, info) -> return value`.
pub type napi_callback =
    Option<unsafe extern "C" fn(napi_env, napi_callback_info) -> napi_value>;

// napi_status values (subset of the C enum).
const NAPI_OK: napi_status = 0;
const NAPI_INVALID_ARG: napi_status = 1;
const NAPI_NUMBER_EXPECTED: napi_status = 6;
const NAPI_BOOLEAN_EXPECTED: napi_status = 7;
const NAPI_PENDING_EXCEPTION: napi_status = 10;

// napi_valuetype values (the C enum returned by napi_typeof).
const NAPI_UNDEFINED: c_int = 0;
const NAPI_NULL: c_int = 1;
const NAPI_BOOLEAN: c_int = 2;
const NAPI_NUMBER: c_int = 3;
const NAPI_STRING: c_int = 4;
const NAPI_SYMBOL: c_int = 5;
const NAPI_OBJECT: c_int = 6;
const NAPI_FUNCTION: c_int = 7;
const NAPI_BIGINT: c_int = 9;

/// `NAPI_AUTO_LENGTH` — a length argument meaning "the string is NUL-terminated".
const NAPI_AUTO_LENGTH: usize = usize::MAX;

// ---- env / handle machinery ------------------------------------------------------------------

/// The concrete backing of an [`napi_env`]: a pointer to the engine plus a per-call arena of
/// boxed values that keeps every [`napi_value`] alive for the duration of the call.
pub struct Env {
    interp: *mut Ctx,
    // The `Box` is load-bearing, not redundant: a napi_value is a raw pointer into this arena, so
    // each value needs a stable heap address that survives the Vec growing. (clippy::vec_box.)
    #[allow(clippy::vec_box)]
    handles: Vec<Box<Value>>,
    /// An exception raised by the addon (`napi_throw*`), surfaced to JS once the call returns.
    pending: Option<Value>,
}

impl Env {
    fn new(interp: *mut Ctx) -> Env {
        Env { interp, handles: Vec::new(), pending: None }
    }

    /// Box a value into the arena and return a stable `napi_value` handle to it.
    fn handle(&mut self, v: Value) -> napi_value {
        let boxed = Box::new(v);
        let ptr = (&*boxed as *const Value) as *mut c_void;
        self.handles.push(boxed);
        ptr
    }

    /// Recover `&mut Ctx`. Unsafe: relies on the call happening on the engine's loop thread.
    #[allow(clippy::mut_from_ref)]
    unsafe fn interp<'a>(&self) -> &'a mut Ctx {
        &mut *self.interp
    }
}

/// A native call's arguments, behind an [`napi_callback_info`].
pub struct CbInfo {
    this: Value,
    args: Vec<Value>,
    data: *mut c_void,
}

/// Clone the `Value` behind a handle. A null handle reads as `undefined`.
unsafe fn value_of(v: napi_value) -> Value {
    if v.is_null() {
        Value::Undefined
    } else {
        (*(v as *const Value)).clone()
    }
}

/// Read a possibly-NUL-terminated (`NAPI_AUTO_LENGTH`) or length-counted UTF-8 C string.
unsafe fn read_utf8(s: *const c_char, len: usize) -> String {
    if s.is_null() {
        return String::new();
    }
    let bytes: &[u8] = if len == NAPI_AUTO_LENGTH {
        std::ffi::CStr::from_ptr(s).to_bytes()
    } else {
        std::slice::from_raw_parts(s as *const u8, len)
    };
    String::from_utf8_lossy(bytes).into_owned()
}

/// ECMAScript ToInt32 — the coercion `napi_get_value_int32` applies to a non-integral number.
fn to_int32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    let m = n.trunc().rem_euclid(4294967296.0); // mod 2^32, always in [0, 2^32)
    m as u32 as i32
}

// ---- the JS-function trampoline --------------------------------------------------------------

/// Wrap an addon C callback (`cb` + its `data`) as a lumen function value. When JS calls it, a
/// fresh [`Env`] and [`CbInfo`] are built and the C callback is invoked.
fn make_callback_fn(interp: &mut Ctx, name: &str, cb: napi_callback, data: *mut c_void) -> Value {
    // The raw C pointers are Copy and move into a 'static closure. They are not `Send`, but a
    // NativeClosure runs only on the engine's own `!Send` loop thread, so that is fine.
    let closure = move |ip: &mut Ctx, this: Value, args: &[Value]| -> Result<Value, Value> {
        invoke_napi_callback(ip, cb, data, this, args)
    };
    interp.new_native_fn(name, 0, Rc::new(closure) as Rc<NativeClosure>)
}

/// Drive one call into an addon C callback, translating its return / thrown exception back into
/// the engine's `Result<Value, Value>`.
fn invoke_napi_callback(
    ip: &mut Ctx,
    cb: napi_callback,
    data: *mut c_void,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let cb = match cb {
        Some(f) => f,
        None => return Ok(Value::Undefined),
    };
    let mut env = Env::new(ip as *mut Ctx);
    let mut info = CbInfo { this, args: args.to_vec(), data };
    let ret = unsafe { cb(&mut env as *mut Env, &mut info as *mut CbInfo) };
    if let Some(err) = env.pending.take() {
        return Err(err);
    }
    Ok(unsafe { value_of(ret) })
}

// ---- the loader op ---------------------------------------------------------------------------

/// Loaded native libraries, kept alive for the process lifetime: an addon's function pointers
/// dangle the moment its library is `dlclose`d, and (like Node) addons are never unloaded.
#[derive(Default)]
pub struct AddonRegistry {
    libs: Vec<DynLib>,
}

/// Persistent N-API state that outlives individual calls: references (values kept alive past a
/// handle scope), object wraps (native pointers attached to JS objects), promise deferreds, and
/// registered async work. Lives in `OpState`, reached from an [`Env`] via the engine pointer.
#[derive(Default)]
struct NapiState {
    refs: Vec<Option<RefEntry>>,
    ref_free: Vec<usize>,
    /// object heap-address → wrapped native pointer (`napi_wrap`). Finalizers are not run:
    /// memory is reclaimed at process exit, which is sufficient for a run-and-exit tool.
    wraps: std::collections::HashMap<usize, *mut c_void>,
    deferreds: Vec<Option<Deferred>>,
    async_works: Vec<Option<AsyncWork>>,
}

struct RefEntry {
    value: Value,
    count: u32,
}

struct Deferred {
    resolve: Value,
    reject: Value,
}

struct AsyncWork {
    execute: napi_async_execute_callback,
    complete: napi_async_complete_callback,
    data: *mut c_void,
}

/// `napi_ref` — an opaque handle to a reference; internally `index + 1` (non-null) into
/// `NapiState::refs`.
pub type napi_ref = *mut c_void;
/// `napi_deferred` — the resolve/reject side of a promise; `index + 1` into `NapiState::deferreds`.
pub type napi_deferred = *mut c_void;
/// `napi_async_work` — a registered unit of async work; `index + 1` into `NapiState::async_works`.
pub type napi_async_work = *mut c_void;
/// `napi_threadsafe_function` — opaque; stubbed (see the threadsafe-function functions).
pub type napi_threadsafe_function = *mut c_void;

/// The off-thread half of an async work item: `(env, data)`. Must not touch JS.
pub type napi_async_execute_callback = Option<unsafe extern "C" fn(napi_env, *mut c_void)>;
/// The on-loop-thread half of an async work item: `(env, status, data)`.
pub type napi_async_complete_callback =
    Option<unsafe extern "C" fn(napi_env, napi_status, *mut c_void)>;

/// A property descriptor for `napi_define_class` / `napi_define_properties` (C ABI layout).
#[repr(C)]
pub struct napi_property_descriptor {
    utf8name: *const c_char,
    name: napi_value,
    method: napi_callback,
    getter: napi_callback,
    setter: napi_callback,
    value: napi_value,
    attributes: c_int,
    data: *mut c_void,
}

/// `napi_static` attribute bit — the property lives on the constructor, not the prototype.
const NAPI_STATIC: c_int = 1 << 10;
/// `napi_enumerable` attribute bit.
const NAPI_ENUMERABLE: c_int = 1 << 1;

impl Env {
    /// The process-wide N-API state, lazily installed in `OpState`.
    #[allow(clippy::mut_from_ref)]
    unsafe fn napi_state(&self) -> &mut NapiState {
        let os = (*self.interp).op_state();
        if os.get::<NapiState>().is_none() {
            os.put(NapiState::default());
        }
        os.get_mut::<NapiState>().unwrap()
    }
}

/// Encode a `NapiState` slot index as a non-null opaque handle.
fn encode_slot(idx: usize) -> *mut c_void {
    (idx + 1) as *mut c_void
}

/// Decode an opaque handle back to a slot index (`None` for a null handle).
fn decode_slot(handle: *mut c_void) -> Option<usize> {
    if handle.is_null() {
        None
    } else {
        Some(handle as usize - 1)
    }
}

/// `__node.loadNativeAddon(path)` — dlopen a `.node` addon, run its N-API registration, and
/// return the exports object.
pub fn op_load_addon(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    keep_napi_exports();

    let path = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();

    let lib = DynLib::open(&path)
        .map_err(|e| ctx.make_error("Error", format!("cannot load native addon '{path}': {e}")))?;
    let sym = lib.symbol("napi_register_module_v1").ok_or_else(|| {
        ctx.make_error(
            "Error",
            format!("'{path}' is not an N-API addon (no napi_register_module_v1)"),
        )
    })?;

    type Register = unsafe extern "C" fn(napi_env, napi_value) -> napi_value;
    let register: Register = unsafe { std::mem::transmute::<*mut c_void, Register>(sym) };

    // Build the exports object and the call env, then hand control to the addon.
    let exports = Value::Obj(ctx.new_object());
    let mut env = Env::new(ctx as *mut Ctx);
    let exports_handle = env.handle(exports.clone());
    let ret = unsafe { register(&mut env as *mut Env, exports_handle) };
    let pending = env.pending.take();
    let result = if ret.is_null() { exports } else { unsafe { value_of(ret) } };

    // Retain the library so its code stays mapped for the process lifetime.
    if ctx.host_mut::<AddonRegistry>().is_none() {
        ctx.op_state().put(AddonRegistry::default());
    }
    ctx.host_mut::<AddonRegistry>().unwrap().libs.push(lib);

    match pending {
        Some(err) => Err(err),
        None => Ok(result),
    }
}

// ---- napi_* C ABI surface --------------------------------------------------------------------
//
// Every function here is `#[no_mangle] extern "C"` so a dlopened addon resolves it. They are the
// host side of the N-API contract; see the module docs for the handle model. Unsupported N-API
// functions are simply absent, so an addon that needs one fails loudly at `dlopen` (RTLD_NOW)
// naming the missing symbol, rather than misbehaving at runtime.

// -- value creation --

#[no_mangle]
pub unsafe extern "C" fn napi_get_undefined(env: napi_env, result: *mut napi_value) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Undefined);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_null(env: napi_env, result: *mut napi_value) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Null);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_global(env: napi_env, result: *mut napi_value) -> napi_status {
    let env = &mut *env;
    let g = env.interp().global_this();
    *result = env.handle(g);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_boolean(
    env: napi_env,
    value: bool,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Bool(value));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_double(
    env: napi_env,
    value: f64,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Num(value));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_int32(
    env: napi_env,
    value: i32,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Num(value as f64));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_uint32(
    env: napi_env,
    value: u32,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Num(value as f64));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_int64(
    env: napi_env,
    value: i64,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    *result = env.handle(Value::Num(value as f64));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_string_utf8(
    env: napi_env,
    s: *const c_char,
    len: usize,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let string = read_utf8(s, len);
    *result = env.handle(Value::from_string(string));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_object(env: napi_env, result: *mut napi_value) -> napi_status {
    let env = &mut *env;
    let o = Value::Obj(env.interp().new_object());
    *result = env.handle(o);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_array(env: napi_env, result: *mut napi_value) -> napi_status {
    let env = &mut *env;
    let a = env.interp().make_array(Vec::new());
    *result = env.handle(a);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_array_with_length(
    env: napi_env,
    length: usize,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let a = env.interp().make_array(vec![Value::Undefined; length]);
    *result = env.handle(a);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_function(
    env: napi_env,
    utf8name: *const c_char,
    length: usize,
    cb: napi_callback,
    data: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let name = read_utf8(utf8name, length);
    let f = make_callback_fn(env.interp(), &name, cb, data);
    *result = env.handle(f);
    NAPI_OK
}

// -- value inspection / extraction --

#[no_mangle]
pub unsafe extern "C" fn napi_typeof(
    env: napi_env,
    value: napi_value,
    result: *mut c_int,
) -> napi_status {
    let _ = env;
    let v = value_of(value);
    let t = if matches!(v, Value::Null) {
        NAPI_NULL
    } else {
        match v.type_of() {
            "undefined" => NAPI_UNDEFINED,
            "boolean" => NAPI_BOOLEAN,
            "number" => NAPI_NUMBER,
            "string" => NAPI_STRING,
            "symbol" => NAPI_SYMBOL,
            "bigint" => NAPI_BIGINT,
            "function" => NAPI_FUNCTION,
            _ => NAPI_OBJECT,
        }
    };
    *result = t;
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_value_double(
    env: napi_env,
    value: napi_value,
    result: *mut f64,
) -> napi_status {
    let _ = env;
    match value_of(value).as_num_opt() {
        Some(n) => {
            *result = n;
            NAPI_OK
        }
        None => NAPI_NUMBER_EXPECTED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_value_int32(
    env: napi_env,
    value: napi_value,
    result: *mut i32,
) -> napi_status {
    let _ = env;
    match value_of(value).as_num_opt() {
        Some(n) => {
            *result = to_int32(n);
            NAPI_OK
        }
        None => NAPI_NUMBER_EXPECTED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_value_uint32(
    env: napi_env,
    value: napi_value,
    result: *mut u32,
) -> napi_status {
    let _ = env;
    match value_of(value).as_num_opt() {
        Some(n) => {
            *result = to_int32(n) as u32;
            NAPI_OK
        }
        None => NAPI_NUMBER_EXPECTED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_value_bool(
    env: napi_env,
    value: napi_value,
    result: *mut bool,
) -> napi_status {
    let _ = env;
    match value_of(value) {
        Value::Bool(b) => {
            *result = b;
            NAPI_OK
        }
        _ => NAPI_BOOLEAN_EXPECTED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_value_string_utf8(
    env: napi_env,
    value: napi_value,
    buf: *mut c_char,
    bufsize: usize,
    result: *mut usize,
) -> napi_status {
    let env = &mut *env;
    let v = value_of(value);
    let s = match env.interp().coerce_string(&v) {
        Ok(s) => s,
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };
    let bytes = s.as_bytes();
    if buf.is_null() {
        // Measurement mode: report the byte length (excluding the NUL).
        if !result.is_null() {
            *result = bytes.len();
        }
        return NAPI_OK;
    }
    if bufsize == 0 {
        if !result.is_null() {
            *result = 0;
        }
        return NAPI_OK;
    }
    let n = core::cmp::min(bytes.len(), bufsize - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
    *buf.add(n) = 0;
    if !result.is_null() {
        *result = n;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_coerce_to_string(
    env: napi_env,
    value: napi_value,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let v = value_of(value);
    match env.interp().coerce_string(&v) {
        Ok(s) => {
            *result = env.handle(Value::str(s));
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

// -- properties --

#[no_mangle]
pub unsafe extern "C" fn napi_set_named_property(
    env: napi_env,
    object: napi_value,
    utf8name: *const c_char,
    value: napi_value,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    let name = read_utf8(utf8name, NAPI_AUTO_LENGTH);
    match env.interp().member_set(&obj, &name, value_of(value)) {
        Ok(()) => NAPI_OK,
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_named_property(
    env: napi_env,
    object: napi_value,
    utf8name: *const c_char,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    let name = read_utf8(utf8name, NAPI_AUTO_LENGTH);
    match env.interp().member_get(&obj, &name) {
        Ok(v) => {
            *result = env.handle(v);
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_has_named_property(
    env: napi_env,
    object: napi_value,
    utf8name: *const c_char,
    result: *mut bool,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    let name = read_utf8(utf8name, NAPI_AUTO_LENGTH);
    match env.interp().member_get(&obj, &name) {
        Ok(v) => {
            *result = !matches!(v, Value::Undefined);
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_set_property(
    env: napi_env,
    object: napi_value,
    key: napi_value,
    value: napi_value,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    let k = match env.interp().coerce_string(&value_of(key)) {
        Ok(k) => k.to_string(),
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };
    match env.interp().member_set(&obj, &k, value_of(value)) {
        Ok(()) => NAPI_OK,
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_property(
    env: napi_env,
    object: napi_value,
    key: napi_value,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    let k = match env.interp().coerce_string(&value_of(key)) {
        Ok(k) => k.to_string(),
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };
    match env.interp().member_get(&obj, &k) {
        Ok(v) => {
            *result = env.handle(v);
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_set_element(
    env: napi_env,
    object: napi_value,
    index: u32,
    value: napi_value,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    match env.interp().member_set(&obj, &index.to_string(), value_of(value)) {
        Ok(()) => NAPI_OK,
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_element(
    env: napi_env,
    object: napi_value,
    index: u32,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(object);
    match env.interp().member_get(&obj, &index.to_string()) {
        Ok(v) => {
            *result = env.handle(v);
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

// -- callbacks / calls --

#[no_mangle]
pub unsafe extern "C" fn napi_get_cb_info(
    env: napi_env,
    cbinfo: napi_callback_info,
    argc: *mut usize,
    argv: *mut napi_value,
    this_arg: *mut napi_value,
    data: *mut *mut c_void,
) -> napi_status {
    let env = &mut *env;
    let info = &*cbinfo;
    if !argc.is_null() {
        let cap = *argc;
        if !argv.is_null() {
            for i in 0..cap {
                let v = info.args.get(i).cloned().unwrap_or(Value::Undefined);
                *argv.add(i) = env.handle(v);
            }
        }
        // Report the true argument count (may exceed the caller's buffer capacity).
        *argc = info.args.len();
    }
    if !this_arg.is_null() {
        *this_arg = env.handle(info.this.clone());
    }
    if !data.is_null() {
        *data = info.data;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_call_function(
    env: napi_env,
    recv: napi_value,
    func: napi_value,
    argc: usize,
    argv: *const napi_value,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let recv = value_of(recv);
    let func = value_of(func);
    let mut args = Vec::with_capacity(argc);
    for i in 0..argc {
        args.push(value_of(*argv.add(i)));
    }
    match env.interp().invoke(func, recv, &args) {
        Ok(v) => {
            if !result.is_null() {
                *result = env.handle(v);
            }
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

// -- errors / exceptions --

#[no_mangle]
pub unsafe extern "C" fn napi_throw(env: napi_env, error: napi_value) -> napi_status {
    let env = &mut *env;
    env.pending = Some(value_of(error));
    NAPI_OK
}

/// Shared body for the `napi_throw_*_error` family.
unsafe fn throw_named(env: napi_env, kind: &str, msg: *const c_char) -> napi_status {
    let env = &mut *env;
    let message = read_utf8(msg, NAPI_AUTO_LENGTH);
    let err = env.interp().make_error(kind, message);
    env.pending = Some(err);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_throw_error(
    env: napi_env,
    _code: *const c_char,
    msg: *const c_char,
) -> napi_status {
    throw_named(env, "Error", msg)
}

#[no_mangle]
pub unsafe extern "C" fn napi_throw_type_error(
    env: napi_env,
    _code: *const c_char,
    msg: *const c_char,
) -> napi_status {
    throw_named(env, "TypeError", msg)
}

#[no_mangle]
pub unsafe extern "C" fn napi_throw_range_error(
    env: napi_env,
    _code: *const c_char,
    msg: *const c_char,
) -> napi_status {
    throw_named(env, "RangeError", msg)
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_error(
    env: napi_env,
    _code: napi_value,
    msg: napi_value,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let message = env
        .interp()
        .coerce_string(&value_of(msg))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let err = env.interp().make_error("Error", message);
    *result = env.handle(err);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_is_exception_pending(
    env: napi_env,
    result: *mut bool,
) -> napi_status {
    let env = &mut *env;
    *result = env.pending.is_some();
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_and_clear_last_exception(
    env: napi_env,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let v = env.pending.take().unwrap_or(Value::Undefined);
    *result = env.handle(v);
    NAPI_OK
}

// -- handle scopes (no-ops: our per-call arena keeps every handle alive) --

#[no_mangle]
pub unsafe extern "C" fn napi_open_handle_scope(
    env: napi_env,
    result: *mut napi_handle_scope,
) -> napi_status {
    let _ = env;
    if !result.is_null() {
        // A non-null token; addons only compare it, they don't dereference it.
        *result = 1 as *mut c_void;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_close_handle_scope(
    env: napi_env,
    scope: napi_handle_scope,
) -> napi_status {
    let _ = (env, scope);
    NAPI_OK
}

// -- misc --

#[no_mangle]
pub unsafe extern "C" fn napi_get_version(env: napi_env, result: *mut u32) -> napi_status {
    let _ = env;
    // Node-API version this host targets.
    *result = 8;
    NAPI_OK
}

// -- value predicates --

#[no_mangle]
pub unsafe extern "C" fn napi_strict_equals(
    env: napi_env,
    lhs: napi_value,
    rhs: napi_value,
    result: *mut bool,
) -> napi_status {
    let env = &mut *env;
    *result = env
        .interp()
        .values_strict_equal(&value_of(lhs), &value_of(rhs));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_is_error(
    env: napi_env,
    value: napi_value,
    result: *mut bool,
) -> napi_status {
    let env = &mut *env;
    *result = env.interp().is_error_value(&value_of(value));
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_prototype(
    env: napi_env,
    object: napi_value,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let proto = env.interp().prototype_of(&value_of(object));
    *result = env.handle(proto);
    NAPI_OK
}

// -- references (values kept alive beyond the handle scope) --

#[no_mangle]
pub unsafe extern "C" fn napi_create_reference(
    env: napi_env,
    value: napi_value,
    initial_refcount: u32,
    result: *mut napi_ref,
) -> napi_status {
    let env = &mut *env;
    let v = value_of(value);
    let state = env.napi_state();
    let entry = RefEntry { value: v, count: initial_refcount };
    let idx = match state.ref_free.pop() {
        Some(i) => {
            state.refs[i] = Some(entry);
            i
        }
        None => {
            state.refs.push(Some(entry));
            state.refs.len() - 1
        }
    };
    *result = encode_slot(idx);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_delete_reference(env: napi_env, r: napi_ref) -> napi_status {
    let env = &mut *env;
    if let Some(idx) = decode_slot(r) {
        let state = env.napi_state();
        if idx < state.refs.len() && state.refs[idx].take().is_some() {
            state.ref_free.push(idx);
        }
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_reference_value(
    env: napi_env,
    r: napi_ref,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let v = decode_slot(r)
        .and_then(|idx| env.napi_state().refs.get(idx))
        .and_then(|slot| slot.as_ref())
        .map(|e| e.value.clone())
        .unwrap_or(Value::Undefined);
    *result = env.handle(v);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_reference_ref(
    env: napi_env,
    r: napi_ref,
    result: *mut u32,
) -> napi_status {
    let env = &mut *env;
    let mut count = 0;
    if let Some(idx) = decode_slot(r) {
        if let Some(Some(e)) = env.napi_state().refs.get_mut(idx) {
            e.count += 1;
            count = e.count;
        }
    }
    if !result.is_null() {
        *result = count;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_reference_unref(
    env: napi_env,
    r: napi_ref,
    result: *mut u32,
) -> napi_status {
    let env = &mut *env;
    let mut count = 0;
    if let Some(idx) = decode_slot(r) {
        if let Some(Some(e)) = env.napi_state().refs.get_mut(idx) {
            e.count = e.count.saturating_sub(1);
            count = e.count;
        }
    }
    if !result.is_null() {
        *result = count;
    }
    NAPI_OK
}

// -- object wrapping (attach a native pointer to a JS object) --

#[no_mangle]
pub unsafe extern "C" fn napi_wrap(
    env: napi_env,
    js_object: napi_value,
    native_object: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut napi_ref,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(js_object);
    let Some(addr) = env.interp().object_addr(&obj) else {
        return NAPI_INVALID_ARG;
    };
    env.napi_state().wraps.insert(addr, native_object);
    // Optionally hand back a reference to the wrapped object.
    if !result.is_null() {
        let mut r: napi_ref = std::ptr::null_mut();
        napi_create_reference(env, js_object, 0, &mut r);
        *result = r;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_unwrap(
    env: napi_env,
    js_object: napi_value,
    result: *mut *mut c_void,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(js_object);
    let Some(addr) = env.interp().object_addr(&obj) else {
        return NAPI_INVALID_ARG;
    };
    *result = env.napi_state().wraps.get(&addr).copied().unwrap_or(std::ptr::null_mut());
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_remove_wrap(
    env: napi_env,
    js_object: napi_value,
    result: *mut *mut c_void,
) -> napi_status {
    let env = &mut *env;
    let obj = value_of(js_object);
    let Some(addr) = env.interp().object_addr(&obj) else {
        return NAPI_INVALID_ARG;
    };
    let data = env.napi_state().wraps.remove(&addr).unwrap_or(std::ptr::null_mut());
    if !result.is_null() {
        *result = data;
    }
    NAPI_OK
}

// -- classes --

#[no_mangle]
pub unsafe extern "C" fn napi_define_class(
    env: napi_env,
    utf8name: *const c_char,
    length: usize,
    constructor: napi_callback,
    data: *mut c_void,
    property_count: usize,
    properties: *const napi_property_descriptor,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let name = read_utf8(utf8name, length);
    let interp = env.interp();

    // The prototype instances inherit from, captured by the constructor trampoline so `new`
    // produces an object with the right shape before the C constructor wraps native state onto it.
    let proto = Value::Obj(interp.new_object());
    let proto_for_ctor = proto.clone();
    let closure = move |ip: &mut Ctx, this: Value, args: &[Value]| -> Result<Value, Value> {
        // Under `new`, the construct path passes `this = undefined`; build the instance. A
        // `super()` call passes an existing `this` to graft onto.
        let instance = match this {
            Value::Undefined => ip.new_object_with_proto(&proto_for_ctor),
            other => other,
        };
        invoke_napi_callback(ip, constructor, data, instance.clone(), args)?;
        Ok(instance)
    };
    let ctor = interp.new_native_fn(&name, 0, Rc::new(closure) as Rc<NativeClosure>);
    interp.set_constructor_prototype(&ctor, &proto);

    // Attach each declared method / accessor / value to the prototype (or the constructor, for
    // `napi_static` members).
    for i in 0..property_count {
        let d = &*properties.add(i);
        let target = if d.attributes & NAPI_STATIC != 0 { ctor.clone() } else { proto.clone() };
        let pname = if !d.utf8name.is_null() {
            read_utf8(d.utf8name, NAPI_AUTO_LENGTH)
        } else {
            env.interp()
                .coerce_string(&value_of(d.name))
                .map(|s| s.to_string())
                .unwrap_or_default()
        };
        let enumerable = d.attributes & NAPI_ENUMERABLE != 0;

        if d.method.is_some() {
            let f = make_callback_fn(env.interp(), &pname, d.method, d.data);
            let _ = env.interp().member_set(&target, &pname, f);
        } else if d.getter.is_some() || d.setter.is_some() {
            let g = d.getter.map(|_| make_callback_fn(env.interp(), &pname, d.getter, d.data));
            let s = d.setter.map(|_| make_callback_fn(env.interp(), &pname, d.setter, d.data));
            env.interp().define_accessor_value(&target, &pname, g, s, enumerable);
        } else if !d.value.is_null() {
            let _ = env.interp().member_set(&target, &pname, value_of(d.value));
        }
    }

    *result = env.handle(ctor);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_new_instance(
    env: napi_env,
    constructor: napi_value,
    argc: usize,
    argv: *const napi_value,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let ctor = value_of(constructor);
    let mut args = Vec::with_capacity(argc);
    for i in 0..argc {
        args.push(value_of(*argv.add(i)));
    }
    match env.interp().construct_value(ctor, &args) {
        Ok(v) => {
            *result = env.handle(v);
            NAPI_OK
        }
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

// -- promises --

#[no_mangle]
pub unsafe extern "C" fn napi_create_promise(
    env: napi_env,
    deferred: *mut napi_deferred,
    promise: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let interp = env.interp();
    let global = interp.global_this();
    let promise_ctor = match interp.member_get(&global, "Promise") {
        Ok(c) => c,
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };

    // Capture the executor's (resolve, reject) — the Promise constructor calls the executor
    // synchronously, so the slot is populated by the time `construct_value` returns.
    let slot: Rc<std::cell::RefCell<(Value, Value)>> =
        Rc::new(std::cell::RefCell::new((Value::Undefined, Value::Undefined)));
    let slot2 = slot.clone();
    let executor = interp.new_native_fn(
        "",
        2,
        Rc::new(move |_ip: &mut Ctx, _this: Value, args: &[Value]| {
            let res = args.first().cloned().unwrap_or(Value::Undefined);
            let rej = args.get(1).cloned().unwrap_or(Value::Undefined);
            *slot2.borrow_mut() = (res, rej);
            Ok(Value::Undefined)
        }) as Rc<NativeClosure>,
    );

    let p = match interp.construct_value(promise_ctor, &[executor]) {
        Ok(p) => p,
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };
    let (resolve, reject) = slot.borrow().clone();

    let state = env.napi_state();
    let idx = {
        state.deferreds.push(Some(Deferred { resolve, reject }));
        state.deferreds.len() - 1
    };
    *deferred = encode_slot(idx);
    *promise = env.handle(p);
    NAPI_OK
}

/// Shared body for resolve/reject: invoke the stored settler with `value` and consume the deferred.
unsafe fn settle_deferred(env: napi_env, deferred: napi_deferred, value: napi_value, reject: bool) -> napi_status {
    let env = &mut *env;
    let settler = decode_slot(deferred)
        .and_then(|idx| env.napi_state().deferreds.get_mut(idx).and_then(|d| d.take()))
        .map(|d| if reject { d.reject } else { d.resolve });
    let Some(settler) = settler else {
        return NAPI_INVALID_ARG;
    };
    match env.interp().invoke(settler, Value::Undefined, &[value_of(value)]) {
        Ok(_) => NAPI_OK,
        Err(e) => {
            env.pending = Some(e);
            NAPI_PENDING_EXCEPTION
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn napi_resolve_deferred(
    env: napi_env,
    deferred: napi_deferred,
    resolution: napi_value,
) -> napi_status {
    settle_deferred(env, deferred, resolution, false)
}

#[no_mangle]
pub unsafe extern "C" fn napi_reject_deferred(
    env: napi_env,
    deferred: napi_deferred,
    rejection: napi_value,
) -> napi_status {
    settle_deferred(env, deferred, rejection, true)
}

// -- buffers / typed arrays --

#[no_mangle]
pub unsafe extern "C" fn napi_create_buffer_copy(
    env: napi_env,
    length: usize,
    data: *const c_void,
    result_data: *mut *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    let env = &mut *env;
    let bytes: &[u8] = if data.is_null() || length == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(data as *const u8, length)
    };
    let buf = match env.interp().make_uint8array(bytes) {
        Ok(b) => b,
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };
    if !result_data.is_null() {
        *result_data = env
            .interp()
            .typed_array_raw(&buf)
            .map(|(_, _, p)| p as *mut c_void)
            .unwrap_or(std::ptr::null_mut());
    }
    *result = env.handle(buf);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_create_external_buffer(
    env: napi_env,
    length: usize,
    data: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    // No zero-copy over foreign memory: copy the bytes into an engine-owned buffer. The finalize
    // callback is not run (the source is left to the addon / process exit).
    let env = &mut *env;
    let bytes: &[u8] = if data.is_null() || length == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(data as *const u8, length)
    };
    let buf = match env.interp().make_uint8array(bytes) {
        Ok(b) => b,
        Err(e) => {
            env.pending = Some(e);
            return NAPI_PENDING_EXCEPTION;
        }
    };
    *result = env.handle(buf);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_get_typedarray_info(
    env: napi_env,
    typedarray: napi_value,
    ta_type: *mut c_int,
    length: *mut usize,
    data: *mut *mut c_void,
    arraybuffer: *mut napi_value,
    byte_offset: *mut usize,
) -> napi_status {
    let env = &mut *env;
    let v = value_of(typedarray);
    let Some((code, byte_len, ptr)) = env.interp().typed_array_raw(&v) else {
        return NAPI_INVALID_ARG;
    };
    let elem = match code {
        0..=2 => 1usize,
        3 | 4 => 2,
        5..=7 => 4,
        _ => 8,
    };
    if !ta_type.is_null() {
        *ta_type = code as c_int;
    }
    if !length.is_null() {
        *length = byte_len / elem;
    }
    if !data.is_null() {
        *data = ptr as *mut c_void;
    }
    if !byte_offset.is_null() {
        // `data` already points at the view's first element, so the offset relative to it is 0.
        *byte_offset = 0;
    }
    if !arraybuffer.is_null() {
        *arraybuffer = env.handle(Value::Undefined);
    }
    NAPI_OK
}

// -- async work (run synchronously on the loop thread: correct, just not off-thread) --

#[no_mangle]
pub unsafe extern "C" fn napi_create_async_work(
    env: napi_env,
    _async_resource: napi_value,
    _async_resource_name: napi_value,
    execute: napi_async_execute_callback,
    complete: napi_async_complete_callback,
    data: *mut c_void,
    result: *mut napi_async_work,
) -> napi_status {
    let env = &mut *env;
    let state = env.napi_state();
    state.async_works.push(Some(AsyncWork { execute, complete, data }));
    *result = encode_slot(state.async_works.len() - 1);
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_queue_async_work(
    env: napi_env,
    work: napi_async_work,
) -> napi_status {
    let env = &mut *env;
    let Some(w) = decode_slot(work).and_then(|idx| {
        env.napi_state().async_works.get_mut(idx).and_then(|w| w.take())
    }) else {
        return NAPI_INVALID_ARG;
    };
    // Run the two halves inline. `execute` must not touch JS (it normally runs off-thread);
    // `complete` runs on the loop thread with JS access, which is where we already are.
    let mut work_env = Env::new(env.interp() as *mut Ctx);
    if let Some(execute) = w.execute {
        execute(&mut work_env as *mut Env, w.data);
    }
    if let Some(complete) = w.complete {
        complete(&mut work_env as *mut Env, NAPI_OK, w.data);
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_delete_async_work(
    env: napi_env,
    work: napi_async_work,
) -> napi_status {
    let env = &mut *env;
    if let Some(idx) = decode_slot(work) {
        if let Some(slot) = env.napi_state().async_works.get_mut(idx) {
            *slot = None;
        }
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_cancel_async_work(
    _env: napi_env,
    _work: napi_async_work,
) -> napi_status {
    // Work runs synchronously on queue, so by the time anything could cancel it, it is done.
    NAPI_OK
}

// -- threadsafe functions (stubbed: symbols must resolve, but the loop is single-threaded) --

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn napi_create_threadsafe_function(
    _env: napi_env,
    _func: napi_value,
    _async_resource: napi_value,
    _async_resource_name: napi_value,
    _max_queue_size: usize,
    _initial_thread_count: usize,
    _thread_finalize_data: *mut c_void,
    _thread_finalize_cb: *mut c_void,
    _context: *mut c_void,
    _call_js_cb: *mut c_void,
    result: *mut napi_threadsafe_function,
) -> napi_status {
    if !result.is_null() {
        *result = 1 as napi_threadsafe_function;
    }
    NAPI_OK
}

#[no_mangle]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    _env: napi_env,
    _func: napi_threadsafe_function,
) -> napi_status {
    NAPI_OK
}

/// Take the address of every exported `napi_*` symbol so the linker cannot drop them from the
/// static archive (they are only ever referenced by an addon at runtime, via the dynamic linker,
/// which the Rust link step can't see). Called once from [`op_load_addon`].
pub fn keep_napi_exports() {
    let anchors: [*const (); 64] = [
        napi_get_undefined as *const (),
        napi_get_null as *const (),
        napi_get_global as *const (),
        napi_get_boolean as *const (),
        napi_create_double as *const (),
        napi_create_int32 as *const (),
        napi_create_uint32 as *const (),
        napi_create_int64 as *const (),
        napi_create_string_utf8 as *const (),
        napi_create_object as *const (),
        napi_create_array as *const (),
        napi_create_array_with_length as *const (),
        napi_create_function as *const (),
        napi_typeof as *const (),
        napi_get_value_double as *const (),
        napi_get_value_int32 as *const (),
        napi_get_value_uint32 as *const (),
        napi_get_value_bool as *const (),
        napi_get_value_string_utf8 as *const (),
        napi_coerce_to_string as *const (),
        napi_set_named_property as *const (),
        napi_get_named_property as *const (),
        napi_has_named_property as *const (),
        napi_set_property as *const (),
        napi_get_property as *const (),
        napi_set_element as *const (),
        napi_get_element as *const (),
        napi_get_cb_info as *const (),
        napi_call_function as *const (),
        napi_throw as *const (),
        napi_throw_error as *const (),
        napi_throw_type_error as *const (),
        napi_throw_range_error as *const (),
        napi_create_error as *const (),
        napi_is_exception_pending as *const (),
        napi_get_and_clear_last_exception as *const (),
        napi_open_handle_scope as *const (),
        napi_close_handle_scope as *const (),
        napi_get_version as *const (),
        napi_strict_equals as *const (),
        napi_is_error as *const (),
        napi_get_prototype as *const (),
        napi_create_reference as *const (),
        napi_delete_reference as *const (),
        napi_get_reference_value as *const (),
        napi_reference_ref as *const (),
        napi_reference_unref as *const (),
        napi_wrap as *const (),
        napi_unwrap as *const (),
        napi_remove_wrap as *const (),
        napi_define_class as *const (),
        napi_new_instance as *const (),
        napi_create_promise as *const (),
        napi_resolve_deferred as *const (),
        napi_reject_deferred as *const (),
        napi_create_buffer_copy as *const (),
        napi_create_external_buffer as *const (),
        napi_get_typedarray_info as *const (),
        napi_create_async_work as *const (),
        napi_queue_async_work as *const (),
        napi_delete_async_work as *const (),
        napi_cancel_async_work as *const (),
        napi_create_threadsafe_function as *const (),
        napi_unref_threadsafe_function as *const (),
    ];
    std::hint::black_box(anchors);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn to_int32_matches_ecmascript() {
        assert_eq!(to_int32(0.0), 0);
        assert_eq!(to_int32(42.9), 42);
        assert_eq!(to_int32(-1.0), -1);
        assert_eq!(to_int32(4294967296.0), 0); // 2^32 wraps to 0
        assert_eq!(to_int32(4294967297.0), 1); // 2^32 + 1
        assert_eq!(to_int32(2147483648.0), i32::MIN); // 2^31 wraps to negative
        assert_eq!(to_int32(f64::INFINITY), 0);
        assert_eq!(to_int32(f64::NAN), 0);
    }

    #[test]
    fn read_utf8_counted_and_nul_terminated() {
        let s = CString::new("hello").unwrap();
        // NUL-terminated (NAPI_AUTO_LENGTH).
        assert_eq!(unsafe { read_utf8(s.as_ptr(), NAPI_AUTO_LENGTH) }, "hello");
        // Explicit length truncates.
        assert_eq!(unsafe { read_utf8(s.as_ptr(), 3) }, "hel");
        // A null pointer reads as empty.
        assert_eq!(unsafe { read_utf8(std::ptr::null(), NAPI_AUTO_LENGTH) }, "");
    }
}
